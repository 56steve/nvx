//! Safe operating-system interface shared by the guest tools.
//!
//! Guest program modules deny unsafe code. This module is their only boundary to the operating
//! system, through `nix`: it owns descriptors and mappings, and its submodules confine the few
//! unsafe `nix` operations, such as ioctls, memory mappings, and signal dispositions, behind
//! wrappers that establish their pointer, lifetime, and ownership invariants. Errors are `nix`'s
//! [`Errno`], whose descriptions the tools print.

mod fd;
mod ioctl;
mod mapping;
mod process;

use std::ffi::CString;
use std::io::{self, Write};
use std::os::unix::ffi::OsStringExt;

pub(crate) use nix::errno::Errno;

#[cfg(test)]
pub(crate) use fd::create_fifo;
pub(crate) use fd::{FileDescriptor, FileLock, Mode, OpenFlags, create_directory, open, page_size};
pub(crate) use ioctl::{console_pending, reseed_random};
pub(crate) use mapping::MemoryMapping;
pub(crate) use process::{Stdout, restore_sigpipe};

/// Returns the process arguments, program name first, as the C strings that `main` received.
///
/// Rust exposes Unix arguments as arbitrary bytes. Converting them back to `CString` is
/// infallible for real process arguments because the kernel terminates, rather than embeds,
/// their NUL bytes.
pub(crate) fn args() -> Vec<CString> {
    std::env::args_os()
        .map(|argument| CString::new(argument.into_vec()).expect("arguments are C strings"))
        .collect()
}

/// Writes the concatenation of `parts` to `stream` in one write.
///
/// The C tools wrote diagnostics through unbuffered `stderr` and ignored a diagnostic write
/// failure. This helper deliberately preserves both behaviors.
pub(crate) fn report(stream: &mut dyn Write, parts: &[&[u8]]) {
    let _ = stream.write_all(&parts.concat());
}

/// Writes `bytes` to `stream`, flushes it, and returns the corresponding `errno` on failure.
pub(crate) fn print(stream: &mut dyn Write, bytes: &[u8]) -> Result<(), Errno> {
    stream
        .write_all(bytes)
        .and_then(|()| stream.flush())
        .map_err(|error| errno_of(&error))
}

/// Returns the `errno` of an I/O error, or `EIO` for an error without one.
///
/// A write that made no progress has no OS error, so it maps to `EIO`, matching the guest tools'
/// generic I/O-failure treatment.
fn errno_of(error: &io::Error) -> Errno {
    error.raw_os_error().map_or(Errno::EIO, Errno::from_raw)
}

/// Repeats a safe system operation while it fails with `EINTR`.
///
/// The first success or non-interruption error is returned. Callers use this only where the C
/// implementation had an explicit retry loop; operations such as seed-file reads deliberately
/// call their wrapper directly.
pub(crate) fn retry<T>(mut operation: impl FnMut() -> Result<T, Errno>) -> Result<T, Errno> {
    loop {
        match operation() {
            Err(Errno::EINTR) => {}
            result => return result,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    //! Test streams and focused checks shared by the guest tools.

    use std::io::{self, Write};

    use super::*;

    /// A stream whose writes fail with a chosen `errno`.
    pub(crate) struct Failing(pub(crate) Errno);

    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(self.0))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn errno_of_takes_the_os_error_or_eio() {
        let error = io::Error::from_raw_os_error(Errno::ENOSPC as i32);
        assert_eq!(errno_of(&error), Errno::ENOSPC);
        assert_eq!(errno_of(&io::ErrorKind::WriteZero.into()), Errno::EIO);
    }

    #[test]
    fn report_concatenates_and_ignores_failures() {
        let mut stream = Vec::new();
        report(&mut stream, &[b"a: ", b"b", b"\n"]);
        assert_eq!(stream, b"a: b\n");
        report(&mut Failing(Errno::EIO), &[b"lost\n"]);
    }

    #[test]
    fn print_returns_the_errno_of_a_failed_write() {
        let mut stream = Vec::new();
        assert_eq!(print(&mut stream, b"7\n"), Ok(()));
        assert_eq!(stream, b"7\n");
        assert_eq!(print(&mut Failing(Errno::EPIPE), b"7\n"), Err(Errno::EPIPE));
    }

    #[test]
    fn retry_repeats_only_after_eintr() {
        let mut results = vec![Err(Errno::EINTR), Err(Errno::EINTR), Ok(3)].into_iter();
        let mut calls = 0;
        let result = retry(|| {
            calls += 1;
            results.next().expect("one result per call")
        });
        assert_eq!((result, calls), (Ok(3), 3));

        let mut calls = 0;
        let result: Result<usize, Errno> = retry(|| {
            calls += 1;
            Err(Errno::EIO)
        });
        assert_eq!((result, calls), (Err(Errno::EIO), 1));
    }

    #[test]
    fn explicit_close_reports_errno() {
        let descriptor = open(c"/dev/null", OpenFlags::O_RDONLY, Mode::empty()).unwrap();
        assert_eq!(descriptor.close(), Ok(()));
    }

    #[test]
    fn file_locks_are_exclusive_until_their_scope_ends() {
        use nix::fcntl::{Flock, FlockArg};

        let file = tempfile::NamedTempFile::new().unwrap();
        let path = CString::new(file.path().to_str().unwrap()).unwrap();
        let try_lock = || {
            Flock::lock(
                std::fs::File::open(file.path()).unwrap(),
                FlockArg::LockExclusiveNonblock,
            )
        };
        {
            let _lock = open(&path, OpenFlags::O_RDWR, Mode::empty())
                .unwrap()
                .lock_exclusive()
                .unwrap();
            assert_eq!(try_lock().map(|_| ()).unwrap_err().1, Errno::EWOULDBLOCK);
        }
        assert!(try_lock().is_ok());
    }
}

//! Safe operating-system interface shared by the guest programs.
//!
//! Guest program modules deny unsafe code. This module is their only boundary to the operating
//! system, through `nix`: it owns descriptors and mappings, and its submodules confine the few
//! unsafe `nix` operations, such as ioctls, memory mappings, signal dispositions, and `fork`,
//! behind wrappers that establish their pointer, lifetime, and ownership invariants. Errors are
//! `nix`'s [`Errno`], whose descriptions the programs print.
//!
//! The managed agent needs a few operations that `nix` cannot provide, which reach libc through
//! `nix`'s re-export: its forked child, which may neither allocate nor drop an owner, closes,
//! executes, and exits through libc; its wait-status decoding keeps the real-time signals that
//! `nix`'s `Signal` cannot represent; and its execution helper duplicates the descriptor that it
//! inherits by number, which it cannot borrow before it knows that the descriptor is open. Each
//! such call says why.

#[cfg(target_arch = "x86_64")]
mod clock;
mod fd;
mod filesystem;
mod ioctl;
mod mapping;
mod poll;
mod process;
mod socket;
mod spawn;
mod terminal;

use std::ffi::CString;
use std::io::{self, Write};
use std::os::unix::ffi::OsStringExt;

pub(crate) use nix::errno::Errno;
pub(crate) use nix::sys::signal::Signal;

#[cfg(target_arch = "x86_64")]
pub(crate) use clock::monotonic_ns;
pub(crate) use fd::{
    FileDescriptor, Mode, OpenFlags, create_directory, create_fifo, create_memfd, create_pipe,
    open, page_size, unlink,
};
pub(crate) use filesystem::{
    MountFlags, change_directory, chmod, chown, current_directory, mount, path_c_string,
};
pub(crate) use ioctl::{console_pending, reseed_random};
pub(crate) use mapping::MemoryMapping;
pub(crate) use poll::readable_or_hung_up;
#[cfg(test)]
pub(crate) use process::sigpipe_disposition;
pub(crate) use process::{
    ChildStatus, DirectStdout, ProcessId, Stdout, exec, has_exited, kill_process_group,
    reap_any_nonblocking, restore_sigpipe, take_inherited_descriptor, wait, with_sigpipe_ignored,
    write_port_diagnostic, write_standard_error,
};
pub(crate) use socket::connect_datagram;
pub(crate) use spawn::{CHILD_STAGE_CWD, PhysicalPwd, SpawnDescriptors, SpawnPlan, spawn};
pub(crate) use terminal::{configure_raw, drain};

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

/// Converts arbitrary bytes into a C string, or `EINVAL` if they contain a NUL.
pub(crate) fn c_string(bytes: &[u8]) -> Result<CString, Errno> {
    CString::new(bytes).map_err(|_| Errno::EINVAL)
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
pub(crate) fn errno_of(error: &io::Error) -> Errno {
    error.raw_os_error().map_or(Errno::EIO, Errno::from_raw)
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
    fn explicit_close_reports_errno() {
        let descriptor = open(c"/dev/null", OpenFlags::O_RDONLY, Mode::empty()).unwrap();
        assert_eq!(descriptor.close(), Ok(()));
    }

    #[test]
    fn truncate_and_sync_resize_a_file() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = std::ffi::CString::new(file.path().to_str().unwrap()).unwrap();
        let descriptor = open(&path, OpenFlags::O_RDWR, Mode::empty()).unwrap();
        assert_eq!(descriptor.truncate(8192), Ok(()));
        assert_eq!(descriptor.sync(), Ok(()));
        assert_eq!(std::fs::metadata(file.path()).unwrap().len(), 8192);
        let read_only = open(&path, OpenFlags::O_RDONLY, Mode::empty()).unwrap();
        assert!(read_only.truncate(0).is_err());
    }

    #[test]
    fn connected_datagrams_wait_for_and_receive_echoes() {
        let server = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let std::net::SocketAddr::V4(peer) = server.local_addr().unwrap() else {
            panic!("an IPv4 server");
        };
        let client = connect_datagram(peer).unwrap();
        assert_eq!(client.wait_readable(1), Ok(false));
        assert_eq!(client.write(b"echo"), Ok(4));
        let mut buffer = [0; 8];
        let (length, source) = server.recv_from(&mut buffer).unwrap();
        assert_eq!(&buffer[..length], b"echo");
        server.send_to(b"back", source).unwrap();
        assert_eq!(client.wait_readable(5_000), Ok(true));
        assert_eq!(client.read(&mut buffer), Ok(4));
        assert_eq!(&buffer[..4], b"back");
    }
}

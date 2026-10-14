//! Owned file descriptors and safe file-oriented system operations.

use std::ffi::CStr;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};

pub(crate) use nix::fcntl::OFlag as OpenFlags;
use nix::fcntl::{Flock, FlockArg};
pub(crate) use nix::sys::stat::Mode;

use super::Errno;

/// An exclusive advisory lock on an owned descriptor, released when it is dropped.
///
/// `nix` unlocks the file, then closes its descriptor. The C tools only closed the descriptor,
/// which also releases the lock. Unlocking an owned descriptor cannot fail, and a failure would
/// panic.
pub(crate) type FileLock = Flock<OwnedFd>;

/// An owned file descriptor.
///
/// The inner [`OwnedFd`] closes the descriptor on every early-return path. [`close`](Self::close)
/// consumes the owner and reports the close error for the C code paths whose diagnostics depend
/// on it; dropping the value is the best-effort fallback where the C program ignored close.
#[derive(Debug)]
pub(crate) struct FileDescriptor(OwnedFd);

impl FileDescriptor {
    /// Reads bytes once, without retrying an interrupted call.
    pub(crate) fn read(&self, bytes: &mut [u8]) -> Result<usize, Errno> {
        nix::unistd::read(&self.0, bytes)
    }

    /// Writes bytes once, without retrying an interrupted or short call.
    pub(crate) fn write(&self, bytes: &[u8]) -> Result<usize, Errno> {
        nix::unistd::write(&self.0, bytes)
    }

    /// Reads at `offset` once, preserving the C cast from `u64` to `off_t`.
    pub(crate) fn pread(&self, bytes: &mut [u8], offset: u64) -> Result<usize, Errno> {
        nix::sys::uio::pread(&self.0, bytes, offset as _)
    }

    /// Writes at `offset` once, preserving the C cast from `u64` to `off_t`.
    pub(crate) fn pwrite(&self, bytes: &[u8], offset: u64) -> Result<usize, Errno> {
        nix::sys::uio::pwrite(&self.0, bytes, offset as _)
    }

    /// Takes an exclusive advisory lock on the descriptor, retrying after `EINTR`.
    ///
    /// The descriptor closes on failure, as in the C tools.
    pub(crate) fn lock_exclusive(self) -> Result<FileLock, Errno> {
        let mut descriptor = self.0;
        loop {
            match Flock::lock(descriptor, FlockArg::LockExclusive) {
                Ok(lock) => return Ok(lock),
                Err((returned, Errno::EINTR)) => descriptor = returned,
                Err((_, errno)) => return Err(errno),
            }
        }
    }

    /// Closes this descriptor and returns an error if the kernel reports one.
    pub(crate) fn close(self) -> Result<(), Errno> {
        nix::unistd::close(self.0)
    }

    /// Borrows the descriptor for another safe wrapper in this module tree.
    pub(super) fn borrowed(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }

    /// Returns the raw descriptor only to the ioctl wrappers in this module tree.
    pub(super) fn raw(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

/// Opens `path` with typed `flags` and `mode`, returning an owned descriptor.
pub(crate) fn open(path: &CStr, flags: OpenFlags, mode: Mode) -> Result<FileDescriptor, Errno> {
    nix::fcntl::open(path, flags, mode).map(FileDescriptor)
}

/// Creates `path` as a directory with `mode`.
pub(crate) fn create_directory(path: &CStr, mode: Mode) -> Result<(), Errno> {
    nix::unistd::mkdir(path, mode)
}

/// Creates `path` as a FIFO with `mode`.
///
/// Tests use it to stand for a console, which keeps their system calls in this module.
#[cfg(test)]
pub(crate) fn create_fifo(path: &CStr, mode: Mode) -> Result<(), Errno> {
    nix::unistd::mkfifo(path, mode)
}

/// Returns the host page size when it is a positive power of two.
pub(crate) fn page_size() -> Option<usize> {
    nix::unistd::sysconf(nix::unistd::SysconfVar::PAGE_SIZE)
        .ok()
        .flatten()
        .and_then(|size| usize::try_from(size).ok())
        .filter(|size| size.is_power_of_two())
}

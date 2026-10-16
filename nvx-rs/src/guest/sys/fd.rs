//! Owned file descriptors and safe file-oriented system operations.

use std::ffi::CStr;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};

pub(crate) use nix::fcntl::OFlag as OpenFlags;
use nix::poll::{PollFd, PollFlags};
pub(crate) use nix::sys::stat::Mode;

use super::Errno;

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

    /// Sets the file's size to `length` bytes, preserving the C cast from `u64` to `off_t`.
    pub(crate) fn truncate(&self, length: u64) -> Result<(), Errno> {
        nix::unistd::ftruncate(&self.0, length as _)
    }

    /// Writes the file's modified data and metadata to its storage device.
    pub(crate) fn sync(&self) -> Result<(), Errno> {
        nix::unistd::fsync(&self.0)
    }

    /// Waits at most `timeout_ms` milliseconds for data to read, with one `poll` call.
    ///
    /// Returns whether `poll` reported `POLLIN`. An interrupted call is an error, not a retry.
    pub(crate) fn wait_readable(&self, timeout_ms: u16) -> Result<bool, Errno> {
        let mut descriptors = [PollFd::new(self.0.as_fd(), PollFlags::POLLIN)];
        let ready = nix::poll::poll(&mut descriptors, timeout_ms)?;
        Ok(ready > 0
            && descriptors[0]
                .revents()
                .is_some_and(|events| events.contains(PollFlags::POLLIN)))
    }

    /// Takes ownership of a descriptor that another wrapper in this module tree created.
    pub(super) fn from_owned(descriptor: OwnedFd) -> Self {
        Self(descriptor)
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

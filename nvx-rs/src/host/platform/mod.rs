//! Operating-system primitives of NVX hosts.
//!
//! Each platform module provides the same functions. Linux and Windows are supported hosts;
//! other platforms compile but report NVX as unavailable.

use std::io;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::hypervisor::Hypervisor;

#[cfg(target_os = "linux")]
/// Linux implementations of host process, socket, and hypervisor primitives.
mod linux;
#[cfg(not(any(target_os = "linux", windows)))]
mod other;
#[cfg(windows)]
/// Windows implementations of host process, pipe, and hypervisor primitives.
#[allow(unsafe_code)]
mod windows;

#[cfg(target_os = "linux")]
use linux as imp;
#[cfg(not(any(target_os = "linux", windows)))]
use other as imp;
#[cfg(windows)]
use windows as imp;

pub(crate) use imp::*;
pub use imp::{SUPPORTED, create_private_dir, file_identity, process_start_time};

/// Checks that `hypervisor` is present and accessible on this host: its device on Linux, and the
/// Windows Hypervisor Platform on Windows.
///
/// Fails with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) otherwise.
pub fn probe_hypervisor(hypervisor: Hypervisor) -> Result<()> {
    imp::probe_hypervisor(hypervisor).map_err(Error::unavailable)
}

/// Byte stream to an OpenVMM control endpoint.
pub trait Transport: Send {
    /// Reads at least one byte, waiting at most `timeout` (`None` waits indefinitely).
    ///
    /// Returns `Ok(0)` at end of stream and an [`io::ErrorKind::TimedOut`] error on timeout.
    fn read(&mut self, buffer: &mut [u8], timeout: Option<Duration>) -> io::Result<usize>;

    /// Writes all of `data` within `timeout` (`None` waits indefinitely), and fails with an
    /// [`io::ErrorKind::TimedOut`] error if it has not by then, however much of it the endpoint
    /// reads meanwhile.
    fn write_all(&mut self, data: &[u8], timeout: Option<Duration>) -> io::Result<()>;
}

/// Returns the time left until `deadline`, `None` without one, or an [`io::ErrorKind::TimedOut`]
/// error once it has passed.
#[cfg(any(target_os = "linux", windows))]
fn time_left(deadline: Option<std::time::Instant>) -> io::Result<Option<Duration>> {
    match deadline {
        None => Ok(None),
        Some(deadline) => deadline
            .checked_duration_since(std::time::Instant::now())
            .filter(|left| !left.is_zero())
            .map(Some)
            .ok_or_else(|| io::ErrorKind::TimedOut.into()),
    }
}

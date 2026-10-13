//! The host side of NVX.
//!
//! - [`artifacts`] locates the OpenVMM executable and the guest kernel and initramfs.
//! - [`session`] speaks the authenticated control protocol, [`process`] launches and terminates
//!   OpenVMM processes, and [`platform`] holds the primitives of Linux and Windows hosts. Other
//!   hosts compile but report NVX as unavailable.

pub mod artifacts;
pub mod platform;
pub mod process;
pub mod session;

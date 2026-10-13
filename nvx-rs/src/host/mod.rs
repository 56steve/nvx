//! The host side of NVX.
//!
//! - [`artifacts`] locates the OpenVMM executable and the guest kernel and initramfs.
//! - [`launch`] builds OpenVMM's command line from a [`LaunchSpec`](launch::LaunchSpec): the
//!   managed lifecycle, the workload identity, the kernel command line, and the network and
//!   filesystem options.
//! - [`network`] lowers egress rules into OpenVMM's rules, and [`filesystem`] plans the export of
//!   host paths through OpenVMM's aggregate virtio-fs device.
//! - [`session`] speaks the authenticated control protocol, [`process`] launches and terminates
//!   OpenVMM processes, and [`platform`] holds the primitives of Linux and Windows hosts. Other
//!   hosts compile but report NVX as unavailable.

pub mod artifacts;
pub mod filesystem;
pub mod launch;
pub mod network;
pub mod platform;
pub mod process;
pub mod session;

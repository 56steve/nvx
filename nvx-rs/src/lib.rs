//! NVX mechanics: the OpenVMM control protocol, and the launch, policy lowering, and supervision
//! of NVX microVMs.
//!
//! An NVX sandbox is a microVM that OpenVMM runs with the managed lifecycle: the guest's init
//! starts a managed agent, which runs workloads that the host sends it over an authenticated
//! control console. This crate holds everything it takes to drive one:
//!
//! - [`protocol`]: the codec of the control console, the `NVXS` records that OpenVMM brokers and
//!   the `NVXC` frames of the guest agent. It has no dependencies, so the host, the guest, and
//!   test doubles share it.
//! - [`contract`]: the control-contract versions that this crate speaks, and the compatibility of
//!   NVX releases with them.
//! - [`net`]: IP networks in CIDR notation and their subtraction.
//! - [`Hypervisor`]: the hypervisors that run the VMs.
//! - [`host`] (feature `host`): artifact discovery, the lowering of network and filesystem
//!   policies, control sessions, and process and platform primitives.
//!
//! # Features
//!
//! - `host` (default): the [`host`] module. It enables `serde`.
//! - `bundled`: stages the OpenVMM executable, guest kernel, and control initramfs at build time;
//!   see `Artifacts::bundled`. The build script downloads the release that `artifacts.json` pins,
//!   or copies the release directory that `NVX_BUNDLE_DIR` names.
//! - `serde`: the serde form of [`Hypervisor`].

pub mod contract;
mod error;
mod hypervisor;
pub mod net;
pub mod protocol;

#[cfg(feature = "host")]
pub mod host;

pub use error::{Error, ErrorKind, Result};
#[cfg(feature = "host")]
pub use host::artifacts::Artifacts;
#[cfg(feature = "host")]
pub use host::filesystem::{guest_path, resolve_guest_path};
pub use hypervisor::Hypervisor;

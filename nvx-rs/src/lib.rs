//! NVX mechanics: the OpenVMM control protocol, and the launch, policy lowering, and supervision
//! of NVX microVMs.
//!
//! An NVX sandbox is a microVM that OpenVMM runs with the managed lifecycle: the guest's init
//! starts a managed agent, which runs workloads that the host sends it over an authenticated
//! control console. This crate holds everything it takes to drive one:
//!
//! - [`net`]: IP networks in CIDR notation and their subtraction.
//! - [`Hypervisor`]: the hypervisors that run the VMs.
//!
//! # Features
//!
//! - `serde`: the serde form of [`Hypervisor`].

mod error;
mod hypervisor;
pub mod net;

pub use error::{Error, ErrorKind, Result};
pub use hypervisor::Hypervisor;

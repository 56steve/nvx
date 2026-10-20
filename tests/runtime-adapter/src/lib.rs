//! An independent adapter of the NVX runtime. NVX's CI builds it against `nvx` the way an
//! adapter outside the repository would: as a path dependency with default features off, with a
//! lockfile of its own, and through the public API alone.
//!
//! Each configuration enables one side of `nvx`:
//!
//! - protocol-only, without features: the control protocol, the contract versions, and the other
//!   items that `nvx` has without features, with the adapter's own [`frame`]s;
//! - host-only, with the `host` feature: the host API too, and the `runtime-adapter-host`
//!   binary;
//! - guest-only, with the `guest` feature: the guest API too, the adapter's guest half in its
//!   `guest` module, and the `runtime-adapter-guest` binary, a static
//!   `x86_64-unknown-linux-musl` executable.
//!
//! The adapter brings its own framing to the NVXS session, in place of NVX's `NVXC` frames. Its
//! guest half serves the guest's role of the session with it, and `runtime-adapter-guest serve`
//! runs the guest half on its standard input and output. The host binary is a skeleton that
//! reports the contract it was built against. Later steps fill the host half, give the guest half
//! the rest of the guest API, and add the scenarios that run both halves on a hypervisor.

pub mod frame;
#[cfg(all(feature = "guest", target_os = "linux"))]
pub mod guest;

/// Describes the runtime contract that this build speaks: the control contract revision, and the
/// versions of the microVM ABI, the NVXS records, and the NVXC frames.
pub fn contract() -> String {
    format!(
        "control contract {} (microVM ABI {}, NVXS v{}, NVXC v{})",
        nvx::contract::CONTROL_CONTRACT_REVISION,
        nvx::contract::MICROVM_ABI_VERSION,
        nvx::protocol::OUTER_VERSION,
        nvx::protocol::APP_VERSION,
    )
}

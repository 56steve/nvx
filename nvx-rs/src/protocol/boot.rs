//! Kernel command-line tokens through which the host configures the guest agent at boot.

/// Prefix of the token that announces how many host mappings the guest agent receives through
/// `MAPS` requests, such as `nvx_maps=3`. The agent refuses workloads until it has mounted that
/// many.
pub const MAPS_TOKEN: &str = "nvx_maps=";

/// Token with which the guest creates an account for the workload identity when its image lacks
/// a usable one.
pub const WORKLOAD_ACCOUNT_TOKEN: &str = "nvx_workload_account=create";

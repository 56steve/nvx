//! Passes the bundle that `nvx-rs` staged on to the build scripts of this crate's dependents.
//!
//! With the `bundled` feature, the build script of `nvx-rs` stages the OpenVMM executable, guest
//! kernel, and Alpine initramfs, and exports the staged directory as `DEP_NVX_ARTIFACTS_DIR`.
//! Cargo gives `DEP_*` variables only to direct dependents, so this script exports the directory
//! again, as `DEP_ACI_EDGE_SANDBOXES_ARTIFACTS_DIR`, for the build scripts of crates that depend on
//! this one and ship the bundle next to their executables.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=DEP_NVX_ARTIFACTS_DIR");
    if let Some(directory) = std::env::var_os("DEP_NVX_ARTIFACTS_DIR") {
        println!(
            "cargo::metadata=artifacts_dir={}",
            std::path::Path::new(&directory).display()
        );
    }
}

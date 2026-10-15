//! The edge agent bundle that the build script stages with the `agent-bundled` feature.
//!
//! The build script verifies the bundle, copies it into an `aci-edge-sandboxes` directory beside
//! the executables that Cargo builds, and compiles in the SHA-256 digests of the bundle's
//! `SOURCE-MANIFEST.json` and agent library. [`BundledAgent::locate`] finds the directory relative
//! to the running executable, so an executable and its bundle directory keep working when they
//! are copied elsewhere together.

use std::env;
use std::path::{Path, PathBuf};

use aci_edge_sandboxes_model::hex;
use aci_edge_sandboxes_model::wire::SANDBOX_ABI_VERSION;

use super::{AgentConfig, BundleSource, RuntimeSource, SetupConfig};
use crate::error::{Error, Result};

/// The bundle's runtime manifest, which a [`RuntimeSource::Bundle`] trusts through its digest.
const MANIFEST_NAME: &str = "SOURCE-MANIFEST.json";
/// SHA-256 of the staged bundle's `SOURCE-MANIFEST.json`.
const MANIFEST_SHA256: Option<&str> =
    option_env!("ACI_EDGE_SANDBOXES_BUNDLED_AGENT_MANIFEST_SHA256");
/// Path of the agent library in the staged bundle, with `/` separators.
const LIBRARY: Option<&str> = option_env!("ACI_EDGE_SANDBOXES_BUNDLED_AGENT_LIBRARY");
/// SHA-256 of the staged bundle's agent library.
const LIBRARY_SHA256: Option<&str> = option_env!("ACI_EDGE_SANDBOXES_BUNDLED_AGENT_LIBRARY_SHA256");
/// Version of the sandbox ABI that the staged bundle declares for its library.
const SANDBOX_ABI: Option<&str> = option_env!("ACI_EDGE_SANDBOXES_BUNDLED_AGENT_SANDBOX_ABI");

// A bundle whose library speaks another sandbox ABI fails the build rather than every load.
const _: () = if let Some(version) = SANDBOX_ABI {
    assert!(
        decimal(version) == SANDBOX_ABI_VERSION,
        "the staged agent bundle's library speaks another sandbox ABI than this crate"
    );
};

/// Parses a decimal number at compile time.
const fn decimal(text: &str) -> u32 {
    let digits = text.as_bytes();
    assert!(!digits.is_empty(), "the sandbox ABI version is empty");
    let mut value = 0u32;
    let mut index = 0;
    while index < digits.len() {
        assert!(
            digits[index].is_ascii_digit(),
            "the sandbox ABI version is not a decimal number"
        );
        value = value * 10 + (digits[index] - b'0') as u32;
        index += 1;
    }
    value
}

/// The edge agent bundle that the build staged with the `agent-bundled` feature: the runtime
/// files that a [`RuntimeSource::Bundle`] names and the agent library, with the SHA-256 digests
/// of its `SOURCE-MANIFEST.json` and library that the build script verified.
///
/// ```no_run
/// use aci_edge_sandboxes::AciEdgeSandbox;
/// use aci_edge_sandboxes::agent::{AgentConfig, BundledAgent, CpuProfile};
///
/// # fn main() -> aci_edge_sandboxes::Result<()> {
/// // Every setup field but the state root at its default:
/// let client = AciEdgeSandbox::agent(AgentConfig::bundled("C:\\edge-state")?)?;
/// // Or a setup of one's own:
/// let bundle = BundledAgent::locate()?;
/// let setup = bundle.setup("C:\\edge-state").with_cpu_profile(CpuProfile::Host);
/// let client = AciEdgeSandbox::agent(bundle.config(setup))?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundledAgent {
    directory: PathBuf,
    manifest_sha256: [u8; 32],
    library: PathBuf,
    library_sha256: [u8; 32],
}

impl BundledAgent {
    /// Name of the directory that holds the bundle, which the build script creates beside the
    /// executables that Cargo builds.
    pub const DIRECTORY: &'static str = "aci-edge-sandboxes";

    /// Locates the bundle relative to the current executable: in a [`DIRECTORY`](Self::DIRECTORY)
    /// beside it or, for the executables that Cargo builds into `examples/` and `deps/`, beside
    /// its directory.
    ///
    /// Fails with [`ErrorCode::BackendUnavailable`](crate::ErrorCode::BackendUnavailable) if the
    /// build staged no bundle or neither directory holds one.
    pub fn locate() -> Result<Self> {
        pins()?;
        let executable = env::current_exe().map_err(|error| {
            Error::backend_unavailable("cannot locate the current executable").with_source(error)
        })?;
        let directory = executable.parent().ok_or_else(|| {
            Error::backend_unavailable("the current executable is not in a directory")
        })?;
        match find(directory) {
            Some(bundle) => Self::from_dir(bundle),
            None => Err(Error::backend_unavailable(format!(
                "no agent bundle is beside {}: copy the {} directory that the build staged \
                 beside it, or beside its directory",
                executable.display(),
                Self::DIRECTORY
            ))),
        }
    }

    /// Uses the copy of the staged bundle in `directory`. The host checks its files against the
    /// compiled-in digests when it opens.
    ///
    /// Fails with [`ErrorCode::BackendUnavailable`](crate::ErrorCode::BackendUnavailable) if the
    /// build staged no bundle.
    pub fn from_dir(directory: impl AsRef<Path>) -> Result<Self> {
        let (manifest_sha256, library, library_sha256) = pins()?;
        let directory = std::path::absolute(directory.as_ref()).map_err(|error| {
            Error::backend_unavailable("cannot resolve the agent bundle directory")
                .with_source(error)
        })?;
        Ok(Self {
            library: library
                .split('/')
                .fold(directory.clone(), |path, part| path.join(part)),
            directory,
            manifest_sha256,
            library_sha256,
        })
    }

    /// Returns the absolute directory of the bundle.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Returns the SHA-256 of the bundle's `SOURCE-MANIFEST.json` that the build verified.
    pub fn manifest_sha256(&self) -> [u8; 32] {
        self.manifest_sha256
    }

    /// Returns the absolute path of the bundle's agent library.
    pub fn library(&self) -> &Path {
        &self.library
    }

    /// Returns the SHA-256 of the agent library that the build verified.
    pub fn library_sha256(&self) -> [u8; 32] {
        self.library_sha256
    }

    /// Returns the runtime that a setup names: this bundle, trusted through its manifest's
    /// digest.
    pub fn runtime(&self) -> RuntimeSource {
        RuntimeSource::Bundle(BundleSource {
            path: self.directory.clone(),
            manifest_sha256: self.manifest_sha256,
        })
    }

    /// Returns a setup with `state_root`, this bundle's runtime, and every other field at its
    /// default.
    pub fn setup(&self, state_root: impl Into<PathBuf>) -> SetupConfig {
        SetupConfig::new(state_root, self.runtime())
    }

    /// Returns the configuration that opens a host for `setup` through this bundle's library.
    pub fn config(&self, setup: SetupConfig) -> AgentConfig {
        AgentConfig::new(setup, self.library.clone(), self.library_sha256)
    }
}

/// Returns the bundle directory that [`BundledAgent::locate`] uses for an executable in
/// `directory`: the first of the candidates that holds a runtime manifest.
fn find(directory: &Path) -> Option<PathBuf> {
    [Some(directory), directory.parent()]
        .into_iter()
        .flatten()
        .map(|candidate| candidate.join(BundledAgent::DIRECTORY))
        .find(|candidate| candidate.join(MANIFEST_NAME).is_file())
}

/// Returns the digests and library path that the build script compiled in.
fn pins() -> Result<([u8; 32], &'static str, [u8; 32])> {
    let digest = |hex: &str| {
        hex::decode_sha256(hex).ok_or_else(|| {
            Error::backend_error("the build compiled in a malformed agent bundle digest")
        })
    };
    match (MANIFEST_SHA256, LIBRARY, LIBRARY_SHA256) {
        (Some(manifest), Some(library), Some(library_sha256)) => {
            Ok((digest(manifest)?, library, digest(library_sha256)?))
        }
        _ => Err(Error::backend_unavailable(
            "this build staged no agent bundle: build with the agent-bundled feature, without \
             ACI_EDGE_SANDBOXES_AGENT_BUNDLE=skip",
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::error::ErrorCode;

    #[test]
    fn the_bundle_is_beside_the_executable_or_beside_its_directory() {
        let root = tempfile::tempdir().unwrap();
        let examples = root.path().join("examples");
        fs::create_dir(&examples).unwrap();
        assert_eq!(find(&examples), None);

        let parent = root.path().join(BundledAgent::DIRECTORY);
        fs::create_dir(&parent).unwrap();
        assert_eq!(find(&examples), None, "a directory without a manifest");
        fs::write(parent.join(MANIFEST_NAME), "{}").unwrap();
        assert_eq!(find(&examples), Some(parent));

        let beside = examples.join(BundledAgent::DIRECTORY);
        fs::create_dir(&beside).unwrap();
        fs::write(beside.join(MANIFEST_NAME), "{}").unwrap();
        assert_eq!(find(&examples), Some(beside));
    }

    #[test]
    fn a_located_bundle_names_its_library_and_runtime() {
        // Unit tests run from `<profile>/deps`, beside the bundle that the build script copied
        // into `<profile>`. A build that skipped staging has no bundle.
        let bundle = match BundledAgent::locate() {
            Ok(bundle) => bundle,
            Err(error) => {
                assert!(MANIFEST_SHA256.is_none(), "{error}");
                assert_eq!(error.code(), ErrorCode::BackendUnavailable);
                return;
            }
        };
        assert!(bundle.directory().is_absolute());
        assert!(bundle.directory().join(MANIFEST_NAME).is_file());
        assert!(bundle.library().starts_with(bundle.directory()));
        assert!(bundle.library().is_file());
        let setup = bundle.setup("state");
        assert_eq!(
            setup.runtime,
            RuntimeSource::Bundle(BundleSource {
                path: bundle.directory().to_path_buf(),
                manifest_sha256: bundle.manifest_sha256(),
            })
        );
        let config = bundle.config(setup);
        assert_eq!(config.library, bundle.library());
        assert_eq!(config.library_sha256, bundle.library_sha256());
    }

    #[test]
    fn sandbox_abi_versions_are_decimal() {
        assert_eq!(decimal("1"), 1);
        assert_eq!(decimal("42"), 42);
    }
}

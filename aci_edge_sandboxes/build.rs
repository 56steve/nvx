//! Passes the bundle that `nvx-rs` staged on to the build scripts of this crate's dependents, and
//! stages the edge agent bundle when the `agent-bundled` feature is enabled.
//!
//! With the `bundled` feature, the build script of `nvx-rs` stages the OpenVMM executable, guest
//! kernel, and Alpine initramfs, and exports the staged directory as `DEP_NVX_ARTIFACTS_DIR`.
//! Cargo gives `DEP_*` variables only to direct dependents, so this script exports the directory
//! again, as `DEP_ACI_EDGE_SANDBOXES_ARTIFACTS_DIR`, for the build scripts of crates that depend on
//! this one and ship the bundle next to their executables.
//!
//! With the `agent-bundled` feature, it stages the edge agent bundle: the agent library, OpenVMM,
//! the edge guest, and the image tool. See the `agent_bundle` module.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=DEP_NVX_ARTIFACTS_DIR");
    if let Some(directory) = std::env::var_os("DEP_NVX_ARTIFACTS_DIR") {
        println!(
            "cargo::metadata=artifacts_dir={}",
            std::path::Path::new(&directory).display()
        );
    }
    #[cfg(feature = "agent-bundled")]
    agent_bundle::run();
}

/// Stages the edge agent bundle when the `agent-bundled` feature is enabled.
///
/// The bundle is the `aci-edge-sandboxes.zip` asset of an NVX release: the agent library, OpenVMM,
/// the edge guest kernel and initramfs, the image tool, `SOURCE-MANIFEST.json`,
/// `bundle-manifest.json`, and `SHA256SUMS`. By default the build script downloads it from the
/// release of the commit being built (`v<VERSION>-dev.<12 hex digits>`, with the version in that
/// commit's `VERSION` file) or, if that release has none, from the release of the nearest of the
/// commit's first-parent ancestors that has one, searching a bounded number of commits.
///
/// The bundle is verified and staged in `OUT_DIR/aci-edge-sandboxes`, which dependent build
/// scripts read as `DEP_ACI_EDGE_SANDBOXES_AGENT_ARTIFACTS_DIR`, and copied to an
/// `aci-edge-sandboxes` directory in the Cargo profile directory, beside the executables that
/// Cargo builds. The crate compiles in the SHA-256 digests of the bundle's runtime manifest and
/// agent library and finds the directory relative to the executable at run time, so no absolute
/// path is compiled in.
///
/// Environment variables:
///
/// - `ACI_EDGE_SANDBOXES_AGENT_BUNDLE_URL`: a bundle ZIP to download instead of the release's; a
///   `file://` URL names a ZIP on the build host.
/// - `ACI_EDGE_SANDBOXES_AGENT_BUNDLE_DIR`: an extracted bundle to stage instead of downloading
///   one, for offline builds or locally built bundles.
/// - `ACI_EDGE_SANDBOXES_AGENT_BUNDLE_SHA256`: the approved SHA-256 of the ZIP.
/// - `ACI_EDGE_SANDBOXES_AGENT_BUNDLE=skip`: stages nothing, as when the feature is disabled.
///
/// Targets other than Linux and Windows, where the agent library cannot run, stage nothing.
/// Every staged file must match the bundle's `SHA256SUMS`, the bundle may hold no links, and the
/// manifests must agree with the files and name an agent library for the target platform. A
/// bundle whose library speaks another sandbox ABI fails the crate's build.
#[cfg(feature = "agent-bundled")]
mod agent_bundle {
    use std::collections::BTreeMap;
    use std::env;
    use std::fs::{self, File};
    use std::io::{BufReader, Read};
    use std::path::{Component, Path, PathBuf};
    use std::process::Command;

    use serde_json::Value;
    use sha2::{Digest, Sha256};

    const SKIP: &str = "ACI_EDGE_SANDBOXES_AGENT_BUNDLE";
    const URL: &str = "ACI_EDGE_SANDBOXES_AGENT_BUNDLE_URL";
    const DIR: &str = "ACI_EDGE_SANDBOXES_AGENT_BUNDLE_DIR";
    const ARCHIVE_SHA256: &str = "ACI_EDGE_SANDBOXES_AGENT_BUNDLE_SHA256";
    /// Where this repository's releases are downloaded from.
    const RELEASES: &str = "https://github.com/microsoft/nvx/releases/download";
    /// The release asset that holds the bundle.
    const ASSET: &str = "aci-edge-sandboxes.zip";
    /// How many first-parent commits, counting the one being built, may be searched for a release
    /// that has the bundle.
    const SEARCH_DEPTH: usize = 32;
    /// The directory that holds the bundle; `agent::BundledAgent::BESIDE_EXE_DIR`.
    const DIRECTORY: &str = "aci-edge-sandboxes";
    const CHECKSUMS: &str = "SHA256SUMS";
    const SOURCE_MANIFEST: &str = "SOURCE-MANIFEST.json";
    const BUNDLE_MANIFEST: &str = "bundle-manifest.json";

    /// What the crate learns about a verified bundle.
    struct Bundle {
        /// Every file that `SHA256SUMS` lists, by its relative path, with its SHA-256.
        files: BTreeMap<String, String>,
        /// SHA-256 of `SOURCE-MANIFEST.json`.
        manifest_sha256: String,
        /// Relative path of the agent library.
        library: String,
        /// SHA-256 of the agent library.
        library_sha256: String,
        /// Version of the sandbox ABI that the library speaks.
        sandbox_abi: u64,
        /// The NVX commit that the bundle was built from, if the bundle records it.
        revision: Option<String>,
    }

    pub(crate) fn run() {
        for name in [SKIP, URL, DIR, ARCHIVE_SHA256] {
            println!("cargo::rerun-if-env-changed={name}");
        }
        if env::var_os(SKIP).is_some_and(|value| value == "skip") {
            return;
        }
        let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
        if target_os != "windows" && target_os != "linux" {
            // The agent library runs on Linux and Windows hosts only.
            return;
        }
        let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set"));
        let downloads = out_dir.join("agent-bundle-download");
        let staged = out_dir.join(DIRECTORY);
        let checkout = Checkout::find();
        // The commit that the bundle should come from: the one being built, unless the bundle of
        // an ancestor's release is used.
        let mut expected = checkout
            .as_ref()
            .map(|checkout| checkout.commits[0].clone());
        match (variable(DIR), variable(URL)) {
            (Some(_), Some(_)) => fail(format!("set {DIR} or {URL}, not both")),
            (Some(path), None) => {
                let path = PathBuf::from(path);
                if !path.is_dir() {
                    fail(format!(
                        "{DIR} must name an extracted bundle; set {URL} to the file:// URL of a \
                         local ZIP instead"
                    ));
                }
                if variable(ARCHIVE_SHA256).is_some() {
                    fail(format!(
                        "{ARCHIVE_SHA256} applies to the ZIP that {URL} names, not to {DIR}"
                    ));
                }
                println!("cargo::rerun-if-changed={}", path.join(CHECKSUMS).display());
                copy_listed(&path, &staged);
            }
            (None, Some(url)) => match local_path(&url) {
                Some(archive) => {
                    if !archive.is_file() {
                        fail(format!(
                            "{URL} names {}, which is not a file",
                            archive.display()
                        ));
                    }
                    println!("cargo::rerun-if-changed={}", archive.display());
                    unpack(&archive, &staged);
                }
                None => unpack(&download(&url, &downloads), &staged),
            },
            (None, None) => {
                if target_os != "windows" {
                    fail(format!(
                        "releases publish {ASSET} for Windows only; set {URL} or {DIR} to a \
                         bundle for {target_os}"
                    ));
                }
                let checkout = checkout.as_ref().unwrap_or_else(|| {
                    fail(format!(
                        "cannot tell which commit is being built: build a git checkout with git \
                         on PATH, or set {URL} or {DIR}"
                    ))
                });
                checkout.watch();
                let (url, commit) = release(checkout, &downloads);
                expected = Some(commit);
                unpack(&download(&url, &downloads), &staged);
            }
        }
        let bundle = verify(&staged, &target_os);
        if let (Some(expected), Some(revision)) = (&expected, &bundle.revision)
            && !revision.eq_ignore_ascii_case(expected)
        {
            warn(format!(
                "the bundle was built from NVX {}, not {}",
                short(revision),
                short(expected)
            ));
        }
        install(&staged, &out_dir, &bundle);
        println!(
            "cargo::rustc-env=ACI_EDGE_SANDBOXES_BUNDLED_AGENT_MANIFEST_SHA256={}",
            bundle.manifest_sha256
        );
        println!(
            "cargo::rustc-env=ACI_EDGE_SANDBOXES_BUNDLED_AGENT_LIBRARY={}",
            bundle.library
        );
        println!(
            "cargo::rustc-env=ACI_EDGE_SANDBOXES_BUNDLED_AGENT_LIBRARY_SHA256={}",
            bundle.library_sha256
        );
        println!(
            "cargo::rustc-env=ACI_EDGE_SANDBOXES_BUNDLED_AGENT_SANDBOX_ABI={}",
            bundle.sandbox_abi
        );
        println!("cargo::metadata=agent_artifacts_dir={}", staged.display());
    }

    /// The git checkout that is being built.
    struct Checkout {
        /// This crate's directory.
        directory: PathBuf,
        /// The commit being built, then its first-parent ancestors.
        commits: Vec<String>,
    }

    impl Checkout {
        fn find() -> Option<Self> {
            let directory = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR")?);
            let depth = format!("--max-count={SEARCH_DEPTH}");
            let commits: Vec<String> =
                git(&directory, &["rev-list", "--first-parent", &depth, "HEAD"])?
                    .lines()
                    .map(str::to_owned)
                    .collect();
            if commits.is_empty() {
                return None;
            }
            Some(Self { directory, commits })
        }

        /// Returns the NVX version that `commit` records in the repository's `VERSION` file, which
        /// the tag of its release carries, or `None` if it records none.
        fn version_at(&self, commit: &str) -> Option<String> {
            let blob = format!("{commit}:VERSION");
            git(&self.directory, &["cat-file", "blob", &blob]).filter(|version| !version.is_empty())
        }

        /// Runs the build script again when HEAD moves to another commit.
        fn watch(&self) {
            // HEAD's reflog records every move, including a commit to a branch that only
            // `packed-refs` held, which creates a loose ref that no earlier run could watch. The
            // other files cover repositories without reflogs and the reftable format.
            let mut references = ["HEAD", "logs/HEAD", "packed-refs", "reftable/tables.list"]
                .map(str::to_owned)
                .to_vec();
            references.extend(git(&self.directory, &["symbolic-ref", "--quiet", "HEAD"]));
            for reference in references {
                if let Some(path) = git(&self.directory, &["rev-parse", "--git-path", &reference]) {
                    let path = self.directory.join(path);
                    // Cargo runs a build script on every build while a watched path is missing.
                    if path.is_file() {
                        println!("cargo::rerun-if-changed={}", path.display());
                    }
                }
            }
        }
    }

    /// Returns the trimmed standard output of a successful git command run in `directory`.
    fn git(directory: &Path, arguments: &[&str]) -> Option<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(arguments)
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    /// Returns the URL of the bundle that the release of the commit being built has or, failing
    /// that, the release of its nearest first-parent ancestor, and that commit.
    fn release(checkout: &Checkout, downloads: &Path) -> (String, String) {
        create_dir(downloads);
        let headers = downloads.join("probe");
        let head = short(&checkout.commits[0]);
        let mut versioned = 0;
        for (distance, commit) in checkout.commits.iter().enumerate() {
            // A release's tag carries the version of its own commit, which can differ from the
            // version of the commit being built.
            let Some(version) = checkout.version_at(commit) else {
                continue;
            };
            versioned += 1;
            let url = format!("{RELEASES}/v{version}-dev.{}/{ASSET}", short(commit));
            if exists(&url, &headers) {
                if distance > 0 {
                    warn(format!(
                        "{head} has no released bundle; using the one released for {}, \
                         {distance} first-parent commit(s) earlier",
                        short(commit)
                    ));
                }
                return (url, commit.clone());
            }
        }
        if versioned == 0 {
            fail(format!(
                "cannot name the releases of {head} and the {} first-parent commit(s) before it, \
                 which have no VERSION file; set {URL} or {DIR}",
                checkout.commits.len() - 1
            ));
        }
        fail(format!(
            "no release of {head} or of the {} first-parent commit(s) before it has {ASSET}; \
             publish one, or set {URL} or {DIR}",
            checkout.commits.len() - 1
        ))
    }

    /// Returns whether `url` exists, writing the response headers to `headers`.
    fn exists(url: &str, headers: &Path) -> bool {
        let mut command = Command::new("curl");
        command
            .args(["--silent", "--show-error", "--location", "--head"])
            .args(["--retry", "3", "--retry-delay", "2"])
            .args(["--write-out", "%{http_code}", "--output"])
            .arg(headers)
            .arg(url);
        let output = command
            .output()
            .unwrap_or_else(|error| fail(format!("cannot run {command:?}: {error}")));
        match String::from_utf8_lossy(&output.stdout).trim() {
            "200" => true,
            "404" => false,
            status => fail(format!(
                "cannot check {url} (HTTP status {status}): {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )),
        }
    }

    /// Downloads `url` into `downloads` and returns the archive, reusing the archive of an
    /// earlier run that downloaded the same URL.
    fn download(url: &str, downloads: &Path) -> PathBuf {
        create_dir(downloads);
        let archive = downloads.join(ASSET);
        let record = downloads.join(format!("{ASSET}.url"));
        let approved = variable(ARCHIVE_SHA256);
        if archive.is_file()
            && fs::read_to_string(&record).is_ok_and(|cached| cached == url)
            && approved
                .as_ref()
                .is_none_or(|digest| digest.eq_ignore_ascii_case(&sha256(&archive)))
        {
            return archive;
        }
        let partial = downloads.join(format!("{ASSET}.part"));
        execute(
            Command::new("curl")
                .args(["--fail", "--silent", "--show-error", "--location"])
                .args(["--retry", "5", "--retry-delay", "5", "--retry-all-errors"])
                .arg("--output")
                .arg(&partial)
                .arg(url),
        );
        let _ = fs::remove_file(&record);
        fs::rename(&partial, &archive)
            .unwrap_or_else(|error| fail(format!("cannot store {}: {error}", archive.display())));
        fs::write(&record, url)
            .unwrap_or_else(|error| fail(format!("cannot write {}: {error}", record.display())));
        // Cargo shows this again on builds that do not run the build script.
        warn(format!("downloaded {url}"));
        archive
    }

    /// Checks `archive` against the approved digest, if one is set, and extracts it into a new
    /// `staged` directory.
    fn unpack(archive: &Path, staged: &Path) {
        if let Some(approved) = variable(ARCHIVE_SHA256) {
            let actual = sha256(archive);
            if !actual.eq_ignore_ascii_case(&approved) {
                fail(format!(
                    "SHA-256 mismatch for {}: expected {approved}, found {actual}",
                    archive.display()
                ));
            }
        }
        recreate(staged);
        execute(&mut extraction(archive, staged));
    }

    /// Returns a command that extracts the ZIP `archive` into `directory`.
    fn extraction(archive: &Path, directory: &Path) -> Command {
        // GNU tar, the usual `tar` on Linux, cannot read ZIP archives, but bsdtar can.
        if let Some(tar) = [system_tar(), PathBuf::from("bsdtar")]
            .into_iter()
            .find(|candidate| is_bsdtar(candidate))
        {
            let mut command = Command::new(tar);
            command.arg("-xf").arg(archive).arg("-C").arg(directory);
            return command;
        }
        if succeeds(Command::new("unzip").arg("-v")) {
            let mut command = Command::new("unzip");
            command.arg("-q").arg(archive).arg("-d").arg(directory);
            return command;
        }
        fail(format!(
            "cannot extract {}: reading ZIP archives requires bsdtar or unzip on the build host; \
             install one, or set {DIR} to an extracted bundle",
            archive.display()
        ))
    }

    /// Returns the host's `tar`.
    fn system_tar() -> PathBuf {
        // Windows ships bsdtar, which reads ZIP archives; GNU tar from Git for Windows does not.
        if let Some(system_root) = env::var_os("SystemRoot") {
            let system_tar = Path::new(&system_root).join("System32").join("tar.exe");
            if system_tar.is_file() {
                return system_tar;
            }
        }
        PathBuf::from("tar")
    }

    /// Returns whether `tar` is libarchive's bsdtar, which also reads ZIP archives.
    fn is_bsdtar(tar: &Path) -> bool {
        Command::new(tar)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success() && output.stdout.starts_with(b"bsdtar"))
    }

    /// Copies the files that the `SHA256SUMS` of `source` lists, and the list itself, into a new
    /// `staged` directory. Fails if `source` holds a link, which copying would follow.
    fn copy_listed(source: &Path, staged: &Path) {
        reject_links(source);
        let files = read_checksums(&source.join(CHECKSUMS));
        recreate(staged);
        for name in files.keys().map(String::as_str).chain([CHECKSUMS]) {
            copy(&inside(source, name), &inside(staged, name));
        }
    }

    /// Checks the bundle in `directory` and removes the files that its `SHA256SUMS` does not
    /// list.
    fn verify(directory: &Path, target_os: &str) -> Bundle {
        reject_links(directory);
        let files = read_checksums(&directory.join(CHECKSUMS));
        for (name, expected) in &files {
            let path = inside(directory, name);
            let actual = sha256(&path);
            if &actual != expected {
                fail(format!(
                    "SHA-256 mismatch for {}: {CHECKSUMS} lists {expected}, found {actual}",
                    path.display()
                ));
            }
        }
        remove_unlisted(directory, directory, &files);
        let listed = |name: &str| {
            files
                .get(name)
                .cloned()
                .unwrap_or_else(|| fail(format!("{CHECKSUMS} does not list {name}")))
        };

        let source = read_json(&directory.join(SOURCE_MANIFEST));
        if source.get("format").and_then(Value::as_u64) != Some(2)
            || source.get("profile").and_then(Value::as_str) != Some("edge")
        {
            fail(format!(
                "{SOURCE_MANIFEST} is not a format-2 manifest of the edge profile"
            ));
        }

        let manifest = read_json(&directory.join(BUNDLE_MANIFEST));
        if manifest
            .pointer("/status/complete")
            .and_then(Value::as_bool)
            != Some(true)
        {
            fail(format!(
                "{BUNDLE_MANIFEST} does not describe a complete bundle"
            ));
        }
        let consumer = |field: &str| {
            manifest
                .pointer(&format!("/consumer/{field}"))
                .unwrap_or_else(|| fail(format!("{BUNDLE_MANIFEST} lacks consumer.{field}")))
        };
        let text = |field: &str| {
            consumer(field)
                .as_str()
                .unwrap_or_else(|| fail(format!("{BUNDLE_MANIFEST} consumer.{field} is not text")))
                .to_ascii_lowercase()
        };
        let manifest_sha256 = listed(SOURCE_MANIFEST);
        if text("runtimeBundleManifest") != SOURCE_MANIFEST.to_ascii_lowercase()
            || text("runtimeBundleManifestSha256") != manifest_sha256
        {
            fail(format!(
                "{BUNDLE_MANIFEST} names another runtime manifest than this {SOURCE_MANIFEST}"
            ));
        }
        let library = consumer("backendLibrary")
            .as_str()
            .unwrap_or_else(|| {
                fail(format!(
                    "{BUNDLE_MANIFEST} consumer.backendLibrary is not text"
                ))
            })
            .to_owned();
        let library_sha256 = listed(&library);
        if text("backendLibrarySha256") != library_sha256 {
            fail(format!(
                "{BUNDLE_MANIFEST} and {CHECKSUMS} disagree about {library}"
            ));
        }
        let suffix = if target_os == "windows" {
            ".dll"
        } else {
            ".so"
        };
        if !library.ends_with(suffix) {
            fail(format!(
                "the bundle's agent library, {library}, does not run on {target_os}"
            ));
        }
        // The files of the format-2 edge layout, each of which the host checks against its pin.
        let executable = if target_os == "windows" { ".exe" } else { "" };
        for (pin, name) in [
            (
                "/openvmm/executable_sha256",
                format!("bin/openvmm{executable}"),
            ),
            ("/linux/kernel_sha256", "guest/vmlinux".to_owned()),
            (
                "/edge/initramfs_sha256",
                "guest/initramfs-edge.cpio.gz".to_owned(),
            ),
            (
                "/direct_images/executable_sha256",
                format!("bin/direct-images{executable}"),
            ),
            ("/agent/library_sha256", library.clone()),
        ] {
            let pinned = source
                .pointer(pin)
                .and_then(Value::as_str)
                .unwrap_or_else(|| fail(format!("{SOURCE_MANIFEST} lacks {pin}")));
            if !pinned.eq_ignore_ascii_case(&listed(&name)) {
                fail(format!(
                    "{SOURCE_MANIFEST} pins {pin} to another digest than {name} has"
                ));
            }
        }
        let sandbox_abi = consumer("sandboxAbiVersion").as_u64().unwrap_or_else(|| {
            fail(format!(
                "{BUNDLE_MANIFEST} consumer.sandboxAbiVersion is not a number"
            ))
        });
        let revision = manifest
            .pointer("/sources/nvx/revision")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Bundle {
            files,
            manifest_sha256,
            library,
            library_sha256,
            sandbox_abi,
            revision,
        }
    }

    /// Copies the staged bundle beside the executables that Cargo builds, replacing only the files
    /// that differ, where `agent::BundledAgent::discover` finds it.
    fn install(staged: &Path, out_dir: &Path, bundle: &Bundle) {
        // `OUT_DIR` is `<profile>/build/<package>-<hash>/out`. Cargo builds executables into
        // `<profile>`, `<profile>/examples`, and `<profile>/deps`.
        let profile = Some(out_dir)
            .filter(|out_dir| out_dir.file_name().is_some_and(|name| name == "out"))
            .and_then(Path::parent)
            .and_then(Path::parent)
            .filter(|build| build.file_name().is_some_and(|name| name == "build"))
            .and_then(Path::parent);
        let Some(profile) = profile else {
            warn(format!(
                "cannot find the directory of Cargo's executables; copy {} beside the executable",
                staged.display()
            ));
            return;
        };
        let installed = profile.join(DIRECTORY);
        // Cleanup never follows a link, not even one in place of the directory itself.
        if fs::symlink_metadata(&installed).is_ok_and(|metadata| !metadata.is_dir()) {
            remove_entry(&installed);
        }
        create_dir(&installed);
        remove_unlisted(&installed, &installed, &bundle.files);
        let mut files: Vec<(&str, String)> = bundle
            .files
            .iter()
            .map(|(name, digest)| (name.as_str(), digest.clone()))
            .collect();
        files.push((CHECKSUMS, sha256(&staged.join(CHECKSUMS))));
        for (name, digest) in files {
            let target = inside(&installed, name);
            if fs::symlink_metadata(&target).is_ok_and(|metadata| metadata.is_file())
                && sha256(&target) == digest
            {
                continue;
            }
            let partial = PathBuf::from(format!("{}.partial", target.display()));
            copy(&inside(staged, name), &partial);
            fs::rename(&partial, &target).unwrap_or_else(|error| {
                let _ = fs::remove_file(&partial);
                fail(format!(
                    "cannot replace {}: {error}; stop the processes that use it",
                    target.display()
                ))
            });
        }
    }

    /// Fails if `directory` holds a link at any depth: a bundle holds only files and directories.
    fn reject_links(directory: &Path) {
        for (path, kind) in entries(directory) {
            if kind.is_symlink() {
                fail(format!(
                    "{} is a link; a bundle holds only files and directories",
                    path.display()
                ));
            }
            if kind.is_dir() {
                reject_links(&path);
            }
        }
    }

    /// Removes the files below `directory` that neither `files` nor `SHA256SUMS` names, relative to
    /// `root`, every link, and the directories that this leaves empty. Links are never followed.
    fn remove_unlisted(root: &Path, directory: &Path, files: &BTreeMap<String, String>) {
        for (path, kind) in entries(directory) {
            // Neither a link to a directory nor a junction is a directory here.
            if kind.is_dir() {
                remove_unlisted(root, &path, files);
                if fs::read_dir(&path).is_ok_and(|mut entries| entries.next().is_none()) {
                    let _ = fs::remove_dir(&path);
                }
                continue;
            }
            let name = path
                .strip_prefix(root)
                .expect("entries are below the root")
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            if kind.is_symlink() || (name != CHECKSUMS && !files.contains_key(&name)) {
                remove_entry(&path);
            }
        }
    }

    /// Removes the file or link at `path`. A link is removed, never what it points to.
    fn remove_entry(path: &Path) {
        // A link to a directory, including a junction, is removed as a directory on Windows.
        fs::remove_file(path)
            .or_else(|_| fs::remove_dir(path))
            .unwrap_or_else(|error| fail(format!("cannot remove {}: {error}", path.display())));
    }

    /// Returns the entries of `directory` with their types, without following links.
    fn entries(directory: &Path) -> Vec<(PathBuf, fs::FileType)> {
        fn listing<T>(directory: &Path, result: std::io::Result<T>) -> T {
            result.unwrap_or_else(|error| {
                fail(format!("cannot list {}: {error}", directory.display()))
            })
        }
        listing(directory, fs::read_dir(directory))
            .map(|entry| {
                let entry = listing(directory, entry);
                (entry.path(), listing(directory, entry.file_type()))
            })
            .collect()
    }

    /// Reads a `SHA256SUMS` file: relative paths and their lowercase SHA-256 digests.
    fn read_checksums(path: &Path) -> BTreeMap<String, String> {
        let text = fs::read_to_string(path)
            .unwrap_or_else(|error| fail(format!("cannot read {}: {error}", path.display())));
        let mut files = BTreeMap::new();
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            let entry = line.split_once(char::is_whitespace).map(|(digest, name)| {
                (
                    digest.to_ascii_lowercase(),
                    name.trim().trim_start_matches('*'),
                )
            });
            match entry {
                Some((digest, name))
                    if digest.len() == 64
                        && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                        && name != CHECKSUMS
                        && stays_inside(name)
                        && !files.contains_key(name) =>
                {
                    files.insert(name.to_owned(), digest);
                }
                _ => fail(format!("malformed line in {}: {line:?}", path.display())),
            }
        }
        files
    }

    /// Returns whether `name` is a relative path below the bundle's root.
    fn stays_inside(name: &str) -> bool {
        !name.is_empty()
            && !name.contains('\\')
            && Path::new(name)
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
    }

    /// Returns the path in `directory` of the bundle file `name`, whose components `/` separates.
    fn inside(directory: &Path, name: &str) -> PathBuf {
        name.split('/')
            .fold(directory.to_path_buf(), |path, part| path.join(part))
    }

    fn read_json(path: &Path) -> Value {
        let text = fs::read_to_string(path)
            .unwrap_or_else(|error| fail(format!("cannot read {}: {error}", path.display())));
        serde_json::from_str(&text)
            .unwrap_or_else(|error| fail(format!("{} is not valid JSON: {error}", path.display())))
    }

    /// Returns the value of an environment variable that is set and not empty.
    fn variable(name: &str) -> Option<String> {
        env::var(name).ok().filter(|value| !value.is_empty())
    }

    /// Returns the path on the build host that a `file:` URL names, or `None` for another URL.
    fn local_path(url: &str) -> Option<PathBuf> {
        const SCHEME: &str = "file://";
        fn hex(digit: u8) -> Option<u8> {
            match digit {
                b'0'..=b'9' => Some(digit - b'0'),
                b'a'..=b'f' => Some(digit - b'a' + 10),
                b'A'..=b'F' => Some(digit - b'A' + 10),
                _ => None,
            }
        }
        let rest = url
            .get(..SCHEME.len())
            .filter(|scheme| scheme.eq_ignore_ascii_case(SCHEME))
            .map(|_| &url[SCHEME.len()..])?;
        // A URL without a host, or with `localhost`, names a path on the build host; `curl`
        // handles others.
        let encoded = if rest.starts_with('/') {
            rest
        } else {
            rest.strip_prefix("localhost")
                .filter(|path| path.starts_with('/'))?
        };
        let mut bytes = Vec::with_capacity(encoded.len());
        let mut rest = encoded.as_bytes();
        while let Some((&byte, tail)) = rest.split_first() {
            rest = tail;
            if byte == b'%'
                && let [high, low, after @ ..] = tail
                && let (Some(high), Some(low)) = (hex(*high), hex(*low))
            {
                bytes.push((high << 4) | low);
                rest = after;
            } else {
                bytes.push(byte);
            }
        }
        let path = String::from_utf8(bytes)
            .unwrap_or_else(|_| fail(format!("{URL} names a path that is not UTF-8")));
        // `file:///C:/bundle.zip` names `C:/bundle.zip` on Windows.
        Some(PathBuf::from(match path.as_bytes() {
            [b'/', drive, b':', ..] if cfg!(windows) && drive.is_ascii_alphabetic() => &path[1..],
            _ => &path[..],
        }))
    }

    /// Returns the abbreviated form of a commit hash that release tags use.
    fn short(commit: &str) -> &str {
        commit.get(..12).unwrap_or(commit)
    }

    fn recreate(directory: &Path) {
        if directory.exists() {
            fs::remove_dir_all(directory).unwrap_or_else(|error| {
                fail(format!("cannot clear {}: {error}", directory.display()))
            });
        }
        create_dir(directory);
    }

    fn create_dir(directory: &Path) {
        fs::create_dir_all(directory).unwrap_or_else(|error| {
            fail(format!("cannot create {}: {error}", directory.display()))
        });
    }

    fn copy(from: &Path, to: &Path) {
        create_dir(to.parent().expect("bundle files have a parent"));
        fs::copy(from, to).unwrap_or_else(|error| {
            fail(format!(
                "cannot copy {} to {}: {error}",
                from.display(),
                to.display()
            ))
        });
    }

    fn succeeds(command: &mut Command) -> bool {
        command.output().is_ok_and(|output| output.status.success())
    }

    fn execute(command: &mut Command) {
        let output = command
            .output()
            .unwrap_or_else(|error| fail(format!("cannot run {command:?}: {error}")));
        if !output.status.success() {
            fail(format!(
                "{command:?} failed with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }

    fn sha256(path: &Path) -> String {
        let file = File::open(path)
            .unwrap_or_else(|error| fail(format!("cannot open {}: {error}", path.display())));
        let mut reader = BufReader::with_capacity(1 << 20, file);
        let mut hasher = Sha256::new();
        let mut buffer = vec![0; 1 << 20];
        loop {
            let read = reader
                .read(&mut buffer)
                .unwrap_or_else(|error| fail(format!("cannot read {}: {error}", path.display())));
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn warn(message: String) {
        println!("cargo::warning=agent: {message}");
    }

    fn fail(message: String) -> ! {
        panic!("agent bundled artifacts: {message}");
    }
}

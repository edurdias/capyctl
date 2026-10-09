//! ADR 0008 (owner decision 2026-09-23): engine installation identity and the
//! internals capyctl hooks, without hard-coded hashes or permission rules.
//!
//! **Fingerprint.** When the host registers its installations (agent start),
//! each is measured: the engine package's version from its `*.dist-info`
//! metadata, and a `sha256:` digest over a canonical manifest of the package's
//! files (relative path, size and SHA-256, sorted), in the style of the WE3
//! checkpoint manifest (`crate::checkpoint`). Bytecode caches (`__pycache__`,
//! `*.pyc`) are left out because the interpreter rewrites them on its own. The
//! walk is bounded and reads metadata and file bytes only; it never runs the
//! installation (ADR 0008 carve-out 2). A later launch measures again: a
//! different digest is drift, flagged in the host's status and in the
//! controller's event journal, and refused only when the installation's host
//! policy says `installation_drift: refuse` (default `warn`). An installation
//! that cannot be measured (an editable install, a package outside the
//! environment's `site-packages`) is `unmeasured`, never a refusal.
//!
//! **Capabilities.** A launch that depends on a gated feature (today: deep
//! parking) runs `runtime/engine_capabilities.py` under the installation's own
//! interpreter (`-I -S`, bounded time and output) to probe by shape the
//! internals that feature needs. A capability reported missing refuses that
//! feature with a closed `capability_missing:<name>` reason; one that could not
//! be probed is unknown and refuses nothing here (the protected entry probes
//! again at startup). Nothing in this module inspects permission bits: engine
//! installation files get no permission rule.
//!
//! Passing fingerprints and probes are not qualification evidence (AGENTS.md).

use capyctl_config::engine_policy::Engine;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::Read,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
    time::{Duration, Instant},
};

/// Domain separation for the canonical installation manifest.
pub const MANIFEST_DOMAIN: &[u8] = b"capyctl/installation-manifest/v1\n";
pub const DIGEST_PREFIX: &str = "sha256:";
/// Bounds on one installation package. Beyond them it is `unmeasured`.
pub const MAX_FILES: usize = 100_000;
pub const MAX_BYTES: u64 = 16 << 30;
const MAX_DEPTH: usize = 32;
const MAX_LIB_ENTRIES: usize = 256;
const MAX_SITE_ENTRIES: usize = 65_536;
const MAX_METADATA: u64 = 1 << 20;
const MAX_VERSION: usize = 128;
const CHUNK: usize = 1 << 20;
/// The capability probe imports the engine; it may take a while, never forever.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_PROBE_OUTPUT: usize = 64 << 10;
/// The probe helper in capyctl's runtime directory.
pub const PROBE_SCRIPT: &str = "engine_capabilities.py";
pub const PROBE_SCHEMA: &str = "capyctl/engine-capabilities/v1";

/// The closed capability names per engine (`runtime/engine_capabilities.py`).
pub fn capability_names(engine: Engine) -> &'static [&'static str] {
    match engine {
        Engine::Sglang => &["core", "deep_park", "metrics", "observation"],
        Engine::Vllm => &["core", "deep_park", "metrics"],
        Engine::Tensorfold => &["core", "deep_park", "metrics"],
        // ADR 0029 §2: nothing is probed; deep parking is reported missing.
        Engine::Llamacpp => &["deep_park"],
    }
}

/// The installed Python package that is the engine; none for llama.cpp, a
/// bare binary (ADR 0029 §2).
pub fn package_name(engine: Engine) -> Option<&'static str> {
    match engine {
        Engine::Sglang => Some("sglang"),
        Engine::Vllm => Some("vllm"),
        Engine::Tensorfold => Some("tensorfold"),
        Engine::Llamacpp => None,
    }
}

/// A closed failure category; never a path, a file name or OS text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FingerprintError {
    /// No `site-packages/<engine>` beside the installation's interpreter.
    #[error("not_found")]
    NotFound,
    /// More than one Python library directory holds the package.
    #[error("ambiguous")]
    Ambiguous,
    #[error("too_large")]
    TooLarge,
    /// A special file (not a regular file, directory or symbolic link).
    #[error("unsafe_file")]
    UnsafeFile,
    /// A file changed while it was being measured.
    #[error("changed")]
    Changed,
    #[error("io_error")]
    Io,
}

/// One installation's measured identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstallationFingerprint {
    /// The package's own version (`Version:` in its dist-info), or `unknown`.
    pub version: String,
    /// `sha256:<64 hex>` over the canonical manifest.
    pub digest: String,
    pub files: usize,
    pub bytes: u64,
}

/// The interpreter an installation runs capyctl's entries with: the executable
/// itself when it is a Python interpreter, else the `python3` beside it.
pub fn interpreter(executable: &Path) -> Option<PathBuf> {
    capyctl_adapters::vllm::interpreter_for(executable.to_str()?)
        .ok()
        .map(PathBuf::from)
}

/// `<prefix>/lib/python3.N/site-packages` holding the engine package, where
/// `<prefix>` is the interpreter's environment (never resolved through links,
/// so a virtual environment keeps its own tree). Exactly one must hold it.
pub fn site_packages(engine: Engine, executable: &Path) -> Result<PathBuf, FingerprintError> {
    let package = package_name(engine).ok_or(FingerprintError::NotFound)?;
    let interpreter = interpreter(executable).ok_or(FingerprintError::NotFound)?;
    let prefix = interpreter
        .parent()
        .and_then(Path::parent)
        .ok_or(FingerprintError::NotFound)?;
    let lib = prefix.join("lib");
    let entries = std::fs::read_dir(&lib).map_err(|_| FingerprintError::NotFound)?;
    let mut found = Vec::new();
    for (index, entry) in entries.enumerate() {
        if index >= MAX_LIB_ENTRIES {
            return Err(FingerprintError::TooLarge);
        }
        let entry = entry.map_err(|_| FingerprintError::Io)?;
        let name = entry.file_name();
        if !name.as_bytes().starts_with(b"python3.") {
            continue;
        }
        let site = lib.join(&name).join("site-packages");
        if std::fs::symlink_metadata(site.join(package)).is_ok_and(|metadata| metadata.is_dir()) {
            found.push(site);
        }
    }
    match found.len() {
        0 => Err(FingerprintError::NotFound),
        1 => Ok(found.remove(0)),
        _ => Err(FingerprintError::Ambiguous),
    }
}

/// The package version from exactly one `<package>-<version>.dist-info`.
fn version(site: &Path, package: &str) -> String {
    let unknown = || "unknown".to_owned();
    let Ok(entries) = std::fs::read_dir(site) else {
        return unknown();
    };
    let prefix = format!("{package}-");
    let mut matches = Vec::new();
    for (index, entry) in entries.enumerate() {
        if index >= MAX_SITE_ENTRIES {
            return unknown();
        }
        let Ok(entry) = entry else {
            return unknown();
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&prefix) && name.ends_with(".dist-info") {
            matches.push(entry.path());
        }
    }
    let [dist] = matches.as_slice() else {
        return unknown();
    };
    let mut text = String::new();
    let read = std::fs::File::open(dist.join("METADATA"))
        .and_then(|file| file.take(MAX_METADATA).read_to_string(&mut text));
    if read.is_err() {
        return unknown();
    }
    text.lines()
        .find_map(|line| line.strip_prefix("Version: "))
        .map(str::trim)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= MAX_VERSION
                && value.bytes().all(|b| b.is_ascii_graphic())
        })
        .map_or_else(unknown, str::to_owned)
}

/// One file's stat identity; any difference forces a rehash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Identity {
    fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.size(),
            mtime: (metadata.mtime(), metadata.mtime_nsec()),
            ctime: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

/// Measures installations, keeping each file's hash under its stat identity so
/// a launch re-measures an unchanged installation without reading it again.
#[derive(Default)]
pub struct InstallationMeasurer {
    cache: Mutex<BTreeMap<PathBuf, (Identity, [u8; 32])>>,
}

impl InstallationMeasurer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Measure one installation of `engine` started through `executable`.
    pub fn measure(
        &self,
        engine: Engine,
        executable: &Path,
    ) -> Result<InstallationFingerprint, FingerprintError> {
        let Some(package) = package_name(engine) else {
            return self.measure_binary(executable);
        };
        let site = site_packages(engine, executable)?;
        let root = site.join(package);
        let mut entries: Vec<(Vec<u8>, u64, [u8; 32])> = Vec::new();
        let mut total: u64 = 0;
        let mut stack: Vec<(PathBuf, Vec<u8>, usize)> = vec![(root.clone(), Vec::new(), 0)];
        let mut seen = std::collections::BTreeSet::new();
        while let Some((dir, relative, depth)) = stack.pop() {
            if depth > MAX_DEPTH {
                return Err(FingerprintError::TooLarge);
            }
            let listing = std::fs::read_dir(&dir).map_err(|_| FingerprintError::Io)?;
            for entry in listing {
                let entry = entry.map_err(|_| FingerprintError::Io)?;
                let name = entry.file_name();
                let name = name.as_bytes();
                let mut path_bytes = relative.clone();
                if !path_bytes.is_empty() {
                    path_bytes.push(b'/');
                }
                path_bytes.extend_from_slice(name);
                let path = entry.path();
                let metadata =
                    std::fs::symlink_metadata(&path).map_err(|_| FingerprintError::Changed)?;
                let kind = metadata.file_type();
                if kind.is_dir() {
                    if name == b"__pycache__" {
                        continue;
                    }
                    stack.push((path, path_bytes, depth + 1));
                    continue;
                }
                if name.ends_with(b".pyc") {
                    continue;
                }
                if entries.len() >= MAX_FILES {
                    return Err(FingerprintError::TooLarge);
                }
                let (size, digest) = if kind.is_symlink() {
                    link_digest(&path)?
                } else if kind.is_file() {
                    (metadata.size(), self.hash(&path, &metadata)?)
                } else {
                    return Err(FingerprintError::UnsafeFile);
                };
                total = total.checked_add(size).ok_or(FingerprintError::TooLarge)?;
                if total > MAX_BYTES {
                    return Err(FingerprintError::TooLarge);
                }
                seen.insert(path);
                entries.push((path_bytes, size, digest));
            }
        }
        // Forget files that left the installation, so the cache stays bounded.
        if let Ok(mut cache) = self.cache.lock() {
            cache.retain(|path, _| !path.starts_with(&root) || seen.contains(path));
        }
        let digest = manifest_digest(&mut entries)?;
        Ok(InstallationFingerprint {
            version: version(&site, package),
            digest,
            files: entries.len(),
            bytes: total,
        })
    }

    /// ADR 0029 §2, ADR 0008: a bare-binary installation (llama.cpp's
    /// `llama-server`) is the binary and every `lib*.so*` entry in its
    /// directory and, for the CMake install layout `<prefix>/bin/<binary>`,
    /// in `<prefix>/lib`: llama.cpp's own libraries, the backends a
    /// `GGML_BACKEND_DL` build loads, and runtime libraries unpacked beside a
    /// prebuilt binary. A link counts by its target text, as in a package
    /// tree. The version is the one a `libllama.so.X.Y.Z` name gives, else
    /// `unknown`; the build `--version` reports is the profile's fingerprint.
    fn measure_binary(
        &self,
        executable: &Path,
    ) -> Result<InstallationFingerprint, FingerprintError> {
        let metadata =
            std::fs::symlink_metadata(executable).map_err(|_| FingerprintError::NotFound)?;
        if !metadata.is_file() {
            return Err(FingerprintError::UnsafeFile);
        }
        let dir = executable.parent().ok_or(FingerprintError::NotFound)?;
        let name = executable.file_name().ok_or(FingerprintError::NotFound)?;
        let mut dirs: Vec<(&[u8], PathBuf)> = vec![(b"", dir.to_path_buf())];
        if let Some(lib) = install_layout_lib(dir) {
            dirs = vec![(b"bin/", dir.to_path_buf()), (b"lib/", lib)];
        }
        let mut entries = vec![(
            [dirs[0].0, name.as_bytes()].concat(),
            metadata.size(),
            self.hash(executable, &metadata)?,
        )];
        let mut total = metadata.size();
        let mut seen = std::collections::BTreeSet::from([executable.to_path_buf()]);
        for (prefix, dir) in &dirs {
            let listing = std::fs::read_dir(dir).map_err(|_| FingerprintError::Io)?;
            for entry in listing {
                let entry = entry.map_err(|_| FingerprintError::Io)?;
                let file = entry.file_name();
                if !shared_library(file.as_bytes()) {
                    continue;
                }
                let path = entry.path();
                let metadata =
                    std::fs::symlink_metadata(&path).map_err(|_| FingerprintError::Changed)?;
                let kind = metadata.file_type();
                if kind.is_dir() {
                    continue;
                }
                if entries.len() >= MAX_FILES {
                    return Err(FingerprintError::TooLarge);
                }
                let (size, digest) = if kind.is_symlink() {
                    link_digest(&path)?
                } else if kind.is_file() {
                    (metadata.size(), self.hash(&path, &metadata)?)
                } else {
                    return Err(FingerprintError::UnsafeFile);
                };
                total = total.checked_add(size).ok_or(FingerprintError::TooLarge)?;
                if total > MAX_BYTES {
                    return Err(FingerprintError::TooLarge);
                }
                seen.insert(path);
                entries.push(([*prefix, file.as_bytes()].concat(), size, digest));
            }
        }
        if let Ok(mut cache) = self.cache.lock() {
            cache.retain(|path, _| {
                !dirs
                    .iter()
                    .any(|(_, dir)| path.parent() == Some(dir.as_path()))
                    || seen.contains(path)
            });
        }
        let digest = manifest_digest(&mut entries)?;
        let lib_dirs: Vec<&Path> = dirs.iter().map(|(_, dir)| dir.as_path()).collect();
        Ok(InstallationFingerprint {
            version: library_version(&lib_dirs).unwrap_or_else(|| "unknown".into()),
            digest,
            files: entries.len(),
            bytes: total,
        })
    }

    fn hash(&self, path: &Path, before: &std::fs::Metadata) -> Result<[u8; 32], FingerprintError> {
        let identity = Identity::of(before);
        if let Some((cached, digest)) = self.cache.lock().ok().and_then(|c| c.get(path).copied()) {
            if cached == identity {
                return Ok(digest);
            }
        }
        let mut file = std::fs::File::open(path).map_err(|_| FingerprintError::Changed)?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; CHUNK];
        let mut read: u64 = 0;
        loop {
            let n = file.read(&mut buffer).map_err(|_| FingerprintError::Io)?;
            if n == 0 {
                break;
            }
            read += n as u64;
            hasher.update(&buffer[..n]);
        }
        let after = file.metadata().map_err(|_| FingerprintError::Io)?;
        if read != identity.size || Identity::of(&after) != identity {
            return Err(FingerprintError::Changed);
        }
        let digest: [u8; 32] = hasher.finalize().into();
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(path.to_owned(), (identity, digest));
        }
        Ok(digest)
    }
}

/// ADR 0008: the canonical manifest over `entries` (relative path, size and
/// SHA-256), sorted by path.
fn manifest_digest(entries: &mut [(Vec<u8>, u64, [u8; 32])]) -> Result<String, FingerprintError> {
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut manifest = Sha256::new();
    manifest.update(MANIFEST_DOMAIN);
    for (path, size, digest) in entries.iter() {
        if path.iter().any(|&b| b == b'\n' || b == 0) {
            return Err(FingerprintError::UnsafeFile);
        }
        manifest.update(path);
        manifest.update([0]);
        manifest.update(size.to_string().as_bytes());
        manifest.update([0]);
        manifest.update(hex::encode(digest).as_bytes());
        manifest.update(b"\n");
    }
    Ok(format!(
        "{DIGEST_PREFIX}{}",
        hex::encode(manifest.finalize())
    ))
}

/// A symbolic link's own target text, never what it points at.
fn link_digest(path: &Path) -> Result<(u64, [u8; 32]), FingerprintError> {
    let target = std::fs::read_link(path).map_err(|_| FingerprintError::Changed)?;
    let mut hasher = Sha256::new();
    hasher.update(b"symlink:");
    hasher.update(target.as_os_str().as_bytes());
    Ok((target.as_os_str().len() as u64, hasher.finalize().into()))
}

/// ADR 0029 §2: `lib*.so` or `lib*.so.*`.
fn shared_library(name: &[u8]) -> bool {
    name.starts_with(b"lib")
        && name
            .windows(3)
            .enumerate()
            .any(|(at, window)| window == b".so" && matches!(name.get(at + 3), None | Some(b'.')))
}

/// ADR 0029 §2: `<prefix>/lib` when `bin` is `<prefix>/bin` and that
/// directory exists (not a link), the CMake install layout.
pub(crate) fn install_layout_lib(bin: &Path) -> Option<PathBuf> {
    let lib = bin
        .file_name()
        .filter(|name| *name == "bin")
        .and(bin.parent())?
        .join("lib");
    std::fs::symlink_metadata(&lib)
        .is_ok_and(|metadata| metadata.is_dir())
        .then_some(lib)
}

/// ADR 0029 §2: the version a `libllama.so.<major>.<minor>.<patch>` name in
/// `dirs` gives, read from directory entries only. `None` when there is no
/// such name, or names disagree.
pub fn library_version(dirs: &[&Path]) -> Option<String> {
    let mut found: Option<String> = None;
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten().take(MAX_SITE_ENTRIES) {
            let name = entry.file_name();
            let Some(version) = name.to_str().and_then(|n| n.strip_prefix("libllama.so.")) else {
                continue;
            };
            let parts: Vec<&str> = version.split('.').collect();
            if parts.len() != 3
                || version.len() > MAX_VERSION
                || parts
                    .iter()
                    .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
            {
                continue;
            }
            match &found {
                Some(seen) if seen != version => return None,
                _ => found = Some(version.to_owned()),
            }
        }
    }
    found
}

/// Host policy for a launch that finds its installation drifted
/// (`runtime_profiles.<name>.security.installation_drift`).
pub use capyctl_config::effective::InstallationDrift;

/// What a capability probe reported: probe labels missing per capability.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct CapabilityReport {
    pub missing: BTreeMap<String, Vec<String>>,
}

impl CapabilityReport {
    /// `Some(true)` available, `Some(false)` missing, `None` not probed.
    pub fn available(&self, capability: &str) -> Option<bool> {
        self.missing.get(capability).map(Vec::is_empty)
    }

    /// The capabilities reported missing, in name order.
    pub fn missing_capabilities(&self) -> Vec<String> {
        self.missing
            .iter()
            .filter(|(_, labels)| !labels.is_empty())
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Parse the probe's one-line JSON report for `engine`, strictly.
    pub fn parse(engine: Engine, output: &[u8]) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_slice(output).ok()?;
        let package = package_name(engine)?;
        if value["schema"] != PROBE_SCHEMA || value["engine"] != package {
            return None;
        }
        let names = capability_names(engine);
        let reported = value["capabilities"].as_object()?;
        if reported.len() != names.len() {
            return None;
        }
        let mut missing = BTreeMap::new();
        for name in names {
            let labels = reported.get(*name)?.as_array()?;
            if labels.len() > 64 {
                return None;
            }
            let labels = labels
                .iter()
                .map(|label| {
                    label
                        .as_str()
                        .filter(|l| {
                            !l.is_empty()
                                && l.len() <= 128
                                && l.bytes().all(|b| b.is_ascii_graphic())
                        })
                        .map(str::to_owned)
                })
                .collect::<Option<Vec<_>>>()?;
            missing.insert((*name).to_owned(), labels);
        }
        Some(Self { missing })
    }
}

/// Run the capability probe for one installation, or `None` when it cannot be
/// run or answered (no probe helper in the runtime directory, no interpreter,
/// no package, a timeout, a malformed report). `None` is unknown: it refuses
/// nothing and grants nothing. ADR 0029 §2: a llama.cpp binary has no deep-park
/// capability and nothing is run for it; its report says so.
pub fn probe_capabilities(
    engine: Engine,
    executable: &Path,
    runtime_dir: &Path,
    timeout: Duration,
) -> Option<CapabilityReport> {
    let Some(package) = package_name(engine) else {
        return Some(CapabilityReport {
            missing: capability_names(engine)
                .iter()
                .map(|name| ((*name).to_owned(), vec!["unsupported".to_owned()]))
                .collect(),
        });
    };
    let script = runtime_dir.join(PROBE_SCRIPT);
    if !std::fs::symlink_metadata(&script).is_ok_and(|m| m.is_file()) {
        return None;
    }
    // SPEC §9.1 / T21 T37: the probe runs capyctl's runtime helper under the
    // engine's interpreter, so the directory must pass the same integrity
    // check a launch does, immediately before every run. Unverified is
    // unknown: the helper is never executed.
    crate::runtime_integrity::verify(runtime_dir, &[PROBE_SCRIPT]).ok()?;
    let interpreter = interpreter(executable)?;
    let site = site_packages(engine, executable).ok()?;
    use std::os::unix::process::CommandExt;
    // Its own process group, so a timeout ends everything the probe started.
    // SPEC §9.1 / T21: the probe writes no bytecode beside the checked source
    // and inherits nothing of the agent's environment.
    let mut command = Command::new(&interpreter);
    command
        .env_clear()
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg("-I")
        .arg("-S")
        .arg("-B")
        .arg(&script)
        .arg(package)
        .arg(&site)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    // SPEC §13.2: its own group, so registered with the role's reaper, which
    // leaves its exit status to the wait below.
    let mut child = capyctl_launchers::subreaper::spawn_direct(&mut command).ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = (&mut stdout)
            .take(MAX_PROBE_OUTPUT as u64 + 1)
            .read_to_end(&mut output);
        output
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                if let Ok(group) = i32::try_from(child.id()) {
                    // SAFETY: signals only the probe's own process group.
                    unsafe { libc::kill(-group, libc::SIGKILL) };
                }
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    // A probe that timed out is unknown; its reader is left to finish alone.
    let status = status?;
    let output = reader.join().ok()?;
    if !status.success() || output.len() > MAX_PROBE_OUTPUT {
        return None;
    }
    CapabilityReport::parse(engine, output.trim_ascii())
}

/// Registration (agent start): the installation fields one profile publishes.
/// A measurement that fails is `unmeasured`, never a refusal.
pub fn registration(
    measurer: &InstallationMeasurer,
    engine: Option<Engine>,
    executable: &Path,
) -> (String, String, String) {
    match engine.map(|engine| measurer.measure(engine, executable)) {
        Some(Ok(fingerprint)) => (fingerprint.version, fingerprint.digest, "measured".into()),
        _ => (String::new(), String::new(), "unmeasured".into()),
    }
}

/// Fill one published profile's installation fields from its host document
/// entry (`engine`, `executable`).
pub fn register_profile(
    measurer: &InstallationMeasurer,
    profile: &serde_json::Value,
    status: &mut capyctl_protocol::pb::RuntimeProfileStatus,
) {
    let engine = serde_json::from_value::<Engine>(profile["engine"].clone()).ok();
    let executable = Path::new(profile["executable"].as_str().unwrap_or(""));
    let (version, digest, state) = registration(measurer, engine, executable);
    status.installation_version = version;
    status.installation_digest = digest;
    status.installation_state = state;
}

/// What an observed digest reads as when the installation could not be
/// measured at launch.
pub const UNMEASURED: &str = "unmeasured";

/// The host's registered installations and what its launches found since.
pub struct InstallationRegistry {
    measurer: InstallationMeasurer,
    /// Profile name → digest measured at registration.
    registered: BTreeMap<String, String>,
    /// Profile name → the digest a launch measured instead (drift).
    drift: Mutex<BTreeMap<String, String>>,
    /// Profile name → (installation digest probed, report).
    capabilities: Mutex<BTreeMap<String, (String, CapabilityReport)>>,
    probe_timeout: Duration,
}

impl InstallationRegistry {
    /// The fingerprints this host registered, from its published inventory.
    pub fn from_inventory(inventory: &capyctl_protocol::pb::ReportInventory) -> Self {
        Self {
            measurer: InstallationMeasurer::new(),
            registered: inventory
                .profiles
                .iter()
                .filter(|p| p.installation_state == "measured" && !p.installation_digest.is_empty())
                .map(|p| (p.name.clone(), p.installation_digest.clone()))
                .collect(),
            drift: Mutex::default(),
            capabilities: Mutex::default(),
            probe_timeout: PROBE_TIMEOUT,
        }
    }

    pub fn with_probe_timeout(mut self, timeout: Duration) -> Self {
        self.probe_timeout = timeout;
        self
    }

    /// Measure `profile`'s installation for a launch. Drift from the
    /// registered digest is recorded (and cleared when it measures as
    /// registered again); it refuses only under `installation_drift: refuse`.
    /// Returns the digest measured now (empty when it could not be measured),
    /// which keys the capability cache.
    pub fn verify(
        &self,
        profile: &str,
        engine: Engine,
        executable: &Path,
        policy: InstallationDrift,
    ) -> Result<String, &'static str> {
        let measured = self.measurer.measure(engine, executable);
        let observed = measured
            .as_ref()
            .map_or_else(|_| UNMEASURED.to_owned(), |f| f.digest.clone());
        if let Some(registered) = self.registered.get(profile) {
            let mut drift = self.drift.lock().map_err(|_| "unauthorized")?;
            if observed != *registered {
                drift.insert(profile.to_owned(), observed.clone());
                if policy == InstallationDrift::Refuse {
                    return Err("installation_drift");
                }
            } else {
                drift.remove(profile);
            }
        }
        Ok(measured.map(|f| f.digest).unwrap_or_default())
    }

    /// The capability report for `profile` at installation digest `key`:
    /// cached, or probed now under the installation's interpreter. `None` is
    /// unknown.
    pub fn capabilities(
        &self,
        profile: &str,
        key: &str,
        engine: Engine,
        executable: &Path,
        runtime_dir: &Path,
    ) -> Option<CapabilityReport> {
        if let Some((cached, report)) = self.capabilities.lock().ok()?.get(profile) {
            if cached == key {
                return Some(report.clone());
            }
        }
        let report = probe_capabilities(engine, executable, runtime_dir, self.probe_timeout)?;
        self.capabilities
            .lock()
            .ok()?
            .insert(profile.to_owned(), (key.to_owned(), report.clone()));
        Some(report)
    }

    /// Record a report directly (a probe run elsewhere, or a test).
    pub fn record_capabilities(&self, profile: &str, key: &str, report: CapabilityReport) {
        if let Ok(mut capabilities) = self.capabilities.lock() {
            capabilities.insert(profile.to_owned(), (key.to_owned(), report));
        }
    }

    /// The last report recorded for `profile`, whatever digest it was for.
    pub fn last_capabilities(&self, profile: &str) -> Option<CapabilityReport> {
        self.capabilities
            .lock()
            .ok()?
            .get(profile)
            .map(|(_, report)| report.clone())
    }

    /// The digest `profile` registered, when it was measured at registration.
    pub fn registered(&self, profile: &str) -> Option<&str> {
        self.registered.get(profile).map(String::as_str)
    }

    /// The digest a launch last measured instead of the registered one, while
    /// `profile` stands drifted.
    pub fn drifted(&self, profile: &str) -> Option<String> {
        self.drift.lock().ok()?.get(profile).cloned()
    }

    /// Status: overlay drift and missing capabilities on published profiles.
    pub fn overlay(&self, profiles: &mut [capyctl_protocol::pb::RuntimeProfileStatus]) {
        let drift = self.drift.lock().map(|d| d.clone()).unwrap_or_default();
        let capabilities = self
            .capabilities
            .lock()
            .map(|c| c.clone())
            .unwrap_or_default();
        for profile in profiles {
            if let Some(observed) = drift.get(&profile.name) {
                profile.installation_state = "drifted".into();
                profile.installation_observed_digest = observed.clone();
            } else if profile.installation_state == "drifted" {
                profile.installation_state = "measured".into();
                profile.installation_observed_digest.clear();
            }
            if let Some((_, report)) = capabilities.get(&profile.name) {
                profile.capabilities_missing = report.missing_capabilities();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    // T41: the TensorFold probe report parses with its three capabilities.
    #[test]
    fn a_tensorfold_probe_report_parses() {
        let good = br#"{"schema":"capyctl/engine-capabilities/v1","engine":"tensorfold","capabilities":{"core":[],"deep_park":["unsupported"],"metrics":[]}}"#;
        let report = CapabilityReport::parse(Engine::Tensorfold, good).unwrap();
        assert_eq!(report.available("core"), Some(true));
        assert_eq!(report.available("deep_park"), Some(false));
    }

    /// A synthetic virtual environment with an engine package tree.
    fn venv(engine: Engine, version: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("python3"), "").unwrap();
        let site = dir.path().join("lib/python3.12/site-packages");
        let name = package_name(engine).unwrap();
        let package = site.join(name);
        std::fs::create_dir_all(package.join("srt/__pycache__")).unwrap();
        std::fs::write(package.join("__init__.py"), "# custom build\n").unwrap();
        std::fs::write(
            package.join("srt/server_args.py"),
            "class ServerArgs: pass\n",
        )
        .unwrap();
        std::fs::write(
            package.join("srt/__pycache__/server_args.cpython-312.pyc"),
            "x",
        )
        .unwrap();
        let dist = site.join(format!("{name}-{version}.dist-info"));
        std::fs::create_dir_all(&dist).unwrap();
        std::fs::write(
            dist.join("METADATA"),
            format!("Name: x\nVersion: {version}\n"),
        )
        .unwrap();
        // A sibling package whose name shares the prefix is not the engine.
        std::fs::create_dir_all(site.join("sglang_kernel-0.4.7.dist-info")).unwrap();
        dir
    }

    // T21 T22: registration records version and a manifest digest; bytecode
    // caches and permission bits never enter it.
    #[test]
    fn a_custom_build_is_fingerprinted_by_version_and_manifest() {
        let dir = venv(Engine::Sglang, "0.5.20+custom");
        let measurer = InstallationMeasurer::new();
        let executable = dir.path().join("bin/python3");
        let first = measurer.measure(Engine::Sglang, &executable).unwrap();
        assert_eq!(first.version, "0.5.20+custom");
        assert!(first.digest.starts_with(DIGEST_PREFIX));
        assert_eq!(first.files, 2);
        // A rewritten bytecode cache is not drift.
        std::fs::write(
            dir.path().join(
                "lib/python3.12/site-packages/sglang/srt/__pycache__/server_args.cpython-312.pyc",
            ),
            "rewritten",
        )
        .unwrap();
        assert_eq!(
            measurer.measure(Engine::Sglang, &executable).unwrap(),
            first
        );
        // T37: a group-writable installation is measured like any other; no
        // permission rule applies to engine installation files.
        for entry in [
            "lib",
            "lib/python3.12/site-packages/sglang",
            "lib/python3.12/site-packages/sglang/srt/server_args.py",
        ] {
            let path = dir.path().join(entry);
            let mode = if path.is_dir() { 0o775 } else { 0o664 };
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        assert_eq!(
            measurer.measure(Engine::Sglang, &executable).unwrap(),
            first
        );
        // A second measurer (no cache) computes the same digest.
        assert_eq!(
            InstallationMeasurer::new()
                .measure(Engine::Sglang, &executable)
                .unwrap(),
            first
        );
    }

    // T21 T22: any change to the package's files is a different digest.
    #[test]
    fn a_changed_file_changes_the_digest() {
        let dir = venv(Engine::Vllm, "0.29.0");
        let measurer = InstallationMeasurer::new();
        let executable = dir.path().join("bin/vllm");
        std::fs::write(&executable, "").unwrap();
        let before = measurer.measure(Engine::Vllm, &executable).unwrap();
        std::fs::write(
            dir.path()
                .join("lib/python3.12/site-packages/vllm/srt/server_args.py"),
            "class ServerArgs: patched = True\n",
        )
        .unwrap();
        let after = measurer.measure(Engine::Vllm, &executable).unwrap();
        assert_ne!(before.digest, after.digest);
        assert_eq!(before.version, after.version);
    }

    #[test]
    fn an_installation_without_the_package_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        assert_eq!(
            InstallationMeasurer::new().measure(Engine::Sglang, &dir.path().join("bin/python3")),
            Err(FingerprintError::NotFound)
        );
    }

    // T21 T22: drift is flagged in status and refused only when the host
    // policy says `refuse`; a later launch that measures as registered clears it.
    #[test]
    fn drift_is_flagged_and_refused_only_under_the_refuse_policy() {
        let dir = venv(Engine::Sglang, "0.5.20");
        let executable = dir.path().join("bin/python3");
        let measurer = InstallationMeasurer::new();
        let mut status = capyctl_protocol::pb::RuntimeProfileStatus {
            name: "local".into(),
            ..Default::default()
        };
        register_profile(
            &measurer,
            &serde_json::json!({"engine": "sglang", "executable": executable}),
            &mut status,
        );
        assert_eq!(status.installation_state, "measured");
        assert_eq!(status.installation_version, "0.5.20");
        let registered = status.installation_digest.clone();
        let inventory = capyctl_protocol::pb::ReportInventory {
            profiles: vec![status.clone()],
            ..Default::default()
        };
        let registry = InstallationRegistry::from_inventory(&inventory);
        assert_eq!(
            registry.verify(
                "local",
                Engine::Sglang,
                &executable,
                InstallationDrift::Warn
            ),
            Ok(registered.clone())
        );
        let file = dir
            .path()
            .join("lib/python3.12/site-packages/sglang/__init__.py");
        std::fs::write(&file, "# patched after registration\n").unwrap();
        let observed = registry
            .verify(
                "local",
                Engine::Sglang,
                &executable,
                InstallationDrift::Warn,
            )
            .unwrap();
        assert_ne!(observed, registered);
        let mut profiles = vec![status.clone()];
        registry.overlay(&mut profiles);
        assert_eq!(profiles[0].installation_state, "drifted");
        assert_eq!(profiles[0].installation_observed_digest, observed);
        assert_eq!(profiles[0].installation_digest, registered);
        assert_eq!(
            registry.verify(
                "local",
                Engine::Sglang,
                &executable,
                InstallationDrift::Refuse
            ),
            Err("installation_drift")
        );
        std::fs::write(&file, "# custom build\n").unwrap();
        registry
            .verify(
                "local",
                Engine::Sglang,
                &executable,
                InstallationDrift::Refuse,
            )
            .unwrap();
        registry.overlay(&mut profiles);
        assert_eq!(profiles[0].installation_state, "measured");
        assert!(profiles[0].installation_observed_digest.is_empty());
        // An unregistered (unmeasured) installation is never drift.
        let unregistered = InstallationRegistry::from_inventory(&Default::default());
        std::fs::write(&file, "# anything\n").unwrap();
        unregistered
            .verify(
                "local",
                Engine::Sglang,
                &executable,
                InstallationDrift::Refuse,
            )
            .unwrap();
    }

    #[test]
    fn a_probe_report_parses_strictly() {
        let good = br#"{"schema":"capyctl/engine-capabilities/v1","engine":"sglang","capabilities":{"core":[],"deep_park":["route:/release_memory_occupation"],"metrics":[],"observation":[]}}"#;
        let report = CapabilityReport::parse(Engine::Sglang, good).unwrap();
        assert_eq!(report.available("core"), Some(true));
        assert_eq!(report.available("deep_park"), Some(false));
        assert_eq!(report.missing_capabilities(), vec!["deep_park".to_owned()]);
        for bad in [
            &br#"{"schema":"other","engine":"sglang","capabilities":{}}"#[..],
            br#"{"schema":"capyctl/engine-capabilities/v1","engine":"vllm","capabilities":{"core":[],"deep_park":[],"metrics":[],"observation":[]}}"#,
            br#"{"schema":"capyctl/engine-capabilities/v1","engine":"sglang","capabilities":{"core":[]}}"#,
            br#"{"schema":"capyctl/engine-capabilities/v1","engine":"sglang","capabilities":{"core":[1],"deep_park":[],"metrics":[],"observation":[]}}"#,
            b"not json",
        ] {
            assert_eq!(CapabilityReport::parse(Engine::Sglang, bad), None);
        }
    }

    // T21 T37: SPEC §9.1. The probe executes capyctl's runtime helper, so the
    // runtime directory is verified first; an untrusted one is never run.
    #[test]
    fn the_probe_never_runs_an_unverified_runtime_directory() {
        let dir = venv(Engine::Sglang, "0.5.20");
        let runtime = tempfile::tempdir().unwrap();
        std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let executable = dir.path().join("bin/python3");
        let marker = runtime.path().join("ran");
        capyctl_config::test_support::write_executable(
            &executable,
            format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
            0o755,
        )
        .unwrap();
        let script = runtime.path().join(PROBE_SCRIPT);
        std::fs::write(&script, "# helper\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(
            probe_capabilities(Engine::Sglang, &executable, runtime.path(), PROBE_TIMEOUT),
            None
        );
        assert!(!marker.exists(), "an unverified helper was executed");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(runtime.path().join("stale.pyc"), "x").unwrap();
        assert_eq!(
            probe_capabilities(Engine::Sglang, &executable, runtime.path(), PROBE_TIMEOUT),
            None
        );
        assert!(!marker.exists(), "an unverified helper was executed");
    }

    // T21 T22: the probe runs the installation's interpreter on capyctl's helper
    // and its answer is parsed; a helper that is absent or silent is unknown.
    #[test]
    fn the_probe_runs_the_installation_interpreter_and_unknown_refuses_nothing() {
        let dir = venv(Engine::Sglang, "0.5.20");
        let runtime = tempfile::tempdir().unwrap();
        let executable = dir.path().join("bin/python3");
        assert_eq!(
            probe_capabilities(Engine::Sglang, &executable, runtime.path(), PROBE_TIMEOUT),
            None,
            "no probe helper in the runtime directory"
        );
        std::fs::write(runtime.path().join(PROBE_SCRIPT), "# helper\n").unwrap();
        // The probe runs only from a verified runtime directory (owner-only
        // whatever the umask).
        std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(
            runtime.path().join(PROBE_SCRIPT),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        capyctl_config::test_support::write_executable(
            &executable,
            "#!/bin/sh\necho '{\"schema\":\"capyctl/engine-capabilities/v1\",\"engine\":\"sglang\",\
             \"capabilities\":{\"core\":[],\"deep_park\":[\"torch_memory_saver\"],\"metrics\":[],\
             \"observation\":[]}}'\n",
            0o755,
        )
        .unwrap();
        let report =
            probe_capabilities(Engine::Sglang, &executable, runtime.path(), PROBE_TIMEOUT).unwrap();
        assert_eq!(report.available("deep_park"), Some(false));
        capyctl_config::test_support::write_executable(&executable, "#!/bin/sh\nsleep 5\n", 0o755)
            .unwrap();
        assert_eq!(
            probe_capabilities(
                Engine::Sglang,
                &executable,
                runtime.path(),
                Duration::from_millis(200)
            ),
            None,
            "a probe that does not answer in time is unknown"
        );
    }

    /// A llama.cpp binary in `bin` with a shared library beside it.
    fn llama_server(bin: &Path) -> PathBuf {
        std::fs::create_dir_all(bin).unwrap();
        let binary = bin.join("llama-server");
        std::fs::write(&binary, "elf").unwrap();
        std::fs::write(bin.join("libllama.so.0.6.0"), "llama").unwrap();
        std::os::unix::fs::symlink("libllama.so.0.6.0", bin.join("libllama.so.0")).unwrap();
        binary
    }

    // T42 T22 (ADR 0029 §2, ADR 0008): a bare binary's digest covers it and
    // every `lib*.so*` beside it, and nothing else in the directory; its
    // version is the library's.
    #[test]
    fn a_llama_server_digest_covers_the_libraries_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let binary = llama_server(&dir.path().join("build/bin"));
        let bin = binary.parent().unwrap();
        let measurer = InstallationMeasurer::new();
        let first = measurer.measure(Engine::Llamacpp, &binary).unwrap();
        assert_eq!(first.version, "0.6.0");
        assert_eq!(first.files, 3, "the binary, the library and its link");
        std::fs::write(bin.join("llama-bench"), "another tool").unwrap();
        std::fs::write(bin.join("libnotes.sources"), "not a library").unwrap();
        assert_eq!(
            measurer.measure(Engine::Llamacpp, &binary).unwrap(),
            first,
            "other files are not the installation"
        );
        std::fs::write(bin.join("libggml-cuda.so"), "backend").unwrap();
        let added = measurer.measure(Engine::Llamacpp, &binary).unwrap();
        assert_ne!(added.digest, first.digest, "a new backend library");
        std::fs::write(bin.join("libggml-cuda.so"), "rebuilt backend").unwrap();
        assert_ne!(
            measurer.measure(Engine::Llamacpp, &binary).unwrap().digest,
            added.digest,
            "a rebuilt library"
        );
        std::fs::write(bin.join("libllama.so.0.6.0"), "rebuilt").unwrap();
        assert_ne!(
            measurer.measure(Engine::Llamacpp, &binary).unwrap().digest,
            added.digest
        );
        // Nothing to probe: deep parking is reported missing, nothing runs.
        let report =
            probe_capabilities(Engine::Llamacpp, &binary, dir.path(), PROBE_TIMEOUT).unwrap();
        assert_eq!(report.available("deep_park"), Some(false));
        assert_eq!(report.missing_capabilities(), vec!["deep_park"]);
    }

    // T42 (ADR 0029 §2): for the install layout `<prefix>/bin/llama-server`,
    // `<prefix>/lib` is covered too.
    #[test]
    fn a_llama_server_digest_covers_the_install_prefix_lib() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("prefix");
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        let binary = prefix.join("bin/llama-server");
        std::fs::write(&binary, "elf").unwrap();
        let measurer = InstallationMeasurer::new();
        let alone = measurer.measure(Engine::Llamacpp, &binary).unwrap();
        assert_eq!((alone.files, alone.version.as_str()), (1, "unknown"));
        std::fs::create_dir_all(prefix.join("lib")).unwrap();
        std::fs::write(prefix.join("lib/libllama.so.0.6.0"), "llama").unwrap();
        let installed = measurer.measure(Engine::Llamacpp, &binary).unwrap();
        assert_eq!((installed.files, installed.version.as_str()), (2, "0.6.0"));
        assert_ne!(installed.digest, alone.digest);
        std::fs::write(prefix.join("lib/libggml-base.so.0.26.0"), "ggml").unwrap();
        assert_ne!(
            measurer.measure(Engine::Llamacpp, &binary).unwrap().digest,
            installed.digest
        );
    }
}

//! ADR 0018 §1: engine installations the operator registers. Detection and
//! resolution read package metadata (and, ADR 0029 §2, a bare binary's file
//! names; ADR 0039 §2, a native TensorFold release's `VERSION` file) only;
//! nothing found here is executed until the operator names or picks it (ADR
//! 0008 carve-out 2, SPEC §4.2).
use capyctl_config::engine_policy::Engine;
use std::path::{Path, PathBuf};

pub mod detect;
pub mod resolve;
pub use detect::*;
pub use resolve::*;

/// Entries read from one `site-packages` directory at most.
pub(crate) const MAX_SITE_ENTRIES: usize = 65_536;
/// Bytes read from one `METADATA` file at most.
const MAX_METADATA: u64 = 1 << 20;

fn engine_of(package: &str) -> Option<Engine> {
    // ADR 0018 §1, ADR 0023 §1: the package names of the engine kinds.
    Engine::ALL
        .into_iter()
        .find(|engine| crate::installation::package_name(*engine) == Some(package))
}

/// The environment's `lib/python3.*/site-packages` directories, sorted.
/// Symlinked entries are skipped: a scan never leaves the root it reads.
pub(crate) fn site_packages(env: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(env.join("lib")) else {
        return Vec::new();
    };
    let mut sites: Vec<PathBuf> = entries
        .flatten()
        .take(256)
        .filter(|e| e.file_name().to_string_lossy().starts_with("python3"))
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path().join("site-packages"))
        .filter(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()))
        .collect();
    sites.sort();
    sites
}

fn metadata_version(info: &Path) -> Option<String> {
    use std::io::Read as _;
    let file = std::fs::File::open(info.join("METADATA")).ok()?;
    let mut text = String::new();
    file.take(MAX_METADATA).read_to_string(&mut text).ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("Version: "))
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty() && v.len() <= 128 && v.bytes().all(|b| b.is_ascii_graphic()))
}

/// ADR 0018 §1: the vLLM, SGLang and TensorFold packages an environment holds, from
/// `<name>-<version>.dist-info` directories (not symlinks) and their
/// `METADATA` `Version:` line. Metadata only; bounded.
pub fn packages(env: &Path) -> Vec<(Engine, String)> {
    let mut found = Vec::new();
    for site in site_packages(env) {
        let Ok(entries) = std::fs::read_dir(&site) else {
            continue;
        };
        for entry in entries.flatten().take(MAX_SITE_ENTRIES) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".dist-info") else {
                continue;
            };
            let Some((package, _)) = stem.split_once('-') else {
                continue;
            };
            let Some(engine) = engine_of(&package.to_ascii_lowercase()) else {
                continue;
            };
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            if let Some(version) = metadata_version(&entry.path()) {
                found.push((engine, version));
            }
        }
    }
    found.sort_by(|a, b| (a.0 as u8, &a.1).cmp(&(b.0 as u8, &b.1)));
    found.dedup();
    found
}

/// ADR 0039 §2: what a native TensorFold release root's `VERSION` file says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseVersion {
    /// The root holds no `VERSION` file.
    Absent,
    /// One `MAJOR.MINOR.PATCH` line.
    Stated(String),
    /// A `VERSION` that is not one such line within the bound (or not a
    /// regular file).
    Unreadable,
}

/// ADR 0039 §2: `text` is a `MAJOR.MINOR.PATCH` version (digits only, at most
/// [`capyctl_config::tensorfold_native::VERSION_FILE_LIMIT`] bytes).
pub(crate) fn release_version_text(text: &str) -> Option<String> {
    let parts: Vec<&str> = text.split('.').collect();
    (text.len() as u64 <= capyctl_config::tensorfold_native::VERSION_FILE_LIMIT
        && parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())))
    .then(|| text.to_owned())
}

/// ADR 0039 §2: the root's `VERSION` file, one `MAJOR.MINOR.PATCH` line of at
/// most 32 bytes (a trailing newline allowed), read without following a
/// symbolic link. Nothing is executed.
pub fn release_version(root: &Path) -> ReleaseVersion {
    use std::io::Read as _;
    let path = root.join(capyctl_config::tensorfold_native::VERSION_FILE);
    let Ok(meta) = std::fs::symlink_metadata(&path) else {
        return ReleaseVersion::Absent;
    };
    if !meta.is_file() {
        return ReleaseVersion::Unreadable;
    }
    let mut bytes = Vec::new();
    let read = std::fs::File::open(&path).and_then(|file| {
        file.take(capyctl_config::tensorfold_native::VERSION_FILE_LIMIT + 1)
            .read_to_end(&mut bytes)
    });
    if read.is_err() || bytes.len() as u64 > capyctl_config::tensorfold_native::VERSION_FILE_LIMIT {
        return ReleaseVersion::Unreadable;
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return ReleaseVersion::Unreadable;
    };
    let line = text.strip_suffix('\n').unwrap_or(&text);
    release_version_text(line).map_or(ReleaseVersion::Unreadable, ReleaseVersion::Stated)
}

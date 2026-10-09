//! ADR 0029: llama.cpp's `llama-server`, the fourth engine. Registration
//! constants shared by `engine add`, the roles' own installations and the
//! listings: the executable's name, the version line `--version` writes to
//! standard error, the build fingerprint made from it, and the machine-wide
//! configuration file that refuses an installation.
use crate::{ConfigError, ConfigErrorCode};
use std::path::{Path, PathBuf};

/// ADR 0029 §2: the file detection and `engine add` look for.
pub const EXECUTABLE: &str = "llama-server";

/// ADR 0029 §2: the root `/etc/llama.cpp/config.ini` is read under. Tests
/// name their own root so the result does not depend on the machine.
pub const SYSTEM_ROOT: &str = "/";

/// ADR 0029 §2, §6: llama.cpp fills every option the command line leaves unset
/// from this file, so CapyCTL could not see what the engine runs with.
pub const SYSTEM_CONFIG_FILE: &str = "etc/llama.cpp/config.ini";

/// ADR 0029 §2: each release's tag commit, so a build of the tag configured
/// without `-DLLAMA_BUILD_IS_DEV=OFF` (which reports `<v>-dev`) counts as that
/// release.
pub const RELEASE_COMMITS: &[(&str, &str)] = &[("0.6.0", "d812350")];

/// Git's shortest abbreviation; a commit compared with a tag's is at least
/// this long.
const MIN_COMMIT_LEN: usize = 7;
const MAX_VERSION_LEN: usize = 128;
const MAX_COMMIT_LEN: usize = 64;

/// `/etc/llama.cpp/config.ini` under `root`.
pub fn system_config_file(root: &Path) -> PathBuf {
    root.join(SYSTEM_CONFIG_FILE)
}

/// ADR 0029 §2: why an installation on a machine with the system
/// configuration file is refused, or `None` when there is none. Any entry at
/// that path counts (`symlink_metadata`, not followed): llama.cpp would read it.
pub fn system_config_refusal(root: &Path) -> Option<String> {
    let file = system_config_file(root);
    std::fs::symlink_metadata(&file).ok().map(|_| {
        format!(
            "{} exists; llama.cpp fills every option CapyCTL leaves unset from it, so \
             llama-server would run with settings CapyCTL cannot see; remove it to use \
             llama.cpp on this machine",
            file.display()
        )
    })
}

/// ADR 0029 §2: what `llama-server --version` reports, as
/// `version: <v> (build <n>, commit <h>)`. The build number counts the
/// commits in the clone, so it is not kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlamacppBuild {
    pub version: String,
    pub commit: String,
}

fn version_token(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_VERSION_LEN
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

fn commit_token(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_COMMIT_LEN
        && text.bytes().all(|b| b.is_ascii_alphanumeric())
}

impl LlamacppBuild {
    /// The `version:` line of `--version`'s standard error. Every other line
    /// (the compiler line, backend notices) is ignored; a malformed version
    /// line, or none, is `None`.
    pub fn parse_version_output(text: &str) -> Option<Self> {
        text.lines()
            .map(str::trim)
            .find_map(|line| line.strip_prefix("version: "))
            .and_then(Self::parse_version_line)
    }

    /// `<v> (build <n>, commit <h>)`.
    fn parse_version_line(rest: &str) -> Option<Self> {
        let (version, rest) = rest.split_once(" (build ")?;
        let (build, rest) = rest.split_once(", commit ")?;
        let commit = rest.strip_suffix(')')?;
        (version_token(version)
            && !build.is_empty()
            && build.bytes().all(|b| b.is_ascii_digit())
            && commit_token(commit))
        .then(|| Self {
            version: version.to_owned(),
            commit: commit.to_owned(),
        })
    }

    /// ADR 0029 §2: the profile's `build_fingerprint`, `<v>+<h>`.
    pub fn fingerprint(&self) -> String {
        format!("{}+{}", self.version, self.commit)
    }

    /// A `build_fingerprint` read back. One without a commit (a role's
    /// stated fingerprint, or a version read from a library name) is the
    /// version alone with an empty commit.
    pub fn from_fingerprint(fingerprint: &str) -> Option<Self> {
        let (version, commit) = fingerprint.split_once('+').unwrap_or((fingerprint, ""));
        (version_token(version) && (commit.is_empty() || commit_token(commit))).then(|| Self {
            version: version.to_owned(),
            commit: commit.to_owned(),
        })
    }

    /// ADR 0029 §2: the release this build counts as: its version, or a
    /// `<v>-dev` build of `<v>`'s tag commit as `<v>`.
    pub fn release(&self) -> &str {
        match self.version.strip_suffix("-dev") {
            Some(base) if self.is_tag_commit(base) => base,
            _ => &self.version,
        }
    }

    fn is_tag_commit(&self, release: &str) -> bool {
        let commit = self.commit.to_ascii_lowercase();
        commit.len() >= MIN_COMMIT_LEN
            && RELEASE_COMMITS.iter().any(|(version, tag)| {
                *version == release && (commit.starts_with(tag) || tag.starts_with(&commit))
            })
    }
}

/// ADR 0029 §1: a llama.cpp profile registers, but deployments on it are
/// refused until the option policy, launch settings and sizing of ADR 0029
/// §5, §6 and §9 are in the release.
pub fn deployment_unsupported() -> ConfigError {
    ConfigError::new(
        ConfigErrorCode::UnsupportedCombination,
        "runtime_profile",
        "llama.cpp deployments are not supported by this release; the profile is \
         registered, but nothing can be deployed on it yet",
    )
}

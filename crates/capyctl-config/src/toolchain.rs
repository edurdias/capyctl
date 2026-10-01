//! ADR 0023 §2: the tools TensorFold's first start needs to build its CUDA
//! extensions, looked up on the engine's closed launch PATH (SPEC §13.3): the
//! installation's `bin`, the profile's `<cuda_home>/bin`, then the fixed
//! system directories. The caller's PATH is never read and nothing is run.
//! Shared by `engine add`, the host and standalone, so every role checks alike.
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// SPEC §13.3 (amended 2026-09-25): the fixed system tool directories after
/// the engine's own bin and the profile's `<cuda_home>/bin`.
pub const SYSTEM_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// One requirement: its name in messages and the file names that satisfy it.
const TOOLS: &[(&str, &[&str])] = &[
    ("ninja", &["ninja"]),
    ("nvcc", &["nvcc"]),
    ("c++ or g++", &["c++", "g++"]),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolchainMissing {
    pub missing: Vec<&'static str>,
    pub searched: Vec<PathBuf>,
}

impl std::fmt::Display for ToolchainMissing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let searched: Vec<String> = self
            .searched
            .iter()
            .map(|d| d.display().to_string())
            .collect();
        let one = self.missing.len() == 1;
        write!(
            f,
            "TensorFold builds CUDA extensions on its first start and needs {}, which \
             {} not on the engine's PATH ({}); install {} there, or name a CUDA toolkit \
             with CUDA_HOME, then add the engine again",
            self.missing.join(", "),
            if one { "is" } else { "are" },
            searched.join(":"),
            if one { "it" } else { "them" },
        )
    }
}

/// An executable regular file; `metadata` follows a symlink, as the engine's
/// own PATH lookup does. The file is never opened or run.
fn executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `Ok` when every tool is an executable regular file in one of the
/// directories, searched in launch order; `system` is a `:`-separated list
/// (normally [`SYSTEM_PATH`]).
pub fn check(
    engine_bin: &Path,
    cuda_home: Option<&Path>,
    system: &str,
) -> Result<(), ToolchainMissing> {
    let searched: Vec<PathBuf> = std::iter::once(engine_bin.to_path_buf())
        .chain(cuda_home.map(|home| home.join("bin")))
        .chain(
            system
                .split(':')
                .filter(|d| !d.is_empty())
                .map(PathBuf::from),
        )
        .collect();
    let missing: Vec<&'static str> = TOOLS
        .iter()
        .filter(|(_, names)| {
            !searched
                .iter()
                .any(|dir| names.iter().any(|name| executable(&dir.join(name))))
        })
        .map(|(label, _)| *label)
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(ToolchainMissing { missing, searched })
    }
}

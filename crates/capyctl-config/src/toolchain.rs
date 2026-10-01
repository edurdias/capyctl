//! ADR 0023 §2: the tools TensorFold's first start needs to build its CUDA
//! extensions, looked up on the engine's closed launch PATH (SPEC §13.3): the
//! installation's `bin`, the profile's `<cuda_home>/bin`, then the fixed
//! system directories. TensorFold 0.6.1 also builds with a pip-only compiler
//! (`pip install ninja "cuda-toolkit[nvcc,cccl]==13.0.*"`), so `nvcc` is also
//! looked up in the environment's `site-packages/nvidia/cu*/bin`, after the
//! installation's `bin` and before `<cuda_home>/bin`. The caller's PATH is never read and nothing is run.
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
        write!(
            f,
            "TensorFold builds CUDA extensions on its first start and needs {}, which \
             {} not on the engine's PATH ({})",
            self.missing.join(", "),
            if self.missing.len() == 1 { "is" } else { "are" },
            searched.join(":"),
        )
    }
}

impl ToolchainMissing {
    fn install(&self) -> &'static str {
        if self.missing.len() == 1 {
            "it"
        } else {
            "them"
        }
    }

    /// The refusal `engine add` gives: it reads the toolkit from `CUDA_HOME`.
    pub fn for_engine_add(&self) -> String {
        format!(
            "{self}; install {} there, install the compiler into the environment \
             with pip install ninja \"cuda-toolkit[nvcc,cccl]==13.0.*\", or name a CUDA \
             toolkit with CUDA_HOME, then add the engine again",
            self.install()
        )
    }

    /// The refusal a role's own `local_engine` TensorFold gives.
    pub fn for_local_engine(&self) -> String {
        format!(
            "{self}; install {} there, install the compiler into the environment \
             with pip install ninja \"cuda-toolkit[nvcc,cccl]==13.0.*\", or name a CUDA \
             toolkit with --cuda-home, CAPYCTL_CUDA_HOME or local_engine.cuda_home",
            self.install()
        )
    }
}

/// Where the check looks beyond the engine's own `bin` and a stated toolkit:
/// the fixed system directories, and the toolkit `engine add` uses when
/// `CUDA_HOME` names none. Roles use the default; a test names its own so the
/// result does not depend on the machine it runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolchainSearch {
    /// `:`-separated directories, normally [`SYSTEM_PATH`].
    pub system: String,
    pub default_cuda_home: PathBuf,
}

impl Default for ToolchainSearch {
    fn default() -> Self {
        Self {
            system: SYSTEM_PATH.into(),
            default_cuda_home: PathBuf::from(crate::registration::DEFAULT_CUDA_HOME),
        }
    }
}

/// An executable regular file; `metadata` follows a symlink, as the engine's
/// own PATH lookup does. The file is never opened or run.
fn executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The `bin` directories of a pip-installed CUDA compiler in the environment
/// that owns `engine_bin`: `<env>/lib/python3.*/site-packages/nvidia/cu*/bin`.
/// Resolved by listing real directories; a symbolic link is never followed,
/// so nothing outside the environment is read. Sorted for a stable order.
fn pip_nvcc_dirs(engine_bin: &Path) -> Vec<PathBuf> {
    fn dirs(parent: &Path, prefix: &str) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(parent) else {
            return Vec::new();
        };
        let mut found: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
            .map(|e| e.path())
            .collect();
        found.sort();
        found
    }
    let Some(env) = engine_bin.parent() else {
        return Vec::new();
    };
    let real_dir = |path: &Path| std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
    let lib = env.join("lib");
    if !real_dir(&lib) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for python in dirs(&lib, "python3.") {
        let nvidia = python.join("site-packages/nvidia");
        if !real_dir(&python.join("site-packages")) || !real_dir(&nvidia) {
            continue;
        }
        out.extend(dirs(&nvidia, "cu").into_iter().map(|d| d.join("bin")));
    }
    out
}

/// A regular file (not a link) that is executable, for the pip compiler.
fn own_executable(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
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
    let pip = pip_nvcc_dirs(engine_bin);
    let missing: Vec<&'static str> = TOOLS
        .iter()
        .filter(|(label, names)| {
            let on_path = |dir: &PathBuf| names.iter().any(|name| executable(&dir.join(name)));
            if *label == "nvcc" {
                // Launch order: the installation's bin, the pip compiler, the rest.
                return !(on_path(&searched[0])
                    || pip.iter().any(|dir| own_executable(&dir.join("nvcc")))
                    || searched[1..].iter().any(on_path));
            }
            !searched.iter().any(on_path)
        })
        .map(|(label, _)| *label)
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(ToolchainMissing { missing, searched })
    }
}

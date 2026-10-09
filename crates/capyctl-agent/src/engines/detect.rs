//! ADR 0018 §1: `capyctl engine detect`. Reads package metadata only, executes
//! nothing, never follows a symlink that resolves outside the root being
//! scanned, and is bounded in environments, depth and directory entries.
//! ADR 0029 §2: llama.cpp's bare `llama-server` binary is a second source,
//! found by file name, its version read from a `libllama.so.X.Y.Z` name.
use super::{packages, resolve::entry, resolve::library_dirs, site_packages};
use capyctl_config::engine_policy::Engine;
use capyctl_config::llamacpp::EXECUTABLE;
use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub engine: Engine,
    pub version: String,
    pub env: PathBuf,
    pub entry: PathBuf,
    /// Where it was found: `PATH`, `conda`, `home`, `venv`, `uv`, `pipx`,
    /// `opt`, `system` (ADR 0029 §2: `/usr/local/bin`) or `path`. For a bare
    /// binary, `env` is the directory holding it.
    pub source: &'static str,
    pub custom: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct ScanBounds {
    pub max_envs: usize,
    pub max_depth: usize,
    pub max_dir_entries: usize,
}

impl Default for ScanBounds {
    fn default() -> Self {
        Self {
            max_envs: 512,
            max_depth: 3,
            max_dir_entries: 4096,
        }
    }
}

/// Where detection looks. `from_env` fills it from the process environment;
/// tests build it directly.
#[derive(Debug, Clone, Default)]
pub struct ScanRoots {
    pub path_dirs: Vec<PathBuf>,
    pub home: Option<PathBuf>,
    pub xdg_data: Option<PathBuf>,
    pub pipx_home: Option<PathBuf>,
    pub conda_roots: Vec<PathBuf>,
    pub opt: Option<PathBuf>,
    /// ADR 0029 §2: fixed binary directories (`/usr/local/bin`).
    pub system_bins: Vec<PathBuf>,
    pub extra: Vec<PathBuf>,
}

/// ADR 0018 §1 (owner decision 2026-09-25): the conda roots detection reads.
const HOME_CONDA_ROOTS: &[&str] = &[
    "miniconda3",
    "anaconda3",
    "miniforge3",
    "mambaforge",
    ".conda",
];
const SYSTEM_CONDA_ROOTS: &[&str] = &["/opt/conda", "/opt/miniconda3", "/opt/anaconda3"];

impl ScanRoots {
    pub fn from_env(extra: Vec<PathBuf>) -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute());
        let path_dirs = std::env::var_os("PATH")
            .map(|p| {
                std::env::split_paths(&p)
                    .filter(|d| d.is_absolute())
                    .take(256)
                    .collect()
            })
            .unwrap_or_default();
        let xdg_data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| home.as_ref().map(|h| h.join(".local/share")));
        let pipx_home = std::env::var_os("PIPX_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| home.as_ref().map(|h| h.join(".local/pipx")));
        let mut conda_roots: Vec<PathBuf> = home
            .iter()
            .flat_map(|h| HOME_CONDA_ROOTS.iter().map(move |r| h.join(r)))
            .collect();
        conda_roots.extend(SYSTEM_CONDA_ROOTS.iter().map(PathBuf::from));
        Self {
            path_dirs,
            home,
            xdg_data,
            pipx_home,
            conda_roots,
            opt: Some("/opt".into()),
            system_bins: vec!["/usr/local/bin".into()],
            extra,
        }
    }
}

struct Scan<'a> {
    bounds: &'a ScanBounds,
    seen: BTreeSet<PathBuf>,
    examined: usize,
    found: Vec<Candidate>,
}

impl Scan<'_> {
    /// Examine one environment root. Metadata only.
    fn env(&mut self, env: &Path, source: &'static str) {
        if self.examined >= self.bounds.max_envs {
            return;
        }
        let Ok(key) = env.canonicalize() else { return };
        if !self.seen.insert(key) || site_packages(env).is_empty() {
            return;
        }
        self.examined += 1;
        for (engine, version) in packages(env) {
            let entry = entry(env, engine);
            if std::fs::symlink_metadata(&entry).is_err() {
                continue;
            }
            let custom = !capyctl_config::registration::is_verified(engine, &version);
            self.found.push(Candidate {
                engine,
                version,
                env: env.to_path_buf(),
                entry,
                source,
                custom,
            });
        }
    }

    /// ADR 0029 §2: an executable regular file named `llama-server` in `dir`.
    /// A link is followed only to a file inside `root`; the version is a
    /// `libllama.so.X.Y.Z` name beside it, else `unknown`. Metadata only.
    fn binary(&mut self, dir: &Path, root: &Path, source: &'static str) {
        if self.examined >= self.bounds.max_envs {
            return;
        }
        let named = dir.join(EXECUTABLE);
        let Ok(meta) = std::fs::symlink_metadata(&named) else {
            return;
        };
        let file = if meta.file_type().is_symlink() {
            match (named.canonicalize(), root.canonicalize()) {
                (Ok(target), Ok(root)) if target.starts_with(&root) => target,
                _ => return,
            }
        } else {
            named
        };
        let regular_executable = std::fs::symlink_metadata(&file)
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
        let Ok(key) = file.canonicalize() else { return };
        if !regular_executable || !self.seen.insert(key) {
            return;
        }
        self.examined += 1;
        let Some(bin) = file.parent().map(Path::to_path_buf) else {
            return;
        };
        let dirs = library_dirs(&bin);
        let dirs: Vec<&Path> = dirs.iter().map(PathBuf::as_path).collect();
        let version =
            crate::installation::library_version(&dirs).unwrap_or_else(|| "unknown".into());
        let custom = !capyctl_config::registration::is_verified(Engine::Llamacpp, &version);
        self.found.push(Candidate {
            engine: Engine::Llamacpp,
            version,
            env: bin,
            entry: file,
            source,
            custom,
        });
    }

    /// The directories directly under `root`; a symlink is kept only if it
    /// resolves inside `root` (spec: never follow one out of the root).
    fn children(&self, root: &Path) -> Vec<PathBuf> {
        let Ok(canonical_root) = root.canonicalize() else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(root) else {
            return Vec::new();
        };
        let mut children: Vec<PathBuf> = entries
            .flatten()
            .take(self.bounds.max_dir_entries)
            .filter_map(|e| {
                let path = e.path();
                let kind = e.file_type().ok()?;
                if kind.is_symlink() {
                    let target = path.canonicalize().ok()?;
                    (target.starts_with(&canonical_root) && target.is_dir()).then_some(path)
                } else {
                    kind.is_dir().then_some(path)
                }
            })
            .collect();
        children.sort();
        children
    }

    /// `--path`: the directory itself, or environments and `llama-server`
    /// binaries below it, to `depth`. A link leads nowhere outside `top`.
    fn tree(&mut self, root: &Path, top: &Path, depth: usize) {
        self.binary(root, top, "path");
        if !site_packages(root).is_empty() {
            self.env(root, "path");
            return;
        }
        if depth == 0 {
            return;
        }
        for child in self.children(root) {
            self.tree(&child, top, depth - 1);
        }
    }
}

/// ADR 0018 §1: candidates in the documented locations, deduplicated by
/// canonical environment (or, ADR 0029 §2, canonical binary), in scan order.
pub fn detect(roots: &ScanRoots, bounds: &ScanBounds) -> Vec<Candidate> {
    let mut scan = Scan {
        bounds,
        seen: BTreeSet::new(),
        examined: 0,
        found: Vec::new(),
    };
    for dir in &roots.path_dirs {
        if dir.file_name().is_some_and(|n| n == "bin") {
            if let Some(env) = dir.parent() {
                scan.env(env, "PATH");
            }
        }
        // ADR 0029 §2: a `llama-server` in any PATH entry.
        scan.binary(dir, dir, "PATH");
    }
    if let Some(home) = &roots.home {
        let listed = home.join(".conda/environments.txt");
        if let Ok(text) = std::fs::read_to_string(&listed) {
            for line in text
                .lines()
                .take(256)
                .map(str::trim)
                .filter(|l| l.starts_with('/'))
            {
                scan.env(Path::new(line), "conda");
            }
        }
    }
    for root in &roots.conda_roots {
        scan.env(root, "conda");
        for env in scan.children(&root.join("envs")) {
            scan.env(&env, "conda");
        }
    }
    if let Some(home) = &roots.home {
        // Owner decision 2026-09-25: a venv directly in the home directory,
        // one level deep, recognised by its `pyvenv.cfg` (metadata only).
        for child in scan.children(home) {
            if std::fs::symlink_metadata(child.join("pyvenv.cfg")).is_ok_and(|m| m.is_file()) {
                scan.env(&child, "home");
            }
        }
        scan.env(&home.join(".venv"), "venv");
        for parent in ["venvs", ".virtualenvs"] {
            for env in scan.children(&home.join(parent)) {
                scan.env(&env, "venv");
            }
        }
        for env in scan.children(&home.join(".local/share/pipx/venvs")) {
            scan.env(&env, "pipx");
        }
        // ADR 0029 §2: llama.cpp's own build directories.
        let source = home.join("llama.cpp");
        for build in scan.children(&source) {
            if build
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("build"))
            {
                scan.binary(&build.join("bin"), &source, "home");
            }
        }
    }
    if let Some(data) = &roots.xdg_data {
        for env in scan.children(&data.join("uv/tools")) {
            scan.env(&env, "uv");
        }
    }
    if let Some(pipx) = &roots.pipx_home {
        for env in scan.children(&pipx.join("venvs")) {
            scan.env(&env, "pipx");
        }
    }
    if let Some(opt) = &roots.opt {
        for child in scan.children(opt) {
            scan.env(&child, "opt");
            scan.env(&child.join("venv"), "opt");
            scan.env(&child.join(".venv"), "opt");
            // ADR 0029 §2: `/opt/*/bin/llama-server`.
            scan.binary(&child.join("bin"), &child, "opt");
        }
    }
    for dir in &roots.system_bins {
        scan.binary(dir, dir, "system");
    }
    for extra in &roots.extra {
        scan.tree(extra, extra, bounds.max_depth);
    }
    scan.found
}

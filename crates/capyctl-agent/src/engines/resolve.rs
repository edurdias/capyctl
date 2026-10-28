//! ADR 0018 §1: from a path the operator named to the environment and the
//! entry point the engine is launched with, then the bounded version check.
//! ADR 0029 §2: a llama.cpp installation is a bare `llama-server` binary.
//! ADR 0038 §2: a TensorRT-LLM installation is a virtual environment holding
//! the `tensorrt_llm` distribution; its version check reads metadata only.
use super::packages;
use capyctl_config::engine_policy::Engine;
use capyctl_config::llamacpp::{LlamacppBuild, EXECUTABLE};
use capyctl_config::tensorrt_llm;
use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const VERSION_CHECK_TIMEOUT: Duration = Duration::from_secs(60);
pub const VERSION_OUTPUT_LIMIT: usize = 4096;
/// Reads the installed version the interpreter would import, without
/// importing the engine.
const SGLANG_VERSION: &str =
    "import importlib.metadata,sys;print(importlib.metadata.version(sys.argv[1]))";
/// ADR 0038 §2: one `<name> <version>` line per distribution named after the
/// script (`<name> ` alone when it is not installed), from the installed
/// metadata only. Nothing imports `tensorrt_llm`, whose import initializes
/// MPI and CUDA.
pub const TENSORRT_LLM_VERSIONS: &str = concat!(
    "import importlib.metadata as m,sys\n",
    "for n in sys.argv[1:]:\n",
    " try:v=m.version(n)\n",
    " except m.PackageNotFoundError:v=''\n",
    " print(n,v)\n",
);

/// ADR 0038 §2: the interpreter the version check runs, the environment's
/// own (never resolved through its link, so it keeps its environment).
pub fn tensorrt_llm_interpreter(env: &Path) -> PathBuf {
    env.join("bin/python3")
}

/// ADR 0038 §2: the version check's arguments after the interpreter:
/// isolated mode, no bytecode, the metadata script, then `tensorrt_llm` and
/// each companion distribution.
pub fn tensorrt_llm_check_args() -> Vec<&'static str> {
    let mut args = vec![
        "-I",
        "-B",
        "-c",
        TENSORRT_LLM_VERSIONS,
        tensorrt_llm::DISTRIBUTION,
    ];
    args.extend_from_slice(tensorrt_llm::COMPANIONS);
    args
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub engine: Engine,
    pub version: String,
    pub env: PathBuf,
    pub executable: PathBuf,
}

impl Resolved {
    /// ADR 0018 §1: outside the verified set. ADR 0038 §2: TensorRT-LLM's
    /// counts beside the torch build its environment's metadata names.
    pub fn custom(&self) -> bool {
        !super::verified_in_env(self.engine, &self.version, &self.env)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    NotFound(String),
    Unsupported(String),
}

impl ResolveError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "engine_not_found",
            Self::Unsupported(_) => "engine_unsupported",
        }
    }
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(m) | Self::Unsupported(m) => f.write_str(m),
        }
    }
}

pub(crate) fn entry(env: &Path, engine: Engine) -> PathBuf {
    match engine {
        Engine::Vllm => env.join("bin/vllm"),
        Engine::Sglang => env.join("bin/python3"),
        Engine::Tensorfold => env.join("bin/tensorfold"),
        // ADR 0029 §2: `env` is the directory holding the binary.
        Engine::Llamacpp => env.join(EXECUTABLE),
        // ADR 0038 §2: the console script `<env>/bin/trtllm-serve`.
        Engine::TensorrtLlm => env.join("bin").join(tensorrt_llm::EXECUTABLE),
        // ADR 0031 §1: a custom engine's executable is named, never derived;
        // `env` is it.
        Engine::Custom => env.to_path_buf(),
    }
}

/// ADR 0029 §2: the directories a bare binary's `lib*.so*` files are read
/// from: its own, and `<prefix>/lib` for `<prefix>/bin/<binary>`.
pub(crate) fn library_dirs(dir: &Path) -> Vec<PathBuf> {
    std::iter::once(dir.to_path_buf())
        .chain(crate::installation::install_layout_lib(dir))
        .collect()
}

/// ADR 0029 §2: `binary` is a `llama-server` regular file (a link is named by
/// what it points to, so the libraries beside it are the ones it loads). Its
/// version is a `libllama.so.X.Y.Z` name beside it, else `unknown` until the
/// version check reads it. Nothing is executed.
fn resolve_binary(binary: PathBuf) -> Result<Resolved, ResolveError> {
    let meta = std::fs::symlink_metadata(&binary)
        .map_err(|_| ResolveError::NotFound(format!("{} does not exist", binary.display())))?;
    if meta.file_type().is_symlink() {
        return Err(ResolveError::Unsupported(
            match std::fs::canonicalize(&binary) {
                Ok(target) => format!(
                    "{} is a symbolic link; name the file it points to, {}",
                    binary.display(),
                    target.display()
                ),
                Err(_) => format!("{} is a symbolic link to nothing", binary.display()),
            },
        ));
    }
    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return Err(ResolveError::Unsupported(format!(
            "{} is not an executable file",
            binary.display()
        )));
    }
    let env = binary
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| ResolveError::NotFound(format!("{} has no directory", binary.display())))?;
    let dirs = library_dirs(&env);
    let dirs: Vec<&Path> = dirs.iter().map(PathBuf::as_path).collect();
    Ok(Resolved {
        engine: Engine::Llamacpp,
        version: crate::installation::library_version(&dirs).unwrap_or_else(|| "unknown".into()),
        env,
        executable: binary,
    })
}

/// ADR 0018 §1: `path` is a venv directory, its `bin/vllm`, or its
/// `bin/python3` (`python`, `python3.N`). Lexical: a venv's interpreter is a
/// symlink to the system Python and is never followed (Review Focus 2).
/// ADR 0029 §2: or a `llama-server` binary, or the directory holding one.
/// ADR 0038 §2: or a TensorRT-LLM environment's `bin/trtllm-serve`; the name
/// and version are read from its `tensorrt_llm` dist-info.
pub fn resolve(path: &Path) -> Result<Resolved, ResolveError> {
    let path = std::path::absolute(path).map_err(|_| {
        ResolveError::Unsupported(format!("{} cannot be made absolute", path.display()))
    })?;
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(ResolveError::Unsupported(format!(
            "{} contains '..'; name the environment directly",
            path.display()
        )));
    }
    // ADR 0029 §2: a llama.cpp binary, named or in the named directory.
    if path.file_name().is_some_and(|name| name == EXECUTABLE) {
        return resolve_binary(path);
    }
    let meta = std::fs::metadata(&path)
        .map_err(|_| ResolveError::NotFound(format!("{} does not exist", path.display())))?;
    if meta.is_dir() && std::fs::symlink_metadata(path.join(EXECUTABLE)).is_ok() {
        return resolve_binary(path.join(EXECUTABLE));
    }
    let (env, wanted) = if meta.is_dir() {
        (path.clone(), None)
    } else {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let bin = path
            .parent()
            .filter(|p| p.file_name().is_some_and(|n| n == "bin"));
        let env = bin
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .ok_or_else(|| {
                ResolveError::Unsupported(format!(
                    "{} is not inside an environment's bin directory",
                    path.display()
                ))
            })?;
        let wanted = if name == "vllm" {
            Engine::Vllm
        } else if name == "tensorfold" {
            Engine::Tensorfold
        } else if name == tensorrt_llm::EXECUTABLE {
            Engine::TensorrtLlm
        } else if name == "python" || name == "python3" || name.starts_with("python3.") {
            Engine::Sglang
        } else {
            return Err(ResolveError::Unsupported(format!(
                "{} is not bin/vllm, bin/tensorfold, bin/trtllm-serve, bin/python3 or \
                 llama-server",
                path.display()
            )));
        };
        (env, Some(wanted))
    };
    if super::site_packages(&env).is_empty() {
        return Err(ResolveError::Unsupported(format!(
            "{} is not a Python environment (no lib/python3.*/site-packages) and holds no \
             llama-server",
            env.display()
        )));
    }
    let found = packages(&env);
    if found.is_empty() {
        return Err(ResolveError::NotFound(format!(
            "{} holds no vllm, sglang, tensorfold or tensorrt_llm package",
            env.display()
        )));
    }
    let (engine, version) = match wanted {
        Some(engine) => found
            .iter()
            .find(|(e, _)| *e == engine)
            .cloned()
            .ok_or_else(|| {
                ResolveError::NotFound(format!(
                    "{} holds no {} package",
                    env.display(),
                    capyctl_agent_engine_name(engine)
                ))
            })?,
        None if found.len() == 1 => found[0].clone(),
        None => {
            let entries: Vec<String> = found
                .iter()
                .map(|(engine, _)| {
                    format!("{} for {}", entry(&env, *engine).display(), engine.name())
                })
                .collect();
            return Err(ResolveError::Unsupported(format!(
                "{} holds several engines; name {}",
                env.display(),
                entries.join(", or ")
            )));
        }
    };
    let executable = entry(&env, engine);
    if std::fs::symlink_metadata(&executable).is_err() {
        return Err(ResolveError::NotFound(format!(
            "{} has the {} package but no {}",
            env.display(),
            capyctl_agent_engine_name(engine),
            executable.display()
        )));
    }
    Ok(Resolved {
        engine,
        version,
        env,
        executable,
    })
}

fn capyctl_agent_engine_name(engine: Engine) -> &'static str {
    crate::installation::package_name(engine).unwrap_or(engine.name())
}

/// ADR 0031 §1: `engine add --custom <executable>`. The path is made
/// absolute lexically, as [`resolve`] does (no `..`, nothing followed), and
/// must name an executable regular file; anything else is
/// `engine_not_found`. A symbolic link is refused with what it points to, so
/// the profile names the file that is measured and run (as for
/// `llama-server`, ADR 0029 §2). Nothing is executed.
pub fn resolve_custom(path: &Path) -> Result<PathBuf, ResolveError> {
    let path = std::path::absolute(path).map_err(|_| {
        ResolveError::NotFound(format!("{} cannot be made absolute", path.display()))
    })?;
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(ResolveError::NotFound(format!(
            "{} contains '..'; name the executable directly",
            path.display()
        )));
    }
    let meta = std::fs::symlink_metadata(&path)
        .map_err(|_| ResolveError::NotFound(format!("{} does not exist", path.display())))?;
    if meta.file_type().is_symlink() {
        return Err(ResolveError::NotFound(match std::fs::canonicalize(&path) {
            Ok(target) => format!(
                "{} is a symbolic link; name the file it points to, {}",
                path.display(),
                target.display()
            ),
            Err(_) => format!("{} is a symbolic link to nothing", path.display()),
        }));
    }
    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return Err(ResolveError::NotFound(format!(
            "{} is not an executable regular file",
            path.display()
        )));
    }
    Ok(path)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionCheckError {
    Spawn,
    TimedOut,
    Failed,
    Output,
    Mismatch {
        reported: String,
        installed: String,
    },
    /// ADR 0029 §2: no `version: <v> (build <n>, commit <h>)` line on
    /// llama-server's standard error.
    Unparsable,
}

impl VersionCheckError {
    /// The closed code `engine add` refuses with. ADR 0029 §2: a llama.cpp
    /// build whose version line does not parse is not one CapyCTL supports.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unparsable => "engine_unsupported",
            _ => "engine_version_failed",
        }
    }
}

impl std::fmt::Display for VersionCheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn => f.write_str("the version check could not be started"),
            Self::TimedOut => f.write_str("the version check timed out"),
            Self::Failed => f.write_str("the version check exited unsuccessfully"),
            Self::Output => f.write_str("the version check printed no usable version"),
            Self::Mismatch {
                reported,
                installed,
            } => write!(
                f,
                "the engine reports {reported} but its package metadata says {installed}"
            ),
            Self::Unparsable => f.write_str(
                "`--version` wrote no `version: <v> (build <n>, commit <h>)` line to standard \
                 error, so this is not a llama-server build CapyCTL can identify",
            ),
        }
    }
}

/// What the version check read: the version, and the profile's
/// `build_fingerprint` (the version itself, or, ADR 0029 §2, `<v>+<commit>`
/// for llama.cpp). ADR 0038 §2: for TensorRT-LLM, the companion versions the
/// same query read (a companion that is not installed is left out); empty
/// for every other engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportedVersion {
    pub version: String,
    pub build_fingerprint: String,
    pub companions: BTreeMap<String, String>,
}

/// ADR 0038 §2: what [`TENSORRT_LLM_VERSIONS`] printed for `tensorrt_llm`
/// and [`tensorrt_llm::COMPANIONS`], strictly: one `<name> <version>` line
/// per distribution in the order asked, `<name> ` for a companion that is
/// not installed, nothing else. `tensorrt_llm` itself must be installed and
/// every version is 1-128 printable characters; anything else is `None`.
pub fn parse_tensorrt_llm_versions(text: &str) -> Option<(String, BTreeMap<String, String>)> {
    let mut lines = text.lines();
    let mut version = None;
    let mut companions = BTreeMap::new();
    for name in
        std::iter::once(tensorrt_llm::DISTRIBUTION).chain(tensorrt_llm::COMPANIONS.iter().copied())
    {
        let value = lines.next()?.strip_prefix(name)?.strip_prefix(' ')?;
        if value.is_empty() && name != tensorrt_llm::DISTRIBUTION {
            continue;
        }
        if !capyctl_config::registration::valid_companion_version(value) {
            return None;
        }
        if name == tensorrt_llm::DISTRIBUTION {
            version = Some(value.to_owned());
        } else {
            companions.insert(name.to_owned(), value.to_owned());
        }
    }
    if lines.any(|line| !line.trim().is_empty()) {
        return None;
    }
    Some((version?, companions))
}

/// ADR 0018 §1: run the installation only now that the operator named it.
/// Cleared environment, stdin and the unread stream closed, own process
/// group, killed at `timeout`, at most [`VERSION_OUTPUT_LIMIT`] bytes kept.
/// The reported version must equal the dist-info version. ADR 0029 §2:
/// `llama-server --version` writes to standard error, which is read instead,
/// and its version line is parsed; there is no dist-info to compare with.
/// ADR 0038 §2: TensorRT-LLM's check runs the environment's interpreter in
/// isolated mode on the installed metadata of `tensorrt_llm` and its
/// companions; it never imports `tensorrt_llm` or runs `trtllm-serve`, either
/// of which would start MPI (and, with Open MPI, a daemon outside the group).
pub fn check_version(
    resolved: &Resolved,
    timeout: Duration,
) -> Result<ReportedVersion, VersionCheckError> {
    let mut command = match resolved.engine {
        Engine::TensorrtLlm => Command::new(tensorrt_llm_interpreter(&resolved.env)),
        _ => Command::new(&resolved.executable),
    };
    match resolved.engine {
        Engine::Vllm | Engine::Tensorfold | Engine::Llamacpp => command.arg("--version"),
        Engine::Sglang => command.args(["-I", "-B", "-c", SGLANG_VERSION, "sglang"]),
        Engine::TensorrtLlm => command.args(tensorrt_llm_check_args()),
        // ADR 0031 §1: a custom engine is never executed to ask its version;
        // its version is its manifest's.
        Engine::Custom => return Err(VersionCheckError::Spawn),
    };
    let reads_stderr = resolved.engine == Engine::Llamacpp;
    let (stdout, stderr) = if reads_stderr {
        (Stdio::null(), Stdio::piped())
    } else {
        (Stdio::piped(), Stdio::null())
    };
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .stdin(Stdio::null())
        .stderr(stderr)
        .stdout(stdout)
        .process_group(0);
    if let Some(home) = std::env::var_os("HOME") {
        command.env("HOME", home);
    }
    // SPEC §13.2: its own group, so registered with the role's reaper, which
    // leaves its exit status to the wait below.
    let mut child = capyctl_launchers::subreaper::spawn_direct(&mut command)
        .map_err(|_| VersionCheckError::Spawn)?;
    let pgid = child.id() as i32;
    let mut pipe: Box<dyn Read + Send> = if reads_stderr {
        Box::new(child.stderr.take().ok_or(VersionCheckError::Spawn)?)
    } else {
        Box::new(child.stdout.take().ok_or(VersionCheckError::Spawn)?)
    };
    let reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = (&mut pipe)
            .take(VERSION_OUTPUT_LIMIT as u64 + 1)
            .read_to_end(&mut buffer);
        buffer
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                // SAFETY: signals only the process group this call created.
                unsafe { libc::kill(-pgid, libc::SIGKILL) };
                let _ = child.wait();
                return Err(VersionCheckError::TimedOut);
            }
        }
    };
    // The group may still hold a writer (a pipeline); end it before reading.
    unsafe { libc::kill(-pgid, libc::SIGKILL) };
    let output = reader.join().map_err(|_| VersionCheckError::Output)?;
    // Too much output first: a writer cut off by the closed pipe also exits
    // unsuccessfully, and the cause is the output.
    if output.len() > VERSION_OUTPUT_LIMIT {
        return Err(VersionCheckError::Output);
    }
    if !status.success() {
        return Err(VersionCheckError::Failed);
    }
    let text = String::from_utf8(output).map_err(|_| VersionCheckError::Output)?;
    if reads_stderr {
        let build =
            LlamacppBuild::parse_version_output(&text).ok_or(VersionCheckError::Unparsable)?;
        return Ok(ReportedVersion {
            build_fingerprint: build.fingerprint(),
            version: build.version,
            companions: BTreeMap::new(),
        });
    }
    let (reported, companions) = if resolved.engine == Engine::TensorrtLlm {
        parse_tensorrt_llm_versions(&text).ok_or(VersionCheckError::Output)?
    } else {
        let line = text
            .lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty())
            .ok_or(VersionCheckError::Output)?;
        (
            line.rsplit(' ').next().unwrap_or(line).to_owned(),
            BTreeMap::new(),
        )
    };
    if reported != resolved.version {
        return Err(VersionCheckError::Mismatch {
            reported,
            installed: resolved.version.clone(),
        });
    }
    Ok(ReportedVersion {
        build_fingerprint: reported.clone(),
        version: reported,
        companions,
    })
}

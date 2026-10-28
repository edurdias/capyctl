//! ADR 0018 §1: from a path the operator named to the environment and the
//! entry point the engine is launched with, then the bounded version check.
//! ADR 0029 §2: a llama.cpp installation is a bare `llama-server` binary.
//! ADR 0039 §2: a native TensorFold installation is a release root holding
//! `bin/tensorfold-native`.
use super::{packages, release_version, release_version_text, ReleaseVersion};
use capyctl_config::engine_policy::Engine;
use capyctl_config::llamacpp::{LlamacppBuild, EXECUTABLE};
use capyctl_config::tensorfold_native::{self, TensorfoldLine};
use std::io::Read;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

pub const VERSION_CHECK_TIMEOUT: Duration = Duration::from_secs(60);
pub const VERSION_OUTPUT_LIMIT: usize = 4096;
/// Reads the installed version the interpreter would import, without
/// importing the engine.
const SGLANG_VERSION: &str =
    "import importlib.metadata,sys;print(importlib.metadata.version(sys.argv[1]))";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub engine: Engine,
    pub version: String,
    /// The environment; a bare binary's directory (ADR 0029 §2); a native
    /// TensorFold release's root (ADR 0039 §2).
    pub env: PathBuf,
    pub executable: PathBuf,
    /// ADR 0039 §1: the TensorFold line, fixed here from the installation's
    /// form (`python` for every other engine).
    pub line: TensorfoldLine,
}

impl Resolved {
    /// ADR 0018 §1: outside the verified set. ADR 0039 §3: a native
    /// TensorFold version outside the certified set.
    pub fn custom(&self) -> bool {
        !capyctl_config::registration::is_verified_line(self.engine, self.line, &self.version)
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
        line: TensorfoldLine::Python,
    })
}

/// ADR 0039 §2: `binary` is a release's `bin/tensorfold-native`, a regular
/// executable file (a link is named by what it points to, so the release
/// measured is the one that runs). The root is the directory above `bin`;
/// the version is its `VERSION` file, else `unknown` until the version check
/// reads it. Nothing is executed.
fn resolve_native(binary: PathBuf) -> Result<Resolved, ResolveError> {
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
    let root = binary
        .parent()
        .filter(|bin| bin.file_name().is_some_and(|name| name == "bin"))
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            ResolveError::Unsupported(format!(
                "{} is not in a TensorFold release's bin directory; name \
                 <release>/bin/{}, its bin directory or the release directory",
                binary.display(),
                tensorfold_native::EXECUTABLE
            ))
        })?;
    let version = match release_version(&root) {
        ReleaseVersion::Stated(version) => version,
        ReleaseVersion::Absent | ReleaseVersion::Unreadable => "unknown".into(),
    };
    Ok(Resolved {
        engine: Engine::Tensorfold,
        version,
        env: root,
        executable: binary,
        line: TensorfoldLine::Native,
    })
}

/// ADR 0039 §2 ("Kind detection on add"): why a `bin/tensorfold` in `env`
/// is not a TensorFold 0.6 entry point: `env` is no Python environment, or a
/// native release root without the `tensorfold` package.
fn stray_tensorfold(path: &Path, env: &Path) -> ResolveError {
    let native = env.join("bin").join(tensorfold_native::EXECUTABLE);
    ResolveError::Unsupported(format!(
        "{} is not in a Python environment with the tensorfold package; TensorFold's \
         native release is registered by its {} binary ({})",
        path.display(),
        tensorfold_native::EXECUTABLE,
        native.display()
    ))
}

/// ADR 0018 §1: `path` is a venv directory, its `bin/vllm`, or its
/// `bin/python3` (`python`, `python3.N`). Lexical: a venv's interpreter is a
/// symlink to the system Python and is never followed (Review Focus 2).
/// ADR 0029 §2: or a `llama-server` binary, or the directory holding one.
/// ADR 0039 §2: or a `tensorfold-native` binary, its `bin` directory, or the
/// release root holding `bin/tensorfold-native`.
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
    // ADR 0039 §2: the native line is a file named `tensorfold-native`.
    if path
        .file_name()
        .is_some_and(|name| name == tensorfold_native::EXECUTABLE)
    {
        return resolve_native(path);
    }
    let meta = std::fs::metadata(&path)
        .map_err(|_| ResolveError::NotFound(format!("{} does not exist", path.display())))?;
    if meta.is_dir() && std::fs::symlink_metadata(path.join(EXECUTABLE)).is_ok() {
        return resolve_binary(path.join(EXECUTABLE));
    }
    // ADR 0039 §2: the release's `bin` directory, or its root.
    if meta.is_dir() {
        for named in [
            path.join(tensorfold_native::EXECUTABLE),
            path.join("bin").join(tensorfold_native::EXECUTABLE),
        ] {
            if std::fs::symlink_metadata(&named).is_ok() {
                return resolve_native(named);
            }
        }
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
        } else if name == "python" || name == "python3" || name.starts_with("python3.") {
            Engine::Sglang
        } else {
            return Err(ResolveError::Unsupported(format!(
                "{} is not bin/vllm, bin/tensorfold, bin/python3 or llama-server",
                path.display()
            )));
        };
        (env, Some(wanted))
    };
    if super::site_packages(&env).is_empty() {
        // ADR 0039 §2, §16: a `tensorfold` file outside a Python environment.
        if wanted == Some(Engine::Tensorfold) {
            return Err(stray_tensorfold(&path, &env));
        }
        return Err(ResolveError::Unsupported(format!(
            "{} is not a Python environment (no lib/python3.*/site-packages) and holds no \
             llama-server",
            env.display()
        )));
    }
    let found = packages(&env);
    // ADR 0039 §2: a `bin/tensorfold` in a native-looking root (one holding
    // `bin/tensorfold-native`) without the tensorfold package.
    if wanted == Some(Engine::Tensorfold)
        && !found
            .iter()
            .any(|(engine, _)| *engine == Engine::Tensorfold)
        && std::fs::symlink_metadata(env.join("bin").join(tensorfold_native::EXECUTABLE)).is_ok()
    {
        return Err(stray_tensorfold(&path, &env));
    }
    if found.is_empty() {
        return Err(ResolveError::NotFound(format!(
            "{} holds no vllm, sglang or tensorfold package",
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
        line: TensorfoldLine::Python,
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
    /// ADR 0039 §2, §16: no `tensorfold-native <MAJOR.MINOR.PATCH>` line on
    /// the native binary's standard output.
    NativeUnreadable,
    /// ADR 0039 §2, §16: the release's `VERSION` file disagrees with what the
    /// binary reports (`stated` is `None` when the file is not one
    /// `MAJOR.MINOR.PATCH` line).
    VersionFile {
        reported: String,
        stated: Option<String>,
    },
}

impl VersionCheckError {
    /// The closed code `engine add` refuses with. ADR 0029 §2: a llama.cpp
    /// build whose version line does not parse is not one CapyCTL supports.
    /// ADR 0039 §16: a native version line that cannot be read, or that
    /// disagrees with `VERSION`, is `engine_version_failed`.
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
            Self::NativeUnreadable => write!(
                f,
                "`--version` wrote no `{} <MAJOR.MINOR.PATCH>` line to standard output, so \
                 CapyCTL cannot read this release's version",
                tensorfold_native::EXECUTABLE
            ),
            Self::VersionFile {
                reported,
                stated: Some(stated),
            } => write!(
                f,
                "the engine reports {reported} but its {} file says {stated}",
                tensorfold_native::VERSION_FILE
            ),
            Self::VersionFile {
                reported,
                stated: None,
            } => write!(
                f,
                "the engine reports {reported} but its {} file is not one MAJOR.MINOR.PATCH \
                 line of at most {} bytes",
                tensorfold_native::VERSION_FILE,
                tensorfold_native::VERSION_FILE_LIMIT
            ),
        }
    }
}

/// What the version check read: the version, and the profile's
/// `build_fingerprint` (the version itself, or, ADR 0029 §2, `<v>+<commit>`
/// for llama.cpp).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportedVersion {
    pub version: String,
    pub build_fingerprint: String,
}

/// Why a bounded run of an installation ([`run_bounded`]) gave no output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunError {
    Spawn,
    TimedOut,
    /// More than the output limit, or the reader failed.
    Output,
}

/// ADR 0018 §1: run `executable` with `args` only now that the operator
/// named it. Cleared environment, stdin and the unread stream closed, own
/// process group, killed at `timeout`, at most `limit` bytes kept from
/// standard error when `stderr` is set, else from standard output.
pub(crate) fn run_bounded(
    executable: &Path,
    args: &[&str],
    stderr: bool,
    timeout: Duration,
    limit: usize,
) -> Result<(ExitStatus, Vec<u8>), RunError> {
    let mut command = Command::new(executable);
    command.args(args);
    let (out, err) = if stderr {
        (Stdio::null(), Stdio::piped())
    } else {
        (Stdio::piped(), Stdio::null())
    };
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .stdin(Stdio::null())
        .stderr(err)
        .stdout(out)
        .process_group(0);
    if let Some(home) = std::env::var_os("HOME") {
        command.env("HOME", home);
    }
    // SPEC §13.2: its own group, so registered with the role's reaper, which
    // leaves its exit status to the wait below.
    let mut child =
        capyctl_launchers::subreaper::spawn_direct(&mut command).map_err(|_| RunError::Spawn)?;
    let pgid = child.id() as i32;
    let mut pipe: Box<dyn Read + Send> = if stderr {
        Box::new(child.stderr.take().ok_or(RunError::Spawn)?)
    } else {
        Box::new(child.stdout.take().ok_or(RunError::Spawn)?)
    };
    let reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = (&mut pipe).take(limit as u64 + 1).read_to_end(&mut buffer);
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
                return Err(RunError::TimedOut);
            }
        }
    };
    // The group may still hold a writer (a pipeline); end it before reading.
    unsafe { libc::kill(-pgid, libc::SIGKILL) };
    let output = reader.join().map_err(|_| RunError::Output)?;
    // Too much output first: a writer cut off by the closed pipe also exits
    // unsuccessfully, and the cause is the output.
    if output.len() > limit {
        return Err(RunError::Output);
    }
    Ok((status, output))
}

/// ADR 0018 §1: run the installation only now that the operator named it,
/// bounded as [`run_bounded`] with at most [`VERSION_OUTPUT_LIMIT`] bytes.
/// The reported version must equal the dist-info version. ADR 0029 §2:
/// `llama-server --version` writes to standard error, which is read instead,
/// and its version line is parsed; there is no dist-info to compare with.
/// ADR 0039 §2: the native TensorFold binary prints `tensorfold-native
/// <version>` on standard output, which must agree with the release's
/// `VERSION` file when it has one.
pub fn check_version(
    resolved: &Resolved,
    timeout: Duration,
) -> Result<ReportedVersion, VersionCheckError> {
    let args: &[&str] = match resolved.engine {
        Engine::Vllm | Engine::Tensorfold | Engine::Llamacpp => &["--version"],
        Engine::Sglang => &["-I", "-B", "-c", SGLANG_VERSION, "sglang"],
        // ADR 0031 §1: a custom engine is never executed to ask its version;
        // its version is its manifest's.
        Engine::Custom => return Err(VersionCheckError::Spawn),
    };
    let reads_stderr = resolved.engine == Engine::Llamacpp;
    let (status, output) = run_bounded(
        &resolved.executable,
        args,
        reads_stderr,
        timeout,
        VERSION_OUTPUT_LIMIT,
    )
    .map_err(|error| match error {
        RunError::Spawn => VersionCheckError::Spawn,
        RunError::TimedOut => VersionCheckError::TimedOut,
        RunError::Output => VersionCheckError::Output,
    })?;
    if !status.success() {
        return Err(VersionCheckError::Failed);
    }
    if resolved.engine == Engine::Tensorfold && resolved.line.is_native() {
        return native_version(resolved, &output);
    }
    let text = String::from_utf8(output).map_err(|_| VersionCheckError::Output)?;
    if reads_stderr {
        let build =
            LlamacppBuild::parse_version_output(&text).ok_or(VersionCheckError::Unparsable)?;
        return Ok(ReportedVersion {
            build_fingerprint: build.fingerprint(),
            version: build.version,
        });
    }
    let line = text
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .ok_or(VersionCheckError::Output)?;
    let reported = line.rsplit(' ').next().unwrap_or(line).to_owned();
    if reported != resolved.version {
        return Err(VersionCheckError::Mismatch {
            reported,
            installed: resolved.version.clone(),
        });
    }
    Ok(ReportedVersion {
        build_fingerprint: reported.clone(),
        version: reported,
    })
}

/// ADR 0039 §2: the native binary's standard output is exactly one line,
/// `tensorfold-native <MAJOR.MINOR.PATCH>` (`server/main.zig:19-20` at
/// v1.0.0), and the release's `VERSION` file, read again now, must say the
/// same. The checked version is the profile's `build_fingerprint`.
fn native_version(
    resolved: &Resolved,
    output: &[u8],
) -> Result<ReportedVersion, VersionCheckError> {
    let text = std::str::from_utf8(output).map_err(|_| VersionCheckError::NativeUnreadable)?;
    let line = text.strip_suffix('\n').unwrap_or(text);
    let reported = line
        .strip_prefix(tensorfold_native::EXECUTABLE)
        .and_then(|rest| rest.strip_prefix(' '))
        .filter(|_| !line.contains('\n'))
        .and_then(release_version_text)
        .ok_or(VersionCheckError::NativeUnreadable)?;
    match release_version(&resolved.env) {
        ReleaseVersion::Absent => {}
        ReleaseVersion::Stated(stated) if stated == reported => {}
        ReleaseVersion::Stated(stated) => {
            return Err(VersionCheckError::VersionFile {
                reported,
                stated: Some(stated),
            })
        }
        ReleaseVersion::Unreadable => {
            return Err(VersionCheckError::VersionFile {
                reported,
                stated: None,
            })
        }
    }
    Ok(ReportedVersion {
        build_fingerprint: reported.clone(),
        version: reported,
    })
}

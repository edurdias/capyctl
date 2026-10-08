//! ADR 0018 §1: from a path the operator named to the environment and the
//! entry point the engine is launched with, then the bounded version check.
use super::packages;
use capyctl_config::engine_policy::Engine;
use std::io::Read as _;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub engine: Engine,
    pub version: String,
    pub env: PathBuf,
    pub executable: PathBuf,
}

impl Resolved {
    /// ADR 0018 §1: outside the verified set.
    pub fn custom(&self) -> bool {
        !capyctl_config::registration::is_verified(self.engine, &self.version)
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
    }
}

/// ADR 0018 §1: `path` is a venv directory, its `bin/vllm`, or its
/// `bin/python3` (`python`, `python3.N`). Lexical: a venv's interpreter is a
/// symlink to the system Python and is never followed (Review Focus 2).
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
    let meta = std::fs::metadata(&path)
        .map_err(|_| ResolveError::NotFound(format!("{} does not exist", path.display())))?;
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
                "{} is not bin/vllm, bin/tensorfold or bin/python3",
                path.display()
            )));
        };
        (env, Some(wanted))
    };
    if super::site_packages(&env).is_empty() {
        return Err(ResolveError::Unsupported(format!(
            "{} is not a Python environment (no lib/python3.*/site-packages)",
            env.display()
        )));
    }
    let found = packages(&env);
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
    })
}

fn capyctl_agent_engine_name(engine: Engine) -> &'static str {
    crate::installation::package_name(engine)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionCheckError {
    Spawn,
    TimedOut,
    Failed,
    Output,
    Mismatch { reported: String, installed: String },
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
        }
    }
}

/// ADR 0018 §1: run the installation only now that the operator named it.
/// Cleared environment, stdin and stderr closed, own process group, killed
/// at `timeout`, at most [`VERSION_OUTPUT_LIMIT`] bytes kept. The reported
/// version must equal the dist-info version.
pub fn check_version(resolved: &Resolved, timeout: Duration) -> Result<String, VersionCheckError> {
    let mut command = Command::new(&resolved.executable);
    match resolved.engine {
        Engine::Vllm | Engine::Tensorfold => command.arg("--version"),
        Engine::Sglang => command.args(["-I", "-B", "-c", SGLANG_VERSION, "sglang"]),
    };
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .process_group(0);
    if let Some(home) = std::env::var_os("HOME") {
        command.env("HOME", home);
    }
    // SPEC §13.2: its own group, so registered with the role's reaper, which
    // leaves its exit status to the wait below.
    let mut child = capyctl_launchers::subreaper::spawn_direct(&mut command)
        .map_err(|_| VersionCheckError::Spawn)?;
    let pgid = child.id() as i32;
    let mut stdout = child.stdout.take().ok_or(VersionCheckError::Spawn)?;
    let reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = (&mut stdout)
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
    Ok(reported)
}

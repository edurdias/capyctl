//! SPEC §13.3 / T21: the redacting writer between an engine and its log.
//!
//! An engine's standard output and error are a pipe into a small writer process,
//! never the log file itself. The writer redacts every line before it reaches
//! the private log ([`capyctl_adapters::engine_log::LogRedactor`]: the launch's
//! own credentials by value, then the shape rules), so native writes are
//! covered as well as the engine's Python logging, and it bounds the log by
//! rotation: at most [`MAX_FILE_BYTES`] per file and [`KEEP_ROTATED`] older
//! files (`<log>.1`, `<log>.2`).
//!
//! The writer is the `capyctl` binary itself, re-executed ([`LogRelay::this_binary`]),
//! in its own process group: it is no member of the engine's group, so group
//! observation and cleanup see the engine alone (ADR 0027), and it outlives an
//! agent restart with the engine it serves. It ends when the last writer of
//! the pipe (the engine and every child it started) has exited.
//!
//! `--debug-engine-logs` keeps its meaning: full native output, written to the
//! log directly, with a `<log>.raw` marker so the management surface never
//! serves it (SPEC §13.3).

use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;

use capyctl_adapters::engine_log::{raw_marker, rotated, LogRedactor};
use capyctl_adapters::protected::ProtectedLaunchDescriptors;
use capyctl_domain::redact::REDACTED;

/// The argument that makes `capyctl` run as the writer.
pub const RELAY_ARG: &str = "__engine-log-relay";

/// The variable naming the log, for the engine and for the writer.
pub const LOG_VARIABLE: &str = "CAPYCTL_ENGINE_LOG";

/// The largest a log file grows before it is rotated.
pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// Rotated files kept beside the current one: a launch's log is at most
/// `MAX_FILE_BYTES * (KEEP_ROTATED + 1)` bytes.
pub const KEEP_ROTATED: u32 = 2;

/// The longest run without a line break redacted as one line. Longer output is
/// broken at its last space; a run with no space at all is not text an operator
/// can read and is replaced whole.
const MAX_LINE_BYTES: usize = 64 * 1024;

/// The descriptor the writer reads the launch's credentials from.
const SECRETS_FD: i32 = 3;

/// How the writer process is started.
#[derive(Clone, Debug)]
pub struct LogRelay {
    program: OsString,
    args: Vec<OsString>,
}

impl LogRelay {
    pub fn new(
        program: impl Into<OsString>,
        args: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }

    /// The running `capyctl` binary, re-executed as the writer. `/proc/self/exe`
    /// names the image this process runs even after an upgrade replaced the file.
    pub fn this_binary() -> Self {
        Self::new("/proc/self/exe", [RELAY_ARG])
    }
}

static INSTALLED: OnceLock<LogRelay> = OnceLock::new();

/// Make `relay` the writer every launcher in this process uses. `capyctl`'s
/// `main` installs [`LogRelay::this_binary`] before anything else; a process
/// that installs none (a test harness) writes engine output to the log
/// directly, as a launch under `--debug-engine-logs` does.
pub fn install(relay: LogRelay) {
    let _ = INSTALLED.set(relay);
}

pub(crate) fn installed() -> Option<LogRelay> {
    INSTALLED.get().cloned()
}

/// Whether this process was started with `--debug-engine-logs` (the flag sets
/// the variable for the role; an inherited value is cleared at startup).
fn debug_engine_logs() -> bool {
    std::env::var("CAPYCTL_DEBUG_ENGINE_LOGS").as_deref() == Ok("1")
}

/// The engine's standard output and error for a launch whose log is `log`:
/// a pipe into a started writer, or, under `--debug-engine-logs` or with no
/// writer installed, the file opened by `open`.
pub(crate) fn attach(
    log: &Path,
    relay: Option<&LogRelay>,
    env: &std::collections::BTreeMap<String, String>,
    descriptors: Option<&ProtectedLaunchDescriptors>,
    open: impl Fn(&Path) -> std::io::Result<std::fs::File>,
) -> std::io::Result<(Stdio, Stdio)> {
    let relay = match relay {
        Some(relay) if !debug_engine_logs() => relay,
        _ => {
            if debug_engine_logs() {
                // SPEC §13.3: raw development output is marked so it is never served.
                open(&raw_marker(log))?;
            }
            let file = open(log)?;
            return Ok((Stdio::from(file.try_clone()?), Stdio::from(file)));
        }
    };
    // The log is created (owner-only, never through a symlink) before the
    // engine starts, so a refused path refuses the launch.
    drop(open(log)?);
    let mut redactor = LogRedactor::new();
    redactor.own_environment(env);
    if let Some(descriptors) = descriptors {
        descriptors.own_credentials(&mut redactor)?;
    }
    let (output_read, output_write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)?;
    start(relay, log, output_read, &redactor.encode())?;
    let second = output_write.try_clone()?;
    Ok((Stdio::from(output_write), Stdio::from(second)))
}

/// Start the writer reading `output` and hand it the launch's credentials on
/// its descriptor 3. It is reaped by a detached thread, like the engine.
fn start(relay: &LogRelay, log: &Path, output: OwnedFd, secrets: &[u8]) -> std::io::Result<()> {
    let (secrets_read, secrets_write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)?;
    let secrets_fd = secrets_read.as_raw_fd();
    let mut command = std::process::Command::new(&relay.program);
    command
        .args(&relay.args)
        // SPEC §13.3 / T21: the writer sees only the log's name.
        .env_clear()
        .env(LOG_VARIABLE, log)
        .stdin(Stdio::from(output))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(move || {
            // Its own group: never signalled with, or observed as part of, the engine's.
            nix::unistd::setpgid(nix::unistd::Pid::from_raw(0), nix::unistd::Pid::from_raw(0))
                .map_err(std::io::Error::other)?;
            if secrets_fd == SECRETS_FD {
                nix::fcntl::fcntl(
                    SECRETS_FD,
                    nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::empty()),
                )
                .map_err(std::io::Error::other)?;
            } else {
                // dup2 leaves the copy inheritable.
                nix::unistd::dup2(secrets_fd, SECRETS_FD).map_err(std::io::Error::other)?;
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    drop(secrets_read);
    // A pipe holds far more than a launch's few credentials, so this never blocks.
    let written = std::fs::File::from(secrets_write).write_all(secrets);
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    written
}

/// The writer's `main`: read the credentials on descriptor 3, then relay
/// standard input into the log named by [`LOG_VARIABLE`] until end of input.
pub fn relay_main() -> i32 {
    let Some(log) = std::env::var_os(LOG_VARIABLE) else {
        return 2;
    };
    let mut secrets = Vec::new();
    if nix::fcntl::fcntl(SECRETS_FD, nix::fcntl::FcntlArg::F_GETFD).is_ok() {
        let mut file = unsafe { std::fs::File::from_raw_fd(SECRETS_FD) };
        if file.read_to_end(&mut secrets).is_err() {
            return 2;
        }
    }
    let redactor = LogRedactor::decode(&secrets);
    let Ok(mut log) = RotatingLog::open(PathBuf::from(log), MAX_FILE_BYTES, KEEP_ROTATED) else {
        // Still drain the pipe: an engine must never block on its own output.
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
        return 1;
    };
    relay(std::io::stdin().lock(), &redactor, &mut log);
    0
}

/// Copy `input` into `log` line by line, redacted, until end of input. A write
/// that fails is dropped and reading continues, so the engine never blocks on
/// a full pipe because its log could not be written.
pub fn relay(mut input: impl Read, redactor: &LogRedactor, log: &mut RotatingLog) {
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let read = match input.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        pending.extend_from_slice(&chunk[..read]);
        let mut start = 0;
        while let Some(offset) = pending[start..]
            .iter()
            .position(|b| matches!(b, b'\n' | b'\r'))
        {
            let end = start + offset;
            emit(redactor, log, &pending[start..end], pending[end]);
            start = end + 1;
        }
        pending.drain(..start);
        if pending.len() > MAX_LINE_BYTES {
            match pending.iter().rposition(|b| *b == b' ') {
                Some(space) => {
                    emit(redactor, log, &pending[..space], b'\n');
                    pending.drain(..=space);
                }
                None => {
                    emit(redactor, log, REDACTED.as_bytes(), b'\n');
                    pending.clear();
                }
            }
        }
    }
    if !pending.is_empty() {
        emit(redactor, log, &pending, b'\n');
    }
}

fn emit(redactor: &LogRedactor, log: &mut RotatingLog, line: &[u8], end: u8) {
    let mut text = redactor.redact(&String::from_utf8_lossy(line)).into_bytes();
    text.push(end);
    let _ = log.write(&text);
}

/// An owner-only log that rotates to `<log>.1` … `<log>.<keep>` when it would
/// grow past `max_bytes`.
pub struct RotatingLog {
    path: PathBuf,
    file: std::fs::File,
    written: u64,
    max_bytes: u64,
    keep: u32,
}

impl RotatingLog {
    pub fn open(path: PathBuf, max_bytes: u64, keep: u32) -> std::io::Result<Self> {
        let file = crate::durable::open_private_log(&path)?;
        let written = file.metadata()?.len();
        Ok(Self {
            path,
            file,
            written,
            max_bytes,
            keep,
        })
    }

    pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if self.written > 0 && self.written + bytes.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        self.file.write_all(bytes)?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        if self.keep == 0 {
            self.file.set_len(0)?;
            self.written = 0;
            return Ok(());
        }
        for generation in (1..self.keep).rev() {
            match std::fs::rename(
                rotated(&self.path, generation),
                rotated(&self.path, generation + 1),
            ) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
        }
        std::fs::rename(&self.path, rotated(&self.path, 1))?;
        self.file = crate::durable::open_private_log(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests;

//! Real exec launcher: process-group ownership, signal escalation, and
//! start-identity verification against `/proc` (F1 design §4, T12
//! mechanics). Engine-agnostic — no engine knowledge lives here.

use std::os::unix::process::CommandExt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use mllm_adapters::traits::{
    ExitReport, HandleStatus, Launcher, LauncherError, OwnedHandle, RenderedCommand,
};

/// Spawns real OS processes in their own process group. Termination targets
/// the whole owned group (SIGTERM → grace → SIGKILL). Handle verification
/// compares the spawn-time `/proc/<pid>/stat` starttime (boot-unique) with
/// the current process — PID-reuse detection, SPEC §13.2: never kill by
/// name, never adopt whatever occupies a port.
pub struct ExecLauncher {
    /// pid → /proc starttime captured at spawn time.
    spawned: Mutex<std::collections::HashMap<u32, String>>,
    identity_counter: std::sync::atomic::AtomicU64,
}

impl Default for ExecLauncher {
    fn default() -> Self {
        Self::new()
    }
}

/// The `/proc/<pid>/stat` starttime field: boot-unique per process — the
/// kernel's own PID-reuse oracle. Field 22 counting the parenthesized comm
/// as field 2.
fn proc_starttime(pid: u32) -> Option<String> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = raw.rfind(')')?;
    let after_comm = &raw[close + 2..];
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    fields.get(19).map(|s| s.to_string())
}

fn pid_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

impl ExecLauncher {
    pub fn new() -> Self {
        Self {
            spawned: Mutex::new(Default::default()),
            identity_counter: std::sync::atomic::AtomicU64::new(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0) as u64,
            ),
        }
    }

    fn next_identity(&self) -> u64 {
        self.identity_counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }

    fn recorded_starttime(&self, pid: u32) -> Option<String> {
        self.spawned.lock().unwrap().get(&pid).cloned()
    }
}

impl Launcher for ExecLauncher {
    fn spawn(&self, cmd: &RenderedCommand) -> Result<OwnedHandle, LauncherError> {
        use std::process::{Command, Stdio};
        let (program, args) = cmd
            .argv
            .split_first()
            .ok_or_else(|| LauncherError::SpawnFailed("empty argv".into()))?;
        let mut command = Command::new(program);
        command.args(args).stdout(Stdio::null()).stderr(Stdio::null());
        for (k, v) in &cmd.env {
            command.env(k, v);
        }
        // Own process group: termination targets the whole owned tree, and
        // the engine's own children belong to us (T12).
        unsafe {
            command.pre_exec(|| {
                nix::unistd::setpgid(nix::unistd::Pid::from_raw(0), nix::unistd::Pid::from_raw(0))
                    .map_err(std::io::Error::other)
            });
        }
        let mut child = command
            .spawn()
            .map_err(|e| LauncherError::SpawnFailed(format!("spawn {program}: {e}")))?;
        let pid = child.id();
        // Reap on exit in a detached thread so no zombie accumulates while
        // the handle stays owned; termination still targets the group.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        // Capture the boot-unique starttime immediately; a process that
        // exited before we read it shows up Gone at verify (correct).
        let starttime = proc_starttime(pid);
        if let Some(st) = &starttime {
            self.spawned.lock().unwrap().insert(pid, st.clone());
        }
        let identity = self.next_identity();
        Ok(OwnedHandle { pid, start_identity: identity.into() })
    }

    fn terminate(&self, h: &OwnedHandle, grace: Duration) -> Result<ExitReport, LauncherError> {
        // Never signal a process we do not own (SPEC §13.2).
        match self.verify_handle(h) {
            HandleStatus::Valid => {}
            HandleStatus::Gone => {
                self.spawned.lock().unwrap().remove(&h.pid);
                return Ok(ExitReport {
                    pid: h.pid,
                    exit_code: None,
                    signal: None,
                    killed: false,
                });
            }
            HandleStatus::StaleReused => {
                return Err(LauncherError::TerminateFailed(format!(
                    "handle pid {} no longer owned (PID reuse detected); refusing to signal",
                    h.pid
                )));
            }
        }
        let pgid = nix::unistd::Pid::from_raw(h.pid as i32);
        let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGTERM);
        let deadline = Instant::now() + grace;
        loop {
            if !pid_alive(h.pid) {
                self.spawned.lock().unwrap().remove(&h.pid);
                return Ok(ExitReport {
                    pid: h.pid,
                    exit_code: None,
                    signal: Some(nix::sys::signal::Signal::SIGTERM as i32),
                    killed: false,
                });
            }
            if Instant::now() >= deadline {
                let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
                // Wait briefly for the kernel to reclaim the process.
                for _ in 0..50 {
                    if !pid_alive(h.pid) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                self.spawned.lock().unwrap().remove(&h.pid);
                return Ok(ExitReport {
                    pid: h.pid,
                    exit_code: None,
                    signal: Some(nix::sys::signal::Signal::SIGKILL as i32),
                    killed: true,
                });
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn verify_handle(&self, h: &OwnedHandle) -> HandleStatus {
        let Some(current) = proc_starttime(h.pid) else {
            return HandleStatus::Gone;
        };
        match self.recorded_starttime(h.pid) {
            // We spawned it and the starttime matches: still ours.
            Some(recorded) if recorded == current => HandleStatus::Valid,
            // PID exists but we never spawned it, or it was replaced: reuse.
            _ => HandleStatus::StaleReused,
        }
    }
}

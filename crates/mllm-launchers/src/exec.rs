//! Real exec launcher: process-group ownership, signal escalation, and
//! start-identity verification against `/proc` (F1 design §4, T12
//! mechanics). Engine-agnostic — no engine knowledge lives here.

use std::os::unix::process::CommandExt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use mllm_adapters::traits::{
    ExitReport, HandleStatus, Launcher, LauncherError, OwnedHandle, RenderedCommand,
};
use mllm_domain::completion::ProcessIdentity;

/// Spawns real OS processes in their own process group. Termination targets
/// the whole owned group (SIGTERM → grace → SIGKILL). Handle verification
/// compares the spawn-time `/proc/<pid>/stat` starttime (boot-unique) with
/// the current process — PID-reuse detection, SPEC §13.2: never kill by
/// name, never adopt whatever occupies a port.
pub struct ExecLauncher {
    /// pid → /proc starttime captured at spawn time.
    spawned: Mutex<std::collections::HashMap<u32, ProcessIdentity>>,
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
        }
    }

    fn recorded_identity(&self, pid: u32) -> Option<ProcessIdentity> {
        self.spawned.lock().unwrap().get(&pid).cloned()
    }
}

pub(crate) fn process_identity(pid: u32, role: &str) -> Option<ProcessIdentity> {
    let start_ticks = proc_starttime(pid)?.parse().ok()?;
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let boot_id = boot_id.trim().to_owned();
    Some(ProcessIdentity { role: role.into(), pid, boot_id, start_ticks })
}

pub(crate) fn legacy_identity(identity: &ProcessIdentity) -> u128 {
    let boot = identity.boot_id.replace('-', "");
    u128::from_str_radix(&boot, 16).unwrap_or(0)
        ^ (u128::from(identity.start_ticks) << 32)
        ^ u128::from(identity.pid)
}

impl Launcher for ExecLauncher {
    fn spawn(&self, cmd: &RenderedCommand) -> Result<OwnedHandle, LauncherError> {
        use std::process::{Command, Stdio};
        let (program, args) = cmd
            .argv
            .split_first()
            .ok_or_else(|| LauncherError::SpawnFailed("empty argv".into()))?;
        let mut command = Command::new(program);
        command.args(args);
        for (k, v) in &cmd.env {
            command.env(k, v);
        }
        // Engine output lands in the deployment's engine log when one is
        // requested (the runbook's evidence); otherwise discarded.
        if let Some(log) = cmd.env.get("MLLM_ENGINE_LOG") {
            let f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log)
                .map_err(|e| LauncherError::SpawnFailed(format!("engine log {log}: {e}")))?;
            let log_clone = f
                .try_clone()
                .map_err(|e| LauncherError::SpawnFailed(format!("engine log clone: {e}")))?;
            command.stdout(Stdio::from(log_clone)).stderr(Stdio::from(f));
        } else {
            command.stdout(Stdio::null()).stderr(Stdio::null());
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
        let process_identity = process_identity(pid, "api");
        if let Some(identity) = &process_identity {
            self.spawned.lock().unwrap().insert(pid, identity.clone());
        }
        let start_identity = process_identity.as_ref().map(legacy_identity).unwrap_or(0);
        Ok(OwnedHandle { pid, start_identity })
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
            HandleStatus::StaleReused => return Err(LauncherError::TerminateFailed(format!(
                "handle pid {} changed immediately before SIGTERM", h.pid
            ))),
        }
        nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGTERM)
            .map_err(|error| LauncherError::TerminateFailed(error.to_string()))?;
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
                match self.verify_handle(h) {
                    HandleStatus::Valid => {}
                    HandleStatus::Gone => {
                        self.spawned.lock().unwrap().remove(&h.pid);
                        return Ok(ExitReport {
                            pid: h.pid,
                            exit_code: None,
                            signal: Some(nix::sys::signal::Signal::SIGTERM as i32),
                            killed: false,
                        });
                    }
                    HandleStatus::StaleReused => {
                        return Err(LauncherError::TerminateFailed(format!(
                            "handle pid {} changed immediately before SIGKILL", h.pid
                        )));
                    }
                }
                nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL)
                    .map_err(|error| LauncherError::TerminateFailed(error.to_string()))?;
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
        match self.recorded_identity(h.pid) {
            // We spawned it and the starttime matches: still ours.
            Some(recorded) if recorded.start_ticks.to_string() == current
                && legacy_identity(&recorded) == h.start_identity => HandleStatus::Valid,
            // PID exists but we never spawned it, or it was replaced: reuse.
            _ => HandleStatus::StaleReused,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escalation_rechecks_identity_immediately_before_sigkill() {
        let launcher = ExecLauncher::new();
        let command = RenderedCommand {
            argv: vec![
                "sh".into(), "-c".into(),
                "trap \"\" TERM; while :; do sleep 1; done".into(),
            ],
            env: Default::default(),
        };
        let handle = launcher.spawn(&command).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                launcher.spawned.lock().unwrap().get_mut(&handle.pid).unwrap().start_ticks += 1;
            });
            let result = launcher.terminate(&handle, Duration::from_millis(300));
            assert!(matches!(result, Err(LauncherError::TerminateFailed(_))));
        });
        assert!(pid_alive(handle.pid));
        nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(handle.pid as i32), nix::sys::signal::Signal::SIGKILL,
        ).unwrap();
    }
}

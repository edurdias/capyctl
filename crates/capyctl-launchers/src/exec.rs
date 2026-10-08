//! Real exec launcher: process-group ownership, signal escalation, and
//! start-identity verification against `/proc` (F1 design §4, T12
//! mechanics). Engine-agnostic — no engine knowledge lives here.

use std::os::unix::process::CommandExt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use capyctl_adapters::traits::{
    ExitReport, HandleStatus, Launcher, LauncherError, OwnedHandle, RenderedCommand,
};
use capyctl_domain::completion::ProcessIdentity;

/// Spawns real OS processes in their own process group. Termination targets
/// the whole owned group (SIGTERM → grace → SIGKILL). Handle verification
/// compares the spawn-time `/proc/<pid>/stat` starttime (boot-unique) with
/// the current process — PID-reuse detection, SPEC §13.2: never kill by
/// name, never adopt whatever occupies a port.
pub struct ExecLauncher {
    /// pid → /proc starttime captured at spawn time.
    spawned: Mutex<std::collections::HashMap<u32, ProcessIdentity>>,
    /// SPEC §13.3 / T21: the redacting writer engine output passes through.
    log_relay: Option<crate::engine_log_relay::LogRelay>,
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
            log_relay: crate::engine_log_relay::installed(),
        }
    }

    /// Use `relay` as the engine log's writer instead of the process's
    /// installed one.
    pub fn with_log_relay(mut self, relay: crate::engine_log_relay::LogRelay) -> Self {
        self.log_relay = Some(relay);
        self
    }

    fn recorded_identity(&self, pid: u32) -> Option<ProcessIdentity> {
        self.spawned.lock().unwrap().get(&pid).cloned()
    }

    fn terminate_with_signal<F>(
        &self,
        h: &OwnedHandle,
        grace: Duration,
        signal_group: &F,
    ) -> Result<ExitReport, LauncherError>
    where
        F: Fn(nix::unistd::Pid, nix::sys::signal::Signal) -> Result<(), nix::errno::Errno>,
    {
        // Never signal a process we do not own (SPEC §13.2).
        match self.verify_handle(h) {
            HandleStatus::Valid => {}
            HandleStatus::Gone => return Ok(self.gone_report(h)),
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
            HandleStatus::Gone => return Ok(self.gone_report(h)),
            HandleStatus::StaleReused => {
                return Err(LauncherError::TerminateFailed(format!(
                    "handle pid {} changed immediately before SIGTERM",
                    h.pid
                )));
            }
        }
        if let Err(error) = signal_group(pgid, nix::sys::signal::Signal::SIGTERM) {
            if error == nix::errno::Errno::ESRCH {
                return Ok(self.gone_report(h));
            }
            return Err(LauncherError::TerminateFailed(error.to_string()));
        }
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
                            "handle pid {} changed immediately before SIGKILL",
                            h.pid
                        )));
                    }
                }
                if let Err(error) = signal_group(pgid, nix::sys::signal::Signal::SIGKILL) {
                    if error == nix::errno::Errno::ESRCH {
                        return Ok(self.gone_report(h));
                    }
                    return Err(LauncherError::TerminateFailed(error.to_string()));
                }
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

    fn gone_report(&self, h: &OwnedHandle) -> ExitReport {
        self.spawned.lock().unwrap().remove(&h.pid);
        ExitReport {
            pid: h.pid,
            exit_code: None,
            signal: None,
            killed: false,
        }
    }
}

pub(crate) fn process_identity(pid: u32, role: &str) -> Option<ProcessIdentity> {
    let start_ticks = proc_starttime(pid)?.parse().ok()?;
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let boot_id = boot_id.trim().to_owned();
    Some(ProcessIdentity {
        role: role.into(),
        pid,
        boot_id,
        start_ticks,
    })
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
        // SPEC §13.3 / T21: the engine sees only its rendered environment.
        command.args(args).env_clear();
        for (k, v) in &cmd.env {
            command.env(k, v);
        }
        // Engine output lands in the deployment's engine log when one is
        // requested (the runbook's evidence); otherwise discarded.
        if let Some(log) = cmd.env.get("CAPYCTL_ENGINE_LOG") {
            // SPEC §13.3 / T21: never through a symlink, always owner-only, and
            // redacted on its way in.
            let (stdout, stderr) = crate::engine_log_relay::attach(
                std::path::Path::new(log),
                self.log_relay.as_ref(),
                &cmd.env,
                None,
                crate::durable::open_private_log,
            )
            .map_err(|e| LauncherError::SpawnFailed(format!("engine log {log}: {e}")))?;
            command.stdout(stdout).stderr(stderr);
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
        Ok(OwnedHandle {
            pid,
            start_identity,
        })
    }

    fn terminate(&self, h: &OwnedHandle, grace: Duration) -> Result<ExitReport, LauncherError> {
        self.terminate_with_signal(h, grace, &nix::sys::signal::killpg)
    }

    fn verify_handle(&self, h: &OwnedHandle) -> HandleStatus {
        let Some(current) = proc_starttime(h.pid) else {
            return HandleStatus::Gone;
        };
        match self.recorded_identity(h.pid) {
            // We spawned it and the starttime matches: still ours.
            Some(recorded)
                if recorded.start_ticks.to_string() == current
                    && legacy_identity(&recorded) == h.start_identity =>
            {
                HandleStatus::Valid
            }
            // PID exists but we never spawned it, or it was replaced: reuse.
            _ => HandleStatus::StaleReused,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sleep_command() -> RenderedCommand {
        RenderedCommand {
            argv: vec!["sleep".into(), "30".into()],
            env: Default::default(),
        }
    }

    fn kill_test_group(handle: &OwnedHandle) {
        nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(handle.pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        )
        .unwrap();
    }

    #[test]
    fn sigterm_esrch_after_valid_verification_reports_gone() {
        let launcher = ExecLauncher::new();
        let handle = launcher.spawn(&sleep_command()).unwrap();
        let signals = Mutex::new(Vec::new());

        let report = launcher
            .terminate_with_signal(&handle, Duration::from_secs(1), &|_, signal| {
                signals.lock().unwrap().push(signal);
                Err(nix::errno::Errno::ESRCH)
            })
            .unwrap();

        assert_eq!(
            signals.into_inner().unwrap(),
            vec![nix::sys::signal::Signal::SIGTERM]
        );
        assert_eq!(
            report,
            ExitReport {
                pid: handle.pid,
                exit_code: None,
                signal: None,
                killed: false
            }
        );
        assert!(launcher.recorded_identity(handle.pid).is_none());
        kill_test_group(&handle);
    }

    #[test]
    fn sigkill_esrch_after_valid_verification_reports_gone() {
        let launcher = ExecLauncher::new();
        let handle = launcher.spawn(&sleep_command()).unwrap();
        let signals = Mutex::new(Vec::new());

        let report = launcher
            .terminate_with_signal(&handle, Duration::ZERO, &|_, signal| {
                signals.lock().unwrap().push(signal);
                if signal == nix::sys::signal::Signal::SIGKILL {
                    Err(nix::errno::Errno::ESRCH)
                } else {
                    Ok(())
                }
            })
            .unwrap();

        assert_eq!(
            signals.into_inner().unwrap(),
            vec![
                nix::sys::signal::Signal::SIGTERM,
                nix::sys::signal::Signal::SIGKILL
            ]
        );
        assert_eq!(
            report,
            ExitReport {
                pid: handle.pid,
                exit_code: None,
                signal: None,
                killed: false
            }
        );
        assert!(launcher.recorded_identity(handle.pid).is_none());
        kill_test_group(&handle);
    }

    #[test]
    fn non_esrch_signal_failure_remains_an_error() {
        let launcher = ExecLauncher::new();
        let handle = launcher.spawn(&sleep_command()).unwrap();

        let result = launcher.terminate_with_signal(&handle, Duration::from_secs(1), &|_, _| {
            Err(nix::errno::Errno::EPERM)
        });

        assert_eq!(
            result,
            Err(LauncherError::TerminateFailed(
                "EPERM: Operation not permitted".into()
            ))
        );
        kill_test_group(&handle);
    }

    #[test]
    fn stale_identity_never_reaches_signal_boundary() {
        let launcher = ExecLauncher::new();
        let handle = launcher.spawn(&sleep_command()).unwrap();
        launcher
            .spawned
            .lock()
            .unwrap()
            .get_mut(&handle.pid)
            .unwrap()
            .start_ticks += 1;
        let signal_calls = std::sync::atomic::AtomicUsize::new(0);

        let result = launcher.terminate_with_signal(&handle, Duration::from_secs(1), &|_, _| {
            signal_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });

        assert!(matches!(result, Err(LauncherError::TerminateFailed(_))));
        assert_eq!(signal_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        kill_test_group(&handle);
    }

    #[test]
    fn escalation_rechecks_identity_immediately_before_sigkill() {
        let launcher = ExecLauncher::new();
        let handle = launcher.spawn(&sleep_command()).unwrap();
        let signals = Mutex::new(Vec::new());

        let result = launcher.terminate_with_signal(&handle, Duration::ZERO, &|_, signal| {
            signals.lock().unwrap().push(signal);
            if signal == nix::sys::signal::Signal::SIGTERM {
                launcher
                    .spawned
                    .lock()
                    .unwrap()
                    .get_mut(&handle.pid)
                    .unwrap()
                    .start_ticks += 1;
            }
            Ok(())
        });

        assert!(matches!(result, Err(LauncherError::TerminateFailed(_))));
        assert_eq!(
            signals.into_inner().unwrap(),
            vec![nix::sys::signal::Signal::SIGTERM]
        );
        assert!(pid_alive(handle.pid));
        kill_test_group(&handle);
    }
}

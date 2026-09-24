use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::sync::Mutex;

use mllm_adapters::protected::ProtectedLaunchDescriptors;
use mllm_adapters::traits::{OwnedHandle, RenderedCommand};
use mllm_domain::completion::ProcessIdentity;

#[derive(Debug, thiserror::Error)]
pub enum AssociationError {
    #[error("launch association uncertain: {0}")]
    Uncertain(String),
}

/// Persists the observed API identity under the caller's already-retained binding fences.
/// It does not assert or manufacture complete worker ownership.
pub trait LaunchAssociation {
    fn persist_api_identity(&self, identity: &ProcessIdentity) -> Result<(), AssociationError>;
}

#[derive(Debug)]
pub enum DurableSpawnOutcome {
    Uncertain {
        handle: OwnedHandle,
        api_identity: Option<ProcessIdentity>,
        initialization_acknowledged: bool,
        reason: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum DurableSpawnError {
    #[error("incarnation was already spawned and must be reconciled")]
    AlreadyAttempted,
    #[error("spawn failed: {0}")]
    Spawn(String),
}

pub struct DurableSpawn {
    attempted: Mutex<HashSet<String>>,
    retained: Mutex<HashMap<String, RetainedChild>>,
    identity_collector: fn(u32, &str) -> Option<ProcessIdentity>,
}

struct RetainedChild {
    child: std::process::Child,
    write_gate: OwnedFd,
    /// SPEC §13.2 (W13): the identity the reaper records the exit status under.
    identity: Option<ProcessIdentity>,
}

impl Default for DurableSpawn {
    fn default() -> Self {
        Self::new()
    }
}

impl DurableSpawn {
    pub fn new() -> Self {
        Self {
            attempted: Mutex::new(HashSet::new()),
            retained: Mutex::new(HashMap::new()),
            identity_collector: super::exec::process_identity,
        }
    }

    #[cfg(test)]
    fn with_identity_collector(collector: fn(u32, &str) -> Option<ProcessIdentity>) -> Self {
        Self {
            attempted: Mutex::new(HashSet::new()),
            retained: Mutex::new(HashMap::new()),
            identity_collector: collector,
        }
    }

    pub fn spawn_persisted(
        &self,
        incarnation: &str,
        cmd: &RenderedCommand,
        association: &dyn LaunchAssociation,
    ) -> Result<DurableSpawnOutcome, DurableSpawnError> {
        self.spawn_inner(incarnation, cmd, association, None)
    }

    /// The single entry point with optional descriptors, so the plain and the
    /// protected paths cannot drift apart: both are the same gated spawn.
    pub fn spawn_persisted_with_descriptors(
        &self,
        incarnation: &str,
        cmd: &RenderedCommand,
        descriptors: Option<&ProtectedLaunchDescriptors>,
        association: &dyn LaunchAssociation,
    ) -> Result<DurableSpawnOutcome, DurableSpawnError> {
        self.spawn_inner(incarnation, cmd, association, descriptors)
    }

    /// Inherit private descriptors only into this supervised child. The parent's
    /// descriptors retain CLOEXEC, and the caller keeps them alive through spawn.
    pub fn spawn_protected(
        &self,
        incarnation: &str,
        cmd: &RenderedCommand,
        descriptors: &ProtectedLaunchDescriptors,
        association: &dyn LaunchAssociation,
    ) -> Result<DurableSpawnOutcome, DurableSpawnError> {
        self.spawn_inner(incarnation, cmd, association, Some(descriptors))
    }

    /// A child whose identity was never recorded must not be left blocked on its
    /// gate. Dropping the write gate makes the child's `dd` read EOF, so the shell
    /// exits 125 without ever reaching `exec`; the group signal is the backstop for
    /// a child that somehow got past the gate, and the wait leaves no zombie.
    /// SPEC §13.2: only the group this launcher just created is signalled.
    fn dispose(&self, incarnation: &str, pid: u32) {
        let Some(retained) = self.retained.lock().unwrap().remove(incarnation) else {
            return;
        };
        let RetainedChild {
            mut child,
            write_gate,
            ..
        } = retained;
        drop(write_gate);
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = child.wait();
    }

    fn spawn_inner(
        &self,
        incarnation: &str,
        cmd: &RenderedCommand,
        association: &dyn LaunchAssociation,
        descriptors: Option<&ProtectedLaunchDescriptors>,
    ) -> Result<DurableSpawnOutcome, DurableSpawnError> {
        if incarnation.is_empty() || !self.attempted.lock().unwrap().insert(incarnation.into()) {
            return Err(DurableSpawnError::AlreadyAttempted);
        }
        let (read_gate, write_gate) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)
            .map_err(|error| DurableSpawnError::Spawn(error.to_string()))?;
        let (program, args) = cmd
            .argv
            .split_first()
            .ok_or_else(|| DurableSpawnError::Spawn("empty argv".into()))?;
        let log = engine_log(cmd)?;
        let read_fd = read_gate.as_raw_fd();
        let write_fd = write_gate.as_raw_fd();
        const CHILD_GATE_FD: i32 = 9;
        let mut command = std::process::Command::new("/bin/sh");
        // SPEC §13.3 / T21: every engine starts from an empty environment and
        // sees only what its rendered command names (the adapter's closed
        // allowlist); nothing of the agent's own environment is inherited.
        command.env_clear();
        if descriptors.is_some() {
            command.stdin(std::process::Stdio::null());
        }
        command
            .arg("-c")
            .arg(format!(
                "ack=$(dd bs=1 count=1 <&{CHILD_GATE_FD} 2>/dev/null); [ \"$ack\" = x ] || exit 125; exec \"$@\""
            ))
            .arg("mllm-init-gate")
            .arg(program)
            .args(args)
            .envs(&cmd.env);
        match log {
            Some(file) => {
                let second = file
                    .try_clone()
                    .map_err(|error| DurableSpawnError::Spawn(error.to_string()))?;
                command
                    .stdout(std::process::Stdio::from(file))
                    .stderr(std::process::Stdio::from(second));
            }
            None => {
                command
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
            }
        }
        let inherited = descriptors.map(ProtectedLaunchDescriptors::numbers);
        unsafe {
            command.pre_exec(move || {
                nix::unistd::setpgid(nix::unistd::Pid::from_raw(0), nix::unistd::Pid::from_raw(0))
                    .map_err(std::io::Error::other)?;
                nix::unistd::close(write_fd).map_err(std::io::Error::other)?;
                if read_fd != CHILD_GATE_FD {
                    nix::unistd::dup2(read_fd, CHILD_GATE_FD).map_err(std::io::Error::other)?;
                    nix::unistd::close(read_fd).map_err(std::io::Error::other)?;
                }
                nix::fcntl::fcntl(
                    CHILD_GATE_FD,
                    nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::empty()),
                )
                .map_err(std::io::Error::other)?;
                if let Some(numbers) = inherited {
                    for fd in numbers {
                        nix::fcntl::fcntl(
                            fd,
                            nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::empty()),
                        )
                        .map_err(std::io::Error::other)?;
                    }
                }
                Ok(())
            });
        }
        let child = command
            .spawn()
            .map_err(|error| DurableSpawnError::Spawn(error.to_string()))?;
        let pid = child.id();
        drop(read_gate);
        let identity = (self.identity_collector)(pid, "api");
        let handle = OwnedHandle {
            pid,
            start_identity: identity
                .as_ref()
                .map(super::exec::legacy_identity)
                .unwrap_or(0),
        };
        self.retained
            .lock()
            .unwrap()
            .insert(
                incarnation.into(),
                RetainedChild {
                    child,
                    write_gate,
                    identity: identity.clone(),
                },
            );
        let Some(identity) = identity else {
            self.dispose(incarnation, pid);
            return Ok(DurableSpawnOutcome::Uncertain {
                handle,
                api_identity: None,
                initialization_acknowledged: false,
                reason: "API identity unavailable after spawn; disposed: true".into(),
            });
        };
        match association.persist_api_identity(&identity) {
            Ok(()) => {
                let mut retained = self
                    .retained
                    .lock()
                    .unwrap()
                    .remove(incarnation)
                    .expect("spawned child remains retained through association");
                if let Err(error) = nix::unistd::write(&retained.write_gate, b"x") {
                    // The store already holds an api identity for this child, and
                    // the realistic cause of this failure is that the gated shell
                    // died before reading. Returning without reaping would leave a
                    // zombie, which `presence` reads as Alive because its
                    // `/proc/<pid>/stat` is still there with matching start ticks:
                    // the failure path would then poll a dead process through
                    // grace and pause the operator over it. Uncertainty must be
                    // real rather than manufactured. The group signal is the same
                    // backstop `dispose` uses, for a child that somehow got past
                    // the gate; the identity's start ticks were read from this
                    // exact child, so the group it leads is ours.
                    drop(retained.write_gate);
                    let _ = nix::sys::signal::killpg(
                        nix::unistd::Pid::from_raw(identity.pid as i32),
                        nix::sys::signal::Signal::SIGKILL,
                    );
                    let _ = retained.child.wait();
                    return Err(DurableSpawnError::Spawn(format!(
                        "{error}; the gated child was reaped and never reached exec"
                    )));
                }
                detach_reaper(retained);
                Ok(DurableSpawnOutcome::Uncertain {
                    handle,
                    api_identity: Some(identity),
                    initialization_acknowledged: true,
                    reason: "API identity recorded; qualified worker ownership unavailable".into(),
                })
            }
            Err(AssociationError::Uncertain(reason)) => {
                self.dispose(incarnation, pid);
                Ok(DurableSpawnOutcome::Uncertain {
                    handle,
                    api_identity: Some(identity),
                    initialization_acknowledged: false,
                    reason: format!("{reason}; disposed: true"),
                })
            }
        }
    }
}

impl Drop for DurableSpawn {
    fn drop(&mut self) {
        for (_, retained) in self.retained.get_mut().unwrap().drain() {
            detach_reaper(retained);
        }
    }
}

/// The engine's own output is evidence, so it is appended to the file the plan
/// names rather than discarded. The log may hold prompts and tokens, so the
/// directories are owner-only and the file is owner read/write.
///
/// `ExecLauncher` opens the same variable but without creating parents or fixing
/// the mode; sharing one helper would change that launcher's behaviour, so the
/// stricter rule lives here with the gated spawn that needs it.
fn engine_log(cmd: &RenderedCommand) -> Result<Option<std::fs::File>, DurableSpawnError> {
    use std::os::unix::fs::DirBuilderExt;
    let Some(path) = cmd.env.get("MLLM_ENGINE_LOG") else {
        return Ok(None);
    };
    let path = std::path::Path::new(path);
    let failed = |error: std::io::Error| {
        DurableSpawnError::Spawn(format!("engine log {}: {error}", path.display()))
    };
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(failed)?;
    }
    open_private_log(path).map(Some).map_err(failed)
}

/// SPEC §13.3 / T21: open an engine log for append without following a
/// symlink, refuse anything but a regular file this user owns, and make it
/// owner-only through the open descriptor (a pre-existing file keeps no wider
/// mode than 0600).
pub fn open_private_log(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags((nix::fcntl::OFlag::O_NOFOLLOW | nix::fcntl::OFlag::O_CLOEXEC).bits())
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.uid() != nix::unistd::geteuid().as_raw() {
        return Err(std::io::Error::other("engine log is not a private regular file"));
    }
    if metadata.permissions().mode() & 0o7777 != 0o600 {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn detach_reaper(mut retained: RetainedChild) {
    std::thread::spawn(move || {
        let _gate = retained.write_gate;
        // SPEC §13.2 (W13): the parent is the only party that learns how the
        // engine ended; an exit report names it from here.
        if let (Ok(status), Some(identity)) = (retained.child.wait(), &retained.identity) {
            crate::reaped::record(identity, status);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Accept;
    impl LaunchAssociation for Accept {
        fn persist_api_identity(&self, _: &ProcessIdentity) -> Result<(), AssociationError> {
            Ok(())
        }
    }

    struct Refuse;
    impl LaunchAssociation for Refuse {
        fn persist_api_identity(&self, _: &ProcessIdentity) -> Result<(), AssociationError> {
            Err(AssociationError::Uncertain("store refused".into()))
        }
    }

    struct UnexpectedAssociation;
    impl LaunchAssociation for UnexpectedAssociation {
        fn persist_api_identity(&self, _: &ProcessIdentity) -> Result<(), AssociationError> {
            panic!("an unknown identity must not reach persistence")
        }
    }

    /// An identity that could not be read is never persisted, and the child that
    /// could not be identified is disposed of rather than left blocked on its gate.
    // T12
    #[test]
    fn unknown_api_identity_is_gated_and_disposed_of() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("must-not-initialize");
        let command = RenderedCommand {
            argv: vec![
                "sh".into(),
                "-c".into(),
                format!("touch '{}'", marker.display()),
            ],
            env: Default::default(),
        };
        let launcher = DurableSpawn::with_identity_collector(|_, _| None);
        let outcome = launcher
            .spawn_persisted("unknown-api", &command, &UnexpectedAssociation)
            .unwrap();
        let pid = match outcome {
            DurableSpawnOutcome::Uncertain {
                handle,
                api_identity: None,
                initialization_acknowledged: false,
                ..
            } => handle.pid,
            other => panic!("unexpected outcome: {other:?}"),
        };
        drop(launcher);
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "gated child still present"
        );
        assert!(!marker.exists());
    }

    /// The engine's output lands in the file the plan names, not in /dev/null, so a
    /// launch failure can be read afterwards (SPEC §13.2: evidence, not guesswork).
    // T12
    #[test]
    fn child_output_is_written_to_the_engine_log() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("logs").join("dep").join("inc.log");
        let mut env = std::collections::BTreeMap::new();
        env.insert(
            "MLLM_ENGINE_LOG".to_string(),
            log.to_str().unwrap().to_string(),
        );
        let command = RenderedCommand {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "echo hello-from-engine; echo oops >&2".into(),
            ],
            env,
        };
        let launcher = DurableSpawn::new();
        launcher
            .spawn_persisted("log-test", &command, &Accept)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(
            text.contains("hello-from-engine") && text.contains("oops"),
            "{text}"
        );
        let mode = std::fs::metadata(&log).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let parent = std::fs::metadata(log.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(parent, 0o700);
    }

    /// SPEC §13.3 / T21: an engine child inherits nothing from the agent's own
    /// environment; it sees exactly the variables its rendered command names.
    // T21
    #[test]
    fn a_durable_child_inherits_no_agent_environment() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("env.txt");
        let command = RenderedCommand {
            argv: vec![
                "/usr/bin/env".into(),
            ],
            env: std::collections::BTreeMap::from([
                ("MLLM_ENGINE_LOG".to_string(), out.to_str().unwrap().to_string()),
                ("NAMED".to_string(), "1".to_string()),
            ]),
        };
        assert!(std::env::var_os("HOME").is_some() || std::env::var_os("PATH").is_some());
        let launcher = DurableSpawn::new();
        launcher.spawn_persisted("env-test", &command, &Accept).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let text = std::fs::read_to_string(&out).unwrap();
        let names: Vec<&str> = text.lines().filter_map(|l| l.split_once('=')).map(|(n, _)| n).collect();
        assert!(names.contains(&"NAMED"), "{text}");
        for inherited in ["HOME", "PATH", "USER", "CARGO_PKG_NAME"] {
            assert!(!names.contains(&inherited), "{inherited} leaked: {text}");
        }
    }

    /// SPEC §13.3 / T21: the engine log is never opened through a symlink, and an
    /// existing log is made owner-only before the engine writes to it.
    // T21
    #[test]
    fn the_engine_log_refuses_a_symlink_and_is_made_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, "").unwrap();
        let log = dir.path().join("inc.log");
        std::os::unix::fs::symlink(&target, &log).unwrap();
        let command = |log: &std::path::Path| RenderedCommand {
            argv: vec!["sh".into(), "-c".into(), "echo out".into()],
            env: std::collections::BTreeMap::from([(
                "MLLM_ENGINE_LOG".to_string(),
                log.to_str().unwrap().to_string(),
            )]),
        };
        let launcher = DurableSpawn::new();
        assert!(launcher.spawn_persisted("symlink-log", &command(&log), &Accept).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "");
        let plain = dir.path().join("plain.log");
        std::fs::write(&plain, "").unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        launcher.spawn_persisted("plain-log", &command(&plain), &Accept).unwrap();
        let mode = std::fs::metadata(&plain).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    /// A child whose identity is never recorded is not left blocked on its gate:
    /// the launcher closes the gate, signals the group and reaps it, so no engine
    /// command ever runs unattributed.
    // T15
    #[test]
    fn an_unreleased_child_is_disposed_of_before_the_error_returns() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("must-not-run");
        let command = RenderedCommand {
            argv: vec![
                "sh".into(),
                "-c".into(),
                format!("touch '{}'", marker.display()),
            ],
            env: Default::default(),
        };
        let launcher = DurableSpawn::new();
        let outcome = launcher
            .spawn_persisted("refused", &command, &Refuse)
            .unwrap();
        let pid = match outcome {
            DurableSpawnOutcome::Uncertain {
                handle,
                initialization_acknowledged: false,
                ..
            } => handle.pid,
            other => panic!("unexpected outcome: {other:?}"),
        };
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "gated child still present"
        );
        assert!(!marker.exists(), "the engine command must never have run");
    }
}

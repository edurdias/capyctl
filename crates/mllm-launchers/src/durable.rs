use std::collections::{HashMap, HashSet};
use std::io::{Seek, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::sync::Mutex;

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

/// Launcher-owned private files. No formatting or serialization surface exposes
/// their contents; the descriptors close when this non-cloneable value is dropped.
pub struct ProtectedLaunchDescriptors {
    files: [std::fs::File; 3],
}

impl ProtectedLaunchDescriptors {
    pub fn new(launch: &[u8], inference: &[u8], admin: &[u8]) -> Result<Self, DurableSpawnError> {
        let credential = |bytes: &[u8]| {
            !bytes.is_empty() && bytes.len() <= 4096 && bytes.iter().all(|b| (33..=126).contains(b))
        };
        if launch.is_empty()
            || launch.len() > 65536
            || !credential(inference)
            || !credential(admin)
            || inference == admin
        {
            return Err(DurableSpawnError::Spawn(
                "invalid protected descriptors".into(),
            ));
        }
        fn file(bytes: &[u8]) -> std::io::Result<std::fs::File> {
            use nix::fcntl::{FcntlArg, SealFlag, fcntl};
            use nix::sys::memfd::{MemFdCreateFlag, memfd_create};
            let fd = memfd_create(
                c"mllm-private-launch",
                MemFdCreateFlag::MFD_CLOEXEC | MemFdCreateFlag::MFD_ALLOW_SEALING,
            )?;
            nix::sys::stat::fchmod(
                fd.as_raw_fd(),
                nix::sys::stat::Mode::from_bits_truncate(0o600),
            )?;
            // FD 9 belongs to the initialization gate. Duplication also ensures
            // no source descriptor can be overwritten while that gate is installed.
            let number = fcntl(fd.as_raw_fd(), FcntlArg::F_DUPFD_CLOEXEC(10))?;
            let mut file = unsafe { std::fs::File::from_raw_fd(number) };
            file.write_all(bytes)?;
            file.rewind()?;
            fcntl(
                number,
                FcntlArg::F_ADD_SEALS(
                    SealFlag::F_SEAL_WRITE
                        | SealFlag::F_SEAL_GROW
                        | SealFlag::F_SEAL_SHRINK
                        | SealFlag::F_SEAL_SEAL,
                ),
            )?;
            Ok(file)
        }
        let files = [launch, inference, admin].map(file);
        let [launch, inference, admin] = files;
        let sanitized = |_| DurableSpawnError::Spawn("protected descriptor creation failed".into());
        Ok(Self {
            files: [
                launch.map_err(sanitized)?,
                inference.map_err(sanitized)?,
                admin.map_err(sanitized)?,
            ],
        })
    }

    pub fn numbers(&self) -> [i32; 3] {
        self.files.each_ref().map(AsRawFd::as_raw_fd)
    }
}

pub struct DurableSpawn {
    attempted: Mutex<HashSet<String>>,
    retained: Mutex<HashMap<String, RetainedChild>>,
    identity_collector: fn(u32, &str) -> Option<ProcessIdentity>,
}

struct RetainedChild {
    child: std::process::Child,
    write_gate: OwnedFd,
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
        let mut command = std::process::Command::new("sh");
        if descriptors.is_some() {
            command.env_clear().stdin(std::process::Stdio::null());
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
            .insert(incarnation.into(), RetainedChild { child, write_gate });
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
                    // real rather than manufactured.
                    drop(retained.write_gate);
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
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
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
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .map(Some)
        .map_err(failed)
}

fn detach_reaper(mut retained: RetainedChild) {
    std::thread::spawn(move || {
        let _gate = retained.write_gate;
        let _ = retained.child.wait();
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

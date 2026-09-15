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
            .envs(&cmd.env)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
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
            return Ok(DurableSpawnOutcome::Uncertain {
                handle,
                api_identity: None,
                initialization_acknowledged: false,
                reason: "API identity unavailable after spawn".into(),
            });
        };
        match association.persist_api_identity(&identity) {
            Ok(()) => {
                let retained = self
                    .retained
                    .lock()
                    .unwrap()
                    .remove(incarnation)
                    .expect("spawned child remains retained through association");
                nix::unistd::write(&retained.write_gate, b"x")
                    .map_err(|error| DurableSpawnError::Spawn(error.to_string()))?;
                detach_reaper(retained);
                Ok(DurableSpawnOutcome::Uncertain {
                    handle,
                    api_identity: Some(identity),
                    initialization_acknowledged: true,
                    reason: "API identity recorded; qualified worker ownership unavailable".into(),
                })
            }
            Err(AssociationError::Uncertain(reason)) => Ok(DurableSpawnOutcome::Uncertain {
                handle,
                api_identity: Some(identity),
                initialization_acknowledged: false,
                reason,
            }),
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

fn detach_reaper(mut retained: RetainedChild) {
    std::thread::spawn(move || {
        let _gate = retained.write_gate;
        let _ = retained.child.wait();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    struct UnexpectedAssociation;
    impl LaunchAssociation for UnexpectedAssociation {
        fn persist_api_identity(&self, _: &ProcessIdentity) -> Result<(), AssociationError> {
            panic!("an unknown identity must not reach persistence")
        }
    }

    #[test]
    fn unknown_api_identity_stays_gated_and_retained() {
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
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(std::path::Path::new(&format!("/proc/{pid}")).exists());
        assert!(!marker.exists());
        nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        )
        .unwrap();
    }
}

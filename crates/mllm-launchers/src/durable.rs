use std::collections::{HashMap, HashSet};
use std::os::fd::AsRawFd;
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
    Released {
        handle: OwnedHandle,
        api_identity: ProcessIdentity,
    },
    Uncertain {
        handle: OwnedHandle,
        api_identity: ProcessIdentity,
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
    retained: Mutex<HashMap<String, std::process::Child>>,
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
        }
    }

    pub fn spawn_persisted(
        &self,
        incarnation: &str,
        cmd: &RenderedCommand,
        association: &dyn LaunchAssociation,
    ) -> Result<DurableSpawnOutcome, DurableSpawnError> {
        if incarnation.is_empty() || !self.attempted.lock().unwrap().insert(incarnation.into()) {
            return Err(DurableSpawnError::AlreadyAttempted);
        }
        let (read_gate, write_gate) =
            nix::unistd::pipe().map_err(|error| DurableSpawnError::Spawn(error.to_string()))?;
        let (program, args) = cmd
            .argv
            .split_first()
            .ok_or_else(|| DurableSpawnError::Spawn("empty argv".into()))?;
        let gate_fd = read_gate.as_raw_fd();
        let mut command = std::process::Command::new("sh");
        command
            .arg("-c")
            .arg(format!(
                "dd bs=1 count=1 <&{gate_fd} >/dev/null 2>&1 || exit 125; exec \"$@\""
            ))
            .arg("mllm-init-gate")
            .arg(program)
            .args(args)
            .envs(&cmd.env)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        unsafe {
            command.pre_exec(|| {
                nix::unistd::setpgid(nix::unistd::Pid::from_raw(0), nix::unistd::Pid::from_raw(0))
                    .map_err(std::io::Error::other)
            });
        }
        let child = command
            .spawn()
            .map_err(|error| DurableSpawnError::Spawn(error.to_string()))?;
        let pid = child.id();
        let identity = super::exec::process_identity(pid, "api").ok_or_else(|| {
            DurableSpawnError::Spawn("API identity unavailable after spawn".into())
        })?;
        let handle = OwnedHandle {
            pid,
            start_identity: super::exec::legacy_identity(&identity),
        };
        self.retained
            .lock()
            .unwrap()
            .insert(incarnation.into(), child);
        match association.persist_api_identity(&identity) {
            Ok(()) => {
                nix::unistd::write(&write_gate, &[1])
                    .map_err(|error| DurableSpawnError::Spawn(error.to_string()))?;
                if let Some(mut child) = self.retained.lock().unwrap().remove(incarnation) {
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                }
                Ok(DurableSpawnOutcome::Released {
                    handle,
                    api_identity: identity,
                })
            }
            Err(AssociationError::Uncertain(reason)) => Ok(DurableSpawnOutcome::Uncertain {
                handle,
                api_identity: identity,
                reason,
            }),
        }
    }
}

impl Drop for DurableSpawn {
    fn drop(&mut self) {
        for child in self.retained.get_mut().unwrap().values_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

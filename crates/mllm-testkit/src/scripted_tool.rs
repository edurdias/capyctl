//! Process tools that answer from a script.
//!
//! The director's decisions are what these tests are about: what mllm does with
//! processes it has already recorded, and what it refuses to conclude when it
//! cannot see them. A real process would make those decisions depend on the host
//! the suite happens to run on, so the tool answers from a script instead and
//! signals nothing. A real engine is qualified on the host, never here.

use std::sync::Mutex;
use std::sync::Arc;
use std::time::Duration;

use mllm_adapters::traits::{OwnedProcessLaunch, RenderedCommand, RuntimeError};
use mllm_domain::completion::{Presence, ProcessIdentity};

pub struct ScriptedTool {
    /// The identity a spawn reports. A tool that starts nothing has none, and its
    /// spawn is uncertain rather than a fabricated success.
    identity: Option<ProcessIdentity>,
    present: Mutex<Presence>,
    group: Vec<ProcessIdentity>,
    spawned: Mutex<Vec<RenderedCommand>>,
    /// The engine dies the moment it is spawned (the crash-before-readiness case).
    gone_on_spawn: bool,
    terminations: Mutex<Vec<Vec<ProcessIdentity>>>,
    /// Why termination could not be completed, for the path where a signal is sent
    /// and the group still cannot be proved gone.
    refusal: Option<String>,
}

impl ScriptedTool {
    /// A launch that produced `identity` and the workers under it, all alive.
    pub fn alive(identity: ProcessIdentity, workers: Vec<ProcessIdentity>) -> Self {
        let group = std::iter::once(identity.clone()).chain(workers).collect();
        Self {
            identity: Some(identity),
            present: Mutex::new(Presence::Alive),
            group,
            spawned: Mutex::new(Vec::new()),
            gone_on_spawn: false,
            terminations: Mutex::new(Vec::new()),
            refusal: None,
        }
    }

    /// A launch whose engine is gone by the time anything looks at it.
    pub fn dies_on_spawn(identity: ProcessIdentity) -> Self {
        Self {
            group: vec![identity.clone()],
            gone_on_spawn: true,
            ..Self::alive(identity, Vec::new())
        }
    }

    /// Tools that start nothing and can prove their (empty) group gone.
    pub fn proving() -> Arc<Self> {
        Self::starts_nothing(None)
    }

    /// Tools that signal and then cannot prove the group gone.
    pub fn unprovable() -> Arc<Self> {
        Self::starts_nothing(Some(
            "a recorded process could not be proven gone".into(),
        ))
    }

    fn starts_nothing(refusal: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            identity: None,
            present: Mutex::new(Presence::Gone),
            group: Vec::new(),
            spawned: Mutex::new(Vec::new()),
            gone_on_spawn: false,
            terminations: Mutex::new(Vec::new()),
            refusal,
        })
    }

    /// What this tool was asked to start, in order.
    pub fn spawned(&self) -> Vec<RenderedCommand> {
        self.spawned.lock().unwrap().clone()
    }

    /// The identity sets this tool was asked to terminate, in order.
    pub fn terminations(&self) -> Vec<Vec<ProcessIdentity>> {
        self.terminations.lock().unwrap().clone()
    }
}

impl OwnedProcessLaunch for ScriptedTool {
    fn spawn_durable(
        &self,
        _incarnation: &str,
        cmd: &RenderedCommand,
    ) -> Result<ProcessIdentity, RuntimeError> {
        self.spawned.lock().unwrap().push(cmd.clone());
        let Some(identity) = self.identity.clone() else {
            return Err(RuntimeError::Uncertain(
                "the scripted tool starts no process".into(),
            ));
        };
        if self.gone_on_spawn {
            *self.present.lock().unwrap() = Presence::Gone;
        }
        Ok(identity)
    }

    fn present(&self, _identity: &ProcessIdentity) -> Presence {
        *self.present.lock().unwrap()
    }

    fn observe_group(&self, _api: &ProcessIdentity) -> Result<Vec<ProcessIdentity>, RuntimeError> {
        Ok(self.group.clone())
    }

    fn terminate_owned(
        &self,
        identities: &[ProcessIdentity],
        _grace: Duration,
    ) -> Result<(), RuntimeError> {
        self.terminations.lock().unwrap().push(identities.to_vec());
        match &self.refusal {
            Some(reason) => Err(RuntimeError::Uncertain(reason.clone())),
            None => Ok(()),
        }
    }
}

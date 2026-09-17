//! Evidence observations for individual persisted controls. This adapter never
//! grants permission, qualifies a recipe, commits a completion, or opens dispatch.

use std::{
    collections::{BTreeSet, HashSet},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use mllm_domain::{
    completion::{ExecutionIdentities, Milestone, ProcessIdentity, TransitionToken},
    launch::NativeLaunch,
    qualification::EffectObservation,
};

use super::{
    SglangLaunch,
    http::{ControlHttp, action_timeout, uncertain},
};
use crate::traits::*;

/// Fresh facts from a trusted local collector, not an engine HTTP response.
/// `quiesced` requires a qualified all-work barrier or confirmed terminal results
/// for every registered request. Missing metrics must set `unknown_work`.
#[derive(Clone, Debug)]
pub struct SglangRuntimeObservation {
    pub token: TransitionToken,
    pub binding_id: String,
    pub incarnation: String,
    pub identities: Vec<ProcessIdentity>,
    pub real_memory_saver: bool,
    pub quiesced: bool,
    pub unknown_work: bool,
    pub allocations: bool,
    pub weights: bool,
    pub cache: bool,
}

/// The coordinator supplies this trusted seam. Each call must inspect current
/// persisted ownership and current local process/saver facts. Cached facts,
/// caller-authored request fields, and endpoint success are insufficient.
/// Implementations must fail when any required fact cannot be observed.
#[async_trait]
pub trait SglangRuntimeObserver: Send + Sync {
    async fn observe(&self) -> Result<SglangRuntimeObservation, RuntimeError>;
}

#[derive(Default)]
struct Attempts {
    active: bool,
    uncertain: bool,
    steps: HashSet<String>,
}

struct Attempt<'a> {
    state: &'a Mutex<Attempts>,
    succeeded: bool,
}
impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            state.active = false;
            state.uncertain |= !self.succeeded;
        }
    }
}

/// One immutable runtime binding. No Debug implementation exposes credentials or
/// the checkpoint root. The local attempt fence survives cancellation, but is
/// supplementary: persisted coordinator fencing remains mandatory across restart.
/// After uncertainty, reconciliation must establish a new adapter binding.
pub struct SglangAdapter {
    pub(super) forward: crate::forward::ChatHttp,
    http: ControlHttp,
    binding_id: String,
    incarnation: String,
    observer: Arc<dyn SglangRuntimeObserver>,
    attempts: Mutex<Attempts>,
}

impl SglangAdapter {
    /// Construction validates a frozen shape; it is not candidate permission or
    /// qualification. Only the coordinator may supply resolved credentials and
    /// send the current persisted child command through `execute_persisted`.
    pub fn from_frozen(
        frozen: &NativeLaunch,
        inference: String,
        admin: String,
        observer: Arc<dyn SglangRuntimeObserver>,
    ) -> Result<Self, RuntimeError> {
        SglangLaunch::from_frozen(frozen)?;
        let metadata = frozen.metadata();
        let http = ControlHttp::new(
            metadata
                .endpoint
                .parse()
                .map_err(|_| RuntimeError::Unsupported)?,
            frozen.checkpoint_root().into(),
            metadata.served_name.clone(),
            inference.clone(),
            admin,
        )?;
        Ok(Self {
            forward: crate::forward::ChatHttp::new(
                metadata
                    .endpoint
                    .parse()
                    .map_err(|_| RuntimeError::Unsupported)?,
                metadata.served_name.clone(),
                Some(inference),
            ),
            http,
            binding_id: metadata.binding_id.clone(),
            incarnation: metadata.incarnation.clone(),
            observer,
            attempts: Mutex::new(Attempts::default()),
        })
    }

    fn validate_observation(
        &self,
        command: &RuntimeCommand,
        observed: &SglangRuntimeObservation,
    ) -> Result<(), RuntimeError> {
        let c = &command.context;
        let ExecutionIdentities::Retained(expected) = &c.identities else {
            return Err(uncertain());
        };
        let identities = expected.iter().collect::<BTreeSet<_>>();
        let processes = expected
            .iter()
            .map(|identity| (&identity.boot_id, identity.pid))
            .collect::<BTreeSet<_>>();
        let roles = expected
            .iter()
            .map(|identity| &identity.role)
            .collect::<BTreeSet<_>>();
        if c.binding_id != self.binding_id
            || c.incarnation != self.incarnation
            || observed.binding_id != self.binding_id
            || observed.incarnation != self.incarnation
            || observed.token != c.token
            || !observed.real_memory_saver
            || identities.len() != expected.len()
            || processes.len() != expected.len()
            || roles.len() != expected.len()
            || expected.is_empty()
            || !expected.iter().any(|identity| identity.role == "api")
            || !expected.iter().any(|identity| identity.role == "worker-0")
            || expected.iter().any(|identity| {
                identity.pid == 0
                    || identity.start_ticks == 0
                    || identity.boot_id.is_empty()
                    || identity.boot_id != expected[0].boot_id
            })
            || observed.identities.len() != expected.len()
            || observed.identities.iter().collect::<BTreeSet<_>>() != identities
            || clock_ms()? > c.deadline_ms
        {
            return Err(uncertain());
        }
        Ok(())
    }

    async fn execute_one(
        &self,
        command: &RuntimeCommand,
        timeout: std::time::Duration,
    ) -> Result<EffectObservation, RuntimeError> {
        let before = self.observer.observe().await.map_err(|_| uncertain())?;
        self.validate_observation(command, &before)?;
        let valid = match command.action {
            RuntimeAction::Drain => before.quiesced && !before.unknown_work,
            RuntimeAction::Park => before.allocations && before.quiesced && !before.unknown_work,
            RuntimeAction::Restore => !before.allocations && !before.unknown_work,
            RuntimeAction::ReloadWeights => {
                before.allocations && !before.weights && !before.unknown_work
            }
            RuntimeAction::InvalidateCache => {
                before.allocations && before.weights && !before.unknown_work
            }
            RuntimeAction::Probe => {
                before.allocations && before.weights && before.cache && !before.unknown_work
            }
            _ => false,
        };
        if !valid {
            return Err(uncertain());
        }
        if command.action != RuntimeAction::Drain {
            self.http.execute(command.action, timeout).await?;
        }
        let after = self.observer.observe().await.map_err(|_| uncertain())?;
        self.validate_observation(command, &after)?;
        let (valid, fact) = match command.action {
            RuntimeAction::Drain => (after.quiesced, Milestone::Quiesced),
            RuntimeAction::Park => (
                !after.allocations && !after.weights && !after.cache && after.quiesced,
                Milestone::MemoryReleased,
            ),
            RuntimeAction::Restore => (after.allocations, Milestone::AllocationsRestored),
            RuntimeAction::ReloadWeights => {
                (after.allocations && after.weights, Milestone::WeightsUsable)
            }
            RuntimeAction::InvalidateCache => (
                after.allocations && after.weights && after.cache,
                Milestone::CacheValid,
            ),
            RuntimeAction::Probe => (
                after.allocations && after.weights && after.cache,
                Milestone::ModelUsable,
            ),
            _ => return Err(RuntimeError::Unsupported),
        };
        if !valid || after.unknown_work {
            return Err(uncertain());
        }
        Ok(EffectObservation {
            token: command.context.token.clone(),
            binding_id: self.binding_id.clone(),
            incarnation: self.incarnation.clone(),
            identities: after.identities,
            observed_at_ms: clock_ms()?,
            receipt: format!(
                "sglang-control-v1:{:?}:{}",
                command.action, command.context.token.step_id
            ),
            facts: vec![fact],
        })
    }
}

fn clock_ms() -> Result<i64, RuntimeError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .ok_or_else(uncertain)
}

#[async_trait]
impl EngineAdapter for SglangAdapter {
    async fn execute_persisted(
        &self,
        command: &RuntimeCommand,
    ) -> Result<EffectObservation, RuntimeError> {
        let cap = action_timeout(command.action)?;
        let c = &command.context;
        let now = clock_ms()?;
        if c.issued_at_ms < 0
            || c.issued_at_ms > now
            || c.deadline_ms <= now
            || c.grant_id.as_ref().is_none_or(|grant| grant.is_empty())
            || c.completion_target.is_some()
            || c.launch_settings.is_some()
            || c.token.revision < 1
            || c.token.generation < 1
            || [
                &c.token.deployment_id,
                &c.token.operation_id,
                &c.token.step_id,
                &c.token.qualification_id,
            ]
            .iter()
            .any(|value| value.is_empty())
        {
            return Err(uncertain());
        }
        let timeout = cap.min(std::time::Duration::from_millis(
            (c.deadline_ms - now) as u64,
        ));
        {
            let mut attempts = self.attempts.lock().map_err(|_| uncertain())?;
            if attempts.active
                || attempts.uncertain
                || attempts.steps.len() >= 4096
                || !attempts.steps.insert(c.token.step_id.clone())
            {
                return Err(uncertain());
            }
            attempts.active = true;
        }
        let mut attempt = Attempt {
            state: &self.attempts,
            succeeded: false,
        };
        let result = tokio::time::timeout(timeout, self.execute_one(command, timeout))
            .await
            .map_err(|_| uncertain())?;
        attempt.succeeded = result.is_ok();
        result
    }

    async fn inspect(&self, _member: &MemberRef) -> Result<EngineState, AdapterError> {
        Err(AdapterError::Uncertain(
            "persisted lifecycle inspection required".into(),
        ))
    }
    async fn render_plan(&self, _plan: &PlanInput) -> Result<RenderedCommand, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn check_readiness(&self, _member: &MemberRef) -> Result<Readiness, AdapterError> {
        // Even a successful probe only emits ModelUsable; the coordinator must
        // verify and commit the complete ordered milestone sequence before Ready.
        Ok(Readiness::Initializing)
    }
    async fn prepare_park(&self, _member: &MemberRef) -> Result<Quiescence, AdapterError> {
        Ok(Quiescence { quiescent: false })
    }
    async fn park(
        &self,
        _member: &MemberRef,
        _level: ParkLevel,
    ) -> Result<ParkOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn restore(&self, _member: &MemberRef) -> Result<RestoreOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn reload_weights(&self, _member: &MemberRef) -> Result<ReloadOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn observe_work(&self, _member: &MemberRef) -> Result<WorkObservation, AdapterError> {
        Ok(WorkObservation::Unknown)
    }
    async fn cancel_work(
        &self,
        _member: &MemberRef,
        _request: &RequestRef,
        _require_ack: bool,
    ) -> Result<CancellationOutcome, AdapterError> {
        Ok(CancellationOutcome::Uncertain)
    }
}

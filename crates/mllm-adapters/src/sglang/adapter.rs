//! Evidence observations for individual persisted controls, and the owned
//! launch this adapter performs on Initialize. This adapter never grants
//! permission, verifies a recipe, commits a completion, or opens dispatch.

use std::{
    collections::{BTreeSet, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use mllm_domain::{
    completion::{
        EffectObservation, ExecutionIdentities, Milestone, ProcessIdentity, TransitionToken,
    },
    launch::NativeLaunch,
};

use super::{
    http::{action_timeout, uncertain, ControlHttp},
    SglangLaunch,
};
use crate::traits::*;

/// Fresh facts from a trusted local collector, not an engine HTTP response.
/// `quiesced` requires a verified all-work barrier or confirmed terminal results
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

/// The frozen launch this adapter instance owns, plus the public settings
/// rendered from it. Rendering is deterministic and refuses anything the frozen
/// validation refused, so re-rendering has no effects and no secret surface.
pub struct SglangLaunchHandle {
    pub(super) frozen: NativeLaunch,
    pub(super) rendered: SglangLaunch,
}

/// One immutable runtime binding. No Debug implementation exposes credentials or
/// the checkpoint root. The local attempt fence survives cancellation, but is
/// supplementary: persisted coordinator fencing remains mandatory across restart.
/// After uncertainty, reconciliation must establish a new adapter binding.
pub struct SglangAdapter {
    pub(super) forward: crate::forward::ChatHttp,
    http: Option<ControlHttp>,
    base: reqwest::Url,
    checkpoint: String,
    binding_id: String,
    incarnation: String,
    served_name: String,
    /// The trusted seam for persisted controls. An adapter built for launch has
    /// none: its control actions are the honest refusal (design §4.4).
    observer: Option<Arc<dyn SglangRuntimeObserver>>,
    attempts: Mutex<Attempts>,
    /// The managed-launch contract (Spec §3): the concrete frozen launch this
    /// adapter renders and spawns for an owned launch.
    launch: Option<NativeLaunch>,
    /// Process tools supplied by the director (Spec §3). The builder spawns,
    /// watches and enumerates through them and never learns where the
    /// identities it produces are recorded.
    tools: Option<Arc<dyn OwnedProcessLaunch>>,
    /// The service-owned protected entrypoint wrapper path. Rendering refuses
    /// to build a command without one.
    wrapper: Option<PathBuf>,
    /// Where the engine's own log is expected; quoted (redacted) when a launch
    /// dies before readiness.
    log: Option<String>,
    /// The coordinator session ULID the private descriptor names (descriptor
    /// contract v2). Threaded by the coordinator's resolved-spawn factory.
    session: Option<String>,
    /// The per-launch credentials. They reach the engine through protected
    /// descriptors and appear in no argv, env, log or receipt (SPEC §13.3).
    inference_key: Option<String>,
    admin_key: Option<String>,
    /// The launch this adapter instance owns: the binding and incarnation it
    /// claimed. The claim is taken before a process exists, so a step that
    /// failed mid-launch still holds it and a repeat cannot start a second
    /// engine over the first one's memory. Spec §5 has cleanup take identities
    /// from the execution context, never from adapter memory, so none is kept.
    launched: Mutex<Option<(String, String)>>,
}

impl SglangAdapter {
    /// Construction validates a frozen shape; it is not runtime permission or
    /// verification. Only the coordinator may supply resolved credentials and
    /// send the current persisted child command through `execute_persisted`.
    pub fn from_frozen(
        frozen: &NativeLaunch,
        observer: Option<Arc<dyn SglangRuntimeObserver>>,
    ) -> Result<Self, RuntimeError> {
        SglangLaunch::from_frozen(frozen)?;
        let metadata = frozen.metadata();
        let base: reqwest::Url = metadata
            .endpoint
            .parse()
            .map_err(|_| RuntimeError::Unsupported)?;
        Ok(Self {
            forward: crate::forward::ChatHttp::new(
                base.clone(),
                metadata.served_name.clone(),
                None,
            ),
            http: None,
            base,
            checkpoint: frozen.checkpoint_root().into(),
            binding_id: metadata.binding_id.clone(),
            incarnation: metadata.incarnation.clone(),
            served_name: metadata.served_name.clone(),
            observer,
            attempts: Mutex::new(Attempts::default()),
            launch: None,
            tools: None,
            wrapper: None,
            log: None,
            session: None,
            inference_key: None,
            admin_key: None,
            launched: Mutex::new(None),
        })
    }

    /// Attach the per-launch credentials (Spec §3). They are delivered to the
    /// child through protected descriptors only, and they are the credentials
    /// this adapter then presents on every request of its own: the engine
    /// guards `/v1` with exactly the inference key, so a readiness probe or an
    /// inference probe sent without it is refused, never merely slow (SPEC
    /// §6.1). An invalid pair leaves the control surface unset, which reports
    /// uncertainty rather than pretending to hold a credential.
    pub fn with_credentials(mut self, inference: String, admin: String) -> Self {
        self.http = ControlHttp::new(
            self.base.clone(),
            self.checkpoint.clone(),
            self.served_name.clone(),
            inference.clone(),
            admin.clone(),
        )
        .ok();
        self.forward = crate::forward::ChatHttp::new(
            self.base.clone(),
            self.served_name.clone(),
            Some(inference.clone()),
        );
        self.inference_key = Some(inference);
        self.admin_key = Some(admin);
        self
    }

    /// The coordinator session whose ULID the private launch descriptor names.
    /// The descriptor contract (`runtime/sglang_entry.py::_validate_launch_scope`)
    /// requires it to be a coordinator session ULID, so it is threaded here by
    /// the one caller — the coordinator's resolved-spawn factory — that holds
    /// the session. A launch built without one refuses the step.
    pub fn with_session(mut self, session_id: impl Into<String>) -> Self {
        self.session = Some(session_id.into());
        self
    }

    /// Attach the managed-launch contract (Spec §3): the concrete frozen launch
    /// this adapter renders in `initialize`. A launch whose served name is not
    /// the deployment's route token (`args::served_name_token`) is not attached:
    /// rendering it would produce a descriptor the entry refuses, so the step
    /// must refuse instead, which is what a missing launch makes `launch_parts`
    /// do.
    pub fn with_launch(mut self, launch: NativeLaunch) -> Self {
        let metadata = launch.metadata();
        if super::args::served_name_token(&metadata.served_name) {
            self.launch = Some(launch);
        }
        self
    }

    /// Attach the process tools the director supplies for an owned launch
    /// (Spec §3). Without them the adapter answers Initialize with
    /// `Unsupported`: it has no way to spawn anything it could prove it owns.
    pub fn with_tools(mut self, tools: Arc<dyn OwnedProcessLaunch>) -> Self {
        self.tools = Some(tools);
        self
    }

    /// Attach the service-owned wrapper path the rendered command runs through.
    /// Service configuration supplies this path, never a candidate or HTTP
    /// request; rendering revalidates it immediately before use.
    pub fn with_wrapper(mut self, wrapper: PathBuf) -> Self {
        self.wrapper = Some(wrapper);
        self
    }

    /// Attach the engine log path, delivered as `MLLM_ENGINE_LOG` and quoted
    /// (redacted) when a launch dies before readiness.
    pub fn with_log(mut self, log: impl Into<String>) -> Self {
        self.log = Some(log.into());
        self
    }

    /// The engine's recipe pin, for the receipt an Initialize records. A builder
    /// with no launch is unsupported rather than assumed to have one.
    pub(super) fn fingerprint(&self) -> Result<&str, RuntimeError> {
        Ok(&self
            .launch
            .as_ref()
            .ok_or(RuntimeError::Unsupported)?
            .metadata()
            .recipe)
    }

    /// The engine endpoint this adapter talks to, for the same receipt.
    pub(super) fn endpoint(&self) -> Result<&str, RuntimeError> {
        Ok(&self
            .launch
            .as_ref()
            .ok_or(RuntimeError::Unsupported)?
            .metadata()
            .endpoint)
    }

    /// The session ULID the private launch descriptor names. A builder without
    /// one — or with an empty one, which names no session — is unsupported:
    /// the descriptor contract requires a coordinator session ULID and there is
    /// no honest substitute for the one the coordinator holds.
    pub(super) fn session(&self) -> Result<&str, RuntimeError> {
        self.session
            .as_deref()
            .filter(|session| !session.is_empty())
            .ok_or(RuntimeError::Unsupported)
    }

    /// The served name the launch was built to answer on.
    pub(super) fn served_name(&self) -> &str {
        &self.served_name
    }

    /// The engine log path, for the tail quoted when a launch dies.
    pub(super) fn engine_log(&self) -> Option<&str> {
        self.log.as_deref()
    }

    /// The wrapper path the rendered command runs through. A builder without
    /// one is unsupported: nothing rendered, nothing spawned.
    pub(super) fn wrapper_path(&self) -> Result<&Path, RuntimeError> {
        self.wrapper.as_deref().ok_or(RuntimeError::Unsupported)
    }

    /// The four things an owned launch needs. Any one missing makes the step
    /// unsupported rather than partly performed.
    pub(super) fn launch_parts(
        &self,
    ) -> Result<
        (
            SglangLaunchHandle,
            Arc<dyn OwnedProcessLaunch>,
            String,
            String,
        ),
        RuntimeError,
    > {
        match (
            &self.launch,
            &self.tools,
            &self.inference_key,
            &self.admin_key,
        ) {
            (Some(launch), Some(tools), Some(inference), Some(admin)) => {
                let rendered = SglangLaunch::from_frozen(launch)?;
                Ok((
                    SglangLaunchHandle {
                        frozen: launch.clone(),
                        rendered,
                    },
                    tools.clone(),
                    inference.clone(),
                    admin.clone(),
                ))
            }
            _ => Err(RuntimeError::Unsupported),
        }
    }

    /// Claim this adapter's one launch. A second claim is refused: the adapter
    /// instance is built for a single incarnation, and a repeat would spawn a
    /// second engine while the first is still recorded as owned.
    pub(super) fn claim_incarnation(
        &self,
        binding_id: &str,
        incarnation: &str,
    ) -> Result<(), RuntimeError> {
        let mut launched = self
            .launched
            .lock()
            .map_err(|_| RuntimeError::Uncertain("launch record is poisoned".into()))?;
        if launched.is_some() {
            return Err(RuntimeError::Unsupported);
        }
        *launched = Some((binding_id.to_string(), incarnation.to_string()));
        Ok(())
    }

    fn require_observer(&self) -> Result<&Arc<dyn SglangRuntimeObserver>, RuntimeError> {
        self.observer.as_ref().ok_or(RuntimeError::Unsupported)
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
        let observer = self.require_observer()?;
        let before = observer.observe().await.map_err(|_| uncertain())?;
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
            self.http
                .as_ref()
                .ok_or_else(uncertain)?
                .execute(command.action, timeout)
                .await?;
        }
        let after = observer.observe().await.map_err(|_| uncertain())?;
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
    /// Spec §4: the builder performs Initialize end to end. The persisted
    /// control path below never sees it: its action table has no Initialize
    /// arm, and an Initialize command carries launch settings the control
    /// fence refuses.
    async fn execute_persisted(
        &self,
        command: &RuntimeCommand,
    ) -> Result<EffectObservation, RuntimeError> {
        if command.action == RuntimeAction::Initialize {
            return crate::sglang::initialize::initialize(self, command).await;
        }
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
        // SPEC §6.1: liveness of an HTTP server is not model readiness; the
        // served name appearing in `/v1/models` is the readiness signal.
        let Some(http) = self.http.as_ref() else {
            // The adapter holds no launch credential, so its readiness poll
            // would be refused by the engine: the refusal is named, never
            // quietly reported as Initializing.
            return Err(AdapterError::Uncertain(
                "no launch credential configured; readiness is refused, never assumed".into(),
            ));
        };
        match http.models().await {
            Ok(ids) if ids.contains(&self.served_name) => Ok(Readiness::Ready),
            Ok(_) => Ok(Readiness::Initializing),
            Err(super::http::ModelsError::Unreachable) => {
                // Not listening yet: Initializing (the readiness loop polls);
                // crash detection is the launcher's job.
                Ok(Readiness::Initializing)
            }
            Err(e) => Err(AdapterError::Uncertain(crate::vllm::args::redact_text(
                &format!("model list: {e}"),
            ))),
        }
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

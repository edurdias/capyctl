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
use capyctl_domain::{
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

/// What the adapter lends its observer for one read: the launch's own loopback
/// endpoint, installation and credentials. An observer built once per launch
/// (the embedded host) takes them from here, so credentials recovered after a
/// restart are the ones used, never a copy taken before.
pub struct ObservationAccess<'a> {
    pub binding_id: &'a str,
    pub incarnation: &'a str,
    pub endpoint: &'a str,
    pub executable: &'a str,
    pub inference_key: &'a str,
    pub admin_key: &'a str,
    /// ADR 0014 amendment A17: the launch parks with its weights resident, so
    /// a released saver map is the KV cache unmapped and the weights mapped.
    pub resident_weights: bool,
}

/// The coordinator supplies this trusted seam. Each call must inspect current
/// persisted ownership and current local process/saver facts. Cached facts,
/// caller-authored request fields, and endpoint success are insufficient.
/// Implementations must fail when any required fact cannot be observed.
#[async_trait]
pub trait SglangRuntimeObserver: Send + Sync {
    async fn observe(&self) -> Result<SglangRuntimeObservation, RuntimeError>;

    /// The observation around one persisted step: before its engine effect
    /// (`after == false`) and after it. An observer built for exactly one step
    /// keeps the default; one that serves a whole launch binds the step here.
    async fn observe_step(
        &self,
        _command: &RuntimeCommand,
        _after: bool,
        _access: &ObservationAccess<'_>,
    ) -> Result<SglangRuntimeObservation, RuntimeError> {
        self.observe().await
    }

    /// SPEC §10 step 4 before an embedded Park, with no engine effect: the
    /// engine proves quiescence and a fully mapped saver. Unknown is `false`.
    async fn quiescent(&self, _access: &ObservationAccess<'_>) -> bool {
        false
    }

    /// SPEC §9.2: the host's private directory a memory-saver launch enrolls
    /// its saver observation in (`CAPYCTL_OBSERVATION_DIR`), if this host has one.
    fn observation_dir(&self) -> Option<PathBuf> {
        None
    }
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

/// The protected credentials one owned launch is handed.
pub(super) enum LaunchCredentials {
    /// A single rank or a group head: the inference and admin credentials.
    Api { inference: String, admin: String },
    /// ADR 0028 §10, ADR 0012: a group worker is handed no API credential;
    /// a deep one gets its own observation credential (ADR 0028 §12).
    Worker { observation: Option<String> },
}

/// An owned launch's parts: the launch, the process tools and its credentials.
pub(super) type LaunchParts = (
    SglangLaunchHandle,
    Arc<dyn OwnedProcessLaunch>,
    LaunchCredentials,
);

/// One immutable runtime binding. No Debug implementation exposes credentials or
/// the checkpoint root. The local attempt fence survives cancellation, but is
/// supplementary: persisted coordinator fencing remains mandatory across restart.
/// After uncertainty, reconciliation must establish a new adapter binding.
pub struct SglangAdapter {
    pub(super) forward: crate::forward::ChatHttp,
    pub(super) http: Option<ControlHttp>,
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
    /// ADR 0028 §12 (R12): a deep group worker's per-launch observation
    /// credential. It reaches the entry through its own protected descriptor
    /// and keys the worker's saver observation (`observation::observation_key`)
    /// exactly as the admin credential keys the head's, so the host's observer
    /// verifies the worker with it.
    observation_credential: Option<String>,
    /// The installation interpreter the frozen launch names (its prefix bounds
    /// where the saver library an observation reports may live).
    executable: String,
    /// SPEC §9.2: the private directory an enrolling launch publishes its saver
    /// observation in; the observer's own directory when this is unset.
    observation_dir: Option<PathBuf>,
    /// SPEC §8.2 / T21: the host-named directory the entry keeps its file
    /// rendezvous in, so the host can remove it once the group is gone.
    rendezvous_dir: Option<PathBuf>,
    /// ADR 0014 §8, SPEC §8.2: the host's approvals for sensitive extra
    /// arguments, delivered as `CAPYCTL_EXTRA_APPROVALS`. `None` approves nothing.
    extra_approvals: Option<String>,
    /// The launch this adapter instance owns: the binding and incarnation it
    /// claimed. The claim is taken before a process exists, so a step that
    /// failed mid-launch still holds it and a repeat cannot start a second
    /// engine over the first one's memory. Spec §5 has cleanup take identities
    /// from the execution context, never from adapter memory, so none is kept.
    launched: Mutex<Option<(String, String)>>,
    /// ADR 0010, ADR 0019, discrete GPU design §5: the frozen launch renders
    /// SGLang's weights CPU backup (`host_backed`), so a resume restores the
    /// weights from the pinned host copy and the reload step has nothing to
    /// send. Fixed by the frozen settings at construction, never by a command.
    cpu_weight_backup: bool,
    /// ADR 0014 amendment A17: the frozen launch's park keeps the weights
    /// resident (`weight_restore: resident`, SGLang with speculative
    /// decoding): release and resume name the KV cache alone, and the reload
    /// step has nothing to send.
    resident_weights: bool,
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
                true,
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
            observation_credential: None,
            executable: frozen.executable().into(),
            observation_dir: None,
            rendezvous_dir: None,
            extra_approvals: None,
            launched: Mutex::new(None),
            cpu_weight_backup: frozen.settings().cpu_weight_backup,
            resident_weights: frozen.settings().weight_restore == "resident",
        })
    }

    /// SPEC §9.2: the host's private directory (0700, service-owned) where a
    /// memory-saver launch enrolls its saver observation. Delivered to the
    /// entry as `CAPYCTL_OBSERVATION_DIR`; the entry revalidates it.
    pub fn with_observation_dir(mut self, dir: PathBuf) -> Self {
        self.observation_dir = Some(dir);
        self
    }

    /// SPEC §8.2 / T21: the per-launch rendezvous directory the host names
    /// (inside its private root) and removes on gone evidence. Delivered to
    /// the entry as `CAPYCTL_RENDEZVOUS_DIR`; the entry creates it 0700 and
    /// refuses one that already exists.
    pub fn with_rendezvous_dir(mut self, dir: PathBuf) -> Self {
        self.rendezvous_dir = Some(dir);
        self
    }

    /// ADR 0014 §8, SPEC §8.2: the host's approvals document
    /// (`capyctl_config::engine_policy::extra_approvals_document`), which the entry
    /// applies to the destinations the extras resolve to.
    pub fn with_extra_approvals(mut self, approvals: String) -> Self {
        self.extra_approvals = Some(approvals);
        self
    }

    pub(super) fn extra_approvals(&self) -> Option<&str> {
        self.extra_approvals.as_deref()
    }

    pub(super) fn rendezvous_dir(&self) -> Option<&std::path::Path> {
        self.rendezvous_dir.as_deref()
    }

    /// The observation directory an Initialize hands the entry, if any.
    pub(super) fn observation_dir(&self) -> Option<PathBuf> {
        self.observation_dir
            .clone()
            .or_else(|| self.observer.as_ref().and_then(|o| o.observation_dir()))
    }

    fn access(&self) -> Result<ObservationAccess<'_>, RuntimeError> {
        match (&self.inference_key, &self.admin_key) {
            (Some(inference), Some(admin)) => Ok(ObservationAccess {
                binding_id: &self.binding_id,
                incarnation: &self.incarnation,
                endpoint: self.base.as_str(),
                executable: &self.executable,
                inference_key: inference,
                admin_key: admin,
                resident_weights: self.resident_weights,
            }),
            _ => Err(uncertain()),
        }
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
        .ok()
        .map(|http| http.with_resident_weights(self.resident_weights));
        self.forward = crate::forward::ChatHttp::new(
            self.base.clone(),
            self.served_name.clone(),
            Some(inference.clone()),
            true,
        );
        self.inference_key = Some(inference);
        self.admin_key = Some(admin);
        self
    }

    /// ADR 0028 §12 (R12): a deep group worker's per-launch observation
    /// credential, handed to its entry on its own protected descriptor. The
    /// worker's scheduler keys its saver observation with it (it holds no
    /// admin credential, ADR 0012), and the host's observer derives the same
    /// key from it (`observation::observation_key`). Ignored for every other
    /// launch.
    pub fn with_observation_credential(mut self, credential: String) -> Self {
        self.observation_credential = Some(credential);
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

    /// Attach the engine log path, delivered as `CAPYCTL_ENGINE_LOG` and quoted
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
    /// unsupported rather than partly performed. ADR 0028 §10, ADR 0012: a
    /// group worker serves no API and is handed no API credential, so its
    /// parts carry none even when this adapter holds a pair; a deep worker
    /// needs its observation credential instead (ADR 0028 §12), and a worker
    /// that is not deep is handed none.
    pub(super) fn launch_parts(&self) -> Result<LaunchParts, RuntimeError> {
        let (Some(launch), Some(tools)) = (&self.launch, &self.tools) else {
            return Err(RuntimeError::Unsupported);
        };
        let rendered = SglangLaunch::from_frozen(launch)?;
        let credentials = if rendered.is_group_worker() {
            LaunchCredentials::Worker {
                observation: match (rendered.worker_observes(), &self.observation_credential) {
                    (false, _) => None,
                    (true, Some(observation)) => Some(observation.clone()),
                    (true, None) => return Err(RuntimeError::Unsupported),
                },
            }
        } else {
            match (&self.inference_key, &self.admin_key) {
                (Some(inference), Some(admin)) => LaunchCredentials::Api {
                    inference: inference.clone(),
                    admin: admin.clone(),
                },
                _ => return Err(RuntimeError::Unsupported),
            }
        };
        Ok((
            SglangLaunchHandle {
                frozen: launch.clone(),
                rendered,
            },
            tools.clone(),
            credentials,
        ))
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
            // ADR 0027: every engine process recorded and no other; a helper
            // (an idle compile worker) may have exited.
            || !capyctl_domain::completion::same_engine(expected, &observed.identities)
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
        let access = self.access()?;
        let before = observer
            .observe_step(command, false, &access)
            .await
            .map_err(|_| uncertain())?;
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
        // Discrete GPU design §5: under the weights CPU backup the resume
        // already copied the weights back from host RAM, so the reload step
        // sends no `update_weights_from_disk`. It still reports
        // `WeightsUsable` only from the saver observations around it, and the
        // fresh probe after the flush proves the model usable.
        // ADR 0014 A17: resident weights were never released, so there is
        // nothing to reload either.
        let host_restored = command.action == RuntimeAction::ReloadWeights
            && (self.cpu_weight_backup || self.resident_weights);
        if command.action != RuntimeAction::Drain && !host_restored {
            self.http
                .as_ref()
                .ok_or_else(uncertain)?
                .execute(command.action, timeout)
                .await?;
        }
        let after = observer
            .observe_step(command, true, &access)
            .await
            .map_err(|_| uncertain())?;
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
            kernel_builds: Vec::new(),
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
        // SPEC §10 step 4: only the observer can prove quiescence and a fully
        // mapped saver; without one (or without credentials) it is not.
        let quiescent = match (&self.observer, self.access()) {
            (Some(observer), Ok(access)) => observer.quiescent(&access).await,
            _ => false,
        };
        Ok(Quiescence { quiescent })
    }
    async fn engine_quiescent(&self, _member: &MemberRef, _after_ms: i64) -> bool {
        // SPEC §10 (amended 2026-10-01): the engine's own running and queued
        // gauges, read now with the launch's inference key. No key is unknown.
        let Some(key) = &self.inference_key else {
            return false;
        };
        crate::sglang::observation::engine_idle(self.base.as_str(), key).await == Ok(true)
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
    /// Owner decision 2026-10-09: the wake canary of an embedded launch, the
    /// completion probe on this engine's own loopback endpoint and key.
    async fn wake_canary(
        &self,
        _context: &capyctl_domain::completion::StepExecutionContext,
        max_tokens: u32,
        bound: std::time::Duration,
    ) -> Result<crate::completion_probe::ProbeAnswer, RuntimeError> {
        match ChatForward::complete_probe(self, "", max_tokens, bound).await {
            Ok(answer) => Ok(answer),
            Err(AdapterError::UnsupportedCapability) => Err(RuntimeError::Unsupported),
            Err(e) => Err(RuntimeError::Uncertain(format!(
                "SGLang did not answer the wake canary: {e}"
            ))),
        }
    }
}

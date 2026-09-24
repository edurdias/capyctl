//! SPEC §§3, 6, 7, 13: native effects are locally resolved and durably owned.
//! The same SGLang and vLLM adapters and guarded recipes serve embedded and
//! remote hosts. Every engine family shares one launch tail (ingress
//! registration, Initialize, durable readiness, gate publication) and one fresh
//! readiness probe, so recovery behaves the same whichever engine is running.
use crate::{
    checkpoint::{CheckpointError, CheckpointVerifier},
    ingress::{Ingress, IngressScope},
    ingress_identity::{IngressIdentities, NativeCredentials},
    journal::{
        Acceptance, ApprovedLaunch, ExecutionTicket, HostJournal, JournalError,
        LocalExecutionPolicy,
    },
    session::{ExecutionFuture, Provisioned, SessionError, SessionExecution},
};
use mllm_adapters::{
    sglang::{SglangAdapter, frozen_from_effective},
    traits::{
        ChatForward, EngineAdapter, MemberRef, OwnedProcessLaunch, Readiness, RuntimeAction,
        RuntimeCommand,
    },
};
use mllm_config::{
    effective::EffectiveDeployment,
    engine_policy::Engine,
    remote_roles::HostConfig,
};
use mllm_domain::completion::{ExecutionIdentities, StepExecutionContext, TransitionToken};
use mllm_protocol::{
    execution::{DigestCheckpointPlan, MemberAction, MemberCommand, SingleLaunchPlan},
    pb,
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

// SPEC §3: vLLM's recipe and guard live in their own module.
mod vllm;
// SPEC §§9.1, 10 (W4): remote Park and Restore of a retained launch.
mod residency;
pub use residency::{SaverMapped, SaverResidency, SaverScope, SaverUnavailable};
// SPEC §9.2: the enrolled-scheduler saver source and the embedded observer.
mod saver_source;
pub use saver_source::{EnrolledSaver, LaunchSglangObserver};
// SPEC §13 (WE3 limit 1): pre-effect policy refusals are terminal results.
mod refusal;
use refusal::LaunchVerdict;
// SPEC §§3.1, 7.3 (per-launch claims): host-side co-residence admission.
mod coresidence;

/// The longest one fresh `/v1/models` readiness read may take.
const PROBE_MODELS_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest the fresh chat probe may take. A loaded model answers an
/// eight-token completion well inside this; one that does not is not usable.
const PROBE_CHAT_TIMEOUT: Duration = Duration::from_secs(60);
/// The probe ends this far ahead of its command deadline so its result, not a
/// transport timeout, is what the controller receives.
const PROBE_MARGIN_MS: i64 = 1_000;

#[derive(Default)]
struct SessionAuthority {
    session: Option<u64>,
    /// Per-launch claims: the command whose persisted readiness opened each
    /// retained launch's gate in this session, keyed by the launch's owned
    /// handle. Launches co-reside, so one launch's readiness never stands in
    /// for, or revokes, another's.
    ready: std::collections::BTreeMap<String, String>,
}
#[derive(Clone)]
pub struct NativeHostExecution {
    journal: Arc<HostJournal>,
    ingress: Arc<Ingress>,
    identities: Arc<IngressIdentities>,
    config: HostConfig,
    host_id: String,
    controller_id: String,
    runtime_dir: PathBuf,
    log_dir: PathBuf,
    inventory: pb::ReportInventory,
    authority: Arc<Mutex<SessionAuthority>>,
    /// SGLang saver-map observation for residency evidence. Without one the
    /// host cannot prove an SGLang release or restoration and refuses both.
    saver: Option<Arc<dyn SaverResidency>>,
    /// SPEC §10, D9: loopback engine load scrapes of Ready scopes.
    load: Option<Arc<crate::load::LoadReporter>>,
    /// ADR 0014 §7 (WE3): this host's checkpoint digests and stat cache.
    checkpoints: Arc<CheckpointVerifier>,
    /// ADR 0008 (owner decision 2026-09-23): the installations this host
    /// registered, drift its launches found, and launch-time capability probes.
    installations: Arc<crate::installation::InstallationRegistry>,
    /// ADR 0007: per-process resident memory reported beside availability,
    /// so the server can credit resident engines instead of counting them
    /// twice (found live 2026-09-23, matrix M33). `None` reports none.
    residency: Option<Arc<crate::process_residency::ResidencySampler>>,
    /// SPEC §8.2 / T21: the private root SGLang launches keep their file
    /// rendezvous in, removed per launch on gone evidence. `None`: the entry
    /// uses its own temporary directory, removed only at interpreter exit.
    rendezvous: Option<crate::rendezvous::RendezvousRoot>,
    /// SPEC §13.2: commands whose slow admission (checkpoint hashing, the
    /// installation measurement, the capability probe) passed just now,
    /// outside the journal's locks, keyed by their canonical digest. Under the
    /// lock such a command is rechecked cheaply; anything else is admitted in
    /// full there.
    pre_admitted: Arc<Mutex<std::collections::HashMap<[u8; 32], std::time::Instant>>>,
}

/// How long a pre-admission stands for the locked recheck.
const PRE_ADMISSION_TTL: Duration = Duration::from_secs(120);
/// The most pre-admissions held at once.
const MAX_PRE_ADMISSIONS: usize = 256;

/// A native engine the host can both drive and question: every supported
/// family's adapter answers the readiness list and the chat probe.
trait NativeEngine: EngineAdapter + ChatForward {}
impl<T: EngineAdapter + ChatForward> NativeEngine for T {}

/// The engine-specific half of a launch, built from local policy before the
/// durable attempt begins. Building it has no effect.
enum PreparedLaunch {
    Sglang(Box<mllm_domain::launch::NativeLaunch>),
    Vllm(Box<mllm_adapters::vllm::PlanInputVllm>),
}

impl NativeHostExecution {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        journal: Arc<HostJournal>,
        ingress: Arc<Ingress>,
        identities: Arc<IngressIdentities>,
        config: HostConfig,
        host_id: String,
        controller_id: String,
        runtime_dir: PathBuf,
        log_dir: PathBuf,
        mut inventory: pb::ReportInventory,
    ) -> Arc<Self> {
        // SPEC §§3.1, 7.3: this executor keeps one journal claim per launch
        // and admits each beside the others, so it says so in every
        // publication; the controller then places co-resident launches here.
        // ADR 0013 §5 (journal v5): it also fences each instance of a
        // deployment on its own, so instances of one deployment may share it.
        inventory.launch_claims = crate::journal::PER_INSTANCE_CLAIMS.into();
        let installations = Arc::new(crate::installation::InstallationRegistry::from_inventory(
            &inventory,
        ));
        // The host document's period, bounded when it was parsed (D9: 250 ms
        // to 5 s), so a reporter is never refused for it here.
        let load = crate::load::LoadReporter::new(ingress.clone(), host_id.clone())
            .and_then(|reporter| reporter.with_interval(config.load_report_interval))
            .ok()
            .map(Arc::new);
        Arc::new(Self {
            load,
            journal,
            ingress,
            identities,
            config,
            host_id,
            controller_id,
            runtime_dir,
            log_dir,
            inventory,
            authority: Arc::new(Mutex::new(SessionAuthority::default())),
            saver: None,
            checkpoints: Arc::new(CheckpointVerifier::in_memory()),
            installations,
            residency: None,
            rendezvous: None,
            pre_admitted: Arc::new(Mutex::new(std::collections::HashMap::new())),
        })
    }

    /// SPEC §13.2: run the slow half of a command's admission outside the
    /// journal's locks. A launch runs its whole admission; a Park runs the
    /// installation measurement and capability probe its tier depends on. A
    /// pass is remembered for the cheap recheck under the lock.
    fn pre_admit(&self, command: &MemberCommand) -> Result<(), LaunchVerdict> {
        if self.pre_admitted(command) {
            return Ok(());
        }
        match &command.action {
            MemberAction::LaunchSingle(plan) => self.admit_launch_full(command, plan)?,
            MemberAction::Park { owned_handle } | MemberAction::Restore { owned_handle, .. } => {
                let owner = self
                    .journal
                    .retained_command(owned_handle)
                    .map_err(|_| LaunchVerdict::Refused("unauthorized"))?;
                let MemberAction::LaunchSingle(plan) = &owner.action else {
                    return Err(LaunchVerdict::Refused("unauthorized"));
                };
                let effective = self
                    .resolve_retained(&owner)
                    .map_err(|_| LaunchVerdict::Refused("unauthorized"))?;
                if let Some(reason) = self.park_capability(&effective, &plan.profile_name) {
                    return Err(LaunchVerdict::Refused(reason));
                }
            }
            _ => return Ok(()),
        }
        let mut admitted = self
            .pre_admitted
            .lock()
            .map_err(|_| LaunchVerdict::Uncertain)?;
        admitted.retain(|_, at| at.elapsed() < PRE_ADMISSION_TTL);
        if admitted.len() >= MAX_PRE_ADMISSIONS {
            return Ok(());
        }
        admitted.insert(command.canonical_digest(), std::time::Instant::now());
        Ok(())
    }

    /// Whether `command` passed its slow admission moments ago.
    fn pre_admitted(&self, command: &MemberCommand) -> bool {
        self.pre_admitted.lock().is_ok_and(|admitted| {
            admitted
                .get(&command.canonical_digest())
                .is_some_and(|at| at.elapsed() < PRE_ADMISSION_TTL)
        })
    }
    /// ADR 0007: report each GPU process's resident memory with every
    /// availability refresh (`process_residency`).
    pub fn with_process_residency(
        mut self: Arc<Self>,
        sampler: Arc<crate::process_residency::ResidencySampler>,
    ) -> Arc<Self> {
        Arc::make_mut(&mut self).residency = Some(sampler);
        self
    }
    /// ADR 0014 §7, owner decision Q9: keep the per-host checkpoint stat cache
    /// in this private directory, so a restarted agent verifies an unchanged
    /// checkpoint without hashing it in full again.
    pub fn with_checkpoint_cache(mut self: Arc<Self>, dir: PathBuf) -> Arc<Self> {
        Arc::make_mut(&mut self).checkpoints = Arc::new(CheckpointVerifier::with_cache_dir(dir));
        self
    }
    /// SPEC §8.2 / T21 (found live 2026-09-23): SGLang launches keep their file
    /// rendezvous in `<dir>/<incarnation>` (the role creates `dir` 0700), and
    /// Terminate removes it once the group is proved gone.
    pub fn with_rendezvous_root(mut self: Arc<Self>, dir: PathBuf) -> Arc<Self> {
        Arc::make_mut(&mut self).rendezvous = Some(crate::rendezvous::RendezvousRoot::new(dir));
        self
    }
    /// Attach the SGLang saver observation source residency evidence needs.
    pub fn with_saver_residency(mut self: Arc<Self>, saver: Arc<dyn SaverResidency>) -> Arc<Self> {
        Arc::make_mut(&mut self).saver = Some(saver);
        self
    }
    /// SPEC §§9.1, 13.2: a launch leaving residency loses its readiness
    /// authority, so no replay of an earlier launch or probe reopens its gate.
    fn revoke_ready(&self, owned_handle: &str) -> Result<(), SessionError> {
        self.authority
            .lock()
            .map_err(|_| SessionError)?
            .ready
            .remove(owned_handle);
        Ok(())
    }
    fn publish_ready(
        &self,
        session: u64,
        owned_handle: &str,
        command: &str,
        scope: &IngressScope,
    ) -> Result<(), SessionError> {
        let mut authority = self.authority.lock().map_err(|_| SessionError)?;
        if authority.session != Some(session) {
            return Err(SessionError);
        }
        self.ingress.open(scope).map_err(|_| SessionError)?;
        authority.ready.insert(owned_handle.into(), command.into());
        Ok(())
    }
    fn resolve(&self, command: &MemberCommand) -> Result<EffectiveDeployment, JournalError> {
        self.resolve_as(command, false)
    }
    /// The approved configuration of a launch this host already owns, for
    /// probing, parking or restoring it; never for rendering a launch.
    ///
    /// Owner decision 2026-09-22 (upgrades carry pre-E1 state forward): a launch
    /// journaled before ADR 0014 names no `engine_config`, and the host policy
    /// fingerprint it was approved under covered the profile `launch_settings`
    /// the operator had to remove from the host document, so that fingerprint can
    /// never match again. The journaled command is digest-bound and is never
    /// re-signed. Such a retained launch is resolved against today's approved
    /// document instead, which still has to name the same installation,
    /// checkpoint, device and port range, and whose security policy (deep
    /// parking included) governs what may be done to it. Its KV cache is
    /// declared as the Ready total admission reserved: a bound, never rendered.
    fn resolve_retained(&self, owned: &MemberCommand) -> Result<EffectiveDeployment, JournalError> {
        self.resolve_as(owned, true)
    }
    fn resolve_as(
        &self,
        command: &MemberCommand,
        retained: bool,
    ) -> Result<EffectiveDeployment, JournalError> {
        let MemberAction::LaunchSingle(plan) = &command.action else {
            return Err(JournalError::Unauthorized);
        };
        let config =
            mllm_config::parse_strict(mllm_config::ConfigKind::Deployment, &plan.deployment_config)
                .map_err(|_| JournalError::Unauthorized)?;
        let legacy = retained
            .then(|| mllm_config::effective::legacy_retained_deployment(&config))
            .flatten();
        if command.identity.controller_id != self.controller_id
            || command.identity.member.host_id != self.host_id
            || (legacy.is_none()
                && plan.host_policy_fingerprint
                    != mllm_config::remote_resources::policy_fingerprint(&self.config.document))
        {
            return Err(JournalError::Unauthorized);
        }
        let config = legacy.unwrap_or(config);
        if config["runtime_profile"].as_str() != Some(&plan.profile_name) {
            return Err(JournalError::Unauthorized);
        }
        let host = mllm_config::remote_resources::local_host_document(&self.config.document)
            .map_err(|_| JournalError::Unauthorized)?;
        // ADR 0014 §5, §7: resolve with exactly the checkpoint facts the server
        // resolved this revision with, so both sides derive the same request.
        let facts = mllm_config::effective::CheckpointFacts {
            weights_bytes: plan.checkpoint_weights_bytes,
            ..Default::default()
        };
        let mut effective = mllm_config::effective::resolve_effective_with_checkpoint(&config, &host, facts)
            .map_err(|_| JournalError::Unauthorized)?;
        // Owner decision 2026-09-23: the launch's starting phase is charged the
        // startup peak the server reserved (declared, measured or placeholder),
        // never below the steady request this host resolved itself.
        if let (Some(startup), [cold], [ready]) = (
            plan.startup_bytes,
            effective.resources.cold.allocations.as_mut_slice(),
            effective.resources.ready.allocations.as_slice(),
        ) {
            cold.bytes = startup.max(ready.bytes);
        }
        if !matches!(effective.profile.engine, Engine::Sglang | Engine::Vllm)
            || effective.profile.build_fingerprint != command.identity.profile_fingerprint
            || effective.model.content_fingerprint != plan.checkpoint_fingerprint
            || effective.selected_devices.len() != 1
            || plan.service_port < effective.host.endpoint_port_range.start
            || plan.service_port > effective.host.endpoint_port_range.end
        {
            return Err(JournalError::Unauthorized);
        }
        let root = effective
            .host
            .model_store
            .canonicalize()
            .map_err(|_| JournalError::Unauthorized)?;
        let checkpoint = std::path::Path::new(
            effective
                .model
                .require_resolved_path()
                .map_err(|_| JournalError::Unauthorized)?,
        )
        .canonicalize()
        .map_err(|_| JournalError::Unauthorized)?;
        if !checkpoint.starts_with(root) || !checkpoint.is_dir() {
            return Err(JournalError::Unauthorized);
        }
        if effective.profile.engine == Engine::Vllm {
            self.admit_vllm(&effective, plan)?;
        }
        Ok(effective)
    }
    /// ADR 0014 §7 (WE3): the checkpoint this launch will load must measure to
    /// the digest the server recorded, before anything is journaled or started.
    /// A plan without a recorded digest (journaled before WE3) may be adopted,
    /// probed, parked or terminated, but never launched or woken.
    pub(crate) fn verify_checkpoint(
        &self,
        effective: &EffectiveDeployment,
        plan: &SingleLaunchPlan,
    ) -> Result<(), CheckpointError> {
        if plan.checkpoint_digest.is_empty() {
            return Err(CheckpointError::Mismatch);
        }
        let checkpoint = effective
            .model
            .require_resolved_path()
            .map_err(|_| CheckpointError::InvalidRoot)?;
        let verified = self.checkpoints.verify(
            &effective.host.model_store,
            std::path::Path::new(checkpoint),
            &plan.checkpoint_digest,
        )?;
        // The same manifest yields the same weights; a plan naming others was
        // resolved against something else.
        if plan
            .checkpoint_weights_bytes
            .is_some_and(|bytes| bytes != verified.manifest.weights_bytes)
        {
            return Err(CheckpointError::Mismatch);
        }
        Ok(())
    }

    /// ADR 0014 §7 (WE3): measure one deployment's checkpoint for the server.
    /// Read-only and never journaled: a redelivery measures again, cheaply once
    /// the stat cache holds it. Refusals are evidence, not session failures.
    async fn digest_checkpoint(
        &self,
        command: &MemberCommand,
        plan: &DigestCheckpointPlan,
    ) -> Result<pb::MemberExecutionResult, SessionError> {
        let id = &command.identity;
        if id.controller_id != self.controller_id
            || id.member.host_id != self.host_id
            || id.expected_state != "checkpoint"
            || id.deadline_ms <= mllm_protocol::now_unix_ms()
        {
            return Err(SessionError);
        }
        let refused = |reason: &str| pb::CheckpointDigestEvidence {
            state: "refused".into(),
            reason: reason.into(),
            ..Default::default()
        };
        let evidence = match self.locate(plan) {
            Err(reason) => refused(reason),
            // Owner decision 2026-09-23 (solo first start): a size-only
            // request walks the checkpoint and hashes nothing.
            Ok(location) if plan.size_only => {
                let checkpoints = self.checkpoints.clone();
                let sized = tokio::task::spawn_blocking(move || {
                    checkpoints.size(&location.model_store, &location.checkpoint)
                })
                .await
                .map_err(|_| SessionError)?;
                match sized {
                    Ok(size) => pb::CheckpointDigestEvidence {
                        state: "sized".into(),
                        weights_bytes: size.weights_bytes,
                        file_count: size.file_count,
                        total_bytes: size.total_bytes,
                        ..Default::default()
                    },
                    Err(error) => refused(error.code()),
                }
            }
            Ok(location) => {
                let checkpoints = self.checkpoints.clone();
                let measured = tokio::task::spawn_blocking(move || {
                    checkpoints.measure(&location.model_store, &location.checkpoint)
                })
                .await
                .map_err(|_| SessionError)?;
                match measured {
                    Ok(verified) => {
                        let manifest = verified.manifest;
                        let mismatch = plan
                            .expected_digest
                            .as_ref()
                            .is_some_and(|expected| *expected != manifest.digest);
                        pb::CheckpointDigestEvidence {
                            state: if mismatch { "mismatch" } else { "computed" }.into(),
                            digest: manifest.digest,
                            weights_bytes: manifest.weights_bytes,
                            file_count: manifest.entries.len() as u64,
                            total_bytes: manifest.total_bytes,
                            reason: String::new(),
                            full_rehash: verified.full_rehash,
                        }
                    }
                    Err(error) => refused(error.code()),
                }
            }
        };
        Ok(pb::MemberExecutionResult {
            identity: command.to_wire().identity,
            state: "completed".into(),
            observed_at_unix_ms: mllm_protocol::now_unix_ms(),
            checkpoint: Some(evidence),
            ..Default::default()
        })
    }

    /// Where a digest request's checkpoint lives on this host, from this host's
    /// approved document only.
    fn locate(
        &self,
        plan: &DigestCheckpointPlan,
    ) -> Result<mllm_config::effective::CheckpointLocation, &'static str> {
        if plan.host_policy_fingerprint
            != mllm_config::remote_resources::policy_fingerprint(&self.config.document)
        {
            return Err("unauthorized");
        }
        let deployment =
            mllm_config::parse_strict(mllm_config::ConfigKind::Deployment, &plan.deployment_config)
                .map_err(|_| "unauthorized")?;
        let host = mllm_config::remote_resources::local_host_document(&self.config.document)
            .map_err(|_| "unauthorized")?;
        mllm_config::effective::checkpoint_location(&deployment, &host).map_err(|error| {
            if error.code == mllm_config::ConfigErrorCode::NotMaterializable {
                "not_materializable"
            } else {
                "unauthorized"
            }
        })
    }

    fn scope(&self, command: &MemberCommand) -> Result<IngressScope, JournalError> {
        let MemberAction::LaunchSingle(plan) = &command.action else {
            return Err(JournalError::Unauthorized);
        };
        Ok(IngressScope {
            host_id: command.identity.member.host_id.clone(),
            member_id: command.identity.member.member_id.clone(),
            deployment_id: command.identity.deployment_id.clone(),
            binding_id: plan.binding_id.clone(),
            incarnation: plan.incarnation.clone(),
            generation: command.identity.generation,
            revision: command.identity.revision,
            // ADR 0013 §5: the gate is this instance's own.
            instance_index: command.identity.instance_index,
        })
    }
    fn endpoint(plan: &SingleLaunchPlan) -> String {
        format!("127.0.0.1:{}", plan.service_port)
    }

    /// The frozen SGLang recipe for this host, from local policy only.
    fn sglang_frozen(
        &self,
        effective: &EffectiveDeployment,
        plan: &SingleLaunchPlan,
        served: &str,
    ) -> Result<mllm_domain::launch::NativeLaunch, SessionError> {
        frozen_from_effective(
            effective,
            &plan.binding_id,
            &plan.incarnation,
            &Self::endpoint(plan),
            served.into(),
            format!("host-inference-{}", plan.binding_id),
            format!("host-admin-{}", plan.binding_id),
        )
        .map_err(|_| SessionError)
    }

    /// Resolve the engine-specific launch before anything durable happens.
    fn prepare(
        &self,
        effective: &EffectiveDeployment,
        plan: &SingleLaunchPlan,
        served: &str,
    ) -> Result<PreparedLaunch, SessionError> {
        match effective.profile.engine {
            Engine::Sglang => Ok(PreparedLaunch::Sglang(Box::new(
                self.sglang_frozen(effective, plan, served)?,
            ))),
            Engine::Vllm => Ok(PreparedLaunch::Vllm(Box::new(
                self.vllm_plan(effective, plan).map_err(|_| SessionError)?,
            ))),
        }
    }

    /// The launching adapter for a prepared launch and its journal-gated tools.
    fn launch_adapter(
        &self,
        prepared: PreparedLaunch,
        effective: &EffectiveDeployment,
        plan: &SingleLaunchPlan,
        keys: &NativeCredentials,
        served: &str,
        tools: Arc<dyn OwnedProcessLaunch>,
    ) -> Result<Box<dyn EngineAdapter>, SessionError> {
        Ok(match prepared {
            PreparedLaunch::Sglang(frozen) => {
                let mut adapter = SglangAdapter::from_frozen(&frozen, None)
                    .map_err(|_| SessionError)?;
                // SPEC §9.2: a memory-saver launch enrolls its saver
                // observation in this host's private directory.
                if let Some(dir) = self.saver.as_ref().and_then(|s| s.observation_dir()) {
                    adapter = adapter.with_observation_dir(dir.to_path_buf());
                }
                // SPEC §8.2 / T21: the host names the launch's rendezvous
                // directory so it can remove it after the group is gone.
                if let Some(dir) = self
                    .rendezvous
                    .as_ref()
                    .and_then(|root| root.launch_dir(&plan.incarnation))
                {
                    adapter = adapter.with_rendezvous_dir(dir);
                }
                // ADR 0014 §8, SPEC §8.2: the host's approvals, which the entry
                // applies to the destinations the extras resolve to.
                let security = &effective.profile.security;
                adapter = adapter.with_extra_approvals(
                    mllm_config::engine_policy::extra_approvals_document(
                        &security.approved_options,
                        &security.approved_paths,
                        security.trust_remote_code,
                    ),
                );
                Box::new(adapter
                    .with_credentials(hex::encode(keys.inference), hex::encode(keys.admin))
                    .with_launch(*frozen)
                    .with_tools(tools)
                    .with_session(plan.coordinator_session_id.clone())
                    .with_wrapper(self.runtime_dir.join("sglang_entry.py"))
                    .with_log(
                        self.log_dir
                            .join(format!("{}.log", plan.incarnation))
                            .to_string_lossy(),
                    ))
            }
            PreparedLaunch::Vllm(launch) => Box::new(
                self.vllm_adapter(effective, plan, keys, served)?
                    .with_launch(*launch)
                    .with_tools(tools),
            ),
        })
    }

    /// An adapter that can only question the retained engine: it carries the
    /// launch's native inference credential and no launch or process tools.
    fn probe_adapter(
        &self,
        effective: &EffectiveDeployment,
        plan: &SingleLaunchPlan,
        keys: &NativeCredentials,
        served: &str,
    ) -> Result<Box<dyn NativeEngine>, SessionError> {
        Ok(match effective.profile.engine {
            Engine::Sglang => Box::new(
                SglangAdapter::from_frozen(&self.sglang_frozen(effective, plan, served)?, None)
                    .map_err(|_| SessionError)?
                    .with_credentials(hex::encode(keys.inference), hex::encode(keys.admin)),
            ),
            Engine::Vllm => Box::new(self.vllm_adapter(effective, plan, keys, served)?),
        })
    }

    /// SPEC §6.1: one accepted launch, for any supported engine. Readiness is
    /// published only on the adapter's model probe; a failure after the durable
    /// attempt leaves retained ownership for the controller, never a release.
    async fn launch_native(
        &self,
        session: u64,
        ticket: ExecutionTicket,
        command: &MemberCommand,
        plan: &SingleLaunchPlan,
    ) -> Result<(), LaunchError> {
        let effective = self.resolve(command).map_err(|_| SessionError)?;
        let scope = self.scope(command).map_err(|_| SessionError)?;
        let keys = self
            .identities
            .load(&scope, command.identity.payload_digest)
            .map_err(|_| SessionError)?;
        let served = effective.routes.first().cloned().ok_or(SessionError)?;
        let prepared = self.prepare(&effective, plan, &served)?;
        let tools = self
            .journal
            .launch_tools(ticket, mllm_protocol::now_unix_ms(), Arc::new(self.clone()))
            .map_err(|_| SessionError)?;
        let adapter = self.launch_adapter(prepared, &effective, plan, &keys, &served, tools)?;
        self.ingress
            .register(
                scope.clone(),
                Self::endpoint(plan).parse().map_err(|_| SessionError)?,
                served,
                keys.gate,
                keys.inference,
            )
            .map_err(|_| SessionError)?;
        // D9: load samples name the launch the controller holds as owned handle.
        self.ingress
            .bind_handle(&scope, &command.identity.command_id)
            .map_err(|_| SessionError)?;
        let observation = adapter
            .execute_persisted(&initialize_command(command, plan, &effective))
            .await
            .map_err(|error| LaunchError {
                engine: match error {
                    mllm_adapters::traits::RuntimeError::LaunchFailed(text) => Some(text),
                    _ => None,
                },
            })?;
        let id = &command.identity;
        self.journal
            .record_launch_ready(session, &id.command_id, &observation)
            .map_err(|_| SessionError)?;
        Ok(self.publish_ready(session, &id.command_id, &id.command_id, &scope)?)
    }

    /// Register a retained launch's exact scope with its protected credentials.
    /// A restarted host has no in-memory gate; the entry is created closed.
    fn register_retained(&self, owned: &MemberCommand) -> Result<IngressScope, SessionError> {
        let MemberAction::LaunchSingle(plan) = &owned.action else {
            return Err(SessionError);
        };
        let scope = self.scope(owned).map_err(|_| SessionError)?;
        let keys = self
            .identities
            .load(&scope, owned.identity.payload_digest)
            .map_err(|_| SessionError)?;
        let deployment =
            mllm_config::parse_strict(mllm_config::ConfigKind::Deployment, &plan.deployment_config)
                .map_err(|_| SessionError)?;
        let served = deployment["routes"]
            .as_array()
            .and_then(|r| r.first())
            .and_then(|r| r.as_str())
            .ok_or(SessionError)?;
        self.ingress
            .register(
                scope.clone(),
                Self::endpoint(plan).parse().map_err(|_| SessionError)?,
                served.into(),
                keys.gate,
                keys.inference,
            )
            .map_err(|_| SessionError)?;
        self.ingress
            .bind_handle(&scope, &owned.identity.command_id)
            .map_err(|_| SessionError)?;
        Ok(scope)
    }

    /// SPEC §§6.1, 13.2 (G2): re-prove a retained launch after a session loss.
    ///
    /// The journaled group must still be exactly the one whose readiness the
    /// launch recorded (PID, boot and start identity), and the engine must answer
    /// a fresh `/v1/models` read and chat probe with the launch's own native key.
    /// Only then is the result persisted and the gate reopened for this session.
    /// Any failure leaves the gate closed and claims nothing; ownership stays.
    async fn reprobe(
        &self,
        session: u64,
        command: &MemberCommand,
        owned_handle: &str,
    ) -> Result<(), SessionError> {
        let owned = self
            .journal
            .retained_command(owned_handle)
            .map_err(|_| SessionError)?;
        let MemberAction::LaunchSingle(plan) = &owned.action else {
            return Err(SessionError);
        };
        let journal = self.journal.clone();
        let probe = command.clone();
        let expected = tokio::task::spawn_blocking(move || journal.probe_target(&probe))
            .await
            .map_err(|_| SessionError)?
            .map_err(|_| SessionError)?;
        let effective = self.resolve_retained(&owned).map_err(|_| SessionError)?;
        let scope = self.scope(&owned).map_err(|_| SessionError)?;
        let keys = self
            .identities
            .load(&scope, owned.identity.payload_digest)
            .map_err(|_| SessionError)?;
        let served = effective.routes.first().cloned().ok_or(SessionError)?;
        let adapter = self.probe_adapter(&effective, plan, &keys, &served)?;
        let member = MemberRef {
            deployment_id: owned.identity.deployment_id.clone(),
            member_id: plan.binding_id.clone(),
        };
        fresh_probe(
            adapter.as_ref(),
            &member,
            &served,
            command.identity.deadline_ms - PROBE_MARGIN_MS,
        )
        .await?;
        let journal = self.journal.clone();
        let id = command.identity.command_id.clone();
        tokio::task::spawn_blocking(move || {
            journal.record_probe_ready(session, &id, mllm_protocol::now_unix_ms(), &expected)
        })
        .await
        .map_err(|_| SessionError)?
        .map_err(|_| SessionError)?;
        let scope = self.register_retained(&owned)?;
        self.publish_ready(session, owned_handle, &command.identity.command_id, &scope)
    }

    /// SPEC §13 / §13.3: the terminal refusal of a command, and for a launch
    /// the deletion of the credentials its provisioning stored: a launch refused
    /// before any effect keeps no keys behind (T37).
    async fn refused_launch(
        &self,
        command: &MemberCommand,
    ) -> Result<pb::MemberExecutionResult, SessionError> {
        let result = self.refused(command).await?;
        if matches!(command.action, MemberAction::LaunchSingle(_)) {
            if let Ok(scope) = self.scope(command) {
                let _ = self
                    .identities
                    .retire(&scope, command.identity.payload_digest);
            }
        }
        Ok(result)
    }

    async fn effect(
        self,
        session: u64,
        command: MemberCommand,
    ) -> Result<pb::MemberExecutionResult, SessionError> {
        if let MemberAction::DigestCheckpoint(plan) = &command.action {
            return self.digest_checkpoint(&command, plan).await;
        }
        // SPEC §13.2: fence and duplicate checks first (cheap), then the slow
        // half of admission outside the journal's locks. A replay or a fenced
        // command never measures anything; a refusal is the terminal answer.
        let journal = self.journal.clone();
        let checked = command.clone();
        let fresh = tokio::task::spawn_blocking(move || journal.precheck(session, &checked))
            .await
            .map_err(|_| SessionError)?;
        if matches!(
            command.action,
            MemberAction::LaunchSingle(_) | MemberAction::Park { .. } | MemberAction::Restore { .. }
        ) && matches!(fresh, Ok(true))
        {
            let host = self.clone();
            let admitted = command.clone();
            match tokio::task::spawn_blocking(move || host.pre_admit(&admitted))
                .await
                .map_err(|_| SessionError)?
            {
                Ok(()) => {}
                Err(LaunchVerdict::Refused(_)) => return self.refused_launch(&command).await,
                Err(LaunchVerdict::Uncertain) => return Err(SessionError),
            }
        }
        let policy = Arc::new(self.clone());
        let journal = self.journal.clone();
        let accepted_command = command.clone();
        // SPEC §§6.4, 13.2: the engine's own account of a launch that exited
        // before readiness; only its bounded summary leaves the host.
        let mut launch_failed: Option<String> = None;
        let acceptance = match tokio::task::spawn_blocking(move || {
            journal.accept(
                session,
                &accepted_command,
                mllm_protocol::now_unix_ms(),
                policy.as_ref(),
            )
        })
        .await
        .map_err(|_| SessionError)?
        {
            Ok(acceptance) => acceptance,
            // SPEC §13: local policy refused before anything was journaled.
            // That is terminal evidence, not a reason to end the session.
            Err(JournalError::Unauthorized) => return self.refused_launch(&command).await,
            Err(_) => return Err(SessionError),
        };
        match acceptance {
            Acceptance::Replay(_) => {}
            Acceptance::Fresh(ticket) => match &command.action {
                MemberAction::LaunchSingle(plan) => {
                    // SPEC §§6.1, 13.2: a launch that fails (an engine that
                    // exits before readiness, an unanswered model probe) is
                    // evidence for the controller, not a lost session. Ending
                    // the session here made the controller wait out the whole
                    // Initialize deadline for an engine that had died at once,
                    // and tore down every other effect on the host (found live,
                    // M16 on host-b). The journal's result below reports the
                    // recorded processes and their presence with the model
                    // unusable and the claim retained, and the controller settles
                    // the launch with a Terminate on gone evidence. A journal
                    // that cannot answer still ends the session there.
                    if let Err(error) = self.launch_native(session, ticket, &command, plan).await {
                        launch_failed = error.engine;
                    }
                }
                MemberAction::Terminate { owned_handle } => {
                    // A restarted host has no in-memory gate. Re-register the
                    // exact retained scope closed using its protected credentials.
                    // A handle this host never launched has no scope to close.
                    // Admission has already closed at the controller; close the
                    // host gate before termination, retaining any active stream
                    // count. A scope that cannot be registered has no entry here
                    // to forward through, so there is nothing of it to close.
                    if let Some(scope) = self
                        .journal
                        .retained_command(owned_handle)
                        .ok()
                        .and_then(|owned| self.register_retained(&owned).ok())
                    {
                        self.ingress.close(&scope).map_err(|_| SessionError)?;
                    }
                    let journal = self.journal.clone();
                    let policy = Arc::new(self.clone());
                    tokio::task::spawn_blocking(move || {
                        journal.execute(ticket, mllm_protocol::now_unix_ms(), policy.as_ref())
                    })
                    .await
                    .map_err(|_| SessionError)?
                    .map_err(|_| SessionError)?;
                }
                MemberAction::Inspect => {
                    let journal = self.journal.clone();
                    let policy = Arc::new(self.clone());
                    tokio::task::spawn_blocking(move || {
                        journal.execute(ticket, mllm_protocol::now_unix_ms(), policy.as_ref())
                    })
                    .await
                    .map_err(|_| SessionError)?
                    .map_err(|_| SessionError)?;
                }
                MemberAction::Probe { owned_handle } => {
                    let journal = self.journal.clone();
                    let policy = Arc::new(self.clone());
                    tokio::task::spawn_blocking(move || {
                        journal.execute(ticket, mllm_protocol::now_unix_ms(), policy.as_ref())
                    })
                    .await
                    .map_err(|_| SessionError)?
                    .map_err(|_| SessionError)?;
                    // An engine that does not answer is ordinary evidence: the
                    // result below reports it unusable with ownership retained.
                    let _ = self.reprobe(session, &command, owned_handle).await;
                }
                // SPEC §§9.1, 10 (W4): the outcome, including a refusal or an
                // uncertain engine effect, is persisted evidence in the result.
                MemberAction::Park { .. } | MemberAction::Restore { .. } => {
                    self.residency(session, ticket, &command).await?;
                }
                _ => return Err(SessionError),
            },
        }
        let now = mllm_protocol::now_unix_ms();
        let mut result = self
            .journal
            .execution_result(&command.identity.command_id, now)
            .map_err(|_| SessionError)?;
        if result.model_usable && now.saturating_sub(result.observed_at_unix_ms) > 2_000 {
            // A replay after the native probe expired is an owned, unresolved
            // launch. Keep reporting current physical ownership, never Ready.
            result.model_usable = false;
            result.observed_at_unix_ms = now;
        }
        if matches!(command.action, MemberAction::LaunchSingle(_))
            && result.state == "launched"
            && !result.model_usable
            && !result.processes.is_empty()
            && result.processes.iter().all(|p| p.presence == "gone")
        {
            // SPEC §§6.4, 13.2: the engine exited before readiness. Say why in
            // one bounded line: the exit this host reaped and the option names
            // the engine refused, never a value or other engine output.
            result.launch_failure = launch_failure(&result, launch_failed.as_deref());
        }
        if let MemberAction::Terminate { owned_handle } = &command.action {
            // SPEC §§6, 13.3 (U5 recovery live run): the terminated launch's
            // ingress entry outlived it, so the next launch of the same member
            // (a failed launch keeps its generation) was refused as stale and
            // never started. Only this host's own gone evidence retires it.
            let gone = result.state == "completed"
                && !result.claim_retained
                && result.processes.iter().all(|p| p.presence == "gone");
            if gone {
                if let Ok(scope) = self
                    .journal
                    .retained_command(owned_handle)
                    .and_then(|owned| self.scope(&owned))
                {
                    self.ingress.retire(&scope).map_err(|_| SessionError)?;
                    // SPEC §13.3 / T37: a gone launch's credentials go with it.
                    if let Ok(owned) = self.journal.retained_command(owned_handle) {
                        let _ = self
                            .identities
                            .retire(&scope, owned.identity.payload_digest);
                    }
                    // SPEC §9.2: a gone launch's saver enrollment is stale.
                    if let Some(saver) = &self.saver {
                        saver.retire(&scope.binding_id);
                    }
                    // SPEC §8.2 / T21 (found live 2026-09-23): a signalled
                    // stop never ran the entry's exit handler, so the gone
                    // launch's rendezvous directory is removed here.
                    if let Some(rendezvous) = &self.rendezvous {
                        rendezvous.retire(&scope.incarnation);
                    }
                }
            }
        }
        let authority = self.authority.lock().map_err(|_| SessionError)?;
        if authority.session != Some(session)
            || !authority
                .ready
                .values()
                .any(|ready| *ready == command.identity.command_id)
        {
            // A previous session's model probe cannot reopen forwarding.
            result.model_usable = false;
            result.observed_at_unix_ms = now;
        }
        if result.model_usable {
            // Exact, still-fresh replay may recover a lost acknowledgement. An
            // in-memory gate lost on host restart is rebuilt only for this scope.
            let owned = match &command.action {
                MemberAction::Probe { owned_handle } | MemberAction::Restore { owned_handle, .. } => self
                    .journal
                    .retained_command(owned_handle)
                    .map_err(|_| SessionError)?,
                _ => command.clone(),
            };
            let scope = self.register_retained(&owned)?;
            self.ingress.open(&scope).map_err(|_| SessionError)?;
        }
        Ok(result)
    }
}

/// Why a native launch did not reach readiness. `engine` is the adapter's own
/// launch failure text, for this host only: it is reduced to a bounded summary
/// ([`launch_failure`]) before anything leaves the host.
struct LaunchError {
    engine: Option<String>,
}
impl From<SessionError> for LaunchError {
    fn from(_: SessionError) -> Self {
        Self { engine: None }
    }
}

/// SPEC §§6.4, 13.2: the bounded summary of a launch whose recorded processes
/// all exited before readiness, with the exit status this host's launcher
/// reaped (the api process first) when it knows it.
fn launch_failure(result: &pb::MemberExecutionResult, engine: Option<&str>) -> String {
    use mllm_adapters::launch_failure::{summary, EngineExit};
    let mut processes: Vec<&pb::OwnedProcessObservation> = result.processes.iter().collect();
    processes.sort_by_key(|p| p.role != "api");
    let exit = processes.iter().find_map(|p| {
        let identity = mllm_domain::completion::ProcessIdentity {
            role: p.role.clone(),
            pid: p.pid,
            boot_id: p.boot_id.clone(),
            start_ticks: p.start_ticks,
        };
        match mllm_launchers::reaped::status_of(&identity)? {
            mllm_launchers::reaped::ReapedStatus::Code(code) => Some(EngineExit::Code(code)),
            mllm_launchers::reaped::ReapedStatus::Signal(signal) => Some(EngineExit::Signal(signal)),
        }
    });
    summary(engine.unwrap_or(""), exit)
}

/// SPEC §6.1: a fresh native model probe, the same evidence a launch's readiness
/// rests on. The served name must be listed, and the model must answer a short
/// completion through the same authenticated path inference uses. HTTP liveness
/// alone is never readiness.
async fn fresh_probe(
    engine: &dyn NativeEngine,
    member: &MemberRef,
    served: &str,
    stop_at_ms: i64,
) -> Result<(), SessionError> {
    let remaining = || {
        Duration::from_millis(
            u64::try_from(stop_at_ms - mllm_protocol::now_unix_ms()).unwrap_or(0),
        )
    };
    let listed = tokio::time::timeout(
        remaining().min(PROBE_MODELS_TIMEOUT),
        engine.check_readiness(member),
    )
    .await
    .map_err(|_| SessionError)?
    .map_err(|_| SessionError)?;
    if listed != Readiness::Ready {
        return Err(SessionError);
    }
    let body = serde_json::json!({
        "model": served,
        "messages": [{"role": "user", "content": "Say ready."}],
        "max_tokens": 8,
        "temperature": 0,
    });
    let answer = tokio::time::timeout(
        remaining().min(PROBE_CHAT_TIMEOUT),
        engine.forward_chat(&body),
    )
    .await
    .map_err(|_| SessionError)?
    .map_err(|_| SessionError)?;
    if answer["choices"][0]["message"]["content"]
        .as_str()
        .is_none_or(str::is_empty)
    {
        return Err(SessionError);
    }
    Ok(())
}

/// The adapter's Initialize step for exactly the accepted command's fence.
fn initialize_command(
    command: &MemberCommand,
    plan: &SingleLaunchPlan,
    effective: &EffectiveDeployment,
) -> RuntimeCommand {
    let id = &command.identity;
    RuntimeCommand {
        action: RuntimeAction::Initialize,
        context: StepExecutionContext {
            token: TransitionToken {
                deployment_id: id.deployment_id.clone(),
                revision: id.revision,
                generation: id.generation,
                operation_id: id.operation_id.clone(),
                step_id: id.step_id.clone(),
            },
            binding_id: plan.binding_id.clone(),
            incarnation: plan.incarnation.clone(),
            issued_at_ms: plan.issued_at_ms,
            deadline_ms: id.deadline_ms,
            identities: ExecutionIdentities::OwnedLaunch,
            completion_target: None,
            grant_id: Some(plan.grant_id.clone()),
            launch_settings: Some(effective.engine_config.clone()),
        },
    }
}

impl LocalExecutionPolicy for NativeHostExecution {
    fn authorize(&self, command: &MemberCommand) -> Result<(), JournalError> {
        if command.identity.controller_id != self.controller_id
            || command.identity.member.host_id != self.host_id
        {
            return Err(JournalError::Unauthorized);
        }
        match &command.action {
            MemberAction::LaunchSingle(plan) if command.identity.expected_state == "reserved" => {
                // ADR 0014 §7, SPEC §13: checkpoint, pool shape and memory, the
                // same admission provisioning runs and reports when refused.
                Ok(self.admit_launch(command, plan)?)
            }
            MemberAction::Terminate { .. } if command.identity.expected_state == "retained" => {
                Ok(())
            }
            // SPEC §§6.1, 13.2: a probe questions an engine this host already
            // owns; the journal binds it to that exact retained launch.
            MemberAction::Probe { .. } if command.identity.expected_state == "ready" => Ok(()),
            MemberAction::Inspect => Ok(()),
            // SPEC §§9.1, 10: Park only a ready launch, Restore only a parked
            // one. The journal binds the named launch and asks
            // `authorize_residency` about its tier before anything is recorded.
            MemberAction::Park { .. } if command.identity.expected_state == "ready" => Ok(()),
            MemberAction::Restore { .. } if command.identity.expected_state == "parked" => Ok(()),
            _ => Err(JournalError::Unauthorized),
        }
    }
    fn authorize_residency(
        &self,
        command: &MemberCommand,
        owner: &MemberCommand,
    ) -> Result<(), JournalError> {
        let MemberAction::LaunchSingle(plan) = &owner.action else {
            return Err(JournalError::Unauthorized);
        };
        if command.identity.controller_id != self.controller_id
            || command.identity.member.host_id != self.host_id
        {
            return Err(JournalError::Unauthorized);
        }
        // The owner's approved configuration, resolved from this host's own
        // policy: never the controller's description of it.
        let effective = self.resolve_retained(owner)?;
        // Owner decision (ADR 0012): park only at the declared tier. A
        // `restart_only` deployment never parks. `host_backed` is refused on
        // unified pools at resolution (ADR 0010 decision 5) and has no remote
        // park path; only `deep` parks here.
        if effective.residency != mllm_config::effective::Residency::Deep {
            return Err(JournalError::Unauthorized);
        }
        // ADR 0008: a Park needs the internals deep parking drives; a build the
        // launch-time probe found without them is refused, launch unchanged.
        // SPEC §13.2: that measurement ran outside the lock when the command
        // was pre-admitted; only a command that was not runs it here.
        if !self.pre_admitted(command)
            && self.park_capability(&effective, &plan.profile_name).is_some()
        {
            return Err(JournalError::Unauthorized);
        }
        match effective.profile.engine {
            // SPEC §9.1 / T21: the host's deep-park policy must admit the
            // sleep and collective controls, and the launch must have been
            // started with sleep mode behind the guard.
            Engine::Vllm => {
                let launch = self.vllm_plan(&effective, plan)?;
                if mllm_adapters::vllm::park_policy(&effective)
                    != mllm_adapters::ParkPolicy::Enabled
                    || launch.sleep_flags.is_empty()
                {
                    return Err(JournalError::Unauthorized);
                }
                Ok(())
            }
            Engine::Sglang => Ok(()),
        }
    }
    fn render_launch(&self, _: &MemberCommand) -> Result<ApprovedLaunch, JournalError> {
        // Rendering belongs to the existing guarded native adapter only.
        Err(JournalError::Unauthorized)
    }
    fn admit_beside(
        &self,
        command: &MemberCommand,
        claimed: &[crate::journal::ClaimedLaunch],
    ) -> Result<(), JournalError> {
        // SPEC §§3.1, 7.3 (per-launch claims): this host keeps one claim per
        // launch and admits a new one beside the others from its own policy.
        match &command.action {
            MemberAction::LaunchSingle(plan) => {
                Ok(self.admit_beside_claims(command, plan, claimed)?)
            }
            _ if claimed.is_empty() => Ok(()),
            _ => Err(JournalError::Uncertain),
        }
    }
    fn admit_wake(
        &self,
        _command: &MemberCommand,
        owner: &MemberCommand,
        claimed: &[crate::journal::ClaimedLaunch],
    ) -> Result<(), JournalError> {
        // SPEC §§3.1, 7.3, 9.1: the woken launch is charged its ready
        // footprint beside every other claim, from this host's own policy.
        Ok(self.admit_wake_beside_claims(owner, claimed)?)
    }
}
impl SessionExecution for NativeHostExecution {
    fn execute(&self, session: u64, command: MemberCommand) -> ExecutionFuture {
        Box::pin(self.clone().effect(session, command))
    }
    fn provision(
        &self,
        command: MemberCommand,
        gate_key: [u8; 32],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Provisioned, SessionError>> + Send>>
    {
        let host = self.clone();
        Box::pin(async move {
            command.verify_digest().map_err(|_| SessionError)?;
            if command.identity.deadline_ms <= mllm_protocol::now_unix_ms()
                || command.identity.controller_id != host.controller_id
                || command.identity.member.host_id != host.host_id
                || command.identity.expected_state != "reserved"
            {
                return Err(SessionError);
            }
            let MemberAction::LaunchSingle(plan) = command.action.clone() else {
                return Err(SessionError);
            };
            // ADR 0014 §7: admission measures the checkpoint, which on a first
            // placement hashes it in full; keep that off the runtime.
            let authorizer = host.clone();
            let authorized = command.clone();
            // SPEC §§3.1, 7.3: alone, then beside every launch still claimed.
            match tokio::task::spawn_blocking(move || authorizer.admit_launch_here(&authorized, &plan))
                .await
                .map_err(|_| SessionError)?
            {
                Ok(()) => {}
                // SPEC §13: a policy refusal is the answer, not a lost session;
                // no key is stored and nothing else happens.
                Err(LaunchVerdict::Refused(reason)) => return Ok(Provisioned::Refused(reason)),
                Err(LaunchVerdict::Uncertain) => return Err(SessionError),
            }
            // SPEC §13.2: the same command's delivery rechecks this cheaply
            // under the journal's lock instead of measuring again.
            let _ = host.pre_admit(&command);
            let scope = host.scope(&command).map_err(|_| SessionError)?;
            host.identities
                .provision(&scope, command.identity.payload_digest, gate_key)
                .map_err(|_| SessionError)?;
            Ok(Provisioned::Stored)
        })
    }
    fn connected(&self, session: u64) -> Result<(), SessionError> {
        let mut authority = self.authority.lock().map_err(|_| SessionError)?;
        self.ingress.close_all().map_err(|_| SessionError)?;
        authority.session = Some(session);
        authority.ready.clear();
        Ok(())
    }
    fn disconnected(&self, session: u64) {
        // The same mutex guards every gate-open check. A completing Initialize
        // can never reopen forwarding after this disconnect fence has committed.
        let mut authority = self.authority.lock().unwrap_or_else(|e| e.into_inner());
        if authority.session == Some(session) {
            authority.session = None;
            authority.ready.clear();
            let _ = self.ingress.close_all();
        }
    }
    fn load_interval(&self) -> std::time::Duration {
        self.load
            .as_ref()
            .map_or(crate::load::DEFAULT_LOAD_INTERVAL, |load| load.interval())
    }
    fn load_reports(&self) -> Option<crate::session::LoadFuture> {
        let load = self.load.clone()?;
        Some(Box::pin(async move { load.reports().await }))
    }
    fn member_exits(&self) -> Option<crate::session::ExitFuture> {
        let host = self.clone();
        Some(Box::pin(async move {
            // `/proc` reads and journal writes are blocking work.
            tokio::task::spawn_blocking(move || {
                let exited = crate::exits::scan(
                    &host.journal,
                    &host.host_id,
                    mllm_protocol::now_unix_ms(),
                );
                for launch in &exited {
                    // SPEC §§6.1, 13.2 (W13): an engine with an exited member
                    // is not the group readiness proved. Its readiness authority
                    // ends and its gate closes now, before the controller acts;
                    // a later probe can only find it unusable. The claim stays.
                    let _ = host.revoke_ready(&launch.exit.owned_handle);
                    if let Ok(scope) = host.scope(&launch.command) {
                        let _ = host.ingress.close(&scope);
                    }
                }
                exited.into_iter().map(|launch| launch.exit.to_wire()).collect()
            })
            .await
            .unwrap_or_default()
        }))
    }
    fn inventory(&self) -> Option<pb::ReportInventory> {
        let mut inventory = self.inventory.clone();
        // ADR 0008: status carries installation drift and missing capabilities.
        self.installations.overlay(&mut inventory.profiles);
        // Startup only publishes measured domains. Refresh exactly the same
        // single unified pool, preserving its approved name and policy binding.
        if inventory.domains.len() != 1 {
            return None;
        }
        let memory = crate::memory::read_host_memory().ok()?.memory;
        // ADR 0007: availability first, then the processes still alive, so a
        // process that grew in between is under-credited, never over-credited.
        let residents = self
            .residency
            .as_ref()
            .map(|sampler| sampler.current())
            .unwrap_or_default();
        let domain = &mut inventory.domains[0];
        domain.observed_bytes = memory.available_bytes;
        domain.available_bytes = memory.available_bytes;
        domain.capacity_bytes = memory.capacity_bytes;
        domain.observed_at_unix = memory.sampled_at_ms / 1000;
        domain.observed_at_unix_ms = memory.sampled_at_ms;
        domain.residents = residents
            .into_iter()
            .map(|p| pb::ProcessResidency {
                pid: p.pid,
                boot_id: p.boot_id,
                start_ticks: p.start_ticks,
                resident_bytes: p.bytes,
            })
            .collect();
        Some(inventory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn directory() -> tempfile::TempDir {
        let directory = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }
    // T37: the host document's `load_report_interval` sets the period of the
    // agent's engine load reports; omitted, it is the 1 s default.
    #[test]
    fn load_reports_follow_the_host_document_interval() {
        let journal_dir = directory();
        let identity_dir = directory();
        let execution = |document: String| {
            NativeHostExecution::new(
                HostJournal::open(journal_dir.path(), "controller", "host").unwrap(),
                Ingress::new().unwrap(),
                IngressIdentities::new(
                    crate::identity_storage::IdentityDirectory::open(identity_dir.path()).unwrap(),
                ),
                HostConfig::parse(&document).unwrap(),
                "host".into(),
                "controller".into(),
                journal_dir.path().join("runtime"),
                journal_dir.path().join("logs"),
                Default::default(),
            )
        };
        let template = HostConfig::template(journal_dir.path());
        assert_eq!(
            execution(template.clone()).load_interval(),
            crate::load::DEFAULT_LOAD_INTERVAL
        );
        let mut document: serde_json::Value = serde_json::from_str(&template).unwrap();
        document["load_report_interval"] = "500ms".into();
        assert_eq!(
            execution(document.to_string()).load_interval(),
            std::time::Duration::from_millis(500)
        );
    }

    // T13 / T38: a completed old-session probe cannot race disconnect to reopen
    // forwarding. Neither closure nor reconnect releases native ownership.
    #[tokio::test]
    async fn disconnected_session_fences_late_ready_and_closes_http_gate() {
        let journal_dir = directory();
        let identity_dir = directory();
        let journal = HostJournal::open(journal_dir.path(), "controller", "host").unwrap();
        let ingress = Ingress::new().unwrap();
        let executor = NativeHostExecution::new(
            journal,
            ingress.clone(),
            IngressIdentities::new(
                crate::identity_storage::IdentityDirectory::open(identity_dir.path()).unwrap(),
            ),
            HostConfig::parse(&HostConfig::template(journal_dir.path())).unwrap(),
            "host".into(),
            "controller".into(),
            journal_dir.path().join("runtime"),
            journal_dir.path().join("logs"),
            Default::default(),
        );
        let scope = IngressScope {
            host_id: "host".into(),
            deployment_id: "deployment".into(),
            binding_id: "binding".into(),
            incarnation: "incarnation".into(),
            member_id: "head".into(),
            generation: 1,
            revision: 1,
            instance_index: 0,
        };
        ingress
            .register(
                scope.clone(),
                "127.0.0.1:1".parse().unwrap(),
                "model".into(),
                [1; 32],
                [2; 32],
            )
            .unwrap();
        executor.connected(1).unwrap();
        executor.publish_ready(1, "first", "first", &scope).unwrap();
        // Race the two actual authority operations. Both lock orderings must
        // end closed, including completion that starts before disconnect.
        let barrier = std::sync::Barrier::new(3);
        std::thread::scope(|threads| {
            threads.spawn(|| {
                barrier.wait();
                let _ = executor.publish_ready(1, "first", "racing-first", &scope);
            });
            threads.spawn(|| {
                barrier.wait();
                executor.disconnected(1);
            });
            barrier.wait();
        });
        assert!(executor.authority.lock().unwrap().session.is_none());
        assert!(executor.publish_ready(1, "first", "late-first", &scope).is_err());
        executor.connected(2).unwrap();
        assert!(executor.publish_ready(1, "first", "late-first", &scope).is_err());
        // A stale disconnect cannot revoke a newer session, but the new session
        // still has no model probe and therefore no forwarding authority.
        executor.disconnected(1);
        assert!(executor.authority.lock().unwrap().ready.is_empty());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, ingress.router()).await.unwrap();
        });
        let response = reqwest::Client::new()
            .post(format!("http://{address}/v1/chat/completions"))
            .bearer_auth(hex::encode([1; 32]))
            .json(&serde_json::json!({"model":"model","messages":[]}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
        // The same mutex guards both orders of completion versus disconnect.
        executor.publish_ready(2, "first", "second", &scope).unwrap();
        executor.disconnected(2);
        assert!(executor.publish_ready(2, "first", "late-second", &scope).is_err());
        server.abort();
    }

    /// Owner decision 2026-09-22: a launch journaled before ADR 0014 stays this
    /// host's own after the operator moved `launch_settings` out of the host
    /// document. It resolves for probing and parking against today's document;
    /// it never authorizes a launch, and a command already in the E1 shape gets
    /// no such exception.
    // T33 T14
    #[test]
    fn a_pre_e1_retained_launch_resolves_for_probe_and_park_but_never_to_launch() {
        use mllm_domain::group::{CommandIdentity, MemberKey};
        let root = directory();
        let identity_dir = directory();
        std::fs::create_dir_all(root.path().join("models/toy")).unwrap();
        std::fs::create_dir_all(root.path().join("runtime")).unwrap();
        std::fs::write(root.path().join("runtime/mllm_vllm_guard.py"), "").unwrap();
        std::fs::write(
            root.path().join("runtime").join(mllm_adapters::vllm::VLLM_ENTRY),
            "",
        )
        .unwrap();
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../mllm-config/tests/fixtures/f2-deployment.json"
        ))
        .unwrap();
        let mut document: serde_json::Value =
            serde_json::from_str(&HostConfig::template(root.path())).unwrap();
        for field in [
            "hardware_fingerprint",
            "environment_fingerprint",
            "resource_policy",
            "runtime_profiles",
        ] {
            document[field] = fixture["host"][field].clone();
        }
        let config = HostConfig::parse(&document.to_string()).unwrap();
        let executor = NativeHostExecution::new(
            HostJournal::open(root.path(), "controller", "host").unwrap(),
            Ingress::new().unwrap(),
            IngressIdentities::new(
                crate::identity_storage::IdentityDirectory::open(identity_dir.path()).unwrap(),
            ),
            config,
            "host".into(),
            "controller".into(),
            root.path().join("runtime"),
            root.path().join("logs"),
            Default::default(),
        );
        let mut deployment = fixture["deployment"].clone();
        deployment["model"]["path"] = serde_json::json!("toy");
        let current = deployment.clone();
        deployment.as_object_mut().unwrap().remove("engine_config");
        let command = |deployment: &serde_json::Value| MemberCommand {
            identity: CommandIdentity {
                controller_id: "controller".into(),
                member: MemberKey {
                    host_id: "host".into(),
                    member_id: "head".into(),
                },
                deployment_id: "deployment".into(),
                operation_id: "operation".into(),
                command_id: "launch".into(),
                step_id: "launch".into(),
                generation: 1,
                revision: 1,
                deadline_ms: 1000,
                payload_digest: [0; 32],
                expected_state: "reserved".into(),
                profile_fingerprint: "vllm-build-1".into(),
                instance_index: 0,
            },
            action: MemberAction::LaunchSingle(SingleLaunchPlan {
                deployment_config: deployment.to_string(),
                profile_name: "local".into(),
                checkpoint_fingerprint: "sha256:model".into(),
                // What the document hashed to before `launch_settings` left it.
                host_policy_fingerprint: "b".repeat(64),
                binding_id: "01K00000000000000000000001".into(),
                incarnation: "01K00000000000000000000002".into(),
                grant_id: "01K00000000000000000000003".into(),
                service_port: 8100,
                issued_at_ms: 1,
                coordinator_session_id: "01K00000000000000000000004".into(),
                checkpoint_digest: String::new(),
                checkpoint_weights_bytes: None,
                startup_bytes: None,
            }),
        };
        let legacy = command(&deployment);
        assert!(executor.resolve(&legacy).is_err());
        let effective = executor.resolve_retained(&legacy).unwrap();
        assert_eq!(effective.profile.engine, Engine::Vllm);
        assert_eq!(effective.engine_config.memory().kv_cache_bytes, 8 << 30);
        assert!(executor.resolve_retained(&command(&current)).is_err());
    }

    /// A vLLM fixture host whose model store holds `models/toy` with files.
    fn checkpoint_fixture(
        root: &std::path::Path,
        identity_dir: &std::path::Path,
    ) -> (Arc<NativeHostExecution>, serde_json::Value, String) {
        std::fs::create_dir_all(root.join("models/toy")).unwrap();
        std::fs::write(root.join("models/toy/config.json"), "{}").unwrap();
        std::fs::write(root.join("models/toy/model.safetensors"), "weights").unwrap();
        std::fs::create_dir_all(root.join("runtime")).unwrap();
        std::fs::write(root.join("runtime/mllm_vllm_guard.py"), "").unwrap();
        std::fs::write(
            root.join("runtime").join(mllm_adapters::vllm::VLLM_ENTRY),
            "",
        )
        .unwrap();
        // ADR 0008: a sleep-mode vLLM launch also imports the capability probes.
        std::fs::write(root.join("runtime/engine_capabilities.py"), "").unwrap();
        // SPEC §9.1 / T21: a prepared host's runtime directory is private to
        // the agent user whatever the umask that wrote it.
        for path in [
            root.join("runtime"),
            root.join("runtime/mllm_vllm_guard.py"),
            root.join("runtime").join(mllm_adapters::vllm::VLLM_ENTRY),
            root.join("runtime/engine_capabilities.py"),
        ] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../mllm-config/tests/fixtures/f2-deployment.json"
        ))
        .unwrap();
        let mut document: serde_json::Value =
            serde_json::from_str(&HostConfig::template(root)).unwrap();
        for field in [
            "hardware_fingerprint",
            "environment_fingerprint",
            "resource_policy",
            "runtime_profiles",
        ] {
            document[field] = fixture["host"][field].clone();
        }
        let config = HostConfig::parse(&document.to_string()).unwrap();
        let policy = mllm_config::remote_resources::policy_fingerprint(&config.document);
        let executor = NativeHostExecution::new(
            HostJournal::open(root, "controller", "host").unwrap(),
            Ingress::new().unwrap(),
            IngressIdentities::new(
                crate::identity_storage::IdentityDirectory::open(identity_dir).unwrap(),
            ),
            config,
            "host".into(),
            "controller".into(),
            root.join("runtime"),
            root.join("logs"),
            Default::default(),
        );
        let mut deployment = fixture["deployment"].clone();
        deployment["model"]["path"] = serde_json::json!("toy");
        (executor, deployment, policy)
    }

    fn checkpoint_identity(id: &str, expected_state: &str) -> mllm_domain::group::CommandIdentity {
        mllm_domain::group::CommandIdentity {
            controller_id: "controller".into(),
            member: mllm_domain::group::MemberKey {
                host_id: "host".into(),
                member_id: "head".into(),
            },
            deployment_id: "deployment".into(),
            operation_id: "operation".into(),
            command_id: id.into(),
            step_id: id.into(),
            generation: 1,
            revision: 1,
            deadline_ms: mllm_protocol::now_unix_ms() + 60_000,
            payload_digest: [0; 32],
            expected_state: expected_state.into(),
            profile_fingerprint: "vllm-build-1".into(),
            instance_index: 0,
        }
    }

    fn launch_with(
        deployment: &serde_json::Value,
        policy: &str,
        digest: &str,
    ) -> (MemberCommand, SingleLaunchPlan) {
        let plan = SingleLaunchPlan {
            deployment_config: deployment.to_string(),
            profile_name: "local".into(),
            checkpoint_fingerprint: "sha256:model".into(),
            host_policy_fingerprint: policy.into(),
            binding_id: "01K00000000000000000000001".into(),
            incarnation: "01K00000000000000000000002".into(),
            grant_id: "01K00000000000000000000003".into(),
            service_port: 8100,
            issued_at_ms: 1,
            coordinator_session_id: "01K00000000000000000000004".into(),
            checkpoint_digest: digest.into(),
            checkpoint_weights_bytes: None,
            startup_bytes: None,
        };
        let command = MemberCommand {
            identity: checkpoint_identity("launch", "reserved"),
            action: MemberAction::LaunchSingle(plan.clone()),
        };
        (command, plan)
    }

    /// ADR 0014 §7 (WE3): a launch and a wake verify the recorded digest on
    /// this host; a changed checkpoint, a stale digest, or a plan journaled
    /// before WE3 (no digest) woken without one is refused before any effect.
    // T34 T14 T15
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn launch_and_wake_refuse_a_checkpoint_that_is_not_the_recorded_one() {
        let root = directory();
        let identity_dir = directory();
        let (executor, deployment, policy) = checkpoint_fixture(root.path(), identity_dir.path());
        let recorded = crate::checkpoint::CheckpointVerifier::in_memory()
            .measure(&root.path().join("models"), &root.path().join("models/toy"))
            .unwrap()
            .manifest
            .digest;
        let (launch, plan) = launch_with(&deployment, &policy, &recorded);
        let effective = executor.resolve(&launch).unwrap();
        executor.verify_checkpoint(&effective, &plan).unwrap();
        assert!(executor.checkpoint_unchanged(&launch, &plan, "").await);
        // Owner decision 5: a wake may name the same digest, never another.
        assert!(executor.checkpoint_unchanged(&launch, &plan, &recorded).await);
        let other_digest = format!("sha256:{}", "0".repeat(64));
        assert!(!executor.checkpoint_unchanged(&launch, &plan, &other_digest).await);
        // Weights the server did not resolve with are refused as well.
        let weighed = SingleLaunchPlan {
            checkpoint_weights_bytes: Some(1),
            startup_bytes: None,
            ..plan.clone()
        };
        let (other, _) = launch_with(&deployment, &policy, &recorded);
        let other = MemberCommand {
            action: MemberAction::LaunchSingle(weighed.clone()),
            ..other
        };
        let effective = executor.resolve(&other).unwrap();
        assert_eq!(
            executor.verify_checkpoint(&effective, &weighed).unwrap_err(),
            CheckpointError::Mismatch
        );
        // Pre-WE3 plan: adoptable, never launched, and woken only against the
        // digest the server measured and recorded for it (owner decision 5).
        let (legacy, legacy_plan) = launch_with(&deployment, &policy, "");
        assert!(executor.authorize(&legacy).is_err());
        assert!(!executor.checkpoint_unchanged(&legacy, &legacy_plan, "").await);
        assert!(executor.checkpoint_unchanged(&legacy, &legacy_plan, &recorded).await);
        assert!(!executor.checkpoint_unchanged(&legacy, &legacy_plan, &other_digest).await);
        // The checkpoint changes under the recorded digest.
        std::fs::write(root.path().join("models/toy/model.safetensors"), "swapped").unwrap();
        let effective = executor.resolve(&launch).unwrap();
        assert_eq!(
            executor.verify_checkpoint(&effective, &plan).unwrap_err(),
            CheckpointError::Mismatch
        );
        assert!(executor.authorize(&launch).is_err());
        assert!(!executor.checkpoint_unchanged(&launch, &plan, "").await);
        assert!(!executor.checkpoint_unchanged(&legacy, &legacy_plan, &recorded).await);
    }

    /// ADR 0014 §7 (WE3), owner decision 5: a pre-WE3 launch has no recorded
    /// digest, so its park journals this host's own measurement, and the wake
    /// verifies against that, not against whatever digest the Restore names. A
    /// checkpoint swapped while parked cannot be woken by a controller that
    /// measured the new files.
    // T34 T15
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pre_we3_wake_verifies_the_digest_journaled_at_park() {
        let root = directory();
        let identity_dir = directory();
        let (executor, deployment, policy) = checkpoint_fixture(root.path(), identity_dir.path());
        let measure = || {
            crate::checkpoint::CheckpointVerifier::in_memory()
                .measure(&root.path().join("models"), &root.path().join("models/toy"))
                .unwrap()
                .manifest
                .digest
        };
        let parked_digest = measure();
        let (legacy, legacy_plan) = launch_with(&deployment, &policy, "");
        executor.journal_park_digest(&legacy).await;
        assert_eq!(
            executor.journal.park_digest("launch").unwrap().as_deref(),
            Some(parked_digest.as_str())
        );
        assert!(executor.checkpoint_unchanged(&legacy, &legacy_plan, "").await);
        assert!(executor.checkpoint_unchanged(&legacy, &legacy_plan, &parked_digest).await);
        std::fs::write(root.path().join("models/toy/model.safetensors"), "swapped").unwrap();
        let swapped = measure();
        assert!(!executor.checkpoint_unchanged(&legacy, &legacy_plan, &swapped).await);
        assert!(!executor.checkpoint_unchanged(&legacy, &legacy_plan, "").await);
    }

    /// SPEC §13.2: the slow half of admission (checkpoint hashing, the
    /// installation measurement, the capability probe) runs before the journal
    /// is locked. Under the lock a command admitted that way is rechecked
    /// cheaply, keyed by its own digest; any other command is still admitted in
    /// full, so nothing is ever journaled without its admission.
    // T34 T13
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_admission_runs_before_the_journal_lock() {
        let root = directory();
        let identity_dir = directory();
        let (executor, deployment, policy) = checkpoint_fixture(root.path(), identity_dir.path());
        let recorded = crate::checkpoint::CheckpointVerifier::in_memory()
            .measure(&root.path().join("models"), &root.path().join("models/toy"))
            .unwrap()
            .manifest
            .digest;
        let (launch, _) = launch_with(&deployment, &policy, &recorded);
        executor.pre_admit(&launch).unwrap();
        // The slow checks passed moments ago; the locked recheck does not
        // hash the checkpoint again.
        std::fs::write(root.path().join("models/toy/model.safetensors"), "swapped").unwrap();
        executor.authorize(&launch).unwrap();
        // A command that was never pre-admitted gets the whole admission.
        let (other, _) = launch_with(&deployment, &policy, &recorded);
        let mut other = other;
        other.identity.command_id = "other".into();
        other.identity.step_id = "other".into();
        assert!(executor.authorize(&other).is_err());
        // And pre-admission itself refuses what the full admission refuses.
        assert!(matches!(
            executor.pre_admit(&other),
            Err(LaunchVerdict::Refused("checkpoint_mismatch"))
        ));
    }

    /// ADR 0014 §7 (WE3): DigestCheckpoint measures on this host from its own
    /// document, reports only bounded evidence, and refuses without failing
    /// the session.
    // T34 T37
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn digest_checkpoint_reports_bounded_evidence_from_local_policy() {
        let root = directory();
        let identity_dir = directory();
        let (executor, deployment, policy) = checkpoint_fixture(root.path(), identity_dir.path());
        let digest = |deployment: &serde_json::Value, policy: &str, expected: Option<&str>| {
            let mut command = MemberCommand {
                identity: checkpoint_identity("digest", "checkpoint"),
                action: MemberAction::DigestCheckpoint(DigestCheckpointPlan {
                    size_only: false,
                    deployment_config: deployment.to_string(),
                    host_policy_fingerprint: policy.into(),
                    expected_digest: expected.map(str::to_owned),
                }),
            };
            command.identity.payload_digest = command.canonical_digest();
            command
        };
        let run = |command: MemberCommand| {
            let executor = executor.clone();
            async move {
                let result = executor.execute(1, command.clone()).await.unwrap();
                mllm_protocol::execution::validate_result(&command, &result).unwrap();
                result.checkpoint.unwrap()
            }
        };
        let computed = run(digest(&deployment, &policy, None)).await;
        assert_eq!(computed.state, "computed");
        assert_eq!(computed.weights_bytes, 7);
        assert_eq!(computed.file_count, 2);
        assert!(computed.full_rehash);
        // Owner decision 2026-09-23 (solo first start): a size-only request
        // sizes the weight files with the same confined walk and hashes nothing.
        let mut sizing = digest(&deployment, &policy, None);
        if let MemberAction::DigestCheckpoint(plan) = &mut sizing.action {
            plan.size_only = true;
        }
        sizing.identity.payload_digest = sizing.canonical_digest();
        let sized = run(sizing).await;
        assert_eq!(sized.state, "sized");
        assert_eq!((sized.weights_bytes, sized.file_count), (7, 2));
        assert!(sized.digest.is_empty() && !sized.full_rehash);
        let again = run(digest(&deployment, &policy, Some(&computed.digest))).await;
        assert_eq!((again.state.as_str(), again.full_rehash), ("computed", false));
        let other = format!("sha256:{}", "0".repeat(64));
        assert_eq!(run(digest(&deployment, &policy, Some(&other))).await.state, "mismatch");
        let refused = run(digest(&deployment, &"c".repeat(64), None)).await;
        assert_eq!((refused.state.as_str(), refused.reason.as_str()), ("refused", "unauthorized"));
        let mut escaping = deployment.clone();
        escaping["model"]["path"] = serde_json::json!("/etc");
        let refused = run(digest(&escaping, &policy, None)).await;
        assert_eq!((refused.state.as_str(), refused.reason.as_str()), ("refused", "invalid_root"));
        // Another controller's command is not evidence at all.
        let mut foreign = digest(&deployment, &policy, None);
        foreign.identity.controller_id = "other".into();
        foreign.identity.payload_digest = foreign.canonical_digest();
        assert!(executor.execute(1, foreign).await.is_err());
    }

    /// SPEC §13 (WE3 limit 1): a launch refused because its checkpoint no
    /// longer measures to the recorded digest is a terminal, typed answer. The
    /// provision stores no key and reports `checkpoint_mismatch`; a delivery of
    /// the launch itself completes refused with no claim and no process, and
    /// nothing is journaled. The session is never ended for it.
    // T14 T20 T34
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_checkpoint_mismatch_is_a_terminal_refusal_not_a_lost_session() {
        let root = directory();
        let identity_dir = directory();
        let (executor, deployment, policy) = checkpoint_fixture(root.path(), identity_dir.path());
        let recorded = crate::checkpoint::CheckpointVerifier::in_memory()
            .measure(&root.path().join("models"), &root.path().join("models/toy"))
            .unwrap()
            .manifest
            .digest;
        std::fs::write(root.path().join("models/toy/model.safetensors"), "swapped").unwrap();
        let (mut launch, _) = launch_with(&deployment, &policy, &recorded);
        launch.identity.payload_digest = launch.canonical_digest();
        assert_eq!(
            executor.provision(launch.clone(), [7; 32]).await.unwrap(),
            Provisioned::Refused("checkpoint_mismatch")
        );
        let scope = executor.scope(&launch).unwrap();
        assert!(
            executor.identities.load(&scope, launch.identity.payload_digest).is_err(),
            "a refused provision stores no key"
        );
        let session = executor.journal.connect().unwrap();
        executor.connected(session).unwrap();
        for _ in 0..2 {
            let result = executor.execute(session, launch.clone()).await.unwrap();
            mllm_protocol::execution::validate_result(&launch, &result).unwrap();
            assert_eq!(result.refused, "checkpoint_mismatch");
            assert_eq!(result.state, "completed");
            assert!(!result.claim_retained && !result.model_usable);
            assert!(result.processes.is_empty());
        }
        assert!(executor.journal.history(0, 100).unwrap().is_empty());
    }

    /// An SGLang host document from the same lab fixture, with the deployment
    /// declaring `residency`.
    fn sglang_fixture(
        root: &std::path::Path,
        identity_dir: &std::path::Path,
        residency: &str,
    ) -> (Arc<NativeHostExecution>, serde_json::Value, String) {
        std::fs::create_dir_all(root.join("models/toy")).unwrap();
        std::fs::write(root.join("models/toy/config.json"), "{}").unwrap();
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../mllm-config/tests/fixtures/f2-deployment.json"
        ))
        .unwrap();
        let mut document: serde_json::Value =
            serde_json::from_str(&HostConfig::template(root)).unwrap();
        for field in [
            "hardware_fingerprint",
            "environment_fingerprint",
            "resource_policy",
            "runtime_profiles",
        ] {
            document[field] = fixture["host"][field].clone();
        }
        let profile = &mut document["runtime_profiles"]["local"];
        profile["engine"] = "sglang".into();
        profile["args"] = serde_json::json!([]);
        profile["security"]["admin_credential_ref"] = "secret://admin-key".into();
        let config = HostConfig::parse(&document.to_string()).unwrap();
        let policy = mllm_config::remote_resources::policy_fingerprint(&config.document);
        let executor = NativeHostExecution::new(
            HostJournal::open(root, "controller", "host").unwrap(),
            Ingress::new().unwrap(),
            IngressIdentities::new(
                crate::identity_storage::IdentityDirectory::open(identity_dir).unwrap(),
            ),
            config,
            "host".into(),
            "controller".into(),
            root.join("runtime"),
            root.join("logs"),
            Default::default(),
        );
        let mut deployment = fixture["deployment"].clone();
        deployment["model"]["path"] = serde_json::json!("toy");
        deployment["residency"] = residency.into();
        (executor, deployment, policy)
    }

    /// Journals an owner launch without running it, as a host that accepted it.
    struct Admit;
    impl LocalExecutionPolicy for Admit {
        fn authorize(&self, _: &MemberCommand) -> Result<(), JournalError> {
            Ok(())
        }
        fn render_launch(&self, _: &MemberCommand) -> Result<ApprovedLaunch, JournalError> {
            Err(JournalError::Unauthorized)
        }
    }

    /// SPEC §§6.2, 9.2: restart-only is first-class for SGLang too. The host
    /// resolves an SGLang `restart_only` launch to settings without the memory
    /// saver or weight backup, and a Park of it (W4) is refused by its declared
    /// tier before anything is journaled: a terminal `unchanged` answer with
    /// the closed reason `residency_tier`, never a park call.
    // T21 T22
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_restart_only_sglang_launch_has_no_saver_and_its_park_is_refused_unchanged() {
        let root = directory();
        let identity_dir = directory();
        let (executor, deployment, policy) =
            sglang_fixture(root.path(), identity_dir.path(), "restart_only");
        let (mut launch, _) = launch_with(&deployment, &policy, "");
        launch.identity.payload_digest = launch.canonical_digest();
        let effective = executor.resolve(&launch).unwrap();
        assert_eq!(effective.profile.engine, Engine::Sglang);
        let mllm_domain::launch::LaunchSettings::Sglang(settings) = &effective.engine_config
        else {
            panic!("an SGLang profile resolves SGLang settings");
        };
        assert!(!settings.memory_saver && !settings.cpu_weight_backup);
        assert_eq!(settings.weight_restore, "disk_reload");

        let session = executor.journal.connect().unwrap();
        executor.connected(session).unwrap();
        executor
            .journal
            .accept(session, &launch, mllm_protocol::now_unix_ms(), &Admit)
            .unwrap();
        let mut park = MemberCommand {
            identity: checkpoint_identity("park", "ready"),
            action: MemberAction::Park {
                owned_handle: "launch".into(),
            },
        };
        park.identity.payload_digest = park.canonical_digest();
        let refused = executor.execute(session, park.clone()).await.unwrap();
        mllm_protocol::execution::validate_result(&park, &refused).unwrap();
        assert_eq!(refused.state, "completed");
        assert_eq!(refused.refused, "residency_tier");
        assert_eq!(refused.residency.as_ref().unwrap().state, "unchanged");
        assert!(!refused.model_usable);
        assert_eq!(executor.journal.history(0, 100).unwrap().len(), 1, "the park is not journaled");
        assert_eq!(executor.journal.residency_of("launch").unwrap(), None);

        // The same launch declared `deep` passes the tier gate: the tier, not
        // the engine, is what refused it.
        let root = directory();
        let identity_dir = directory();
        let (deep, deployment, policy) = sglang_fixture(root.path(), identity_dir.path(), "deep");
        let (mut owner, _) = launch_with(&deployment, &policy, "");
        owner.identity.payload_digest = owner.canonical_digest();
        deep.authorize_residency(&park, &owner).unwrap();
    }

    /// A prepared host's runtime directory for SGLang: the agent user's
    /// modules, not writable by group or other.
    fn sglang_runtime(root: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        let runtime = root.join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
        for module in crate::runtime_integrity::SGLANG_RUNTIME_FILES {
            let path = runtime.join(module);
            std::fs::write(&path, "# module\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
    }

    /// ADR 0008 (owner decision 2026-09-23): a launch that finds its
    /// installation no longer measuring to the fingerprint registered at agent
    /// start flags the drift in the host's published status; the default
    /// `warn` policy launches on (here it reaches the next gate, the fixture's
    /// unrecorded checkpoint), and `installation_drift: refuse` refuses it
    /// before any effect with the closed reason `installation_drift`.
    // T21 T22
    #[test]
    fn installation_drift_is_flagged_and_refused_only_by_host_policy() {
        for (policy_value, expected) in [
            (None, "checkpoint_mismatch"),
            (Some("warn"), "checkpoint_mismatch"),
            (Some("refuse"), "installation_drift"),
        ] {
            let root = directory();
            let identity_dir = directory();
            let (mut executor, deployment, _) =
                sglang_fixture(root.path(), identity_dir.path(), "restart_only");
            sglang_runtime(root.path());
            let host = Arc::make_mut(&mut executor);
            let mut document = host.config.document.clone();
            if let Some(value) = policy_value {
                document["runtime_profiles"]["local"]["security"]["installation_drift"] =
                    value.into();
            }
            host.config = HostConfig::parse(&document.to_string()).unwrap();
            let policy = mllm_config::remote_resources::policy_fingerprint(&host.config.document);
            // Registered at agent start with a digest this installation no
            // longer measures to (the fixture's has no package tree at all).
            host.inventory.profiles = vec![pb::RuntimeProfileStatus {
                name: "local".into(),
                installation_version: "0.5.20".into(),
                installation_digest: format!("sha256:{}", "a".repeat(64)),
                installation_state: "measured".into(),
                ..Default::default()
            }];
            host.installations = Arc::new(crate::installation::InstallationRegistry::from_inventory(
                &host.inventory,
            ));
            let (mut launch, _) = launch_with(&deployment, &policy, "");
            launch.identity.payload_digest = launch.canonical_digest();
            let MemberAction::LaunchSingle(plan) = launch.action.clone() else {
                panic!("a launch");
            };
            assert_eq!(
                executor.admit_launch(&launch, &plan),
                Err(LaunchVerdict::Refused(expected)),
                "{policy_value:?}"
            );
            let mut profiles = executor.inventory.profiles.clone();
            executor.installations.overlay(&mut profiles);
            assert_eq!(profiles[0].installation_state, "drifted");
            assert_eq!(
                profiles[0].installation_observed_digest,
                crate::installation::UNMEASURED
            );
        }
    }

    /// A launch-time probe report with `deep_park` missing, as a build without
    /// the memory saver hooks answers (`runtime/engine_capabilities.py`).
    fn without_saver_hooks() -> crate::installation::CapabilityReport {
        crate::installation::CapabilityReport {
            missing: [
                ("core", vec![]),
                ("deep_park", vec!["torch_memory_saver".to_owned()]),
                ("metrics", vec![]),
                ("observation", vec!["TorchMemorySaver".to_owned()]),
            ]
            .into_iter()
            .map(|(name, labels)| (name.to_owned(), labels))
            .collect(),
        }
    }

    /// ADR 0008 (owner decision 2026-09-23): a build the launch-time probe
    /// found without the memory saver hooks refuses a `deep` launch with the
    /// typed reason `capability_missing:deep_park` before any effect, and a
    /// Park of a `deep` launch with the same reason, `unchanged`. The same
    /// build admits a `restart_only` launch past the capability gate (it then
    /// stops at the fixture's unrecorded checkpoint, the next gate).
    // T21 T22
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_build_without_saver_hooks_refuses_deep_and_park_but_not_restart_only() {
        let root = directory();
        let identity_dir = directory();
        let (executor, deployment, policy) = sglang_fixture(root.path(), identity_dir.path(), "deep");
        sglang_runtime(root.path());
        let (mut owner, _) = launch_with(&deployment, &policy, "");
        owner.identity.payload_digest = owner.canonical_digest();
        let MemberAction::LaunchSingle(plan) = owner.action.clone() else {
            panic!("a launch");
        };
        // The fixture's installation has no package tree: unmeasured, keyed "".
        executor
            .installations
            .record_capabilities(&plan.profile_name, "", without_saver_hooks());
        assert_eq!(
            executor.admit_launch(&owner, &plan),
            Err(LaunchVerdict::Refused("capability_missing:deep_park"))
        );

        let session = executor.journal.connect().unwrap();
        executor.connected(session).unwrap();
        executor
            .journal
            .accept(session, &owner, mllm_protocol::now_unix_ms(), &Admit)
            .unwrap();
        let mut park = MemberCommand {
            identity: checkpoint_identity("park", "ready"),
            action: MemberAction::Park {
                owned_handle: "launch".into(),
            },
        };
        park.identity.payload_digest = park.canonical_digest();
        let refused = executor.execute(session, park.clone()).await.unwrap();
        mllm_protocol::execution::validate_result(&park, &refused).unwrap();
        assert_eq!(refused.state, "completed");
        assert_eq!(refused.refused, "capability_missing:deep_park");
        assert_eq!(refused.residency.as_ref().unwrap().state, "unchanged");
        assert_eq!(executor.journal.history(0, 100).unwrap().len(), 1, "the park is not journaled");

        let root = directory();
        let identity_dir = directory();
        let (restart, deployment, policy) =
            sglang_fixture(root.path(), identity_dir.path(), "restart_only");
        sglang_runtime(root.path());
        let (mut launch, _) = launch_with(&deployment, &policy, "");
        launch.identity.payload_digest = launch.canonical_digest();
        let MemberAction::LaunchSingle(plan) = launch.action.clone() else {
            panic!("a launch");
        };
        restart
            .installations
            .record_capabilities(&plan.profile_name, "", without_saver_hooks());
        assert_eq!(
            restart.admit_launch(&launch, &plan),
            Err(LaunchVerdict::Refused("checkpoint_mismatch"))
        );
    }

    /// SPEC §§6.2, 9.1 / ADR 0010: a residency tier the engine cannot honor
    /// fails closed. SGLang 0.5.20 cannot reload modelopt (NVFP4) weights from
    /// disk, which every deep wake does, so a `deep` SGLang launch declaring a
    /// modelopt quantization is refused `capability_missing:deep_park` before
    /// any effect even with no probe report, and a Park of one already
    /// journaled is refused the same way, `unchanged`. `restart_only` on the
    /// same checkpoint and a `deep` vLLM launch with the same quantization pass
    /// the gate (they stop at the fixture's unrecorded checkpoint).
    // T22 T21
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sglang_modelopt_quantization_refuses_deep_but_not_restart_only_or_vllm() {
        for quantization in ["modelopt_fp4", "modelopt", "MODELOPT_FP8", "nvfp4"] {
            let root = directory();
            let identity_dir = directory();
            let (executor, mut deployment, policy) =
                sglang_fixture(root.path(), identity_dir.path(), "deep");
            sglang_runtime(root.path());
            deployment["engine_config"]["quantization"] = quantization.into();
            let (mut owner, _) = launch_with(&deployment, &policy, "");
            owner.identity.payload_digest = owner.canonical_digest();
            let MemberAction::LaunchSingle(plan) = owner.action.clone() else {
                panic!("a launch");
            };
            assert_eq!(
                executor.admit_launch(&owner, &plan),
                Err(LaunchVerdict::Refused("capability_missing:deep_park")),
                "{quantization}"
            );

            let session = executor.journal.connect().unwrap();
            executor.connected(session).unwrap();
            executor
                .journal
                .accept(session, &owner, mllm_protocol::now_unix_ms(), &Admit)
                .unwrap();
            let mut park = MemberCommand {
                identity: checkpoint_identity("park", "ready"),
                action: MemberAction::Park {
                    owned_handle: "launch".into(),
                },
            };
            park.identity.payload_digest = park.canonical_digest();
            let refused = executor.execute(session, park.clone()).await.unwrap();
            mllm_protocol::execution::validate_result(&park, &refused).unwrap();
            assert_eq!(refused.refused, "capability_missing:deep_park", "{quantization}");
            assert_eq!(refused.residency.as_ref().unwrap().state, "unchanged");
        }

        let root = directory();
        let identity_dir = directory();
        let (restart, mut deployment, policy) =
            sglang_fixture(root.path(), identity_dir.path(), "restart_only");
        sglang_runtime(root.path());
        deployment["engine_config"]["quantization"] = "modelopt_fp4".into();
        let (mut launch, _) = launch_with(&deployment, &policy, "");
        launch.identity.payload_digest = launch.canonical_digest();
        let MemberAction::LaunchSingle(plan) = launch.action.clone() else {
            panic!("a launch");
        };
        assert_eq!(
            restart.admit_launch(&launch, &plan),
            Err(LaunchVerdict::Refused("checkpoint_mismatch"))
        );

        // vLLM behavior is unchanged: the rule is SGLang's.
        let root = directory();
        let identity_dir = directory();
        let (vllm, mut deployment, policy) = checkpoint_fixture(root.path(), identity_dir.path());
        deployment["engine_config"]["quantization"] = "modelopt_fp4".into();
        let (mut launch, _) = launch_with(&deployment, &policy, "");
        launch.identity.payload_digest = launch.canonical_digest();
        let MemberAction::LaunchSingle(plan) = launch.action.clone() else {
            panic!("a launch");
        };
        assert_eq!(
            vllm.admit_launch(&launch, &plan),
            Err(LaunchVerdict::Refused("checkpoint_mismatch"))
        );
    }

    /// SPEC §§6.4, 13.2: a launch whose recorded processes all exited before
    /// readiness is reported with one bounded line that names the option the
    /// engine refused and never its value or any other engine output; one the
    /// engine said nothing about still says what happened. The line is one the
    /// wire validation accepts.
    // T20 T29
    #[test]
    fn an_exited_launch_reports_a_bounded_summary() {
        let gone = |role: &str, pid| pb::OwnedProcessObservation {
            role: role.into(),
            pid,
            boot_id: "boot-that-never-reaped".into(),
            start_ticks: 7,
            presence: "gone".into(),
        };
        let result = pb::MemberExecutionResult {
            state: "launched".into(),
            processes: vec![gone("worker-0", 11), gone("api", 10)],
            ..Default::default()
        };
        let engine = "the engine exited before readiness; log tail:\nerror: argument --moe-backend: invalid choice: 'bogus-m53' (choose from 'a')";
        let text = launch_failure(&result, Some(engine));
        assert_eq!(
            text,
            "the engine exited before readiness; it rejected argument --moe-backend"
        );
        assert!(mllm_protocol::execution::is_launch_failure_text(&text));
        assert_eq!(
            launch_failure(&result, None),
            "the engine exited before readiness"
        );
    }
}

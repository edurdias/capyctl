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
use capyctl_adapters::{
    sglang::{frozen_from_effective, SglangAdapter},
    traits::{
        ChatForward, EngineAdapter, MemberRef, OwnedProcessLaunch, Readiness, RuntimeAction,
        RuntimeCommand,
    },
};
use capyctl_config::{
    effective::EffectiveDeployment, engine_policy::Engine, remote_roles::HostConfig,
};
use capyctl_domain::completion::{ExecutionIdentities, StepExecutionContext, TransitionToken};
use capyctl_domain::group::{GroupEngine, GroupPlan, MemberPlan};
use capyctl_protocol::{
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
// ADR 0023: TensorFold's plan, adapter and idle check before Terminate.
mod tensorfold;
// SPEC §§9.1, 10 (W4): remote Park and Restore of a retained launch.
mod residency;
pub use residency::{SaverMapped, SaverResidency, SaverScope, SaverUnavailable};
// SPEC §9.2: the enrolled-scheduler saver source and the embedded observer.
mod saver_source;
pub use saver_source::{EnrolledSaver, LaunchSglangObserver};
// SPEC §13 (WE3 limit 1): pre-effect policy refusals are terminal results.
mod refusal;
// Discrete GPU design §4: the launch check the switch planner agrees with.
use refusal::charges_device;
pub use refusal::{admit_memory_with, LaunchVerdict};
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
    host_id: String,
    controller_id: String,
    runtime_dir: PathBuf,
    log_dir: PathBuf,
    /// ADR 0018 §3: the accepted, pending and previous runtime profile sets,
    /// each with its inventory and the installations registered for it
    /// (ADR 0008: installation fingerprints, drift, launch-time probes).
    /// `config` above keeps the role-local settings only.
    profiles: Arc<crate::profiles::HostProfiles>,
    authority: Arc<Mutex<SessionAuthority>>,
    /// SGLang saver-map observation for residency evidence. Without one the
    /// host cannot prove an SGLang release or restoration and refuses both.
    saver: Option<Arc<dyn SaverResidency>>,
    /// SPEC §10, D9: loopback engine load scrapes of Ready scopes.
    load: Option<Arc<crate::load::LoadReporter>>,
    /// ADR 0014 §7 (WE3): this host's checkpoint digests and stat cache.
    checkpoints: Arc<CheckpointVerifier>,
    /// ADR 0007: per-process resident memory reported beside availability,
    /// so the server can credit resident engines instead of counting them
    /// twice (found live 2026-09-23, matrix M33). `None` reports none.
    residency: Option<Arc<crate::process_residency::ResidencySampler>>,
    /// SPEC §8.2 / T21: the private root SGLang launches keep their file
    /// rendezvous in, removed per launch on gone evidence. `None`: the entry
    /// uses its own temporary directory, removed only at interpreter exit.
    rendezvous: Option<crate::rendezvous::RendezvousRoot>,
    /// ADR 0023 §3: the private root TensorFold launches build extensions in.
    engine_cache: Option<crate::engine_cache::EngineCacheRoot>,
    /// SPEC §13.2: commands whose slow admission (checkpoint hashing, the
    /// installation measurement, the capability probe) passed just now,
    /// outside the journal's locks, keyed by their canonical digest. Under the
    /// lock such a command is rechecked cheaply; anything else is admitted in
    /// full there.
    pre_admitted: Arc<Mutex<std::collections::HashMap<[u8; 32], std::time::Instant>>>,
    /// ADR 0008: declared remote model sources, materialized into this host's
    /// model store under its own `model_sources` policy. `None` when the host
    /// document states no usable store, which refuses every request.
    sources: Option<Arc<crate::sources::SourceStore>>,
    /// SPEC §7.2 / ADR 0019: where a device domain's memory is read from. It
    /// runs only when the accepted policy declares a device domain, off the
    /// session loop, so a slow collector never delays a heartbeat.
    gpu: Option<Arc<crate::gpu_memory::CachedGpuSampler>>,
    /// SPEC §13.2, §7.2 (review decision, discrete GPU design §4): the GPU
    /// sample each launch took just before the journal was locked, keyed by the
    /// command's canonical digest. A collector run is bounded at seconds, so it
    /// never runs under the journal's locks; the locked recheck reads this.
    lock_samples: Arc<Mutex<std::collections::HashMap<[u8; 32], LockSample>>>,
    /// ADR 0028 §7, §10 (R11): where group checks read this host's
    /// interfaces and probe its ports.
    probes: crate::host_checks::HostProbes,
}

/// A GPU sample taken for one command before the journal was locked, and when.
type LockSample = (std::time::Instant, Option<crate::gpu_memory::GpuSample>);

/// How long a sample taken before the lock stands for the locked recheck. A
/// launch's last check runs when the engine is spawned, after its durable
/// attempt began; an older sample leaves the device unobserved.
const LOCK_SAMPLE_TTL: Duration = Duration::from_secs(60);
/// A sample this recent is reused instead of running the collector again.
const LOCK_SAMPLE_REUSE: Duration = Duration::from_secs(1);

/// Where the memory half of a launch's admission reads the GPU from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GpuReading {
    /// Outside the journal's locks: sample the card now (bounded), and keep
    /// the sample for the locked recheck of the same command.
    Now,
    /// Under the journal's locks: the sample this command took just before the
    /// lock. Never a collector run: a slow `nvidia-smi` would stall every
    /// other host operation for its whole bound.
    BeforeLock,
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
    Sglang(Box<capyctl_domain::launch::NativeLaunch>),
    Vllm(Box<capyctl_adapters::vllm::PlanInputVllm>),
    /// The plan and whether its extensions are already built.
    Tensorfold(Box<(capyctl_adapters::tensorfold::PlanInputTensorfold, bool)>),
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
        // ADR 0018 §3: plans name the document they were approved under by
        // this fingerprint; the role's startup inventory already carries it.
        inventory.policy_fingerprint =
            capyctl_config::remote_resources::policy_fingerprint(&config.document);
        let profiles = crate::profiles::HostProfiles::new(crate::profiles::ProfileSet::new(
            config.clone(),
            inventory,
        ));
        // The host document's period, bounded when it was parsed (D9: 250 ms
        // to 5 s), so a reporter is never refused for it here.
        let load = crate::load::LoadReporter::new(ingress.clone(), host_id.clone())
            .and_then(|reporter| reporter.with_interval(config.load_report_interval))
            .ok()
            .map(Arc::new);
        // ADR 0008: the store and policy come from this host's own approved
        // document; token references resolve from `<state_dir>/secrets`.
        let sources = capyctl_config::remote_resources::local_host_document(&config.document)
            .ok()
            .and_then(|host| capyctl_config::effective::normalize_host_policy(&host).ok())
            .map(|policy| {
                crate::sources::SourceStore::new(
                    policy.model_sources.root(&policy.model_store),
                    policy.model_sources.clone(),
                    Some(config.state_dir.join("secrets")),
                )
            });
        let this = Arc::new(Self {
            sources,
            load,
            journal,
            ingress,
            identities,
            host_id,
            controller_id,
            runtime_dir,
            log_dir,
            profiles,
            authority: Arc::new(Mutex::new(SessionAuthority::default())),
            saver: None,
            checkpoints: Arc::new(CheckpointVerifier::in_memory()),
            residency: None,
            rendezvous: None,
            engine_cache: None,
            pre_admitted: Arc::new(Mutex::new(std::collections::HashMap::new())),
            lock_samples: Arc::new(Mutex::new(std::collections::HashMap::new())),
            probes: Default::default(),
            gpu: Some(crate::gpu_memory::CachedGpuSampler::new(Arc::new(
                crate::gpu_memory::sample,
            ))),
        });
        this.prime_gpu();
        this
    }

    /// Start the first device sample on a host that declares a device
    /// domain, so the session's first inventory is likely to carry it. A
    /// unified host never runs the collector.
    fn prime_gpu(&self) {
        let declared = !device_domains(&self.profiles.accepted().config.document).is_empty();
        if let Some(gpu) = self.gpu.as_ref().filter(|_| declared) {
            let _ = gpu.current();
        }
    }

    /// SPEC §7.2 / ADR 0019: a GPU sample taken now, for a launch's memory
    /// check. The cached reading may predate a victim's verified release by
    /// seconds, and refusing on it would refuse the very launch the switch
    /// planner made room for. `None` (no sampler, a failed or overdue
    /// collector) leaves the device unobserved.
    fn fresh_gpu(&self) -> Option<crate::gpu_memory::GpuSample> {
        self.gpu.as_ref().and_then(|sampler| sampler.fresh())
    }

    /// The GPU sample a launch's memory check reads ([`GpuReading`]). Outside
    /// the journal's locks the card is sampled now and the sample kept for the
    /// command's locked recheck; under the locks only that kept sample is read.
    pub(crate) fn gpu_for(
        &self,
        command: &MemberCommand,
        reading: GpuReading,
    ) -> Option<crate::gpu_memory::GpuSample> {
        let key = command.canonical_digest();
        match reading {
            GpuReading::Now => {
                let sample = self.fresh_gpu();
                if let Ok(mut samples) = self.lock_samples.lock() {
                    samples.retain(|_, (at, _)| at.elapsed() < LOCK_SAMPLE_TTL);
                    if samples.len() < MAX_PRE_ADMISSIONS {
                        samples.insert(key, (std::time::Instant::now(), sample.clone()));
                    }
                }
                sample
            }
            GpuReading::BeforeLock => self.lock_samples.lock().ok().and_then(|samples| {
                samples
                    .get(&key)
                    .filter(|(at, _)| at.elapsed() < LOCK_SAMPLE_TTL)
                    .and_then(|(_, sample)| sample.clone())
            }),
        }
    }

    /// SPEC §13.2 (review decision): sample the GPU for `command` before the
    /// journal is locked, so the locked recheck and the launch it admits read a
    /// sample taken moments ago without running the collector under the lock.
    /// A sample taken within the last second for the same command (its
    /// pre-admission just now) is reused. `None` when the launch charges no
    /// device domain, or when the card is unobserved.
    pub(crate) fn sample_before_lock(
        &self,
        command: &MemberCommand,
    ) -> Option<crate::gpu_memory::GpuSample> {
        command.action.launch_plan()?;
        let effective = self.resolve(command).ok()?;
        if !charges_device(&effective) {
            return None;
        }
        let key = command.canonical_digest();
        if let Some(recent) = self.lock_samples.lock().ok().and_then(|samples| {
            samples
                .get(&key)
                .filter(|(at, _)| at.elapsed() < LOCK_SAMPLE_REUSE)
                .map(|(_, sample)| sample.clone())
        }) {
            return recent;
        }
        self.gpu_for(command, GpuReading::Now)
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
            MemberAction::LaunchSingle(plan) | MemberAction::Launch { member: plan, .. } => {
                self.admit_launch_full(command, plan, GpuReading::Now)?
            }
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
    /// SPEC §7.2 / ADR 0019: read device domains through `sampler` instead
    /// of `nvidia-smi`.
    pub fn with_gpu_sampler(
        mut self: Arc<Self>,
        sampler: Arc<crate::gpu_memory::GpuSampler>,
    ) -> Arc<Self> {
        Arc::make_mut(&mut self).gpu = Some(crate::gpu_memory::CachedGpuSampler::new(sampler));
        self.prime_gpu();
        self
    }
    /// ADR 0018 §3: the host's runtime profile sets, shared with the live
    /// session and the local control handler.
    pub fn profiles(&self) -> Arc<crate::profiles::HostProfiles> {
        self.profiles.clone()
    }

    /// ADR 0008: the installations registered for the document `plan` was
    /// approved under; the accepted set's for a plan that names none of the
    /// held documents (a legacy retained launch).
    fn installations_for(
        &self,
        plan: &SingleLaunchPlan,
    ) -> Arc<crate::installation::InstallationRegistry> {
        self.profiles
            .for_fingerprint(&plan.host_policy_fingerprint)
            .unwrap_or_else(|| self.profiles.accepted())
            .installations
            .clone()
    }

    /// ADR 0014 §7, owner decision Q9: keep the per-host checkpoint stat cache
    /// in this private directory, so a restarted agent verifies an unchanged
    /// checkpoint without hashing it in full again.
    pub fn with_checkpoint_cache(mut self: Arc<Self>, dir: PathBuf) -> Arc<Self> {
        Arc::make_mut(&mut self).checkpoints = Arc::new(CheckpointVerifier::with_cache_dir(dir));
        self
    }
    /// ADR 0008: materialize model sources through this store instead of the
    /// one built from the host document (tests serve a loopback origin).
    pub fn with_model_sources(
        mut self: Arc<Self>,
        store: Arc<crate::sources::SourceStore>,
    ) -> Arc<Self> {
        Arc::make_mut(&mut self).sources = Some(store);
        self
    }
    /// SPEC §8.2 / T21 (found live 2026-09-23): SGLang launches keep their file
    /// rendezvous in `<dir>/<incarnation>` (the role creates `dir` 0700), and
    /// Terminate removes it once the group is proved gone.
    pub fn with_rendezvous_root(mut self: Arc<Self>, dir: PathBuf) -> Arc<Self> {
        Arc::make_mut(&mut self).rendezvous = Some(crate::rendezvous::RendezvousRoot::new(dir));
        self
    }
    /// ADR 0023 §3: where TensorFold launches keep their extension builds.
    pub fn with_engine_cache_root(mut self: Arc<Self>, dir: PathBuf) -> Arc<Self> {
        Arc::make_mut(&mut self).engine_cache =
            Some(crate::engine_cache::EngineCacheRoot::new(dir));
        self
    }
    /// ADR 0028 §7, §10: read interfaces and probe ports through `probes`
    /// instead of the host's own. Tests only: a test host holds documentation
    /// addresses no machine has.
    #[doc(hidden)]
    pub fn with_host_probes(
        mut self: Arc<Self>,
        probes: crate::host_checks::HostProbes,
    ) -> Arc<Self> {
        Arc::make_mut(&mut self).probes = probes;
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
        let Some(plan) = command.action.launch_plan() else {
            return Err(JournalError::Unauthorized);
        };
        let config = capyctl_config::parse_strict(
            capyctl_config::ConfigKind::Deployment,
            &plan.deployment_config,
        )
        .map_err(|_| JournalError::Unauthorized)?;
        let legacy = retained
            .then(|| capyctl_config::effective::legacy_retained_deployment(&config))
            .flatten();
        // ADR 0018 §3: authorized against the document the plan names:
        // accepted, being published, or the one before.
        let set = self.profiles.for_fingerprint(&plan.host_policy_fingerprint);
        if command.identity.controller_id != self.controller_id
            || command.identity.member.host_id != self.host_id
            || (legacy.is_none() && set.is_none())
        {
            return Err(JournalError::Unauthorized);
        }
        // A legacy retained launch resolves against today's accepted document.
        let set = set.unwrap_or_else(|| self.profiles.accepted());
        let config = legacy.unwrap_or(config);
        if config["runtime_profile"].as_str() != Some(&plan.profile_name) {
            return Err(JournalError::Unauthorized);
        }
        let host = capyctl_config::remote_resources::local_host_document(&set.config.document)
            .map_err(|_| JournalError::Unauthorized)?;
        // ADR 0014 §5, §7: resolve with exactly the checkpoint facts the server
        // resolved this revision with, so both sides derive the same request.
        let facts = capyctl_config::effective::CheckpointFacts {
            weights_bytes: plan.checkpoint_weights_bytes,
            state_slot_bytes: plan.checkpoint_state_slot_bytes,
            ..Default::default()
        };
        let mut effective =
            capyctl_config::effective::resolve_effective_with_checkpoint(&config, &host, facts)
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
        // ADR 0028 §4, §8: a group member runs on exactly the plan's devices,
        // `local_ranks` of them; a single launch on exactly one.
        let group = group_member(command);
        let devices = group.map_or(1, |(plan, _)| plan.topology().local_ranks as usize);
        if !matches!(
            effective.profile.engine,
            Engine::Sglang | Engine::Vllm | Engine::Tensorfold
        )
            || effective.profile.build_fingerprint != command.identity.profile_fingerprint
            || effective.model.content_fingerprint != plan.checkpoint_fingerprint
            || effective.selected_devices.len() != devices
            // Discrete GPU design §7 (review decision): with a choice of
            // GPU the launch pins the selected one; one it cannot pin (no
            // published UUID, no `gpuN` index) is refused before any effect.
            || effective.cuda_namespace().is_err()
            // ADR 0028 §8: a worker leases no service port; a SGLang worker
            // leases its loopback port from the same range.
            || leased_port(command).is_some_and(|port| {
                port < effective.host.endpoint_port_range.start
                    || port > effective.host.endpoint_port_range.end
            })
        {
            return Err(JournalError::Unauthorized);
        }
        if let Some((plan, member)) = group {
            // ADR 0028 §4: the member's engine and devices are the plan's.
            let engine = match plan.engine() {
                GroupEngine::Vllm => Engine::Vllm,
                GroupEngine::Sglang => Engine::Sglang,
                GroupEngine::Tensorfold => Engine::Tensorfold,
            };
            if effective.profile.engine != engine
                || effective
                    .selected_devices
                    .iter()
                    .map(|device| device.id.as_str())
                    .ne(member.devices.iter().map(String::as_str))
            {
                return Err(JournalError::Unauthorized);
            }
        }
        // ADR 0008: a downloaded checkpoint is inside the sources store.
        let root = effective
            .checkpoint_store()
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
        // Found live 2026-10-03: a local model named by an absolute path may
        // lie outside the store (the guides allow it; a Hugging Face cache
        // snapshot is the usual case). Its digest walk confines it to its own
        // root and refuses one other users may write (`verify_checkpoint`).
        let outside_allowed = matches!(
            effective.model.source,
            capyctl_config::model_source::ModelSource::Local { .. }
        );
        if !(checkpoint.starts_with(root) || outside_allowed) || !checkpoint.is_dir() {
            return Err(JournalError::Unauthorized);
        }
        // ADR 0028 §6 (R7): a member loads its own host's path of the
        // checkpoint, the one its plan names.
        if let Some((_, member)) = group {
            if std::path::Path::new(&member.model_path).canonicalize().ok() != Some(checkpoint) {
                return Err(JournalError::Unauthorized);
            }
        }
        // R11: a new member needs its interface; a retained one is resolved
        // to be probed, charged or stopped, never rendered again.
        let args = match self.group_args(command) {
            Ok(args) => args,
            Err(_) if retained => None,
            Err(error) => return Err(error),
        };
        if effective.profile.engine == Engine::Vllm {
            self.admit_vllm(&effective, plan, args.clone())?;
        }
        // ADR 0023 §3: a new launch needs its private build directory; a
        // retained one is never blocked by it (a Terminate must still run).
        if effective.profile.engine == Engine::Tensorfold && !retained {
            self.tensorfold_plan(&effective, plan, args)?;
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
            effective.checkpoint_store(),
            std::path::Path::new(checkpoint),
            &plan.checkpoint_digest,
        )?;
        // The same manifest yields the same weights; a plan naming others was
        // resolved against something else. ADR 0014 §5 amendment A6: the
        // weights count the draft model too; a revision recorded before they
        // did names the checkpoint's alone, which is what it was sized with.
        let drafter = self
            .checkpoints
            .drafter_weights(effective.drafter_location().as_ref())?;
        let target = verified.manifest.weights_bytes;
        if plan
            .checkpoint_weights_bytes
            .is_some_and(|bytes| bytes != target && Some(bytes) != target.checked_add(drafter))
        {
            return Err(CheckpointError::Mismatch);
        }
        // ADR 0014 amendment A16: likewise the hybrid state slot it reads.
        if plan.checkpoint_state_slot_bytes.is_some()
            && plan.checkpoint_state_slot_bytes != effective.state_slot_bytes()
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
            || id.deadline_ms <= capyctl_protocol::now_unix_ms()
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
                    let size = checkpoints.size(&location.model_store, &location.checkpoint)?;
                    let weights = with_drafter(&checkpoints, &location, size.weights_bytes)?;
                    Ok::<_, CheckpointError>((size, weights))
                })
                .await
                .map_err(|_| SessionError)?;
                match sized {
                    Ok((size, weights_bytes)) => pb::CheckpointDigestEvidence {
                        state: "sized".into(),
                        weights_bytes,
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
                    let verified =
                        checkpoints.measure(&location.model_store, &location.checkpoint)?;
                    let weights =
                        with_drafter(&checkpoints, &location, verified.manifest.weights_bytes)?;
                    // ADR 0014 amendment A16: the hybrid state slot, beside
                    // the weights, from the checkpoint's configuration.
                    Ok::<_, CheckpointError>((verified, weights, location.state_slot_bytes()))
                })
                .await
                .map_err(|_| SessionError)?;
                match measured {
                    Ok((verified, weights_bytes, state_slot_bytes)) => {
                        let manifest = verified.manifest;
                        let mismatch = plan
                            .expected_digest
                            .as_ref()
                            .is_some_and(|expected| *expected != manifest.digest);
                        pb::CheckpointDigestEvidence {
                            state: if mismatch { "mismatch" } else { "computed" }.into(),
                            digest: manifest.digest,
                            weights_bytes,
                            file_count: manifest.entries.len() as u64,
                            total_bytes: manifest.total_bytes,
                            reason: String::new(),
                            full_rehash: verified.full_rehash,
                            state_slot_bytes,
                        }
                    }
                    Err(error) => refused(error.code()),
                }
            }
        };
        Ok(pb::MemberExecutionResult {
            identity: command.to_wire().identity,
            state: "completed".into(),
            observed_at_unix_ms: capyctl_protocol::now_unix_ms(),
            checkpoint: Some(evidence),
            ..Default::default()
        })
    }

    /// ADR 0008: start or report the materialization of a deployment's
    /// declared remote source, under this host's own model-source policy. Not
    /// journaled: the store's own files make it idempotent and resumable, and
    /// a redelivery reports the same download. Refusals are evidence.
    fn materialize_source(
        &self,
        command: &MemberCommand,
        plan: &capyctl_protocol::execution::MaterializeSourcePlan,
    ) -> Result<pb::MemberExecutionResult, SessionError> {
        let id = &command.identity;
        if id.controller_id != self.controller_id
            || id.member.host_id != self.host_id
            || id.expected_state != "source"
            || id.deadline_ms <= capyctl_protocol::now_unix_ms()
        {
            return Err(SessionError);
        }
        use crate::sources::{reason, SourceStatus};
        let status = match (&self.sources, plan.source()) {
            // ADR 0018 §3: any document this host holds, as for a launch.
            (Some(store), Some(source))
                if self
                    .profiles
                    .for_fingerprint(&plan.host_policy_fingerprint)
                    .is_some() =>
            {
                store.request(&source)
            }
            _ => SourceStatus::Failed(crate::sources::SourceFailure {
                reason: reason::DENIED,
                reservation_retained: false,
            }),
        };
        let mut evidence = pb::ModelSourceEvidence {
            source_key: plan.source_key.clone(),
            ..Default::default()
        };
        match status {
            SourceStatus::Pending => evidence.state = "pending".into(),
            SourceStatus::Downloading {
                bytes_done,
                bytes_total,
            } => {
                evidence.state = "downloading".into();
                evidence.bytes_total = bytes_total;
                evidence.bytes_done = if bytes_total == 0 {
                    0
                } else {
                    bytes_done.min(bytes_total)
                };
            }
            SourceStatus::Verified { bytes } => {
                evidence.state = "verified".into();
                evidence.bytes_done = bytes;
                evidence.bytes_total = bytes;
            }
            SourceStatus::Failed(failure) => {
                evidence.state = "failed".into();
                evidence.reason = failure.reason.into();
                evidence.reservation_retained = failure.reservation_retained;
            }
        }
        Ok(pb::MemberExecutionResult {
            identity: command.to_wire().identity,
            state: "completed".into(),
            observed_at_unix_ms: capyctl_protocol::now_unix_ms(),
            source: Some(evidence),
            ..Default::default()
        })
    }

    /// ADR 0028 §7 (owner decision 10): read, never change. A group Prepare
    /// answers whether this host's member of `plan` could launch now. It
    /// journals nothing, spawns nothing and holds no port: every check reads a
    /// file, a syscall or the checkpoint cache, and a port probe binds and
    /// drops. A redelivery checks again. A failed check is a closed refusal in
    /// a completed result, never a session failure.
    async fn prepare_group(
        &self,
        command: &MemberCommand,
        plan: &GroupPlan,
    ) -> Result<pb::MemberExecutionResult, SessionError> {
        if self.authorize(command).is_err()
            || command.identity.deadline_ms <= capyctl_protocol::now_unix_ms()
        {
            return Err(SessionError);
        }
        let host = self.clone();
        let checked = plan.clone();
        let verdict = tokio::task::spawn_blocking(move || host.check_group(&checked))
            .await
            .map_err(|_| SessionError)?;
        Ok(pb::MemberExecutionResult {
            identity: command.to_wire().identity,
            state: "completed".into(),
            observed_at_unix_ms: capyctl_protocol::now_unix_ms(),
            refused: verdict.err().unwrap_or_default(),
            ..Default::default()
        })
    }

    /// ADR 0028 §7: this host's checks of its member of `plan`, against its
    /// own accepted document: the groups policy (peer address, `require_rdma`),
    /// the profile table's build fingerprint, and the digest the checkpoint
    /// cache already proves for the member's model path (never a full hash: an
    /// unmeasured path is `group_checkpoint_mismatch`). Ports are probed on
    /// the peer address, loopback and both wildcards (R15). Blocking; the `Err`
    /// is one closed code.
    pub(crate) fn check_group(
        &self,
        plan: &GroupPlan,
    ) -> Result<crate::host_checks::CheckVerdict, String> {
        let set = self.profiles.accepted();
        let host = capyctl_config::remote_resources::local_host_document(&set.config.document).ok();
        // ADR 0028 §3: the host's own resolved settings (flag > env > YAML >
        // default), as written into its document at start.
        let policy = host
            .as_ref()
            .and_then(|host| capyctl_config::groups_policy::host_groups_policy(host).ok())
            .unwrap_or_default();
        // The roots a checkpoint is measured under (ADR 0008): the sources
        // store for a materialized source, the model store otherwise.
        let roots = host
            .as_ref()
            .and_then(|host| capyctl_config::effective::normalize_host_policy(host).ok())
            .map(|policy| {
                (
                    policy.model_sources.root(&policy.model_store).to_path_buf(),
                    policy.model_store,
                )
            });
        let facts = self.probes.facts(std::path::Path::new("/"));
        crate::host_checks::prepare_member(
            plan,
            &self.host_id,
            &facts,
            &policy,
            |address, port| self.probes.port_free(address, port),
            |path| {
                let (sources, store) = roots.as_ref()?;
                let path = std::path::Path::new(path);
                let root = if path.starts_with(sources) {
                    sources
                } else {
                    store
                };
                self.checkpoints.known_digest(root, path)
            },
            |name| {
                set.config.profiles.get(name)?["build_fingerprint"]
                    .as_str()
                    .map(str::to_owned)
            },
        )
    }

    /// ADR 0028 §8 (R7): a group Launch's host checks, run again just before
    /// it is journaled: the Prepare checks ([`Self::check_group`]) and, on a
    /// SGLang head with DP attention, the ports SGLang derives from the
    /// rendezvous port (R33). Blocking; the `Err` is one closed code.
    pub(crate) fn check_group_launch(&self, command: &MemberCommand) -> Result<(), String> {
        let MemberAction::Launch { plan, member } = &command.action else {
            return Err("unauthorized".into());
        };
        self.check_group(plan)?;
        crate::host_checks::derived_ports_held(
            plan,
            &self.host_id,
            dp_attention(&member.deployment_config),
            |address, port| self.probes.port_free(address, port),
        )
    }

    /// ADR 0028 §10 (R11): this host's arguments for its member of a group
    /// Launch, with the interface holding its peer address, which vLLM and
    /// SGLang render (TensorFold renders none). A member whose address no
    /// local interface holds is refused. `None` for a single launch.
    fn group_args(
        &self,
        command: &MemberCommand,
    ) -> Result<Option<capyctl_domain::group::GroupMemberArgs>, JournalError> {
        let Some(plan) = command.action.group_launch() else {
            return Ok(None);
        };
        let mut args = capyctl_adapters::group::member_args(plan, &self.host_id)
            .ok_or(JournalError::Unauthorized)?;
        if plan.engine() != GroupEngine::Tensorfold {
            args.own_interface = Some(
                self.probes
                    .interface_for(args.own_address)
                    .ok_or(JournalError::Unauthorized)?,
            );
        }
        Ok(Some(args))
    }

    /// Where a digest request's checkpoint lives on this host, from this host's
    /// approved document only.
    fn locate(
        &self,
        plan: &DigestCheckpointPlan,
    ) -> Result<capyctl_config::effective::CheckpointLocation, &'static str> {
        // ADR 0018 §3: the document the plan names, as for a launch.
        let Some(set) = self.profiles.for_fingerprint(&plan.host_policy_fingerprint) else {
            return Err("unauthorized");
        };
        let deployment = capyctl_config::parse_strict(
            capyctl_config::ConfigKind::Deployment,
            &plan.deployment_config,
        )
        .map_err(|_| "unauthorized")?;
        let host = capyctl_config::remote_resources::local_host_document(&set.config.document)
            .map_err(|_| "unauthorized")?;
        capyctl_config::effective::checkpoint_location(&deployment, &host).map_err(|error| {
            if error.code == capyctl_config::ConfigErrorCode::NotMaterializable {
                "not_materializable"
            } else {
                "unauthorized"
            }
        })
    }

    fn scope(&self, command: &MemberCommand) -> Result<IngressScope, JournalError> {
        let Some(plan) = command.action.launch_plan() else {
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

    /// The frozen SGLang recipe for this host, from local policy only. ADR
    /// 0028 §10: a group member carries its group arguments, and a worker
    /// names its loopback port as its endpoint.
    fn sglang_frozen(
        &self,
        effective: &EffectiveDeployment,
        plan: &SingleLaunchPlan,
        served: &str,
        group: Option<capyctl_domain::group::GroupMemberArgs>,
    ) -> Result<capyctl_domain::launch::NativeLaunch, SessionError> {
        let endpoint = match group.as_ref().and_then(|group| group.worker_port) {
            Some(port) => format!("127.0.0.1:{port}"),
            None => Self::endpoint(plan),
        };
        let frozen = frozen_from_effective(
            effective,
            &plan.binding_id,
            &plan.incarnation,
            &endpoint,
            served.into(),
            format!("host-inference-{}", plan.binding_id),
            format!("host-admin-{}", plan.binding_id),
        )
        .map_err(|_| SessionError)?;
        Ok(match group {
            Some(group) => frozen.with_group(group),
            None => frozen,
        })
    }

    /// Resolve the engine-specific launch before anything durable happens.
    /// ADR 0028 §10: `group` is a group member's arguments (`None` for a
    /// single launch), rendered by each engine in its own spelling.
    fn prepare(
        &self,
        effective: &EffectiveDeployment,
        plan: &SingleLaunchPlan,
        served: &str,
        group: Option<capyctl_domain::group::GroupMemberArgs>,
    ) -> Result<PreparedLaunch, SessionError> {
        match effective.profile.engine {
            Engine::Sglang => Ok(PreparedLaunch::Sglang(Box::new(
                self.sglang_frozen(effective, plan, served, group)?,
            ))),
            Engine::Vllm => Ok(PreparedLaunch::Vllm(Box::new(
                self.vllm_plan(effective, plan, group)
                    .map_err(|_| SessionError)?,
            ))),
            Engine::Tensorfold => {
                // ADR 0023 §3: a build killed mid way left its lock; cleared
                // only while no claimed launch here may still run.
                if let Some(cache) = &self.engine_cache {
                    let dir = cache
                        .torch_extensions(&effective.profile.build_fingerprint)
                        .map_err(|_| SessionError)?;
                    self.journal
                        .with_claimed_processes(|recorded| {
                            crate::engine_cache::clear_stale_locks(&dir, recorded)
                        })
                        .map_err(|_| SessionError)?;
                }
                Ok(PreparedLaunch::Tensorfold(Box::new(
                    self.tensorfold_plan(effective, plan, group)
                        .map_err(|_| SessionError)?,
                )))
            }
        }
    }

    /// The launching adapter for a prepared launch and its journal-gated tools.
    /// `keys` are the launch's protected credentials: the API keys of a single
    /// launch or a group head; for a deep SGLang group worker, its admin key is
    /// its saver observation credential (ADR 0028 §12, R12); other workers
    /// hold none (ADR 0012: the keyed surfaces are the head's).
    fn launch_adapter(
        &self,
        prepared: PreparedLaunch,
        effective: &EffectiveDeployment,
        plan: &SingleLaunchPlan,
        keys: Option<&NativeCredentials>,
        served: &str,
        tools: Arc<dyn OwnedProcessLaunch>,
    ) -> Result<Box<dyn EngineAdapter>, SessionError> {
        Ok(match prepared {
            PreparedLaunch::Sglang(frozen) => {
                let mut adapter =
                    SglangAdapter::from_frozen(&frozen, None).map_err(|_| SessionError)?;
                // SPEC §9.2: a memory-saver launch enrolls its saver
                // observation in this host's private directory.
                if let Some(dir) = self.saver.as_ref().and_then(|s| s.observation_dir()) {
                    adapter = adapter.with_observation_dir(dir.to_path_buf());
                }
                // SPEC §8.2 / T21: the host names the launch's rendezvous
                // directory so it can remove it after the group is gone.
                // ADR 0028 §10: a group member has no file rendezvous.
                if let Some(dir) = self
                    .rendezvous
                    .as_ref()
                    .filter(|_| frozen.group().is_none())
                    .and_then(|root| root.launch_dir(&plan.incarnation))
                {
                    adapter = adapter.with_rendezvous_dir(dir);
                }
                // ADR 0014 §8, SPEC §8.2: the host's approvals, which the entry
                // applies to the destinations the extras resolve to.
                let security = &effective.profile.security;
                adapter = adapter.with_extra_approvals(
                    capyctl_config::engine_policy::extra_approvals_document(
                        &security.approved_options,
                        &security.approved_paths,
                        security.trust_remote_code,
                    ),
                );
                let worker = frozen.group().is_some_and(|group| !group.is_head());
                match keys {
                    // ADR 0028 §12 (R12): the worker's own saver credential.
                    Some(keys) if worker => {
                        adapter = adapter.with_observation_credential(hex::encode(keys.admin));
                    }
                    Some(keys) => {
                        adapter = adapter
                            .with_credentials(hex::encode(keys.inference), hex::encode(keys.admin));
                    }
                    None => {}
                }
                Box::new(
                    adapter
                        .with_launch(*frozen)
                        .with_tools(tools)
                        .with_session(plan.coordinator_session_id.clone())
                        .with_wrapper(self.runtime_dir.join("sglang_entry.py"))
                        .with_log(
                            self.log_dir
                                .join(format!("{}.log", plan.incarnation))
                                .to_string_lossy(),
                        ),
                )
            }
            PreparedLaunch::Vllm(launch) => Box::new(
                self.vllm_adapter(effective, plan, keys, served)?
                    .with_launch(*launch)
                    .with_tools(tools),
            ),
            PreparedLaunch::Tensorfold(prepared) => {
                let (input, built) = *prepared;
                Box::new(
                    self.tensorfold_adapter(effective, plan, served)?
                        .with_launch(input)
                        .with_extensions_built(built)
                        .with_tools(tools),
                )
            }
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
                SglangAdapter::from_frozen(
                    &self.sglang_frozen(effective, plan, served, None)?,
                    None,
                )
                .map_err(|_| SessionError)?
                .with_credentials(hex::encode(keys.inference), hex::encode(keys.admin)),
            ),
            Engine::Vllm => Box::new(self.vllm_adapter(effective, plan, Some(keys), served)?),
            Engine::Tensorfold => Box::new(self.tensorfold_adapter(effective, plan, served)?),
        })
    }

    /// SPEC §6.1: one accepted launch, for any supported engine. Readiness is
    /// published only on the adapter's model probe; a failure after the durable
    /// attempt leaves retained ownership for the controller, never a release.
    ///
    /// ADR 0028 §8, §9, §11: a group member launches through the same durable
    /// path with its group arguments. The head is a single launch's twin
    /// (keys, ingress, readiness). A worker opens no ingress, holds no API key
    /// and never waits for readiness: its step ends once its processes are
    /// recorded, and the reply carries them.
    async fn launch_native(
        &self,
        session: u64,
        ticket: ExecutionTicket,
        command: &MemberCommand,
        plan: &SingleLaunchPlan,
    ) -> Result<(), LaunchError> {
        // SPEC §13.2 (review decision): the durable attempt and the spawn
        // recheck admission under the journal's locks; the GPU is sampled for
        // them now, before either lock is taken.
        let host = self.clone();
        let sampled = command.clone();
        let sample = tokio::task::spawn_blocking(move || host.sample_before_lock(&sampled))
            .await
            .map_err(|_| SessionError)?;
        // Discrete GPU design §6: an engine on a device domain is sized against
        // the card's total, read from the same sample its launch check reads.
        let effective = self
            .resolve(command)
            .map_err(|_| SessionError)?
            .with_device_total(|index| device_total(sample.as_ref(), index))
            .map_err(|_| SessionError)?;
        let scope = self.scope(command).map_err(|_| SessionError)?;
        // ADR 0028 §9: only a launch with a service port serves (a single
        // launch or a group head).
        let serves = plan.service_port != 0;
        let keys = if serves {
            Some(
                self.identities
                    .load(&scope, command.identity.payload_digest)
                    .map_err(|_| SessionError)?,
            )
        } else if worker_observes(&effective) {
            // ADR 0028 §12 (R12): a deep SGLang worker enrolls its saver
            // observation with its own per-launch credential, minted and kept
            // here like every launch secret, never on argv or in the env.
            Some(
                self.identities
                    .provision_worker(&scope, command.identity.payload_digest)
                    .map_err(|_| SessionError)?,
            )
        } else {
            None
        };
        let served = effective.routes.first().cloned().ok_or(SessionError)?;
        let group = self.group_args(command).map_err(|_| SessionError)?;
        let prepared = self.prepare(&effective, plan, &served, group)?;
        let tools = self
            .journal
            .launch_tools(
                ticket,
                capyctl_protocol::now_unix_ms(),
                Arc::new(self.clone()),
            )
            .map_err(|_| SessionError)?;
        let adapter =
            self.launch_adapter(prepared, &effective, plan, keys.as_ref(), &served, tools)?;
        if let Some(keys) = keys.as_ref().filter(|_| serves) {
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
        }
        let observation = adapter
            .execute_persisted(&initialize_command(command, plan, &effective))
            .await
            .map_err(|error| LaunchError {
                engine: match error {
                    capyctl_adapters::traits::RuntimeError::LaunchFailed(text) => Some(text),
                    _ => None,
                },
            })?;
        if !serves {
            // ADR 0028 §9: a worker's readiness is the head's; its spawned
            // and recorded processes are its whole answer.
            return Ok(());
        }
        let id = &command.identity;
        self.journal
            .record_launch_ready(session, &id.command_id, &observation)
            .map_err(|_| SessionError)?;
        Ok(self.publish_ready(session, &id.command_id, &id.command_id, &scope)?)
    }

    /// Register a retained launch's exact scope with its protected credentials.
    /// A restarted host has no in-memory gate; the entry is created closed.
    fn register_retained(&self, owned: &MemberCommand) -> Result<IngressScope, SessionError> {
        let Some(plan) = owned
            .action
            .launch_plan()
            .filter(|plan| plan.service_port != 0)
        else {
            return Err(SessionError);
        };
        let scope = self.scope(owned).map_err(|_| SessionError)?;
        let keys = self
            .identities
            .load(&scope, owned.identity.payload_digest)
            .map_err(|_| SessionError)?;
        let deployment = capyctl_config::parse_strict(
            capyctl_config::ConfigKind::Deployment,
            &plan.deployment_config,
        )
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
            journal.record_probe_ready(session, &id, capyctl_protocol::now_unix_ms(), &expected)
        })
        .await
        .map_err(|_| SessionError)?
        .map_err(|_| SessionError)?;
        let scope = self.register_retained(&owned)?;
        self.publish_ready(session, owned_handle, &command.identity.command_id, &scope)
    }

    /// ADR 0028 §9 (decided 2026-10-06): the completion probe. The retained
    /// launch it names (a single launch or a group's head, the only member
    /// that serves) must still be the live, ready group its readiness
    /// recorded; the engine is then asked for one completion at temperature 0
    /// of at most `max_tokens` tokens, on its loopback endpoint with the
    /// launch's own inference key (ADR 0012), never through ingress or the
    /// router. The answer is the generated token ids; nothing is recorded and
    /// no gate moves. Any failure, or an answer with no token ids or more than
    /// were asked for, is an error, which the result reports as no tokens.
    async fn complete(
        &self,
        command: &MemberCommand,
        owned_handle: &str,
        max_tokens: u32,
    ) -> Result<Vec<u32>, SessionError> {
        let owned = self
            .journal
            .retained_command(owned_handle)
            .map_err(|_| SessionError)?;
        let plan = owned.action.launch_plan().ok_or(SessionError)?;
        let journal = self.journal.clone();
        let probe = command.clone();
        tokio::task::spawn_blocking(move || journal.probe_target(&probe))
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
        let bound = Duration::from_millis(
            u64::try_from(
                command.identity.deadline_ms - PROBE_MARGIN_MS - capyctl_protocol::now_unix_ms(),
            )
            .unwrap_or(0),
        );
        let tokens = tokio::time::timeout(
            bound,
            adapter.complete_token_ids(&served, max_tokens, bound),
        )
        .await
        .map_err(|_| SessionError)?
        .map_err(|_| SessionError)?;
        if tokens.is_empty() || tokens.len() > max_tokens as usize {
            return Err(SessionError);
        }
        Ok(tokens)
    }

    /// SPEC §13 / §13.3: the terminal refusal of a command, and for a launch
    /// the deletion of the credentials its provisioning stored: a launch refused
    /// before any effect keeps no keys behind (T37). `reason` is a closed code
    /// already known (a group Launch's host checks, ADR 0028 §8); without one
    /// the refusal's policy category is recomputed.
    async fn refused_launch(
        &self,
        command: &MemberCommand,
        reason: Option<String>,
    ) -> Result<pb::MemberExecutionResult, SessionError> {
        let result = self.refused(command, reason).await?;
        if command.action.launch_plan().is_some() {
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
        if let MemberAction::MaterializeSource(plan) = &command.action {
            return self.materialize_source(&command, plan);
        }
        // ADR 0028 §7: a group Prepare is checked here and never journaled.
        if let MemberAction::Prepare(plan) = &command.action {
            return self.prepare_group(&command, plan).await;
        }
        // SPEC §13.2: fence and duplicate checks first (cheap), then the slow
        // half of admission outside the journal's locks. A replay or a fenced
        // command never measures anything; a refusal is the terminal answer.
        let journal = self.journal.clone();
        let checked = command.clone();
        let fresh = tokio::task::spawn_blocking(move || journal.precheck(session, &checked))
            .await
            .map_err(|_| SessionError)?;
        // ADR 0028 §8 (R7, R33): a fresh group Launch runs its Prepare checks
        // again now, before its admission, against the digest its host's
        // DigestCheckpoint measured. One that fails them is refused with the
        // closed code: nothing is journaled or spawned. A replay is never
        // checked again: its own engine holds these ports.
        if matches!(command.action, MemberAction::Launch { .. }) && matches!(fresh, Ok(true)) {
            let host = self.clone();
            let checked = command.clone();
            if let Err(code) =
                tokio::task::spawn_blocking(move || host.check_group_launch(&checked))
                    .await
                    .map_err(|_| SessionError)?
            {
                return self.refused_launch(&command, Some(code)).await;
            }
        }
        if matches!(
            command.action,
            MemberAction::LaunchSingle(_)
                | MemberAction::Launch { .. }
                | MemberAction::Park { .. }
                | MemberAction::Restore { .. }
        ) && matches!(fresh, Ok(true))
        {
            let host = self.clone();
            let admitted = command.clone();
            // SPEC §13.2 (review decision): the GPU is sampled here, before
            // the journal is locked; the locked recheck reads this sample.
            match tokio::task::spawn_blocking(move || {
                host.pre_admit(&admitted)?;
                host.sample_before_lock(&admitted);
                Ok(())
            })
            .await
            .map_err(|_| SessionError)?
            {
                Ok(()) => {}
                Err(LaunchVerdict::Refused(_)) => return self.refused_launch(&command, None).await,
                Err(LaunchVerdict::Uncertain) => return Err(SessionError),
            }
        }
        if let MemberAction::Terminate { owned_handle, .. } = &command.action {
            match fresh {
                Ok(true) => {
                    // A restarted host has no in-memory gate. Re-register the
                    // exact retained scope closed using its protected
                    // credentials. A handle this host never launched has no
                    // scope to close. Admission has already closed at the
                    // controller; close the host gate before termination,
                    // retaining any active stream count. A scope that cannot
                    // be registered has no entry here to forward through, so
                    // there is nothing of it to close.
                    if let Some(scope) = self
                        .journal
                        .retained_command(owned_handle)
                        .ok()
                        .and_then(|owned| self.register_retained(&owned).ok())
                    {
                        self.ingress.close(&scope).map_err(|_| SessionError)?;
                    }
                    // Spec §5, ADR 0023 §6: the engine's own counters are read
                    // before the Terminate is journaled. One held back is
                    // answered unresolved and is gated again when redelivered;
                    // it never ends the session.
                    if !self
                        .clear_to_terminate(owned_handle, command.identity.deadline_ms)
                        .await?
                    {
                        return self.unsignalled_terminate(&command, owned_handle);
                    }
                }
                Ok(false) => {}
                // What `accept` would refuse it with; nothing is closed.
                Err(_) => return Err(SessionError),
            }
        }
        let policy = Arc::new(self.clone());
        let journal = self.journal.clone();
        let accepted_command = command.clone();
        // SPEC §§6.4, 13.2: the engine's own account of a launch that exited
        // before readiness; only its bounded summary leaves the host.
        let mut launch_failed: Option<String> = None;
        // ADR 0028 §9 (decided 2026-10-06): what a completion probe generated.
        let mut probe_tokens: Vec<u32> = Vec::new();
        let acceptance = match tokio::task::spawn_blocking(move || {
            journal.accept(
                session,
                &accepted_command,
                capyctl_protocol::now_unix_ms(),
                policy.as_ref(),
            )
        })
        .await
        .map_err(|_| SessionError)?
        {
            Ok(acceptance) => acceptance,
            // SPEC §13: local policy refused before anything was journaled.
            // That is terminal evidence, not a reason to end the session.
            Err(JournalError::Unauthorized) => return self.refused_launch(&command, None).await,
            Err(_) => return Err(SessionError),
        };
        match acceptance {
            // ADR 0028 §9 (decided 2026-10-06, OD6): the journal keeps no
            // token ids, so an exact resend of a completion probe (its first
            // reply lost) runs the bounded completion again. It records
            // nothing and moves no gate, so running it twice is harmless; a
            // reply without tokens would read as a failed probe and tear a
            // healthy group down.
            Acceptance::Replay(_) => {
                if let MemberAction::Probe {
                    owned_handle,
                    max_tokens: Some(max_tokens),
                } = &command.action
                {
                    probe_tokens = self
                        .complete(&command, owned_handle, *max_tokens)
                        .await
                        .unwrap_or_default();
                }
            }
            Acceptance::Fresh(ticket) => match &command.action {
                MemberAction::LaunchSingle(plan) | MemberAction::Launch { member: plan, .. } => {
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
                // Gated before acceptance, above.
                MemberAction::Terminate { .. } => {
                    let journal = self.journal.clone();
                    let policy = Arc::new(self.clone());
                    tokio::task::spawn_blocking(move || {
                        journal.execute(ticket, capyctl_protocol::now_unix_ms(), policy.as_ref())
                    })
                    .await
                    .map_err(|_| SessionError)?
                    .map_err(|_| SessionError)?;
                }
                MemberAction::Inspect => {
                    let journal = self.journal.clone();
                    let policy = Arc::new(self.clone());
                    tokio::task::spawn_blocking(move || {
                        journal.execute(ticket, capyctl_protocol::now_unix_ms(), policy.as_ref())
                    })
                    .await
                    .map_err(|_| SessionError)?
                    .map_err(|_| SessionError)?;
                }
                MemberAction::Probe {
                    owned_handle,
                    max_tokens,
                } => {
                    let journal = self.journal.clone();
                    let policy = Arc::new(self.clone());
                    tokio::task::spawn_blocking(move || {
                        journal.execute(ticket, capyctl_protocol::now_unix_ms(), policy.as_ref())
                    })
                    .await
                    .map_err(|_| SessionError)?
                    .map_err(|_| SessionError)?;
                    match max_tokens {
                        // ADR 0028 §9: a completion that fails or answers no
                        // token ids is ordinary evidence too: the result
                        // carries none, which the controller reads as a
                        // failed probe. Readiness and the gate are untouched.
                        Some(max_tokens) => {
                            probe_tokens = self
                                .complete(&command, owned_handle, *max_tokens)
                                .await
                                .unwrap_or_default();
                        }
                        // An engine that does not answer is ordinary evidence:
                        // the result below reports it unusable with ownership
                        // retained.
                        None => {
                            let _ = self.reprobe(session, &command, owned_handle).await;
                        }
                    }
                }
                // SPEC §§9.1, 10 (W4): the outcome, including a refusal or an
                // uncertain engine effect, is persisted evidence in the result.
                MemberAction::Park { .. } | MemberAction::Restore { .. } => {
                    self.residency(session, ticket, &command).await?;
                }
                _ => return Err(SessionError),
            },
        }
        let now = capyctl_protocol::now_unix_ms();
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
        if result.state == "completed" {
            result.probe_tokens = probe_tokens;
        }
        if command.action.launch_plan().is_some()
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
        if let MemberAction::Terminate { owned_handle, .. } = &command.action {
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
                MemberAction::Probe { owned_handle, .. }
                | MemberAction::Restore { owned_handle, .. } => self
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

/// ADR 0028 §8: a group Launch's plan and this command's member of it.
fn group_member(command: &MemberCommand) -> Option<(&GroupPlan, &MemberPlan)> {
    let plan = command.action.group_launch()?;
    let member = plan
        .members()
        .iter()
        .find(|member| member.member == command.identity.member)?;
    Some((plan, member))
}

/// SPEC §3.1, ADR 0028 §5: the loopback port a launch leases: a single
/// launch's or a group head's service port, a SGLang worker's loopback port,
/// none for any other group worker (it serves nothing).
fn leased_port(command: &MemberCommand) -> Option<u16> {
    match &command.action {
        MemberAction::LaunchSingle(plan) => Some(plan.service_port),
        MemberAction::Launch { member, .. } if member.service_port != 0 => {
            Some(member.service_port)
        }
        MemberAction::Launch { .. } => group_member(command).and_then(|(_, m)| m.worker_port),
        _ => None,
    }
}

/// ADR 0028 §12 (R12): whether a group worker of `effective` enrolls a saver
/// observation, so needs its own observation credential: a SGLang launch with
/// the memory saver on (a deep one).
fn worker_observes(effective: &EffectiveDeployment) -> bool {
    matches!(
        &effective.engine_config,
        capyctl_domain::launch::LaunchSettings::Sglang(settings) if settings.memory_saver
    )
}

/// R33 (ADR 0028 §5, §13): whether a SGLang deployment document turns DP
/// attention on, read from its own `engine_config`: the setting itself, or
/// the flag among its extra arguments. `enable_dp_attention` is a reserved
/// SGLang field today, so such a document is refused at resolution; this
/// keeps the head's port checks right if it is ever admitted.
fn dp_attention(deployment_config: &str) -> bool {
    let Ok(document) = serde_json::from_str::<serde_json::Value>(deployment_config) else {
        return false;
    };
    let config = &document["engine_config"];
    config["enable_dp_attention"].as_bool() == Some(true)
        || config["extra_args"].as_array().is_some_and(|args| {
            args.iter().any(|arg| {
                arg.as_str().is_some_and(|arg| {
                    arg == "--enable-dp-attention" || arg.starts_with("--enable-dp-attention=")
                })
            })
        })
}

/// SPEC §§6.4, 13.2: the bounded summary of a launch whose recorded processes
/// all exited before readiness, with the exit status this host's launcher
/// reaped (the api process first) when it knows it.
fn launch_failure(result: &pb::MemberExecutionResult, engine: Option<&str>) -> String {
    use capyctl_adapters::launch_failure::{summary, EngineExit};
    let mut processes: Vec<&pb::OwnedProcessObservation> = result.processes.iter().collect();
    processes.sort_by_key(|p| p.role != "api");
    let exit = processes.iter().find_map(|p| {
        let identity = capyctl_domain::completion::ProcessIdentity {
            role: p.role.clone(),
            pid: p.pid,
            boot_id: p.boot_id.clone(),
            start_ticks: p.start_ticks,
        };
        match capyctl_launchers::reaped::status_of(&identity)? {
            capyctl_launchers::reaped::ReapedStatus::Code(code) => Some(EngineExit::Code(code)),
            capyctl_launchers::reaped::ReapedStatus::Signal(signal) => {
                Some(EngineExit::Signal(signal))
            }
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
            u64::try_from(stop_at_ms - capyctl_protocol::now_unix_ms()).unwrap_or(0),
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
    if !capyctl_adapters::forward::probe_answered(&answer) {
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
            // ADR 0028 §8: a group member's launch is admitted exactly as a
            // single launch is, on its own member launch (its host checks ran
            // again just before acceptance, `check_group_launch`). Like a
            // single launch it is sent to a reserved member.
            MemberAction::LaunchSingle(plan) | MemberAction::Launch { member: plan, .. }
                if command.identity.expected_state == "reserved" =>
            {
                // ADR 0014 §7, SPEC §13: checkpoint, pool shape and memory, the
                // same admission provisioning runs and reports when refused.
                // This runs under the journal's locks, so the GPU is read from
                // the sample taken just before them (`sample_before_lock`).
                Ok(self.admit_launch(command, plan, GpuReading::BeforeLock)?)
            }
            MemberAction::Terminate { .. } if command.identity.expected_state == "retained" => {
                Ok(())
            }
            // SPEC §§6.1, 13.2: a probe questions an engine this host already
            // owns; the journal binds it to that exact retained launch.
            MemberAction::Probe { .. } if command.identity.expected_state == "ready" => Ok(()),
            MemberAction::Inspect => Ok(()),
            // ADR 0028 §7: a group Prepare (it always carries a group plan) has
            // no process effect; this host's own checks decide its answer, and
            // it is never journaled.
            MemberAction::Prepare(_) => Ok(()),
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
        // unified pools at resolution (ADR 0010 decision 5), so where it
        // resolved the pools are distinct and it parks to host RAM (discrete
        // GPU design §5); `deep` parks everywhere.
        if !effective.residency.parks() {
            return Err(JournalError::Unauthorized);
        }
        // ADR 0008: a Park needs the internals deep parking drives; a build the
        // launch-time probe found without them is refused, launch unchanged.
        // SPEC §13.2: that measurement ran outside the lock when the command
        // was pre-admitted; only a command that was not runs it here.
        if !self.pre_admitted(command)
            && self
                .park_capability(&effective, &plan.profile_name)
                .is_some()
        {
            return Err(JournalError::Unauthorized);
        }
        match effective.profile.engine {
            // SPEC §9.1 / T21: the host's deep-park policy must admit the
            // sleep and collective controls, and the launch must have been
            // started with sleep mode behind the guard.
            Engine::Vllm => {
                let launch = self.vllm_plan(&effective, plan, None)?;
                if capyctl_adapters::vllm::park_policy(&effective)
                    != capyctl_adapters::ParkPolicy::Enabled
                    || launch.sleep_flags.is_empty()
                {
                    return Err(JournalError::Unauthorized);
                }
                Ok(())
            }
            Engine::Sglang => Ok(()),
            // ADR 0023 §6: TensorFold never parks.
            Engine::Tensorfold => Err(JournalError::Unauthorized),
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
            MemberAction::LaunchSingle(_) | MemberAction::Launch { .. } => {
                Ok(self.admit_beside_claims(command, claimed)?)
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
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Provisioned, SessionError>> + Send>,
    > {
        let host = self.clone();
        Box::pin(async move {
            command.verify_digest().map_err(|_| SessionError)?;
            if command.identity.deadline_ms <= capyctl_protocol::now_unix_ms()
                || command.identity.controller_id != host.controller_id
                || command.identity.member.host_id != host.host_id
                || command.identity.expected_state != "reserved"
            {
                return Err(SessionError);
            }
            // ADR 0028 §9: a group head is provisioned like a single launch
            // (its keys scoped by its own binding and incarnation); a worker
            // serves nothing and is never provisioned.
            let Some(plan) = command
                .action
                .launch_plan()
                .filter(|plan| plan.service_port != 0)
                .cloned()
            else {
                return Err(SessionError);
            };
            // ADR 0014 §7: admission measures the checkpoint, which on a first
            // placement hashes it in full; keep that off the runtime.
            let authorizer = host.clone();
            let authorized = command.clone();
            // SPEC §§3.1, 7.3: alone, then beside every launch still claimed.
            match tokio::task::spawn_blocking(move || {
                authorizer.admit_launch_here(&authorized, &plan)
            })
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
                    capyctl_protocol::now_unix_ms(),
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
                exited
                    .into_iter()
                    .map(|launch| launch.exit.to_wire())
                    .collect()
            })
            .await
            .unwrap_or_default()
        }))
    }
    fn session_inventory(&self) -> Option<pb::ReportInventory> {
        // SPEC §§4.2, 13: a new session's first inventory waits (bounded) for
        // a device sample, so it never publishes a declared device as
        // unobserved only because the collector was still running. The
        // sample becomes the cached reading `inventory` reports.
        let declared = !device_domains(&self.profiles.accepted().config.document).is_empty();
        if let Some(gpu) = self.gpu.as_ref().filter(|_| declared) {
            let _ = gpu.fresh();
        }
        self.inventory()
    }
    fn inventory(&self) -> Option<pb::ReportInventory> {
        // ADR 0018 §3: the accepted set describes what this host publishes.
        let set = self.profiles.accepted();
        let mut inventory = set.inventory.clone();
        // ADR 0008: status carries installation drift and missing capabilities.
        set.installations.overlay(&mut inventory.profiles);
        // SPEC §7.2 / ADR 0019: each domain is refreshed from its own source.
        // A device domain reads its GPU; every other domain reads host memory.
        let devices = device_domains(&set.config.document);
        let (device, host): (Vec<usize>, Vec<usize>) = (0..inventory.domains.len())
            .partition(|i| devices.contains_key(&inventory.domains[*i].domain_id));
        // Startup only publishes measured domains. Refresh exactly the one host
        // pool, preserving its approved name and policy binding; several host
        // pools would need their own observers, never a copied reading.
        let [host] = host[..] else {
            return None;
        };
        let memory = crate::memory::read_host_memory().ok()?.memory;
        // ADR 0007: availability first, then the processes still alive, so a
        // process that grew in between is under-credited, never over-credited.
        let residents = self
            .residency
            .as_ref()
            .map(|sampler| sampler.current())
            .unwrap_or_default();
        let domain = &mut inventory.domains[host];
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
                // ADR 0019 (`device_memory_domains`): the split figures, so
                // the server credits each domain with its own memory.
                device_bytes: p.device_bytes,
                host_bytes: p.host_bytes,
            })
            .collect();
        if !device.is_empty() {
            // One sample for every device domain, taken only on a host that
            // declares one. A failed, hung or stale sample leaves every device
            // unobserved; this refresh runs on the session loop and never
            // waits for the collector (heartbeats share that loop).
            let sample = self.gpu.as_ref().and_then(|sampler| sampler.current());
            for i in device {
                let domain = &mut inventory.domains[i];
                let observed = devices[&domain.domain_id].and_then(|index| {
                    let sample = sample.as_ref()?;
                    let memory = sample
                        .devices
                        .iter()
                        .find(|d| d.index == index)?
                        .memory
                        .as_ref()?;
                    Some((memory, sample.sampled_at_ms))
                });
                match observed {
                    Some((memory, sampled_at_ms)) => {
                        domain.observed_bytes = memory.free_bytes;
                        domain.available_bytes = memory.free_bytes;
                        domain.capacity_bytes = memory.total_bytes;
                        domain.observed_at_unix = sampled_at_ms / 1000;
                        domain.observed_at_unix_ms = sampled_at_ms;
                    }
                    // SPEC §7.2: an unobserved device is unknown (`-1`), which
                    // closes admission on its domain; RAM never stands in.
                    None => {
                        domain.observed_bytes = -1;
                        domain.available_bytes = -1;
                        domain.capacity_bytes = -1;
                    }
                }
                domain.residents.clear();
            }
        }
        // ADR 0028 §3, §14: the host's peer address and check findings,
        // read fresh for this report.
        inventory.group = group_inventory(&set.config.document);
        Some(inventory)
    }
}

/// ADR 0028 §3, §14: what a host with a peer address publishes about group
/// work: the address, every host tuning finding (warnings, and refusals under
/// `require_rdma`) and `peer_address_not_local` when no local interface holds
/// it. A host that declares no peer address publishes none, exactly as before
/// groups existed. ADR 0028 §7 (owner decision 10): read, never change.
fn group_inventory(document: &serde_json::Value) -> Option<pb::GroupInventory> {
    let host = capyctl_config::remote_resources::local_host_document(document).ok()?;
    let policy = capyctl_config::groups_policy::host_groups_policy(&host).ok()?;
    let peer = policy.peer_address?;
    let facts = crate::host_checks::read_host_facts(std::path::Path::new("/"));
    // ADR 0028 §7: every tuning finding, not only the first refusal, so status
    // shows each gap at once.
    let mut findings = crate::host_checks::tuning_findings(&facts, &policy);
    // SPEC §3 (ADR 0028 §3): the agent verifies its peer address is on a local
    // interface and reports it in its inventory; a mismatch is a finding here
    // and refuses group work at Prepare.
    if !crate::host_checks::peer_address_local(&facts, &policy) {
        findings.push("peer_address_not_local".into());
    }
    Some(pb::GroupInventory {
        peer_address: peer.to_string(),
        findings,
    })
}

/// The device domains of an approved host document, each with the nvidia-smi
/// index its `gpuN` device names (`None`: a device id with no index, which is
/// never observed). A document whose policy does not resolve declares none, so
/// its domains are refreshed as before.
/// The total memory of GPU `index` in `sample`, when the sample reports it.
fn device_total(sample: Option<&crate::gpu_memory::GpuSample>, index: u32) -> Option<i64> {
    sample?
        .devices
        .iter()
        .find(|device| device.index == index)?
        .memory
        .as_ref()
        .map(|memory| memory.total_bytes)
}

fn device_domains(document: &serde_json::Value) -> std::collections::BTreeMap<String, Option<u32>> {
    crate::device_domains::device_domains(document)
        .into_iter()
        .map(|(id, domain)| (id, domain.index))
        .collect()
}

/// ADR 0014 §5 amendment A6: a checkpoint's weights plus those of the draft
/// model its launch loads beside it.
fn with_drafter(
    checkpoints: &crate::checkpoint::CheckpointVerifier,
    location: &capyctl_config::effective::CheckpointLocation,
    weights: i64,
) -> Result<i64, CheckpointError> {
    checkpoints
        .drafter_weights(location.drafter.as_ref())?
        .checked_add(weights)
        .ok_or(CheckpointError::TooLarge)
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
        assert!(executor
            .publish_ready(1, "first", "late-first", &scope)
            .is_err());
        executor.connected(2).unwrap();
        assert!(executor
            .publish_ready(1, "first", "late-first", &scope)
            .is_err());
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
        executor
            .publish_ready(2, "first", "second", &scope)
            .unwrap();
        executor.disconnected(2);
        assert!(executor
            .publish_ready(2, "first", "late-second", &scope)
            .is_err());
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
        use capyctl_domain::group::{CommandIdentity, MemberKey};
        let root = directory();
        let identity_dir = directory();
        std::fs::create_dir_all(root.path().join("models/toy")).unwrap();
        std::fs::create_dir_all(root.path().join("runtime")).unwrap();
        std::fs::write(root.path().join("runtime/capyctl_vllm_guard.py"), "").unwrap();
        std::fs::write(
            root.path()
                .join("runtime")
                .join(capyctl_adapters::vllm::VLLM_ENTRY),
            "",
        )
        .unwrap();
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../capyctl-config/tests/fixtures/f2-deployment.json"
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
        // Plans lease a free port at runtime (`closed_port`), not the
        // fixture's fixed 8100-8199, which tests running in parallel share.
        document["resource_policy"]["endpoint_port_range"] = wide_ports();
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
                service_port: closed_port(),
                issued_at_ms: 1,
                coordinator_session_id: "01K00000000000000000000004".into(),
                checkpoint_digest: String::new(),
                checkpoint_weights_bytes: None,
                checkpoint_state_slot_bytes: None,
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
        checkpoint_fixture_with(root, identity_dir, |_| {})
    }

    /// Admission reads real host memory; shrink the 48 GiB fixture to fit it.
    fn host_memory_scale() -> f64 {
        let memory = crate::memory::read_host_memory().unwrap().memory;
        let gib = (1u64 << 30) as f64;
        (memory.capacity_bytes as f64 / 2.0 / (48.0 * gib))
            .min(memory.available_bytes as f64 / 2.0 / (26.0 * gib))
            .min(1.0)
    }

    fn scale_bytes(value: &mut serde_json::Value, scale: f64) {
        let bytes = capyctl_config::effective::parse_bytes(value.as_str().unwrap()).unwrap();
        *value = serde_json::json!(format!("{}MiB", (bytes as f64 * scale) as i64 >> 20));
    }

    /// [`checkpoint_fixture`] with an edit of the host document before it is
    /// parsed.
    fn checkpoint_fixture_with(
        root: &std::path::Path,
        identity_dir: &std::path::Path,
        edit: impl FnOnce(&mut serde_json::Value),
    ) -> (Arc<NativeHostExecution>, serde_json::Value, String) {
        std::fs::create_dir_all(root.join("models/toy")).unwrap();
        std::fs::write(root.join("models/toy/config.json"), "{}").unwrap();
        std::fs::write(root.join("models/toy/model.safetensors"), "weights").unwrap();
        std::fs::create_dir_all(root.join("runtime")).unwrap();
        std::fs::write(root.join("runtime/capyctl_vllm_guard.py"), "").unwrap();
        std::fs::write(
            root.join("runtime")
                .join(capyctl_adapters::vllm::VLLM_ENTRY),
            "",
        )
        .unwrap();
        // ADR 0008: a sleep-mode vLLM launch also imports the capability probes.
        std::fs::write(root.join("runtime/engine_capabilities.py"), "").unwrap();
        // SPEC §9.1 / T21: a prepared host's runtime directory is private to
        // the agent user whatever the umask that wrote it.
        for path in [
            root.join("runtime"),
            root.join("runtime/capyctl_vllm_guard.py"),
            root.join("runtime")
                .join(capyctl_adapters::vllm::VLLM_ENTRY),
            root.join("runtime/engine_capabilities.py"),
        ] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../capyctl-config/tests/fixtures/f2-deployment.json"
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
        // Plans lease a free port at runtime (`closed_port`), not the
        // fixture's fixed 8100-8199, which tests running in parallel share.
        document["resource_policy"]["endpoint_port_range"] = wide_ports();
        edit(&mut document);
        let config = HostConfig::parse(&document.to_string()).unwrap();
        let policy = capyctl_config::remote_resources::policy_fingerprint(&config.document);
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

    fn checkpoint_identity(
        id: &str,
        expected_state: &str,
    ) -> capyctl_domain::group::CommandIdentity {
        capyctl_domain::group::CommandIdentity {
            controller_id: "controller".into(),
            member: capyctl_domain::group::MemberKey {
                host_id: "host".into(),
                member_id: "head".into(),
            },
            deployment_id: "deployment".into(),
            operation_id: "operation".into(),
            command_id: id.into(),
            step_id: id.into(),
            generation: 1,
            revision: 1,
            deadline_ms: capyctl_protocol::now_unix_ms() + 60_000,
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
            service_port: closed_port(),
            issued_at_ms: 1,
            coordinator_session_id: "01K00000000000000000000004".into(),
            checkpoint_digest: digest.into(),
            checkpoint_weights_bytes: None,
            checkpoint_state_slot_bytes: None,
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
        assert!(
            executor
                .checkpoint_unchanged(&launch, &plan, &recorded)
                .await
        );
        let other_digest = format!("sha256:{}", "0".repeat(64));
        assert!(
            !executor
                .checkpoint_unchanged(&launch, &plan, &other_digest)
                .await
        );
        // Weights the server did not resolve with are refused as well.
        let weighed = SingleLaunchPlan {
            checkpoint_weights_bytes: Some(1),
            checkpoint_state_slot_bytes: None,
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
            executor
                .verify_checkpoint(&effective, &weighed)
                .unwrap_err(),
            CheckpointError::Mismatch
        );
        // ADR 0014 amendment A16: so is a state slot this host does not read.
        let stated = SingleLaunchPlan {
            checkpoint_state_slot_bytes: Some(1),
            ..plan.clone()
        };
        assert_eq!(
            executor.verify_checkpoint(&effective, &stated).unwrap_err(),
            CheckpointError::Mismatch
        );
        // Pre-WE3 plan: adoptable, never launched, and woken only against the
        // digest the server measured and recorded for it (owner decision 5).
        let (legacy, legacy_plan) = launch_with(&deployment, &policy, "");
        assert!(executor.authorize(&legacy).is_err());
        assert!(
            !executor
                .checkpoint_unchanged(&legacy, &legacy_plan, "")
                .await
        );
        assert!(
            executor
                .checkpoint_unchanged(&legacy, &legacy_plan, &recorded)
                .await
        );
        assert!(
            !executor
                .checkpoint_unchanged(&legacy, &legacy_plan, &other_digest)
                .await
        );
        // The checkpoint changes under the recorded digest.
        std::fs::write(root.path().join("models/toy/model.safetensors"), "swapped").unwrap();
        let effective = executor.resolve(&launch).unwrap();
        assert_eq!(
            executor.verify_checkpoint(&effective, &plan).unwrap_err(),
            CheckpointError::Mismatch
        );
        assert!(executor.authorize(&launch).is_err());
        assert!(!executor.checkpoint_unchanged(&launch, &plan, "").await);
        assert!(
            !executor
                .checkpoint_unchanged(&legacy, &legacy_plan, &recorded)
                .await
        );
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
        assert!(
            executor
                .checkpoint_unchanged(&legacy, &legacy_plan, "")
                .await
        );
        assert!(
            executor
                .checkpoint_unchanged(&legacy, &legacy_plan, &parked_digest)
                .await
        );
        std::fs::write(root.path().join("models/toy/model.safetensors"), "swapped").unwrap();
        let swapped = measure();
        assert!(
            !executor
                .checkpoint_unchanged(&legacy, &legacy_plan, &swapped)
                .await
        );
        assert!(
            !executor
                .checkpoint_unchanged(&legacy, &legacy_plan, "")
                .await
        );
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
        let scale = host_memory_scale();
        let (executor, mut deployment, policy) =
            checkpoint_fixture_with(root.path(), identity_dir.path(), |document| {
                let domain = &mut document["resource_policy"]["domains"]["unified"];
                for field in [
                    "managed_limit",
                    "free_reserve",
                    "parked_limit",
                    "host_kv_limit",
                ] {
                    scale_bytes(&mut domain[field], scale);
                }
            });
        for phase in ["cold", "ready", "parking", "parked", "wake"] {
            let allocations = deployment["resources"][phase]["allocations"].as_array_mut();
            for allocation in allocations.into_iter().flatten() {
                scale_bytes(&mut allocation["bytes"], scale);
                scale_bytes(&mut allocation["host_kv_bytes"], scale);
            }
        }
        scale_bytes(
            &mut deployment["engine_config"]["memory"]["kv_cache"],
            scale,
        );
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

    /// ADR 0014 §5 amendment A6 (found live 2026-10-02): a host counts the
    /// draft model's weight files with the checkpoint's, sized or measured;
    /// the digest stays the checkpoint's own.
    // T34
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn digest_checkpoint_counts_the_draft_models_weights() {
        let root = directory();
        let identity_dir = directory();
        let models = root.path().join("models");
        let (executor, mut deployment, policy) =
            checkpoint_fixture_with(root.path(), identity_dir.path(), |document| {
                let security = &mut document["runtime_profiles"]["local"]["security"];
                security["approved_options"] = serde_json::json!(["--speculative-config"]);
                security["approved_paths"] = serde_json::json!([models]);
            });
        std::fs::create_dir_all(models.join("drafter")).unwrap();
        std::fs::write(models.join("drafter/model.safetensors"), "draft-weights").unwrap();
        let config = serde_json::json!({"method": "dflash", "model": models.join("drafter")});
        deployment["engine_config"]["accept_extra_args"] = serde_json::json!(true);
        deployment["engine_config"]["extra_args"] =
            serde_json::json!(["--speculative-config", config.to_string()]);
        for size_only in [false, true] {
            let mut command = MemberCommand {
                identity: checkpoint_identity("digest", "checkpoint"),
                action: MemberAction::DigestCheckpoint(DigestCheckpointPlan {
                    size_only,
                    deployment_config: deployment.to_string(),
                    host_policy_fingerprint: policy.clone(),
                    expected_digest: None,
                }),
            };
            command.identity.payload_digest = command.canonical_digest();
            let result = executor.execute(1, command.clone()).await.unwrap();
            let evidence = result.checkpoint.unwrap();
            assert_eq!(evidence.weights_bytes, 7 + 13, "size_only {size_only}");
            if !size_only {
                let target = crate::checkpoint::CheckpointVerifier::in_memory()
                    .measure(&models, &models.join("toy"))
                    .unwrap()
                    .manifest;
                assert_eq!(evidence.digest, target.digest);
            }
        }
        // A launch resolved with both weights verifies; so does one recorded
        // before the draft model was counted; any other weights do not.
        let recorded = crate::checkpoint::CheckpointVerifier::in_memory()
            .measure(&models, &models.join("toy"))
            .unwrap()
            .manifest
            .digest;
        for (weights, verifies) in [(20, true), (7, true), (13, false)] {
            let (launch, plan) = launch_with(&deployment, &policy, &recorded);
            let plan = SingleLaunchPlan {
                checkpoint_weights_bytes: Some(weights),
                checkpoint_state_slot_bytes: None,
                ..plan
            };
            let launch = MemberCommand {
                action: MemberAction::LaunchSingle(plan.clone()),
                ..launch
            };
            let effective = executor.resolve(&launch).unwrap();
            assert_eq!(
                executor.verify_checkpoint(&effective, &plan).is_ok(),
                verifies,
                "{weights}"
            );
        }
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
                capyctl_protocol::execution::validate_result(&command, &result).unwrap();
                result.checkpoint.unwrap()
            }
        };
        let computed = run(digest(&deployment, &policy, None)).await;
        assert_eq!(computed.state, "computed");
        assert_eq!(computed.weights_bytes, 7);
        assert_eq!(computed.file_count, 2);
        assert!(computed.full_rehash);
        // ADR 0014 amendment A16: no hybrid state slot for a vLLM launch.
        assert_eq!(computed.state_slot_bytes, None);
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
        assert_eq!(
            (again.state.as_str(), again.full_rehash),
            ("computed", false)
        );
        let other = format!("sha256:{}", "0".repeat(64));
        assert_eq!(
            run(digest(&deployment, &policy, Some(&other))).await.state,
            "mismatch"
        );
        let refused = run(digest(&deployment, &"c".repeat(64), None)).await;
        assert_eq!(
            (refused.state.as_str(), refused.reason.as_str()),
            ("refused", "unauthorized")
        );
        let mut escaping = deployment.clone();
        escaping["model"]["path"] = serde_json::json!("/etc");
        let refused = run(digest(&escaping, &policy, None)).await;
        assert_eq!(
            (refused.state.as_str(), refused.reason.as_str()),
            ("refused", "invalid_root")
        );
        // Another controller's command is not evidence at all.
        let mut foreign = digest(&deployment, &policy, None);
        foreign.identity.controller_id = "other".into();
        foreign.identity.payload_digest = foreign.canonical_digest();
        assert!(executor.execute(1, foreign).await.is_err());
    }

    /// ADR 0008: MaterializeSource answers from this host's own policy and
    /// store; a remote source is denied where the host turned it off; once
    /// verified, the WE3 digest measures the materialized directory like any
    /// local checkpoint. Refusals never end the session.
    // T14 T34
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn materialize_source_downloads_then_the_digest_measures_the_copy() {
        use sha2::Digest as _;
        let root = directory();
        let identity_dir = directory();
        // Owner decision 2026-09-25: sources are allowed by default; this
        // host turns them off explicitly, so nothing is fetched for it.
        let (executor, mut deployment, policy) =
            checkpoint_fixture_with(root.path(), identity_dir.path(), |host| {
                host["model_sources"] =
                    serde_json::json!({"huggingface": "disabled", "http": "disabled"});
            });
        let weights = vec![3_u8; 5000];
        let sha = hex::encode(sha2::Sha256::digest(&weights));
        let served = weights.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let app = axum::Router::new().route(
            "/model.bin",
            axum::routing::get(move || {
                let served = served.clone();
                async move { served }
            }),
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let model = deployment["model"].as_object_mut().unwrap();
        model.remove("path");
        model.insert(
            "source".into(),
            serde_json::json!({"http": {"url": "https://weights.example.test/model.bin", "sha256": sha}}),
        );
        let command = |id: &str, policy: &str| {
            let plan = capyctl_protocol::execution::MaterializeSourcePlan::new(
                &deployment.to_string(),
                policy,
            )
            .expect("a remote source");
            let mut command = MemberCommand {
                identity: checkpoint_identity(id, "source"),
                action: MemberAction::MaterializeSource(plan),
            };
            command.identity.payload_digest = command.canonical_digest();
            command
        };
        let run = |executor: Arc<NativeHostExecution>, command: MemberCommand| async move {
            let result = executor.execute(1, command.clone()).await.unwrap();
            capyctl_protocol::execution::validate_result(&command, &result).unwrap();
            result.source.unwrap()
        };
        // The fixture host turned model sources off: denied, nothing fetched.
        let denied = run(executor.clone(), command("s1", &policy)).await;
        assert_eq!(
            (denied.state.as_str(), denied.reason.as_str()),
            ("failed", "denied")
        );
        // A command resolved against another host document is denied too.
        let foreign = run(executor.clone(), command("s2", &"c".repeat(64))).await;
        assert_eq!(foreign.reason, "denied");

        let models = root.path().join("models");
        let store = crate::sources::SourceStore::with_loopback_origin(
            &models,
            capyctl_config::effective::ModelSourcePolicy {
                http: capyctl_config::effective::SourceSwitch::Allowed,
                max_bytes: Some(1 << 20),
                ..Default::default()
            },
            None,
            &origin,
        );
        let executor = executor.with_model_sources(store);
        let mut evidence = run(executor.clone(), command("s3", &policy)).await;
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        for attempt in 0.. {
            if evidence.state == "verified" || std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            evidence = run(executor.clone(), command(&format!("p{attempt}"), &policy)).await;
        }
        assert_eq!(evidence.state, "verified", "{evidence:?}");
        assert_eq!((evidence.bytes_done, evidence.bytes_total), (5000, 5000));
        assert_eq!(evidence.source_key, format!("sources/http/{sha}"));
        // WE3 over the materialized copy (ADR 0014 §7).
        let mut digest = MemberCommand {
            identity: checkpoint_identity("d", "checkpoint"),
            action: MemberAction::DigestCheckpoint(DigestCheckpointPlan {
                size_only: false,
                deployment_config: deployment.to_string(),
                host_policy_fingerprint: policy.clone(),
                expected_digest: None,
            }),
        };
        digest.identity.payload_digest = digest.canonical_digest();
        let measured = executor
            .execute(1, digest)
            .await
            .unwrap()
            .checkpoint
            .unwrap();
        assert_eq!(measured.state, "computed", "{measured:?}");
        assert_eq!(measured.total_bytes, 5000);
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
            executor
                .identities
                .load(&scope, launch.identity.payload_digest)
                .is_err(),
            "a refused provision stores no key"
        );
        let session = executor.journal.connect().unwrap();
        executor.connected(session).unwrap();
        for _ in 0..2 {
            let result = executor.execute(session, launch.clone()).await.unwrap();
            capyctl_protocol::execution::validate_result(&launch, &result).unwrap();
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
        sglang_fixture_with(root, identity_dir, residency, |_| {})
    }

    /// [`sglang_fixture`] with an edit of the host document before it is
    /// parsed (a `host_backed` launch needs distinct host and device memory).
    fn sglang_fixture_with(
        root: &std::path::Path,
        identity_dir: &std::path::Path,
        residency: &str,
        edit: impl FnOnce(&mut serde_json::Value),
    ) -> (Arc<NativeHostExecution>, serde_json::Value, String) {
        std::fs::create_dir_all(root.join("models/toy")).unwrap();
        std::fs::write(root.join("models/toy/config.json"), "{}").unwrap();
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../capyctl-config/tests/fixtures/f2-deployment.json"
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
        // Plans lease a free port at runtime (`closed_port`), not the
        // fixture's fixed 8100-8199, which tests running in parallel share.
        document["resource_policy"]["endpoint_port_range"] = wide_ports();
        let profile = &mut document["runtime_profiles"]["local"];
        profile["engine"] = "sglang".into();
        profile["args"] = serde_json::json!([]);
        profile["security"]["admin_credential_ref"] = "secret://admin-key".into();
        edit(&mut document);
        let config = HostConfig::parse(&document.to_string()).unwrap();
        let policy = capyctl_config::remote_resources::policy_fingerprint(&config.document);
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
        let capyctl_domain::launch::LaunchSettings::Sglang(settings) = &effective.engine_config
        else {
            panic!("an SGLang profile resolves SGLang settings");
        };
        assert!(!settings.memory_saver && !settings.cpu_weight_backup);
        assert_eq!(settings.weight_restore, "disk_reload");

        let session = executor.journal.connect().unwrap();
        executor.connected(session).unwrap();
        executor
            .journal
            .accept(session, &launch, capyctl_protocol::now_unix_ms(), &Admit)
            .unwrap();
        let mut park = MemberCommand {
            identity: checkpoint_identity("park", "ready"),
            action: MemberAction::Park {
                owned_handle: "launch".into(),
            },
        };
        park.identity.payload_digest = park.canonical_digest();
        let refused = executor.execute(session, park.clone()).await.unwrap();
        capyctl_protocol::execution::validate_result(&park, &refused).unwrap();
        assert_eq!(refused.state, "completed");
        assert_eq!(refused.refused, "residency_tier");
        assert_eq!(refused.residency.as_ref().unwrap().state, "unchanged");
        assert!(!refused.model_usable);
        assert_eq!(
            executor.journal.history(0, 100).unwrap().len(),
            1,
            "the park is not journaled"
        );
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

    /// A TensorFold profile on the lab document, restart_only, with a cache root.
    fn tensorfold_fixture(
        root: &std::path::Path,
        identity_dir: &std::path::Path,
    ) -> (Arc<NativeHostExecution>, serde_json::Value, String) {
        let (executor, mut deployment, policy) =
            sglang_fixture_with(root, identity_dir, "restart_only", |document| {
                document["resource_policy"]["endpoint_port_range"] =
                    serde_json::json!({"start": 1024, "end": 65535});
                let profile = &mut document["runtime_profiles"]["local"];
                profile["engine"] = "tensorfold".into();
                profile["executable"] = "/opt/tf/bin/tensorfold".into();
                profile["build_fingerprint"] = "0.6.0".into();
                profile["args"] = serde_json::json!([]);
                profile["security"]["deep_park"] = "disabled".into();
                profile["security"]
                    .as_object_mut()
                    .unwrap()
                    .remove("admin_credential_ref");
            });
        deployment["engine_config"] = serde_json::json!({"context_length": 8192});
        let engines = root.join("engines");
        std::fs::create_dir(&engines).unwrap();
        std::fs::set_permissions(&engines, std::fs::Permissions::from_mode(0o700)).unwrap();
        let executor = executor.with_engine_cache_root(engines);
        (executor, deployment, policy)
    }

    /// A TensorFold launch command for the fixture, on `port`.
    fn tensorfold_launch(
        deployment: &serde_json::Value,
        policy: &str,
        port: u16,
    ) -> (MemberCommand, SingleLaunchPlan) {
        let (mut launch, mut plan) = launch_with(deployment, policy, "");
        plan.service_port = port;
        launch.action = MemberAction::LaunchSingle(plan.clone());
        launch.identity.profile_fingerprint = "0.6.0".into();
        launch.identity.payload_digest = launch.canonical_digest();
        (launch, plan)
    }

    // T41 (ADR 0023 §3): the host resolves and prepares a TensorFold launch
    // from its own document; the plan renders with a private extensions
    // directory and no build yet.
    #[test]
    fn a_tensorfold_launch_prepares_from_local_policy() {
        let root = directory();
        let identity_dir = directory();
        let (executor, deployment, policy) = tensorfold_fixture(root.path(), identity_dir.path());
        let (launch, plan) = tensorfold_launch(&deployment, &policy, closed_port());
        let effective = executor.resolve(&launch).unwrap();
        let (input, built) = executor.tensorfold_plan(&effective, &plan, None).unwrap();
        assert!(!built);
        assert!(input
            .extensions_dir
            .unwrap()
            .ends_with("engines/tensorfold/0.6.0/torch_extensions"));
        assert_eq!(input.context_length, 8192);
        assert!(matches!(
            executor.prepare(&effective, &plan, "toy", None),
            Ok(PreparedLaunch::Tensorfold(..))
        ));
    }

    // T41 (ADR 0023 §3, found live 2026-10-02): a build killed mid way left
    // its lock; with no launch claimed here that may still run, preparing the
    // next launch clears it, and the start does not wait on it forever.
    #[test]
    fn a_host_launch_clears_a_killed_builds_lock() {
        let root = directory();
        let identity_dir = directory();
        let (executor, deployment, policy) = tensorfold_fixture(root.path(), identity_dir.path());
        let (launch, plan) = tensorfold_launch(&deployment, &policy, closed_port());
        let effective = executor.resolve(&launch).unwrap();
        let (input, _) = executor.tensorfold_plan(&effective, &plan, None).unwrap();
        let extension =
            std::path::Path::new(&input.extensions_dir.unwrap()).join("tensorfold_qmm_v5");
        std::fs::create_dir(&extension).unwrap();
        std::fs::write(extension.join("lock"), "").unwrap();
        assert!(matches!(
            executor.prepare(&effective, &plan, "toy", None),
            Ok(PreparedLaunch::Tensorfold(..))
        ));
        assert!(!extension.join("lock").exists());
    }

    // T41 (ADR 0023 §3): a host without a private cache root
    // refuses a new TensorFold launch, yet still resolves a retained one so
    // its Terminate can run.
    #[test]
    fn a_missing_cache_root_refuses_launch_but_not_retained() {
        let root = directory();
        let identity_dir = directory();
        let (executor, deployment, policy) = tensorfold_fixture(root.path(), identity_dir.path());
        std::fs::set_permissions(
            root.path().join("engines"),
            std::fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        let (launch, _) = tensorfold_launch(&deployment, &policy, closed_port());
        assert!(matches!(
            executor.resolve(&launch),
            Err(JournalError::Unauthorized)
        ));
        assert!(executor.resolve_retained(&launch).is_ok());
    }

    // T41 T21 (ADR 0023 §6): a Park of a TensorFold launch is refused by its
    // tier before anything is journaled.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tensorfold_park_is_refused_unchanged() {
        let root = directory();
        let identity_dir = directory();
        let (executor, deployment, policy) = tensorfold_fixture(root.path(), identity_dir.path());
        let (launch, _) = tensorfold_launch(&deployment, &policy, closed_port());
        let session = executor.journal.connect().unwrap();
        executor.connected(session).unwrap();
        executor
            .journal
            .accept(session, &launch, capyctl_protocol::now_unix_ms(), &Admit)
            .unwrap();
        let mut park = MemberCommand {
            identity: checkpoint_identity("park", "ready"),
            action: MemberAction::Park {
                owned_handle: "launch".into(),
            },
        };
        park.identity.payload_digest = park.canonical_digest();
        let refused = executor.execute(session, park).await.unwrap();
        assert_eq!(refused.refused, "residency_tier");
        assert_eq!(refused.residency.as_ref().unwrap().state, "unchanged");
    }

    // T41 (spec §5): before Terminate the host waits for TensorFold's own
    // counters; busy at the bound is not idle.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminate_waits_for_tensorfold_to_be_idle() {
        // A stub /health on the launch's service port, idle after two reads.
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = reads.clone();
        let app = axum::Router::new().route(
            "/health",
            axum::routing::get(move || {
                let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    axum::Json(serde_json::json!({
                        "ok": true, "busy": n < 2, "requests_running": if n < 2 { 1 } else { 0 }
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let root = directory();
        let identity_dir = directory();
        let (executor, deployment, policy) = tensorfold_fixture(root.path(), identity_dir.path());
        let (launch, _) = tensorfold_launch(&deployment, &policy, port);
        let deadline = capyctl_protocol::now_unix_ms() + 10_000;
        assert!(
            executor
                .tensorfold_idle_before_terminate(&launch, deadline)
                .await
        );
        assert!(reads.load(std::sync::atomic::Ordering::SeqCst) >= 3);
        // Busy for good: the wait ends at the command's remaining time.
        let short = capyctl_protocol::now_unix_ms() + 1_200;
        let app = axum::Router::new().route(
            "/health",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({"ok": true, "busy": true, "requests_running": 1}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let busy_port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (busy_launch, _) = tensorfold_launch(&deployment, &policy, busy_port);
        assert!(
            !executor
                .tensorfold_idle_before_terminate(&busy_launch, short)
                .await
        );
    }

    /// Renders a shell that stands in for the TensorFold process.
    struct Sleeper;
    impl LocalExecutionPolicy for Sleeper {
        fn authorize(&self, _: &MemberCommand) -> Result<(), JournalError> {
            Ok(())
        }
        fn render_launch(&self, _: &MemberCommand) -> Result<ApprovedLaunch, JournalError> {
            Ok(ApprovedLaunch {
                command: capyctl_adapters::traits::RenderedCommand {
                    argv: vec!["/bin/sh".into(), "-c".into(), "exec sleep 60".into()],
                    env: Default::default(),
                },
                descriptors: None,
            })
        }
    }

    /// Kills a stand-in process a test left running.
    struct KillOnDrop(u32);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            unsafe { libc::kill(self.0 as i32, libc::SIGKILL) };
        }
    }

    /// A `/health` on a fresh port: busy for good, or never answering.
    async fn health_stub(hung: bool) -> u16 {
        let app = axum::Router::new().route(
            "/health",
            axum::routing::get(move || async move {
                if hung {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                }
                axum::Json(serde_json::json!({"ok": true, "busy": true, "requests_running": 1}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        port
    }

    /// An engine port range every [`closed_port`] falls in.
    fn wide_ports() -> serde_json::Value {
        serde_json::json!({"start": 1024, "end": 65535})
    }

    /// A port nobody listens on.
    fn closed_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// A TensorFold launch on `port`, accepted with its stand-in process
    /// running and owned by the journal.
    async fn owned_tensorfold(
        port: u16,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        Arc<NativeHostExecution>,
        u64,
        capyctl_domain::completion::ProcessIdentity,
    ) {
        let root = directory();
        let identity_dir = directory();
        let (executor, deployment, policy) = tensorfold_fixture(root.path(), identity_dir.path());
        let (launch, plan) = tensorfold_launch(&deployment, &policy, port);
        let session = executor.journal.connect().unwrap();
        executor.connected(session).unwrap();
        let sleeper = Arc::new(Sleeper);
        let now = capyctl_protocol::now_unix_ms();
        let Acceptance::Fresh(ticket) = executor
            .journal
            .accept(session, &launch, now, sleeper.as_ref())
            .unwrap()
        else {
            panic!("a fresh launch");
        };
        let tools = executor
            .journal
            .launch_tools(ticket, now, sleeper.clone())
            .unwrap();
        let command = sleeper.render_launch(&launch).unwrap().command;
        let api = tools.spawn_durable(&plan.incarnation, &command).unwrap();
        (root, identity_dir, executor, session, api)
    }

    fn tensorfold_terminate(id: &str, deadline_in_ms: i64) -> MemberCommand {
        let mut terminate = MemberCommand {
            identity: checkpoint_identity(id, "retained"),
            action: MemberAction::Terminate {
                owned_handle: "launch".into(),
                recorded: Vec::new(),
            },
        };
        terminate.identity.deadline_ms = capyctl_protocol::now_unix_ms() + deadline_in_ms;
        terminate.identity.profile_fingerprint = "0.6.0".into();
        terminate.identity.payload_digest = terminate.canonical_digest();
        terminate
    }

    use capyctl_domain::completion::Presence;
    fn presence_of(api: &capyctl_domain::completion::ProcessIdentity) -> Presence {
        capyctl_launchers::process_absence::presence(api)
    }

    // T41 (spec §5, ADR 0023 §6): a TensorFold launch whose process is gone is
    // terminated at once, whatever answers on its old port.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_gone_tensorfold_launch_terminates_at_once() {
        let port = health_stub(false).await;
        let (_root, _ids, executor, session, api) = owned_tensorfold(port).await;
        unsafe { libc::kill(api.pid as i32, libc::SIGKILL) };
        let gone_by = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while presence_of(&api) != Presence::Gone {
            assert!(std::time::Instant::now() < gone_by, "the stand-in exits");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let started = std::time::Instant::now();
        let result = executor
            .execute(session, tensorfold_terminate("terminate", 20_000))
            .await
            .unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(result.state, "completed");
        assert!(!result.claim_retained);
    }

    // T41 (spec §5, ADR 0023 §6): an alive engine that does not listen (still
    // building, or never bound) serves nothing; it is signalled at once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tensorfold_launch_that_is_not_listening_terminates_at_once() {
        let (_root, _ids, executor, session, api) = owned_tensorfold(closed_port()).await;
        let _kill = KillOnDrop(api.pid);
        let started = std::time::Instant::now();
        let result = executor
            .execute(session, tensorfold_terminate("terminate", 20_000))
            .await
            .unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(result.state, "completed");
        assert_eq!(presence_of(&api), Presence::Gone);
    }

    // T41 (spec §5, ADR 0023 §6): a hung `/health`
    // is signalled once the bound passes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_hung_tensorfold_launch_terminates_at_the_bound() {
        let port = health_stub(true).await;
        let (_root, _ids, executor, session, api) = owned_tensorfold(port).await;
        let _kill = KillOnDrop(api.pid);
        let started = std::time::Instant::now();
        let result = executor
            .execute(session, tensorfold_terminate("terminate", 9_000))
            .await
            .unwrap();
        // The bound (the deadline less its margin), then the read in flight.
        assert!(started.elapsed() >= std::time::Duration::from_secs(5));
        assert_eq!(result.state, "completed");
        assert_eq!(presence_of(&api), Presence::Gone);
    }

    // T41 (spec §5, ADR 0023 §6): a TensorFold launch that still answers busy
    // at the bound is not signalled. The Terminate is answered unresolved
    // (never ending the session), is not journaled, and runs the gate again
    // when it is redelivered; the launch stays owned and alive.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_busy_tensorfold_terminate_signals_nothing() {
        let port = health_stub(false).await;
        let (_root, _ids, executor, session, api) = owned_tensorfold(port).await;
        let _kill = KillOnDrop(api.pid);
        let terminate = tensorfold_terminate("terminate", 2_500);
        for _ in 0..2 {
            let result = executor.execute(session, terminate.clone()).await.unwrap();
            assert_eq!(result.state, "accepted");
            assert_eq!(result.owned_handle, "launch");
            assert!(result.claim_retained);
            assert!(result.refused.is_empty());
            assert!(result.processes.iter().any(|p| p.presence == "alive"));
            capyctl_protocol::execution::validate_result(&terminate, &result).unwrap();
            assert!(matches!(
                executor.journal.precheck(session, &terminate),
                Ok(true)
            ));
        }
        assert_eq!(presence_of(&api), Presence::Alive);
        assert!(executor.journal.retained_command("launch").is_ok());
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
            let (executor, deployment, _) =
                sglang_fixture(root.path(), identity_dir.path(), "restart_only");
            sglang_runtime(root.path());
            let accepted = executor.profiles.accepted();
            let mut document = accepted.config.document.clone();
            if let Some(value) = policy_value {
                document["runtime_profiles"]["local"]["security"]["installation_drift"] =
                    value.into();
            }
            let config = HostConfig::parse(&document.to_string()).unwrap();
            let policy = capyctl_config::remote_resources::policy_fingerprint(&config.document);
            // Registered at agent start with a digest this installation no
            // longer measures to (the fixture's has no package tree at all).
            let mut inventory = accepted.inventory.clone();
            inventory.policy_fingerprint = policy.clone();
            inventory.profiles = vec![pb::RuntimeProfileStatus {
                name: "local".into(),
                installation_version: "0.5.20".into(),
                installation_digest: format!("sha256:{}", "a".repeat(64)),
                installation_state: "measured".into(),
                ..Default::default()
            }];
            executor
                .profiles
                .replace(crate::profiles::ProfileSet::new(config, inventory));
            let (mut launch, _) = launch_with(&deployment, &policy, "");
            launch.identity.payload_digest = launch.canonical_digest();
            let MemberAction::LaunchSingle(plan) = launch.action.clone() else {
                panic!("a launch");
            };
            assert_eq!(
                executor.admit_launch(&launch, &plan, GpuReading::Now),
                Err(LaunchVerdict::Refused(expected)),
                "{policy_value:?}"
            );
            let accepted = executor.profiles.accepted();
            let mut profiles = accepted.inventory.profiles.clone();
            accepted.installations.overlay(&mut profiles);
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
        let (executor, deployment, policy) =
            sglang_fixture(root.path(), identity_dir.path(), "deep");
        sglang_runtime(root.path());
        let (mut owner, _) = launch_with(&deployment, &policy, "");
        owner.identity.payload_digest = owner.canonical_digest();
        let MemberAction::LaunchSingle(plan) = owner.action.clone() else {
            panic!("a launch");
        };
        // The fixture's installation has no package tree: unmeasured, keyed "".
        executor
            .profiles
            .accepted()
            .installations
            .record_capabilities(&plan.profile_name, "", without_saver_hooks());
        assert_eq!(
            executor.admit_launch(&owner, &plan, GpuReading::Now),
            Err(LaunchVerdict::Refused("capability_missing:deep_park"))
        );

        let session = executor.journal.connect().unwrap();
        executor.connected(session).unwrap();
        executor
            .journal
            .accept(session, &owner, capyctl_protocol::now_unix_ms(), &Admit)
            .unwrap();
        let mut park = MemberCommand {
            identity: checkpoint_identity("park", "ready"),
            action: MemberAction::Park {
                owned_handle: "launch".into(),
            },
        };
        park.identity.payload_digest = park.canonical_digest();
        let refused = executor.execute(session, park.clone()).await.unwrap();
        capyctl_protocol::execution::validate_result(&park, &refused).unwrap();
        assert_eq!(refused.state, "completed");
        assert_eq!(refused.refused, "capability_missing:deep_park");
        assert_eq!(refused.residency.as_ref().unwrap().state, "unchanged");
        assert_eq!(
            executor.journal.history(0, 100).unwrap().len(),
            1,
            "the park is not journaled"
        );

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
            .profiles
            .accepted()
            .installations
            .record_capabilities(&plan.profile_name, "", without_saver_hooks());
        assert_eq!(
            restart.admit_launch(&launch, &plan, GpuReading::Now),
            Err(LaunchVerdict::Refused("checkpoint_mismatch"))
        );
    }

    /// ADR 0010 decision 5: `host_backed` resolves only where host and device
    /// memory are distinct.
    fn distinct_memory(document: &mut serde_json::Value) {
        document["resource_policy"]["domains"]["unified"]["memory"] = "distinct".into();
    }

    /// ADR 0008 (review decision, discrete GPU design §5): the launch-time
    /// capability gate covers both parking tiers. A `host_backed` launch parks
    /// with SGLang's memory saver and weights CPU backup, or vLLM's sleep mode,
    /// which the probe's `deep_park` capability covers; a build the probe found
    /// without it refuses the launch before any effect with the typed reason
    /// `capability_missing:deep_park` (its hint names `restart_only`). With
    /// the capability available the launch passes the gate and stops at the
    /// fixture's unrecorded checkpoint, the next gate.
    // T21 T22
    #[test]
    fn a_build_without_the_park_capability_refuses_a_host_backed_launch() {
        let report =
            |names: &[&str], missing: &str, label: &str| crate::installation::CapabilityReport {
                missing: names
                    .iter()
                    .map(|name| {
                        let labels = if *name == missing {
                            vec![label.to_owned()]
                        } else {
                            vec![]
                        };
                        ((*name).to_owned(), labels)
                    })
                    .collect(),
            };
        let sglang = ["core", "deep_park", "metrics", "observation"];
        let vllm = ["core", "deep_park", "metrics"];

        // SGLang: without the memory saver hooks, then with them.
        let root = directory();
        let identity_dir = directory();
        let (executor, deployment, policy) = sglang_fixture_with(
            root.path(),
            identity_dir.path(),
            "host_backed",
            distinct_memory,
        );
        sglang_runtime(root.path());
        let (mut launch, _) = launch_with(&deployment, &policy, "");
        launch.identity.payload_digest = launch.canonical_digest();
        let MemberAction::LaunchSingle(plan) = launch.action.clone() else {
            panic!("a launch");
        };
        assert_eq!(
            executor.resolve(&launch).unwrap().residency,
            capyctl_config::effective::Residency::HostBacked
        );
        let installations = executor.profiles.accepted().installations.clone();
        installations.record_capabilities(&plan.profile_name, "", without_saver_hooks());
        assert_eq!(
            executor.admit_launch(&launch, &plan, GpuReading::Now),
            Err(LaunchVerdict::Refused("capability_missing:deep_park"))
        );
        installations.record_capabilities(&plan.profile_name, "", report(&sglang, "", ""));
        assert_eq!(
            executor.admit_launch(&launch, &plan, GpuReading::Now),
            Err(LaunchVerdict::Refused("checkpoint_mismatch"))
        );

        // vLLM: without sleep mode, then with it.
        let root = directory();
        let identity_dir = directory();
        let (executor, mut deployment, policy) =
            checkpoint_fixture_with(root.path(), identity_dir.path(), distinct_memory);
        deployment["residency"] = "host_backed".into();
        let (mut launch, _) = launch_with(&deployment, &policy, "");
        launch.identity.payload_digest = launch.canonical_digest();
        let MemberAction::LaunchSingle(plan) = launch.action.clone() else {
            panic!("a launch");
        };
        assert_eq!(
            executor.resolve(&launch).unwrap().residency,
            capyctl_config::effective::Residency::HostBacked
        );
        let installations = executor.profiles.accepted().installations.clone();
        installations.record_capabilities(
            &plan.profile_name,
            "",
            report(&vllm, "deep_park", "destination:enable_sleep_mode"),
        );
        assert_eq!(
            executor.admit_launch(&launch, &plan, GpuReading::Now),
            Err(LaunchVerdict::Refused("capability_missing:deep_park"))
        );
        installations.record_capabilities(&plan.profile_name, "", report(&vllm, "", ""));
        assert_eq!(
            executor.admit_launch(&launch, &plan, GpuReading::Now),
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
                executor.admit_launch(&owner, &plan, GpuReading::Now),
                Err(LaunchVerdict::Refused("capability_missing:deep_park")),
                "{quantization}"
            );

            let session = executor.journal.connect().unwrap();
            executor.connected(session).unwrap();
            executor
                .journal
                .accept(session, &owner, capyctl_protocol::now_unix_ms(), &Admit)
                .unwrap();
            let mut park = MemberCommand {
                identity: checkpoint_identity("park", "ready"),
                action: MemberAction::Park {
                    owned_handle: "launch".into(),
                },
            };
            park.identity.payload_digest = park.canonical_digest();
            let refused = executor.execute(session, park.clone()).await.unwrap();
            capyctl_protocol::execution::validate_result(&park, &refused).unwrap();
            assert_eq!(
                refused.refused, "capability_missing:deep_park",
                "{quantization}"
            );
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
            restart.admit_launch(&launch, &plan, GpuReading::Now),
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
            vllm.admit_launch(&launch, &plan, GpuReading::Now),
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
        assert!(capyctl_protocol::execution::is_launch_failure_text(&text));
        assert_eq!(
            launch_failure(&result, None),
            "the engine exited before readiness"
        );
    }

    /// ADR 0028 §7 (owner decision 10): a group Prepare answers from this
    /// host's own groups policy, profile table and checkpoint cache. A failed
    /// check is a closed refusal in a valid completed result, a port held
    /// outside CapyCTL is named, and nothing is journaled or claimed.
    // T30 T29, Review Focus 2
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn group_prepare_refuses_with_closed_codes_and_journals_nothing() {
        use capyctl_domain::group::{
            member_id, GroupEngine, GroupTopology, MemberKey, MemberPlan, MemberRole,
        };
        use std::net::IpAddr;
        // A non-loopback IPv4 address this machine holds stands for host A's
        // peer address; without one only the address refusal is exercised.
        let local = crate::host_checks::read_host_facts(std::path::Path::new("/"))
            .local_addresses
            .into_iter()
            .find(|a| a.is_ipv4() && !a.is_loopback() && !a.is_unspecified());
        let peer = local.unwrap_or_else(|| "192.0.2.10".parse().unwrap());
        let root = directory();
        let identity_dir = directory();
        let (executor, _, _) =
            checkpoint_fixture_with(root.path(), identity_dir.path(), |document| {
                document["resource_policy"]["groups"] =
                    serde_json::json!({"peer_address": peer.to_string()});
            });
        let models = root.path().join("models");
        let digest = executor
            .checkpoints
            .measure(&models, &models.join("toy"))
            .unwrap()
            .manifest
            .digest;
        let model_path = models.join("toy").to_string_lossy().into_owned();
        let plan = |peer: IpAddr, digest: &str, rendezvous: u16| {
            let member = |rank: u32, host: &str, address: IpAddr| MemberPlan {
                member: MemberKey {
                    host_id: host.into(),
                    member_id: member_id(rank),
                },
                rank,
                role: if rank == 0 {
                    MemberRole::Head
                } else {
                    MemberRole::Worker
                },
                profile_name: "local".into(),
                profile_fingerprint: "vllm-build-1".into(),
                checkpoint_fingerprint: digest.into(),
                model_path: model_path.clone(),
                devices: vec!["gpu0".into()],
                peer_address: address,
                service_port: (rank == 0).then(closed_port),
                worker_port: None,
            };
            GroupPlan::new(
                GroupEngine::Vllm,
                vec![
                    member(0, "host", peer),
                    member(1, "host-b", "198.51.100.11".parse().unwrap()),
                ],
                GroupTopology {
                    tensor_parallel: 2,
                    pipeline_parallel: 1,
                    local_ranks: 1,
                },
                rendezvous,
                1,
            )
            .unwrap()
        };
        let run = |plan: GroupPlan| {
            let executor = executor.clone();
            async move {
                let mut command = MemberCommand {
                    identity: checkpoint_identity("prepare", "reserved"),
                    action: MemberAction::Prepare(plan),
                };
                command.identity.payload_digest = command.canonical_digest();
                let result = executor.execute(1, command.clone()).await.unwrap();
                capyctl_protocol::execution::validate_result(&command, &result).unwrap();
                result.refused
            }
        };
        let free = closed_port();
        assert_eq!(
            run(plan("192.0.2.200".parse().unwrap(), &digest, free)).await,
            "peer_address_not_local"
        );
        if local.is_some() {
            let other = format!("sha256:{}", "0".repeat(64));
            assert_eq!(
                run(plan(peer, &other, free)).await,
                "group_checkpoint_mismatch"
            );
            // Review Focus 2: held on the wildcard by something else.
            let holder = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
            let held = holder.local_addr().unwrap().port();
            assert_eq!(
                run(plan(peer, &digest, held)).await,
                format!("rendezvous_port_in_use:{held}")
            );
            drop(holder);
            // Without require_rdma the tuning only warns: the member passes.
            assert_eq!(run(plan(peer, &digest, held)).await, "");
        }
        assert!(executor.journal.claimed_launches("").unwrap().is_empty());
        // Another controller's Prepare is not answered at all.
        let mut foreign = MemberCommand {
            identity: checkpoint_identity("prepare", "reserved"),
            action: MemberAction::Prepare(plan(peer, &digest, free)),
        };
        foreign.identity.controller_id = "other".into();
        foreign.identity.payload_digest = foreign.canonical_digest();
        assert!(executor.execute(1, foreign).await.is_err());
    }

    /// ADR 0028 §3, §14: only a host with a peer address publishes group
    /// inventory; an address no local interface holds is a finding.
    // T39 T29
    #[test]
    fn group_inventory_is_published_only_with_a_peer_address() {
        let root = directory();
        let mut document: serde_json::Value =
            serde_json::from_str(&HostConfig::template(root.path())).unwrap();
        assert_eq!(group_inventory(&document), None);
        document["resource_policy"]["groups"] = serde_json::json!({"require_rdma": true});
        assert_eq!(group_inventory(&document), None);
        document["resource_policy"]["groups"]["peer_address"] = "192.0.2.10".into();
        let group = group_inventory(&document).unwrap();
        assert_eq!(group.peer_address, "192.0.2.10");
        assert!(group
            .findings
            .contains(&"peer_address_not_local".to_string()));
        assert!(group.findings.iter().all(|finding| {
            finding == "peer_address_not_local"
                || finding == "host_tuning_warning:compaction"
                || finding.starts_with("host_tuning_missing:")
        }));
    }
}

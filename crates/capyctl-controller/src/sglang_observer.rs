//! Production runtime residency observation.
//!
//! An observation is a fusion, not a sensor reading. Validity of weights and cache
//! contents cannot be read from the allocator: SPEC §9.1 and the measured
//! 2026-09-16 qualification both show that resuming an allocation restores the
//! mapping while leaving its contents unusable. Those facts therefore come from
//! committed lifecycle milestones. Fresh local physical facts are used only to
//! *falsify* a committed fact, never to substitute for one, and quiescence comes
//! from the controller's own request leases rather than from the engine.
//!
//! Every source must answer. A source that cannot be read sets `unknown_work`,
//! which closes every action, rather than reporting a convenient default.
//!
//! The fusion itself is engine-neutral: it knows about allocations, milestones,
//! leases and process liveness, none of which are SGLang concepts. Only the thin
//! binding at the end of this module is SGLang-specific, so a second engine adds
//! a binding rather than a second copy of these rules.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use capyctl_adapters::sglang::{SglangRuntimeObservation, SglangRuntimeObserver};
use capyctl_adapters::traits::RuntimeError;
use capyctl_domain::completion::{Milestone, ProcessIdentity, TransitionToken};
use capyctl_launchers::native_observation::{AllocationFacts, AllocationTag};

/// A source could not be read. It carries no detail on purpose: the observer
/// treats every unreadable source identically, as unknown rather than absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("observation source unavailable")]
pub struct SourceUnavailable;

type Observed<T> = Result<T, SourceUnavailable>;

/// Committed lifecycle milestones for this incarnation, most recent last.
/// The implementation must read persisted state, never a cached summary.
pub trait CommittedFacts: Send + Sync {
    fn facts(&self, binding_id: &str, incarnation: &str) -> Observed<Vec<Milestone>>;
}

/// Request leases the controller still owns for this deployment. Zero outstanding
/// leases is the only evidence of quiescence this observer accepts; the engine's
/// own view of its queue is not a substitute.
pub trait RegisteredWork: Send + Sync {
    fn outstanding(&self, deployment_id: &str) -> Observed<usize>;
}

/// Fresh saver-map facts from the enrolled runtime.
pub trait PhysicalFacts: Send + Sync {
    fn observe(&self, timeout: Duration) -> Observed<AllocationFacts>;
}

/// Liveness of the persisted identity set in the collector's PID namespace.
pub trait GroupLiveness: Send + Sync {
    fn live(&self, expected: &[ProcessIdentity]) -> Observed<Vec<ProcessIdentity>>;
}

const PHYSICAL_TIMEOUT: Duration = Duration::from_millis(500);

/// Engine-neutral residency facts for one runtime binding at one instant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeResidency {
    pub identities: Vec<ProcessIdentity>,
    pub real_memory_saver: bool,
    pub quiesced: bool,
    pub unknown_work: bool,
    pub allocations: bool,
    pub weights: bool,
    pub cache: bool,
}

/// Bound to one immutable runtime binding and one coordinator step. The token is
/// held because the adapter's observer hook takes no arguments and the adapter
/// requires the observation to carry the current step's token.
pub struct NativeResidencyObserver {
    token: TransitionToken,
    binding_id: String,
    incarnation: String,
    expected_identities: Vec<ProcessIdentity>,
    expected_saver_sha256: String,
    /// The launch's GPU count, one rank per GPU on CUDA ordinals `0..devices`.
    devices: u32,
    committed: Arc<dyn CommittedFacts>,
    work: Arc<dyn RegisteredWork>,
    physical: Arc<dyn PhysicalFacts>,
    liveness: Arc<dyn GroupLiveness>,
}

/// The most GPUs one launch may span. A larger count is not a launch this
/// observer can judge, so construction refuses it.
const MAX_DEVICES: u32 = 64;

/// Residency implied by committed milestones since the most recent release.
#[derive(Debug, Default, PartialEq, Eq)]
struct Residency {
    allocations: bool,
    weights: bool,
    cache: bool,
    quiesced: bool,
}

/// Milestones before the latest `MemoryReleased` describe a runtime that no longer
/// exists, so the fold restarts there. `Quiesced` is advisory: a later committed
/// drain does not keep the runtime idle, so live leases still override it.
fn residency(facts: &[Milestone]) -> Residency {
    let start = facts
        .iter()
        .rposition(|f| *f == Milestone::MemoryReleased)
        .map_or(0, |i| i + 1);
    let mut r = Residency::default();
    for fact in &facts[start..] {
        match fact {
            Milestone::MemoryReleased => r = Residency::default(),
            Milestone::AllocationsRestored => r.allocations = true,
            Milestone::WeightsUsable => r.weights = true,
            Milestone::CacheValid => r.cache = true,
            Milestone::Quiesced => r.quiesced = true,
            Milestone::ModelUsable => {}
        }
    }
    r
}

/// Bytes the saver currently has mapped on one device. Paused allocations are
/// mapped in the saver's table but hold no device memory, so they are not
/// residency.
#[derive(Clone, Copy, Debug, Default)]
struct DeviceMapped {
    weights: u64,
    cache: u64,
    seen: bool,
}

impl DeviceMapped {
    fn add(&mut self, tag: &AllocationTag, bytes: u64) {
        match tag {
            AllocationTag::Weights => self.weights += bytes,
            AllocationTag::KvCache => self.cache += bytes,
        }
    }

    fn any(&self) -> bool {
        self.weights > 0 || self.cache > 0
    }
}

/// Mapped bytes per device, and whether the map covers exactly the launch's
/// devices.
///
/// ADR 0035 §9: summing across devices is unsound under tensor parallelism, since
/// a non-zero sum can mean one rank restored while another stayed dark. Each rank
/// runs on CUDA ordinal `r` of the launch's own GPUs, so the devices in the map
/// must be exactly `0..devices`: a missing device is unread evidence and an extra
/// one is not this launch's, and both report unknown.
///
/// One device keeps the rule that predates ADR 0035 unchanged: the map may name at
/// most one device, whatever its ordinal, and an empty map reads as nothing mapped.
fn mapped(facts: &AllocationFacts, devices: u32) -> (Vec<DeviceMapped>, bool) {
    let groups = &facts.allocations.groups;
    if devices == 1 {
        let mut only = DeviceMapped::default();
        for g in groups {
            only.add(&g.tag, g.mapped_bytes);
        }
        let one = groups.iter().all(|g| g.device == groups[0].device);
        return (vec![only], one);
    }
    let mut per = vec![DeviceMapped::default(); devices as usize];
    let mut complete = true;
    for g in groups {
        match usize::try_from(g.device).ok().and_then(|i| per.get_mut(i)) {
            Some(device) => {
                device.seen = true;
                device.add(&g.tag, g.mapped_bytes);
            }
            None => complete = false,
        }
    }
    complete &= per.iter().all(|d| d.seen);
    (per, complete)
}

impl NativeResidencyObserver {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        token: TransitionToken,
        binding_id: String,
        incarnation: String,
        expected_identities: Vec<ProcessIdentity>,
        expected_saver_sha256: String,
        devices: u32,
        committed: Arc<dyn CommittedFacts>,
        work: Arc<dyn RegisteredWork>,
        physical: Arc<dyn PhysicalFacts>,
        liveness: Arc<dyn GroupLiveness>,
    ) -> Result<Self, RuntimeError> {
        if binding_id.is_empty()
            || incarnation.is_empty()
            || expected_identities.is_empty()
            || expected_saver_sha256.len() != 64
            || !expected_saver_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || devices == 0
            || devices > MAX_DEVICES
        {
            return Err(RuntimeError::Unsupported);
        }
        Ok(Self {
            token,
            binding_id,
            incarnation,
            expected_identities,
            expected_saver_sha256,
            devices,
            committed,
            work,
            physical,
            liveness,
        })
    }

    pub fn residency(&self) -> RuntimeResidency {
        let mut unknown_work = false;
        let mut note = |ok: bool| {
            unknown_work |= !ok;
        };

        let facts = self.committed.facts(&self.binding_id, &self.incarnation);
        note(facts.is_ok());
        let residency = residency(facts.as_deref().unwrap_or(&[]));

        let physical = self.physical.observe(PHYSICAL_TIMEOUT);
        note(physical.is_ok());

        // A committed fact is only trusted while the allocator agrees that memory
        // is actually mapped. Disagreement is never resolved in favour of either
        // side: it is reported as unknown so every action stays closed.
        let (mut allocations, mut weights, mut cache) = (false, false, false);
        if let Ok(observed) = &physical {
            if observed.binding_id != self.binding_id || observed.incarnation_id != self.incarnation
            {
                note(false);
            }
            note(observed.library_sha256 == self.expected_saver_sha256);
            // ADR 0035 §9: the committed residency must agree with every device.
            // A park settles only when every device is unmapped, a wake only when
            // every device is mapped; any device missing or disagreeing is unknown.
            let (per_device, complete) = mapped(observed, self.devices);
            note(complete);
            let every = per_device.iter().all(DeviceMapped::any);
            let some = per_device.iter().any(DeviceMapped::any);
            if every != some {
                note(false);
            }
            if residency.allocations != every {
                note(false);
            }
            allocations = residency.allocations && every;
            // Contents are never physically provable; the committed milestone is
            // authoritative and the mapping is only a necessary condition.
            let all_weights = per_device.iter().all(|d| d.weights > 0);
            let all_cache = per_device.iter().all(|d| d.cache > 0);
            weights = residency.weights && all_weights;
            cache = residency.cache && all_cache;
            if residency.weights && !all_weights {
                note(false);
            }
            if residency.cache && !all_cache {
                note(false);
            }
        }

        let live = self.liveness.live(&self.expected_identities);
        note(live.is_ok());
        let identities = live.unwrap_or_default();

        let outstanding = self.work.outstanding(&self.token.deployment_id);
        note(outstanding.is_ok());
        let quiesced = outstanding.is_ok_and(|count| count == 0);

        RuntimeResidency {
            identities,
            real_memory_saver: physical
                .is_ok_and(|o| o.library_sha256 == self.expected_saver_sha256),
            quiesced,
            unknown_work,
            allocations,
            weights,
            cache,
        }
    }
}

/// SGLang binding. Adapting engine-neutral residency to one engine's observation
/// type is the only engine-specific step; a second engine adds its own binding.
pub struct NativeSglangObserver(pub NativeResidencyObserver);

#[async_trait]
impl SglangRuntimeObserver for NativeSglangObserver {
    async fn observe(&self) -> Result<SglangRuntimeObservation, RuntimeError> {
        let inner = &self.0;
        let r = inner.residency();
        Ok(SglangRuntimeObservation {
            token: inner.token.clone(),
            binding_id: inner.binding_id.clone(),
            incarnation: inner.incarnation.clone(),
            identities: r.identities,
            real_memory_saver: r.real_memory_saver,
            quiesced: r.quiesced,
            unknown_work: r.unknown_work,
            allocations: r.allocations,
            weights: r.weights,
            cache: r.cache,
        })
    }
}

#[cfg(test)]
mod tests;

/// Store-backed residency sources.
///
/// The lock is held only for the duration of one read. Both reads are plain
/// queries, so they can run while a coordinator step is in flight without
/// serialising against it for longer than the query itself.
pub struct StoreSources {
    store: Arc<std::sync::Mutex<capyctl_store::Store>>,
    deployment_id: String,
}

impl StoreSources {
    pub fn new(
        store: Arc<std::sync::Mutex<capyctl_store::Store>>,
        deployment_id: String,
    ) -> Result<Self, RuntimeError> {
        if deployment_id.is_empty() {
            return Err(RuntimeError::Unsupported);
        }
        Ok(Self {
            store,
            deployment_id,
        })
    }
}

impl CommittedFacts for StoreSources {
    fn facts(&self, binding_id: &str, incarnation: &str) -> Observed<Vec<Milestone>> {
        self.store
            .lock()
            .map_err(|_| SourceUnavailable)?
            .committed_milestones(binding_id, incarnation)
            .map_err(|_| SourceUnavailable)
    }
}

impl RegisteredWork for StoreSources {
    fn outstanding(&self, deployment_id: &str) -> Observed<usize> {
        // The observer passes the token's deployment. A mismatch means this source
        // was bound to a different deployment than the step being executed, which
        // is unknown rather than zero outstanding work.
        if deployment_id != self.deployment_id {
            return Err(SourceUnavailable);
        }
        self.store
            .lock()
            .map_err(|_| SourceUnavailable)?
            .outstanding_requests(deployment_id)
            .map_err(|_| SourceUnavailable)
    }
}

/// Saver-map facts from the enrolled runtime's observation socket.
pub struct NativeSaverFacts(pub capyctl_launchers::native_observation::NativeObservationClient);

impl PhysicalFacts for NativeSaverFacts {
    fn observe(&self, timeout: Duration) -> Observed<AllocationFacts> {
        self.0.observe(timeout).map_err(|_| SourceUnavailable)
    }
}

/// Liveness of the persisted identity set, anchored to the API process.
///
/// `observe_process_group` deliberately infers no engine roles, so this matches the
/// expected identities against observed kernel facts by process identity alone and
/// returns only those actually found. A missing rank shortens the returned set, and
/// the adapter rejects the observation because it no longer matches what the step
/// expects. Absence therefore fails closed rather than being reported as liveness.
pub struct NativeLiveness {
    api: ProcessIdentity,
}

impl NativeLiveness {
    pub fn new(api: ProcessIdentity) -> Result<Self, RuntimeError> {
        if api.role != "api" || api.pid == 0 || api.start_ticks == 0 || api.boot_id.is_empty() {
            return Err(RuntimeError::Unsupported);
        }
        Ok(Self { api })
    }
}

impl GroupLiveness for NativeLiveness {
    fn live(&self, expected: &[ProcessIdentity]) -> Observed<Vec<ProcessIdentity>> {
        let observed = capyctl_launchers::group_observation::observe_process_group(&self.api)
            .map_err(|_| SourceUnavailable)?;
        let members = observed
            .members()
            .iter()
            .map(|m| (m.pid, m.boot_id.as_str(), m.start_ticks))
            .collect::<std::collections::BTreeSet<_>>();
        Ok(expected
            .iter()
            .filter(|i| members.contains(&(i.pid, i.boot_id.as_str(), i.start_ticks)))
            .cloned()
            .collect())
    }
}

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
use mllm_adapters::sglang::{SglangRuntimeObservation, SglangRuntimeObserver};
use mllm_adapters::traits::RuntimeError;
use mllm_domain::completion::{Milestone, ProcessIdentity, TransitionToken};
use mllm_launchers::native_observation::{AllocationFacts, AllocationTag};

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
    committed: Arc<dyn CommittedFacts>,
    work: Arc<dyn RegisteredWork>,
    physical: Arc<dyn PhysicalFacts>,
    liveness: Arc<dyn GroupLiveness>,
}

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

/// Bytes the saver currently has mapped for a tag. Paused allocations are mapped
/// in the saver's table but hold no device memory, so they are not residency.
fn mapped(facts: &AllocationFacts, tag: AllocationTag) -> u64 {
    facts
        .allocations
        .groups
        .iter()
        .filter(|g| g.tag == tag)
        .map(|g| g.mapped_bytes)
        .sum()
}

impl NativeResidencyObserver {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        token: TransitionToken,
        binding_id: String,
        incarnation: String,
        expected_identities: Vec<ProcessIdentity>,
        expected_saver_sha256: String,
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
        {
            return Err(RuntimeError::Unsupported);
        }
        Ok(Self {
            token,
            binding_id,
            incarnation,
            expected_identities,
            expected_saver_sha256,
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
            let weight_bytes = mapped(observed, AllocationTag::Weights);
            let cache_bytes = mapped(observed, AllocationTag::KvCache);
            let any = weight_bytes > 0 || cache_bytes > 0;
            if residency.allocations != any {
                note(false);
            }
            allocations = residency.allocations && any;
            // Contents are never physically provable; the committed milestone is
            // authoritative and the mapping is only a necessary condition.
            weights = residency.weights && weight_bytes > 0;
            cache = residency.cache && cache_bytes > 0;
            if residency.weights && weight_bytes == 0 {
                note(false);
            }
            if residency.cache && cache_bytes == 0 {
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

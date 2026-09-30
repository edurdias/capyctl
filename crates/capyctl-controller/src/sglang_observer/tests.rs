use super::*;
use capyctl_launchers::native_observation::{AllocationGroup, Allocations};

const SAVER: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";
const OTHER: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

fn token() -> TransitionToken {
    TransitionToken {
        deployment_id: "dep-1".into(),
        revision: 1,
        generation: 1,
        operation_id: "op-1".into(),
        step_id: "step-1".into(),
    }
}

fn identities() -> Vec<ProcessIdentity> {
    vec![
        ProcessIdentity {
            role: "api".into(),
            pid: 100,
            boot_id: "boot".into(),
            start_ticks: 10,
        },
        ProcessIdentity {
            role: "worker-0".into(),
            pid: 101,
            boot_id: "boot".into(),
            start_ticks: 11,
        },
    ]
}

fn group(tag: AllocationTag, mapped_bytes: u64) -> AllocationGroup {
    AllocationGroup {
        device: 0,
        tag,
        allocation_count: 1,
        active_count: u64::from(mapped_bytes > 0),
        paused_count: u64::from(mapped_bytes == 0),
        virtual_bytes: 8 << 30,
        mapped_bytes,
        backup_bytes: 0,
        backup_enabled_count: 0,
    }
}

fn facts_with(weights: u64, kv: u64, saver: &str) -> AllocationFacts {
    let groups = vec![
        group(AllocationTag::Weights, weights),
        group(AllocationTag::KvCache, kv),
    ];
    AllocationFacts {
        binding_id: "bind-1".into(),
        incarnation_id: "inc-1".into(),
        request_id: "req-1".into(),
        owner: identities()[0].clone(),
        library_sha256: saver.into(),
        started_ns: 1,
        finished_ns: 2,
        allocations: Allocations {
            allocation_count: 2,
            virtual_bytes: 16 << 30,
            mapped_bytes: weights + kv,
            backup_bytes: 0,
            groups,
        },
    }
}

struct Facts(Observed<Vec<Milestone>>);
impl CommittedFacts for Facts {
    fn facts(&self, _: &str, _: &str) -> Observed<Vec<Milestone>> {
        self.0.clone()
    }
}
struct Work(Observed<usize>);
impl RegisteredWork for Work {
    fn outstanding(&self, _: &str) -> Observed<usize> {
        self.0
    }
}
/// Rebuilds facts on every call: the observer must re-read physical state for
/// each observation, so a one-shot double would hide that requirement.
struct Physical(Option<(u64, u64, String)>);
impl PhysicalFacts for Physical {
    fn observe(&self, _: Duration) -> Observed<AllocationFacts> {
        match &self.0 {
            Some((w, kv, saver)) => Ok(facts_with(*w, *kv, saver)),
            None => Err(SourceUnavailable),
        }
    }
}
struct Live(Observed<Vec<ProcessIdentity>>);
impl GroupLiveness for Live {
    fn live(&self, _: &[ProcessIdentity]) -> Observed<Vec<ProcessIdentity>> {
        self.0.clone()
    }
}

fn observer(
    milestones: Observed<Vec<Milestone>>,
    physical: Option<(u64, u64, String)>,
    outstanding: Observed<usize>,
    live: Observed<Vec<ProcessIdentity>>,
) -> NativeResidencyObserver {
    NativeResidencyObserver::new(
        token(),
        "bind-1".into(),
        "inc-1".into(),
        identities(),
        SAVER.into(),
        Arc::new(Facts(milestones)),
        Arc::new(Work(outstanding)),
        Arc::new(Physical(physical)),
        Arc::new(Live(live)),
    )
    .unwrap()
}

fn ready() -> NativeResidencyObserver {
    observer(
        Ok(vec![
            Milestone::AllocationsRestored,
            Milestone::WeightsUsable,
            Milestone::CacheValid,
        ]),
        Some((8 << 30, 4 << 30, SAVER.into())),
        Ok(0),
        Ok(identities()),
    )
}

#[test]
fn ready_runtime_reports_full_residency() {
    let o = ready().residency();
    assert!(o.allocations && o.weights && o.cache);
    assert!(o.quiesced && !o.unknown_work && o.real_memory_saver);
    assert_eq!(o.identities, identities());
}

/// The measured 2026-09-16 qualification: resume restores the mapping but not the
/// contents, so `weights` must stay false until a reload is committed. Reporting
/// it true here would let the adapter skip the reload and serve garbage.
#[test]
fn resumed_allocations_do_not_imply_usable_weights() {
    let o = observer(
        Ok(vec![Milestone::AllocationsRestored]),
        Some((8 << 30, 4 << 30, SAVER.into())),
        Ok(0),
        Ok(identities()),
    )
    .residency();
    assert!(o.allocations, "resume restored the mapping");
    assert!(!o.weights, "contents are not restored by resume");
    assert!(!o.cache);
    assert!(!o.unknown_work);
}

#[test]
fn released_runtime_reports_no_residency() {
    let o = observer(
        Ok(vec![
            Milestone::AllocationsRestored,
            Milestone::WeightsUsable,
            Milestone::CacheValid,
            Milestone::Quiesced,
            Milestone::MemoryReleased,
        ]),
        Some((0, 0, SAVER.into())),
        Ok(0),
        Ok(identities()),
    )
    .residency();
    assert!(!o.allocations && !o.weights && !o.cache);
    assert!(!o.unknown_work);
}

/// Milestones from before the last release describe a runtime that is gone.
#[test]
fn facts_before_release_do_not_survive_it() {
    let o = observer(
        Ok(vec![
            Milestone::WeightsUsable,
            Milestone::CacheValid,
            Milestone::MemoryReleased,
            Milestone::AllocationsRestored,
        ]),
        Some((8 << 30, 4 << 30, SAVER.into())),
        Ok(0),
        Ok(identities()),
    )
    .residency();
    assert!(o.allocations);
    assert!(
        !o.weights,
        "pre-release weights validity must not carry over"
    );
    assert!(!o.cache);
}

#[test]
fn outstanding_leases_deny_quiescence() {
    let o = observer(
        Ok(vec![Milestone::AllocationsRestored, Milestone::Quiesced]),
        Some((8 << 30, 4 << 30, SAVER.into())),
        Ok(3),
        Ok(identities()),
    )
    .residency();
    assert!(
        !o.quiesced,
        "a committed drain cannot outrank live request leases"
    );
    assert!(!o.unknown_work);
}

#[test]
fn unreadable_source_sets_unknown_work() {
    for o in [
        observer(
            Err(SourceUnavailable),
            Some((8 << 30, 4 << 30, SAVER.into())),
            Ok(0),
            Ok(identities()),
        ),
        observer(
            Ok(vec![Milestone::AllocationsRestored]),
            None,
            Ok(0),
            Ok(identities()),
        ),
        observer(
            Ok(vec![Milestone::AllocationsRestored]),
            Some((8 << 30, 4 << 30, SAVER.into())),
            Err(SourceUnavailable),
            Ok(identities()),
        ),
        observer(
            Ok(vec![Milestone::AllocationsRestored]),
            Some((8 << 30, 4 << 30, SAVER.into())),
            Ok(0),
            Err(SourceUnavailable),
        ),
    ] {
        assert!(o.residency().unknown_work, "every source must answer");
    }
}

/// A committed fact the allocator contradicts is never resolved in either
/// direction; the disagreement itself closes every action.
#[test]
fn committed_residency_without_mapped_memory_is_unknown() {
    let o = observer(
        Ok(vec![
            Milestone::AllocationsRestored,
            Milestone::WeightsUsable,
        ]),
        Some((0, 0, SAVER.into())),
        Ok(0),
        Ok(identities()),
    )
    .residency();
    assert!(o.unknown_work);
    assert!(!o.allocations && !o.weights);
}

#[test]
fn mapped_memory_without_a_committed_fact_is_unknown() {
    let o = observer(
        Ok(vec![]),
        Some((8 << 30, 0, SAVER.into())),
        Ok(0),
        Ok(identities()),
    )
    .residency();
    assert!(o.unknown_work);
    assert!(!o.allocations);
}

#[test]
fn foreign_saver_library_denies_real_memory_saver() {
    let o = observer(
        Ok(vec![Milestone::AllocationsRestored]),
        Some((8 << 30, 4 << 30, OTHER.into())),
        Ok(0),
        Ok(identities()),
    )
    .residency();
    assert!(!o.real_memory_saver, "the no-op saver must never qualify");
    assert!(o.unknown_work);
}

#[test]
fn facts_from_another_binding_or_incarnation_are_unknown() {
    struct Foreign;
    impl PhysicalFacts for Foreign {
        fn observe(&self, _: Duration) -> Observed<AllocationFacts> {
            let mut wrong = facts_with(8 << 30, 4 << 30, SAVER);
            wrong.binding_id = "bind-2".into();
            Ok(wrong)
        }
    }
    let o = NativeResidencyObserver::new(
        token(),
        "bind-1".into(),
        "inc-1".into(),
        identities(),
        SAVER.into(),
        Arc::new(Facts(Ok(vec![Milestone::AllocationsRestored]))),
        Arc::new(Work(Ok(0))),
        Arc::new(Foreign),
        Arc::new(Live(Ok(identities()))),
    )
    .unwrap()
    .residency();
    assert!(o.unknown_work);
}

#[test]
fn construction_rejects_a_malformed_saver_digest() {
    for digest in ["", "abc", &"g".repeat(64), &"A".repeat(64)] {
        assert!(NativeResidencyObserver::new(
            token(),
            "bind-1".into(),
            "inc-1".into(),
            identities(),
            digest.into(),
            Arc::new(Facts(Ok(vec![]))),
            Arc::new(Work(Ok(0))),
            Arc::new(Physical(None)),
            Arc::new(Live(Ok(vec![]))),
        )
        .is_err());
    }
}

#[test]
fn residency_fold_restarts_at_the_latest_release() {
    assert_eq!(
        residency(&[
            Milestone::AllocationsRestored,
            Milestone::WeightsUsable,
            Milestone::MemoryReleased,
        ]),
        Residency::default()
    );
}

/// The engine binding must not reinterpret residency. If a second engine is added,
/// this is the property that keeps the rules in one place.
#[tokio::test]
async fn sglang_binding_carries_residency_through_unchanged() {
    let inner = observer(
        Ok(vec![Milestone::AllocationsRestored]),
        Some((8 << 30, 4 << 30, SAVER.into())),
        Ok(0),
        Ok(identities()),
    );
    let expected = inner.residency();
    let observed = NativeSglangObserver(inner).observe().await.unwrap();
    assert_eq!(observed.allocations, expected.allocations);
    assert_eq!(observed.weights, expected.weights);
    assert_eq!(observed.cache, expected.cache);
    assert_eq!(observed.quiesced, expected.quiesced);
    assert_eq!(observed.unknown_work, expected.unknown_work);
    assert_eq!(observed.real_memory_saver, expected.real_memory_saver);
    assert_eq!(observed.identities, expected.identities);
    assert_eq!(observed.token, token());
    assert_eq!(observed.binding_id, "bind-1");
}

/// Tensor parallelism is not in scope for the pinned recipe. A runtime spanning
/// devices must read as unknown rather than letting one restored rank stand in for
/// the whole group.
#[test]
fn multi_device_residency_is_unknown() {
    struct TwoDevices;
    impl PhysicalFacts for TwoDevices {
        fn observe(&self, _: Duration) -> Observed<AllocationFacts> {
            let mut facts = facts_with(8 << 30, 4 << 30, SAVER);
            let mut second = group(AllocationTag::Weights, 8 << 30);
            second.device = 1;
            facts.allocations.groups.push(second);
            Ok(facts)
        }
    }
    let o = NativeResidencyObserver::new(
        token(),
        "bind-1".into(),
        "inc-1".into(),
        identities(),
        SAVER.into(),
        Arc::new(Facts(Ok(vec![Milestone::AllocationsRestored]))),
        Arc::new(Work(Ok(0))),
        Arc::new(TwoDevices),
        Arc::new(Live(Ok(identities()))),
    )
    .unwrap()
    .residency();
    assert!(o.unknown_work, "multi-rank evidence is not implemented");
}

/// A source bound to one deployment must never answer for another. Returning zero
/// would report quiescence the controller has not established.
#[test]
fn store_sources_reject_a_foreign_deployment() {
    let store = Arc::new(std::sync::Mutex::new(
        capyctl_store::Store::open_in_memory().unwrap(),
    ));
    let sources = StoreSources::new(store, "dep-1".into()).unwrap();
    assert_eq!(sources.outstanding("dep-2"), Err(SourceUnavailable));
    assert!(StoreSources::new(
        Arc::new(std::sync::Mutex::new(
            capyctl_store::Store::open_in_memory().unwrap()
        )),
        String::new()
    )
    .is_err());
}

/// An unknown binding is unavailable, not an empty history: empty would read as a
/// released runtime and permit a restore that has no committed basis.
#[test]
fn store_sources_report_an_unknown_binding_as_unavailable() {
    let store = Arc::new(std::sync::Mutex::new(
        capyctl_store::Store::open_in_memory().unwrap(),
    ));
    let sources = StoreSources::new(store, "dep-1".into()).unwrap();
    assert_eq!(sources.facts("bind-x", "inc-x"), Err(SourceUnavailable));
}

#[test]
fn liveness_requires_an_api_anchor() {
    let mut worker = identities()[1].clone();
    assert!(
        NativeLiveness::new(worker.clone()).is_err(),
        "role must be api"
    );
    worker.role = "api".into();
    worker.pid = 0;
    assert!(NativeLiveness::new(worker).is_err());
    assert!(NativeLiveness::new(identities()[0].clone()).is_ok());
}

/// A process that no longer exists must shorten the live set rather than be assumed
/// present; the adapter then rejects the observation on arity.
#[test]
fn liveness_of_a_dead_anchor_is_unavailable() {
    let mut api = identities()[0].clone();
    api.pid = 0x7FFF_FFFE; // not a live pid in this namespace
    api.boot_id = "00000000-0000-0000-0000-000000000000".into();
    let liveness = NativeLiveness::new(api).unwrap();
    assert_eq!(liveness.live(&identities()), Err(SourceUnavailable));
}

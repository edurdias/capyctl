//! Ordinary lifecycle coverage: qualified start receipts, initialize, unarmed
//! stop, deadline expiry and ordinary cleanup, all against managed deployments
//! created by the ordinary writers. These tests were ported out of the deleted
//! qualification support directory; ADR 0011 removed that concept entirely.
use mllm_adapters::{fake::FakeEngine, traits::EngineAdapter};
use mllm_controller::{RuntimeAction, RuntimeCommand};
use mllm_controller::coordinator::{CoordinatorOptions, OwnedCoordinator, ServiceObservation};
use mllm_controller::ownership::{OwnedCoordinatorState, SharedCoordinatorState};
use mllm_domain::completion::{CleanupEvidence, CompletionEvidence, OwnedLaunchReceipt};
use mllm_domain::resources::{MemoryLimit, MemoryObservation, ResourcePhase};
use mllm_scheduler::residency::AdmissionContext;
use mllm_store::lifecycle::{ArmResult, DeploymentFence, LifecycleError};
use mllm_store::ordinary_lifecycle::cleanup::{CleanupExecutionContext, CleanupMode};
use mllm_store::ordinary_lifecycle::worker::InitializeStatus;
use mllm_store::ordinary_lifecycle::Start;
use mllm_store::Store;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[path = "support/fixture.rs"]
mod fixture;

/// The ordinary cleanup lane's local Fake execution, with the bounds the worker
/// applies before it sends. This is what the deleted controller qualification
/// module did for the same ordinary context.
fn collect_cleanup(
    engine: &FakeEngine,
    context: &CleanupExecutionContext,
    observed_at_ms: i64,
) -> Result<CleanupEvidence, LifecycleError> {
    if observed_at_ms < context.issued_at_ms || observed_at_ms > context.deadline_ms {
        return Err(LifecycleError::Invalid);
    }
    engine
        .lifecycle_cleanup(
            &context.binding_id,
            &context.incarnation,
            &context.identities,
            context.mode == CleanupMode::TerminateOwned,
            observed_at_ms,
        )
        .map_err(|_| LifecycleError::Conflict)
}

/// Row counts over every table an ordinary transaction may write.
fn full_counts(sql: &rusqlite::Connection) -> Vec<i64> {
    [
        "operations",
        "lifecycle_runs",
        "lifecycle_steps",
        "lifecycle_claims",
        "lifecycle_evidence",
        "command_receipts",
        "resource_owners",
        "resource_grants",
        "request_leases",
        "endpoint_leases",
        "management_events",
    ]
    .into_iter()
    .map(|table| {
        sql.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    })
    .collect()
}

#[tokio::test]
async fn ordinary_policy_change_stop_and_restart_retain_peak_and_never_resend() {
    let f = fixture::fixture();
    let fence = fixture::managed(&f, "ordinary");
    let accepted = f
        .store
        .accept_start(&f.session, &fence, 1800, 10000)
        .unwrap();
    let mut context = f.admission();
    context.now_ms = 1900;
    let mut controls = f.store.resource_policy("lab").unwrap().unwrap().controls;
    let previous = controls.domains["unified"].managed_limit;
    controls.domains.get_mut("unified").unwrap().managed_limit = 9_i64 << 30;
    f.store
        .update_resource_policy(
            &f.session,
            "owner",
            "lab",
            1,
            "lower",
            &controls,
            &f.observations,
            1800,
        )
        .unwrap();
    let before = full_counts(&f.sql);
    assert!(
        f.store
            .arm_step(&f.session, &accepted.step_id, context)
            .is_err()
    );
    assert_eq!(full_counts(&f.sql), before);
    assert!(f.store.resource_snapshot().unwrap().owners.is_empty());
    controls.domains.get_mut("unified").unwrap().managed_limit = previous;
    f.store
        .update_resource_policy(
            &f.session,
            "owner",
            "lab",
            2,
            "restore",
            &controls,
            &f.observations,
            1850,
        )
        .unwrap();
    let mut stale = context;
    stale.now_ms = 10001;
    assert!(
        f.store
            .arm_step(&f.session, &accepted.step_id, stale)
            .is_err()
    );
    assert!(matches!(
        f.store
            .arm_step(&f.session, &accepted.step_id, context)
            .unwrap(),
        ArmResult::New { .. }
    ));
    let execution = f
        .store
        .initialize_execution(&f.session, &accepted.step_id)
        .unwrap();
    let observation = FakeEngine::with_lifecycle()
        .execute_persisted(&RuntimeCommand {
            action: RuntimeAction::Initialize,
            context: execution,
        })
        .await
        .unwrap();
    f.store
        .record_owned_launch(
            &f.session,
            &accepted.step_id,
            &OwnedLaunchReceipt {
                binding_id: observation.binding_id,
                incarnation: observation.incarnation,
                identities: observation.identities.clone(),
                observed_at_ms: observation.observed_at_ms,
                receipt: observation.receipt.clone(),
            },
            1950,
        )
        .unwrap();
    f.store.fence_stop(&f.session, &fence, 10000).unwrap();
    let before = full_counts(&f.sql);
    let evidence = CompletionEvidence {
        token: observation.token,
        identities: observation.identities,
        observed_at_ms: observation.observed_at_ms,
        control_receipt: Some(observation.receipt),
        milestones: observation.facts,
    };
    assert!(
        f.store
            .complete_step(&f.session, &accepted.step_id, &evidence, 2000, f.ttl)
            .is_err()
    );
    assert_eq!(full_counts(&f.sql), before);
    let reopened = Store::open(&f._dir.path().join("ordinary.db")).unwrap();
    let session = reopened.begin_coordinator_session().unwrap();
    assert!(
        reopened
            .arm_step(&session, &accepted.step_id, context)
            .is_err()
    );
    assert!(
        reopened
            .arm_step(&f.session, &accepted.step_id, context)
            .is_err()
    );
    assert_eq!(
        reopened.resource_snapshot().unwrap().owners[&fence.deployment_id].phase,
        ResourcePhase::Cold
    );
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM lifecycle_steps WHERE state='uncertain'"),
        1
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM endpoint_leases"), 1);
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM deployments WHERE name='ordinary' AND dispatch_enabled=1"),
        0
    );
}

#[tokio::test]
async fn ordinary_initialize_to_ready() {
    let f = fixture::fixture();
    let fence = fixture::managed(&f, "ordinary");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let barrier = barrier.clone();
            let path = f._dir.path().join("ordinary.db");
            let session = f.session.clone();
            let target = fence.clone();
            std::thread::spawn(move || {
                let store = Store::open(&path).unwrap();
                barrier.wait();
                store.accept_start(&session, &target, 1800, 10000)
            })
        })
        .collect();
    let results: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    let accepted = results
        .iter()
        .find_map(|r| r.as_ref().ok())
        .unwrap()
        .clone();
    let joined = f
        .store
        .accept_start(&f.session, &fence, 1801, 20000)
        .unwrap();
    assert!(joined.joined);
    assert_eq!(accepted.operation_id, joined.operation_id);
    assert_eq!(accepted.step_id, joined.step_id);
    assert_eq!(accepted.binding_id, joined.binding_id);
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM deployments WHERE name='ordinary' AND dispatch_enabled=1"),
        0
    );
    let mut admission = f.admission();
    admission.now_ms = 1900;
    let mut stale = admission;
    stale.now_ms = 4000;
    assert!(
        f.store
            .arm_step(&f.session, &accepted.step_id, stale)
            .is_err()
    );
    let before = full_counts(&f.sql);
    let epoch_before = f.store.resource_snapshot().unwrap().epoch;
    f.sql.execute_batch("CREATE TRIGGER ordinary_arm_failure BEFORE UPDATE OF state ON lifecycle_steps WHEN NEW.state='armed' AND json_extract(NEW.step_json,'$.kind')='initialize' BEGIN SELECT RAISE(ABORT,'ordinary arm failure'); END;").unwrap();
    assert!(
        f.store
            .arm_step(&f.session, &accepted.step_id, admission)
            .is_err()
    );
    assert_eq!(full_counts(&f.sql), before);
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch_before);
    assert!(f.store.resource_snapshot().unwrap().owners.is_empty());
    f.sql
        .execute_batch("DROP TRIGGER ordinary_arm_failure;")
        .unwrap();
    assert!(matches!(
        f.store
            .arm_step(&f.session, &accepted.step_id, admission)
            .unwrap(),
        ArmResult::New { .. }
    ));
    let context = f
        .store
        .initialize_execution(&f.session, &accepted.step_id)
        .unwrap();
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        2,
        "concurrent accepts must both succeed"
    );
    assert_eq!(
        results
            .iter()
            .filter(|r| r.as_ref().is_ok_and(|r| r.joined))
            .count(),
        1
    );
    assert!(
        results
            .iter()
            .all(|r| r.as_ref().is_ok_and(|r| r.step_id == accepted.step_id))
    );
    assert_eq!(context.deadline_ms, 10000);
    assert_eq!(
        f.store.resource_snapshot().unwrap().owners[&fence.deployment_id].phase,
        ResourcePhase::Cold
    );
    assert_eq!(
        f.store
            .arm_step(&f.session, &accepted.step_id, admission)
            .unwrap(),
        ArmResult::AlreadyRecorded
    );
    let observation = FakeEngine::with_lifecycle()
        .execute_persisted(&RuntimeCommand {
            action: RuntimeAction::Initialize,
            context,
        })
        .await
        .unwrap();
    let evidence = CompletionEvidence {
        token: observation.token,
        identities: observation.identities.clone(),
        observed_at_ms: observation.observed_at_ms,
        control_receipt: Some(observation.receipt.clone()),
        milestones: observation.facts,
    };
    assert!(
        f.store
            .complete_step(&f.session, &accepted.step_id, &evidence, 1950, f.ttl)
            .is_err()
    );
    f.store
        .record_owned_launch(
            &f.session,
            &accepted.step_id,
            &OwnedLaunchReceipt {
                binding_id: observation.binding_id,
                incarnation: observation.incarnation,
                identities: observation.identities,
                observed_at_ms: observation.observed_at_ms,
                receipt: observation.receipt,
            },
            1950,
        )
        .unwrap();
    let before = full_counts(&f.sql);
    let peak_epoch = f.store.resource_snapshot().unwrap().epoch;
    let mut mutations = Vec::new();
    let mut wrong = evidence.clone();
    wrong.token.generation += 1;
    mutations.push(wrong);
    let mut wrong = evidence.clone();
    wrong.token.revision += 1;
    mutations.push(wrong);
    let mut wrong = evidence.clone();
    wrong.identities[1].start_ticks += 1;
    mutations.push(wrong);
    let mut wrong = evidence.clone();
    wrong.identities.pop();
    mutations.push(wrong);
    let mut wrong = evidence.clone();
    wrong.milestones.swap(0, 1);
    mutations.push(wrong);
    let mut wrong = evidence.clone();
    wrong.milestones.pop();
    mutations.push(wrong);
    let mut wrong = evidence.clone();
    wrong.observed_at_ms = 10001;
    mutations.push(wrong);
    for wrong in mutations {
        assert!(
            f.store
                .complete_step(&f.session, &accepted.step_id, &wrong, 1950, f.ttl)
                .is_err()
        );
        assert_eq!(full_counts(&f.sql), before);
        assert_eq!(f.store.resource_snapshot().unwrap().epoch, peak_epoch);
    }
    f.sql.execute("INSERT INTO request_leases(id,deployment_id,revision,generation,session_id,disposition) VALUES('ordinary-unknown',?1,1,1,?2,'uncertain')",rusqlite::params![fence.deployment_id,f.session.id()]).unwrap();
    assert!(
        f.store
            .complete_step(&f.session, &accepted.step_id, &evidence, 1950, f.ttl)
            .is_err()
    );
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, peak_epoch);
    f.sql
        .execute("DELETE FROM request_leases WHERE id='ordinary-unknown'", [])
        .unwrap();
    f.sql.execute_batch("CREATE TRIGGER ordinary_completion_failure BEFORE INSERT ON lifecycle_evidence WHEN NEW.step_id IN (SELECT id FROM lifecycle_steps WHERE json_extract(step_json,'$.kind')='initialize') BEGIN SELECT RAISE(ABORT,'ordinary completion failure'); END;").unwrap();
    assert!(
        f.store
            .complete_step(&f.session, &accepted.step_id, &evidence, 1950, f.ttl)
            .is_err()
    );
    assert_eq!(full_counts(&f.sql), before);
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, peak_epoch);
    f.sql
        .execute_batch("DROP TRIGGER ordinary_completion_failure;")
        .unwrap();
    f.store
        .complete_step(&f.session, &accepted.step_id, &evidence, 1950, f.ttl)
        .unwrap();
    let epoch = f.store.resource_snapshot().unwrap().epoch;
    f.store
        .complete_step(&f.session, &accepted.step_id, &evidence, 2000, f.ttl)
        .unwrap();
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
    assert_eq!(
        f.store.resource_snapshot().unwrap().owners[&fence.deployment_id].phase,
        ResourcePhase::Ready
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM deployments WHERE name='ordinary' AND dispatch_enabled=1 AND observed_state='ready'"), 1);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM endpoint_leases"), 1);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM lifecycle_claims"), 0);
    let mut wrong = evidence.clone();
    wrong.observed_at_ms += 1;
    assert!(
        f.store
            .complete_step(&f.session, &accepted.step_id, &wrong, 2000, f.ttl)
            .is_err()
    );
}

#[tokio::test]
async fn worker_selects_oldest_current_generation_without_adopting_superseded_work() {
    let f = fixture::fixture();
    let first = fixture::managed_edit(&f, "first", |_, _| {});
    let second = fixture::managed_edit(&f, "second", |_, _| {});
    let a = f
        .store
        .accept_start(&f.session, &first, 1800, 10000)
        .unwrap();
    let b = f
        .store
        .accept_start(&f.session, &second, 1801, 10001)
        .unwrap();
    // Control only acceptance ordering; all authorization/evidence came through writers.
    f.sql
        .execute(
            "UPDATE operations SET accepted_at='2026-09-15T00:00:00Z' WHERE id=?1",
            [&a.operation_id],
        )
        .unwrap();
    f.sql
        .execute(
            "UPDATE operations SET accepted_at='2026-09-15T00:00:01Z' WHERE id=?1",
            [&b.operation_id],
        )
        .unwrap();
    assert_eq!(
        f.store
            .next_initialize(&f.session)
            .unwrap()
            .unwrap()
            .operation_id(),
        a.operation_id
    );
    f.store.fence_stop(&f.session, &first, 10000).unwrap();
    assert_eq!(
        f.store
            .next_initialize(&f.session)
            .unwrap()
            .unwrap()
            .operation_id(),
        b.operation_id
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM endpoint_leases"), 2);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM lifecycle_claims"), 2);
    assert!(f.store.resource_snapshot().unwrap().owners.is_empty());
    // A new coordinator may inspect/reconcile the reservations, never silently adopt them.
    let next = f.store.begin_coordinator_session().unwrap();
    assert!(f.store.next_initialize(&next).unwrap().is_none());
    assert_eq!(f.scalar("SELECT COUNT(*) FROM endpoint_leases"), 2);
}

#[tokio::test]
async fn worker_selection_and_uncertainty_preserve_exact_durable_intent() {
    let f = fixture::fixture();
    assert!(
        f.store
            .next_initialize(&f.session)
            .unwrap()
            .is_none()
    );
    let fence = fixture::managed(&f, "ordinary");
    let accepted = f
        .store
        .accept_start(&f.session, &fence, 1800, 10000)
        .unwrap();
    let before = full_counts(&f.sql);
    let work = f
        .store
        .next_initialize(&f.session)
        .unwrap()
        .unwrap();
    assert_eq!(work.operation_id(), accepted.operation_id);
    assert_eq!(work.step_id(), accepted.step_id);
    assert_eq!(work.binding_id(), accepted.binding_id);
    assert_eq!(work.fence(), &fence);
    assert_eq!(work.deadline_ms(), 10000);
    assert_eq!(work.effective().name, "ordinary");
    assert_eq!(work.policy().revision, 1);
    assert_eq!(full_counts(&f.sql), before);
    let stored_plan: String = f
        .sql
        .query_row(
            "SELECT step_json FROM lifecycle_steps WHERE id=?1",
            [&accepted.step_id],
            |row| row.get(0),
        )
        .unwrap();
    f.sql
        .execute(
            "UPDATE lifecycle_steps SET step_json='{}' WHERE id=?1",
            [&accepted.step_id],
        )
        .unwrap();
    assert!(f.store.next_initialize(&f.session).is_err());
    f.sql
        .execute(
            "UPDATE lifecycle_steps SET step_json=?2 WHERE id=?1",
            rusqlite::params![accepted.step_id, stored_plan],
        )
        .unwrap();
    assert!(
        f.store
            .mark_initialize_uncertain(&f.session, &accepted.step_id, 1900)
            .is_err()
    );
    let mut admission = f.admission();
    admission.now_ms = 1900;
    assert!(matches!(
        f.store
            .arm_step(&f.session, &accepted.step_id, admission)
            .unwrap(),
        ArmResult::New { .. }
    ));
    assert!(
        f.store
            .next_initialize(&f.session)
            .unwrap()
            .is_none()
    );
    let charged = f.store.resource_snapshot().unwrap();
    f.sql.execute_batch("CREATE TRIGGER uncertain_event_failure BEFORE INSERT ON management_events WHEN NEW.kind='initialize_uncertain' BEGIN SELECT RAISE(ABORT,'uncertain event failure'); END;").unwrap();
    assert!(
        f.store
            .mark_initialize_uncertain(&f.session, &accepted.step_id, 10001)
            .is_err()
    );
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM lifecycle_steps WHERE state='armed'"),
        1
    );
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM lifecycle_runs WHERE state='running'"),
        1
    );
    assert_eq!(f.store.resource_snapshot().unwrap(), charged);
    f.sql
        .execute_batch("DROP TRIGGER uncertain_event_failure;")
        .unwrap();
    assert!(
        f.store
            .mark_initialize_uncertain(&f.session, &accepted.step_id, 10001)
            .unwrap()
    );
    assert!(
        !f.store
            .mark_initialize_uncertain(&f.session, &accepted.step_id, 10002)
            .unwrap()
    );
    assert_eq!(f.store.resource_snapshot().unwrap(), charged);
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM lifecycle_steps WHERE state='uncertain'"),
        1
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM lifecycle_claims"), 1);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM endpoint_leases"), 1);
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM management_events WHERE kind='initialize_uncertain'"
        ),
        1
    );
    assert_eq!(
        f.store
            .arm_step(&f.session, &accepted.step_id, admission)
            .unwrap(),
        ArmResult::AlreadyRecorded
    );
    let session = f.store.begin_coordinator_session().unwrap();
    assert!(f.store.next_initialize(&f.session).is_err());
    assert!(
        f.store
            .mark_initialize_uncertain(&session, &accepted.step_id, 10003)
            .is_err()
    );
    assert_eq!(f.store.resource_snapshot().unwrap(), charged);
}


// A real completed, promoted and cleaned-up qualification, copied without
// modifying its evidence. Ownership is acquired before accepting ordinary work.
async fn owned_fixture() -> (
    tempfile::TempDir,
    SharedCoordinatorState,
    mllm_store::lifecycle::DeploymentFence,
    Vec<MemoryObservation>,
) {
    use std::os::unix::fs::PermissionsExt;
    let source = fixture::owned_source().await;
    let fence = source.fence.clone();
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().join("srv.sqlite3");
    std::fs::copy(source.dir.path().join("srv.sqlite3"), &path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let owner = Arc::new(Mutex::new(OwnedCoordinatorState::open(dir.path()).unwrap()));
    (dir, owner, fence, source.observations.clone())
}

struct Observations(Vec<MemoryObservation>);
impl ServiceObservation for Observations {
    fn observe(&self, _: String) -> mllm_controller::coordinator::ObservationFuture {
        let values = self.0.clone();
        Box::pin(async move { Ok(values) })
    }
}

#[tokio::test]
async fn owned_worker_initializes_once_for_joined_and_dropped_observers() {
    let (_dir, owner, fence, observations) = owned_fixture().await;
    let worker = OwnedCoordinator::spawn_fake(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions::default(),
    )
    .unwrap();
    let a = worker.start(&fence, 10000).unwrap();
    let b = worker.start(&fence, 10000).unwrap();
    assert_eq!(a.operation_id(), b.operation_id());
    let step = a.step_id().to_owned();
    drop(a);
    drop(b);
    // Parallel qualification fixtures can occupy the CPU while this worker
    // validates durable provenance. This is a test hang detector; the service
    // clock and persisted operation deadline remain 1900 and 10000 below.
    let ready_wait_started = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let done = {
                let state = owner.lock().unwrap();
                state
                    .store()
                    .runtime_binding(&fence.deployment_id)
                    .unwrap()
                    .is_some_and(|b| b.state == "live")
            };
            if done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "owned worker did not reach Ready after {:?}: {error}",
            ready_wait_started.elapsed()
        )
    });
    eprintln!(
        "owned worker Ready wait: {:?}",
        ready_wait_started.elapsed()
    );
    {
        let state = owner.lock().unwrap();
        assert_eq!(
            state.store().resource_snapshot().unwrap().owners[&fence.deployment_id].phase,
            ResourcePhase::Ready
        );
        let context = state
            .store()
            .initialize_execution(state.session(), &step)
            .unwrap();
        assert_eq!(context.deadline_ms, 10000);
    }
    worker.shutdown().await.unwrap();
}


struct CleanupFixture {
    store: Store,
    session: mllm_store::dispatch::CoordinatorSession,
    sql: rusqlite::Connection,
    observations: Vec<MemoryObservation>,
    limits: Vec<MemoryLimit>,
    ttl: i64,
    max_parked: usize,
    _dir: tempfile::TempDir,
}
impl CleanupFixture {
    fn scalar(&self, sql: &str) -> i64 {
        self.sql.query_row(sql, [], |r| r.get(0)).unwrap()
    }
    fn admission(&self) -> AdmissionContext<'_> {
        AdmissionContext::new(
            &self.observations,
            &self.limits,
            1900,
            self.ttl,
            self.max_parked,
        )
    }
}

async fn cleanup_fixture() -> (CleanupFixture, DeploymentFence) {
    let source = fixture::owned_source().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("srv.sqlite3");
    std::fs::copy(source.dir.path().join("srv.sqlite3"), &path).unwrap();
    let store = Store::open(&path).unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let sql = rusqlite::Connection::open(path).unwrap();
    let raw: String = sql
        .query_row(
            "SELECT effective_json FROM effective_revisions WHERE deployment_id=?1",
            [&source.fence.deployment_id],
            |r| r.get(0),
        )
        .unwrap();
    let e = mllm_config::effective::decode_effective_snapshot(&raw).unwrap();
    let controls = store
        .resource_policy(&e.host.name)
        .unwrap()
        .unwrap()
        .controls;
    let limits = controls
        .domains
        .iter()
        .map(|(domain, d)| MemoryLimit {
            domain: domain.clone(),
            managed_bytes: d.managed_limit,
            free_reserve_bytes: d.free_reserve,
            host_kv_bytes: d.host_kv_limit,
            parked_bytes: d.parked_limit,
        })
        .collect();
    (
        CleanupFixture {
            store,
            session,
            sql,
            observations: source.observations.clone(),
            limits,
            ttl: controls.observation_ttl_ms,
            max_parked: controls.max_parked as usize,
            _dir: dir,
        },
        source.fence.clone(),
    )
}

async fn started(
    ready: bool,
) -> (
    CleanupFixture,
    DeploymentFence,
    Start,
    FakeEngine,
    CompletionEvidence,
) {
    let (f, fence) = cleanup_fixture().await;
    let start = f
        .store
        .accept_start(&f.session, &fence, 1800, 10000)
        .unwrap();
    let mut admission = f.admission();
    admission.now_ms = 1900;
    f.store
        .arm_step(&f.session, &start.step_id, admission)
        .unwrap();
    let context = f
        .store
        .initialize_execution(&f.session, &start.step_id)
        .unwrap();
    let fake = FakeEngine::with_lifecycle();
    let observation = fake
        .execute_persisted(&RuntimeCommand {
            action: RuntimeAction::Initialize,
            context,
        })
        .await
        .unwrap();
    f.store
        .record_owned_launch(
            &f.session,
            &start.step_id,
            &OwnedLaunchReceipt {
                binding_id: observation.binding_id,
                incarnation: observation.incarnation,
                identities: observation.identities.clone(),
                observed_at_ms: observation.observed_at_ms,
                receipt: observation.receipt.clone(),
            },
            1950,
        )
        .unwrap();
    let evidence = CompletionEvidence {
        token: observation.token,
        identities: observation.identities,
        observed_at_ms: observation.observed_at_ms,
        control_receipt: Some(observation.receipt),
        milestones: observation.facts,
    };
    if ready {
        f.store
            .complete_step(&f.session, &start.step_id, &evidence, 1950, f.ttl)
            .unwrap();
    }
    (f, fence, start, fake, evidence)
}
#[tokio::test]
async fn ordinary_cleanup_exact_stop_replay_retains_then_releases_once() {
    let (f, fence, start, fake, _) = started(true).await;
    let ticket = f
        .store
        .grant_dispatch(
            &f.session,
            mllm_store::dispatch::DispatchRequest {
                deployment_id: &fence.deployment_id,
                revision: fence.revision,
                generation: fence.generation,
                max_per_deployment: 4,
                max_total: 8,
            },
        )
        .unwrap();
    let epoch = f.store.resource_snapshot().unwrap().epoch;
    let stop = f
        .store
        .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2000, 10000)
        .unwrap();
    let replay = f
        .store
        .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2001, 10000)
        .unwrap();
    assert_eq!(stop, replay);
    let resolved = DeploymentFence {
        generation: fence.generation + 1,
        ..fence.clone()
    };
    assert_eq!(
        stop,
        f.store
            .accept_ordinary_cleanup(&f.session, "owner", &resolved, "stop", 2001, 10000)
            .unwrap()
    );
    assert!(
        f.store
            .accept_ordinary_cleanup(&f.session, "owner", &resolved, "different-key", 2001, 10000)
            .is_err()
    );
    assert!(
        f.store
            .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2001, 11000)
            .is_err()
    );
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
    assert_eq!(
        f.store.pending_dispatches(&fence.deployment_id).unwrap()[0].id,
        ticket.id()
    );
    assert!(
        f.store
            .resource_snapshot()
            .unwrap()
            .owners
            .contains_key(&fence.deployment_id)
    );
    assert!(
        f.store
            .initialize_execution(&f.session, &start.step_id)
            .is_err()
    );
    let (arm, context) = f
        .store
        .arm_ordinary_cleanup_with_context(&f.session, &stop.step_id, 2050)
        .unwrap();
    assert!(matches!(arm, ArmResult::New { .. }));
    let (again, none) = f
        .store
        .arm_ordinary_cleanup_with_context(&f.session, &stop.step_id, 2051)
        .unwrap();
    assert_eq!(again, ArmResult::AlreadyRecorded);
    assert!(none.is_none());
    let context = context.unwrap();
    let gone = collect_cleanup(&fake, &context, 2100).unwrap();
    f.store
        .complete_cleanup(&f.session, &stop.step_id, &gone, 2150, f.ttl)
        .unwrap();
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch + 1);
    assert!(
        !f.store
            .resource_snapshot()
            .unwrap()
            .owners
            .contains_key(&fence.deployment_id)
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM management_events WHERE kind IN ('ordinary_cleanup_accepted','ordinary_cleanup_armed','ordinary_cleanup_completed')"), 3);
    assert!(
        f.store
            .pending_dispatches(&fence.deployment_id)
            .unwrap()
            .is_empty()
    );
    let counts = full_counts(&f.sql);
    f.store
        .complete_cleanup(&f.session, &stop.step_id, &gone, 90000, f.ttl)
        .unwrap();
    assert_eq!(full_counts(&f.sql), counts);
    let mut changed = gone;
    changed.receipt.push('x');
    assert!(
        f.store
            .complete_cleanup(&f.session, &stop.step_id, &changed, 2150, f.ttl)
            .is_err()
    );
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch + 1);
    let golden: Value = serde_json::from_str(include_str!(
        "../../mllm-config/tests/fixtures/effective-fake-golden.json"
    ))
    .unwrap();
    let mut config = golden["input"]["deployment"].clone();
    let mut host = golden["input"]["host"].clone();
    config["name"] = json!("ordinary");
    config["routes"] = json!(["ordinary-replaced"]);
    host["runtime_profiles"]["local"]["build_fingerprint"] = json!("qualification-fake-v1");
    host["runtime_profiles"]["local"]["security"]["admin_credential_ref"] =
        json!("secret://another-admin");
    let replaced = f
        .store
        .replace_stopped_managed_configuration(
            &f.session,
            "owner",
            "replace-after-stop",
            &fence.deployment_id,
            &json!({"expected_revision":1,"config":config}).to_string(),
            &host,
            2200,
        )
        .unwrap();
    let next = DeploymentFence {
        deployment_id: fence.deployment_id.clone(),
        revision: replaced.revision,
        generation: replaced.generation,
    };
    let fresh = f
        .store
        .accept_start(&f.session, &next, 2200, 10000)
        .unwrap();
    assert_ne!(fresh.binding_id, start.binding_id);
    let mut admission = f.admission();
    admission.now_ms = 2350;
    f.store
        .arm_step(&f.session, &fresh.step_id, admission)
        .unwrap();
    let context = f
        .store
        .initialize_execution(&f.session, &fresh.step_id)
        .unwrap();
    let observation = FakeEngine::with_lifecycle()
        .execute_persisted(&RuntimeCommand {
            action: RuntimeAction::Initialize,
            context,
        })
        .await
        .unwrap();
    f.store
        .record_owned_launch(
            &f.session,
            &fresh.step_id,
            &OwnedLaunchReceipt {
                binding_id: observation.binding_id,
                incarnation: observation.incarnation,
                identities: observation.identities.clone(),
                observed_at_ms: observation.observed_at_ms,
                receipt: observation.receipt.clone(),
            },
            2400,
        )
        .unwrap();
    f.store
        .complete_step(
            &f.session,
            &fresh.step_id,
            &CompletionEvidence {
                token: observation.token,
                identities: observation.identities,
                observed_at_ms: observation.observed_at_ms,
                control_receipt: Some(observation.receipt),
                milestones: observation.facts,
            },
            2400,
            f.ttl,
        )
        .unwrap();
    assert_eq!(
        f.store
            .runtime_binding(&next.deployment_id)
            .unwrap()
            .unwrap()
            .state,
        "live"
    );
    assert_eq!(
        f.store
            .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2300, 10000)
            .unwrap(),
        stop
    );
    assert_eq!(
        f.store
            .arm_ordinary_cleanup_with_context(&f.session, &stop.step_id, 2300)
            .unwrap(),
        (ArmResult::AlreadyRecorded, None)
    );
}

#[tokio::test]
async fn ordinary_cleanup_rejects_missing_ready_evidence_before_fencing() {
    let (f, fence, start, _, _) = started(true).await;
    // Named crash corruption: a completed source without its atomic evidence.
    f.sql
        .execute(
            "DELETE FROM lifecycle_evidence WHERE step_id=?1",
            [&start.step_id],
        )
        .unwrap();
    let before = full_counts(&f.sql);
    assert!(
        f.store
            .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2000, 10000)
            .is_err()
    );
    assert_eq!(full_counts(&f.sql), before);
    assert_eq!(
        f.store
            .runtime_binding(&fence.deployment_id)
            .unwrap()
            .unwrap()
            .state,
        "live"
    );
}

#[tokio::test]
async fn ordinary_cleanup_armed_and_uncertain_handoff_settles_only_after_verified_exit() {
    for uncertain in [false, true] {
        let (f, fence, start, fake, evidence) = started(false).await;
        if uncertain {
            f.store
                .mark_initialize_uncertain(&f.session, &start.step_id, 2000)
                .unwrap();
        }
        let stop = f
            .store
            .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2000, 10000)
            .unwrap();
        let before = full_counts(&f.sql);
        assert!(
            f.store
                .complete_step(&f.session, &start.step_id, &evidence, 2010, f.ttl)
                .is_err()
        );
        assert_eq!(full_counts(&f.sql), before);
        let (prior_state,history):(String,String)=f.sql.query_row("SELECT s.state,r.plan_json FROM lifecycle_steps s JOIN lifecycle_runs r ON r.operation_id=?2 WHERE s.id=?1",rusqlite::params![start.step_id,stop.operation_id],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(prior_state, if uncertain { "uncertain" } else { "armed" });
        assert!(history.contains(&start.step_id));
        assert!(history.contains(&start.operation_id));
        let epoch = f.store.resource_snapshot().unwrap().epoch;
        let (_, context) = f
            .store
            .arm_ordinary_cleanup_with_context(&f.session, &stop.step_id, 2050)
            .unwrap();
        let gone = collect_cleanup(&fake, &context.unwrap(), 2100)
            .unwrap();
        f.store
            .complete_cleanup(&f.session, &stop.step_id, &gone, 2150, f.ttl)
            .unwrap();
        assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch + 1);
        assert_eq!(
            f.sql
                .query_row(
                    "SELECT state FROM lifecycle_steps WHERE id=?1",
                    [&start.step_id],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "cancelled"
        );
        f.store
            .complete_cleanup(&f.session, &stop.step_id, &gone, 999999, f.ttl)
            .unwrap();
        assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch + 1);
    }
}

#[tokio::test]
async fn ordinary_cleanup_rejects_bad_evidence_and_rolls_back_release_failure() {
    let (f, fence, _, fake, _) = started(true).await;
    let stop = f
        .store
        .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2000, 10000)
        .unwrap();
    let (_, context) = f
        .store
        .arm_ordinary_cleanup_with_context(&f.session, &stop.step_id, 2050)
        .unwrap();
    let gone =
        collect_cleanup(&fake, &context.unwrap(), 2100).unwrap();
    let before = full_counts(&f.sql);
    let epoch = f.store.resource_snapshot().unwrap().epoch;
    for mutation in 0..10 {
        let mut wrong = gone.clone();
        let mut now = 2150;
        let mut ttl = f.ttl;
        match mutation {
            0 => {
                wrong.identities.pop();
            }
            1 => wrong.identities[1].start_ticks += 1,
            2 => wrong.binding_id = ulid::Ulid::new().to_string(),
            3 => wrong.incarnation = ulid::Ulid::new().to_string(),
            4 => wrong.receipt = " ".into(),
            5 => wrong.receipt = "x".repeat(524289),
            6 => wrong.observed_at_ms = 2049,
            7 => now = 10001,
            8 => ttl += 1,
            9 => now = wrong.observed_at_ms + f.ttl + 1,
            _ => unreachable!(),
        }
        assert!(
            f.store
                .complete_cleanup(&f.session, &stop.step_id, &wrong, now, ttl)
                .is_err(),
            "mutation {mutation}"
        );
        assert_eq!(full_counts(&f.sql), before);
        assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
    }
    f.sql.execute_batch("CREATE TRIGGER ordinary_cleanup_release_failure BEFORE INSERT ON management_events WHEN NEW.kind='ordinary_cleanup_completed' BEGIN SELECT RAISE(ABORT,'cleanup release rollback'); END;").unwrap();
    assert!(
        f.store
            .complete_cleanup(&f.session, &stop.step_id, &gone, 2150, f.ttl)
            .is_err()
    );
    assert_eq!(full_counts(&f.sql), before);
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
    assert_eq!(
        f.store
            .runtime_binding(&fence.deployment_id)
            .unwrap()
            .unwrap()
            .state,
        "uncertain"
    );
    f.sql
        .execute_batch("DROP TRIGGER ordinary_cleanup_release_failure;")
        .unwrap();
    f.store
        .complete_cleanup(&f.session, &stop.step_id, &gone, 2150, f.ttl)
        .unwrap();
}

#[tokio::test]
async fn ordinary_cleanup_stale_sessions_keep_original_authority_bounded() {
    let (f, fence, start, fake, _) = started(true).await;
    let stop = f
        .store
        .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2000, 10000)
        .unwrap();
    let (_, context) = f
        .store
        .arm_ordinary_cleanup_with_context(&f.session, &stop.step_id, 2050)
        .unwrap();
    let gone =
        collect_cleanup(&fake, &context.unwrap(), 2100).unwrap();
    f.store
        .complete_cleanup(&f.session, &stop.step_id, &gone, 2150, f.ttl)
        .unwrap();
    assert_eq!(
        f.sql
            .query_row(
                "SELECT state FROM lifecycle_steps WHERE id=?1",
                [&start.step_id],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "completed"
    );

    let (f, fence, _, fake, _) = started(false).await;
    let stop = f
        .store
        .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2000, 10000)
        .unwrap();
    let (_, context) = f
        .store
        .arm_ordinary_cleanup_with_context(&f.session, &stop.step_id, 2050)
        .unwrap();
    let gone =
        collect_cleanup(&fake, &context.unwrap(), 2100).unwrap();
    let current = f.store.begin_coordinator_session().unwrap();
    let before = full_counts(&f.sql);
    assert_eq!(
        f.store
            .accept_ordinary_cleanup(&current, "owner", &fence, "stop", 2200, 10000)
            .unwrap(),
        stop
    );
    for session in [&f.session, &current] {
        assert!(
            f.store
                .complete_cleanup(session, &stop.step_id, &gone, 2150, f.ttl)
                .is_err()
        );
        assert!(
            f.store
                .arm_ordinary_cleanup_with_context(session, &stop.step_id, 2200)
                .is_err()
        );
    }
    assert_eq!(full_counts(&f.sql), before);
    assert!(
        f.store
            .resource_snapshot()
            .unwrap()
            .owners
            .contains_key(&fence.deployment_id)
    );
}

#[tokio::test]
async fn ordinary_cleanup_unproven_lease_blocks_all_release_and_other_deployment_is_untouched() {
    let (f, fence, _, fake, _) = started(true).await;
    let other = fixture::owned_source().await.other.clone();
    let start = f
        .store
        .accept_start(&f.session, &other, 1800, 10000)
        .unwrap();
    f.store
        .arm_step(&f.session, &start.step_id, f.admission())
        .unwrap();
    let context = f
        .store
        .initialize_execution(&f.session, &start.step_id)
        .unwrap();
    let observation = FakeEngine::with_lifecycle()
        .execute_persisted(&RuntimeCommand {
            action: RuntimeAction::Initialize,
            context,
        })
        .await
        .unwrap();
    f.store
        .record_owned_launch(
            &f.session,
            &start.step_id,
            &OwnedLaunchReceipt {
                binding_id: observation.binding_id,
                incarnation: observation.incarnation,
                identities: observation.identities.clone(),
                observed_at_ms: observation.observed_at_ms,
                receipt: observation.receipt.clone(),
            },
            1950,
        )
        .unwrap();
    f.store
        .complete_step(
            &f.session,
            &start.step_id,
            &CompletionEvidence {
                token: observation.token,
                identities: observation.identities,
                observed_at_ms: observation.observed_at_ms,
                control_receipt: Some(observation.receipt),
                milestones: observation.facts,
            },
            1950,
            f.ttl,
        )
        .unwrap();
    let ticket = f
        .store
        .grant_dispatch(
            &f.session,
            mllm_store::dispatch::DispatchRequest {
                deployment_id: &other.deployment_id,
                revision: other.revision,
                generation: other.generation,
                max_per_deployment: 2,
                max_total: 8,
            },
        )
        .unwrap();
    let stop = f
        .store
        .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2000, 10000)
        .unwrap();
    let (_, context) = f
        .store
        .arm_ordinary_cleanup_with_context(&f.session, &stop.step_id, 2050)
        .unwrap();
    let gone =
        collect_cleanup(&fake, &context.unwrap(), 2100).unwrap();
    // Named corruption: an unexplained older incarnation/session lease.
    f.sql.execute("INSERT INTO request_leases VALUES('unproven-prior-incarnation',?1,1,999,'unknown-session','uncertain')",[&fence.deployment_id]).unwrap();
    let before = full_counts(&f.sql);
    let epoch = f.store.resource_snapshot().unwrap().epoch;
    assert!(
        f.store
            .complete_cleanup(&f.session, &stop.step_id, &gone, 2150, f.ttl)
            .is_err()
    );
    assert_eq!(full_counts(&f.sql), before);
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
    assert_eq!(
        f.store
            .runtime_binding(&fence.deployment_id)
            .unwrap()
            .unwrap()
            .state,
        "uncertain"
    );
    f.sql
        .execute(
            "DELETE FROM request_leases WHERE id='unproven-prior-incarnation'",
            [],
        )
        .unwrap();
    f.store
        .complete_cleanup(&f.session, &stop.step_id, &gone, 2150, f.ttl)
        .unwrap();
    assert_eq!(
        f.store.pending_dispatches(&other.deployment_id).unwrap()[0].id,
        ticket.id()
    );
    assert!(
        f.store
            .resource_snapshot()
            .unwrap()
            .owners
            .contains_key(&other.deployment_id)
    );
    assert_eq!(
        f.store
            .runtime_binding(&other.deployment_id)
            .unwrap()
            .unwrap()
            .state,
        "live"
    );
}

#[tokio::test]
async fn ordinary_cleanup_missing_ownership_and_unarmed_reservations_stay_retained() {
    let (f, fence) = cleanup_fixture().await;
    let start = f
        .store
        .accept_start(&f.session, &fence, 1800, 10000)
        .unwrap();
    let before = full_counts(&f.sql);
    assert!(
        f.store
            .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2000, 10000)
            .is_err()
    );
    assert_eq!(full_counts(&f.sql), before);
    f.store
        .arm_step(&f.session, &start.step_id, f.admission())
        .unwrap();
    let before = full_counts(&f.sql);
    assert!(
        f.store
            .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2000, 10000)
            .is_err()
    );
    assert_eq!(full_counts(&f.sql), before);
    assert_eq!(
        f.store
            .runtime_binding(&fence.deployment_id)
            .unwrap()
            .unwrap()
            .state,
        "uncertain"
    );
}

#[tokio::test]
async fn ordinary_cleanup_terminal_corruption_never_becomes_recorded_arm_authority() {
    let (f, fence, _, fake, _) = started(true).await;
    let stop = f
        .store
        .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2000, 10000)
        .unwrap();
    let (_, context) = f
        .store
        .arm_ordinary_cleanup_with_context(&f.session, &stop.step_id, 2050)
        .unwrap();
    let gone =
        collect_cleanup(&fake, &context.unwrap(), 2100).unwrap();
    f.store
        .complete_cleanup(&f.session, &stop.step_id, &gone, 2150, f.ttl)
        .unwrap();
    // Named crash corruption: terminal rows without the committed evidence.
    f.sql
        .execute(
            "DELETE FROM lifecycle_evidence WHERE step_id=?1",
            [&stop.step_id],
        )
        .unwrap();
    let before = full_counts(&f.sql);
    assert!(
        f.store
            .arm_ordinary_cleanup_with_context(&f.session, &stop.step_id, 2300)
            .is_err()
    );
    assert_eq!(full_counts(&f.sql), before);
}

#[tokio::test]
async fn ordinary_cleanup_strict_identity_kind_and_fence_corruption_retains_all_charges() {
    let (f, fence, start, fake, _) = started(true).await;
    let stop = f
        .store
        .accept_ordinary_cleanup(&f.session, "owner", &fence, "stop", 2000, 10000)
        .unwrap();
    let (_, context) = f
        .store
        .arm_ordinary_cleanup_with_context(&f.session, &stop.step_id, 2050)
        .unwrap();
    let gone =
        collect_cleanup(&fake, &context.unwrap(), 2100).unwrap();
    let original_ids: String = f
        .sql
        .query_row(
            "SELECT identities_json FROM runtime_bindings WHERE id=?1",
            [&start.binding_id],
            |r| r.get(0),
        )
        .unwrap();
    let association: String = f
        .sql
        .query_row(
            "SELECT association_json FROM owned_launch_associations WHERE step_id=?1",
            [&start.step_id],
            |r| r.get(0),
        )
        .unwrap();
    for mutation in 0..7 {
        // Named corruption matrix; all starting authority comes from real writers.
        match mutation {
            0 => {
                f.sql
                    .execute(
                        "UPDATE runtime_bindings SET identities_json='[]' WHERE id=?1",
                        [&start.binding_id],
                    )
                    .unwrap();
            }
            1 => {
                let mut ids: Value = serde_json::from_str(&original_ids).unwrap();
                ids.as_array_mut().unwrap().pop();
                f.sql
                    .execute(
                        "UPDATE runtime_bindings SET identities_json=?2 WHERE id=?1",
                        rusqlite::params![start.binding_id, ids.to_string()],
                    )
                    .unwrap();
            }
            2 => {
                let mut a: Value = serde_json::from_str(&association).unwrap();
                a["identities"][1]["start_ticks"] = json!(123456789);
                f.sql
                    .execute(
                        "UPDATE owned_launch_associations SET association_json=?2 WHERE step_id=?1",
                        rusqlite::params![start.step_id, a.to_string()],
                    )
                    .unwrap();
            }
            3 => {
                f.sql
                    .execute(
                        "UPDATE operations SET kind='foreign_cleanup' WHERE id=?1",
                        [&stop.operation_id],
                    )
                    .unwrap();
            }
            4 => {
                f.sql
                    .execute(
                        "UPDATE operations SET kind='stop' WHERE id=?1",
                        [&stop.operation_id],
                    )
                    .unwrap();
            }
            5 => {
                f.sql
                    .execute(
                        "UPDATE lifecycle_claims SET generation=generation+1 WHERE operation_id=?1",
                        [&stop.operation_id],
                    )
                    .unwrap();
            }
            6 => {
                f.sql
                    .execute(
                        "UPDATE runtime_bindings SET ownership='attached' WHERE id=?1",
                        [&start.binding_id],
                    )
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let before = full_counts(&f.sql);
        let epoch = f.store.resource_snapshot().unwrap().epoch;
        assert!(
            f.store
                .complete_cleanup(&f.session, &stop.step_id, &gone, 2150, f.ttl)
                .is_err(),
            "mutation {mutation}"
        );
        assert!(
            f.store
                .arm_ordinary_cleanup_with_context(&f.session, &stop.step_id, 2150)
                .is_err(),
            "mutation {mutation}"
        );
        assert_eq!(full_counts(&f.sql), before);
        assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
        match mutation {
            0 | 1 => {
                f.sql
                    .execute(
                        "UPDATE runtime_bindings SET identities_json=?2 WHERE id=?1",
                        rusqlite::params![start.binding_id, original_ids],
                    )
                    .unwrap();
            }
            2 => {
                f.sql
                    .execute(
                        "UPDATE owned_launch_associations SET association_json=?2 WHERE step_id=?1",
                        rusqlite::params![start.step_id, association],
                    )
                    .unwrap();
            }
            3 | 4 => {
                f.sql
                    .execute(
                        "UPDATE operations SET kind='ordinary_cleanup' WHERE id=?1",
                        [&stop.operation_id],
                    )
                    .unwrap();
            }
            5 => {
                f.sql
                    .execute(
                        "UPDATE lifecycle_claims SET generation=generation-1 WHERE operation_id=?1",
                        [&stop.operation_id],
                    )
                    .unwrap();
            }
            6 => {
                f.sql
                    .execute(
                        "UPDATE runtime_bindings SET ownership='managed' WHERE id=?1",
                        [&start.binding_id],
                    )
                    .unwrap();
            }
            _ => unreachable!(),
        }
    }
    f.store
        .complete_cleanup(&f.session, &stop.step_id, &gone, 2150, f.ttl)
        .unwrap();
}
async fn start_fixture() -> (
    Store,
    mllm_store::dispatch::CoordinatorSession,
    rusqlite::Connection,
    tempfile::TempDir,
) {
    let source = fixture::owned_source().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("start.sqlite3");
    std::fs::copy(source.dir.path().join("srv.sqlite3"), &path).unwrap();
    let store = Store::open(&path).unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let sql = rusqlite::Connection::open(path).unwrap();
    (store, session, sql, dir)
}

fn counts(sql: &rusqlite::Connection) -> Vec<i64> {
    [
        "operations",
        "lifecycle_runs",
        "lifecycle_steps",
        "runtime_bindings",
        "endpoint_leases",
        "management_events",
        "command_receipts",
        "resource_grants",
        "resource_owners",
        "request_leases",
    ]
    .map(|table| {
        sql.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    })
    .to_vec()
}

#[tokio::test]
async fn start_receipt_faults_are_atomic_and_historical_corruption_is_rejected() {
    let (store, session, sql, _dir) = start_fixture().await;
    let source = fixture::owned_source().await;
    let id = &source.fence.deployment_id;
    for (table, condition) in [
        ("command_receipts", "NEW.idempotency_key='start'"),
        (
            "management_events",
            "NEW.kind='initialize_accepted'",
        ),
    ] {
        sql.execute_batch(&format!("CREATE TRIGGER fail_start BEFORE INSERT ON {table} WHEN {condition} BEGIN SELECT RAISE(ABORT,'start rollback'); END;")).unwrap();
        let before = counts(&sql);
        assert!(matches!(
            store.accept_start_command(&session, "owner", id, 1, "start", 1800, 10000),
            Err(LifecycleError::Sql(_))
        ));
        assert_eq!(counts(&sql), before);
        sql.execute_batch("DROP TRIGGER fail_start").unwrap();
    }
    let accepted = store
        .accept_start_command(&session, "owner", id, 1, "start", 1800, 10000)
        .unwrap();
    sql.execute_batch("CREATE TRIGGER fail_join BEFORE INSERT ON command_receipts WHEN NEW.idempotency_key='join' BEGIN SELECT RAISE(ABORT,'join rollback'); END;").unwrap();
    let before_join = counts(&sql);
    assert!(matches!(
        store.accept_start_command(&session, "owner", id, 1, "join", 1801, 20000),
        Err(LifecycleError::Sql(_))
    ));
    assert_eq!(counts(&sql), before_join);
    sql.execute_batch("DROP TRIGGER fail_join").unwrap();
    for corruption in [
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.unknown',1) WHERE idempotency_key='start'",
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.method','GET') WHERE idempotency_key='start'",
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.action','stop') WHERE idempotency_key='start'",
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.principal','someone') WHERE idempotency_key='start'",
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.scope','wrong') WHERE idempotency_key='start'",
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.request_hash','wrong') WHERE idempotency_key='start'",
        "UPDATE command_receipts SET operation_id=(SELECT id FROM operations WHERE kind='managed_configuration_create' LIMIT 1) WHERE idempotency_key='start'",
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.receipt.generation',2) WHERE idempotency_key='start'",
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.receipt.joined',json('true')) WHERE idempotency_key='start'",
        "UPDATE command_receipts SET response_json=response_json || printf('%1048577s',' ') WHERE idempotency_key='start'",
        "UPDATE command_receipts SET response_json=CAST(response_json AS BLOB) WHERE idempotency_key='start'",
        "UPDATE lifecycle_steps SET step_json=CAST(step_json AS BLOB) WHERE operation_id IN (SELECT operation_id FROM command_receipts WHERE idempotency_key='start')",
        "UPDATE lifecycle_steps SET step_json=step_json || printf('%1048577s',' ') WHERE operation_id IN (SELECT operation_id FROM command_receipts WHERE idempotency_key='start')",
        "UPDATE lifecycle_steps SET step_json=json_set(step_json,'$.unknown',1) WHERE operation_id IN (SELECT operation_id FROM command_receipts WHERE idempotency_key='start')",
        "UPDATE lifecycle_steps SET step_json=json_set(step_json,'$.accepted_at_ms',1801) WHERE operation_id IN (SELECT operation_id FROM command_receipts WHERE idempotency_key='start')",
        "UPDATE lifecycle_runs SET plan_json=json_set(plan_json,'$.unknown',1) WHERE operation_id IN (SELECT operation_id FROM command_receipts WHERE idempotency_key='start')",
        "UPDATE lifecycle_runs SET deadline_ms=9999 WHERE operation_id IN (SELECT operation_id FROM command_receipts WHERE idempotency_key='start')",
        "UPDATE runtime_bindings SET incarnation='bad' WHERE id IN (SELECT binding_id FROM lifecycle_steps WHERE operation_id IN (SELECT operation_id FROM command_receipts WHERE idempotency_key='start'))",
        "UPDATE operations SET kind='ordinary_cleanup' WHERE id IN (SELECT operation_id FROM command_receipts WHERE idempotency_key='start')",
        "DELETE FROM lifecycle_steps WHERE operation_id IN (SELECT operation_id FROM command_receipts WHERE idempotency_key='start')",
        "DELETE FROM lifecycle_runs WHERE operation_id IN (SELECT operation_id FROM command_receipts WHERE idempotency_key='start')",
        "INSERT INTO lifecycle_steps(id,operation_id,ordinal,deployment_id,binding_id,session_id,state,step_json) SELECT 'ambiguous',operation_id,1,deployment_id,binding_id,session_id,state,step_json FROM lifecycle_steps WHERE operation_id IN (SELECT operation_id FROM command_receipts WHERE idempotency_key='start')",
    ] {
        let corrupted_dir = tempfile::tempdir().unwrap();
        let corrupted_path = corrupted_dir.path().join("corrupt.sqlite3");
        sql.execute("VACUUM INTO ?1", [corrupted_path.to_str().unwrap()])
            .unwrap();
        let corrupted_store = Store::open(&corrupted_path).unwrap();
        let corrupted_sql = rusqlite::Connection::open(&corrupted_path).unwrap();
        // Missing historical rows deliberately violate references in this
        // disposable copy; positive fixtures always use the normal writers.
        corrupted_sql
            .execute_batch("PRAGMA foreign_keys=OFF")
            .unwrap();
        corrupted_sql
            .execute_batch(corruption)
            .unwrap_or_else(|error| panic!("{corruption}: {error}"));
        assert!(
            matches!(
                corrupted_store.accept_start_command(
                    &session, "owner", id, 1, "start", 90000, 10000
                ),
                Err(LifecycleError::CorruptStoredData)
            ),
            "{corruption}"
        );
    }
    assert_eq!(
        store
            .accept_start_command(&session, "owner", id, 1, "start", 90000, 10000)
            .unwrap(),
        accepted
    );
}

#[tokio::test]
async fn start_receipt_observes_ready_cleanup_replacement_and_revoked_policy() {
    let (store, session, sql, _dir) = start_fixture().await;
    let source = fixture::owned_source().await;
    let id = &source.fence.deployment_id;
    let receipt = store
        .accept_start_command(&session, "owner", id, 1, "start", 1800, 10000)
        .unwrap();
    let joined = store
        .accept_start_command(&session, "owner", id, 1, "join", 1801, 11000)
        .unwrap();
    assert!(joined.joined());
    let raw: String = sql
        .query_row(
            "SELECT effective_json FROM effective_revisions WHERE deployment_id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    let effective = mllm_config::effective::decode_effective_snapshot(&raw).unwrap();
    let controls = store
        .resource_policy(&effective.host.name)
        .unwrap()
        .unwrap()
        .controls;
    let limits: Vec<_> = controls
        .domains
        .iter()
        .map(|(domain, d)| mllm_domain::resources::MemoryLimit {
            domain: domain.clone(),
            managed_bytes: d.managed_limit,
            free_reserve_bytes: d.free_reserve,
            host_kv_bytes: d.host_kv_limit,
            parked_bytes: d.parked_limit,
        })
        .collect();
    store
        .arm_step(
            &session,
            receipt.step_id(),
            AdmissionContext::new(
                &source.observations,
                &limits,
                1900,
                controls.observation_ttl_ms,
                controls.max_parked as usize,
            ),
        )
        .unwrap();
    let fake = FakeEngine::with_lifecycle();
    let observation = fake
        .execute_persisted(&RuntimeCommand {
            action: RuntimeAction::Initialize,
            context: store
                .initialize_execution(&session, receipt.step_id())
                .unwrap(),
        })
        .await
        .unwrap();
    store
        .record_owned_launch(
            &session,
            receipt.step_id(),
            &OwnedLaunchReceipt {
                binding_id: observation.binding_id,
                incarnation: observation.incarnation,
                identities: observation.identities.clone(),
                observed_at_ms: observation.observed_at_ms,
                receipt: observation.receipt.clone(),
            },
            1950,
        )
        .unwrap();
    store
        .complete_step(
            &session,
            receipt.step_id(),
            &CompletionEvidence {
                token: observation.token,
                identities: observation.identities,
                observed_at_ms: observation.observed_at_ms,
                control_receipt: Some(observation.receipt),
                milestones: observation.facts,
            },
            1950,
            controls.observation_ttl_ms,
        )
        .unwrap();
    let ready = counts(&sql);
    assert_eq!(
        store
            .accept_start_command(&session, "owner", id, 1, "start", 90000, 10000)
            .unwrap(),
        receipt
    );
    assert!(store
        .accept_start_command(&session, "owner", id, 1, "new-ready", 2000, 10000)
        .is_err());
    assert_eq!(counts(&sql), ready);
    let stop = store
        .accept_ordinary_cleanup(&session, "owner", &source.fence, "stop", 2000, 10000)
        .unwrap();
    let stopping = counts(&sql);
    assert_eq!(
        store
            .accept_start_command(&session, "owner", id, 1, "start", 90000, 10000)
            .unwrap(),
        receipt
    );
    assert_eq!(counts(&sql), stopping);
    assert!(matches!(
        store.accept_start_command(&session, "owner", id, 1, "stop", 2000, 10000),
        Err(LifecycleError::IdempotencyConflict)
    ));
    let (_, context) = store
        .arm_ordinary_cleanup_with_context(&session, &stop.step_id, 2050)
        .unwrap();
    let gone =
        collect_cleanup(&fake, &context.unwrap(), 2100).unwrap();
    store
        .complete_cleanup(
            &session,
            &stop.step_id,
            &gone,
            2150,
            controls.observation_ttl_ms,
        )
        .unwrap();
    let stopped = counts(&sql);
    assert_eq!(
        store
            .accept_start_command(&session, "owner", id, 1, "start", 90000, 10000)
            .unwrap(),
        receipt
    );
    assert_eq!(counts(&sql), stopped);
    let golden: Value = serde_json::from_str(include_str!(
        "../../mllm-config/tests/fixtures/effective-fake-golden.json"
    ))
    .unwrap();
    let mut config = golden["input"]["deployment"].clone();
    let mut host = golden["input"]["host"].clone();
    config["name"] = json!("ordinary");
    config["routes"] = json!(["ordinary-replaced"]);
    host["runtime_profiles"]["local"]["build_fingerprint"] = json!("qualification-fake-v1");
    host["runtime_profiles"]["local"]["security"]["admin_credential_ref"] =
        json!("secret://another-admin");
    let replaced = store
        .replace_stopped_managed_configuration(
            &session,
            "owner",
            "replace",
            id,
            &json!({"expected_revision":1,"config":config}).to_string(),
            &host,
            2200,
        )
        .unwrap();
    // Ordinary Starts retain their existing resource-sharing policy gate; a host
    // policy change does not revoke an already-frozen recipe.
    let snapshot = store
        .resource_policy(&effective.host.name)
        .unwrap()
        .unwrap();
    let mut revoked = snapshot.controls;
    revoked.device_sharing = mllm_config::effective::Sharing::Exclusive;
    for sharing in revoked.device_sharing_overrides.values_mut() {
        *sharing = mllm_config::effective::Sharing::Exclusive;
    }
    store
        .update_resource_policy(
            &session,
            "owner",
            &effective.host.name,
            snapshot.revision,
            "revoke-sharing",
            &revoked,
            &source.observations,
            2250,
        )
        .unwrap();
    let current = store.begin_coordinator_session().unwrap();
    let before = counts(&sql);
    assert_eq!(
        store
            .accept_start_command(&current, "owner", id, 1, "join", 90000, 11000)
            .unwrap(),
        joined
    );
    assert_eq!(
        store
            .accept_start_command(&current, "owner", id, 1, "start", 90000, 10000)
            .unwrap(),
        receipt
    );
    assert!(store
        .accept_start_command(
            &current,
            "owner",
            id,
            replaced.revision,
            "new",
            2300,
            10000
        )
        .is_err());
    assert_eq!(counts(&sql), before);
    assert!(store
        .arm_step(
            &current,
            receipt.step_id(),
            AdmissionContext::new(
                &source.observations,
                &limits,
                2300,
                controls.observation_ttl_ms,
                controls.max_parked as usize
            )
        )
        .is_err());
}

#[tokio::test]
async fn start_receipt_concurrent_same_key_has_one_acceptance() {
    let (store, session, sql, dir) = start_fixture().await;
    let source = fixture::owned_source().await;
    let before = counts(&sql);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let store = Store::open(&dir.path().join("start.sqlite3")).unwrap();
            let session = session.clone();
            let id = source.fence.deployment_id.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store
                    .accept_start_command(&session, "owner", &id, 1, "race", 1800, 10000)
                    .unwrap()
            })
        })
        .collect();
    let receipts: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(receipts[0], receipts[1]);
    assert!(!receipts[0].joined());
    assert_eq!(
        counts(&sql)
            .iter()
            .zip(&before)
            .map(|(a, b)| a - b)
            .collect::<Vec<_>>(),
        [1, 1, 1, 1, 1, 1, 1, 0, 0, 0]
    );
    assert!(store
        .initialize_execution(&session, receipts[0].step_id())
        .is_err());
}

#[tokio::test]
async fn start_receipt_acceptance_replay_join_and_scopes_have_no_execution_effect() {
    let (store, session, sql, _dir) = start_fixture().await;
    let source = fixture::owned_source().await;
    let id = &source.fence.deployment_id;
    let before = counts(&sql);
    let epoch = store.resource_snapshot().unwrap().epoch;
    let accepted = store
        .accept_start_command(&session, "owner", id, 1, "start", 1800, 10000)
        .unwrap();
    assert!(!accepted.joined());
    assert_eq!(
        (
            accepted.revision(),
            accepted.generation(),
            accepted.accepted_at_ms(),
            accepted.deadline_ms()
        ),
        (1, 1, 1800, 10000)
    );
    let after = counts(&sql);
    assert_eq!(
        after
            .iter()
            .zip(&before)
            .map(|(a, b)| a - b)
            .collect::<Vec<_>>(),
        [1, 1, 1, 1, 1, 1, 1, 0, 0, 0]
    );
    assert_eq!(store.resource_snapshot().unwrap().epoch, epoch);
    assert_eq!(
        store
            .accept_start_command(&session, "owner", id, 1, "start", 90000, 10000)
            .unwrap(),
        accepted
    );
    assert_eq!(counts(&sql), after);
    let long = "a".repeat(257);
    for (principal, target, revision, key, now, deadline) in [
        ("", id.as_str(), 1, "start", 1800, 10000),
        (long.as_str(), id.as_str(), 1, "start", 1800, 10000),
        ("owner", "invalid", 1, "start", 1800, 10000),
        ("owner", id.as_str(), 0, "start", 1800, 10000),
        ("owner", id.as_str(), 1, "", 1800, 10000),
        ("owner", id.as_str(), 1, long.as_str(), 1800, 10000),
        ("owner", id.as_str(), 1, "start", -1, 10000),
        ("owner", id.as_str(), 1, "start", 1800, 0),
        ("owner", id.as_str(), 1, "new-expired", 1800, 1800),
    ] {
        assert!(matches!(
            store.accept_start_command(
                &session, principal, target, revision, key, now, deadline
            ),
            Err(LifecycleError::Invalid)
        ));
    }
    assert_eq!(counts(&sql), after);
    let joined = store
        .accept_start_command(&session, "owner", id, 1, "join", 1801, 20000)
        .unwrap();
    assert!(joined.joined());
    assert_eq!(joined.operation_id(), accepted.operation_id());
    assert_eq!(
        (joined.deadline_ms(), joined.accepted_at_ms()),
        (10000, 1800)
    );
    assert_eq!(
        counts(&sql)
            .iter()
            .zip(&after)
            .map(|(a, b)| a - b)
            .collect::<Vec<_>>(),
        [0, 0, 0, 0, 0, 0, 1, 0, 0, 0]
    );
    assert_eq!(store.resource_snapshot().unwrap().epoch, epoch);
    assert_eq!(
        store
            .accept_start_command(&session, "owner", id, 1, "join", 90000, 20000)
            .unwrap(),
        joined
    );
    let principal = store
        .accept_start_command(&session, "another", id, 1, "start", 1802, 11000)
        .unwrap();
    assert!(principal.joined());
    assert_eq!(principal.operation_id(), accepted.operation_id());
    let other = store
        .accept_start_command(
            &session,
            "owner",
            &source.other.deployment_id,
            1,
            "start",
            1800,
            10000,
        )
        .unwrap();
    assert_ne!(other.operation_id(), accepted.operation_id());
    let fixed = counts(&sql);
    for (revision, key, deadline) in [
        (2, "start", 10000),
        (1, "start", 10001),
        (2, "stale", 10000),
    ] {
        let error = store
            .accept_start_command(&session, "owner", id, revision, key, 1802, deadline)
            .unwrap_err();
        if key == "stale" {
            assert!(matches!(error, LifecycleError::RevisionConflict));
        } else {
            assert!(matches!(error, LifecycleError::IdempotencyConflict));
        }
    }
    assert!(matches!(
        store.accept_ordinary_cleanup(&session, "owner", &source.fence, "start", 1802, 10000),
        Err(LifecycleError::Conflict)
    ));
    assert_eq!(counts(&sql), fixed);
    let current = store.begin_coordinator_session().unwrap();
    assert!(matches!(
        store.accept_start_command(&session, "owner", id, 1, "start", 1802, 10000),
        Err(LifecycleError::Stale)
    ));
    assert_eq!(
        store
            .accept_start_command(&current, "owner", id, 1, "start", 90000, 10000)
            .unwrap(),
        accepted
    );
}

fn terminal(sql: &rusqlite::Connection, step: &str) -> (String, String, String, String, String) {
    sql.query_row("SELECT s.state,r.state,o.state,o.error_code,b.state FROM lifecycle_steps s JOIN lifecycle_runs r ON r.operation_id=s.operation_id JOIN operations o ON o.id=s.operation_id JOIN runtime_bindings b ON b.id=s.binding_id WHERE s.id=?1", [step], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap()
}

#[tokio::test]
async fn expired_unarmed_is_atomic_at_deadline_and_replays_history_after_replacement() {
    let (store, session, sql, _dir) = start_fixture().await;
    let source = fixture::owned_source().await;
    let id = &source.fence.deployment_id;
    let receipt = store
        .accept_start_command(&session, "owner", id, 1, "expiry", 1800, 1901)
        .unwrap();
    let plan: String = sql
        .query_row(
            "SELECT step_json FROM lifecycle_steps WHERE id=?1",
            [receipt.step_id()],
            |r| r.get(0),
        )
        .unwrap();
    let before = counts(&sql);
    let ledger = store.resource_snapshot().unwrap();
    assert!(store
        .expire_unarmed_initialize(&session, receipt.step_id(), 1900)
        .is_err());
    assert_eq!(counts(&sql), before);
    sql.execute_batch("CREATE TRIGGER expiry_failure BEFORE INSERT ON management_events WHEN NEW.kind='initialize_expired_unarmed' BEGIN SELECT RAISE(ABORT,'expiry rollback'); END;").unwrap();
    assert!(matches!(
        store.expire_unarmed_initialize(&session, receipt.step_id(), 1901),
        Err(LifecycleError::Sql(_))
    ));
    assert_eq!(counts(&sql), before);
    assert_eq!(
        store.runtime_binding(id).unwrap().unwrap().state,
        "reserved"
    );
    assert_eq!(
        store
            .initialize_status(&session, receipt.step_id(), 1900)
            .unwrap(),
        InitializeStatus::Planned
    );
    sql.execute_batch("DROP TRIGGER expiry_failure").unwrap();
    assert!(store
        .expire_unarmed_initialize(&session, receipt.step_id(), 1901)
        .unwrap());
    assert_eq!(
        terminal(&sql, receipt.step_id()),
        (
            "cancelled".into(),
            "failed".into(),
            "failed".into(),
            "deadline_expired_unarmed".into(),
            "released".into()
        )
    );
    assert_eq!(store.resource_snapshot().unwrap(), ledger);
    assert!(store.runtime_binding(id).unwrap().is_none());
    let state: (String,String,bool,bool) = sql.query_row("SELECT desired_state,observed_state,admission_enabled,dispatch_enabled FROM deployments WHERE id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
    assert_eq!(state, ("stopped".into(), "stopped".into(), false, false));
    assert_eq!(
        store
            .initialize_status(&session, receipt.step_id(), 1901)
            .unwrap(),
        InitializeStatus::ExpiredUnarmed
    );
    let ended = counts(&sql);
    assert!(!store
        .expire_unarmed_initialize(&session, receipt.step_id(), 1902)
        .unwrap());
    assert_eq!(counts(&sql), ended);
    assert_eq!(
        sql.query_row(
            "SELECT step_json FROM lifecycle_steps WHERE id=?1",
            [receipt.step_id()],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        plan
    );
    assert_eq!(
        store
            .accept_start_command(&session, "owner", id, 1, "expiry", 90000, 1901)
            .unwrap(),
        receipt
    );

    let golden: Value = serde_json::from_str(include_str!(
        "../../mllm-config/tests/fixtures/effective-fake-golden.json"
    ))
    .unwrap();
    let mut config = golden["input"]["deployment"].clone();
    let mut host = golden["input"]["host"].clone();
    config["name"] = json!("ordinary");
    config["routes"] = json!(["ordinary-expired-replaced"]);
    host["runtime_profiles"]["local"]["build_fingerprint"] = json!("qualification-fake-v1");
    store
        .replace_stopped_managed_configuration(
            &session,
            "owner",
            "replace-expired",
            id,
            &json!({"expected_revision":1,"config":config}).to_string(),
            &host,
            2000,
        )
        .unwrap();
    let replaced = counts(&sql);
    assert_eq!(
        store
            .initialize_status(&session, receipt.step_id(), 2001)
            .unwrap(),
        InitializeStatus::Superseded
    );
    assert_eq!(
        store
            .accept_start_command(&session, "owner", id, 1, "expiry", 90000, 1901)
            .unwrap(),
        receipt
    );
    assert!(store
        .expire_unarmed_initialize(&session, receipt.step_id(), 2001)
        .is_err());
    assert_eq!(counts(&sql), replaced);
}

#[tokio::test]
async fn expired_unarmed_rejects_contradictions_and_stale_ownership_without_release() {
    let (store, session, sql, _dir) = start_fixture().await;
    let source = fixture::owned_source().await;
    let receipt = store
        .accept_start_command(
            &session,
            "owner",
            &source.fence.deployment_id,
            1,
            "expiry",
            1800,
            1901,
        )
        .unwrap();
    for corruption in [
        "UPDATE deployments SET revision=revision+1 WHERE id=(SELECT deployment_id FROM operations WHERE id=(SELECT operation_id FROM command_receipts WHERE idempotency_key='expiry'))",
        "UPDATE deployments SET current_generation=current_generation+1 WHERE id=(SELECT deployment_id FROM operations WHERE id=(SELECT operation_id FROM command_receipts WHERE idempotency_key='expiry'))",
        "UPDATE lifecycle_claims SET generation=generation+1",
        // A claim naming an operation that is not this one at all.
        "UPDATE lifecycle_claims SET operation_id='foreign-operation'",
        "UPDATE lifecycle_steps SET step_json=json_set(step_json,'$.execution',json('{}')) WHERE id=(SELECT json_extract(response_json,'$.receipt.step_id') FROM command_receipts WHERE idempotency_key='expiry')",
        "UPDATE runtime_bindings SET identities_json='[{}]' WHERE state='reserved'",
        "UPDATE runtime_bindings SET incarnation='contradiction' WHERE state='reserved'",
        "UPDATE endpoint_leases SET port=port+1",
        "UPDATE deployments SET dispatch_enabled=1 WHERE desired_state='ready'",
        "UPDATE deployments SET observed_state='ready' WHERE desired_state='ready'",
        "INSERT INTO request_leases SELECT 'retained-lease',deployment_id,revision,generation,session_id,'uncertain' FROM lifecycle_runs WHERE operation_id=(SELECT operation_id FROM command_receipts WHERE idempotency_key='expiry')",
        "INSERT INTO resource_grants SELECT 'retained-grant',deployment_id,operation_id,'{}',999999 FROM lifecycle_runs WHERE operation_id=(SELECT operation_id FROM command_receipts WHERE idempotency_key='expiry')",
        "INSERT INTO resource_owners SELECT deployment_id,'{}' FROM lifecycle_runs WHERE operation_id=(SELECT operation_id FROM command_receipts WHERE idempotency_key='expiry')",
        "INSERT INTO owned_launch_associations SELECT id,binding_id,'contradiction','{}' FROM lifecycle_steps WHERE operation_id=(SELECT operation_id FROM command_receipts WHERE idempotency_key='expiry')",
        "INSERT INTO lifecycle_evidence SELECT id,'{}',0 FROM lifecycle_steps WHERE operation_id=(SELECT operation_id FROM command_receipts WHERE idempotency_key='expiry')",
        "UPDATE lifecycle_steps SET grant_id='retained-grant' WHERE operation_id=(SELECT operation_id FROM command_receipts WHERE idempotency_key='expiry')",
    ] {
        let copy = tempfile::tempdir().unwrap();
        let path = copy.path().join("corrupt.sqlite3");
        sql.execute("VACUUM INTO ?1",[path.to_str().unwrap()]).unwrap();
        let reopened = Store::open(&path).unwrap();
        let corrupt = rusqlite::Connection::open(&path).unwrap();
        corrupt.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        corrupt.execute_batch(corruption).unwrap_or_else(|e|panic!("{corruption}: {e}"));
        let before = counts(&corrupt);
        assert!(reopened.expire_unarmed_initialize(&session,receipt.step_id(),1901).is_err(),"{corruption}");
        assert_eq!(counts(&corrupt),before,"{corruption}");
        assert_eq!(corrupt.query_row("SELECT state FROM runtime_bindings WHERE id=?1",[receipt.binding_id()],|r|r.get::<_,String>(0)).unwrap(),"reserved");
    }
    let current = store.begin_coordinator_session().unwrap();
    let before = counts(&sql);
    assert!(store
        .expire_unarmed_initialize(&session, receipt.step_id(), 1901)
        .is_err());
    assert!(store
        .expire_unarmed_initialize(&current, receipt.step_id(), 1901)
        .is_err());
    assert_eq!(counts(&sql), before);
}

fn limits(
    store: &Store,
    sql: &rusqlite::Connection,
    id: &str,
) -> (Vec<mllm_domain::resources::MemoryLimit>, i64, usize) {
    let raw: String = sql
        .query_row(
            "SELECT effective_json FROM effective_revisions WHERE deployment_id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    let effective = mllm_config::effective::decode_effective_snapshot(&raw).unwrap();
    let controls = store
        .resource_policy(&effective.host.name)
        .unwrap()
        .unwrap()
        .controls;
    (
        controls
            .domains
            .iter()
            .map(|(domain, d)| mllm_domain::resources::MemoryLimit {
                domain: domain.clone(),
                managed_bytes: d.managed_limit,
                free_reserve_bytes: d.free_reserve,
                host_kv_bytes: d.host_kv_limit,
                parked_bytes: d.parked_limit,
            })
            .collect(),
        controls.observation_ttl_ms,
        controls.max_parked as usize,
    )
}

#[tokio::test]
async fn expired_unarmed_and_arm_serialize_on_independent_connections() {
    let (store, session, sql, dir) = start_fixture().await;
    let source = fixture::owned_source().await;
    let receipt = store
        .accept_start_command(
            &session,
            "owner",
            &source.fence.deployment_id,
            1,
            "expiry",
            1800,
            1901,
        )
        .unwrap();
    let (limits, ttl, max_parked) = limits(&store, &sql, &source.fence.deployment_id);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let arm_store = Store::open(&dir.path().join("start.sqlite3")).unwrap();
    let arm_session = session.clone();
    let arm_step = receipt.step_id().to_owned();
    let arm_barrier = barrier.clone();
    let observations = source.observations.clone();
    let arm = std::thread::spawn(move || {
        arm_barrier.wait();
        arm_store.arm_initialize_with_context(
            &arm_session,
            &arm_step,
            AdmissionContext::new(&observations, &limits, 1900, ttl, max_parked),
        )
    });
    let expire_store = Store::open(&dir.path().join("start.sqlite3")).unwrap();
    let expire_session = session.clone();
    let expire_step = receipt.step_id().to_owned();
    let expiry = std::thread::spawn(move || {
        barrier.wait();
        expire_store.expire_unarmed_initialize(&expire_session, &expire_step, 1901)
    });
    let armed = arm.join().unwrap();
    let expired = expiry.join().unwrap();
    assert_ne!(armed.is_ok(), expired.is_ok());
    if armed.is_ok() {
        let retained = store.resource_snapshot().unwrap();
        assert!(retained.owners.contains_key(&source.fence.deployment_id));
        assert_eq!(
            store
                .runtime_binding(&source.fence.deployment_id)
                .unwrap()
                .unwrap()
                .state,
            "uncertain"
        );
        assert!(store
            .expire_unarmed_initialize(&session, receipt.step_id(), 2000)
            .is_err());
        assert_eq!(store.resource_snapshot().unwrap(), retained);
    } else {
        assert!(expired.unwrap());
        assert!(store.resource_snapshot().unwrap().owners.is_empty());
        assert!(store
            .runtime_binding(&source.fence.deployment_id)
            .unwrap()
            .is_none());
    }
}

#[tokio::test]
async fn expired_unarmed_never_releases_armed_without_association_and_proves_retry_state() {
    let (store, session, sql, _dir) = start_fixture().await;
    let source = fixture::owned_source().await;
    let receipt = store
        .accept_start_command(
            &session,
            "owner",
            &source.fence.deployment_id,
            1,
            "expiry",
            1800,
            1901,
        )
        .unwrap();
    let (limits, ttl, max_parked) = limits(&store, &sql, &source.fence.deployment_id);
    store
        .arm_initialize_with_context(
            &session,
            receipt.step_id(),
            AdmissionContext::new(&source.observations, &limits, 1900, ttl, max_parked),
        )
        .unwrap();
    let before = counts(&sql);
    let retained = store.resource_snapshot().unwrap();
    assert!(store
        .expire_unarmed_initialize(&session, receipt.step_id(), 1901)
        .is_err());
    assert_eq!(counts(&sql), before);
    assert_eq!(store.resource_snapshot().unwrap(), retained);
    assert_eq!(
        sql.query_row(
            "SELECT COUNT(*) FROM owned_launch_associations WHERE step_id=?1",
            [receipt.step_id()],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        store
            .runtime_binding(&source.fence.deployment_id)
            .unwrap()
            .unwrap()
            .state,
        "uncertain"
    );

    let receipt = store
        .accept_start_command(
            &session,
            "owner",
            &source.other.deployment_id,
            1,
            "other-expiry",
            1800,
            1901,
        )
        .unwrap();
    store
        .expire_unarmed_initialize(&session, receipt.step_id(), 1901)
        .unwrap();
    // A failed row alone must never be treated as a successful exact retry.
    for corruption in [
        "UPDATE operations SET error_code='arbitrary_failure' WHERE id=?1",
        "UPDATE lifecycle_steps SET state='planned' WHERE operation_id=?1",
        "UPDATE lifecycle_runs SET state='queued' WHERE operation_id=?1",
        "UPDATE runtime_bindings SET state='reserved' WHERE id=(SELECT binding_id FROM lifecycle_steps WHERE operation_id=?1)",
        "INSERT INTO lifecycle_claims SELECT deployment_id,operation_id,revision,generation FROM lifecycle_runs WHERE operation_id=?1",
    ] {
        let copy = tempfile::tempdir().unwrap();
        let path = copy.path().join("terminal.sqlite3");
        sql.execute("VACUUM INTO ?1",[path.to_str().unwrap()]).unwrap();
        let reopened = Store::open(&path).unwrap();
        let corrupt = rusqlite::Connection::open(path).unwrap();
        corrupt.execute(corruption,[receipt.operation_id()]).unwrap();
        let before = counts(&corrupt);
        assert!(reopened.expire_unarmed_initialize(&session,receipt.step_id(),1902).is_err(),"{corruption}");
        assert_eq!(counts(&corrupt),before);
    }
}

#[tokio::test]
async fn unarmed_stop_rejects_nontext_history_as_internal_corruption() {
    let (store, session, sql, _dir) = start_fixture().await;
    let source = fixture::owned_source().await;
    let id = &source.fence.deployment_id;
    store
        .accept_start_command(&session, "owner", id, 1, "start", 1800, 10000)
        .unwrap();
    let stop = store
        .accept_ordinary_stop_command(&session, "owner", id, 1, "stop", 1900, 10000)
        .unwrap();
    for corruption in [
        "UPDATE lifecycle_steps SET step_json=CAST(step_json AS BLOB) WHERE operation_id=?1",
        "UPDATE lifecycle_steps SET step_json=step_json || printf('%1048577s',' ') WHERE operation_id=?1",
        "UPDATE operations SET kind=CAST(kind AS BLOB) WHERE id=?1",
        "UPDATE operations SET kind=printf('%1048577s','x') WHERE id=?1",
    ] {
        let copy=tempfile::tempdir().unwrap();let path=copy.path().join("corrupt.sqlite3");sql.execute("VACUUM INTO ?1",[path.to_str().unwrap()]).unwrap();
        let reopened=Store::open(&path).unwrap();let corrupt=rusqlite::Connection::open(path).unwrap();corrupt.execute(corruption,[&stop.operation_id]).unwrap();
        let before=state(&corrupt);
        assert!(matches!(reopened.accept_ordinary_stop_command(&session,"owner",id,1,"stop",1900,10000),Err(LifecycleError::CorruptStoredData)),"{corruption}");
        assert_eq!(state(&corrupt),before);
    }
}

#[tokio::test]
async fn unarmed_stop_serializes_with_arm_and_expiry_on_independent_connections() {
    for competing_arm in [true, false] {
        let (store, session, sql, dir) = start_fixture().await;
        let source = fixture::owned_source().await;
        let id = source.fence.deployment_id.clone();
        let start = store
            .accept_start_command(&session, "owner", &id, 1, "start", 1800, 1901)
            .unwrap();
        let (limits, ttl, max_parked) = limits(&store, &sql, &id);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let other = Store::open(&dir.path().join("start.sqlite3")).unwrap();
        let other_session = session.clone();
        let other_step = start.step_id().to_owned();
        let other_barrier = barrier.clone();
        let observations = source.observations.clone();
        let competing = std::thread::spawn(move || {
            other_barrier.wait();
            if competing_arm {
                other
                    .arm_initialize_with_context(
                        &other_session,
                        &other_step,
                        AdmissionContext::new(&observations, &limits, 1900, ttl, max_parked),
                    )
                    .map(|_| ())
            } else {
                other
                    .expire_unarmed_initialize(&other_session, &other_step, 1901)
                    .map(|_| ())
            }
        });
        let stopper = Store::open(&dir.path().join("start.sqlite3")).unwrap();
        let stop_session = session.clone();
        let stop_id = id.clone();
        let stop = std::thread::spawn(move || {
            barrier.wait();
            stopper.accept_ordinary_stop_command(
                &stop_session,
                "owner",
                &stop_id,
                1,
                "stop",
                1900,
                10000,
            )
        });
        let competing = competing.join().unwrap();
        let stop = stop.join().unwrap();
        assert_ne!(competing.is_ok(), stop.is_ok());
        if let Ok(stop) = stop {
            assert!(store
                .complete_unarmed_stop(&session, &stop.step_id)
                .unwrap());
            assert!(store.resource_snapshot().unwrap().owners.is_empty());
            assert!(store.runtime_binding(&id).unwrap().is_none());
        } else if competing_arm {
            let before = state(&sql);
            assert!(store
                .accept_ordinary_stop_command(&session, "owner", &id, 1, "retry", 1900, 10000)
                .is_err());
            assert_eq!(state(&sql), before);
            assert_eq!(
                store.runtime_binding(&id).unwrap().unwrap().state,
                "uncertain"
            );
            assert!(store.resource_snapshot().unwrap().owners.contains_key(&id));
        } else {
            assert_eq!(
                sql.query_row(
                    "SELECT error_code FROM operations WHERE id=?1",
                    [start.operation_id()],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
                "deadline_expired_unarmed"
            );
        }
    }
}

#[tokio::test]
async fn unarmed_stop_terminal_history_rejects_contradictions() {
    let (store, session, sql, _dir) = start_fixture().await;
    let source = fixture::owned_source().await;
    let id = &source.fence.deployment_id;
    let start = store
        .accept_start_command(&session, "owner", id, 1, "start", 1800, 10000)
        .unwrap();
    let stop = store
        .accept_ordinary_stop_command(&session, "owner", id, 1, "stop", 1900, 10000)
        .unwrap();
    store
        .complete_unarmed_stop(&session, &stop.step_id)
        .unwrap();
    for corruption in [
        "UPDATE operations SET error_code='deadline_expired_unarmed' WHERE id='$SOURCE'",
        "UPDATE lifecycle_steps SET state='planned' WHERE operation_id='$SOURCE'",
        "UPDATE lifecycle_runs SET state='queued' WHERE operation_id='$SOURCE'",
        "UPDATE operations SET state='pending' WHERE id='$STOP'",
        "UPDATE lifecycle_steps SET state='planned' WHERE operation_id='$STOP'",
        "UPDATE lifecycle_runs SET plan_json=json_set(plan_json,'$.handoffs[0].steps[0].state','armed') WHERE operation_id='$STOP'",
        "UPDATE runtime_bindings SET state='reserved' WHERE id=(SELECT binding_id FROM lifecycle_steps WHERE operation_id='$SOURCE')",
        "INSERT INTO lifecycle_claims SELECT deployment_id,operation_id,revision,generation FROM lifecycle_runs WHERE operation_id='$STOP'",
        "INSERT INTO lifecycle_claims SELECT deployment_id,operation_id,revision,generation FROM lifecycle_runs WHERE operation_id='$SOURCE'",
        "INSERT INTO resource_owners SELECT deployment_id,'{}' FROM lifecycle_runs WHERE operation_id='$SOURCE'",
        "INSERT INTO request_leases SELECT 'old-lease',deployment_id,revision,generation,session_id,'uncertain' FROM lifecycle_runs WHERE operation_id='$SOURCE'",
        "INSERT INTO lifecycle_evidence SELECT id,'{}',0 FROM lifecycle_steps WHERE operation_id='$STOP'",
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.kind','unknown') WHERE idempotency_key='stop'",
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.kind','ordinary_cleanup') WHERE idempotency_key='stop'",
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.extra',1) WHERE idempotency_key='stop'",
        "UPDATE operations SET kind='unknown' WHERE id='$STOP'",
        "INSERT INTO lifecycle_steps SELECT 'extra-step',operation_id,1,deployment_id,binding_id,session_id,'planned',step_json,NULL FROM lifecycle_steps WHERE operation_id='$STOP'",
    ] {
        let copy=tempfile::tempdir().unwrap();let path=copy.path().join("corrupt.sqlite3");sql.execute("VACUUM INTO ?1",[path.to_str().unwrap()]).unwrap();
        let reopened=Store::open(&path).unwrap();let corrupt=rusqlite::Connection::open(path).unwrap();corrupt.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        corrupt.execute_batch(&corruption.replace("$SOURCE",start.operation_id()).replace("$STOP",&stop.operation_id)).unwrap();
        let before=state(&corrupt);
        assert!(reopened.complete_unarmed_stop(&session,&stop.step_id).is_err(),"{corruption}");
        assert!(matches!(reopened.accept_ordinary_stop_command(&session,"owner",id,1,"stop",90000,10000),Err(LifecycleError::CorruptStoredData)),"{corruption}");
        assert_eq!(state(&corrupt),before,"{corruption}");
    }
}

fn state(sql: &rusqlite::Connection) -> Vec<String> {
    [
        "deployments",
        "generation_history",
        "operations",
        "lifecycle_runs",
        "lifecycle_steps",
        "runtime_bindings",
        "endpoint_leases",
        "lifecycle_claims",
        "command_receipts",
        "management_events",
        "resource_ledger_meta",
        "resource_grants",
        "resource_owners",
        "request_leases",
        "owned_launch_associations",
        "lifecycle_evidence",
    ]
    .into_iter()
    .flat_map(|table| {
        let mut statement = sql
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .unwrap();
        let count = statement.column_count();
        statement
            .query_map([], |r| {
                Ok((0..count)
                    .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                    .collect::<Vec<_>>()
                    .join("|"))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    })
    .collect()
}

#[tokio::test]
async fn unarmed_stop_contradictions_fail_closed_at_acceptance_and_completion() {
    let (store, session, sql, _dir) = start_fixture().await;
    let source = fixture::owned_source().await;
    let id = &source.fence.deployment_id;
    let start = store
        .accept_start_command(&session, "owner", id, 1, "start", 1800, 10000)
        .unwrap();
    for accepted in [false, true] {
        let stop = accepted.then(|| {
            store
                .accept_ordinary_stop_command(&session, "owner", id, 1, "stop", 1900, 10000)
                .unwrap()
        });
        for corruption in [
            "UPDATE deployments SET revision=revision+1 WHERE id=(SELECT deployment_id FROM lifecycle_runs WHERE operation_id='$SOURCE')",
            "UPDATE deployments SET current_generation=current_generation+1 WHERE id=(SELECT deployment_id FROM lifecycle_runs WHERE operation_id='$SOURCE')",
            "UPDATE lifecycle_claims SET generation=generation+1",
            "DELETE FROM lifecycle_claims",
            "UPDATE lifecycle_claims SET operation_id='wrong-claim'",
            "UPDATE lifecycle_steps SET step_json=json_set(step_json,'$.execution',json('{}')) WHERE operation_id='$SOURCE'",
            "UPDATE lifecycle_steps SET state='armed' WHERE operation_id='$SOURCE'",
            "UPDATE lifecycle_steps SET session_id='other-session' WHERE operation_id='$SOURCE'",
            "UPDATE runtime_bindings SET identities_json='[{}]' WHERE state='reserved'",
            "UPDATE runtime_bindings SET incarnation='contradiction' WHERE state='reserved'",
            "UPDATE runtime_bindings SET ownership='attached' WHERE state='reserved'",
            "UPDATE endpoint_leases SET port=port+1",
            "DELETE FROM endpoint_leases",
            "UPDATE deployments SET dispatch_enabled=1 WHERE id=(SELECT deployment_id FROM lifecycle_runs WHERE operation_id='$SOURCE')",
            "UPDATE deployments SET observed_state='ready' WHERE id=(SELECT deployment_id FROM lifecycle_runs WHERE operation_id='$SOURCE')",
            "UPDATE deployments SET suspended=1 WHERE id=(SELECT deployment_id FROM lifecycle_runs WHERE operation_id='$SOURCE')",
            "INSERT INTO request_leases SELECT 'retained-lease',deployment_id,revision,generation,session_id,'uncertain' FROM lifecycle_runs WHERE operation_id='$SOURCE'",
            "INSERT INTO resource_grants SELECT 'retained-grant',deployment_id,operation_id,'{}',999999 FROM lifecycle_runs WHERE operation_id='$SOURCE'",
            "INSERT INTO resource_owners SELECT deployment_id,'{}' FROM lifecycle_runs WHERE operation_id='$SOURCE'",
            "INSERT INTO owned_launch_associations SELECT id,binding_id,'contradiction','{}' FROM lifecycle_steps WHERE operation_id='$SOURCE'",
            "INSERT INTO lifecycle_evidence SELECT id,'{}',0 FROM lifecycle_steps WHERE operation_id='$SOURCE'",
            "UPDATE lifecycle_steps SET grant_id='retained-grant' WHERE operation_id='$SOURCE'",
            "INSERT INTO lifecycle_steps SELECT 'extra-step',operation_id,1,deployment_id,binding_id,session_id,'planned',step_json,NULL FROM lifecycle_steps WHERE operation_id='$SOURCE'",
            "INSERT INTO lifecycle_steps SELECT 'extra-step',operation_id,1,deployment_id,binding_id,session_id,'cancelled',step_json,NULL FROM lifecycle_steps WHERE operation_id='$SOURCE'",
            "INSERT INTO lifecycle_steps SELECT 'extra-step','foreign-operation',99,deployment_id,binding_id,session_id,'planned',step_json,NULL FROM lifecycle_steps WHERE operation_id='$SOURCE'",
            "INSERT INTO lifecycle_steps SELECT 'extra-step','foreign-operation',99,deployment_id,binding_id,session_id,'cancelled',step_json,NULL FROM lifecycle_steps WHERE operation_id='$SOURCE'",
        ] {
            let copy=tempfile::tempdir().unwrap();let path=copy.path().join("corrupt.sqlite3");
            sql.execute("VACUUM INTO ?1",[path.to_str().unwrap()]).unwrap();
            let reopened=Store::open(&path).unwrap();let corrupt=rusqlite::Connection::open(path).unwrap();
            corrupt.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
            corrupt.execute_batch(&corruption.replace("$SOURCE",start.operation_id())).unwrap();
            let before=state(&corrupt);
            let denied=if let Some(stop)=&stop {reopened.complete_unarmed_stop(&session,&stop.step_id).is_err()}else {reopened.accept_ordinary_stop_command(&session,"owner",id,1,"stop",1900,10000).is_err()};
            assert!(denied,"accepted={accepted}: {corruption}");
            assert_eq!(state(&corrupt),before,"accepted={accepted}: {corruption}");
        }
    }
    let stop = store
        .ordinary_stop_command_receipt(&session, "owner", id, 1, "stop", 10000)
        .unwrap()
        .unwrap();
    for corruption in [
        "UPDATE lifecycle_steps SET state='armed' WHERE operation_id=?1",
        "UPDATE lifecycle_steps SET state='uncertain' WHERE operation_id=?1",
        "UPDATE lifecycle_steps SET state='cancelled' WHERE operation_id=?1",
        "UPDATE lifecycle_steps SET grant_id='retained-grant' WHERE operation_id=?1",
        "INSERT INTO lifecycle_evidence SELECT id,'{}',0 FROM lifecycle_steps WHERE operation_id=?1",
        "INSERT INTO resource_grants SELECT 'retained-grant',deployment_id,operation_id,'{}',999999 FROM lifecycle_runs WHERE operation_id=?1",
        "UPDATE lifecycle_steps SET step_json=json_set(step_json,'$.source.generation',999) WHERE operation_id=?1",
        "UPDATE lifecycle_steps SET step_json=json_set(step_json,'$.receipt.incarnation','wrong') WHERE operation_id=?1",
        "INSERT INTO lifecycle_steps SELECT 'extra-stop',operation_id,1,deployment_id,binding_id,session_id,'planned',step_json,NULL FROM lifecycle_steps WHERE operation_id=?1",
    ] {
        let copy=tempfile::tempdir().unwrap();let path=copy.path().join("corrupt.sqlite3");sql.execute("VACUUM INTO ?1",[path.to_str().unwrap()]).unwrap();
        let reopened=Store::open(&path).unwrap();let corrupt=rusqlite::Connection::open(path).unwrap();corrupt.execute_batch("PRAGMA foreign_keys=OFF").unwrap();corrupt.execute(corruption,[&stop.operation_id]).unwrap();
        let before=state(&corrupt);
        assert!(reopened.complete_unarmed_stop(&session,&stop.step_id).is_err(),"{corruption}");
        assert_eq!(state(&corrupt),before);
    }
    let current = store.begin_coordinator_session().unwrap();
    let before = state(&sql);
    assert!(store
        .complete_unarmed_stop(&session, &stop.step_id)
        .is_err());
    assert!(store
        .complete_unarmed_stop(&current, &stop.step_id)
        .is_err());
    assert!(store
        .accept_ordinary_stop_command(&current, "owner", id, 1, "fresh", 1900, 10000)
        .is_err());
    assert_eq!(state(&sql), before);
    assert_eq!(
        store
            .ordinary_stop_command_receipt(&current, "owner", id, 1, "stop", 10000)
            .unwrap()
            .unwrap(),
        stop
    );
}

#[tokio::test]
async fn unarmed_stop_rolls_back_receipt_and_events_and_replays_after_replacement() {
    let (store, session, sql, _dir) = start_fixture().await;
    let source = fixture::owned_source().await;
    let id = &source.fence.deployment_id;
    let start = store
        .accept_start_command(&session, "owner", id, 1, "start", 1800, 1901)
        .unwrap();
    for (table, condition) in [
        ("command_receipts", "NEW.idempotency_key='stop'"),
        (
            "management_events",
            "NEW.kind='ordinary_unarmed_stop_accepted'",
        ),
    ] {
        sql.execute_batch(&format!("CREATE TRIGGER failure BEFORE INSERT ON {table} WHEN {condition} BEGIN SELECT RAISE(ABORT,'rollback'); END;")).unwrap();
        let before = state(&sql);
        assert!(matches!(
            store.accept_ordinary_stop_command(&session, "owner", id, 1, "stop", 1900, 10000),
            Err(LifecycleError::Sql(_))
        ));
        assert_eq!(state(&sql), before);
        sql.execute_batch("DROP TRIGGER failure").unwrap();
    }
    let stop = store
        .accept_ordinary_stop_command(&session, "owner", id, 1, "stop", 1900, 10000)
        .unwrap();
    let ledger = store.resource_snapshot().unwrap();
    let before = state(&sql);
    assert!(store
        .expire_unarmed_initialize(&session, start.step_id(), 1901)
        .is_err());
    assert_eq!(state(&sql), before);
    for (key, action) in [("start", "stop"), ("stop", "start")] {
        let error = if action == "stop" {
            store
                .accept_ordinary_stop_command(&session, "owner", id, 1, key, 1900, 1901)
                .unwrap_err()
        } else {
            store
                .accept_start_command(&session, "owner", id, 1, key, 1900, 10000)
                .unwrap_err()
        };
        assert!(matches!(error, LifecycleError::IdempotencyConflict));
    }
    assert!(store
        .accept_ordinary_stop_command(&session, "owner", id, 1, "different", 1900, 10000)
        .is_err());
    sql.execute_batch("CREATE TRIGGER failure BEFORE INSERT ON management_events WHEN NEW.kind='ordinary_unarmed_stop_completed' BEGIN SELECT RAISE(ABORT,'rollback'); END;").unwrap();
    let before = state(&sql);
    assert!(matches!(
        store.complete_unarmed_stop(&session, &stop.step_id),
        Err(LifecycleError::Sql(_))
    ));
    assert_eq!(state(&sql), before);
    sql.execute_batch("DROP TRIGGER failure").unwrap();
    assert!(store
        .complete_unarmed_stop(&session, &stop.step_id)
        .unwrap());
    assert_eq!(store.resource_snapshot().unwrap(), ledger);
    let before = state(&sql);
    assert!(!store
        .complete_unarmed_stop(&session, &stop.step_id)
        .unwrap());
    assert_eq!(
        store
            .accept_ordinary_stop_command(&session, "owner", id, 1, "stop", 90000, 10000)
            .unwrap(),
        stop
    );
    assert_eq!(state(&sql), before);
    let golden: Value = serde_json::from_str(include_str!(
        "../../mllm-config/tests/fixtures/effective-fake-golden.json"
    ))
    .unwrap();
    let mut config = golden["input"]["deployment"].clone();
    let mut host = golden["input"]["host"].clone();
    config["name"] = json!("ordinary");
    config["routes"] = json!(["ordinary-stop-replaced"]);
    host["runtime_profiles"]["local"]["build_fingerprint"] = json!("qualification-fake-v1");
    host["runtime_profiles"]["local"]["security"]["admin_credential_ref"] =
        json!("secret://another-admin");
    store
        .replace_stopped_managed_configuration(
            &session,
            "owner",
            "replace",
            id,
            &json!({"expected_revision":1,"config":config}).to_string(),
            &host,
            2000,
        )
        .unwrap();
    let replacement = store
        .accept_start_command(&session, "owner", id, 2, "replacement", 2000, 10000)
        .unwrap();
    let (limits, ttl, max_parked) = limits(&store, &sql, id);
    store
        .arm_initialize_with_context(
            &session,
            replacement.step_id(),
            AdmissionContext::new(&source.observations, &limits, 2000, ttl, max_parked),
        )
        .unwrap();
    let retained = store.resource_snapshot().unwrap();
    assert!(retained.owners.contains_key(id));
    assert_eq!(
        store
            .accept_ordinary_stop_command(&session, "owner", id, 1, "stop", 90000, 10000)
            .unwrap(),
        stop
    );
    assert!(!store
        .complete_unarmed_stop(&session, &stop.step_id)
        .unwrap());
    assert_eq!(
        store.runtime_binding(id).unwrap().unwrap().id,
        replacement.binding_id()
    );
    assert_eq!(store.resource_snapshot().unwrap(), retained);
    assert_eq!(
        store
            .initialize_status(&session, start.step_id(), 2000)
            .unwrap(),
        mllm_store::ordinary_lifecycle::worker::InitializeStatus::Superseded
    );
    let new_session = store.begin_coordinator_session().unwrap();
    assert_eq!(
        store
            .accept_ordinary_stop_command(&new_session, "owner", id, 1, "stop", 90000, 10000)
            .unwrap(),
        stop
    );
    assert!(!store
        .complete_unarmed_stop(&new_session, &stop.step_id)
        .unwrap());
    assert!(store
        .complete_unarmed_stop(&session, &stop.step_id)
        .is_err());
}

// The handoff must record that the predecessor never armed. A generic cleanup
// validator accepting an armed historical step must not permit this release.
#[tokio::test]
async fn unarmed_stop_rejects_armed_predecessor_history_before_release() {
    let (store, session, sql, _dir) = start_fixture().await;
    let source = fixture::owned_source().await;
    store
        .accept_start_command(
            &session,
            "owner",
            &source.fence.deployment_id,
            1,
            "start",
            1800,
            10000,
        )
        .unwrap();
    let stop = store
        .accept_ordinary_stop_command(
            &session,
            "owner",
            &source.fence.deployment_id,
            1,
            "stop",
            1900,
            10000,
        )
        .unwrap();
    sql.execute("UPDATE lifecycle_runs SET plan_json=json_set(plan_json,'$.handoffs[0].steps[0].state','armed') WHERE operation_id=?1",[&stop.operation_id]).unwrap();
    let before = counts(&sql);
    assert!(
        store
            .complete_unarmed_stop(&session, &stop.step_id)
            .is_err(),
        "armed predecessor history must deny no-effect release"
    );
    assert_eq!(counts(&sql), before);
    assert_eq!(
        store
            .runtime_binding(&source.fence.deployment_id)
            .unwrap()
            .unwrap()
            .state,
        "reserved"
    );
}

// A start proves the deployment's own stored configuration. Tampered revision
// history and a rewritten route must both be refused without writing anything.
// The host-profile mismatches this test also covered were refusals the
// qualification catalog produced, and went with it under ADR 0011.
#[tokio::test]
async fn ordinary_rejects_revision_history_and_route_tampering() {
    let f = fixture::fixture();
    let fence = fixture::managed(&f, "ordinary");
    let raw: String = f
        .sql
        .query_row(
            "SELECT effective_json FROM effective_revisions WHERE deployment_id=?1",
            [&fence.deployment_id],
            |r| r.get(0),
        )
        .unwrap();
    let mut altered: Value = serde_json::from_str(&raw).unwrap();
    altered["routes"] = json!(["silently-replaced-route"]);
    f.sql
        .execute(
            "UPDATE effective_revisions SET effective_json=?2 WHERE deployment_id=?1",
            rusqlite::params![fence.deployment_id, altered.to_string()],
        )
        .unwrap();
    let before = full_counts(&f.sql);
    assert!(
        f.store
            .accept_start(&f.session, &fence, 1800, 10000)
            .is_err()
    );
    assert_eq!(full_counts(&f.sql), before);
    f.sql
        .execute(
            "UPDATE effective_revisions SET effective_json=?2 WHERE deployment_id=?1",
            rusqlite::params![fence.deployment_id, raw],
        )
        .unwrap();
    f.sql
        .execute(
            "UPDATE deployment_routes SET route='corrupt-route' WHERE deployment_id=?1",
            [&fence.deployment_id],
        )
        .unwrap();
    let before = full_counts(&f.sql);
    assert!(
        f.store
            .accept_start(&f.session, &fence, 1800, 10000)
            .is_err()
    );
    assert_eq!(full_counts(&f.sql), before);
}

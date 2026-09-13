use super::*;
use crate::{AcceptDeployment, Store};
use mllm_domain::{DeploymentId, LifecycleState, OperationId};

// Fixture-only readiness: production readiness must come from completion evidence.
fn ready_deployment(store: &Store, name: &str) -> String {
    let id = DeploymentId::new();
    store
        .accept_deployment(AcceptDeployment {
            id,
            name: name.into(),
            kind: "model".into(),
            route_model_id: Some(name.into()),
            desired_state: LifecycleState::Ready,
            schema_version: 1,
            idempotency_key: name.into(),
            initial_operation_id: OperationId(format!("op-{name}")),
        })
        .unwrap();
    store
        .set_observed_state(&id.to_string(), LifecycleState::Ready)
        .unwrap();
    let gate: i64 = store
        .conn
        .query_row(
            "SELECT dispatch_enabled FROM deployments WHERE id=?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(gate, 0);
    store
        .conn
        .execute(
            "UPDATE deployments SET dispatch_enabled=1 WHERE id=?1",
            [id.to_string()],
        )
        .unwrap();
    id.to_string()
}

fn wanted(deployment: &str) -> DispatchRequest<'_> {
    DispatchRequest {
        deployment_id: deployment,
        revision: 1,
        generation: 1,
        max_per_deployment: 2,
        max_total: 4,
    }
}

#[test]
fn closure_sees_registered_work_and_rejects_later_dispatch() {
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let deployment = ready_deployment(&store, "a");
    let ticket = store.grant_dispatch(&session, wanted(&deployment)).unwrap();
    assert_eq!(ticket.deployment_id(), deployment);
    assert_eq!(ticket.generation(), 1);
    assert_eq!(
        store.close_dispatch(&session, &deployment, 1, 1).unwrap(),
        1
    );
    assert!(matches!(
        store.grant_dispatch(&session, wanted(&deployment)),
        Err(DispatchError::Closed)
    ));
}

#[test]
fn uncertainty_survives_ticket_drop_and_completion_is_idempotent() {
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let deployment = ready_deployment(&store, "a");
    let ticket = store.grant_dispatch(&session, wanted(&deployment)).unwrap();
    let ticket_copy = ticket.clone();
    drop(ticket);
    assert_eq!(store.pending_dispatches(&deployment).unwrap().len(), 1);
    assert!(store
        .mark_dispatch_uncertain(&session, &ticket_copy)
        .unwrap());
    let leases = store.pending_dispatches(&deployment).unwrap();
    assert_eq!(leases.len(), 1);
    assert!(leases[0].uncertain);
    // A completion correlated with the original ticket can settle draining work.
    store.bump_generation(&deployment).unwrap();
    assert!(store.finish_dispatch(&session, &ticket_copy).unwrap());
    assert!(!store.finish_dispatch(&session, &ticket_copy).unwrap());
    assert!(store.pending_dispatches(&deployment).unwrap().is_empty());
}

#[test]
fn catalog_survives_runtime_gate_closure() {
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let deployment = ready_deployment(&store, "a");
    store.close_dispatch(&session, &deployment, 1, 1).unwrap();
    assert_eq!(
        store.list_enabled_route_ids().unwrap(),
        vec!["a".to_string()]
    );
    assert!(store.find_deployment_by_route("a").unwrap().is_some());
}

#[test]
fn stale_or_suspended_deployments_cannot_dispatch() {
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let deployment = ready_deployment(&store, "a");
    let mut request = wanted(&deployment);
    request.revision = 2;
    assert!(matches!(
        store.grant_dispatch(&session, request),
        Err(DispatchError::Conflict)
    ));
    request = wanted(&deployment);
    request.generation = 2;
    assert!(matches!(
        store.grant_dispatch(&session, request),
        Err(DispatchError::Conflict)
    ));
    store.set_suspended(&deployment, true).unwrap();
    assert!(matches!(
        store.grant_dispatch(&session, wanted(&deployment)),
        Err(DispatchError::Closed)
    ));
    assert!(store.pending_dispatches(&deployment).unwrap().is_empty());
}

#[test]
fn unknown_work_counts_against_per_deployment_and_host_limits() {
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let a = ready_deployment(&store, "a");
    let b = ready_deployment(&store, "b");
    let first = store.grant_dispatch(&session, wanted(&a)).unwrap();
    store.mark_dispatch_uncertain(&session, &first).unwrap();
    let second = store.grant_dispatch(&session, wanted(&a)).unwrap();
    assert!(matches!(
        store.grant_dispatch(&session, wanted(&a)),
        Err(DispatchError::Full)
    ));
    let mut bounded = wanted(&b);
    bounded.max_total = 2;
    assert!(matches!(
        store.grant_dispatch(&session, bounded),
        Err(DispatchError::Full)
    ));
    store.finish_dispatch(&session, &second).unwrap();
    let accepted = store.grant_dispatch(&session, bounded).unwrap();
    assert_eq!(accepted.deployment_id(), b);
}

#[test]
fn previous_generation_uncertain_work_counts_after_close_and_reopen() {
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let deployment = ready_deployment(&store, "a");
    let first = store.grant_dispatch(&session, wanted(&deployment)).unwrap();
    store.mark_dispatch_uncertain(&session, &first).unwrap();
    assert_eq!(store.bump_generation(&deployment).unwrap(), 2);
    assert_eq!(
        store.close_dispatch(&session, &deployment, 1, 2).unwrap(),
        1
    );
    store
        .conn
        .execute(
            "UPDATE deployments SET dispatch_enabled=1 WHERE id=?1",
            [&deployment],
        )
        .unwrap();
    let mut request = wanted(&deployment);
    request.generation = 2;
    request.max_per_deployment = 1;
    assert!(matches!(
        store.grant_dispatch(&session, request),
        Err(DispatchError::Full)
    ));
}

#[test]
fn reopen_retains_work_and_new_session_fences_old_callbacks() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("store.sqlite3");
    let store = Store::open(&path).unwrap();
    let old = store.begin_coordinator_session().unwrap();
    let deployment = ready_deployment(&store, "a");
    let ticket = store.grant_dispatch(&old, wanted(&deployment)).unwrap();
    drop(store);
    let reopened = Store::open(&path).unwrap();
    let current = reopened.begin_coordinator_session().unwrap();
    assert!(current.epoch() > old.epoch());
    assert_ne!(current.id(), old.id());
    assert!(matches!(
        reopened.finish_dispatch(&old, &ticket),
        Err(DispatchError::StaleSession)
    ));
    assert!(matches!(
        reopened.finish_dispatch(&current, &ticket),
        Err(DispatchError::StaleSession)
    ));
    assert!(matches!(
        reopened.grant_dispatch(&current, wanted(&deployment)),
        Err(DispatchError::Closed)
    ));
    let pending = reopened.pending_dispatches(&deployment).unwrap();
    assert_eq!(pending.len(), 1);
    assert!(pending[0].uncertain);
}

#[test]
fn independent_connections_serialize_close_against_dispatch() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("store.sqlite3");
    let left_store = Store::open(&path).unwrap();
    let right_store = Store::open(&path).unwrap();
    let session = left_store.begin_coordinator_session().unwrap();
    let deployment = ready_deployment(&left_store, "a");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let left_session = session.clone();
    let left_deployment = deployment.clone();
    let left_barrier = barrier.clone();
    let dispatch = std::thread::spawn(move || {
        left_barrier.wait();
        left_store.grant_dispatch(&left_session, wanted(&left_deployment))
    });
    let right_deployment = deployment.clone();
    let close = std::thread::spawn(move || {
        barrier.wait();
        right_store.close_dispatch(&session, &right_deployment, 1, 1)
    });
    let dispatched = dispatch.join().unwrap();
    let closed = close.join().unwrap();
    match (&dispatched, &closed) {
        (Ok(_), Ok(1)) | (Err(DispatchError::Closed), Ok(0)) => {}
        (Err(DispatchError::Sql(rusqlite::Error::SqliteFailure(e, _))), Ok(0))
            if e.code == rusqlite::ErrorCode::DatabaseBusy => {}
        (Ok(_), Err(DispatchError::Sql(rusqlite::Error::SqliteFailure(e, _))))
            if e.code == rusqlite::ErrorCode::DatabaseBusy => {}
        (
            Err(DispatchError::Sql(rusqlite::Error::SqliteFailure(left, _))),
            Err(DispatchError::Sql(rusqlite::Error::SqliteFailure(right, _))),
        ) if left.code == rusqlite::ErrorCode::DatabaseBusy
            && right.code == rusqlite::ErrorCode::DatabaseBusy => {}
        other => panic!("invalid race outcome: {other:?}"),
    }
    let pending = Store::open(&path)
        .unwrap()
        .pending_dispatches(&deployment)
        .unwrap();
    assert_eq!(pending.len(), usize::from(dispatched.is_ok()));
}

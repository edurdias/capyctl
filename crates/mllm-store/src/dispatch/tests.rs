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

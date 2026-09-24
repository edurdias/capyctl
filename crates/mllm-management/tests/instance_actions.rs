//! Owner decision Q7 (ADR 0013 as amended): per-instance `start` and `stop`
//! through the management API. Fake-engine tests only; not qualification.
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use mllm_controller::{
    coordinator::{
        CoordinatorError, CoordinatorOptions, ObservationFuture, OwnedCoordinator,
        ServiceObservation,
    },
    OwnedCoordinatorState,
};
use mllm_domain::resources::MemoryObservation;
use mllm_management::{
    actions::OwnedActionSource, configuration::SharedConfigurationSource, lifecycle_router,
    ManagementCredentials,
};
use mllm_testkit::fixture;
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
    time::Duration,
};
use tower::ServiceExt;

const MANAGEMENT: &str = "management-credential-012345678901234567890";
const INFERENCE: &str = "inference-credential-0123456789012345678901";

struct Observations(Vec<MemoryObservation>);
impl ServiceObservation for Observations {
    fn observe(&self, _: String) -> ObservationFuture {
        let values = self.0.clone();
        Box::pin(async move { Ok(values) })
    }
}

async fn setup() -> (
    tempfile::TempDir,
    Arc<Mutex<OwnedCoordinatorState>>,
    OwnedCoordinator,
    String,
    axum::Router,
) {
    let source = fixture::owned_source().await;
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().join("srv.sqlite3");
    std::fs::copy(source.dir.path().join("srv.sqlite3"), &path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let owner = Arc::new(Mutex::new(OwnedCoordinatorState::open(dir.path()).unwrap()));
    let worker = mllm_testkit::spawn_fake_coordinator(
        owner.clone(),
        Arc::new(Observations(source.observations.clone())),
        Arc::new(|| Ok::<_, CoordinatorError>(1900)),
        CoordinatorOptions::default(),
    )
    .unwrap();
    let host: Value = serde_json::from_str(include_str!(
        "../../mllm-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let configuration = Arc::new(
        SharedConfigurationSource::new(owner.clone(), host["input"]["host"].clone(), "owner")
            .unwrap(),
    );
    let app = lifecycle_router(
        ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap(),
        Arc::new(OwnedActionSource::new(configuration, worker.commands()).unwrap()),
    );
    (dir, owner, worker, source.fence.deployment_id.clone(), app)
}

fn request(path: &str, key: &str, action: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/management/v1/deployments/{path}/actions"))
        .header("authorization", format!("Bearer {MANAGEMENT}"))
        .header("content-type", "application/json")
        .header("idempotency-key", key)
        .body(Body::from(
            json!({"expected_revision":1,"action":action,"deadline_ms":10000}).to_string(),
        ))
        .unwrap()
}

async fn send(app: &axum::Router, path: &str, key: &str, action: &str) -> (u16, Value) {
    let response = app
        .clone()
        .oneshot(request(path, key, action))
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body =
        serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap();
    (status, body)
}

async fn operation(owner: &Arc<Mutex<OwnedCoordinatorState>>, id: &str) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let done = owner
                .lock()
                .unwrap()
                .store()
                .snapshot()
                .unwrap()
                .operations
                .iter()
                .any(|op| op.id == id && op.state == "succeeded");
            if done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

fn stopped(owner: &Arc<Mutex<OwnedCoordinatorState>>, id: &str) -> Vec<bool> {
    owner
        .lock()
        .unwrap()
        .store()
        .deployment_instances(id)
        .unwrap()
        .iter()
        .map(|row| row.operator_stopped)
        .collect()
}

/// `stop instance d/0` stops the realized instance with an ordinary stop: the
/// deployment is not administratively stopped, but on-demand activation leaves
/// that instance alone until `start instance d/0` lifts the mark.
// T10 T18
#[tokio::test]
async fn instance_zero_stops_and_starts_through_the_lifecycle() {
    let (_dir, owner, worker, id, app) = setup().await;
    let (status, start) = send(&app, &format!("{id}/instances/0"), "start-0", "start").await;
    assert_eq!(status, 202, "{start}");
    assert_eq!(start["instance"], 0);
    operation(&owner, start["operation_id"].as_str().unwrap()).await;
    let (status, stop) = send(&app, &format!("{id}/instances/0"), "stop-0", "stop").await;
    assert_eq!(status, 202, "{stop}");
    assert!(stop["operation_id"].is_string());
    {
        let state = owner.lock().unwrap();
        assert!(!state.store().is_admin_stopped(&id).unwrap());
        assert!(state.store().on_demand_instance_stopped(&id).unwrap());
    }
    operation(&owner, stop["operation_id"].as_str().unwrap()).await;
    assert_eq!(stopped(&owner, &id), vec![true]);
    // An exact retry replays the same receipt and keeps the mark.
    assert_eq!(
        send(&app, &format!("{id}/instances/0"), "stop-0", "stop")
            .await
            .1,
        stop
    );
    let (status, restart) = send(&app, &format!("{id}/instances/0"), "start-1", "start").await;
    assert_eq!(status, 202, "{restart}");
    assert_eq!(stopped(&owner, &id), vec![false]);
    operation(&owner, restart["operation_id"].as_str().unwrap()).await;
    worker.shutdown().await.unwrap();
}

/// I2 (ADR 0013 §4): another instance is placed and started through the same
/// lifecycle as instance 0. Stopping one that holds nothing records intent
/// only; `start instance` lifts its mark and starts it; `start deployment`
/// lifts every per-instance stop (Q5). Unknown or malformed instances are
/// refused.
// T03 T05 T10
#[tokio::test]
async fn other_instances_are_placed_started_and_stopped_through_the_lifecycle() {
    let (dir, owner, worker, id, app) = setup().await;
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    // Declare two instances, so the reconciler does not compact instance 1
    // away as surplus while it holds nothing (ADR 0013 §5).
    assert_eq!(
        sql.execute(
            "UPDATE deployment_revision_instances SET instances=2 WHERE deployment_id=?1",
            [&id],
        )
        .unwrap(),
        1
    );
    sql.execute(
        "INSERT INTO deployment_instances(deployment_id,instance_index) VALUES(?1,1)",
        [&id],
    )
    .unwrap();
    let (status, body) = send(&app, &format!("{id}/instances/1"), "stop-1", "stop").await;
    assert_eq!(status, 202, "{body}");
    assert!(body["operation_id"].is_null());
    assert_eq!(stopped(&owner, &id), vec![false, true]);
    let (status, body) = send(&app, &format!("{id}/instances/1"), "start-1", "start").await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["instance"], 1);
    assert_eq!(stopped(&owner, &id), vec![false, false]);
    operation(&owner, body["operation_id"].as_str().unwrap()).await;
    {
        let state = owner.lock().unwrap();
        let rows = state.store().deployment_instances(&id).unwrap();
        // Instance 1 drew its own generation from the deployment's counter
        // and was placed on the one allowed host; instance 0 never started.
        assert!(rows[1].host_id.is_some());
        assert!(rows[1].generation > rows[0].generation);
        let owners = state.store().resource_snapshot().unwrap().owners;
        assert!(owners.contains_key(&format!("deployment:{id}/instance:1")));
        assert!(!owners.contains_key(&id));
    }
    for (path, code) in [
        (format!("{id}/instances/2"), 404),
        (format!("{id}/instances/01"), 400),
        (format!("{id}/instances/x"), 400),
        (format!("{id}/instances/64"), 400),
    ] {
        assert_eq!(send(&app, &path, "bad", "stop").await.0, code, "{path}");
    }
    let (status, body) = send(&app, &format!("{id}/instances/1"), "stop-1b", "stop").await;
    assert_eq!(status, 202, "{body}");
    operation(&owner, body["operation_id"].as_str().unwrap()).await;
    let (status, body) = send(&app, &id, "start-all", "start").await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(stopped(&owner, &id), vec![false, false]);
    operation(&owner, body["operation_id"].as_str().unwrap()).await;
    worker.shutdown().await.unwrap();
}

/// ADR 0013 §7 (I3 hand-off): an instance retired or compacted away is a 404,
/// decided in the same transaction that would set its mark. The lookup used to
/// run first and separately, so a compaction in between turned the mark write
/// into a 500.
// T09 T10
#[tokio::test]
async fn a_retired_or_compacted_instance_is_not_found_never_an_internal_error() {
    let (dir, owner, worker, id, app) = setup().await;
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    sql.execute(
        "UPDATE deployment_revision_instances SET instances=2 WHERE deployment_id=?1",
        [&id],
    )
    .unwrap();
    sql.execute(
        "INSERT INTO deployment_instances(deployment_id,instance_index,state) VALUES(?1,1,'retiring')",
        [&id],
    )
    .unwrap();
    for action in ["stop", "start"] {
        let (status, body) =
            send(&app, &format!("{id}/instances/1"), &format!("retiring-{action}"), action).await;
        assert_eq!(status, 404, "{action}: {body}");
    }
    assert_eq!(stopped(&owner, &id), vec![false, false], "no mark was set");
    sql.execute(
        "DELETE FROM deployment_instances WHERE deployment_id=?1 AND instance_index=1",
        [&id],
    )
    .unwrap();
    for action in ["stop", "start"] {
        let (status, body) =
            send(&app, &format!("{id}/instances/1"), &format!("gone-{action}"), action).await;
        assert_eq!(status, 404, "{action}: {body}");
    }
    worker.shutdown().await.unwrap();
}

fn request_at(path: &str, key: &str, action: &str, revision: i64) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/management/v1/deployments/{path}/actions"))
        .header("authorization", format!("Bearer {MANAGEMENT}"))
        .header("content-type", "application/json")
        .header("idempotency-key", key)
        .body(Body::from(
            json!({"expected_revision":revision,"action":action,"deadline_ms":10000}).to_string(),
        ))
        .unwrap()
}

/// SPEC §6.4: "Use idempotency keys for retries." An exact retry of an earlier
/// command is answered from its receipt and changes nothing: a replayed
/// `stop instance` must not re-set a mark a later `start instance` lifted, and
/// a replayed `start deployment` must not lift a later `stop instance`.
// T09 T10
#[tokio::test]
async fn replayed_instance_and_deployment_commands_leave_operator_marks_alone() {
    let (_dir, owner, worker, id, app) = setup().await;
    let (status, start) = send(&app, &id, "start-all", "start").await;
    assert_eq!(status, 202, "{start}");
    operation(&owner, start["operation_id"].as_str().unwrap()).await;
    let (status, stop) = send(&app, &format!("{id}/instances/0"), "stop-0", "stop").await;
    assert_eq!(status, 202, "{stop}");
    operation(&owner, stop["operation_id"].as_str().unwrap()).await;
    assert_eq!(stopped(&owner, &id), vec![true]);
    // Replaying the earlier deployment start keeps the later instance stop.
    let (status, replay) = send(&app, &id, "start-all", "start").await;
    assert_eq!(status, 202, "{replay}");
    assert_eq!(replay, start);
    assert_eq!(stopped(&owner, &id), vec![true], "a replayed start lifted a later stop");
    // Lift the stop with a new start, then replay the old stop.
    let (status, restart) = send(&app, &format!("{id}/instances/0"), "start-0", "start").await;
    assert_eq!(status, 202, "{restart}");
    operation(&owner, restart["operation_id"].as_str().unwrap()).await;
    assert_eq!(stopped(&owner, &id), vec![false]);
    let (status, replay) = send(&app, &format!("{id}/instances/0"), "stop-0", "stop").await;
    assert_eq!(status, 202, "{replay}");
    assert_eq!(replay, stop);
    assert_eq!(stopped(&owner, &id), vec![false], "a replayed stop re-set a lifted mark");
    worker.shutdown().await.unwrap();
}

/// SPEC §6.4: "A response lost after persistence must not produce another
/// [effect] on retry." The receipt is looked up before today's revision is
/// compared, so a retry after an unrelated revision bump still gets its
/// original answer instead of a revision conflict.
// T09
#[tokio::test]
async fn an_instance_action_retry_is_answered_before_the_revision_check() {
    let (dir, owner, worker, id, app) = setup().await;
    let (status, start) = send(&app, &format!("{id}/instances/0"), "start-0", "start").await;
    assert_eq!(status, 202, "{start}");
    operation(&owner, start["operation_id"].as_str().unwrap()).await;
    let (status, stop) = send(&app, &format!("{id}/instances/0"), "stop-0", "stop").await;
    assert_eq!(status, 202, "{stop}");
    operation(&owner, stop["operation_id"].as_str().unwrap()).await;
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    sql.execute("UPDATE deployments SET revision=2 WHERE id=?1", [&id])
        .unwrap();
    for (key, action, first) in [("stop-0", "stop", &stop), ("start-0", "start", &start)] {
        let response = app
            .clone()
            .oneshot(request_at(&format!("{id}/instances/0"), key, action, 1))
            .await
            .unwrap();
        assert_eq!(response.status(), 202, "{action}");
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap())
                .unwrap();
        assert_eq!(&body, first, "{action}");
    }
    // A new command at the stale revision is still a conflict.
    let response = app
        .clone()
        .oneshot(request_at(&format!("{id}/instances/0"), "stop-new", "stop", 1))
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    worker.shutdown().await.unwrap();
}

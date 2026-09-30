//! SPEC §6.3 (owner decisions D11 and 2026-09-23, plan unit W6): "delete
//! deployment: Remove route
//! and deployment after authorized cleanup. Do not delete user-owned
//! checkpoints or cache files implicitly." The management action is `delete`.
//!
//! Fake-engine tests. They are not qualification of any native engine recipe.
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use capyctl_controller::{
    coordinator::{
        CoordinatorError, CoordinatorOptions, ObservationFuture, OwnedCoordinator,
        ServiceObservation,
    },
    OwnedCoordinatorState,
};
use capyctl_domain::resources::MemoryObservation;
use capyctl_management::{
    actions::OwnedActionSource, configuration::SharedConfigurationSource, lifecycle_router,
    ManagementCredentials,
};
use capyctl_testkit::fixture;
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

struct Setup {
    dir: tempfile::TempDir,
    owner: Arc<Mutex<OwnedCoordinatorState>>,
    worker: OwnedCoordinator,
    /// The stopped managed deployment named and routed `ordinary`.
    id: String,
    /// Another stopped managed deployment, `second`, which must be untouched.
    other: String,
    app: axum::Router,
}

async fn setup() -> Setup {
    let source = fixture::owned_source().await;
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().join("srv.sqlite3");
    std::fs::copy(source.dir.path().join("srv.sqlite3"), &path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let owner = Arc::new(Mutex::new(OwnedCoordinatorState::open(dir.path()).unwrap()));
    let worker = capyctl_testkit::spawn_fake_coordinator(
        owner.clone(),
        Arc::new(Observations(source.observations.clone())),
        Arc::new(|| Ok::<_, CoordinatorError>(1900)),
        CoordinatorOptions::default(),
    )
    .unwrap();
    let host: Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
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
    Setup {
        dir,
        owner,
        worker,
        id: source.fence.deployment_id.clone(),
        other: source.other.deployment_id.clone(),
        app,
    }
}

fn request(id: &str, key: &str, action: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/management/v1/deployments/{id}/actions"))
        .header("authorization", format!("Bearer {MANAGEMENT}"))
        .header("content-type", "application/json")
        .header("idempotency-key", key)
        .body(Body::from(
            json!({"expected_revision":1,"action":action,"deadline_ms":10000}).to_string(),
        ))
        .unwrap()
}

fn create(key: &str, name: &str) -> Request<Body> {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let mut config = fixture["input"]["deployment"].clone();
    config["name"] = json!(name);
    config["routes"] = json!([name]);
    Request::builder()
        .method("POST")
        .uri("/management/v1/deployments")
        .header("authorization", format!("Bearer {MANAGEMENT}"))
        .header("content-type", "application/json")
        .header("idempotency-key", key)
        .body(Body::from(
            json!({"config":config,"activate":false}).to_string(),
        ))
        .unwrap()
}

async fn send(app: &axum::Router, request: Request<Body>) -> (u16, Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let body =
        serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap();
    (status, body)
}

async fn succeeded(owner: &Arc<Mutex<OwnedCoordinatorState>>, operation: &str) {
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
                .any(|op| op.id == operation && op.state == "succeeded");
            if done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

fn listed(owner: &Arc<Mutex<OwnedCoordinatorState>>, id: &str) -> bool {
    owner
        .lock()
        .unwrap()
        .store()
        .snapshot()
        .unwrap()
        .deployments
        .iter()
        .any(|d| d.id == id)
}

fn routed(owner: &Arc<Mutex<OwnedCoordinatorState>>, route: &str) -> Option<String> {
    owner
        .lock()
        .unwrap()
        .store()
        .find_deployment_by_route(route)
        .unwrap()
        .map(|row| row.id)
}

// T10 T09: a stopped deployment is deleted. Its route, instances and
// checkpoint digest records go; its history stays behind a tombstone; an exact
// retry after the deployment is gone returns the original receipt.
#[tokio::test]
async fn delete_of_a_stopped_deployment_removes_its_route_and_keeps_its_history() {
    let s = setup().await;
    let sql = rusqlite::Connection::open(s.dir.path().join("srv.sqlite3")).unwrap();
    let count = |query: &str| {
        sql.query_row(query, [&s.id], |r| r.get::<_, i64>(0))
            .unwrap()
    };
    // A digest record of the checkpoint; the checkpoint itself is on the host.
    sql.execute(
        "INSERT OR IGNORE INTO checkpoint_digests(deployment_id,revision,state,host_id,provisional,updated_at_ms) VALUES(?1,1,'pending','local',0,0)",
        [&s.id],
    )
    .unwrap();
    assert_eq!(routed(&s.owner, "ordinary").as_deref(), Some(s.id.as_str()));
    let history = count("SELECT COUNT(*) FROM operations WHERE deployment_id=?1");
    assert!(history >= 1);
    // Owner decision 2026-09-23: the action is `delete`; `undeploy` is gone.
    let (status, body) = send(&s.app, request(&s.id, "undeploy-1", "undeploy")).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (400, Some("invalid_request"))
    );

    let (status, receipt) = send(&s.app, request(&s.id, "delete-1", "delete")).await;
    assert_eq!(status, 202, "{receipt}");
    assert_eq!(receipt["deployment_id"], s.id);
    assert_eq!(receipt["revision"], "1");
    assert_eq!(receipt["joined"], false);
    let operation = receipt["operation_id"].as_str().unwrap().to_owned();
    succeeded(&s.owner, &operation).await;

    // SPEC §6.3: the route is gone at once; the router answers 404 for it.
    assert_eq!(routed(&s.owner, "ordinary"), None);
    assert!(!s
        .owner
        .lock()
        .unwrap()
        .store()
        .list_enabled_route_ids()
        .unwrap()
        .contains(&"ordinary".to_string()));
    assert!(!listed(&s.owner, &s.id));
    assert!(
        listed(&s.owner, &s.other),
        "another deployment is untouched"
    );
    assert_eq!(
        routed(&s.owner, "second").as_deref(),
        Some(s.other.as_str())
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM deployment_routes WHERE deployment_id=?1"),
        0
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM deployment_instances WHERE deployment_id=?1"),
        0
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM checkpoint_digests WHERE deployment_id=?1"),
        0
    );
    // The tombstone: id and history kept, name released.
    let (kind, name): (String, String) = sql
        .query_row(
            "SELECT kind,name FROM deployments WHERE id=?1",
            [&s.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (kind.as_str(), name),
        ("deleted", format!("deleted/{}", s.id))
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM operations WHERE deployment_id=?1"),
        history + 1
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM effective_revisions WHERE deployment_id=?1"),
        1
    );
    let evidence: String = sql
        .query_row(
            "SELECT evidence FROM journal_entries WHERE operation_id=?1 AND state='deleted'",
            [&operation],
            |r| r.get(0),
        )
        .unwrap();
    let evidence: Value = serde_json::from_str(&evidence).unwrap();
    assert_eq!(evidence["name"], "ordinary");
    assert_eq!(evidence["routes"], json!(["ordinary"]));
    assert_eq!(
        count("SELECT COUNT(*) FROM management_events WHERE deployment_id=?1 AND kind='deployment_deleted'"),
        1
    );

    // T09: an exact retry answers from the receipt, the deployment being gone.
    assert_eq!(
        send(&s.app, request(&s.id, "delete-1", "delete")).await,
        (202, receipt)
    );
    // The same key naming another action is a different command.
    let (status, body) = send(&s.app, request(&s.id, "delete-1", "stop")).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (409, Some("idempotency_conflict"))
    );
    // Anything new against the deleted deployment finds nothing.
    for (key, action) in [("again", "delete"), ("start", "start"), ("stop", "stop")] {
        let (status, body) = send(&s.app, request(&s.id, key, action)).await;
        assert_eq!(
            (status, body["error"]["code"].as_str()),
            (404, Some("not_found")),
            "{action}"
        );
    }
    assert_eq!(
        count("SELECT COUNT(*) FROM operations WHERE deployment_id=?1"),
        history + 1
    );
    s.worker.shutdown().await.unwrap();
}

// T32: a delete never releases accounting. While any instance holds a runtime
// the command is refused and changes nothing; after an ordinary stop completes
// with verified cleanup it is accepted.
#[tokio::test]
async fn delete_is_refused_while_an_instance_holds_a_runtime() {
    let s = setup().await;
    let sql = rusqlite::Connection::open(s.dir.path().join("srv.sqlite3")).unwrap();
    let receipts = || {
        sql.query_row("SELECT COUNT(*) FROM command_receipts", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap()
    };
    let (status, start) = send(&s.app, request(&s.id, "start", "start")).await;
    assert_eq!(status, 202, "{start}");
    succeeded(&s.owner, start["operation_id"].as_str().unwrap()).await;
    let before = receipts();
    let (status, body) = send(&s.app, request(&s.id, "delete-early", "delete")).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["code"], "delete_requires_cleanup");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("stop the deployment"));
    assert_eq!(receipts(), before, "a refusal records nothing");
    assert_eq!(routed(&s.owner, "ordinary").as_deref(), Some(s.id.as_str()));
    assert!(listed(&s.owner, &s.id));
    assert!(!s
        .owner
        .lock()
        .unwrap()
        .store()
        .resource_snapshot()
        .unwrap()
        .owners
        .is_empty());

    let (status, stop) = send(&s.app, request(&s.id, "stop", "stop")).await;
    assert_eq!(status, 202, "{stop}");
    succeeded(&s.owner, stop["operation_id"].as_str().unwrap()).await;
    let (status, body) = send(&s.app, request(&s.id, "delete", "delete")).await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(routed(&s.owner, "ordinary"), None);
    assert!(s
        .owner
        .lock()
        .unwrap()
        .store()
        .resource_snapshot()
        .unwrap()
        .owners
        .is_empty());
    s.worker.shutdown().await.unwrap();
}

// T09 (live M16): a name that was deleted can be deployed again, and the new
// deployment is a new deployment with a new id; the old one stays a tombstone.
#[tokio::test]
async fn a_redeploy_of_a_deleted_name_gets_a_new_deployment_id() {
    let s = setup().await;
    let (status, body) = send(&s.app, create("before", "ordinary")).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (409, Some("route_conflict"))
    );

    let (status, body) = send(&s.app, request(&s.id, "delete", "delete")).await;
    assert_eq!(status, 202, "{body}");
    let (status, created) = send(&s.app, create("after", "ordinary")).await;
    assert_eq!(status, 202, "{created}");
    let fresh = created["deployment_id"].as_str().unwrap().to_owned();
    assert_ne!(fresh, s.id);
    assert_eq!(
        routed(&s.owner, "ordinary").as_deref(),
        Some(fresh.as_str())
    );
    assert!(listed(&s.owner, &fresh));
    assert!(!listed(&s.owner, &s.id));
    let snapshot = s.owner.lock().unwrap().store().snapshot().unwrap();
    assert_eq!(
        snapshot
            .deployments
            .iter()
            .filter(|d| d.name == "ordinary")
            .count(),
        1
    );
    s.worker.shutdown().await.unwrap();
}

// T10 T09 (live M16, M53): `delete --stop` of a deployment holding nothing. Its
// Stop is accepted, not refused as a lifecycle conflict: the operator's intent
// is recorded at once (automatic activation suspended), nothing is released, an
// exact retry answers the same receipt, and the delete follows.
#[tokio::test]
async fn a_stop_of_a_deployment_holding_nothing_is_recorded_and_the_delete_follows() {
    let s = setup().await;
    let (status, stop) = send(&s.app, request(&s.id, "stop-first", "stop")).await;
    assert_eq!(status, 202, "{stop}");
    let operation = stop["operation_id"].as_str().unwrap().to_owned();
    succeeded(&s.owner, &operation).await;
    {
        let o = s.owner.lock().unwrap();
        assert!(o.store().is_admin_stopped(&s.id).unwrap());
        assert!(o.store().resource_snapshot().unwrap().owners.is_empty());
        let snapshot = o.store().snapshot().unwrap();
        let d = snapshot.deployments.iter().find(|d| d.id == s.id).unwrap();
        assert_eq!(
            (d.desired_state.as_str(), d.observed_state.as_str()),
            ("stopped", "stopped")
        );
    }
    assert_eq!(
        send(&s.app, request(&s.id, "stop-first", "stop")).await,
        (202, stop),
        "an exact retry answers the original receipt"
    );
    let (status, body) = send(&s.app, request(&s.id, "stop-first", "start")).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (409, Some("idempotency_conflict"))
    );
    let (status, body) = send(&s.app, request(&s.id, "delete-after", "delete")).await;
    assert_eq!(status, 202, "{body}");
    assert!(!listed(&s.owner, &s.id));
    s.worker.shutdown().await.unwrap();
}

// T09 (SPEC §6.3, §14): a revision of a deleted deployment finds nothing: 404
// `not_found`, never a conflict or an internal error, and nothing is recorded.
#[tokio::test]
async fn a_revision_of_a_deleted_deployment_is_not_found() {
    let s = setup().await;
    let (status, receipt) = send(&s.app, request(&s.id, "delete-1", "delete")).await;
    assert_eq!(status, 202, "{receipt}");
    succeeded(&s.owner, receipt["operation_id"].as_str().unwrap()).await;
    let fixture: Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let put = Request::builder()
        .method("PUT")
        .uri(format!("/management/v1/deployments/{}", s.id))
        .header("authorization", format!("Bearer {MANAGEMENT}"))
        .header("content-type", "application/json")
        .header("idempotency-key", "revise-deleted")
        .body(Body::from(
            json!({"expected_revision":1,"config":fixture["input"]["deployment"]}).to_string(),
        ))
        .unwrap();
    let (status, body) = send(&s.app, put).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (404, Some("not_found")),
        "{body}"
    );
    s.worker.shutdown().await.unwrap();
}

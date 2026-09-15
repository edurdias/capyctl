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
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
    time::Duration,
};
use tower::ServiceExt;

#[path = "../../mllm-controller/tests/qualification_support/fixture.rs"]
mod fixture;
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
    let worker = OwnedCoordinator::spawn_fake(
        owner.clone(),
        Arc::new(Observations(source.observations.clone())),
        Arc::new(|| Ok::<_, CoordinatorError>(1900)),
        CoordinatorOptions::default(),
    )
    .unwrap();
    let host: Value = serde_json::from_str(include_str!(
        "../../mllm-config/tests/fixtures/effective-fake-golden.json"
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
async fn value(response: axum::response::Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap()
}
async fn operation(owner: &Arc<Mutex<OwnedCoordinatorState>>, id: &str) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let done = {
                let owner = owner.lock().unwrap();
                owner
                    .store()
                    .snapshot()
                    .unwrap()
                    .operations
                    .iter()
                    .any(|op| op.id == id && op.state == "succeeded")
            };
            if done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn authenticated_actions_start_and_stop_the_owned_qualified_fake() {
    let (dir, owner, worker, id, app) = setup().await;
    let response = app
        .clone()
        .oneshot(request(&id, "start", "start"))
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    let start = value(response).await;
    assert_eq!(start.as_object().unwrap().len(), 5);
    assert_eq!(start["revision"], "1");
    assert_eq!(start["joined"], false);
    operation(&owner, start["operation_id"].as_str().unwrap()).await;
    assert_eq!(
        owner
            .lock()
            .unwrap()
            .store()
            .resource_snapshot()
            .unwrap()
            .owners[&id]
            .phase,
        mllm_domain::resources::ResourcePhase::Ready
    );
    assert_eq!(
        value(
            app.clone()
                .oneshot(request(&id, "start", "start"))
                .await
                .unwrap()
        )
        .await,
        start
    );
    let response = app
        .clone()
        .oneshot(request(&id, "stop", "stop"))
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    let stop = value(response).await;
    operation(&owner, stop["operation_id"].as_str().unwrap()).await;
    assert!(owner
        .lock()
        .unwrap()
        .store()
        .resource_snapshot()
        .unwrap()
        .owners
        .is_empty());
    assert_eq!(
        value(
            app.clone()
                .oneshot(request(&id, "stop", "stop"))
                .await
                .unwrap()
        )
        .await,
        stop
    );
    for (key, action, code) in [
        ("start", "stop", "idempotency_conflict"),
        ("stop", "start", "idempotency_conflict"),
    ] {
        assert_eq!(
            value(
                app.clone()
                    .oneshot(request(&id, key, action))
                    .await
                    .unwrap()
            )
            .await["error"]["code"],
            code
        );
    }
    worker.shutdown().await.unwrap();
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    assert_eq!(sql.query_row("SELECT COUNT(*) FROM management_events WHERE kind IN ('ordinary_cleanup_accepted','ordinary_cleanup_armed','ordinary_cleanup_completed')",[],|r|r.get::<_,i64>(0)).unwrap(),3);
    assert_eq!(
        sql.query_row("SELECT COUNT(*) FROM endpoint_leases", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        value(
            app.clone()
                .oneshot(request(&id, "stop", "stop"))
                .await
                .unwrap()
        )
        .await,
        stop
    );
    assert_eq!(
        value(
            app.clone()
                .oneshot(request(&id, "new-stop", "stop"))
                .await
                .unwrap()
        )
        .await["error"]["code"],
        "reconciliation_required"
    );
    assert_eq!(
        value(
            app.clone()
                .oneshot(request(&id, "start", "start"))
                .await
                .unwrap()
        )
        .await,
        start
    );
}

#[tokio::test]
async fn action_rejections_are_typed_before_any_new_receipt() {
    let (dir, owner, worker, id, app) = setup().await;
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    let count = || {
        sql.query_row("SELECT COUNT(*) FROM command_receipts", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap()
    };
    let before = count();
    for (target, body, code) in [
        (
            ulid::Ulid::new().to_string(),
            json!({"expected_revision":1,"action":"start","deadline_ms":10000}),
            "not_found",
        ),
        (
            id.clone(),
            json!({"expected_revision":2,"action":"start","deadline_ms":10000}),
            "revision_conflict",
        ),
        (
            id.clone(),
            json!({"expected_revision":1,"action":"park","deadline_ms":10000}),
            "unsupported_capability",
        ),
        (
            id.clone(),
            json!({"expected_revision":1,"action":"unknown","deadline_ms":10000}),
            "invalid_request",
        ),
    ] {
        let mut req = request(&target, "denied", "start");
        *req.body_mut() = Body::from(body.to_string());
        assert_eq!(
            value(app.clone().oneshot(req).await.unwrap()).await["error"]["code"],
            code
        );
    }
    assert_eq!(count(), before);
    let attached = mllm_domain::DeploymentId::new();
    owner
        .lock()
        .unwrap()
        .store()
        .accept_deployment(mllm_store::AcceptDeployment {
            id: attached,
            name: "attached".into(),
            kind: "attachment".into(),
            route_model_id: None,
            desired_state: mllm_domain::LifecycleState::Stopped,
            schema_version: 1,
            idempotency_key: "attached".into(),
            initial_operation_id: mllm_domain::OperationId(ulid::Ulid::new().to_string()),
        })
        .unwrap();
    for action in ["start", "stop"] {
        assert_eq!(
            value(
                app.clone()
                    .oneshot(request(&attached.to_string(), "attached-action", action))
                    .await
                    .unwrap()
            )
            .await["error"]["code"],
            "unsupported_capability"
        );
    }
    let start = value(
        app.clone()
            .oneshot(request(&id, "start", "start"))
            .await
            .unwrap(),
    )
    .await;
    operation(&owner, start["operation_id"].as_str().unwrap()).await;
    assert_eq!(
        value(
            app.clone()
                .oneshot(request(&id, "another", "start"))
                .await
                .unwrap()
        )
        .await["error"]["code"],
        "runtime_retained"
    );
    let body: Value = serde_json::from_str(include_str!(
        "../../mllm-config/tests/fixtures/effective-fake-golden.json"
    ))
    .unwrap();
    let mut config = body["input"]["deployment"].clone();
    config["name"] = json!("unqualified");
    config["routes"] = json!(["unqualified"]);
    let req = Request::builder()
        .method("POST")
        .uri("/management/v1/deployments")
        .header("authorization", format!("Bearer {MANAGEMENT}"))
        .header("content-type", "application/json")
        .header("idempotency-key", "unqualified")
        .body(Body::from(
            json!({"config":config,"activate":false}).to_string(),
        ))
        .unwrap();
    let created = value(app.clone().oneshot(req).await.unwrap()).await;
    assert_eq!(
        value(
            app.clone()
                .oneshot(request(
                    created["deployment_id"].as_str().unwrap(),
                    "start",
                    "start"
                ))
                .await
                .unwrap()
        )
        .await["error"]["code"],
        "unsupported_capability"
    );
    let stop = value(
        app.clone()
            .oneshot(request(&id, "stop", "stop"))
            .await
            .unwrap(),
    )
    .await;
    operation(&owner, stop["operation_id"].as_str().unwrap()).await;
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_action_wires_and_credentials_do_not_mutate_owned_state() {
    let (dir, _owner, worker, id, app) = setup().await;
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    let count = || {
        sql.query_row("SELECT (SELECT COUNT(*) FROM command_receipts)+(SELECT COUNT(*) FROM operations)+(SELECT COUNT(*) FROM management_events)",[],|r| r.get::<_,i64>(0)).unwrap()
    };
    let before = count();
    for body in [
        r#"{"expected_revision":true,"action":"start","deadline_ms":10000}"#,
        r#"{"expected_revision":1.0,"action":"start","deadline_ms":10000}"#,
        r#"{"expected_revision":"1","action":"start","deadline_ms":10000}"#,
        r#"{"expected_revision":0,"action":"start","deadline_ms":10000}"#,
        r#"{"expected_revision":1,"action":"start","deadline_ms":9223372036854775808}"#,
        r#"{"expected_revision":1,"action":"start","deadline_ms":false}"#,
        r#"{"expected_revision":1,"action":"start","deadline_ms":1.0}"#,
        r#"{"expected_revision":1,"action":"start","deadline_ms":"10000"}"#,
        r#"{"expected_revision":1,"action":"start","deadline_ms":0}"#,
        r#"{"expected_revision":1,"action":"start","deadline_ms":10000,"generation":1}"#,
        r#"{"expected_revision":1,"action":"start","deadline_ms":10000,"principal":"owner"}"#,
        r#"{"expected_revision":1,"action":"start","deadline_ms":10000,"evidence":{}}"#,
        r#"{"expected_revision":1,"expected_revision":1,"action":"start","deadline_ms":10000}"#,
    ] {
        let mut req = request(&id, "bad", "start");
        *req.body_mut() = Body::from(body);
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), 400, "{body}");
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    }
    for token in [None, Some(INFERENCE)] {
        let mut req = request(&id, "bad", "start");
        req.headers_mut().remove("authorization");
        if let Some(token) = token {
            req.headers_mut()
                .insert("authorization", format!("Bearer {token}").parse().unwrap());
        }
        assert_eq!(app.clone().oneshot(req).await.unwrap().status(), 401);
    }
    for path in [
        format!("/management/v1/deployments/{id}/actions?x=1"),
        format!("/management/v1/deployments/{}/actions", id.to_lowercase()),
        format!("/management/v1/deployments/%30{}/actions", &id[1..]),
    ] {
        let mut req = request(&id, "bad", "start");
        *req.uri_mut() = path.parse().unwrap();
        assert_eq!(app.clone().oneshot(req).await.unwrap().status(), 400);
    }
    for header in ["idempotency-key", "content-type"] {
        let mut req = request(&id, "bad", "start");
        req.headers_mut()
            .append(header, "duplicate".parse().unwrap());
        assert_eq!(app.clone().oneshot(req).await.unwrap().status(), 400);
        let mut req = request(&id, "bad", "start");
        req.headers_mut().remove(header);
        assert_eq!(app.clone().oneshot(req).await.unwrap().status(), 400);
    }
    assert_eq!(count(), before);
    worker.shutdown().await.unwrap();
}

#[tokio::test]
// Deliberately hold the service mutex to test requests waiting in spawn_blocking.
#[allow(clippy::await_holding_lock)]
async fn actions_share_capacity_with_configuration_and_candidates_and_retain_cancelled_work() {
    let (_dir, owner, worker, id, app) = setup().await;
    let guard = owner.lock().unwrap();
    let first = tokio::spawn(app.clone().oneshot(request(&id, "cancelled-1", "start")));
    let second = tokio::spawn(app.clone().oneshot(request(&id, "cancelled-2", "start")));
    tokio::time::sleep(Duration::from_millis(100)).await;
    first.abort();
    second.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    assert!(second.await.unwrap_err().is_cancelled());
    for (path, body) in [
        (
            "/management/v1/deployments",
            json!({"config":{},"activate":false}),
        ),
        ("/management/v1/qualification-runs", json!({})),
    ] {
        let mut req = request(&id, "capacity", "start");
        *req.uri_mut() = path.parse().unwrap();
        *req.body_mut() = Body::from(body.to_string());
        assert_eq!(
            value(app.clone().oneshot(req).await.unwrap()).await["error"]["code"],
            "queue_full"
        );
    }
    drop(guard);
    let start = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let response = app
                .clone()
                .oneshot(request(&id, "cancelled-1", "start"))
                .await
                .unwrap();
            if response.status() == 202 {
                break value(response).await;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    operation(&owner, start["operation_id"].as_str().unwrap()).await;
    let stop = value(
        app.clone()
            .oneshot(request(&id, "stop", "stop"))
            .await
            .unwrap(),
    )
    .await;
    operation(&owner, stop["operation_id"].as_str().unwrap()).await;
    worker.shutdown().await.unwrap();
}

#[tokio::test]
// Deliberately hold the service mutex past the HTTP observation deadline.
#[allow(clippy::await_holding_lock)]
async fn action_response_and_body_deadlines_retain_or_release_the_correct_permits() {
    let (_dir, owner, worker, id, app) = setup().await;
    let guard = owner.lock().unwrap();
    let blocked = tokio::spawn(app.clone().oneshot(request(&id, "timed-out", "start")));
    let blocked2 = tokio::spawn(app.clone().oneshot(request(&id, "timed-out-2", "start")));
    assert_eq!(
        value(blocked.await.unwrap().unwrap()).await["error"]["code"],
        "deadline_exceeded"
    );
    assert_eq!(
        value(blocked2.await.unwrap().unwrap()).await["error"]["code"],
        "deadline_exceeded"
    );
    assert_eq!(
        value(
            app.clone()
                .oneshot(request(&id, "full", "start"))
                .await
                .unwrap()
        )
        .await["error"]["code"],
        "queue_full"
    );
    drop(guard);
    let start = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let response = app
                .clone()
                .oneshot(request(&id, "timed-out", "start"))
                .await
                .unwrap();
            if response.status() == 202 {
                break value(response).await;
            }
            let body = value(response).await;
            assert_eq!(body["error"]["code"], "queue_full", "{body}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    operation(&owner, start["operation_id"].as_str().unwrap()).await;
    let mut slow = request(&id, "slow", "start");
    *slow.body_mut() = Body::from_stream(futures::stream::pending::<
        Result<axum::body::Bytes, std::io::Error>,
    >());
    assert_eq!(
        value(app.clone().oneshot(slow).await.unwrap()).await["error"]["code"],
        "deadline_exceeded"
    );
    let mut large = request(&id, "large", "start");
    *large.body_mut() = Body::from(vec![b' '; (1 << 20) + 1]);
    assert_eq!(app.clone().oneshot(large).await.unwrap().status(), 413);
    let stop = value(
        app.clone()
            .oneshot(request(&id, "stop", "stop"))
            .await
            .unwrap(),
    )
    .await;
    operation(&owner, stop["operation_id"].as_str().unwrap()).await;
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn composed_sources_reject_mismatched_owned_state_and_stale_sessions() {
    let (_dir, owner, worker, id, app) = setup().await;
    let (_other_dir, other, other_worker, _, _) = setup().await;
    let host: Value = serde_json::from_str(include_str!(
        "../../mllm-config/tests/fixtures/effective-fake-golden.json"
    ))
    .unwrap();
    let source = Arc::new(
        SharedConfigurationSource::new(other, host["input"]["host"].clone(), "owner").unwrap(),
    );
    assert!(OwnedActionSource::new(source, worker.commands()).is_err());
    owner
        .lock()
        .unwrap()
        .store()
        .begin_coordinator_session()
        .unwrap();
    assert_eq!(
        value(app.oneshot(request(&id, "stale", "stop")).await.unwrap()).await["error"]["code"],
        "reconciliation_required"
    );
    worker.shutdown().await.unwrap();
    other_worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn store_failure_is_redacted_and_closes_new_command_admission() {
    let (dir, _owner, worker, id, app) = setup().await;
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    sql.execute_batch("CREATE TRIGGER fail_action BEFORE INSERT ON command_receipts WHEN NEW.idempotency_key='fail' BEGIN SELECT RAISE(ABORT,'/private/credential-secret'); END;").unwrap();
    let result = value(
        app.clone()
            .oneshot(request(&id, "fail", "start"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(result["error"]["code"], "internal");
    assert!(!result.to_string().contains("credential-secret"));
    assert_eq!(
        value(
            app.clone()
                .oneshot(request(&id, "later", "stop"))
                .await
                .unwrap()
        )
        .await["error"]["code"],
        "reconciliation_required"
    );
    assert_eq!(
        sql.query_row(
            "SELECT COUNT(*) FROM operations WHERE kind='qualified_initialize'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    worker.shutdown().await.unwrap();
}

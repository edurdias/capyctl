use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use mllm_controller::{
    coordinator::{CoordinatorOptions, ObservationFuture, OwnedCoordinator, ServiceObservation},
    OwnedCoordinatorState,
};
use mllm_domain::resources::MemoryObservation;
use mllm_management::{
    actions::OwnedActionSource, configuration::SharedConfigurationSource, lifecycle_router,
    ManagementCredentials,
};
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tower::ServiceExt;

const AUTH: &str = "management-credential-012345678901234567890";
const INFERENCE: &str = "inference-credential-0123456789012345678901";
#[path = "../../mllm-controller/tests/qualification_support/candidate_fixture.rs"]
mod candidate_fixture;
struct Observations(Vec<MemoryObservation>);
impl ServiceObservation for Observations {
    fn observe(&self, _: String) -> ObservationFuture {
        let values = self.0.clone();
        Box::pin(async move { Ok(values) })
    }
}

fn setup() -> (
    tempfile::TempDir,
    Arc<Mutex<OwnedCoordinatorState>>,
    OwnedCoordinator,
    String,
    axum::Router,
) {
    let (dir, owner, run, observations, host) = candidate_fixture::fixture();
    let worker = OwnedCoordinator::spawn_fake(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1200)),
        CoordinatorOptions::default(),
    )
    .unwrap();
    let configuration =
        Arc::new(SharedConfigurationSource::new(owner.clone(), host, "owner").unwrap());
    let app = lifecycle_router(
        ManagementCredentials::from_trusted_resolver(AUTH, INFERENCE).unwrap(),
        Arc::new(OwnedActionSource::new(configuration, worker.commands()).unwrap()),
    );
    (dir, owner, worker, run, app)
}
fn request(run: &str, key: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/management/v1/qualification-runs/{run}/actions"))
        .header("authorization", format!("Bearer {AUTH}"))
        .header("content-type", "application/json")
        .header("idempotency-key", key)
        .body(Body::from(body.to_string()))
        .unwrap()
}
async fn value(response: axum::response::Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap()
}

fn inference_request(run: &str, key: &str, body: Value) -> Request<Body> {
    let mut req = request(run, key, body);
    *req.uri_mut() = format!("/management/v1/qualification-runs/{run}/inference")
        .parse()
        .unwrap();
    req
}

#[tokio::test]
async fn candidate_inference_denies_wrong_body_scope_and_credentials_without_leases() {
    let (dir, _owner, worker, run, app) = setup();
    let init = app
        .clone()
        .oneshot(request(
            &run,
            "init",
            json!({"expected_revision":1,"action":"initialize","deadline_ms":400000}),
        ))
        .await
        .unwrap();
    let init = value(init).await;
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    wait_operation(&worker, &sql, init["operation_id"].as_str().unwrap()).await;
    let body = json!({"expected_revision":1,"request":{"model":format!("candidate-{}",init["deployment_id"].as_str().unwrap()),"messages":[{"role":"user","content":"Repeat exactly: MLLM_ALPHA_71"}],"temperature":0,"max_tokens":16,"stream":false}});
    for (field, status) in [
        ("revision", 409),
        ("model", 409),
        ("prompt", 409),
        ("tokens", 409),
        ("stream", 409),
        ("evidence", 400),
        ("unknown", 400),
    ] {
        let mut changed = body.clone();
        match field {
            "revision" => changed["expected_revision"] = json!(2),
            "model" => changed["request"]["model"] = json!("candidate-other-binding"),
            "prompt" => {
                changed["request"]["messages"][0]["content"] = json!("Repeat exactly: MLLM_BETA_29")
            }
            "tokens" => changed["request"]["max_tokens"] = json!(17),
            "stream" => changed["request"]["stream"] = json!(true),
            "evidence" => changed["evidence"] = json!({"passed":true}),
            _ => changed["request"]["unknown"] = json!(true),
        }
        let response = app
            .clone()
            .oneshot(inference_request(&run, field, changed))
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{field}");
        let text = value(response).await.to_string();
        assert!(!text.contains(AUTH) && !text.contains("MLLM_ALPHA_71"));
    }
    for credential in [INFERENCE, "wrong"] {
        let mut req = inference_request(&run, "auth", body.clone());
        req.headers_mut().insert(
            "authorization",
            format!("Bearer {credential}").parse().unwrap(),
        );
        assert_eq!(app.clone().oneshot(req).await.unwrap().status(), 401);
    }
    for missing in ["authorization", "idempotency-key"] {
        let mut req = inference_request(&run, "headers", body.clone());
        req.headers_mut().remove(missing);
        assert_eq!(
            app.clone().oneshot(req).await.unwrap().status(),
            if missing == "authorization" { 401 } else { 400 }
        );
    }
    let mut duplicate = inference_request(&run, "duplicate", body.clone());
    *duplicate.body_mut() = Body::from(
        body.to_string()
            .replace("\"max_tokens\":16", "\"max_tokens\":16,\"max_tokens\":16"),
    );
    assert_eq!(app.clone().oneshot(duplicate).await.unwrap().status(), 400);
    assert_eq!(
        app.clone()
            .oneshot(inference_request(
                "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                "wrong-run",
                body.clone()
            ))
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        app.clone()
            .oneshot(inference_request("not-a-run", "invalid", body.clone()))
            .await
            .unwrap()
            .status(),
        400
    );
    let mut query = inference_request(&run, "query", body.clone());
    *query.uri_mut() = format!("/management/v1/qualification-runs/{run}/inference?x=1")
        .parse()
        .unwrap();
    assert_eq!(app.clone().oneshot(query).await.unwrap().status(), 400);
    let mut oversized = inference_request(&run, "oversized", body);
    *oversized.body_mut() = Body::from(" ".repeat((1 << 20) + 1));
    assert_eq!(app.oneshot(oversized).await.unwrap().status(), 413);
    assert_eq!(
        sql.query_row(
            "SELECT COUNT(*) FROM operations WHERE kind='candidate_marker_v3'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        sql.query_row("SELECT COUNT(*) FROM request_leases", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    worker.shutdown().await.unwrap();
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // Hold acceptance only to drop real HTTP observers.
async fn candidate_inference_dropped_http_callers_keep_acceptance_capacity_and_one_send() {
    let (dir, owner, worker, run, app) = setup();
    let init = app
        .clone()
        .oneshot(request(
            &run,
            "init",
            json!({"expected_revision":1,"action":"initialize","deadline_ms":400000}),
        ))
        .await
        .unwrap();
    let init = value(init).await;
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    wait_operation(&worker, &sql, init["operation_id"].as_str().unwrap()).await;
    let body = json!({"expected_revision":1,"request":{"model":format!("candidate-{}",init["deployment_id"].as_str().unwrap()),"messages":[{"role":"user","content":"Repeat exactly: MLLM_ALPHA_71"}],"temperature":0,"max_tokens":16,"stream":false}});
    let guard = owner.lock().unwrap();
    let first = tokio::spawn(
        app.clone()
            .oneshot(inference_request(&run, "lost", body.clone())),
    );
    let second = tokio::spawn(
        app.clone()
            .oneshot(inference_request(&run, "lost", body.clone())),
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    first.abort();
    second.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    assert!(second.await.unwrap_err().is_cancelled());
    let full = app
        .clone()
        .oneshot(inference_request(&run, "full", body.clone()))
        .await
        .unwrap();
    assert_eq!(full.status(), 429);
    drop(guard);
    let accepted = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let response = app
                .clone()
                .oneshot(inference_request(&run, "lost", body.clone()))
                .await
                .unwrap();
            if response.status() == 202 {
                break value(response).await;
            }
            assert_eq!(response.status(), 429);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    wait_operation(&worker, &sql, accepted["operation_id"].as_str().unwrap()).await;
    assert_eq!(
        sql.query_row(
            "SELECT COUNT(*) FROM operations WHERE kind='candidate_marker_v3'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        sql.query_row("SELECT COUNT(*) FROM request_leases", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    worker.shutdown().await.unwrap();
    let replay = app
        .oneshot(inference_request(&run, "lost", body))
        .await
        .unwrap();
    assert_eq!(replay.status(), 202);
    assert_eq!(value(replay).await, accepted);
}

async fn wait_operation(worker: &OwnedCoordinator, sql: &rusqlite::Connection, operation: &str) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if sql
                .query_row(
                    "SELECT state='succeeded' FROM operations WHERE id=?1",
                    [operation],
                    |r| r.get::<_, bool>(0),
                )
                .unwrap()
            {
                break;
            }
            assert_eq!(
                worker.status(),
                mllm_controller::coordinator::WorkerStatus::Running
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn candidate_inference_http_runs_exact_corpus_once_with_original_acceptance_scope() {
    let (dir, _owner, worker, run, app) = setup();
    let init = app
        .clone()
        .oneshot(request(
            &run,
            "init",
            json!({"expected_revision":1,"action":"initialize","deadline_ms":400000}),
        ))
        .await
        .unwrap();
    assert_eq!(init.status(), 202);
    let init = value(init).await;
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    wait_operation(&worker, &sql, init["operation_id"].as_str().unwrap()).await;
    let mut receipts = Vec::new();
    let original_grant: String = sql
        .query_row("SELECT request_json FROM resource_grants", [], |r| r.get(0))
        .unwrap();
    for stream in [false, true] {
        for marker in ["MLLM_ALPHA_71", "MLLM_BETA_29"] {
            let key = format!("marker-{stream}-{marker}");
            let body = json!({"expected_revision":1,"request":{"model":format!("candidate-{}", init["deployment_id"].as_str().unwrap()),"messages":[{"role":"user","content":format!("Repeat exactly: {marker}")}],"temperature":0,"max_tokens":16,"stream":stream}});
            let response = app
                .clone()
                .oneshot(inference_request(&run, &key, body.clone()))
                .await
                .unwrap();
            assert_eq!(response.status(), 202);
            let accepted = value(response).await;
            assert_eq!(accepted["api_version"], "1");
            assert_eq!(accepted["deployment_id"], init["deployment_id"]);
            assert_eq!(accepted["qualification_run_id"], run);
            assert_eq!(accepted["revision"], "1");
            assert_eq!(accepted["joined"], false);
            wait_operation(&worker, &sql, accepted["operation_id"].as_str().unwrap()).await;
            receipts.push((key, body, accepted));
        }
    }
    worker.shutdown().await.unwrap();
    for (key, body, accepted) in receipts {
        let replay = app
            .clone()
            .oneshot(inference_request(&run, &key, body.clone()))
            .await
            .unwrap();
        assert_eq!(replay.status(), 202);
        assert_eq!(value(replay).await, accepted);
        let mut changed = body;
        changed["expected_revision"] = json!(2);
        let conflict = app
            .clone()
            .oneshot(inference_request(&run, &key, changed))
            .await
            .unwrap();
        assert_eq!(conflict.status(), 409);
        assert_eq!(
            value(conflict).await["error"]["code"],
            "idempotency_conflict"
        );
    }
    for (query, expected) in [
        ("SELECT COUNT(*) FROM operations WHERE kind='candidate_marker_v3' AND state='succeeded'",4),
        ("SELECT COUNT(*) FROM request_leases",0),
        ("SELECT COUNT(*) FROM resource_grants",1),
        ("SELECT COUNT(*) FROM qualifications",0),
        ("SELECT COUNT(*) FROM deployments WHERE admission_enabled!=0 OR dispatch_enabled!=0",0),
    ] {
        assert_eq!(sql.query_row(query, [], |r|r.get::<_, i64>(0)).unwrap(), expected, "{query}");
    }
    assert_eq!(
        sql.query_row("SELECT request_json FROM resource_grants", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        original_grant
    );
}
#[tokio::test]
async fn candidate_initialize_http_retry_completes_once_without_ordinary_dispatch_or_promotion() {
    let (dir, owner, worker, run, app) = setup();
    let body = json!({"expected_revision":1,"action":"initialize","deadline_ms":400000});
    let response = app
        .clone()
        .oneshot(request(&run, "init", body.clone()))
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    let accepted = value(response).await;
    assert_eq!(accepted["joined"], false);
    assert_eq!(accepted["revision"], "1");
    assert_eq!(accepted["qualification_run_id"], run);
    {
        let state = owner.lock().unwrap();
        let original = state
            .store()
            .candidate_run_snapshot("owner", &run)
            .unwrap()
            .unwrap();
        assert_eq!(
            accepted["deployment_id"],
            original.receipt().deployment_id()
        );
    }
    let replay = app
        .clone()
        .oneshot(request(&run, "init", body.clone()))
        .await
        .unwrap();
    assert_eq!(replay.status(), 202);
    assert_eq!(value(replay).await, accepted);
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let done: bool = sql
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1 AND state='succeeded')",
                    [accepted["operation_id"].as_str().unwrap()],
                    |r| r.get(0),
                )
                .unwrap();
            if done {
                break;
            }
            assert!(
                matches!(
                    worker.status(),
                    mllm_controller::coordinator::WorkerStatus::Running
                ),
                "{:?}",
                worker.status()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    for (query, expected) in [
        (
            "SELECT COUNT(*) FROM operations WHERE kind='candidate_action_v3'",
            1,
        ),
        (
            "SELECT COUNT(*) FROM operations WHERE kind='candidate_probe_v3'",
            1,
        ),
        (
            "SELECT COUNT(*) FROM lifecycle_steps WHERE state='completed'",
            3,
        ),
        ("SELECT COUNT(*) FROM request_leases", 0),
        ("SELECT COUNT(*) FROM qualifications", 0),
        (
            "SELECT COUNT(*) FROM deployments WHERE admission_enabled!=0 OR dispatch_enabled!=0",
            0,
        ),
        ("SELECT COUNT(*) FROM resource_grants", 1),
    ] {
        assert_eq!(
            sql.query_row(query, [], |r| r.get::<_, i64>(0)).unwrap(),
            expected,
            "{query}"
        );
    }
    {
        let state = owner.lock().unwrap();
        let snapshot = state
            .store()
            .candidate_run_snapshot("owner", &run)
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.requests_used(), 1);
        assert_eq!(snapshot.receipt().deadline_ms(), 500000);
    }
    worker.shutdown().await.unwrap();
    let replay = app.oneshot(request(&run, "init", body)).await.unwrap();
    assert_eq!(replay.status(), 202);
    assert_eq!(value(replay).await, accepted);
}

#[tokio::test]
async fn candidate_initialize_rejects_wrong_scope_wire_credentials_and_deadlines_before_effects() {
    let (dir, owner, worker, run, app) = setup();
    let body = json!({"expected_revision":1,"action":"initialize","deadline_ms":400000});
    let response = app
        .clone()
        .oneshot(request(
            &run,
            "internal-security",
            json!({"expected_revision":1,"action":"security","deadline_ms":400000}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(value(response).await["error"]["code"], "invalid_request");
    let mut cases = Vec::new();
    for change in [
        "revision",
        "elapsed",
        "extended",
        "evidence",
        "unknown",
        "unsupported",
    ] {
        let mut wire = body.clone();
        match change {
            "revision" => wire["expected_revision"] = json!(2),
            "elapsed" => wire["deadline_ms"] = json!(1000),
            "extended" => wire["deadline_ms"] = json!(500001),
            "evidence" => wire["evidence"] = json!({"ready":true}),
            "unknown" => wire["action"] = json!("anything"),
            _ => wire["action"] = json!("park"),
        }
        cases.push(request(&run, change, wire));
    }
    cases.push(request(
        "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "wrong-run",
        body.clone(),
    ));
    for uri in [
        format!("/management/v1/qualification-runs/{run}/actions?x=1"),
        "/management/v1/qualification-runs/not-a-run/actions".into(),
    ] {
        let mut req = request(&run, "target", body.clone());
        *req.uri_mut() = uri.parse().unwrap();
        cases.push(req);
    }
    for credential in [INFERENCE, "wrong"] {
        let mut req = request(&run, "credential", body.clone());
        req.headers_mut().insert(
            "authorization",
            format!("Bearer {credential}").parse().unwrap(),
        );
        cases.push(req);
    }
    for header in ["idempotency-key", "content-type", "authorization"] {
        let mut req = request(&run, "duplicate-header", body.clone());
        let value = req.headers().get(header).unwrap().clone();
        req.headers_mut().append(header, value);
        cases.push(req);
    }
    let mut duplicate = request(&run, "duplicate-json", body.clone());
    *duplicate.body_mut() = Body::from(
        r#"{"expected_revision":1,"action":"initialize","action":"initialize","deadline_ms":400000}"#,
    );
    cases.push(duplicate);
    for req in cases {
        let response = app.clone().oneshot(req).await.unwrap();
        assert!(
            response.status().is_client_error() || response.status() == 503,
            "{}",
            response.status()
        );
        let text = value(response).await.to_string();
        assert!(!text.contains(AUTH) && !text.contains(INFERENCE) && !text.contains("secret://"));
    }
    assert!(worker
        .commands()
        .initialize_candidate("other-principal", &run, 1, "wrong-principal", 400000)
        .is_err());
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    for table in [
        "lifecycle_steps",
        "lifecycle_runs",
        "resource_grants",
        "request_leases",
        "lifecycle_evidence",
    ] {
        assert_eq!(
            sql.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    owner.lock().unwrap().store().snapshot().unwrap();
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn candidate_durable_retry_requires_coherent_history_and_current_session() {
    let (dir, owner, worker, run, app) = setup();
    let body = json!({"expected_revision":1,"action":"initialize","deadline_ms":400000});
    let response = app
        .clone()
        .oneshot(request(&run, "init", body.clone()))
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    worker.shutdown().await.unwrap();
    let changed = json!({"expected_revision":1,"action":"initialize","deadline_ms":399999});
    let response = app
        .clone()
        .oneshot(request(&run, "init", changed))
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    assert_eq!(
        value(response).await["error"]["code"],
        "idempotency_conflict"
    );
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    sql.execute(
        "UPDATE command_receipts SET response_json='{}' WHERE idempotency_key='init'",
        [],
    )
    .unwrap();
    let response = app
        .clone()
        .oneshot(request(&run, "init", body.clone()))
        .await
        .unwrap();
    assert_eq!(response.status(), 500);
    owner
        .lock()
        .unwrap()
        .store()
        .begin_coordinator_session()
        .unwrap();
    let response = app.oneshot(request(&run, "init", body)).await.unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(
        value(response).await["error"]["code"],
        "reconciliation_required"
    );
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // Deliberately block acceptance to drop actual HTTP callers.
async fn cancelled_candidate_http_callers_keep_shared_capacity_and_one_owned_operation() {
    let (dir, owner, worker, run, app) = setup();
    let body = json!({"expected_revision":1,"action":"initialize","deadline_ms":400000});
    let guard = owner.lock().unwrap();
    let first = tokio::spawn(
        app.clone()
            .oneshot(request(&run, "lost-response", body.clone())),
    );
    let second = tokio::spawn(
        app.clone()
            .oneshot(request(&run, "lost-response", body.clone())),
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    first.abort();
    second.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    assert!(second.await.unwrap_err().is_cancelled());
    for path in [
        "/management/v1/deployments",
        "/management/v1/qualification-runs",
    ] {
        let mut req = request(&run, "capacity", json!({}));
        *req.uri_mut() = path.parse().unwrap();
        assert_eq!(
            value(app.clone().oneshot(req).await.unwrap()).await["error"]["code"],
            "queue_full"
        );
    }
    drop(guard);
    let accepted = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let response = app
                .clone()
                .oneshot(request(&run, "lost-response", body.clone()))
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
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        while !sql
            .query_row(
                "SELECT state='succeeded' FROM operations WHERE id=?1",
                [accepted["operation_id"].as_str().unwrap()],
                |r| r.get::<_, bool>(0),
            )
            .unwrap()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        sql.query_row(
            "SELECT COUNT(*) FROM operations WHERE kind='candidate_action_v3'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        sql.query_row(
            "SELECT COUNT(*) FROM operations WHERE kind='candidate_probe_v3'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn loopback_candidate_action_and_inference_retry_returns_acceptance_after_transport_loss() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (dir, _owner, worker, run, app) = setup();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = stopped.await;
            })
            .await
            .unwrap();
    });
    let body =
        json!({"expected_revision":1,"action":"initialize","deadline_ms":400000}).to_string();
    let wire=format!("POST /management/v1/qualification-runs/{run}/actions HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {AUTH}\r\nContent-Type: application/json\r\nIdempotency-Key: network-retry\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    client.write_all(wire.as_bytes()).await.unwrap();
    let sql = rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        while sql
            .query_row(
                "SELECT COUNT(*) FROM operations WHERE kind='candidate_action_v3'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            == 0
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // Lose the response without cancelling the application's accepted work.
    drop(client);
    let mut retry = tokio::net::TcpStream::connect(address).await.unwrap();
    retry.write_all(wire.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(30),
        retry.take(65536).read_to_end(&mut bytes),
    )
    .await
    .unwrap()
    .unwrap();
    let response = String::from_utf8(bytes).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 202 Accepted\r\n"),
        "{response}"
    );
    assert!(!response.contains(AUTH));
    let accepted: Value = serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        while !sql
            .query_row(
                "SELECT state='succeeded' FROM operations WHERE id=?1",
                [accepted["operation_id"].as_str().unwrap()],
                |r| r.get::<_, bool>(0),
            )
            .unwrap()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        sql.query_row(
            "SELECT COUNT(*) FROM operations WHERE kind='candidate_action_v3'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    let body=json!({"expected_revision":1,"request":{"model":format!("candidate-{}",accepted["deployment_id"].as_str().unwrap()),"messages":[{"role":"user","content":"Repeat exactly: MLLM_ALPHA_71"}],"temperature":0,"max_tokens":16,"stream":false}}).to_string();
    let wire=format!("POST /management/v1/qualification-runs/{run}/inference HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {AUTH}\r\nContent-Type: application/json\r\nIdempotency-Key: marker-network-retry\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    client.write_all(wire.as_bytes()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        while sql
            .query_row(
                "SELECT COUNT(*) FROM operations WHERE kind='candidate_marker_v3'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            == 0
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    drop(client);
    let mut retry = tokio::net::TcpStream::connect(address).await.unwrap();
    retry.write_all(wire.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(30),
        retry.take(65536).read_to_end(&mut bytes),
    )
    .await
    .unwrap()
    .unwrap();
    let response = String::from_utf8(bytes).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 202 Accepted\r\n"),
        "{response}"
    );
    assert!(!response.contains(AUTH) && !response.contains("MLLM_ALPHA_71"));
    let marker: Value = serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
    wait_operation(&worker, &sql, marker["operation_id"].as_str().unwrap()).await;
    assert_eq!(marker["deployment_id"], accepted["deployment_id"]);
    assert_eq!(marker["qualification_run_id"], run);
    assert_eq!(
        sql.query_row(
            "SELECT COUNT(*) FROM operations WHERE kind='candidate_marker_v3'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    stop.send(()).unwrap();
    server.await.unwrap();
    worker.shutdown().await.unwrap();
}

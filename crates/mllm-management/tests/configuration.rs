use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use mllm_config::effective::resolve_effective;
use mllm_controller::OwnedCoordinatorState;
use mllm_management::{
    configuration::SharedConfigurationSource, configuration_router, ManagementCredentials,
};
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
};
use tower::ServiceExt;

const MANAGEMENT: &str = "management-credential-012345678901234567890";
const INFERENCE: &str = "inference-credential-0123456789012345678901";

fn fixture() -> (
    tempfile::TempDir,
    Arc<Mutex<OwnedCoordinatorState>>,
    Value,
    Value,
) {
    // Production custody rejects the group-writable checkout and shared /tmp.
    let directory = tempfile::Builder::new()
        .prefix("mllm-management-")
        .tempdir_in(std::env::var_os("HOME").unwrap())
        .unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let state = OwnedCoordinatorState::open(directory.path()).unwrap();
    let fixture: Value = serde_json::from_str(include_str!(
        "../../mllm-config/tests/fixtures/effective-fake-golden.json"
    ))
    .unwrap();
    let config = fixture["input"]["deployment"].clone();
    let host = fixture["input"]["host"].clone();
    let effective = resolve_effective(&config, &host).unwrap();
    state
        .store()
        .import_resource_policy(
            state.session(),
            &effective.host,
            &[mllm_domain::resources::MemoryObservation {
                domain: "unified".into(),
                capacity_bytes: 64_i64 << 30,
                available_bytes: 64_i64 << 30,
                sampled_at_ms: 1000,
            }],
            1000,
        )
        .unwrap();
    (directory, Arc::new(Mutex::new(state)), config, host)
}

fn app(state: Arc<Mutex<OwnedCoordinatorState>>, host: Value) -> axum::Router {
    configuration_router(
        ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap(),
        Arc::new(SharedConfigurationSource::new(state, host, "owner").unwrap()),
    )
}
fn request(method: &str, path: &str, key: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {MANAGEMENT}"))
        .header("content-type", "application/json")
        .header("idempotency-key", key)
        .body(Body::from(body.to_string()))
        .unwrap()
}
async fn json_response(response: axum::response::Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap()
}

#[tokio::test]
async fn authenticated_configuration_accepts_replays_and_replaces_without_runtime_effects() {
    let (_directory, state, mut config, host) = fixture();
    let router = app(state.clone(), host);
    let command = json!({"config":config,"activate":false});
    let response = router
        .clone()
        .oneshot(request(
            "POST",
            "/management/v1/deployments",
            "create",
            command.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let created = json_response(response).await;
    assert_eq!(created["api_version"], "1");
    assert_eq!(created["revision"], "1");
    let replay = router
        .clone()
        .oneshot(request(
            "POST",
            "/management/v1/deployments",
            "create",
            command,
        ))
        .await
        .unwrap();
    assert_eq!(replay.status(), 202);
    assert_eq!(json_response(replay).await, created);
    config["routes"] = json!(["replacement"]);
    let path = format!(
        "/management/v1/deployments/{}",
        created["deployment_id"].as_str().unwrap()
    );
    let replaced = router
        .clone()
        .oneshot(request(
            "PUT",
            &path,
            "replace",
            json!({"config":config,"expected_revision":1}),
        ))
        .await
        .unwrap();
    assert_eq!(replaced.status(), 202);
    assert_eq!(json_response(replaced).await["revision"], "2");
    let owner = state.lock().unwrap();
    assert_eq!(owner.session().epoch(), 1);
    assert!(owner.store().resource_snapshot().unwrap().owners.is_empty());
    assert!(owner
        .store()
        .runtime_binding(created["deployment_id"].as_str().unwrap())
        .unwrap()
        .is_none());
    let deployment = owner
        .store()
        .get_deployment(created["deployment_id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(
        deployment.desired_state,
        mllm_domain::LifecycleState::Stopped
    );
}

#[tokio::test]
async fn activation_and_inference_credentials_cannot_create_configuration() {
    let (_directory, state, config, host) = fixture();
    let router = app(state.clone(), host);
    let mut unauthorized = request(
        "POST",
        "/management/v1/deployments",
        "deny",
        json!({"config":config,"activate":false}),
    );
    unauthorized.headers_mut().insert(
        "authorization",
        format!("Bearer {INFERENCE}").parse().unwrap(),
    );
    assert_eq!(
        router.clone().oneshot(unauthorized).await.unwrap().status(),
        401
    );
    let response = router
        .oneshot(request(
            "POST",
            "/management/v1/deployments",
            "deny",
            json!({"config":config,"activate":true}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(
        json_response(response).await["error"]["code"],
        "unsupported_capability"
    );
    assert_eq!(state.lock().unwrap().store().deployment_count().unwrap(), 0);
}

#[derive(Default)]
struct RejectingSource(
    std::sync::atomic::AtomicUsize,
    Option<Arc<Gate>>,
    ProviderResult,
);
#[derive(Default)]
enum ProviderResult {
    #[default]
    Failure,
    Receipt(mllm_store::managed_configuration::ManagedConfigurationReceipt),
    Panic,
}
#[derive(Default)]
struct Gate(Mutex<bool>, std::sync::Condvar);
impl Gate {
    fn release(&self) {
        *self.0.lock().unwrap() = true;
        self.1.notify_all();
    }
}
struct ReleaseGate(Arc<Gate>);
impl Drop for ReleaseGate {
    fn drop(&mut self) {
        self.0.release();
    }
}
impl mllm_management::SnapshotSource for RejectingSource {
    fn snapshot(
        &self,
    ) -> Result<mllm_store::snapshot::Snapshot, mllm_management::SnapshotUnavailable> {
        Err(mllm_management::SnapshotUnavailable)
    }
}
impl mllm_management::events::EventSource for RejectingSource {
    fn events_after(
        &self,
        _: Option<&str>,
        _: usize,
    ) -> Result<mllm_store::events::EventPage, mllm_store::events::EventReadError> {
        Err(mllm_store::events::EventReadError::InvalidLimit)
    }
}
impl mllm_management::configuration::ConfigurationSource for RejectingSource {
    fn accept(
        &self,
        _: &str,
        _: mllm_management::configuration::ConfigurationCommand,
    ) -> Result<
        mllm_store::managed_configuration::ManagedConfigurationReceipt,
        mllm_management::configuration::ConfigurationFailure,
    > {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(gate) = &self.1 {
            let guard = gate.0.lock().unwrap();
            let _ = gate
                .1
                .wait_timeout_while(guard, std::time::Duration::from_secs(30), |released| {
                    !*released
                })
                .unwrap();
        }
        match &self.2 {
            ProviderResult::Failure => {
                Err(mllm_management::configuration::ConfigurationFailure::Internal)
            }
            ProviderResult::Receipt(receipt) => Ok(receipt.clone()),
            ProviderResult::Panic => panic!("test-only private provider diagnostic"),
        }
    }
}
impl mllm_management::candidates::CandidateSource for RejectingSource {
    fn create_candidate(
        &self,
        _: &str,
        _: &str,
    ) -> Result<
        mllm_store::candidate_creation::CandidateCreationReceipt,
        mllm_management::configuration::ConfigurationFailure,
    > {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(mllm_management::configuration::ConfigurationFailure::Internal)
    }
}
fn rejecting_app(source: Arc<RejectingSource>) -> axum::Router {
    mllm_management::candidate_acceptance_router(
        ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap(),
        source,
    )
}
fn raw_request(method: &str, path: &str, body: &str) -> Request<Body> {
    let mut value = request(method, path, "syntax", Value::Null);
    *value.body_mut() = Body::from(body.to_owned());
    value
}

#[tokio::test]
async fn malformed_envelopes_never_reach_command_provider() {
    let source = Arc::new(RejectingSource::default());
    let router = rejecting_app(source.clone());
    for body in [
        "",
        "null",
        "[]",
        "{}",
        r#"{"config":{},"activate":false,"extra":0}"#,
        r#"{"config":{},"activate":false,"activate":false}"#,
        r#"{"config":{"nested":{"a":1,"a":2}},"activate":false}"#,
        r#"{"config":{"a":1,"\u0061":2},"activate":false}"#,
        r#"{"config":{},"activate":"false"}"#,
        r#"{"config":{},"activate":false} {}"#,
    ] {
        let response = router
            .clone()
            .oneshot(raw_request("POST", "/management/v1/deployments", body))
            .await
            .unwrap();
        assert_eq!(response.status(), 400, "body: {body}");
    }
    let deep = format!(
        "{{\"config\":{}0{},\"activate\":false}}",
        "[".repeat(150),
        "]".repeat(150)
    );
    assert_eq!(
        router
            .clone()
            .oneshot(raw_request("POST", "/management/v1/deployments", &deep))
            .await
            .unwrap()
            .status(),
        400
    );
    let oversized = " ".repeat((1 << 20) + 1);
    assert_eq!(
        router
            .clone()
            .oneshot(raw_request(
                "POST",
                "/management/v1/deployments",
                &oversized
            ))
            .await
            .unwrap()
            .status(),
        413
    );
    let target = format!("/management/v1/deployments/{}", ulid::Ulid::new());
    for revision in ["0", "-1", "1.0", "\"1\"", "9223372036854775808", "null"] {
        let body = format!("{{\"config\":{{}},\"expected_revision\":{revision}}}");
        assert_eq!(
            router
                .clone()
                .oneshot(raw_request("PUT", &target, &body))
                .await
                .unwrap()
                .status(),
            400
        );
    }
    assert_eq!(source.0.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn ambiguous_headers_queries_and_targets_never_reach_provider() {
    let source = Arc::new(RejectingSource::default());
    let router = rejecting_app(source.clone());
    for case in 0..9 {
        let mut command = request(
            "POST",
            "/management/v1/deployments",
            "syntax",
            json!({"config":{},"activate":false}),
        );
        let headers = command.headers_mut();
        match case {
            0 => {
                headers.remove("idempotency-key");
            }
            1 => {
                headers.append("idempotency-key", "second".parse().unwrap());
            }
            2 => {
                headers.insert("idempotency-key", " ".parse().unwrap());
            }
            3 => {
                headers.insert("idempotency-key", "x".repeat(257).parse().unwrap());
            }
            4 => {
                headers.remove("content-type");
            }
            5 => {
                headers.append("content-type", "application/json".parse().unwrap());
            }
            6 => {
                headers.insert("content-type", "text/plain".parse().unwrap());
            }
            7 => {
                headers.insert("content-encoding", "identity".parse().unwrap());
            }
            8 => {
                *command.uri_mut() = "/management/v1/deployments?activate=false".parse().unwrap();
            }
            _ => unreachable!(),
        }
        assert_eq!(
            router.clone().oneshot(command).await.unwrap().status(),
            400,
            "case {case}"
        );
    }
    for id in [
        "bad",
        "01arz3ndektsv4rrffq69g5fav",
        "%30%31ARZ3NDEKTSV4RRFFQ69G5FAV",
    ] {
        assert_eq!(
            router
                .clone()
                .oneshot(request(
                    "PUT",
                    &format!("/management/v1/deployments/{id}"),
                    "syntax",
                    json!({"config":{},"expected_revision":1})
                ))
                .await
                .unwrap()
                .status(),
            400
        );
    }
    let mut duplicate_auth = request(
        "POST",
        "/management/v1/deployments",
        "syntax",
        json!({"config":{},"activate":false}),
    );
    duplicate_auth.headers_mut().append(
        "authorization",
        format!("Bearer {MANAGEMENT}").parse().unwrap(),
    );
    assert_eq!(
        router
            .clone()
            .oneshot(duplicate_auth)
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(source.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    let response = router
        .oneshot(request(
            "POST",
            "/management/v1/deployments",
            "valid",
            json!({"config":{},"activate":false}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 500);
    assert_eq!(
        json_response(response).await["error"]["message"],
        "Management command failed"
    );
    assert_eq!(source.0.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_requests_retain_command_capacity_until_blocking_work_finishes() {
    let gate = Arc::new(Gate::default());
    let _release_on_failure = ReleaseGate(gate.clone());
    let source = Arc::new(RejectingSource(
        Default::default(),
        Some(gate.clone()),
        ProviderResult::Failure,
    ));
    let router = rejecting_app(source.clone());
    let mut requests = Vec::new();
    for index in 0..2 {
        requests.push(tokio::spawn(router.clone().oneshot(request(
            "POST",
            "/management/v1/deployments",
            &format!("blocking-{index}"),
            json!({"config":{},"activate":false}),
        ))));
    }
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while source.0.load(std::sync::atomic::Ordering::SeqCst) != 2 {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    for pending in requests {
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
    }
    let response = router
        .clone()
        .oneshot(request(
            "POST",
            "/management/v1/deployments",
            "overflow",
            json!({"config":{},"activate":false}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 429);
    assert_eq!(json_response(response).await["error"]["code"], "queue_full");
    assert_eq!(source.0.load(std::sync::atomic::Ordering::SeqCst), 2);
    // Candidate commands share the same capacity; separate route budgets would
    // permit unbounded growth as management actions are added.
    let candidate = router
        .clone()
        .oneshot(request(
            "POST",
            "/management/v1/qualification-runs",
            "overflow",
            json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(candidate.status(), 429);
    assert_eq!(source.0.load(std::sync::atomic::Ordering::SeqCst), 2);
    // Read capacity is independent: the snapshot provider's closed error is 500,
    // not the command queue's 429.
    let read = Request::builder()
        .uri("/management/v1/snapshot")
        .header("authorization", format!("Bearer {MANAGEMENT}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(router.clone().oneshot(read).await.unwrap().status(), 500);
    gate.release();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let response = router
                .clone()
                .oneshot(request(
                    "POST",
                    "/management/v1/deployments",
                    "after",
                    json!({"config":{},"activate":false}),
                ))
                .await
                .unwrap();
            if response.status() != 429 {
                assert_eq!(response.status(), 500);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn durable_conflicts_are_closed_and_do_not_replace_configuration() {
    let (_directory, state, mut config, host) = fixture();
    let router = app(state.clone(), host);
    let created = json_response(
        router
            .clone()
            .oneshot(request(
                "POST",
                "/management/v1/deployments",
                "first",
                json!({"config":config,"activate":false}),
            ))
            .await
            .unwrap(),
    )
    .await;
    let path = format!(
        "/management/v1/deployments/{}",
        created["deployment_id"].as_str().unwrap()
    );
    let response = router
        .clone()
        .oneshot(request(
            "POST",
            "/management/v1/deployments",
            "another",
            json!({"config":config,"activate":false}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    assert_eq!(
        json_response(response).await["error"]["code"],
        "route_conflict"
    );
    config["routes"] = json!(["changed"]);
    let response = router
        .clone()
        .oneshot(request(
            "POST",
            "/management/v1/deployments",
            "first",
            json!({"config":config,"activate":false}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    assert_eq!(
        json_response(response).await["error"]["code"],
        "idempotency_conflict"
    );
    let response = router
        .clone()
        .oneshot(request(
            "PUT",
            &path,
            "stale",
            json!({"config":config,"expected_revision":2}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    assert_eq!(
        json_response(response).await["error"]["code"],
        "revision_conflict"
    );
    let missing = format!("/management/v1/deployments/{}", ulid::Ulid::new());
    let response = router
        .oneshot(request(
            "PUT",
            &missing,
            "missing",
            json!({"config":config,"expected_revision":1}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert_eq!(json_response(response).await["error"]["code"], "not_found");
    let owner = state.lock().unwrap();
    assert_eq!(owner.store().deployment_count().unwrap(), 1);
    assert!(owner.store().resource_snapshot().unwrap().owners.is_empty());
}

#[tokio::test]
async fn corrupt_receipts_and_provider_panics_never_escape_as_success() {
    let valid = mllm_store::managed_configuration::ManagedConfigurationReceipt {
        version: 1,
        operation_id: ulid::Ulid::new().to_string(),
        deployment_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
        revision: 1,
        generation: 1,
        resource_policy_revision: 1,
        accepted_at_ms: 1,
    };
    for case in 0..9 {
        let mut receipt = valid.clone();
        match case {
            0 => receipt.version = 2,
            1 => receipt.operation_id = "private diagnostic".into(),
            2 => receipt.deployment_id = receipt.deployment_id.to_lowercase(),
            3 => receipt.revision = 0,
            4 => receipt.generation = 0,
            5 => receipt.resource_policy_revision = 0,
            6 => receipt.accepted_at_ms = -1,
            _ => (),
        }
        let outcome = if case == 7 {
            ProviderResult::Panic
        } else {
            ProviderResult::Receipt(receipt)
        };
        let router = rejecting_app(Arc::new(RejectingSource(Default::default(), None, outcome)));
        let response = router
            .oneshot(request(
                "POST",
                "/management/v1/deployments",
                "receipt",
                json!({"config":{},"activate":false}),
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if case == 8 { 202 } else { 500 },
            "case {case}"
        );
        let body = json_response(response).await;
        assert!(!body.to_string().contains("private"));
        if case == 8 {
            assert_eq!(body["revision"], "1");
        }
    }
}

#[tokio::test]
async fn response_deadline_does_not_cancel_started_provider_work() {
    let gate = Arc::new(Gate::default());
    let _release_on_failure = ReleaseGate(gate.clone());
    let source = Arc::new(RejectingSource(
        Default::default(),
        Some(gate.clone()),
        ProviderResult::Failure,
    ));
    let router = rejecting_app(source.clone());
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(17),
        router.oneshot(request(
            "POST",
            "/management/v1/deployments",
            "deadline",
            json!({"config":{},"activate":false}),
        )),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), 504);
    assert_eq!(
        json_response(response).await["error"]["code"],
        "deadline_exceeded"
    );
    assert_eq!(source.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(!*gate.0.lock().unwrap());
    gate.release();
}

#[tokio::test]
async fn slow_bodies_time_out_without_provider_work_and_release_capacity() {
    let source = Arc::new(RejectingSource::default());
    let router = rejecting_app(source.clone());
    let mut pending = request(
        "POST",
        "/management/v1/deployments",
        "slow",
        json!({"config":{},"activate":false}),
    );
    *pending.body_mut() = Body::from_stream(futures::stream::pending::<
        Result<axum::body::Bytes, std::io::Error>,
    >());
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(7),
        router.clone().oneshot(pending),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), 504);
    assert_eq!(source.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    let response = router
        .oneshot(request(
            "POST",
            "/management/v1/deployments",
            "after",
            json!({"config":{},"activate":false}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 500);
    assert_eq!(source.0.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn current_policy_is_composed_without_breaking_historical_retries() {
    let (_directory, state, mut config, host) = fixture();
    config.as_object_mut().unwrap().remove("request_deadline");
    let router = app(state.clone(), host.clone());
    let command = json!({"config":config,"activate":false});
    let response = router
        .clone()
        .oneshot(request(
            "POST",
            "/management/v1/deployments",
            "historical",
            command.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    let created = json_response(response).await;
    {
        let owner = state.lock().unwrap();
        let host_id = host["name"].as_str().unwrap();
        let mut policy = owner.store().resource_policy(host_id).unwrap().unwrap();
        policy.controls.queue.request_deadline_ms = 100_000;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        owner
            .store()
            .update_resource_policy(
                owner.session(),
                "owner",
                host_id,
                1,
                "policy-change",
                &policy.controls,
                &[mllm_domain::resources::MemoryObservation {
                    domain: "unified".into(),
                    capacity_bytes: 64_i64 << 30,
                    available_bytes: 64_i64 << 30,
                    sampled_at_ms: now,
                }],
                now,
            )
            .unwrap();
    }
    let replay = router
        .clone()
        .oneshot(request(
            "POST",
            "/management/v1/deployments",
            "historical",
            command.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(replay.status(), 202);
    assert_eq!(json_response(replay).await, created);
    // Reconstructed provider retains the same owner/session, but current profile
    // removal must not invalidate an already accepted historical command.
    let mut removed_profiles = host;
    removed_profiles["runtime_profiles"] = json!({});
    let replay = app(state.clone(), removed_profiles)
        .oneshot(request(
            "POST",
            "/management/v1/deployments",
            "historical",
            command,
        ))
        .await
        .unwrap();
    assert_eq!(replay.status(), 202);
    assert_eq!(json_response(replay).await, created);
    config["name"] = json!("current-policy");
    config["routes"] = json!(["current-policy"]);
    let response = router
        .oneshot(request(
            "POST",
            "/management/v1/deployments",
            "new-policy",
            json!({"config":config,"activate":false}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    assert_eq!(state.lock().unwrap().session().epoch(), 1);
}

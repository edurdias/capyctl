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
        "../../mllm-config/tests/fixtures/effective-vllm-golden.json"
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
fn rejecting_app(source: Arc<RejectingSource>) -> axum::Router {
    configuration_router(
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
    // Every command route shares the same capacity; separate route budgets would
    // permit unbounded growth as management commands are added.
    let replacement = router
        .clone()
        .oneshot(request(
            "PUT",
            "/management/v1/deployments/01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "overflow",
            json!({"expected_revision":1,"config":{}}),
        ))
        .await
        .unwrap();
    assert_eq!(replacement.status(), 429);
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

// T07, T16, T33: registry selection uses authenticated host identity, preserves
// disjoint resource keys, and retains the source needed for remote execution.
#[tokio::test]
async fn registry_configuration_freezes_selected_host_and_replays() {
    let (_directory, state, mut config, mut host) = fixture();
    let host_id = {
        let owner = state.lock().unwrap();
        owner
            .store()
            .create_host_invitation(&"b".repeat(64), "host-a", 100, 0)
            .unwrap();
        let cert = owner
            .store()
            .redeem_host_invitation(
                &mllm_store::enrollment::Redemption {
                    invitation_digest: "b".repeat(64),
                    transaction_id: "remote-config".into(),
                    host_name: "host-a".into(),
                    key_digest: "c".repeat(64),
                    csr_digest: "d".repeat(64),
                },
                1,
                |id| {
                    Ok(mllm_store::enrollment::CertificateRecord {
                        host_id: id.into(),
                        fingerprint: "a".repeat(64),
                        certificate_pem: "certificate".into(),
                        expires_unix: 500,
                    })
                },
            )
            .unwrap();
        host["name"] = "host-a".into();
        host["state_dir"] = "/home/operator/host".into();
        host["identity_dir"] = "/home/operator/host/identity".into();
        let publication = mllm_store::host_publication::HostPublication {
            host_id: cert.host_id.clone(),
            config_json: host.to_string(),
            boot_id: "boot-a".into(),
            fingerprint: mllm_config::remote_resources::policy_fingerprint(&host),
            received_at_ms: 1000,
        };
        owner
            .store()
            .publish_host_configuration(&publication)
            .unwrap();
        let local = mllm_config::remote_resources::local_host_document(&host).unwrap();
        let policy = mllm_config::effective::normalize_host_policy(&local).unwrap();
        owner
            .store()
            .import_remote_resource_policy(
                owner.session(),
                &cert.host_id,
                &policy,
                &[mllm_domain::resources::MemoryObservation {
                    domain: "unified".into(),
                    capacity_bytes: 64_i64 << 30,
                    available_bytes: 64_i64 << 30,
                    sampled_at_ms: 1000,
                }],
                1000,
            )
            .unwrap();
        cert.host_id
    };
    let router = configuration_router(
        ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap(),
        Arc::new(SharedConfigurationSource::from_registry(state.clone(), "owner").unwrap()),
    );
    config["host"] = "host-a".into();
    let body = json!({"config":config,"activate":false});
    let accepted = router
        .clone()
        .oneshot(request(
            "POST",
            "/management/v1/deployments",
            "remote-create",
            body.clone(),
        ))
        .await
        .unwrap();
    let status = accepted.status();
    let accepted = json_response(accepted).await;
    assert_eq!(status, 202, "{accepted}");
    let replay = router
        .clone()
        .oneshot(request(
            "POST",
            "/management/v1/deployments",
            "remote-create",
            body,
        ))
        .await
        .unwrap();
    assert_eq!(replay.status(), 202);
    assert_eq!(json_response(replay).await, accepted);
    {
        let owner = state.lock().unwrap();
        let source = owner
            .store()
            .managed_configuration_source(accepted["deployment_id"].as_str().unwrap(), 1)
            .unwrap()
            .unwrap();
        assert_eq!(
            source["devices"][0]["id"],
            mllm_config::remote_resources::ledger_key(&host_id, "device", "gpu0")
        );
        let restored =
            mllm_config::remote_resources::local_deployment_document(&host_id, &source).unwrap();
        config.as_object_mut().unwrap().remove("host");
        assert_eq!(restored, config);
    }
    config["host"] = "unknown-host".into();
    let denied = router
        .oneshot(request(
            "POST",
            "/management/v1/deployments",
            "unknown-create",
            json!({"config":config,"activate":false}),
        ))
        .await
        .unwrap();
    assert_eq!(denied.status(), 503);
    assert_eq!(state.lock().unwrap().store().deployment_count().unwrap(), 1);
}

/// ADR 0014 §7 (WE3): a deploy returns with its checkpoint digest pending
/// (`checkpoint_digest_pending`) until a host holding the checkpoint measures
/// it, including a deploy whose memory request derives from the weights.
// T14
#[tokio::test]
async fn a_deploy_returns_with_its_checkpoint_digest_pending() {
    let (_directory, state, mut config, host) = fixture();
    let router = app(state.clone(), host);
    let created = json_response(
        router
            .clone()
            .oneshot(request(
                "POST",
                "/management/v1/deployments",
                "declared",
                json!({"config":config,"activate":false}),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(created["checkpoint_digest"], "pending");
    config["name"] = json!("derived");
    config["routes"] = json!(["derived"]);
    config.as_object_mut().unwrap().remove("resources");
    config["engine_config"]["memory"] = json!({"kv_cache": "4GiB"});
    let derived = router
        .clone()
        .oneshot(request(
            "POST",
            "/management/v1/deployments",
            "derived",
            json!({"config":config,"activate":false}),
        ))
        .await
        .unwrap();
    assert_eq!(derived.status(), 202);
    let derived = json_response(derived).await;
    assert_eq!(derived["checkpoint_digest"], "pending");
    let owner = state.lock().unwrap();
    let record = owner
        .store()
        .checkpoint_digest(derived["deployment_id"].as_str().unwrap(), 1)
        .unwrap()
        .unwrap();
    assert!(record.provisional, "activation waits for the digest");
}

/// Enroll and publish one registry host with the given placement labels.
fn publish_host(
    state: &Arc<Mutex<OwnedCoordinatorState>>,
    name: &str,
    digest: char,
    labels: Value,
) -> String {
    let owner = state.lock().unwrap();
    let invitation = digest.to_string().repeat(64);
    owner
        .store()
        .create_host_invitation(&invitation, name, 100, 0)
        .unwrap();
    let cert = owner
        .store()
        .redeem_host_invitation(
            &mllm_store::enrollment::Redemption {
                invitation_digest: invitation,
                transaction_id: format!("tx-{name}"),
                host_name: name.into(),
                key_digest: "c".repeat(64),
                csr_digest: "d".repeat(64),
            },
            1,
            |id| {
                Ok(mllm_store::enrollment::CertificateRecord {
                    host_id: id.into(),
                    fingerprint: digest.to_string().repeat(64),
                    certificate_pem: "certificate".into(),
                    expires_unix: 500,
                })
            },
        )
        .unwrap();
    let (_, mut host) = {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../mllm-config/tests/fixtures/effective-vllm-golden.json"
        ))
        .unwrap();
        (
            fixture["input"]["deployment"].clone(),
            fixture["input"]["host"].clone(),
        )
    };
    host["name"] = name.into();
    host["state_dir"] = format!("/home/operator/{name}").into();
    host["identity_dir"] = format!("/home/operator/{name}/identity").into();
    host["resource_policy"]["labels"] = labels;
    owner
        .store()
        .publish_host_configuration(&mllm_store::host_publication::HostPublication {
            host_id: cert.host_id.clone(),
            config_json: host.to_string(),
            boot_id: "boot-a".into(),
            fingerprint: mllm_config::remote_resources::policy_fingerprint(&host),
            received_at_ms: 1000,
        })
        .unwrap();
    let local = mllm_config::remote_resources::local_host_document(&host).unwrap();
    let policy = mllm_config::effective::normalize_host_policy(&local).unwrap();
    owner
        .store()
        .import_remote_resource_policy(
            owner.session(),
            &cert.host_id,
            &policy,
            &[mllm_domain::resources::MemoryObservation {
                domain: "unified".into(),
                capacity_bytes: 64_i64 << 30,
                available_bytes: 64_i64 << 30,
                sampled_at_ms: 1000,
            }],
            1000,
        )
        .unwrap();
    cert.host_id
}

/// ADR 0013 §2–3: a registry deployment is resolved against every allowed
/// host whose published labels satisfy its selector; a host that does not is
/// recorded refused with its reason and is never a candidate, and a
/// deployment no allowed host can take is refused.
// T03 T14 T16
#[tokio::test]
async fn registry_resolves_every_allowed_host_the_selector_matches() {
    let (_directory, state, mut config, _) = fixture();
    let a = publish_host(&state, "host-a", 'e', json!({"gpu": "gb10"}));
    let b = publish_host(&state, "host-b", 'f', json!({"gpu": "other"}));
    let router = configuration_router(
        ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap(),
        Arc::new(SharedConfigurationSource::from_registry(state.clone(), "owner").unwrap()),
    );
    config["instances"] = json!(1);
    config["placement"] = json!({"hosts": ["host-a", "host-b"], "selector": {"gpu": "gb10"}});
    config["devices"] = json!([{"sharing": "shared"}]);
    for phase in ["cold", "ready", "parking", "wake"] {
        config["resources"][phase]["devices"] = json!([{"sharing": "shared"}]);
    }
    let response = router
        .clone()
        .oneshot(request(
            "POST",
            "/management/v1/deployments",
            "selected",
            json!({"config":config,"activate":false}),
        ))
        .await
        .unwrap();
    let status = response.status();
    let body = json_response(response).await;
    assert_eq!(status, 202, "{body}");
    let id = body["deployment_id"].as_str().unwrap().to_owned();
    let snapshot = state.lock().unwrap().store().snapshot().unwrap();
    let deployment = snapshot.deployments.iter().find(|d| d.id == id).unwrap();
    let hosts: Vec<_> = deployment
        .hosts
        .iter()
        .map(|h| (h.host_id.clone(), h.outcome.clone(), h.diagnostic.clone()))
        .collect();
    assert!(hosts.contains(&(a, "resolved".into(), None)), "{hosts:?}");
    assert!(
        hosts.contains(&(
            "host-b".into(),
            "refused".into(),
            Some("selector_mismatch".into())
        )),
        "{hosts:?}"
    );
    let _ = b;
    // No allowed host carries the label: refused as a whole.
    config["name"] = json!("nowhere");
    config["routes"] = json!(["nowhere"]);
    config["placement"]["selector"] = json!({"gpu": "h100"});
    let response = router
        .oneshot(request(
            "POST",
            "/management/v1/deployments",
            "nowhere",
            json!({"config":config,"activate":false}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
}

/// SPEC §15.3 / §14: a deploy the configuration refuses names why, exactly as
/// `validate config` does, instead of a bare "Invalid deployment
/// configuration" (found live 2026-09-23 in the two-host matrix).
// T03
#[tokio::test]
async fn a_refused_deploy_names_its_configuration_reason() {
    let (_directory, state, mut config, host) = fixture();
    let router = app(state, host);
    config["engine_config"]["accept_extra_args"] = json!(true);
    config["engine_config"]["extra_args"] = json!(["--port", "9000"]);
    let response = router
        .oneshot(request(
            "POST",
            "/management/v1/deployments",
            "refused",
            json!({"config":config,"activate":false}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let body = json_response(response).await;
    assert_eq!(body["error"]["code"], "invalid_config", "{body}");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.starts_with("Invalid deployment configuration: "),
        "{message}"
    );
    assert!(message.contains("engine_config.extra_args"), "{message}");
    assert!(message.contains("reserved option `--port`"), "{message}");
    assert_eq!(
        body["error"]["details"]["path"], "engine_config.extra_args",
        "{body}"
    );
    assert!(
        !message.contains("9000"),
        "values are never echoed: {message}"
    );
}

/// SPEC §8.2: `inspect deployment --effective-config` exposes the resolved
/// configuration and its provenance with secrets redacted; it was unsupported
/// on the server role (found live 2026-09-23).
// T14
#[tokio::test]
async fn effective_configuration_is_served_with_provenance_and_redacted_secrets() {
    let (_directory, state, mut config, host) = fixture();
    config["engine_config"]["accept_extra_args"] = json!(true);
    config["engine_config"]["extra_args"] = json!(["--seed", "7"]);
    let router = app(state, host);
    let created = json_response(
        router
            .clone()
            .oneshot(request(
                "POST",
                "/management/v1/deployments",
                "effective",
                json!({"config":config,"activate":false}),
            ))
            .await
            .unwrap(),
    )
    .await;
    let id = created["deployment_id"].as_str().unwrap().to_owned();
    for target in [id.as_str(), "toy"] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!(
                        "/management/v1/deployments/{target}/effective-config"
                    ))
                    .header("authorization", format!("Bearer {MANAGEMENT}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{target}");
        let view = json_response(response).await;
        assert_eq!(view["deployment_id"], id.as_str(), "{view}");
        assert_eq!(view["revision"], 1, "{view}");
        let effective = &view["effective"];
        assert_eq!(effective["name"], "toy", "{view}");
        // Provenance: what the deployment declared and what mllm derived.
        assert!(
            effective["engine_config"]["provenance"].is_object(),
            "{view}"
        );
        assert_eq!(
            effective["engine_config"]["extra_args"],
            json!(["--seed", "7"]),
            "{view}"
        );
        // The credential reference is redacted, never shown.
        let text = view.to_string();
        assert!(!text.contains("secret://engine-key"), "{text}");
        assert!(text.contains("[redacted]"), "{text}");
        assert!(view["hosts"].is_array(), "{view}");
    }
    let unknown = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/management/v1/deployments/nope/effective-config")
                .header("authorization", format!("Bearer {MANAGEMENT}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404);
}

/// SPEC §8.2: secrets in engine arguments are redacted in the effective view,
/// whether written as `--option value` or `--option=value`.
// T14
#[test]
fn effective_view_redacts_secret_argument_values() {
    let mut value = json!({
        "engine_config": {"extra_args": ["--hf-token", "hf_a", "--api-key=k_b", "--seed", "7"]},
        "profile": {"args": ["--admin-password", "p_c"], "security": {"credential_ref": "secret://x"}},
    });
    mllm_management::configuration::redact_effective(&mut value);
    let text = value.to_string();
    for secret in ["hf_a", "k_b", "p_c", "secret://x"] {
        assert!(!text.contains(secret), "{text}");
    }
    assert_eq!(value["engine_config"]["extra_args"][3], "--seed");
    assert_eq!(value["engine_config"]["extra_args"][4], "7");
    assert_eq!(value["engine_config"]["extra_args"][0], "--hf-token");
    assert_eq!(
        value["engine_config"]["extra_args"][2],
        "--api-key=[redacted]"
    );
    // Ordinary settings whose names merely contain such a word stay visible.
    let mut ordinary = json!({"max_total_tokens": 4096, "tokenizer_workers": 2,
                              "extra_args": ["--max-num-batched-tokens", "8192"]});
    let before = ordinary.clone();
    mllm_management::configuration::redact_effective(&mut ordinary);
    assert_eq!(ordinary, before);
}

/// ADR 0008: a deployment declaring a remote source is refused on a host that
/// did not opt in (`model_source_denied` names why), accepted on one that did,
/// and its store key is listed as referenced for `mllm prune sources`.
// T14
#[tokio::test]
async fn remote_sources_need_host_opt_in_and_are_listed_as_referenced() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let (_directory, state, mut config, mut host) = fixture();
    let model = config["model"].as_object_mut().unwrap();
    model.remove("path");
    model.insert(
        "source".into(),
        json!({"huggingface": {"repo": "Qwen/Qwen3-4B", "revision": sha}}),
    );
    let refused = app(state.clone(), host.clone())
        .oneshot(request(
            "POST",
            "/management/v1/deployments",
            "denied",
            json!({"config":config,"activate":false}),
        ))
        .await
        .unwrap();
    assert!(refused.status().is_client_error(), "{}", refused.status());
    let body = json_response(refused).await;
    assert_eq!(body["error"]["details"]["path"], "model.source", "{body}");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("model source denied by host policy"),
        "{message}"
    );

    host["model_sources"] = json!({"huggingface": "allowed", "max_bytes": "100GiB"});
    let router = app(state, host);
    let created = router
        .clone()
        .oneshot(request(
            "POST",
            "/management/v1/deployments",
            "allowed",
            json!({"config":config,"activate":false}),
        ))
        .await
        .unwrap();
    assert_eq!(created.status(), 202);
    let listed = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/management/v1/model-sources")
                .header("authorization", format!("Bearer {MANAGEMENT}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(listed.status(), 200);
    let body = json_response(listed).await;
    assert_eq!(
        body["referenced"],
        json!([format!("sources/huggingface/Qwen--Qwen3-4B@{sha}")]),
        "{body}"
    );
}

/// ADR 0018 §7 (owner decision 2026-09-25): a deploy naming a runtime profile
/// no allowed host publishes is refused at once and nothing is stored; the
/// refusal names the profile, each host with what it publishes, and the fix.
// T03 T07
#[tokio::test]
async fn a_deploy_naming_an_unpublished_profile_fails_fast() {
    let (_directory, state, mut config, _) = fixture();
    publish_host(&state, "host-a", 'e', json!({}));
    publish_host(&state, "host-b", 'f', json!({}));
    let router = configuration_router(
        ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap(),
        Arc::new(SharedConfigurationSource::from_registry(state.clone(), "owner").unwrap()),
    );
    config["instances"] = json!(1);
    config["placement"] = json!({"hosts": ["host-a", "host-b"]});
    config["devices"] = json!([{"sharing": "shared"}]);
    for phase in ["cold", "ready", "parking", "wake"] {
        config["resources"][phase]["devices"] = json!([{"sharing": "shared"}]);
    }
    let before = state
        .lock()
        .unwrap()
        .store()
        .snapshot()
        .unwrap()
        .deployments
        .len();
    let mut missing = config.clone();
    missing["runtime_profile"] = json!("vllm-patched");
    let response = router
        .clone()
        .oneshot(request(
            "POST",
            "/management/v1/deployments",
            "missing",
            json!({"config":missing,"activate":false}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    let body = json_response(response).await;
    assert_eq!(body["error"]["code"], "profile_not_published", "{body}");
    let message = body["error"]["message"].as_str().unwrap();
    for needle in [
        "vllm-patched",
        "host-a",
        "host-b",
        "local",
        "mllm engine add",
        "--name vllm-patched",
    ] {
        assert!(message.contains(needle), "{needle}: {message}");
    }
    assert_eq!(body["error"]["details"]["profile"], "vllm-patched");
    assert_eq!(
        body["error"]["details"]["hosts"]["host-a"],
        json!(["local"]),
        "{body}"
    );
    assert_eq!(
        state
            .lock()
            .unwrap()
            .store()
            .snapshot()
            .unwrap()
            .deployments
            .len(),
        before,
        "nothing stored"
    );
    // The same deployment naming a published profile is accepted.
    let response = router
        .oneshot(request(
            "POST",
            "/management/v1/deployments",
            "present",
            json!({"config":config,"activate":false}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
}

/// ADR 0018 §7: the embedded (standalone) host fails fast the same way.
// T03 T07
#[tokio::test]
async fn an_embedded_deploy_naming_an_unpublished_profile_fails_fast() {
    let (_directory, state, mut config, host) = fixture();
    let router = app(state.clone(), host);
    config["runtime_profile"] = json!("sglang");
    let response = router
        .oneshot(request(
            "POST",
            "/management/v1/deployments",
            "embedded",
            json!({"config":config,"activate":false}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    let body = json_response(response).await;
    assert_eq!(body["error"]["code"], "profile_not_published", "{body}");
    assert!(state
        .lock()
        .unwrap()
        .store()
        .snapshot()
        .unwrap()
        .deployments
        .is_empty());
}

/// ADR 0018 §7: when another allowed host publishes the profile, the deploy
/// is accepted and a host that lacks it is recorded refused with the closed
/// diagnostic `profile_not_published`, never a candidate.
// T03 T07
#[tokio::test]
async fn a_host_lacking_the_profile_is_refused_while_another_has_it() {
    let (_directory, state, mut config, _) = fixture();
    let a = publish_host(&state, "host-a", 'e', json!({}));
    let b = publish_host(&state, "host-b", 'f', json!({}));
    {
        // host-b re-publishes with its only profile under another name.
        let owner = state.lock().unwrap();
        let mut document: Value = serde_json::from_str(
            &owner
                .store()
                .host_publication(&b)
                .unwrap()
                .unwrap()
                .config_json,
        )
        .unwrap();
        let profile = document["runtime_profiles"]["local"].take();
        document["runtime_profiles"] = json!({ "sglang": profile });
        owner
            .store()
            .publish_host_configuration(&mllm_store::host_publication::HostPublication {
                host_id: b.clone(),
                config_json: document.to_string(),
                boot_id: "boot-b".into(),
                fingerprint: mllm_config::remote_resources::policy_fingerprint(&document),
                received_at_ms: 2000,
            })
            .unwrap();
    }
    let router = configuration_router(
        ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap(),
        Arc::new(SharedConfigurationSource::from_registry(state.clone(), "owner").unwrap()),
    );
    config["instances"] = json!(1);
    config["placement"] = json!({"hosts": ["host-a", "host-b"]});
    config["devices"] = json!([{"sharing": "shared"}]);
    for phase in ["cold", "ready", "parking", "wake"] {
        config["resources"][phase]["devices"] = json!([{"sharing": "shared"}]);
    }
    let response = router
        .oneshot(request(
            "POST",
            "/management/v1/deployments",
            "partial",
            json!({"config":config,"activate":false}),
        ))
        .await
        .unwrap();
    let status = response.status();
    let body = json_response(response).await;
    assert_eq!(status, 202, "{body}");
    let id = body["deployment_id"].as_str().unwrap().to_owned();
    let snapshot = state.lock().unwrap().store().snapshot().unwrap();
    let deployment = snapshot.deployments.iter().find(|d| d.id == id).unwrap();
    let hosts: Vec<_> = deployment
        .hosts
        .iter()
        .map(|h| (h.host_id.clone(), h.outcome.clone(), h.diagnostic.clone()))
        .collect();
    assert!(hosts.contains(&(a, "resolved".into(), None)), "{hosts:?}");
    assert!(
        hosts.contains(&(
            "host-b".into(),
            "refused".into(),
            Some("profile_not_published".into())
        )),
        "{hosts:?}"
    );
}

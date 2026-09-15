use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use mllm_config::effective::{
    candidate::validate_candidate_reviewed_snapshot_text, resolve_effective,
};
use mllm_controller::OwnedCoordinatorState;
use mllm_management::{
    candidate_acceptance_router, configuration::SharedConfigurationSource, ManagementCredentials,
};
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tower::ServiceExt;

const MANAGEMENT: &str = "management-credential-012345678901234567890";
const INFERENCE: &str = "inference-credential-0123456789012345678901";
const PATH: &str = "/management/v1/qualification-runs";

fn fixture(
    engine: &str,
) -> (
    tempfile::TempDir,
    Arc<Mutex<OwnedCoordinatorState>>,
    Value,
    Value,
) {
    let directory = tempfile::Builder::new()
        .prefix("mllm-candidate-http-")
        .tempdir_in(std::env::var_os("HOME").unwrap())
        .unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let state = OwnedCoordinatorState::open(directory.path()).unwrap();
    let (golden, manifest) = match engine {
        "fake" => (
            include_str!("../../mllm-config/tests/fixtures/effective-fake-golden.json"),
            include_str!("../../mllm-config/tests/fixtures/candidate-fake.json"),
        ),
        "vllm" => (
            include_str!("../../mllm-config/tests/fixtures/effective-vllm-golden.json"),
            include_str!("../../mllm-config/tests/fixtures/candidate-vllm.json"),
        ),
        "sglang" => (
            include_str!("../../mllm-config/tests/fixtures/effective-sglang-golden.json"),
            include_str!("../../mllm-config/tests/fixtures/candidate-sglang.json"),
        ),
        _ => unreachable!(),
    };
    let golden: Value = serde_json::from_str(golden).unwrap();
    let mut host = golden["input"]["host"].clone();
    let mut manifest: Value = serde_json::from_str(manifest).unwrap();
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = socket.local_addr().unwrap().port();
    host["resource_policy"]["endpoint_port_range"] = json!({"start":port,"end":port});
    manifest["limits"]["max_run_duration_ms"] = json!(600_000);
    manifest["limits"]["max_cleanup_duration_ms"] = json!(60_000);
    let digest = validate_candidate_reviewed_snapshot_text(&manifest.to_string())
        .unwrap()
        .manifest_digest()
        .to_owned();
    host["qualification_policy"] = json!({"revision":1,"allow_qualification_runs":true,
        "allow_experimental_controls":true,"allowed_manifest_digests":[digest],"max_run_duration":"600s",
        "max_cleanup_duration":"60s","max_cases":128,"max_requests":4096,"max_request_body_bytes":"1MiB",
        "max_input_tokens_per_request":131072,"max_output_tokens_per_request":16384});
    let policy = resolve_effective(&golden["input"]["deployment"], &host)
        .unwrap()
        .host;
    let now = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let observations: Vec<_> = policy
        .domains
        .keys()
        .map(|domain| mllm_domain::resources::MemoryObservation {
            domain: domain.clone(),
            capacity_bytes: 64_i64 << 30,
            available_bytes: 64_i64 << 30,
            sampled_at_ms: now,
        })
        .collect();
    state
        .store()
        .import_resource_policy(state.session(), &policy, &observations, now)
        .unwrap();
    state
        .store()
        .import_qualification_policy(state.session(), &policy)
        .unwrap();
    let command = json!({"host_id":manifest["host"]["id"],"expected_host_revision":1,
        "recipe_digest":digest,"manifest":manifest,"deadline_ms":now+500_000,"allow_owned_abort_cleanup":true});
    (directory, Arc::new(Mutex::new(state)), host, command)
}

fn app(state: Arc<Mutex<OwnedCoordinatorState>>, host: Value) -> axum::Router {
    candidate_acceptance_router(
        ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap(),
        Arc::new(SharedConfigurationSource::new(state, host, "owner").unwrap()),
    )
}
fn request(key: &str, body: impl Into<Body>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(PATH)
        .header("authorization", format!("Bearer {MANAGEMENT}"))
        .header("content-type", "application/json")
        .header("idempotency-key", key)
        .body(body.into())
        .unwrap()
}
async fn response_json(response: axum::response::Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap()
}

#[tokio::test]
async fn candidate_acceptance_persists_scoped_run_without_launch_and_replays_original_receipt() {
    for engine in ["fake", "vllm", "sglang"] {
        let (_directory, state, host, command) = fixture(engine);
        let router = app(state.clone(), host);
        let response = router
            .clone()
            .oneshot(request("create", command.to_string()))
            .await
            .unwrap();
        assert_eq!(response.status(), 202);
        assert_eq!(response.headers()["cache-control"], "no-store");
        let accepted = response_json(response).await;
        assert_eq!(accepted["api_version"], "1");
        assert_eq!(accepted["revision"], "1");
        assert_eq!(accepted["joined"], false);
        let replay = router
            .oneshot(request("create", command.to_string()))
            .await
            .unwrap();
        assert_eq!(replay.status(), 202);
        assert_eq!(response_json(replay).await, accepted);
        let owner = state.lock().unwrap();
        let run_id = accepted["qualification_run_id"].as_str().unwrap();
        let run = owner
            .store()
            .candidate_run_snapshot("owner", run_id)
            .unwrap()
            .unwrap();
        assert_eq!(run.receipt().operation_id(), accepted["operation_id"]);
        assert_eq!(run.receipt().deployment_id(), accepted["deployment_id"]);
        assert_eq!(
            run.state(),
            mllm_store::candidate_creation::CandidateRunState::Accepted
        );
        assert_eq!(run.requests_used(), 0);
        assert_eq!(owner.store().deployment_count().unwrap(), 1);
        assert_eq!(owner.session().epoch(), 1);
        assert!(owner.store().resource_snapshot().unwrap().owners.is_empty());
        let deployment = owner
            .store()
            .get_deployment(run.receipt().deployment_id())
            .unwrap()
            .unwrap();
        assert_eq!(
            deployment.desired_state,
            mllm_domain::LifecycleState::Stopped
        );
        assert!(owner
            .store()
            .candidate_run_snapshot("other-principal", run_id)
            .unwrap()
            .is_none());
    }
}

#[tokio::test]
async fn candidate_creation_rejects_untrusted_authority_and_malformed_commands_without_writes() {
    let (_directory, state, host, command) = fixture("fake");
    let router = app(state.clone(), host);
    let mut wrong_token = request("denied", "not JSON");
    wrong_token.headers_mut().insert(
        "authorization",
        format!("Bearer {INFERENCE}").parse().unwrap(),
    );
    assert_eq!(
        router.clone().oneshot(wrong_token).await.unwrap().status(),
        401
    );
    let mut unknown = command.clone();
    unknown["evidence"] = json!({"qualified":true});
    let mut string_revision = command.clone();
    string_revision["expected_host_revision"] = json!("1");
    let mut overflow = command.clone();
    overflow["deadline_ms"] = json!(u64::MAX);
    let mut nested_unknown = command.clone();
    nested_unknown["manifest"]["evidence"] = json!("caller-owned");
    let mut duplicate = command.to_string();
    duplicate.insert_str(1, "\"expected_host_revision\":1,");
    let nested_duplicate = command.to_string().replacen(
        "\"limits\":{",
        "\"limits\":{\"max_run_duration_ms\":600000,",
        1,
    );
    for body in [
        unknown.to_string(),
        string_revision.to_string(),
        overflow.to_string(),
        nested_unknown.to_string(),
        duplicate,
        nested_duplicate,
    ] {
        let response = router
            .clone()
            .oneshot(request("invalid", body))
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        assert_eq!(
            response_json(response).await["error"]["code"],
            "invalid_request"
        );
    }
    let response = router
        .clone()
        .oneshot(request("large", "x".repeat((1 << 20) + 1)))
        .await
        .unwrap();
    assert_eq!(response.status(), 413);
    let mut ambiguous = request("invalid", command.to_string());
    ambiguous
        .headers_mut()
        .append("idempotency-key", "other".parse().unwrap());
    assert_eq!(
        router.clone().oneshot(ambiguous).await.unwrap().status(),
        400
    );
    let mut query = request("invalid", command.to_string());
    *query.uri_mut() = format!("{PATH}?execute=true").parse().unwrap();
    assert_eq!(router.oneshot(query).await.unwrap().status(), 400);
    assert_eq!(state.lock().unwrap().store().deployment_count().unwrap(), 0);
}

#[tokio::test]
async fn candidate_creation_enforces_host_policy_and_preserves_retries_after_revocation() {
    let (_directory, state, host, command) = fixture("fake");
    let router = app(state.clone(), host.clone());
    let mut stale = command.clone();
    stale["expected_host_revision"] = json!(2);
    let response = router
        .clone()
        .oneshot(request("stale", stale.to_string()))
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    assert_eq!(
        response_json(response).await["error"]["code"],
        "revision_conflict"
    );
    let mut wrong_digest = command.clone();
    wrong_digest["recipe_digest"] = json!("0".repeat(64));
    let response = router
        .clone()
        .oneshot(request("digest", wrong_digest.to_string()))
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(
        response_json(response).await["error"]["code"],
        "invalid_request"
    );
    let response = router
        .clone()
        .oneshot(request("create", command.to_string()))
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    let accepted = response_json(response).await;
    let mut conflict = command.clone();
    conflict["allow_owned_abort_cleanup"] = json!(false);
    let response = router
        .clone()
        .oneshot(request("create", conflict.to_string()))
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    assert_eq!(
        response_json(response).await["error"]["code"],
        "idempotency_conflict"
    );
    {
        let owner = state.lock().unwrap();
        let mut removed = host.clone();
        removed
            .as_object_mut()
            .unwrap()
            .remove("qualification_policy");
        let golden: Value = serde_json::from_str(include_str!(
            "../../mllm-config/tests/fixtures/effective-fake-golden.json"
        ))
        .unwrap();
        let policy = resolve_effective(&golden["input"]["deployment"], &removed)
            .unwrap()
            .host;
        owner
            .store()
            .import_qualification_policy(owner.session(), &policy)
            .unwrap();
    }
    let replay = router
        .clone()
        .oneshot(request("create", command.to_string()))
        .await
        .unwrap();
    assert_eq!(replay.status(), 202);
    assert_eq!(response_json(replay).await, accepted);
    let denied = router
        .oneshot(request("new", command.to_string()))
        .await
        .unwrap();
    assert_eq!(denied.status(), 403);
    assert_eq!(
        response_json(denied).await["error"]["code"],
        "host_policy_denied"
    );
    assert_eq!(state.lock().unwrap().store().deployment_count().unwrap(), 1);
}

#[tokio::test]
async fn unavailable_endpoint_and_stale_session_never_allocate_a_candidate() {
    let (_directory, state, host, command) = fixture("fake");
    let port = u16::try_from(
        host["resource_policy"]["endpoint_port_range"]["start"]
            .as_u64()
            .unwrap(),
    )
    .unwrap();
    let socket = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
    let router = app(state.clone(), host);
    let blocked = router
        .clone()
        .oneshot(request("blocked", command.to_string()))
        .await
        .unwrap();
    assert_eq!(blocked.status(), 503);
    assert_eq!(
        response_json(blocked).await["error"]["code"],
        "capacity_blocked"
    );
    assert_eq!(state.lock().unwrap().store().deployment_count().unwrap(), 0);
    drop(socket);
    // A deliberately newer session invalidates the source's original authority.
    state
        .lock()
        .unwrap()
        .store()
        .begin_coordinator_session()
        .unwrap();
    let stale = router
        .oneshot(request("stale", command.to_string()))
        .await
        .unwrap();
    assert_eq!(stale.status(), 503);
    assert_eq!(
        response_json(stale).await["error"]["code"],
        "reconciliation_required"
    );
    assert_eq!(state.lock().unwrap().store().deployment_count().unwrap(), 0);
}

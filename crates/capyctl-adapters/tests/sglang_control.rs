use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::IntoResponse,
    Router,
};
use capyctl_adapters::{
    sglang::{SglangAdapter, SglangRuntimeObservation, SglangRuntimeObserver},
    *,
};
use capyctl_domain::{
    completion::{
        ExecutionIdentities, Milestone, ProcessIdentity, StepExecutionContext, TransitionToken,
    },
    launch::{NativeLaunch, NativeLaunchMetadata},
};
use serde_json::{json, Value};
use tokio::sync::Notify;

const BINDING: &str = "01K00000000000000000000001";
const INCARNATION: &str = "01K00000000000000000000002";
const MODEL: &str = "toy";
const FLUSH_RESPONSE: &str = "Cache flushed.\nPlease check backend logs for more details. (When there are running or waiting requests, the operation will not be performed.)\n";

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn identities() -> Vec<ProcessIdentity> {
    vec![
        ProcessIdentity {
            role: "api".into(),
            pid: 21,
            boot_id: "boot".into(),
            start_ticks: 100,
        },
        ProcessIdentity {
            role: "worker-0".into(),
            pid: 22,
            boot_id: "boot".into(),
            start_ticks: 101,
        },
    ]
}

fn command(action: RuntimeAction, step: &str) -> RuntimeCommand {
    RuntimeCommand {
        action,
        context: StepExecutionContext {
            token: TransitionToken {
                deployment_id: "deployment".into(),
                revision: 1,
                generation: 1,
                operation_id: "operation".into(),
                step_id: step.into(),
            },
            binding_id: BINDING.into(),
            incarnation: INCARNATION.into(),
            issued_at_ms: now() - 10,
            deadline_ms: now() + 5000,
            identities: ExecutionIdentities::Retained(identities()),
            completion_target: None,
            grant_id: Some("grant".into()),
            launch_settings: None,
        },
    }
}

/// ADR 0010, discrete GPU design §5: the settings a `host_backed` deployment
/// renders (the memory saver with its weights CPU backup).
fn host_backed_settings() -> capyctl_domain::launch::SglangLaunchSettings {
    let mut settings = capyctl_testkit::sglang_launch_settings();
    settings.cpu_weight_backup = true;
    settings.weight_restore = "cpu_backup".into();
    settings
}

fn frozen_with(
    endpoint: String,
    settings: capyctl_domain::launch::SglangLaunchSettings,
) -> NativeLaunch {
    NativeLaunch::from_frozen_store(
        NativeLaunchMetadata {
            binding_id: BINDING.into(),
            incarnation: INCARNATION.into(),
            endpoint,
            served_name: MODEL.into(),
            engine: "sglang".into(),
            recipe: "sglang_engine_config_v2".into(),
            checkpoint_revision: "cdbee75f17c01a7cc42f958dc650907174af0554".into(),
            rendered_settings_digest: "a".repeat(64),
            placement_digest: None,
            device: capyctl_domain::launch::NativeDeviceSelection {
                host_id: "host-a".into(),
                hardware_fingerprint: "hardware-v1".into(),
                device_id: "gpu0".into(),
                memory_domain: "uma".into(),
                physical_gpu_uuid: None,
                cuda_pci_index: None,
            },
        },
        "/private/checkpoint".into(),
        "/opt/sglang/python".into(),
        "inference-ref".into(),
        "admin-ref".into(),
        settings,
    )
}

/// The snapshot, and whether the next observation is a step's precondition
/// after which the weights read as usable (the host observer's step content
/// for a reload: weights unproven before, usable after).
struct Observer(Mutex<SglangRuntimeObservation>, Mutex<bool>);
#[async_trait]
impl SglangRuntimeObserver for Observer {
    async fn observe(&self) -> Result<SglangRuntimeObservation, RuntimeError> {
        let mut snapshot = self.0.lock().unwrap();
        let observed = snapshot.clone();
        if std::mem::take(&mut *self.1.lock().unwrap()) {
            snapshot.weights = true;
        }
        Ok(observed)
    }
}

#[derive(Clone, Debug)]
struct Request {
    method: Method,
    path: String,
    authorization: String,
    body: Value,
}
struct Server {
    observer: Arc<Observer>,
    requests: Mutex<Vec<Request>>,
    reply: Mutex<(StatusCode, String)>,
    advance: Mutex<bool>,
    block: Mutex<bool>,
    replace: Mutex<bool>,
    stale: Mutex<bool>,
    entered: Notify,
    proceed: Notify,
}
async fn handle(
    State(server): State<Arc<Server>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    server.requests.lock().unwrap().push(Request {
        method,
        path: uri.to_string(),
        authorization: headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap()
            .into(),
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    });
    if *server.advance.lock().unwrap() {
        let mut snapshot = server.observer.0.lock().unwrap();
        match uri.path() {
            "/release_memory_occupation" => {
                snapshot.allocations = false;
                snapshot.weights = false;
                snapshot.cache = false;
            }
            "/resume_memory_occupation" => {
                snapshot.allocations = true;
                snapshot.quiesced = false;
            }
            "/update_weights_from_disk" => snapshot.weights = true,
            "/flush_cache" => snapshot.cache = true,
            _ => {}
        }
    }
    if *server.replace.lock().unwrap() {
        server.observer.0.lock().unwrap().identities[1].start_ticks += 1;
    }
    if *server.stale.lock().unwrap() {
        server.observer.0.lock().unwrap().token.generation += 1;
    }
    server.entered.notify_one();
    let block = *server.block.lock().unwrap();
    if block {
        server.proceed.notified().await;
    }
    let mut response = server.reply.lock().unwrap().clone().into_response();
    response
        .headers_mut()
        .insert("location", "/followed".parse().unwrap());
    response
}

struct Fixture {
    adapter: Arc<SglangAdapter>,
    server: Arc<Server>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Fixture {
    async fn new() -> Self {
        Self::with_settings(capyctl_testkit::sglang_launch_settings()).await
    }
    async fn with_settings(settings: capyctl_domain::launch::SglangLaunchSettings) -> Self {
        let observer = Arc::new(Observer(
            Mutex::new(SglangRuntimeObservation {
                token: command(RuntimeAction::Park, "park").context.token,
                binding_id: BINDING.into(),
                incarnation: INCARNATION.into(),
                identities: identities(),
                real_memory_saver: true,
                quiesced: true,
                unknown_work: false,
                allocations: true,
                weights: true,
                cache: true,
            }),
            Mutex::new(false),
        ));
        let server = Arc::new(Server {
            observer: observer.clone(),
            requests: Mutex::new(vec![]),
            reply: Mutex::new((StatusCode::OK, "null".into())),
            advance: Mutex::new(true),
            block: Mutex::new(false),
            replace: Mutex::new(false),
            stale: Mutex::new(false),
            entered: Notify::new(),
            proceed: Notify::new(),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        // The control tests exercise persisted controls; readiness is polled
        // out of band, so the stub answers an always-empty model list without
        // recording it, matching the adapter's separate readiness surface.
        let app = Router::new()
            .route(
                "/v1/models",
                axum::routing::get(|| async {
                    axum::Json(json!({"object":"list","data":[]})).into_response()
                }),
            )
            .fallback(handle)
            .with_state(server.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let adapter = Arc::new(
            SglangAdapter::from_frozen(&frozen_with(endpoint, settings), Some(observer))
                .unwrap()
                .with_credentials("inference-secret".into(), "admin-secret".into()),
        );
        Self {
            adapter,
            server,
            task,
        }
    }
    fn next(&self, action: RuntimeAction, step: &str) -> RuntimeCommand {
        let command = command(action, step);
        self.server.observer.0.lock().unwrap().token = command.context.token.clone();
        command
    }
    fn parked(&self) {
        let mut snapshot = self.server.observer.0.lock().unwrap();
        snapshot.allocations = false;
        snapshot.weights = false;
        snapshot.cache = false;
    }
    fn reply(&self, status: StatusCode, body: &str) {
        *self.server.reply.lock().unwrap() = (status, body.into());
    }
    fn requests(&self) -> Vec<Request> {
        self.server.requests.lock().unwrap().clone()
    }
}
fn uncertain(result: Result<capyctl_domain::completion::EffectObservation, RuntimeError>) {
    assert!(
        matches!(result, Err(RuntimeError::Uncertain(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn release_sends_exact_tags_once_with_admin_key_and_rejects_duplicate() {
    let f = Fixture::new().await;
    let c = f.next(RuntimeAction::Park, "park");
    let result = f.adapter.execute_persisted(&c).await.unwrap();
    assert_eq!(result.facts, vec![Milestone::MemoryReleased]);
    assert_eq!(result.token, c.context.token);
    assert_eq!(result.identities, identities());
    uncertain(f.adapter.execute_persisted(&c).await);
    let requests = f.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, Method::POST);
    assert_eq!(requests[0].path, "/release_memory_occupation");
    assert_eq!(requests[0].body, json!({"tags":["kv_cache","weights"]}));
    assert_eq!(requests[0].authorization, "Bearer admin-secret");
}

// T16
#[tokio::test]
async fn restore_is_separate_from_reload_flush_and_accounted_probe() {
    let f = Fixture::new().await;
    f.parked();
    for (action, step, reply, fact) in [
        (RuntimeAction::Restore, "resume", "null".to_owned(), Milestone::AllocationsRestored),
        (RuntimeAction::ReloadWeights, "reload", "{\"success\":true,\"message\":\"Success\",\"num_paused_requests\":0}".into(), Milestone::WeightsUsable),
        (RuntimeAction::InvalidateCache, "flush", FLUSH_RESPONSE.into(), Milestone::CacheValid),
        (RuntimeAction::Probe, "probe", json!({"model":MODEL,"choices":[{"index":0,"message":{"role":"assistant","content":"OK"},"finish_reason":"stop"}]}).to_string(), Milestone::ModelUsable),
    ] {
        f.reply(StatusCode::OK, &reply);
        let result = f.adapter.execute_persisted(&f.next(action, step)).await.unwrap();
        assert_eq!(result.facts, vec![fact]);
        assert_eq!(f.adapter.check_readiness(&MemberRef { deployment_id: "deployment".into(), member_id: "member".into() }).await.unwrap(), Readiness::Initializing);
    }
    let requests = f.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[0].path, "/resume_memory_occupation");
    assert_eq!(requests[0].body, json!({"tags":["kv_cache","weights"]}));
    assert_eq!(requests[1].path, "/update_weights_from_disk");
    assert_eq!(
        requests[1].body,
        json!({"model_path":"/private/checkpoint","load_format":"auto","abort_all_requests":false,"is_async":false,"keep_pause":false,"recapture_cuda_graph":false,"flush_cache":true})
    );
    assert_eq!(requests[2].path, "/flush_cache?timeout=0");
    assert_eq!(requests[3].path, "/v1/chat/completions");
    assert_eq!(requests[3].authorization, "Bearer inference-secret");
    assert_eq!(
        requests[3].body,
        json!({"model":MODEL,"messages":[{"role":"user","content":"Reply with exactly the two letters OK and nothing else. Do not add punctuation."}],"temperature":0,"max_tokens":8,"stream":false})
    );
    assert!(requests[..3]
        .iter()
        .all(|r| r.authorization == "Bearer admin-secret"));
}

/// Discrete GPU design §5, ADR 0019: a `host_backed` launch parks to host RAM
/// with SGLang's weights CPU backup. Release and resume carry both tags as for
/// `deep`; the resume restores the weights from the pinned copy, so the reload
/// step makes no `update_weights_from_disk` call and reports `WeightsUsable`
/// from its own before and after saver observations. Flush and the fresh probe
/// follow. Fake-engine tests are not qualification of a native SGLang recipe.
// T16 T20 T22
#[tokio::test]
async fn host_backed_restores_from_host_ram_without_a_disk_reload() {
    let f = Fixture::with_settings(host_backed_settings()).await;
    for (action, step, reply, fact) in [
        (RuntimeAction::Park, "park", "null".to_owned(), Milestone::MemoryReleased),
        (RuntimeAction::Restore, "resume", "null".to_owned(), Milestone::AllocationsRestored),
        (RuntimeAction::ReloadWeights, "reload", "null".to_owned(), Milestone::WeightsUsable),
        (RuntimeAction::InvalidateCache, "flush", FLUSH_RESPONSE.into(), Milestone::CacheValid),
        (RuntimeAction::Probe, "probe", json!({"model":MODEL,"choices":[{"index":0,"message":{"role":"assistant","content":"OK"},"finish_reason":"stop"}]}).to_string(), Milestone::ModelUsable),
    ] {
        f.reply(StatusCode::OK, &reply);
        if action == RuntimeAction::ReloadWeights {
            // No engine call moves the fake: the observer's step content does.
            *f.server.observer.1.lock().unwrap() = true;
        }
        let result = f.adapter.execute_persisted(&f.next(action, step)).await;
        assert_eq!(result.unwrap().facts, vec![fact], "{action:?}");
    }
    let paths: Vec<_> = f.requests().into_iter().map(|r| r.path).collect();
    assert_eq!(
        paths,
        [
            "/release_memory_occupation",
            "/resume_memory_occupation",
            "/flush_cache?timeout=0",
            "/v1/chat/completions"
        ]
    );
}

/// The reload step under `host_backed` still needs its evidence: with no
/// allocations observed it is uncertain and sends nothing.
// T20 T22
#[tokio::test]
async fn host_backed_reload_without_restored_allocations_is_uncertain() {
    let f = Fixture::with_settings(host_backed_settings()).await;
    f.parked();
    uncertain(
        f.adapter
            .execute_persisted(&f.next(RuntimeAction::ReloadWeights, "reload"))
            .await,
    );
    assert!(f.requests().is_empty());
}

#[tokio::test]
async fn reload_false_http_failure_and_malformed_bodies_stay_uncertain() {
    for (status, body) in [
        (StatusCode::OK, "{\"success\":false}"),
        (StatusCode::INTERNAL_SERVER_ERROR, "{\"success\":true}"),
        (StatusCode::OK, "invalid"),
        (StatusCode::OK, "null"),
        (StatusCode::OK, "{\"success\":false,\"success\":true}"),
        (
            StatusCode::OK,
            "{\"success\":true,\"error\":\"/private/checkpoint admin-secret\"}",
        ),
    ] {
        let f = Fixture::new().await;
        f.server.observer.0.lock().unwrap().weights = false;
        f.reply(status, body);
        let c = f.next(RuntimeAction::ReloadWeights, "reload");
        let result = f.adapter.execute_persisted(&c).await;
        let diagnostic = format!("{result:?}");
        assert!(!diagnostic.contains("/private/checkpoint"));
        assert!(!diagnostic.contains("admin-secret"));
        uncertain(result);
        assert_eq!(f.requests().len(), 1);
    }
}

#[tokio::test]
async fn null_and_empty_controls_succeed_but_error_objects_and_large_bodies_fail() {
    for body in ["null", "", "{}"] {
        let f = Fixture::new().await;
        f.reply(StatusCode::OK, body);
        assert_eq!(
            f.adapter
                .execute_persisted(&f.next(RuntimeAction::Park, "park"))
                .await
                .unwrap()
                .facts,
            vec![Milestone::MemoryReleased]
        );
    }
    for body in [
        "{\"error\":\"secret\"}".to_owned(),
        "true".into(),
        format!("{}null", " ".repeat(65_536)),
    ] {
        let f = Fixture::new().await;
        f.reply(StatusCode::OK, &body);
        uncertain(
            f.adapter
                .execute_persisted(&f.next(RuntimeAction::Park, "park"))
                .await,
        );
        assert_eq!(f.requests().len(), 1);
    }
}

#[tokio::test]
async fn every_control_obeys_operation_deadline_and_lost_reply_never_resends() {
    for action in [
        RuntimeAction::Park,
        RuntimeAction::Restore,
        RuntimeAction::ReloadWeights,
        RuntimeAction::InvalidateCache,
        RuntimeAction::Probe,
    ] {
        let f = Fixture::new().await;
        if action == RuntimeAction::Restore {
            f.parked();
        }
        if action == RuntimeAction::ReloadWeights {
            f.server.observer.0.lock().unwrap().weights = false;
        }
        *f.server.block.lock().unwrap() = true;
        let mut c = f.next(action, "lost");
        c.context.deadline_ms = now() + 250;
        uncertain(f.adapter.execute_persisted(&c).await);
        uncertain(f.adapter.execute_persisted(&c).await);
        assert_eq!(f.requests().len(), 1);
    }
}

#[tokio::test]
async fn caller_cancellation_retains_attempt_fence() {
    let f = Fixture::new().await;
    *f.server.block.lock().unwrap() = true;
    let c = f.next(RuntimeAction::Park, "cancelled");
    let command = c.clone();
    let adapter = f.adapter.clone();
    let work = tokio::spawn(async move { adapter.execute_persisted(&command).await });
    f.server.entered.notified().await;
    work.abort();
    let _ = work.await;
    uncertain(f.adapter.execute_persisted(&c).await);
    assert_eq!(f.requests().len(), 1);
}

#[tokio::test]
async fn reload_failure_after_resume_never_implies_ready() {
    let f = Fixture::new().await;
    f.parked();
    assert_eq!(
        f.adapter
            .execute_persisted(&f.next(RuntimeAction::Restore, "resume"))
            .await
            .unwrap()
            .facts,
        vec![Milestone::AllocationsRestored]
    );
    f.reply(
        StatusCode::INTERNAL_SERVER_ERROR,
        "{\"error\":\"reload failed\"}",
    );
    uncertain(
        f.adapter
            .execute_persisted(&f.next(RuntimeAction::ReloadWeights, "reload"))
            .await,
    );
    assert_eq!(
        f.adapter
            .check_readiness(&MemberRef {
                deployment_id: "deployment".into(),
                member_id: "member".into()
            })
            .await
            .unwrap(),
        Readiness::Initializing
    );
    assert_eq!(f.requests().len(), 2);
}

#[tokio::test]
async fn flush_failure_and_duplicate_resume_are_rejected() {
    let f = Fixture::new().await;
    f.parked();
    f.adapter
        .execute_persisted(&f.next(RuntimeAction::Restore, "resume"))
        .await
        .unwrap();
    uncertain(
        f.adapter
            .execute_persisted(&f.next(RuntimeAction::Restore, "resume-again"))
            .await,
    );
    assert_eq!(f.requests().len(), 1);
    let f = Fixture::new().await;
    f.reply(StatusCode::INTERNAL_SERVER_ERROR, "null");
    uncertain(
        f.adapter
            .execute_persisted(&f.next(RuntimeAction::InvalidateCache, "flush"))
            .await,
    );
    assert_eq!(f.requests().len(), 1);
}

#[tokio::test]
async fn flush_requires_pinned_plain_text_acknowledgement() {
    for body in [
        "null",
        "{}",
        "{\"error\":\"flush failed\"}",
        "Cache flushed.",
    ] {
        let f = Fixture::new().await;
        f.reply(StatusCode::OK, body);
        uncertain(
            f.adapter
                .execute_persisted(&f.next(RuntimeAction::InvalidateCache, "flush"))
                .await,
        );
    }
}

#[tokio::test]
async fn worker_replacement_and_stale_completion_are_uncertain() {
    for replace in [true, false] {
        let f = Fixture::new().await;
        if replace {
            *f.server.replace.lock().unwrap() = true;
        } else {
            *f.server.stale.lock().unwrap() = true;
        }
        uncertain(
            f.adapter
                .execute_persisted(&f.next(RuntimeAction::Park, "park"))
                .await,
        );
        assert_eq!(f.requests().len(), 1);
    }
}

#[tokio::test]
async fn unproven_saver_identity_quiescence_or_grant_prevents_send() {
    for case in 0..6 {
        let f = Fixture::new().await;
        let mut c = f.next(RuntimeAction::Park, "park");
        {
            let mut snapshot = f.server.observer.0.lock().unwrap();
            match case {
                0 => snapshot.real_memory_saver = false,
                1 => snapshot.identities[1].start_ticks += 1,
                2 => snapshot.quiesced = false,
                3 => snapshot.unknown_work = true,
                4 => c.context.grant_id = None,
                _ => snapshot.token.generation += 1,
            }
        }
        uncertain(f.adapter.execute_persisted(&c).await);
        assert!(f.requests().is_empty());
    }
}

#[tokio::test]
async fn failed_model_probe_and_unobserved_effect_do_not_emit_milestones() {
    let f = Fixture::new().await;
    f.reply(StatusCode::OK, &json!({"model":"other-model","choices":[{"index":0,"message":{"role":"assistant","content":"OK"},"finish_reason":"stop"}]}).to_string());
    uncertain(
        f.adapter
            .execute_persisted(&f.next(RuntimeAction::Probe, "probe"))
            .await,
    );
    let f = Fixture::new().await;
    *f.server.advance.lock().unwrap() = false;
    uncertain(
        f.adapter
            .execute_persisted(&f.next(RuntimeAction::Park, "park"))
            .await,
    );
}

#[tokio::test]
async fn qualified_drain_observation_emits_only_quiesced_without_http() {
    let f = Fixture::new().await;
    let effect = f
        .adapter
        .execute_persisted(&f.next(RuntimeAction::Drain, "drain"))
        .await
        .unwrap();
    assert_eq!(effect.facts, vec![Milestone::Quiesced]);
    assert!(f.requests().is_empty());
}

#[tokio::test]
async fn legacy_compound_controls_never_bypass_persisted_substeps() {
    let f = Fixture::new().await;
    let member = MemberRef {
        deployment_id: "deployment".into(),
        member_id: "member".into(),
    };
    for level in [ParkLevel::One, ParkLevel::Two] {
        assert_eq!(
            f.adapter.park(&member, level).await,
            Err(AdapterError::UnsupportedCapability)
        );
    }
    assert_eq!(
        f.adapter.restore(&member).await,
        Err(AdapterError::UnsupportedCapability)
    );
    assert_eq!(
        f.adapter.reload_weights(&member).await,
        Err(AdapterError::UnsupportedCapability)
    );
    assert!(f.requests().is_empty());
}

#[tokio::test]
async fn unknown_metrics_never_infer_idle_or_qualified_drain() {
    let f = Fixture::new().await;
    f.server.observer.0.lock().unwrap().unknown_work = true;
    let member = MemberRef {
        deployment_id: "deployment".into(),
        member_id: "member".into(),
    };
    assert_eq!(
        f.adapter.observe_work(&member).await.unwrap(),
        WorkObservation::Unknown
    );
    assert!(!f.adapter.prepare_park(&member).await.unwrap().quiescent);
    uncertain(
        f.adapter
            .execute_persisted(&f.next(RuntimeAction::Drain, "drain"))
            .await,
    );
    assert!(f.requests().is_empty());
}

#[tokio::test]
async fn expired_command_and_redirect_never_repeat_or_follow_control() {
    let f = Fixture::new().await;
    let mut c = f.next(RuntimeAction::Park, "expired");
    c.context.deadline_ms = now() - 1;
    uncertain(f.adapter.execute_persisted(&c).await);
    assert!(f.requests().is_empty());
    f.reply(StatusCode::TEMPORARY_REDIRECT, "null");
    uncertain(
        f.adapter
            .execute_persisted(&f.next(RuntimeAction::Park, "redirect"))
            .await,
    );
    assert_eq!(f.requests().len(), 1);
}

#[tokio::test]
async fn pending_control_cannot_report_ready_or_accept_concurrent_work() {
    let f = Fixture::new().await;
    *f.server.block.lock().unwrap() = true;
    let c = f.next(RuntimeAction::Park, "park");
    let adapter = f.adapter.clone();
    let work = tokio::spawn(async move { adapter.execute_persisted(&c).await });
    tokio::time::timeout(Duration::from_secs(2), f.server.entered.notified())
        .await
        .unwrap();
    let member = MemberRef {
        deployment_id: "deployment".into(),
        member_id: "member".into(),
    };
    assert_eq!(
        f.adapter.check_readiness(&member).await.unwrap(),
        Readiness::Initializing
    );
    uncertain(
        f.adapter
            .execute_persisted(&command(RuntimeAction::Restore, "concurrent"))
            .await,
    );
    f.server.proceed.notify_one();
    work.await.unwrap().unwrap();
    assert_eq!(f.requests().len(), 1);
}

/// A reasoning model's trace stays in `content` when the engine runs without a
/// reasoning parser. The runtime is healthy, so the failure must name the
/// misconfiguration instead of presenting as indistinguishable uncertainty.
/// Measured live on host-a with Qwen3.5-27B on 2026-09-16.
#[tokio::test]
async fn a_leaked_reasoning_trace_is_reported_as_a_parser_misconfiguration() {
    for content in [
        "We need answer exactly OK. Final only OK.\n</think>\n\nOK",
        "thinking</reasoning>OK",
        "aside<|end_thinking|>OK",
    ] {
        let f = Fixture::new().await;
        f.reply(
            StatusCode::OK,
            &json!({"model":MODEL,"choices":[{"index":0,"message":{"role":"assistant","content":content},"finish_reason":"stop"}]}).to_string(),
        );
        let error = f
            .adapter
            .execute_persisted(&f.next(RuntimeAction::Probe, "probe"))
            .await
            .expect_err("a leaked trace is not a usable model");
        match error {
            RuntimeError::Uncertain(message) => assert!(
                message.contains("reasoning parser"),
                "expected the parser misconfiguration to be named, got: {message}"
            ),
            other => panic!("expected uncertainty, got {other:?}"),
        }
    }
}

/// The marker check itself must stay exact. Resumed-but-not-reloaded weights
/// produce plausible output, and relaxing the comparison would let that pass. The
/// live reproduction of that failure emitted `!!!!!!!!`.
#[tokio::test]
async fn plausible_but_wrong_probe_output_is_still_rejected() {
    for content in ["!!!!!!!!", "OK.", "Sure, OK", "ok"] {
        let f = Fixture::new().await;
        f.reply(
            StatusCode::OK,
            &json!({"model":MODEL,"choices":[{"index":0,"message":{"role":"assistant","content":content},"finish_reason":"stop"}]}).to_string(),
        );
        let error = f
            .adapter
            .execute_persisted(&f.next(RuntimeAction::Probe, "probe"))
            .await
            .expect_err("only the exact marker establishes model usability");
        match error {
            RuntimeError::Uncertain(message) => assert!(
                !message.contains("reasoning parser"),
                "a wrong answer is not a parser problem: {message}"
            ),
            other => panic!("expected uncertainty, got {other:?}"),
        }
    }
}

/// A trace terminator in a reply from another model is ordinary uncertainty: the
/// reply is not evidence about this runtime at all.
#[tokio::test]
async fn a_foreign_reply_with_a_trace_is_not_a_parser_diagnosis() {
    let f = Fixture::new().await;
    f.reply(
        StatusCode::OK,
        &json!({"model":"other-model","choices":[{"index":0,"message":{"role":"assistant","content":"x</think>OK"},"finish_reason":"stop"}]}).to_string(),
    );
    let error = f
        .adapter
        .execute_persisted(&f.next(RuntimeAction::Probe, "probe"))
        .await
        .expect_err("a foreign reply proves nothing");
    match error {
        RuntimeError::Uncertain(message) => assert!(!message.contains("reasoning parser")),
        other => panic!("expected uncertainty, got {other:?}"),
    }
}

/// With a reasoning parser configured the trace is separated correctly, but the
/// model can still spend the whole probe budget thinking. The runtime is healthy and
/// the probe is undersized, which is a different repair from a missing parser.
/// Measured live: Qwen3.5-27B with `--reasoning-parser qwen3` returned empty content
/// at eight tokens and the exact marker at 512.
#[tokio::test]
async fn a_budget_consumed_by_reasoning_is_reported_separately() {
    let f = Fixture::new().await;
    f.reply(
        StatusCode::OK,
        &json!({"model":MODEL,"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"We need answer exactly OK. User says"},"finish_reason":"length"}]}).to_string(),
    );
    let error = f
        .adapter
        .execute_persisted(&f.next(RuntimeAction::Probe, "probe"))
        .await
        .expect_err("an unanswered probe is not model usability");
    match error {
        RuntimeError::Uncertain(message) => {
            assert!(
                message.contains("probe budget"),
                "expected the budget to be named, got: {message}"
            );
            assert!(
                !message.contains("reasoning parser"),
                "the parser is configured; naming it would send the wrong repair"
            );
        }
        other => panic!("expected uncertainty, got {other:?}"),
    }
}

/// An empty answer without a separated trace is ordinary failure, not a budget
/// problem: nothing shows the model was thinking rather than broken.
#[tokio::test]
async fn an_empty_answer_without_a_trace_is_ordinary_uncertainty() {
    for reply in [
        json!({"model":MODEL,"choices":[{"index":0,"message":{"role":"assistant","content":""},"finish_reason":"length"}]}),
        json!({"model":MODEL,"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"  "},"finish_reason":"length"}]}),
    ] {
        let f = Fixture::new().await;
        f.reply(StatusCode::OK, &reply.to_string());
        let error = f
            .adapter
            .execute_persisted(&f.next(RuntimeAction::Probe, "probe"))
            .await
            .expect_err("an empty answer is never usability");
        match error {
            RuntimeError::Uncertain(message) => assert!(
                !message.contains("probe budget") && !message.contains("reasoning parser"),
                "no reasoning evidence, so no reasoning diagnosis: {message}"
            ),
            other => panic!("expected uncertainty, got {other:?}"),
        }
    }
}

/// A separated trace with a correct answer is a healthy probe and must still pass.
#[tokio::test]
async fn a_separated_trace_with_the_exact_marker_still_passes() {
    let f = Fixture::new().await;
    f.reply(
        StatusCode::OK,
        &json!({"model":MODEL,"choices":[{"index":0,"message":{"role":"assistant","content":"\n\nOK","reasoning_content":"Final only OK."},"finish_reason":"stop"}]}).to_string(),
    );
    f.adapter
        .execute_persisted(&f.next(RuntimeAction::Probe, "probe"))
        .await
        .expect("a separated trace with the exact marker is usable");
}

/// A loopback SGLang `/metrics` that answers `body` to the inference key only.
async fn sglang_metrics(body: &'static str) -> SglangAdapter {
    let app = Router::new().route(
        "/metrics",
        axum::routing::get(move |headers: HeaderMap| async move {
            if headers.get("authorization").and_then(|v| v.to_str().ok())
                != Some("Bearer inference-secret")
            {
                return StatusCode::UNAUTHORIZED.into_response();
            }
            body.into_response()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    SglangAdapter::from_frozen(
        &frozen_with(endpoint, capyctl_testkit::sglang_launch_settings()),
        None,
    )
    .unwrap()
    .with_credentials("inference-secret".into(), "admin-secret".into())
}

// T17 T22, SPEC §10 (amended 2026-10-01): a cancelled request is acknowledged
// only by `sglang:num_running_reqs` and `sglang:num_queue_reqs` both at zero.
#[tokio::test]
async fn engine_quiescence_reads_both_sglang_gauges() {
    let member = MemberRef {
        deployment_id: "deployment".into(),
        member_id: "member".into(),
    };
    let queued = sglang_metrics("sglang:num_running_reqs 0\nsglang:num_queue_reqs 2\n").await;
    assert!(!queued.engine_quiescent(&member, now()).await);
    let idle = sglang_metrics("sglang:num_running_reqs 0\nsglang:num_queue_reqs 0\n").await;
    assert!(idle.engine_quiescent(&member, now()).await);
    let unkeyed = SglangAdapter::from_frozen(
        &frozen_with(
            "http://127.0.0.1:1".into(),
            capyctl_testkit::sglang_launch_settings(),
        ),
        None,
    )
    .unwrap();
    assert!(!unkeyed.engine_quiescent(&member, 0).await);
}

//! W5: the embedded host's SGLang observer, driving the real SGLang adapter.
//!
//! A local axum stand-in answers SGLang's control surface, its `/metrics`
//! gauges and the chat probe; a fake saver maps bytes as the stand releases and
//! resumes them. The adapter runs exactly the steps the embedded coordinator
//! runs (quiescence check, Park, then Restore, ReloadWeights, InvalidateCache
//! and Probe). The recorded processes are this test process and a child it
//! spawns, so their liveness is read from `/proc` as in production. CPU and
//! fake-engine only; never qualification (AGENTS.md).
use super::*;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::IntoResponse,
};
use capyctl_adapters::{sglang::SglangAdapter, traits::EngineAdapter};
use capyctl_domain::{
    completion::{Milestone, StepExecutionContext, TransitionToken},
    launch::{
        CommonEngineSettings, MemoryRequest, NativeLaunch, NativeLaunchMetadata,
        SglangLaunchSettings,
    },
};
use std::sync::Mutex;

const BINDING: &str = "01K00000000000000000000001";
const INCARNATION: &str = "01K00000000000000000000002";
const FLUSH: &str = "Cache flushed.\nPlease check backend logs for more details. (When there are running or waiting requests, the operation will not be performed.)\n";

#[derive(Default)]
struct Engine {
    calls: Vec<String>,
    released: bool,
    /// Resume only the weights: a partial restoration.
    partial_resume: bool,
    weights_only: bool,
    busy: bool,
    /// The launch enrolled no observation.
    unenrolled: bool,
}
type Shared = Arc<Mutex<Engine>>;

async fn serve(
    State(engine): State<Shared>,
    uri: Uri,
    headers: HeaderMap,
) -> axum::response::Response {
    let bearer = headers.get("authorization").and_then(|v| v.to_str().ok());
    let mut e = engine.lock().unwrap();
    match uri.path() {
        "/metrics" if bearer == Some("Bearer inference-key") => {
            let n = u8::from(e.busy);
            format!("sglang:num_running_reqs{{model_name=\"toy\"}} {n}.0\nsglang:num_queue_reqs{{model_name=\"toy\"}} 0.0\n")
                .into_response()
        }
        "/v1/chat/completions" if bearer == Some("Bearer inference-key") => {
            e.calls.push(uri.path().into());
            axum::Json(serde_json::json!({"model": "toy", "choices": [{"index": 0,
                "message": {"role": "assistant", "content": "OK"}, "finish_reason": "stop"}]}))
            .into_response()
        }
        _ if bearer != Some("Bearer admin-key") => StatusCode::UNAUTHORIZED.into_response(),
        path => {
            e.calls.push(path.into());
            match path {
                "/release_memory_occupation" => {
                    e.released = true;
                    "null".into_response()
                }
                "/resume_memory_occupation" => {
                    e.released = false;
                    e.weights_only = e.partial_resume;
                    "null".into_response()
                }
                "/update_weights_from_disk" => {
                    axum::Json(serde_json::json!({"success": true})).into_response()
                }
                "/flush_cache" => FLUSH.into_response(),
                _ => StatusCode::NOT_FOUND.into_response(),
            }
        }
    }
}

struct Saver(Shared);
impl SaverResidency for Saver {
    fn mapped(&self, scope: &SaverScope) -> Result<SaverMapped, SaverUnavailable> {
        let e = self.0.lock().unwrap();
        if e.unenrolled
            || (scope.binding_id.as_str(), scope.incarnation.as_str()) != (BINDING, INCARNATION)
            || scope.admin_key != "admin-key"
        {
            return Err(SaverUnavailable);
        }
        let weights = if e.released { 0 } else { 1 << 20 };
        let kv = if e.released || e.weights_only {
            0
        } else {
            1 << 20
        };
        Ok(SaverMapped {
            real_saver: true,
            weight_bytes: weights,
            kv_bytes: kv,
            weight_virtual_bytes: 1 << 20,
            kv_virtual_bytes: 1 << 20,
        })
    }
}

struct Stand {
    engine: Shared,
    endpoint: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Stand {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn stand() -> Stand {
    let engine = Shared::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new()
        .fallback(serve)
        .with_state(engine.clone());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Stand {
        engine,
        endpoint,
        task,
    }
}

fn frozen(endpoint: String) -> NativeLaunch {
    NativeLaunch::from_frozen_store(
        NativeLaunchMetadata {
            binding_id: BINDING.into(),
            incarnation: INCARNATION.into(),
            endpoint,
            served_name: "toy".into(),
            engine: "sglang".into(),
            recipe: "sglang_engine_config_v2".into(),
            checkpoint_revision: "cdbee75f17c01a7cc42f958dc650907174af0554".into(),
            rendered_settings_digest: "a".repeat(64),
            placement_digest: None,
            device: capyctl_domain::launch::NativeDeviceSelection {
                host_id: "host".into(),
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
        SglangLaunchSettings {
            common: CommonEngineSettings {
                cuda_graphs: Some(false),
                ..CommonEngineSettings::default()
            },
            memory: MemoryRequest {
                request_bytes: 16 << 30,
                kv_cache_bytes: 4_294_967_296,
                margin_bytes: 8 << 30,
                weights_bytes: None,
                startup_bytes: None,
                device_total_bytes: None,
                overhead_bytes: None,
                startup_graphs_bytes: None,
                state_slot_bytes: None,
                state_bytes: None,
            },
            max_total_tokens: None,
            max_mamba_cache_size: None,
            static_allowance_bytes: None,
            chunked_prefill_size: None,
            tokenizer_workers: 1,
            tool_call_parser: None,
            reasoning_parser: None,
            memory_saver: true,
            cpu_weight_backup: false,
            weight_restore: "disk_reload".into(),
            extra_args: Vec::new(),
            provenance: Default::default(),
        },
    )
}

/// This process's own identity from `/proc`, for a recorded process that is
/// really alive.
fn identity(pid: u32, role: &str) -> ProcessIdentity {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let fields: Vec<&str> = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .collect();
    ProcessIdentity {
        role: role.into(),
        pid,
        start_ticks: fields[19].parse().unwrap(),
        boot_id: std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .unwrap()
            .trim()
            .into(),
    }
}

struct Group {
    child: std::process::Child,
    identities: Vec<ProcessIdentity>,
}
impl Drop for Group {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn group() -> Group {
    let child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let identities = vec![
        identity(std::process::id(), "api"),
        identity(child.id(), "worker-0"),
    ];
    Group { child, identities }
}

fn command(action: RuntimeAction, step: &str, identities: &[ProcessIdentity]) -> RuntimeCommand {
    let now = capyctl_protocol::now_unix_ms();
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
            issued_at_ms: now - 1,
            deadline_ms: now + 30_000,
            identities: ExecutionIdentities::Retained(identities.to_vec()),
            completion_target: None,
            grant_id: Some("grant".into()),
            launch_settings: None,
        },
    }
}

fn adapter(stand: &Stand) -> SglangAdapter {
    let observer: Arc<dyn SglangRuntimeObserver> = Arc::new(LaunchSglangObserver::new(Arc::new(
        Saver(stand.engine.clone()),
    )));
    SglangAdapter::from_frozen(&frozen(stand.endpoint.clone()), Some(observer))
        .unwrap()
        .with_credentials("inference-key".into(), "admin-key".into())
}

fn member() -> capyctl_adapters::traits::MemberRef {
    capyctl_adapters::traits::MemberRef {
        deployment_id: "deployment".into(),
        member_id: BINDING.into(),
    }
}

/// W5 (SPEC §§9.2, 10): the embedded SGLang launch is quiescent by its own
/// gauges with a fully mapped saver, parks when the saver shows every
/// allocation released, and restores through resume, disk reload, cache flush
/// and a fresh probe, each proven on both sides by the saver map.
// T22 T16
#[tokio::test]
async fn an_embedded_sglang_parks_and_restores_on_saver_evidence() {
    let stand = stand().await;
    let group = group();
    let adapter = adapter(&stand);
    assert!(adapter.prepare_park(&member()).await.unwrap().quiescent);
    let parked = adapter
        .execute_persisted(&command(RuntimeAction::Park, "park", &group.identities))
        .await
        .unwrap();
    assert_eq!(parked.facts, [Milestone::MemoryReleased]);
    let mut facts = Vec::new();
    for (action, step) in [
        (RuntimeAction::Restore, "restore"),
        (RuntimeAction::ReloadWeights, "restore:reload"),
        (RuntimeAction::InvalidateCache, "restore:cache"),
        (RuntimeAction::Probe, "restore:probe"),
    ] {
        facts.extend(
            adapter
                .execute_persisted(&command(action, step, &group.identities))
                .await
                .unwrap()
                .facts,
        );
    }
    assert_eq!(
        facts,
        [
            Milestone::AllocationsRestored,
            Milestone::WeightsUsable,
            Milestone::CacheValid,
            Milestone::ModelUsable
        ]
    );
    assert_eq!(
        stand.engine.lock().unwrap().calls,
        [
            "/release_memory_occupation",
            "/resume_memory_occupation",
            "/update_weights_from_disk",
            "/flush_cache",
            "/v1/chat/completions"
        ]
    );
}

/// W5: an embedded launch with engine work, or with no enrolled observation,
/// is not quiescent, so the coordinator refuses the park before any call.
// T22 T20
#[tokio::test]
async fn an_embedded_sglang_without_evidence_is_not_quiescent() {
    let stand = stand().await;
    let adapter = adapter(&stand);
    stand.engine.lock().unwrap().busy = true;
    assert!(!adapter.prepare_park(&member()).await.unwrap().quiescent);
    {
        let mut e = stand.engine.lock().unwrap();
        e.busy = false;
        e.unenrolled = true;
    }
    assert!(!adapter.prepare_park(&member()).await.unwrap().quiescent);
    assert!(stand.engine.lock().unwrap().calls.is_empty());
}

/// T20: a resume that maps the weights but not the cache is partial evidence:
/// the step is uncertain and the adapter refuses every later step.
// T22 T20
#[tokio::test]
async fn an_embedded_partial_restore_is_uncertain() {
    let stand = stand().await;
    let group = group();
    let adapter = adapter(&stand);
    adapter
        .execute_persisted(&command(RuntimeAction::Park, "park", &group.identities))
        .await
        .unwrap();
    stand.engine.lock().unwrap().partial_resume = true;
    assert!(adapter
        .execute_persisted(&command(
            RuntimeAction::Restore,
            "restore",
            &group.identities
        ))
        .await
        .is_err());
    assert!(adapter
        .execute_persisted(&command(
            RuntimeAction::ReloadWeights,
            "restore:reload",
            &group.identities
        ))
        .await
        .is_err());
    assert_eq!(
        stand.engine.lock().unwrap().calls,
        ["/release_memory_occupation", "/resume_memory_occupation"]
    );
}

/// A launch-wide observer never answers without a named step.
#[tokio::test]
async fn a_launch_observer_answers_only_for_a_step() {
    let stand = stand().await;
    let observer = LaunchSglangObserver::new(Arc::new(Saver(stand.engine.clone())));
    assert!(observer.observe().await.is_err());
}

/// T15 T20: two park requests racing on one launch send one release: the
/// adapter's single-attempt fence refuses the second rather than repeating
/// the engine effect.
// T15 T20
#[tokio::test]
async fn racing_embedded_parks_send_one_release() {
    let stand = stand().await;
    let group = group();
    let adapter = adapter(&stand);
    let (first, second) = (
        command(RuntimeAction::Park, "park-a", &group.identities),
        command(RuntimeAction::Park, "park-b", &group.identities),
    );
    let (a, b) = tokio::join!(
        adapter.execute_persisted(&first),
        adapter.execute_persisted(&second)
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert_eq!(
        stand.engine.lock().unwrap().calls,
        ["/release_memory_occupation"]
    );
}

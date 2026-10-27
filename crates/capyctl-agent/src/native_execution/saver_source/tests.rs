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
    /// The last release named the KV cache alone: the weights stay mapped.
    weights_kept: bool,
}
type Shared = Arc<Mutex<Engine>>;

async fn serve(
    State(engine): State<Shared>,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
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
                    let tags: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                    e.weights_kept = !tags["tags"]
                        .as_array()
                        .is_some_and(|tags| tags.iter().any(|tag| tag == "weights"));
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
        let weights = if e.released && !e.weights_kept {
            0
        } else {
            1 << 20
        };
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
    fn unenrolled(&self, binding_id: &str) -> bool {
        self.0.lock().unwrap().unenrolled && binding_id == BINDING
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

fn frozen_with(endpoint: String, weight_restore: &str) -> NativeLaunch {
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
            devices: Vec::new(),
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
                startup_eager_cap_bytes: None,
                state_slot_bytes: None,
                state_bytes: None,
                member: None,
                disk_tables: None,
                gguf: None,
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
            weight_restore: weight_restore.into(),
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
    adapter_with(stand, "disk_reload")
}

fn adapter_with(stand: &Stand, weight_restore: &str) -> SglangAdapter {
    let observer: Arc<dyn SglangRuntimeObserver> = Arc::new(LaunchSglangObserver::new(Arc::new(
        Saver(stand.engine.clone()),
    )));
    SglangAdapter::from_frozen(
        &frozen_with(stand.endpoint.clone(), weight_restore),
        Some(observer),
    )
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

/// ADR 0014 amendment A17: a launch whose weights stay resident (SGLang with
/// speculative decoding) parks when the saver shows the KV cache released and
/// the weights still fully mapped, and wakes without a disk reload.
// T22 T16
#[tokio::test]
async fn an_embedded_launch_with_resident_weights_parks_its_kv_cache_alone() {
    let stand = stand().await;
    let group = group();
    let adapter = adapter_with(&stand, "resident");
    assert!(adapter.prepare_park(&member()).await.unwrap().quiescent);
    let parked = adapter
        .execute_persisted(&command(RuntimeAction::Park, "park", &group.identities))
        .await
        .unwrap();
    assert_eq!(parked.facts, [Milestone::MemoryReleased]);
    for (action, step) in [
        (RuntimeAction::Restore, "restore"),
        (RuntimeAction::ReloadWeights, "restore:reload"),
        (RuntimeAction::InvalidateCache, "restore:cache"),
        (RuntimeAction::Probe, "restore:probe"),
    ] {
        adapter
            .execute_persisted(&command(action, step, &group.identities))
            .await
            .unwrap();
    }
    assert_eq!(
        stand.engine.lock().unwrap().calls,
        [
            "/release_memory_occupation",
            "/resume_memory_occupation",
            "/flush_cache",
            "/v1/chat/completions"
        ]
    );
}

/// The same saver map (weights mapped, KV cache released) is partial evidence
/// for a launch that reloads its weights from disk.
// T22 T20
#[test]
fn weights_left_mapped_count_as_released_only_when_they_stay_resident() {
    let kept = SaverMapped {
        real_saver: true,
        weight_bytes: 1 << 20,
        kv_bytes: 0,
        weight_virtual_bytes: 1 << 20,
        kv_virtual_bytes: 1 << 20,
    };
    let mapped = Ok(vec![kept]);
    assert!(saver_facts(&mapped, false).is_none());
    assert!(saver_facts(&mapped, true).is_some_and(|f| f.released && !f.resident));
    // Weights released under a resident launch are not its park either.
    let gone = Ok(vec![SaverMapped {
        weight_bytes: 0,
        ..kept
    }]);
    assert!(saver_facts(&gone, true).is_none());
    assert!(saver_facts(&gone, false).is_some_and(|f| f.released));
}

/// One rank's whole map: every allocation of both tags mapped, or none.
fn rank_map(real_saver: bool, mapped: bool) -> SaverMapped {
    let bytes = if mapped { 1 << 20 } else { 0 };
    SaverMapped {
        real_saver,
        weight_bytes: bytes,
        kv_bytes: bytes,
        weight_virtual_bytes: 1 << 20,
        kv_virtual_bytes: 1 << 20,
    }
}

/// ADR 0035 §9: the park settles only when every GPU's map is released, the
/// wake only when every GPU's is mapped; no ranks, a rank that cannot be
/// judged, or ranks that disagree are unknown, and the saver is real only
/// when every rank's is.
// T46 T20
#[test]
fn saver_facts_settle_only_when_every_rank_agrees() {
    let (released, resident) = (rank_map(true, false), rank_map(true, true));
    assert!(saver_facts(&Ok(vec![released, released]), false)
        .is_some_and(|f| f.released && !f.resident && f.real_saver));
    assert!(saver_facts(&Ok(vec![resident, resident]), false)
        .is_some_and(|f| f.resident && !f.released && f.real_saver));
    // One GPU released, the other still mapped.
    assert_eq!(saver_facts(&Ok(vec![released, resident]), false), None);
    assert_eq!(saver_facts(&Ok(vec![resident, released]), false), None);
    // One rank with no weights pool at all.
    let unjudgeable = SaverMapped {
        weight_bytes: 0,
        weight_virtual_bytes: 0,
        ..released
    };
    assert_eq!(saver_facts(&Ok(vec![released, unjudgeable]), false), None);
    assert_eq!(saver_facts(&Err(SaverUnavailable), false), None);
    assert_eq!(saver_facts(&Ok(Vec::new()), false), None);
    // One rank's saver is a stand-in.
    assert!(
        saver_facts(&Ok(vec![resident, rank_map(false, true)]), false)
            .is_some_and(|f| f.resident && !f.real_saver)
    );
}

/// A private observation directory (0700), as the role creates it.
fn private_dir() -> (tempfile::TempDir, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("observation");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    (root, dir)
}

/// A recorded scheduler process (never read for liveness: the scope names it).
fn scheduler(pid: u32) -> ProcessIdentity {
    ProcessIdentity {
        role: format!("worker-{pid}"),
        pid,
        start_ticks: 100 + u64::from(pid),
        boot_id: "boot".into(),
    }
}

/// One enrollment record, owner-only, as the enrollment writes it.
fn write_record(dir: &Path, name: &str, socket: &str, pid: u32) {
    use std::os::unix::fs::PermissionsExt;
    let owner = scheduler(pid);
    let path = dir.join(format!("{name}.json"));
    let record = serde_json::json!({
        "version": 1,
        "binding_id": BINDING,
        "incarnation_id": INCARNATION,
        "owner": {"pid": owner.pid, "start_ticks": owner.start_ticks, "boot_id": owner.boot_id},
        "library_path": "/opt/sglang/lib/python3.12/site-packages/torch_memory_saver_hook_mode_preload.so",
        "library_sha256": "a".repeat(64),
        "hook_mode": "preload",
        "socket": socket,
    });
    std::fs::write(&path, record.to_string()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

/// A launch on `ranks` GPUs whose recorded schedulers are pids 31 and 32.
fn ranks_scope(ranks: u32) -> SaverScope {
    SaverScope {
        binding_id: BINDING.into(),
        incarnation: INCARNATION.into(),
        members: Some(vec![scheduler(31), scheduler(32)]),
        admin_key: "admin-key".into(),
        executable: "/opt/sglang/bin/python".into(),
        ranks,
    }
}

/// ADR 0035 §9: an enrollment is the binding itself (rank 0) or the binding
/// with the fixed `.rank<r>` suffix the enrollment writes; nothing else is
/// ever opened.
// T46 T20
#[test]
fn enrollment_names_are_the_binding_and_a_fixed_rank_suffix() {
    assert_eq!(enrollment_name(BINDING, 0), BINDING);
    assert_eq!(enrollment_name(BINDING, 3), format!("{BINDING}.rank3"));
    assert!(enrollment(BINDING));
    assert!(enrollment(&format!("{BINDING}.rank63")));
    for name in [
        "../escape".to_owned(),
        format!("{BINDING}.rank0"),
        format!("{BINDING}.rank01"),
        format!("{BINDING}.rank+1"),
        format!("{BINDING}.rank64"),
        format!("{BINDING}.rank"),
        format!("{BINDING}.rank1.json"),
        format!("../{BINDING}.rank1"),
    ] {
        assert!(!enrollment(&name), "{name}");
    }
}

/// ADR 0035 §9: a launch on two GPUs is bound rank by rank before any socket
/// is asked: rank 1's record is `<binding>.rank1.json`, names rank 1's own
/// socket and is enrolled by a recorded scheduler other than rank 0's.
/// Anything else makes the whole read unknown. (The socket read itself needs
/// a live enrolled scheduler per rank, which the fixture enrolls as rank 0
/// only; the record, owner and device checks are proven here.)
// T46 T20
#[test]
fn every_rank_is_bound_to_its_own_enrollment() {
    let (_root, dir) = private_dir();
    let saver = EnrolledSaver::new(dir.clone());
    let rank1 = format!("{BINDING}.rank1");
    write_record(&dir, BINDING, &format!("{BINDING}.sock"), 31);
    assert!(saver
        .enrolled_ranks(&ranks_scope(1))
        .is_ok_and(|enrolled| enrolled.len() == 1));
    // One rank's map is never a launch on two GPUs.
    assert!(saver.mapped(&ranks_scope(2)).is_err());
    // Rank 1 never enrolled.
    assert!(saver.enrolled_ranks(&ranks_scope(2)).is_err());
    assert!(saver.mapped_ranks(&ranks_scope(2)).is_err());
    // Rank 1's record names rank 0's socket.
    write_record(&dir, &rank1, &format!("{BINDING}.sock"), 32);
    assert!(saver.enrolled_ranks(&ranks_scope(2)).is_err());
    // Rank 1 enrolled by rank 0's own scheduler.
    write_record(&dir, &rank1, &format!("{rank1}.sock"), 31);
    assert!(saver.enrolled_ranks(&ranks_scope(2)).is_err());
    // Rank 1 enrolled by a process outside the launch.
    write_record(&dir, &rank1, &format!("{rank1}.sock"), 33);
    assert!(saver.enrolled_ranks(&ranks_scope(2)).is_err());
    // Its own record, socket and scheduler: bound, in rank order.
    write_record(&dir, &rank1, &format!("{rank1}.sock"), 32);
    let enrolled = saver.enrolled_ranks(&ranks_scope(2)).unwrap();
    assert_eq!(
        enrolled
            .iter()
            .map(|(record, owner)| (record.socket.clone(), owner.pid))
            .collect::<Vec<_>>(),
        [
            (format!("{BINDING}.sock"), 31),
            (format!("{rank1}.sock"), 32)
        ]
    );
    // A third rank that never enrolled, and rank counts out of bounds.
    for ranks in [0, 3, 65] {
        assert!(
            saver.enrolled_ranks(&ranks_scope(ranks)).is_err(),
            "{ranks}"
        );
    }
}

/// ADR 0035 §9: rank r's scheduler runs on CUDA ordinal r, so each rank's map
/// must be on its own GPU; on one GPU, at most one device as before.
// T46 T20
#[test]
fn each_ranks_map_is_on_its_own_gpu() {
    assert!(rank_devices([0], 2, 0).is_ok());
    assert!(rank_devices([1], 2, 1).is_ok());
    // Rank 1's map on rank 0's GPU.
    assert!(rank_devices([0], 2, 1).is_err());
    assert!(rank_devices([1, 0], 2, 1).is_err());
    // No allocations is no device to disagree with; `saver_facts` finds no
    // pools in it and calls it unknown.
    assert!(rank_devices(Vec::<u32>::new(), 2, 1).is_ok());
    assert!(rank_devices([0], 1, 0).is_ok());
    assert!(rank_devices([0, 1], 1, 0).is_err());
}

/// ADR 0035 §9: a proven-gone launch's every rank's record and socket go, and
/// nothing else in the directory: another launch's, a name outside the rank
/// range, or an unrelated file.
// T46 T20
#[test]
fn retiring_a_launch_removes_every_ranks_enrollment_only() {
    let (_root, dir) = private_dir();
    let records = [
        format!("{BINDING}.json"),
        format!("{BINDING}.rank1.json"),
        format!("{BINDING}.rank63.json"),
    ];
    let foreign = [
        format!("{INCARNATION}.json"),
        format!("{INCARNATION}.rank1.json"),
        format!("{BINDING}.rank64.json"),
        format!("{BINDING}.rank01.json"),
        "notes.txt".to_owned(),
    ];
    for name in records.iter().chain(&foreign) {
        std::fs::write(dir.join(name), "{}").unwrap();
    }
    let sockets = [format!("{BINDING}.sock"), format!("{BINDING}.rank1.sock")]
        .map(|name| std::os::unix::net::UnixListener::bind(dir.join(name)).unwrap());
    EnrolledSaver::new(dir.clone()).retire(BINDING);
    drop(sockets);
    let mut left = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect::<Vec<_>>();
    left.sort();
    let mut expected = foreign.to_vec();
    expected.sort();
    assert_eq!(left, expected);
}

/// A saver source that reads one map only.
struct OneMap;
impl SaverResidency for OneMap {
    fn mapped(&self, _scope: &SaverScope) -> Result<SaverMapped, SaverUnavailable> {
        Ok(rank_map(true, true))
    }
}

/// ADR 0035 §9: a source that reads one map cannot prove a launch on several
/// GPUs, so by default its per-rank read fails closed there.
// T46 T20
#[test]
fn a_one_map_source_never_reads_a_launch_on_several_gpus() {
    assert!(OneMap.mapped_ranks(&ranks_scope(2)).is_err());
    assert_eq!(
        OneMap.mapped_ranks(&ranks_scope(1)).ok(),
        Some(vec![rank_map(true, true)])
    );
}

/// W5: an embedded launch with engine work, or with no enrolled observation,
/// is not quiescent, so the coordinator refuses the park before any call.
/// Found live 2026-10-14: the one that never enrolled says so.
// T22 T20
#[tokio::test]
async fn an_embedded_sglang_without_evidence_is_not_quiescent() {
    let stand = stand().await;
    let adapter = adapter(&stand);
    stand.engine.lock().unwrap().busy = true;
    let busy = adapter.prepare_park(&member()).await.unwrap();
    assert_eq!((busy.quiescent, busy.unproven), (false, None));
    {
        let mut e = stand.engine.lock().unwrap();
        e.busy = false;
        e.unenrolled = true;
    }
    let unobserved = adapter.prepare_park(&member()).await.unwrap();
    assert_eq!(
        (unobserved.quiescent, unobserved.unproven),
        (false, Some(Unproven::ObservationNotEnrolled))
    );
    assert!(stand.engine.lock().unwrap().calls.is_empty());
}

/// Found live 2026-10-14: a launch provably never enrolled is one whose
/// readable private directory holds no record for its binding; anything
/// unreadable or malformed is unknown, never "not enrolled".
// T22 T20
#[test]
fn only_a_readable_directory_without_a_record_is_unenrolled() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let observation = dir.path().join("observation");
    std::fs::create_dir(&observation).unwrap();
    std::fs::set_permissions(&observation, std::fs::Permissions::from_mode(0o700)).unwrap();
    let saver = EnrolledSaver::new(observation.clone());
    assert!(saver.unenrolled(BINDING));
    assert!(!saver.unenrolled("../escape"));
    std::fs::write(observation.join(format!("{BINDING}.json")), "{}").unwrap();
    assert!(!saver.unenrolled(BINDING));
    std::fs::set_permissions(&observation, std::fs::Permissions::from_mode(0o750)).unwrap();
    assert!(!saver.unenrolled(INCARNATION));
    assert!(!EnrolledSaver::new(dir.path().join("absent")).unenrolled(BINDING));
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

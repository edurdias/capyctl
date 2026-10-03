//! T22: SGLang conformance on the same host park contract.
//!
//! A local axum stand-in answers SGLang's pinned control surface (release,
//! resume, disk reload, cache flush) with the admin key, and a fake saver maps
//! bytes as the engine releases and resumes them. The host observer and the
//! existing SGLang persisted control path then run exactly the steps a remote
//! Park and Restore run. The enrolled-saver tests replace the fake saver with
//! the production source (`EnrolledSaver`) reading an actual key-mode Python
//! listener and record. Nothing here qualifies an SGLang recipe or proves the
//! saver's evidence on hardware (AGENTS.md: CPU and fake tests are not
//! qualification).
use super::*;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::IntoResponse,
};
use capyctl_domain::{
    group::{CommandIdentity, MemberKey},
    launch::{CommonEngineSettings, MemoryRequest, NativeLaunchMetadata, SglangLaunchSettings},
};
use capyctl_protocol::execution::MemberAction;
use std::sync::Mutex;

const BINDING: &str = "01K00000000000000000000001";
const INCARNATION: &str = "01K00000000000000000000002";
const FLUSH: &str = "Cache flushed.\nPlease check backend logs for more details. (When there are running or waiting requests, the operation will not be performed.)\n";

#[derive(Default)]
struct Engine {
    calls: Vec<String>,
    released: bool,
    /// Acknowledge a release without releasing anything.
    lie: bool,
    /// Release (or resume) the weights only, leaving the cache as it was.
    partial: bool,
    weights_only_released: bool,
    /// Report running or waiting requests on the engine's own gauges.
    busy: bool,
    /// The enrolled scheduler fixture the controls drive, when present.
    enrolled: Option<Arc<Mutex<Enrolled>>>,
}
type Shared = Arc<Mutex<Engine>>;

async fn control(
    State(engine): State<Shared>,
    uri: Uri,
    headers: HeaderMap,
) -> axum::response::Response {
    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer admin-key") {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut e = engine.lock().unwrap();
    e.calls.push(uri.path().to_string());
    match uri.path() {
        "/release_memory_occupation" => {
            if e.partial {
                e.weights_only_released = true;
            } else {
                e.released = !e.lie;
            }
            if let (Some(enrolled), false) = (&e.enrolled, e.lie) {
                enrolled
                    .lock()
                    .unwrap()
                    .send(if e.partial { "partial" } else { "release" });
            }
            "null".into_response()
        }
        "/resume_memory_occupation" => {
            if e.partial {
                e.weights_only_released = true;
                e.released = false;
            } else {
                e.released = false;
                e.weights_only_released = false;
            }
            if let Some(enrolled) = &e.enrolled {
                enrolled
                    .lock()
                    .unwrap()
                    .send(if e.partial { "partial" } else { "resume" });
            }
            "null".into_response()
        }
        "/update_weights_from_disk" => {
            axum::Json(serde_json::json!({"success": true})).into_response()
        }
        "/flush_cache" => FLUSH.into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

struct Saver {
    engine: Shared,
    real: bool,
}
impl SaverResidency for Saver {
    fn mapped(&self, scope: &SaverScope) -> Result<SaverMapped, SaverUnavailable> {
        if (scope.binding_id.as_str(), scope.incarnation.as_str()) != (BINDING, INCARNATION)
            || scope.members.as_deref() != Some(&identities()[..])
            || scope.admin_key != "admin-key"
        {
            return Err(SaverUnavailable);
        }
        let engine = self.engine.lock().unwrap();
        let kv = if engine.released { 0 } else { 1 << 20 };
        let weights = if engine.released || engine.weights_only_released {
            0
        } else {
            1 << 20
        };
        Ok(SaverMapped {
            real_saver: self.real,
            weight_bytes: weights,
            kv_bytes: kv,
            weight_virtual_bytes: 1 << 20,
            kv_virtual_bytes: 1 << 20,
        })
    }
}

fn identities() -> Vec<ProcessIdentity> {
    ["api", "worker-0"]
        .iter()
        .enumerate()
        .map(|(i, role)| ProcessIdentity {
            role: (*role).into(),
            pid: 21 + i as u32,
            boot_id: "boot".into(),
            start_ticks: 100 + i as u64,
        })
        .collect()
}

fn frozen(endpoint: String) -> NativeLaunch {
    frozen_at(endpoint, "/opt/sglang/python")
}

fn frozen_at(endpoint: String, executable: &str) -> NativeLaunch {
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
        executable.into(),
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
            },
            max_total_tokens: None,
            max_mamba_cache_size: None,
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

fn park_command() -> MemberCommand {
    MemberCommand {
        identity: CommandIdentity {
            controller_id: "controller".into(),
            member: MemberKey {
                host_id: "host".into(),
                member_id: "head".into(),
            },
            deployment_id: "deployment".into(),
            operation_id: "operation".into(),
            command_id: "park".into(),
            step_id: "park".into(),
            generation: 1,
            revision: 1,
            deadline_ms: capyctl_protocol::now_unix_ms() + 30_000,
            payload_digest: [0; 32],
            expected_state: "ready".into(),
            profile_fingerprint: "fingerprint".into(),
            instance_index: 0,
        },
        action: MemberAction::Park {
            owned_handle: "launch".into(),
        },
    }
}

fn plan() -> SingleLaunchPlan {
    SingleLaunchPlan {
        deployment_config: "{}".into(),
        profile_name: "local".into(),
        checkpoint_fingerprint: "checkpoint".into(),
        host_policy_fingerprint: "a".repeat(64),
        binding_id: BINDING.into(),
        incarnation: INCARNATION.into(),
        grant_id: "01K00000000000000000000003".into(),
        service_port: 1,
        issued_at_ms: 1,
        coordinator_session_id: "01K00000000000000000000004".into(),
        checkpoint_digest: String::new(),
        checkpoint_weights_bytes: None,
        startup_bytes: None,
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
        .fallback(control)
        .with_state(engine.clone());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Stand {
        engine,
        endpoint,
        task,
    }
}

fn driver(stand: &Stand, saver: Option<Arc<dyn SaverResidency>>, in_flight: usize) -> Driver {
    driver_with(
        stand,
        saver,
        in_flight,
        frozen(stand.endpoint.clone()),
        identities(),
    )
}

fn driver_with(
    stand: &Stand,
    saver: Option<Arc<dyn SaverResidency>>,
    in_flight: usize,
    frozen: NativeLaunch,
    live: Vec<ProcessIdentity>,
) -> Driver {
    let engine = stand.engine.clone();
    Driver::Sglang {
        frozen: Box::new(frozen),
        inference: "inference-key".into(),
        admin: "admin-key".into(),
        saver,
        live: Arc::new(move || Ok(live.clone())),
        in_flight: Arc::new(move || Ok(in_flight)),
        // The engine's own gauges (SPEC §10 step 4), as the stand reports them.
        idle: Arc::new(move || {
            let busy = engine.lock().unwrap().busy;
            Box::pin(async move { Ok(!busy) })
        }),
    }
}

fn saver(stand: &Stand, real: bool) -> Option<Arc<dyn SaverResidency>> {
    Some(Arc::new(Saver {
        engine: stand.engine.clone(),
        real,
    }))
}

/// T22 / SPEC §9.2: the host's SGLang park and restore run the existing
/// persisted control path in order, each step proven by the saver's mapping
/// on both sides, with the admin key only.
// T22 T16
#[tokio::test]
async fn sglang_parks_and_restores_through_its_persisted_controls() {
    let stand = stand().await;
    let (command, plan, expected) = (park_command(), plan(), identities());
    let run = Run {
        driver: driver(&stand, saver(&stand, true), 0),
        command: &command,
        plan: &plan,
        expected: &expected,
        stop_at_ms: capyctl_protocol::now_unix_ms() + 20_000,
    };
    assert!(run.quiescent().await);
    let parked = run.run(RuntimeAction::Park, true).await.ok().unwrap();
    assert_eq!(
        parked.facts,
        [capyctl_domain::completion::Milestone::MemoryReleased]
    );
    assert_eq!(parked.identities, expected);
    let mut facts = Vec::new();
    for (action, first) in [
        (RuntimeAction::Restore, true),
        (RuntimeAction::ReloadWeights, false),
        (RuntimeAction::InvalidateCache, false),
    ] {
        facts.extend(run.run(action, first).await.ok().unwrap().facts);
    }
    use capyctl_domain::completion::Milestone::*;
    assert_eq!(facts, [AllocationsRestored, WeightsUsable, CacheValid]);
    assert_eq!(
        stand.engine.lock().unwrap().calls,
        [
            "/release_memory_occupation",
            "/resume_memory_occupation",
            "/update_weights_from_disk",
            "/flush_cache"
        ]
    );
}

/// A host that cannot observe the saver, whose saver is not the approved one,
/// or whose ingress still forwards work refuses the park before any engine
/// call: nothing is released on evidence the host does not have.
// T22 T20
#[tokio::test]
async fn sglang_without_saver_evidence_or_quiescence_is_refused_before_any_call() {
    let stand = stand().await;
    let (command, plan, expected) = (park_command(), plan(), identities());
    for driver in [
        driver(&stand, None, 0),
        driver(&stand, saver(&stand, false), 0),
        driver(&stand, saver(&stand, true), 1),
    ] {
        let run = Run {
            driver,
            command: &command,
            plan: &plan,
            expected: &expected,
            stop_at_ms: capyctl_protocol::now_unix_ms() + 20_000,
        };
        assert!(!run.quiescent().await);
        assert!(matches!(
            run.run(RuntimeAction::Park, true).await,
            Err(StepFailure::Refused)
        ));
    }
    assert!(stand.engine.lock().unwrap().calls.is_empty());
}

/// SPEC §13.2: an acknowledged release the saver does not corroborate is not a
/// park. The outcome is uncertain, never parked.
// T22 T20
#[tokio::test]
async fn an_uncorroborated_sglang_release_is_uncertain() {
    let stand = stand().await;
    stand.engine.lock().unwrap().lie = true;
    let (command, plan, expected) = (park_command(), plan(), identities());
    let run = Run {
        driver: driver(&stand, saver(&stand, true), 0),
        command: &command,
        plan: &plan,
        expected: &expected,
        stop_at_ms: capyctl_protocol::now_unix_ms() + 20_000,
    };
    assert!(matches!(
        run.run(RuntimeAction::Park, true).await,
        Err(StepFailure::Uncertain)
    ));
    assert_eq!(
        stand.engine.lock().unwrap().calls,
        ["/release_memory_occupation"]
    );
}

/// T20: a release that leaves the cache mapped (the weights alone released)
/// is partial evidence: uncertain, never parked.
// T22 T20
#[tokio::test]
async fn a_partial_sglang_release_is_uncertain() {
    let stand = stand().await;
    stand.engine.lock().unwrap().partial = true;
    let (command, plan, expected) = (park_command(), plan(), identities());
    let run = Run {
        driver: driver(&stand, saver(&stand, true), 0),
        command: &command,
        plan: &plan,
        expected: &expected,
        stop_at_ms: capyctl_protocol::now_unix_ms() + 20_000,
    };
    assert!(run.quiescent().await);
    assert!(matches!(
        run.run(RuntimeAction::Park, true).await,
        Err(StepFailure::Uncertain)
    ));
}

/// T20: a resume that maps the weights but not the cache is not restored
/// allocations: uncertain after the first effect, with nothing retried.
// T22 T20
#[tokio::test]
async fn a_partial_sglang_resume_is_uncertain() {
    let stand = stand().await;
    let (command, plan, expected) = (park_command(), plan(), identities());
    let run = Run {
        driver: driver(&stand, saver(&stand, true), 0),
        command: &command,
        plan: &plan,
        expected: &expected,
        stop_at_ms: capyctl_protocol::now_unix_ms() + 20_000,
    };
    run.run(RuntimeAction::Park, true).await.ok().unwrap();
    stand.engine.lock().unwrap().partial = true;
    assert!(matches!(
        run.run(RuntimeAction::Restore, true).await,
        Err(StepFailure::Uncertain)
    ));
    assert_eq!(
        stand.engine.lock().unwrap().calls,
        ["/release_memory_occupation", "/resume_memory_occupation"]
    );
}

/// SPEC §10 step 4: the engine's own gauges showing running or waiting work
/// refuse the park before any call, whatever the ingress count says.
// T22
#[tokio::test]
async fn sglang_with_engine_work_is_refused_before_any_call() {
    let stand = stand().await;
    stand.engine.lock().unwrap().busy = true;
    let (command, plan, expected) = (park_command(), plan(), identities());
    let run = Run {
        driver: driver(&stand, saver(&stand, true), 0),
        command: &command,
        plan: &plan,
        expected: &expected,
        stop_at_ms: capyctl_protocol::now_unix_ms() + 20_000,
    };
    assert!(!run.quiescent().await);
    assert!(matches!(
        run.run(RuntimeAction::Park, true).await,
        Err(StepFailure::Refused)
    ));
    assert!(stand.engine.lock().unwrap().calls.is_empty());
}

/// An actual key-mode enrolled-scheduler listener (the production Python
/// listener and record format) whose saver map the stand's controls flip.
struct Enrolled {
    child: std::process::Child,
    stdout: std::io::BufReader<std::process::ChildStdout>,
    identity: ProcessIdentity,
}
impl Enrolled {
    fn send(&mut self, command: &str) {
        use std::io::{BufRead, Write};
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{command}").unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "ok");
    }
}
impl Drop for Enrolled {
    fn drop(&mut self) {
        // A clean exit closes the listener, which removes its socket.
        drop(self.child.stdin.take());
        for _ in 0..300 {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Venv {
    root: tempfile::TempDir,
    observation: std::path::PathBuf,
    executable: String,
}

/// A private observation directory and an installation prefix holding a saver
/// preload library, as the enrollment finds them on a Spark.
fn venv() -> Venv {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::Builder::new()
        .prefix("capyctl-saver-")
        .tempdir_in(std::env::var("HOME").unwrap())
        .unwrap();
    // The socket's ancestors must admit no group or other writer.
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let observation = root.path().join("obs");
    std::fs::create_dir(&observation).unwrap();
    std::fs::set_permissions(&observation, std::fs::Permissions::from_mode(0o700)).unwrap();
    let site = root.path().join("venv/lib/python3.12/site-packages");
    std::fs::create_dir_all(&site).unwrap();
    std::fs::create_dir_all(root.path().join("venv/bin")).unwrap();
    std::fs::write(
        site.join("torch_memory_saver_hook_mode_preload_cu13.abi3.so"),
        b"preload library bytes",
    )
    .unwrap();
    Venv {
        executable: root
            .path()
            .join("venv/bin/python")
            .to_string_lossy()
            .into_owned(),
        observation,
        root,
    }
}

fn enroll(venv: &Venv, admin: &str) -> Enrolled {
    use std::io::BufRead;
    use std::process::{Command, Stdio};
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let library = venv.root.path().join(
        "venv/lib/python3.12/site-packages/torch_memory_saver_hook_mode_preload_cu13.abi3.so",
    );
    let mut child = Command::new("python3")
        .args(["-I", "-B"])
        .arg(manifest.join("tests/fixtures/enrolled_scheduler.py"))
        .arg(manifest.join("../..").canonicalize().unwrap())
        .arg(&venv.observation)
        .args([BINDING, INCARNATION, admin])
        .arg(library.canonicalize().unwrap())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let wire: serde_json::Value = serde_json::from_str(&line).unwrap();
    Enrolled {
        child,
        stdout,
        identity: ProcessIdentity {
            role: "worker-0".into(),
            pid: wire["pid"].as_u64().unwrap() as u32,
            boot_id: wire["boot_id"].as_str().unwrap().into(),
            start_ticks: wire["start_ticks"].as_u64().unwrap(),
        },
    }
}

/// The launch's recorded processes: an API process and the enrolled scheduler.
fn enrolled_group(enrolled: &Enrolled) -> Vec<ProcessIdentity> {
    vec![
        ProcessIdentity {
            role: "api".into(),
            pid: 21,
            boot_id: enrolled.identity.boot_id.clone(),
            start_ticks: 100,
        },
        enrolled.identity.clone(),
    ]
}

/// T22 T16 T33: the production saver source over an enrolled scheduler. A
/// host parks (the saver shows every allocation unmapped), then restarts: a
/// new host instance with a new `EnrolledSaver` over the same private
/// directory and the launch's recorded credential restores it, proven by the
/// saver map on both sides of each step.
// T22 T16 T33
#[tokio::test]
async fn an_enrolled_sglang_parks_and_a_restarted_host_restores_it() {
    let stand = stand().await;
    let venv = venv();
    let enrolled = Arc::new(Mutex::new(enroll(&venv, "admin-key")));
    stand.engine.lock().unwrap().enrolled = Some(enrolled.clone());
    let expected = enrolled_group(&enrolled.lock().unwrap());
    let (command, plan) = (park_command(), plan());
    let source = |dir: &std::path::Path| -> Option<Arc<dyn SaverResidency>> {
        Some(Arc::new(super::super::EnrolledSaver::new(
            dir.to_path_buf(),
        )))
    };
    {
        let run = Run {
            driver: driver_with(
                &stand,
                source(&venv.observation),
                0,
                frozen_at(stand.endpoint.clone(), &venv.executable),
                expected.clone(),
            ),
            command: &command,
            plan: &plan,
            expected: &expected,
            stop_at_ms: capyctl_protocol::now_unix_ms() + 20_000,
        };
        assert!(run.quiescent().await);
        assert_eq!(run.saver_mapped_bytes().await, Some(8192));
        let parked = run.run(RuntimeAction::Park, true).await.ok().unwrap();
        assert_eq!(
            parked.facts,
            [capyctl_domain::completion::Milestone::MemoryReleased]
        );
        assert_eq!(run.saver_mapped_bytes().await, Some(0));
    }
    // The host restarted: nothing in memory survives but the directory, the
    // enrolled engine and the launch's recorded credential.
    let run = Run {
        driver: driver_with(
            &stand,
            source(&venv.observation),
            0,
            frozen_at(stand.endpoint.clone(), &venv.executable),
            expected.clone(),
        ),
        command: &command,
        plan: &plan,
        expected: &expected,
        stop_at_ms: capyctl_protocol::now_unix_ms() + 20_000,
    };
    let mut facts = Vec::new();
    for (action, first) in [
        (RuntimeAction::Restore, true),
        (RuntimeAction::ReloadWeights, false),
        (RuntimeAction::InvalidateCache, false),
    ] {
        facts.extend(run.run(action, first).await.ok().unwrap().facts);
    }
    use capyctl_domain::completion::Milestone::*;
    assert_eq!(facts, [AllocationsRestored, WeightsUsable, CacheValid]);
    assert_eq!(run.saver_mapped_bytes().await, Some(8192));
    assert_eq!(
        stand.engine.lock().unwrap().calls,
        [
            "/release_memory_occupation",
            "/resume_memory_occupation",
            "/update_weights_from_disk",
            "/flush_cache"
        ]
    );
    // Close the listener while its directory still exists.
    stand.engine.lock().unwrap().enrolled.take();
    drop(enrolled);
    // A launch proven gone leaves no enrollment behind.
    let saver = super::super::EnrolledSaver::new(venv.observation.clone());
    saver.retire("../escape");
    saver.retire(BINDING);
    assert_eq!(std::fs::read_dir(&venv.observation).unwrap().count(), 0);
}

/// T20: a saver read that misses its bound is read again, never guessed.
/// Found live 2026-09-26 on a discrete-GPU laptop: a busy SGLang scheduler
/// answered in 1.0-1.6 s against the 1.5 s bound, so one late answer left a
/// park uncertain. A read has no effect, so a fresh read (new request id) is
/// safe; the answer still has to be a whole, bound observation.
// T20
#[test]
fn a_saver_read_that_misses_its_bound_is_read_again() {
    let venv = venv();
    let mut enrolled = enroll(&venv, "admin-key");
    let scope = SaverScope {
        binding_id: BINDING.into(),
        incarnation: INCARNATION.into(),
        members: Some(enrolled_group(&enrolled)),
        admin_key: "admin-key".into(),
        executable: venv.executable.clone(),
    };
    let saver = super::super::EnrolledSaver::new(venv.observation.clone());
    enrolled.send("stall");
    let Ok(mapped) = saver.mapped(&scope) else {
        panic!("a late read is read again");
    };
    assert_eq!(mapped.weight_bytes + mapped.kv_bytes, 8192);
}

/// T22 T20: the enrolled source refuses what it cannot bind to this launch
/// before any engine call: no record (the launch enrolled nothing), another
/// credential, a scheduler outside the recorded group, or a saver library
/// whose bytes are not the ones the scheduler loaded.
// T22 T20
#[tokio::test]
async fn an_unbound_enrollment_refuses_the_park_before_any_call() {
    let stand = stand().await;
    let venv = venv();
    let (command, plan) = (park_command(), plan());
    let refused = |saver: Option<Arc<dyn SaverResidency>>, expected: Vec<ProcessIdentity>| {
        let stand = &stand;
        let venv = &venv;
        let (command, plan) = (&command, &plan);
        async move {
            let run = Run {
                driver: driver_with(
                    stand,
                    saver,
                    0,
                    frozen_at(stand.endpoint.clone(), &venv.executable),
                    expected.clone(),
                ),
                command,
                plan,
                expected: &expected,
                stop_at_ms: capyctl_protocol::now_unix_ms() + 20_000,
            };
            !run.quiescent().await
                && matches!(
                    run.run(RuntimeAction::Park, true).await,
                    Err(StepFailure::Refused)
                )
        }
    };
    let saver = || -> Option<Arc<dyn SaverResidency>> {
        Some(Arc::new(super::super::EnrolledSaver::new(
            venv.observation.clone(),
        )))
    };
    // Nothing enrolled yet.
    assert!(refused(saver(), identities()).await);
    // Enrolled under another credential.
    let enrolled = enroll(&venv, "another-admin-key");
    let group = enrolled_group(&enrolled);
    assert!(refused(saver(), group.clone()).await);
    drop(enrolled);
    std::fs::remove_file(venv.observation.join(format!("{BINDING}.json"))).unwrap();
    // The right credential, but the scheduler is not in the recorded group.
    let enrolled = enroll(&venv, "admin-key");
    assert!(refused(saver(), identities()).await);
    // The saver library changed on disk after the scheduler loaded it.
    std::fs::write(
        venv.root.path().join(
            "venv/lib/python3.12/site-packages/torch_memory_saver_hook_mode_preload_cu13.abi3.so",
        ),
        b"other bytes",
    )
    .unwrap();
    assert!(refused(saver(), enrolled_group(&enrolled)).await);
    assert!(stand.engine.lock().unwrap().calls.is_empty());
}

// T21: SPEC §13.3, the observation's admin credential is never formatted.
#[test]
fn saver_scope_debug_never_formats_the_admin_key() {
    let scope = SaverScope {
        binding_id: "binding".into(),
        incarnation: "incarnation".into(),
        members: None,
        admin_key: "deadbeefsecret".into(),
        executable: "/venv/bin/python".into(),
    };
    let shown = format!("{scope:?}");
    assert!(shown.contains("binding"));
    assert!(!shown.contains("deadbeefsecret"));
}

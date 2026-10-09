use super::*;
use crate::ownership::OwnedCoordinatorState;
use capyctl_store::lifecycle::DeploymentFence;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

struct Fixture {
    _state: tempfile::TempDir,
    owner: SharedCoordinatorState,
    fence: DeploymentFence,
}

/// An owned store with one accepted deployment declaring a Hugging Face source.
fn fixture() -> Fixture {
    use std::os::unix::fs::PermissionsExt;
    let source: Value = serde_json::from_str(include_str!(
        "../../../capyctl-config/tests/fixtures/f2-deployment.json"
    ))
    .unwrap();
    let mut host = source["host"].clone();
    host["model_sources"] = json!({"huggingface": "allowed", "max_bytes": "100GiB"});
    let mut deployment = source["deployment"].clone();
    let model = deployment["model"].as_object_mut().unwrap();
    model.remove("path");
    model.insert(
        "source".into(),
        json!({"huggingface": {"repo": "Qwen/Qwen3-4B", "revision": SHA}}),
    );
    let state = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let owner = Arc::new(Mutex::new(
        OwnedCoordinatorState::open(state.path()).unwrap(),
    ));
    let fence = {
        let o = owner.lock().unwrap();
        let session = o.session().clone();
        let policy = capyctl_config::effective::resolve_effective(&deployment, &host)
            .unwrap()
            .host;
        o.store()
            .import_resource_policy(
                &session,
                &policy,
                &[capyctl_domain::resources::MemoryObservation {
                    domain: "unified".into(),
                    capacity_bytes: 64 << 30,
                    available_bytes: 60 << 30,
                    sampled_at_ms: 1,
                }],
                1,
            )
            .unwrap();
        let receipt = o
            .store()
            .create_stopped_managed_configuration(
                &session,
                "owner",
                "hf",
                &json!({"config": deployment}).to_string(),
                &host,
                1,
            )
            .unwrap();
        DeploymentFence {
            deployment_id: receipt.deployment_id,
            revision: receipt.revision,
            generation: receipt.generation,
        }
    };
    Fixture {
        _state: state,
        owner,
        fence,
    }
}

fn state_of(f: &Fixture) -> capyctl_store::model_sources::ModelSourceRecord {
    f.owner
        .lock()
        .unwrap()
        .store()
        .model_source(
            &f.fence.deployment_id,
            f.fence.revision,
            "lab",
            &format!("sources/huggingface/Qwen--Qwen3-4B@{SHA}"),
        )
        .unwrap()
        .unwrap()
}

struct Scripted {
    reachable: bool,
    answers: Mutex<VecDeque<Result<SourceReport, Unavailable>>>,
    calls: AtomicUsize,
}
impl SourceHost for Scripted {
    fn reachable(&self, _: &str) -> bool {
        self.reachable
    }
    fn request(&self, _: PendingSource) -> ReportFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let answer = self
            .answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Err(Unavailable));
        Box::pin(async move { answer })
    }
}

fn scripted(reachable: bool, answers: Vec<Result<SourceReport, Unavailable>>) -> Arc<Scripted> {
    Arc::new(Scripted {
        reachable,
        answers: Mutex::new(answers.into()),
        calls: AtomicUsize::new(0),
    })
}

fn report(state: SourceState, done: u64, total: u64, reason: Option<&str>) -> SourceReport {
    SourceReport {
        state,
        bytes_done: done,
        bytes_total: total,
        reason: reason.map(Into::into),
    }
}

async fn run(supervisor: &Arc<SourceMaterializer>) {
    for task in supervisor.clone().pass() {
        task.await.unwrap();
    }
}

// T14 (ADR 0008): the supervisor records the host's progress, polls a running
// download, and stops once the copy is verified.
#[tokio::test]
async fn the_supervisor_records_progress_until_verified() {
    let f = fixture();
    let host = scripted(
        true,
        vec![
            Ok(report(SourceState::Downloading, 0, 0, None)),
            Ok(report(SourceState::Downloading, 50, 100, None)),
            Ok(report(SourceState::Verified, 100, 100, None)),
        ],
    );
    let supervisor = SourceMaterializer::new(f.owner.clone(), host.clone());
    run(&supervisor).await;
    assert_eq!(state_of(&f).state, SourceState::Downloading);
    // Polled again only after the poll interval.
    run(&supervisor).await;
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    supervisor.attempts.lock().unwrap().clear();
    run(&supervisor).await;
    assert_eq!(state_of(&f).bytes_done, 50);
    supervisor.attempts.lock().unwrap().clear();
    run(&supervisor).await;
    assert_eq!(state_of(&f).state, SourceState::Verified);
    // Nothing pending remains: no further request.
    supervisor.attempts.lock().unwrap().clear();
    run(&supervisor).await;
    assert_eq!(host.calls.load(Ordering::SeqCst), 3);
}

// T14 (ADR 0008): an unreachable host is not asked; a terminal failure is
// recorded and never retried; a retryable one backs off.
#[tokio::test]
async fn failures_are_recorded_and_terminal_ones_are_not_retried() {
    let f = fixture();
    let offline = scripted(false, vec![]);
    run(&SourceMaterializer::new(f.owner.clone(), offline.clone())).await;
    assert_eq!(offline.calls.load(Ordering::SeqCst), 0);

    let host = scripted(
        true,
        vec![
            Ok(report(SourceState::Failed, 0, 0, Some("network"))),
            Ok(report(SourceState::Failed, 0, 0, Some("hash_mismatch"))),
        ],
    );
    let supervisor = SourceMaterializer::new(f.owner.clone(), host.clone());
    run(&supervisor).await;
    assert_eq!(state_of(&f).reason.as_deref(), Some("network"));
    assert!(!state_of(&f).terminal);
    run(&supervisor).await;
    assert_eq!(host.calls.load(Ordering::SeqCst), 1, "backed off");
    supervisor.attempts.lock().unwrap().clear();
    run(&supervisor).await;
    assert!(state_of(&f).terminal);
    supervisor.attempts.lock().unwrap().clear();
    run(&supervisor).await;
    assert_eq!(
        host.calls.load(Ordering::SeqCst),
        2,
        "terminal: not retried"
    );
}

// T34 (ADR 0008): only source evidence converts into a report.
#[test]
fn reports_come_from_source_evidence_only() {
    let mut result = pb::MemberExecutionResult::default();
    assert_eq!(report_from(&result), Err(Unavailable));
    result.source = Some(pb::ModelSourceEvidence {
        state: "failed".into(),
        reason: "too_large".into(),
        ..Default::default()
    });
    assert_eq!(
        report_from(&result).unwrap(),
        report(SourceState::Failed, 0, 0, Some("too_large"))
    );
    result.source.as_mut().unwrap().state = "bogus".into();
    assert_eq!(report_from(&result), Err(Unavailable));
}

/// An owned store under `models` with one accepted deployment whose weights
/// are local and whose drafter is the http payload pinned by `sha256`.
fn drafter_fixture(models: &std::path::Path, sha256: &str) -> Fixture {
    use std::os::unix::fs::PermissionsExt;
    let source: Value = serde_json::from_str(include_str!(
        "../../../capyctl-config/tests/fixtures/f2-deployment.json"
    ))
    .unwrap();
    let mut host = source["host"].clone();
    host["model_store"]["path"] = json!(models);
    host["model_sources"] = json!({"http": "allowed", "max_bytes": "1GiB"});
    host["runtime_profiles"]["local"]["security"]["approved_options"] =
        json!(["--speculative-config"]);
    let mut deployment = source["deployment"].clone();
    deployment["model"]["draft"] = json!({"http": {
        "url": "https://drafts.example.test/d.bin", "sha256": sha256}});
    deployment["engine_config"]["accept_extra_args"] = json!(true);
    deployment["engine_config"]["extra_args"] = json!([
        "--speculative-config",
        json!({"method": "draft_model", "num_speculative_tokens": 4}).to_string()
    ]);
    let state = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let owner = Arc::new(Mutex::new(
        OwnedCoordinatorState::open(state.path()).unwrap(),
    ));
    let fence = {
        let o = owner.lock().unwrap();
        let session = o.session().clone();
        let policy = capyctl_config::effective::resolve_effective(&deployment, &host)
            .unwrap()
            .host;
        o.store()
            .import_resource_policy(
                &session,
                &policy,
                &[capyctl_domain::resources::MemoryObservation {
                    domain: "unified".into(),
                    capacity_bytes: 64 << 30,
                    available_bytes: 60 << 30,
                    sampled_at_ms: 1,
                }],
                1,
            )
            .unwrap();
        let receipt = o
            .store()
            .create_stopped_managed_configuration(
                &session,
                "owner",
                "drafted",
                &json!({"config": deployment}).to_string(),
                &host,
                1,
            )
            .unwrap();
        DeploymentFence {
            deployment_id: receipt.deployment_id,
            revision: receipt.revision,
            generation: receipt.generation,
        }
    };
    Fixture {
        _state: state,
        owner,
        fence,
    }
}

/// A loopback origin serving `payload` at `/d.bin`.
async fn origin(payload: Vec<u8>) -> String {
    let app = axum::Router::new().route(
        "/d.bin",
        axum::routing::get(move || {
            let payload = payload.clone();
            async move { payload }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    origin
}

/// Run the supervisor over the host's own store until the drafter's record
/// settles (verified or terminal).
async fn settle(f: &Fixture, store: Arc<capyctl_agent::sources::SourceStore>, key: &str) {
    let supervisor = SourceMaterializer::new(f.owner.clone(), LocalSources::new(store));
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        run(&supervisor).await;
        supervisor.attempts.lock().unwrap().clear();
        let record = f
            .owner
            .lock()
            .unwrap()
            .store()
            .model_source(&f.fence.deployment_id, f.fence.revision, "lab", key)
            .unwrap()
            .unwrap();
        if record.state == SourceState::Verified || record.terminal {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "{record:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// T14 (ADR 0008 amendment 2026-10-08): a declared remote drafter is
// materialized into the host's sources store through the same path and
// verification as the weights: the pinned SHA-256 verifies it, and bytes
// that do not match their pin fail `hash_mismatch`, terminally.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_drafter_is_materialized_and_verified_like_the_weights() {
    use sha2::Digest as _;
    let payload = vec![7_u8; 4096];
    let sha = hex::encode(sha2::Sha256::digest(&payload));
    let origin = origin(payload).await;
    let policy = capyctl_config::effective::ModelSourcePolicy {
        http: capyctl_config::effective::SourceSwitch::Allowed,
        max_bytes: Some(1 << 30),
        ..Default::default()
    };
    for (pinned, verified) in [(sha.clone(), true), ("c".repeat(64), false)] {
        let models = tempfile::tempdir().unwrap();
        let f = drafter_fixture(models.path(), &pinned);
        let key = format!("sources/http/{pinned}");
        let store = capyctl_agent::sources::SourceStore::with_loopback_origin(
            models.path(),
            policy.clone(),
            None,
            &origin,
        );
        settle(&f, store, &key).await;
        let record = f
            .owner
            .lock()
            .unwrap()
            .store()
            .model_source(&f.fence.deployment_id, f.fence.revision, "lab", &key)
            .unwrap()
            .unwrap();
        if verified {
            assert_eq!(record.state, SourceState::Verified, "{record:?}");
            assert_eq!(record.bytes_total, 4096);
            assert_eq!(
                std::fs::read(models.path().join(&key).join("d.bin")).unwrap(),
                vec![7_u8; 4096]
            );
        } else {
            assert_eq!(
                record.reason.as_deref(),
                Some("hash_mismatch"),
                "{record:?}"
            );
            assert!(record.terminal);
            assert!(!models.path().join(&key).exists());
        }
    }
}

// T14 (ADR 0008 amendment 2026-10-08): before a launch, a host is asked for
// every remote source the revision names, its weights' and its drafter's,
// each by its own key; the launch is refused while either is not verified.
#[tokio::test]
async fn a_launch_waits_for_the_drafter_beside_the_weights() {
    let f = fixture();
    let mut config: Value = serde_json::from_str(include_str!(
        "../../../capyctl-config/tests/fixtures/f2-deployment.json"
    ))
    .unwrap();
    let config = &mut config["deployment"];
    config["model"] = json!({
        "source": {"huggingface": {"repo": "Qwen/Qwen3-4B", "revision": SHA}},
        "draft": {"http": {"url": "https://drafts.example.test/d.bin", "sha256": "d".repeat(64)}},
        "content_fingerprint": "measured", "revision": "1",
    });
    let config = config.to_string();
    for (drafter, expected) in [
        ("verified", Ok(())),
        (
            "failed",
            Err(RuntimeError::Refused("model_source:hash_mismatch".into())),
        ),
    ] {
        let asked = Mutex::new(Vec::new());
        let outcome = ensure_materialized_with(
            &f.owner,
            true,
            |command: MemberCommand| {
                let MemberAction::MaterializeSource(plan) = &command.action else {
                    panic!("{:?}", command.action);
                };
                asked.lock().unwrap().push(plan.source_key.clone());
                let draft = plan.source_key.starts_with("sources/http/");
                let state = if draft { drafter } else { "verified" };
                let result = pb::MemberExecutionResult {
                    source: Some(pb::ModelSourceEvidence {
                        source_key: plan.source_key.clone(),
                        state: state.into(),
                        reason: if state == "failed" {
                            "hash_mismatch".into()
                        } else {
                            String::new()
                        },
                        ..Default::default()
                    }),
                    ..Default::default()
                };
                async move { Ok::<_, ()>(result) }
            },
            "controller",
            "lab",
            "head",
            &f.fence.deployment_id,
            f.fence.revision,
            f.fence.generation,
            "fp",
            &config,
            &"a".repeat(64),
            capyctl_protocol::now_unix_ms() + 60_000,
        )
        .await;
        assert_eq!(outcome, expected, "{drafter}");
        assert_eq!(
            *asked.lock().unwrap(),
            [
                format!("sources/huggingface/Qwen--Qwen3-4B@{SHA}"),
                format!("sources/http/{}", "d".repeat(64))
            ]
        );
    }
}

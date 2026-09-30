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
        .model_source(&f.fence.deployment_id, f.fence.revision, "lab")
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

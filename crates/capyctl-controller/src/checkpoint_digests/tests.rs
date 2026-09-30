use super::*;
use crate::ownership::OwnedCoordinatorState;
use capyctl_domain::completion::{ExecutionIdentities, StepExecutionContext, TransitionToken};
use capyctl_store::checkpoint_digests::DigestState;
use capyctl_store::lifecycle::DeploymentFence;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture {
    _state: tempfile::TempDir,
    models: tempfile::TempDir,
    owner: SharedCoordinatorState,
    fence: DeploymentFence,
}

/// An owned store with one accepted deployment whose checkpoint is a real
/// directory in a real model store.
fn fixture(edit: impl FnOnce(&mut Value)) -> Fixture {
    use std::os::unix::fs::PermissionsExt;
    let source: Value = serde_json::from_str(include_str!(
        "../../../capyctl-config/tests/fixtures/f2-deployment.json"
    ))
    .unwrap();
    let models = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(models.path().join("toy")).unwrap();
    std::fs::write(models.path().join("toy/config.json"), "{}").unwrap();
    std::fs::write(models.path().join("toy/model.safetensors"), "weights").unwrap();
    let mut host = source["host"].clone();
    host["model_store"]["path"] = json!(models.path());
    let mut deployment = source["deployment"].clone();
    deployment["model"]["path"] = json!("toy");
    edit(&mut deployment);
    let state = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let owner = Arc::new(Mutex::new(
        OwnedCoordinatorState::open(state.path()).unwrap(),
    ));
    let fence = {
        let o = owner.lock().unwrap();
        let session = o.session().clone();
        let policy =
            capyctl_config::effective::resolve_effective(&source["deployment"], &source["host"])
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
                "toy",
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
        models,
        owner,
        fence,
    }
}

fn record_of(f: &Fixture) -> capyctl_store::checkpoint_digests::CheckpointDigest {
    f.owner
        .lock()
        .unwrap()
        .store()
        .checkpoint_digest(&f.fence.deployment_id, f.fence.revision)
        .unwrap()
        .unwrap()
}

struct Scripted {
    reachable: bool,
    answer: Result<Measured, MeasureError>,
    calls: AtomicUsize,
}
impl DigestSource for Scripted {
    fn reachable(&self, _: &str) -> bool {
        self.reachable
    }
    fn measure(&self, _: PendingDigest) -> MeasureFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let answer = self.answer.clone();
        Box::pin(async move { answer })
    }
}

async fn run(supervisor: &Arc<CheckpointDigests>) {
    for task in supervisor.clone().pass() {
        task.await.unwrap();
    }
}

// T14 T34: the supervisor records what the host measured; an offline host is
// not asked; a refusal keeps the digest pending with its closed reason and
// backs off rather than asking again at once.
#[tokio::test]
async fn the_supervisor_records_measurements_and_backs_off_after_refusals() {
    let f = fixture(|_| {});
    let digest = format!("sha256:{}", "1".repeat(64));
    let offline = Arc::new(Scripted {
        reachable: false,
        answer: Err(MeasureError::Unavailable),
        calls: AtomicUsize::new(0),
    });
    run(&CheckpointDigests::new(f.owner.clone(), offline.clone())).await;
    assert_eq!(offline.calls.load(Ordering::SeqCst), 0);
    let refusing = Arc::new(Scripted {
        reachable: true,
        answer: Err(MeasureError::Refused("unsafe_file".into())),
        calls: AtomicUsize::new(0),
    });
    let supervisor = CheckpointDigests::new(f.owner.clone(), refusing.clone());
    run(&supervisor).await;
    run(&supervisor).await;
    assert_eq!(refusing.calls.load(Ordering::SeqCst), 1, "backed off");
    let record = record_of(&f);
    assert_eq!(record.state, DigestState::Pending);
    assert_eq!(record.diagnostic.as_deref(), Some("unsafe_file"));
    let measuring = Arc::new(Scripted {
        reachable: true,
        answer: Ok(Measured {
            digest: digest.clone(),
            weights_bytes: 7,
        }),
        calls: AtomicUsize::new(0),
    });
    run(&CheckpointDigests::new(f.owner.clone(), measuring.clone())).await;
    let record = record_of(&f);
    assert_eq!(record.state, DigestState::Recorded);
    assert_eq!(record.digest.as_deref(), Some(digest.as_str()));
    // Nothing is pending any more.
    run(&CheckpointDigests::new(f.owner.clone(), measuring.clone())).await;
    assert_eq!(measuring.calls.load(Ordering::SeqCst), 1);
}

// T22: the embedded host measures with the same verifier a remote agent uses.
#[tokio::test]
async fn the_embedded_source_measures_the_local_checkpoint() {
    let f = fixture(|_| {});
    let checkpoints = Arc::new(CheckpointVerifier::in_memory());
    run(&CheckpointDigests::new(
        f.owner.clone(),
        LocalDigests::new(checkpoints.clone()),
    ))
    .await;
    let expected = checkpoints
        .measure(f.models.path(), &f.models.path().join("toy"))
        .unwrap()
        .manifest;
    let record = record_of(&f);
    assert_eq!(record.state, DigestState::Recorded);
    assert_eq!(record.digest, Some(expected.digest));
    assert_eq!(record.weights_bytes, Some(expected.weights_bytes));
}

/// Sizes first, then hashes: the sizing is recorded while the full digest has
/// not answered yet.
struct SizedFirst {
    owner: SharedCoordinatorState,
    deployment: String,
    revision: i64,
    seen_during_measure: Mutex<Option<Option<i64>>>,
}
impl DigestSource for SizedFirst {
    fn reachable(&self, _: &str) -> bool {
        true
    }
    fn size(&self, _: PendingDigest) -> SizeFuture {
        Box::pin(async { Ok(7) })
    }
    fn measure(&self, _: PendingDigest) -> MeasureFuture {
        // What a start accepted now would see: the sized weights, digest pending.
        let pending = self
            .owner
            .lock()
            .unwrap()
            .store()
            .pending_checkpoint_digests()
            .unwrap()
            .into_iter()
            .find(|p| p.deployment_id == self.deployment && p.revision == self.revision)
            .map(|p| p.weights_bytes);
        *self.seen_during_measure.lock().unwrap() = pending;
        Box::pin(async { Err(MeasureError::Unavailable) })
    }
}

// T29 T34, owner decision 2026-09-23 (solo first start): the supervisor sizes
// a pending checkpoint (a stat walk) before hashing it, and records the
// weights at once, so a first start's startup estimate uses them long before
// the digest exists. The embedded source sizes with the same confined walk.
#[tokio::test]
async fn the_supervisor_records_sized_weights_before_the_digest() {
    let f = fixture(|_| {});
    let source = Arc::new(SizedFirst {
        owner: f.owner.clone(),
        deployment: f.fence.deployment_id.clone(),
        revision: f.fence.revision,
        seen_during_measure: Mutex::new(None),
    });
    run(&CheckpointDigests::new(f.owner.clone(), source.clone())).await;
    assert_eq!(*source.seen_during_measure.lock().unwrap(), Some(Some(7)));
    assert_eq!(record_of(&f).state, DigestState::Pending);

    let checkpoints = Arc::new(CheckpointVerifier::in_memory());
    let pending = f
        .owner
        .lock()
        .unwrap()
        .store()
        .pending_checkpoint_digests()
        .unwrap()
        .remove(0);
    let sized = LocalDigests::new(checkpoints.clone())
        .size(pending)
        .await
        .unwrap();
    let expected = checkpoints
        .measure(f.models.path(), &f.models.path().join("toy"))
        .unwrap()
        .manifest;
    assert_eq!(sized, expected.weights_bytes);
}

/// Counts the engine calls a gate lets through.
#[derive(Default)]
struct Counting(AtomicUsize);
#[async_trait::async_trait]
impl EngineAdapter for Counting {
    async fn execute_persisted(
        &self,
        _: &RuntimeCommand,
    ) -> Result<EffectObservation, RuntimeError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(RuntimeError::Unsupported)
    }
    async fn inspect(&self, _: &MemberRef) -> Result<EngineState, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn render_plan(&self, _: &PlanInput) -> Result<RenderedCommand, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn check_readiness(&self, _: &MemberRef) -> Result<Readiness, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn prepare_park(&self, _: &MemberRef) -> Result<Quiescence, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn park(&self, _: &MemberRef, _: ParkLevel) -> Result<ParkOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn restore(&self, _: &MemberRef) -> Result<RestoreOutcome, AdapterError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(AdapterError::UnsupportedCapability)
    }
    async fn reload_weights(&self, _: &MemberRef) -> Result<ReloadOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn observe_work(&self, _: &MemberRef) -> Result<WorkObservation, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn cancel_work(
        &self,
        _: &MemberRef,
        _: &RequestRef,
        _: bool,
    ) -> Result<CancellationOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
}

fn step(f: &Fixture, action: RuntimeAction) -> RuntimeCommand {
    RuntimeCommand {
        action,
        context: StepExecutionContext {
            token: TransitionToken {
                deployment_id: f.fence.deployment_id.clone(),
                revision: f.fence.revision,
                generation: f.fence.generation,
                operation_id: "operation".into(),
                step_id: "step".into(),
            },
            binding_id: "binding".into(),
            incarnation: "incarnation".into(),
            issued_at_ms: 1,
            deadline_ms: 2,
            identities: ExecutionIdentities::OwnedLaunch,
            completion_target: None,
            grant_id: None,
            launch_settings: None,
        },
    }
}

// T34 T22: the embedded launch records the digest on first placement, then
// refuses a checkpoint changed under it, before the engine is asked anything,
// at launch and at wake alike.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_embedded_gate_verifies_before_initialize_and_restore() {
    let f = fixture(|_| {});
    let work = {
        let o = f.owner.lock().unwrap();
        o.store()
            .accept_start(o.session(), &f.fence, 100, 100_100)
            .unwrap();
        o.store().next_initialize(o.session()).unwrap().unwrap()
    };
    let inner = Arc::new(Counting::default());
    let gate = CheckpointGate::new(
        inner.clone(),
        f.owner.clone(),
        Arc::new(CheckpointVerifier::in_memory()),
        &work,
    );
    let _ = gate
        .execute_persisted(&step(&f, RuntimeAction::Initialize))
        .await;
    assert_eq!(
        inner.0.load(Ordering::SeqCst),
        1,
        "a verified launch proceeds"
    );
    assert_eq!(record_of(&f).state, DigestState::Recorded);
    let member = MemberRef {
        deployment_id: f.fence.deployment_id.clone(),
        member_id: "binding".into(),
    };
    let _ = gate.restore(&member).await;
    assert_eq!(
        inner.0.load(Ordering::SeqCst),
        2,
        "a verified wake proceeds"
    );
    std::fs::write(f.models.path().join("toy/model.safetensors"), "swapped").unwrap();
    let refused = gate
        .execute_persisted(&step(&f, RuntimeAction::Initialize))
        .await
        .unwrap_err();
    assert!(
        matches!(refused, RuntimeError::Uncertain(ref text) if text.contains("recorded digest"))
    );
    assert!(gate
        .execute_persisted(&step(&f, RuntimeAction::Restore))
        .await
        .is_err());
    assert!(gate.restore(&member).await.is_err());
    assert_eq!(
        inner.0.load(Ordering::SeqCst),
        2,
        "nothing reached the engine"
    );
}

/// Remove the revision's digest row, as for a revision accepted before WE3
/// (schema v20), whose launches were parked without any digest.
fn make_legacy(f: &Fixture) {
    let sql = rusqlite::Connection::open(f._state.path().join("srv.sqlite3")).unwrap();
    sql.execute(
        "DELETE FROM checkpoint_digests WHERE deployment_id=?1 AND revision=?2",
        rusqlite::params![f.fence.deployment_id, f.fence.revision],
    )
    .unwrap();
    assert!(f
        .owner
        .lock()
        .unwrap()
        .store()
        .checkpoint_digest(&f.fence.deployment_id, f.fence.revision)
        .unwrap()
        .is_none());
}

/// A declared canonical content fingerprint the fixture's checkpoint does not
/// measure to.
fn declared_other(deployment: &mut Value) {
    deployment["model"]["content_fingerprint"] = json!(format!("sha256:{}", "9".repeat(64)));
}

// T14 T15 T33: owner decision 5. Waking a launch parked before digests
// existed first measures its checkpoint on the host and records the digest
// under the same validation as a first placement, then wakes with it. A
// recorded digest is carried without measuring again.
#[tokio::test]
async fn a_legacy_wake_measures_and_records_the_digest_first() {
    let f = fixture(|_| {});
    make_legacy(&f);
    let digest = format!("sha256:{}", "2".repeat(64));
    let calls = AtomicUsize::new(0);
    let measure = || {
        calls.fetch_add(1, Ordering::SeqCst);
        let digest = digest.clone();
        async move {
            Ok(Measured {
                digest,
                weights_bytes: 7,
            })
        }
    };
    let woken = wake_digest(
        &f.owner,
        &f.fence.deployment_id,
        f.fence.revision,
        "lab",
        measure,
    )
    .await
    .unwrap();
    assert_eq!(woken, digest);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let record = record_of(&f);
    assert_eq!(record.state, DigestState::Recorded);
    assert_eq!(record.digest.as_deref(), Some(digest.as_str()));
    // Recorded now: the next wake carries it without asking the host.
    let again = wake_digest(
        &f.owner,
        &f.fence.deployment_id,
        f.fence.revision,
        "lab",
        || async { Err::<Measured, _>(MeasureError::Unavailable) },
    )
    .await
    .unwrap();
    assert_eq!(again, digest);
}

// T14 T15: a measurement that is not the declared canonical fingerprint is a
// mismatch: the wake is refused with `checkpoint_mismatch` before anything is
// sent, and the launch stays parked. A host that cannot measure refuses the
// wake without effect and leaves the digest unrecorded.
#[tokio::test]
async fn a_legacy_wake_is_refused_on_mismatch_or_without_a_measurement() {
    let f = fixture(declared_other);
    make_legacy(&f);
    let unmeasured = wake_digest(
        &f.owner,
        &f.fence.deployment_id,
        f.fence.revision,
        "lab",
        || async { Err::<Measured, _>(MeasureError::Unavailable) },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(unmeasured, RuntimeError::Unsupported),
        "{unmeasured:?}"
    );
    let refused = wake_digest(
        &f.owner,
        &f.fence.deployment_id,
        f.fence.revision,
        "lab",
        || async {
            Ok(Measured {
                digest: format!("sha256:{}", "2".repeat(64)),
                weights_bytes: 7,
            })
        },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(refused, RuntimeError::Refused(ref reason) if reason == "checkpoint_mismatch"),
        "{refused:?}"
    );
    assert_eq!(record_of(&f).state, DigestState::Mismatch);
}

// T14 T15 T33: a first placement whose checkpoint does not measure to the
// declared canonical fingerprint is refused with `checkpoint_mismatch` before
// the launch is sent, not reported as uncertain ownership (M48 soak,
// 2026-09-24). A matching measurement is recorded and launched with; a host
// that cannot measure leaves the digest unrecorded and the launch uncertain.
#[tokio::test]
async fn a_first_placement_mismatch_is_refused_as_checkpoint_mismatch() {
    let f = fixture(declared_other);
    let refused = first_placement_digest(
        &f.owner,
        &f.fence.deployment_id,
        f.fence.revision,
        "lab",
        || async {
            Ok(Measured {
                digest: format!("sha256:{}", "2".repeat(64)),
                weights_bytes: 7,
            })
        },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(refused, RuntimeError::Refused(ref reason) if reason == "checkpoint_mismatch"),
        "{refused:?}"
    );
    assert_eq!(record_of(&f).state, DigestState::Mismatch);

    let f = fixture(|_| {});
    let unmeasured = first_placement_digest(
        &f.owner,
        &f.fence.deployment_id,
        f.fence.revision,
        "lab",
        || async { Err::<Measured, _>(MeasureError::Unavailable) },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(unmeasured, RuntimeError::Uncertain(_)),
        "{unmeasured:?}"
    );
    assert_ne!(record_of(&f).state, DigestState::Recorded);
    let digest = format!("sha256:{}", "2".repeat(64));
    let placed = first_placement_digest(
        &f.owner,
        &f.fence.deployment_id,
        f.fence.revision,
        "lab",
        || {
            let digest = digest.clone();
            async move {
                Ok(Measured {
                    digest,
                    weights_bytes: 7,
                })
            }
        },
    )
    .await
    .unwrap();
    assert_eq!(placed, digest);
    assert_eq!(record_of(&f).state, DigestState::Recorded);
}

// T14 T15 T33: the embedded gate wakes a legacy launch after measuring and
// recording its digest, and refuses a mismatching one with
// `checkpoint_mismatch` before the engine is asked anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_embedded_gate_wakes_a_legacy_launch_only_on_a_recorded_match() {
    for (edit, matches) in [
        (None, true),
        (Some(declared_other as fn(&mut Value)), false),
    ] {
        let f = fixture(|deployment| {
            if let Some(edit) = edit {
                edit(deployment)
            }
        });
        let work = {
            let o = f.owner.lock().unwrap();
            o.store()
                .accept_start(o.session(), &f.fence, 100, 100_100)
                .unwrap();
            o.store().next_initialize(o.session()).unwrap().unwrap()
        };
        make_legacy(&f);
        let inner = Arc::new(Counting::default());
        let gate = CheckpointGate::new(
            inner.clone(),
            f.owner.clone(),
            Arc::new(CheckpointVerifier::in_memory()),
            &work,
        );
        let woken = gate
            .execute_persisted(&step(&f, RuntimeAction::Restore))
            .await;
        if matches {
            assert_eq!(inner.0.load(Ordering::SeqCst), 1, "the wake proceeds");
            assert_eq!(record_of(&f).state, DigestState::Recorded);
        } else {
            assert!(
                matches!(woken, Err(RuntimeError::Refused(ref reason)) if reason == "checkpoint_mismatch"),
                "{woken:?}"
            );
            assert_eq!(
                inner.0.load(Ordering::SeqCst),
                0,
                "nothing reached the engine"
            );
            assert_eq!(record_of(&f).state, DigestState::Mismatch);
        }
    }
}

/// A host that never finishes a measurement, and says when one was dropped.
struct Hanging {
    started: tokio::sync::Semaphore,
    dropped: Arc<std::sync::atomic::AtomicBool>,
}
impl DigestSource for Hanging {
    fn reachable(&self, _: &str) -> bool {
        true
    }
    fn measure(&self, _: PendingDigest) -> MeasureFuture {
        struct Flag(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        self.started.add_permits(1);
        let flag = Flag(self.dropped.clone());
        Box::pin(async move {
            let _flag = flag;
            std::future::pending().await
        })
    }
}

// ADR 0014 §7 (review finding 15): a measurement belongs to its supervisor.
// Aborting the supervisor (role shutdown) aborts the measurement, which then
// no longer holds the owned coordinator state; the digest stays pending.
// T34
#[tokio::test]
async fn an_aborted_supervisor_aborts_its_measurements() {
    let f = fixture(|_| {});
    let source = Arc::new(Hanging {
        started: tokio::sync::Semaphore::new(0),
        dropped: Default::default(),
    });
    let handle = CheckpointDigests::new(f.owner.clone(), source.clone()).spawn();
    tokio::time::timeout(Duration::from_secs(10), source.started.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    handle.abort();
    let _ = handle.await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !source.dropped.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the measurement outlived its supervisor");
    assert_eq!(record_of(&f).state, DigestState::Pending);
}

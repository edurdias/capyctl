use super::*;
use mllm_controller::coordinator::{CoordinatorOptions, OwnedCoordinator, ServiceObservation};
use mllm_controller::ownership::{OwnedCoordinatorState, SharedCoordinatorState};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// A real completed, promoted and cleaned-up qualification, copied without
// modifying its evidence. Ownership is acquired before accepting ordinary work.
async fn owned_fixture() -> (
    tempfile::TempDir,
    SharedCoordinatorState,
    mllm_store::lifecycle::DeploymentFence,
    Vec<MemoryObservation>,
) {
    use std::os::unix::fs::PermissionsExt;
    let source = fixture_support::owned_source().await;
    let fence = source.fence.clone();
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().join("srv.sqlite3");
    std::fs::copy(source.dir.path().join("srv.sqlite3"), &path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let owner = Arc::new(Mutex::new(OwnedCoordinatorState::open(dir.path()).unwrap()));
    (dir, owner, fence, source.observations.clone())
}

struct Observations(Vec<MemoryObservation>);
impl ServiceObservation for Observations {
    fn observe(&self, _: String) -> mllm_controller::coordinator::ObservationFuture {
        let values = self.0.clone();
        Box::pin(async move { Ok(values) })
    }
}

#[tokio::test]
async fn owned_worker_initializes_once_for_joined_and_dropped_observers() {
    let (_dir, owner, fence, observations) = owned_fixture().await;
    let worker = OwnedCoordinator::spawn_fake(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(|| Ok(1900)),
        CoordinatorOptions::default(),
    )
    .unwrap();
    let a = worker.start(&fence, 10000).unwrap();
    let b = worker.start(&fence, 10000).unwrap();
    assert_eq!(a.operation_id(), b.operation_id());
    let step = a.step_id().to_owned();
    drop(a);
    drop(b);
    // Parallel qualification fixtures can occupy the CPU while this worker
    // validates durable provenance. This is a test hang detector; the service
    // clock and persisted operation deadline remain 1900 and 10000 below.
    let ready_wait_started = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let done = {
                let state = owner.lock().unwrap();
                state
                    .store()
                    .runtime_binding(&fence.deployment_id)
                    .unwrap()
                    .is_some_and(|b| b.state == "live")
            };
            if done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "owned worker did not reach Ready after {:?}: {error}",
            ready_wait_started.elapsed()
        )
    });
    eprintln!(
        "owned worker Ready wait: {:?}",
        ready_wait_started.elapsed()
    );
    {
        let state = owner.lock().unwrap();
        assert_eq!(
            state.store().resource_snapshot().unwrap().owners[&fence.deployment_id].phase,
            ResourcePhase::Ready
        );
        let context = state
            .store()
            .qualified_initialize_execution(state.session(), &step)
            .unwrap();
        assert_eq!(context.deadline_ms, 10000);
    }
    worker.shutdown().await.unwrap();
}

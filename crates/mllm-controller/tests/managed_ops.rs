//! Managed operations on the REAL launcher (T12 mechanics through the
//! controller) and administrative-stop vs idle-stop semantics (T10).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mllm_adapters::traits::{
    AdapterError, CancellationOutcome, EngineAdapter, MemberRef, ParkLevel,
    ParkOutcome, Phase, PlanInput, Readiness, ReloadOutcome, RenderedCommand, RequestRef,
    RestoreOutcome, WorkObservation,
};
use mllm_controller::{Controller, DeployRequest};
use mllm_domain::LifecycleState;
use mllm_launchers::ExecLauncher;
use mllm_store::Store;

/// A "process-backed" test adapter: readiness is deterministic, but the
/// rendered command is a real long-lived process, so the controller's
/// managed path exercises the real launcher end-to-end.
struct ProcAdapter;

#[async_trait]
impl EngineAdapter for ProcAdapter {
    async fn inspect(&self, _: &MemberRef) -> Result<mllm_adapters::traits::EngineState, AdapterError> {
        Ok(mllm_adapters::traits::EngineState {
            phase: Phase::Ready,
            retained_bytes: 0,
            build_fingerprint: Some("proc-adapter".into()),
        })
    }
    async fn render_plan(&self, _plan: &PlanInput) -> Result<RenderedCommand, AdapterError> {
        Ok(RenderedCommand {
            argv: vec!["sleep".into(), "30".into()],
            env: Default::default(),
        })
    }
    async fn check_readiness(&self, _: &MemberRef) -> Result<Readiness, AdapterError> {
        Ok(Readiness::Ready)
    }
    async fn prepare_park(&self, _: &MemberRef) -> Result<mllm_adapters::traits::Quiescence, AdapterError> {
        Ok(mllm_adapters::traits::Quiescence { quiescent: true })
    }
    async fn park(&self, _: &MemberRef, _: ParkLevel) -> Result<ParkOutcome, AdapterError> {
        Ok(ParkOutcome::Parked { retained_bytes: 0 })
    }
    async fn restore(&self, _: &MemberRef) -> Result<RestoreOutcome, AdapterError> {
        Ok(RestoreOutcome::Restored)
    }
    async fn reload_weights(&self, _: &MemberRef) -> Result<ReloadOutcome, AdapterError> {
        Ok(ReloadOutcome::Reloaded)
    }
    async fn observe_work(&self, _: &MemberRef) -> Result<WorkObservation, AdapterError> {
        Ok(WorkObservation::Idle)
    }
    async fn cancel_work(
        &self,
        _: &MemberRef,
        _: &RequestRef,
        _: bool,
    ) -> Result<CancellationOutcome, AdapterError> {
        Ok(CancellationOutcome::Uncertain)
    }
}

fn controller() -> (Arc<Controller>, Arc<Mutex<Store>>) {
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let c = Arc::new(Controller::new(
        store.clone(),
        Arc::new(ProcAdapter),
        Arc::new(ExecLauncher::new()),
    ));
    (c, store)
}

fn req(name: &str) -> DeployRequest {
    DeployRequest {
        name: name.into(),
        kind: "model".into(),
        manifest: format!(r#"{{"kind":"model","name":"{name}"}}"#).into_bytes(),
        route_model_id: Some(name.into()),
    }
}

#[tokio::test]
async fn start_spawns_real_process_and_stop_terminates_it() {
    let (c, _store) = controller();
    let id = c.submit_deploy(req("proc-m")).await.unwrap();
    let op = c.request_transition(&id, mllm_domain::LifecycleAction::Start).await.unwrap();
    c.wait_terminal(&op).await.unwrap();

    let pid = c.live_pid(&id).expect("real process spawned");
    assert!(std::path::Path::new(&format!("/proc/{pid}")).exists());

    let op2 = c.request_transition(&id, mllm_domain::LifecycleAction::Stop).await.unwrap();
    c.wait_terminal(&op2).await.unwrap();
    assert!(c.live_pid(&id).is_none(), "handle released");
    // The process group was terminated: give the kernel a beat.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "engine process terminated (T12)"
    );
}

#[tokio::test]
async fn administrative_stop_blocks_autoactivation() {
    let (c, store) = controller();
    let id = c.submit_deploy(req("admin-m")).await.unwrap();
    let op = c.request_transition(&id, mllm_domain::LifecycleAction::Start).await.unwrap();
    c.wait_terminal(&op).await.unwrap();
    let op2 = c.request_transition(&id, mllm_domain::LifecycleAction::Stop).await.unwrap();
    c.wait_terminal(&op2).await.unwrap();

    // Administrative stop suspends: autoactivation is blocked (T10).
    assert!(store.lock().unwrap().is_suspended(&id).unwrap());
    let res = c.request_transition(&id, mllm_domain::LifecycleAction::Start).await;
    assert!(res.is_err(), "suspended deployment rejects activation Start");
}

#[tokio::test]
async fn idle_stop_leaves_on_demand_eligible() {
    let (c, store) = controller();
    let id = c.submit_deploy(req("idle-m")).await.unwrap();
    let op = c.request_transition(&id, mllm_domain::LifecycleAction::Start).await.unwrap();
    c.wait_terminal(&op).await.unwrap();

    // Idle eviction: stop the engine but keep the deployment on-demand
    // eligible (T10's other half).
    let op2 = c.idle_stop(&id).await.unwrap();
    c.wait_terminal(&op2).await.unwrap();
    assert_eq!(
        store.lock().unwrap().get_deployment(&id).unwrap().unwrap().observed_state,
        LifecycleState::Stopped
    );
    assert!(!store.lock().unwrap().is_suspended(&id).unwrap(), "on-demand eligible");

    // A later Start succeeds (explicit activation after idle eviction).
    let op3 = c.request_transition(&id, mllm_domain::LifecycleAction::Start).await.unwrap();
    c.wait_terminal(&op3).await.unwrap();
}
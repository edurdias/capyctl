//! What the inference router needs from the lifecycle authority.
//!
//! The router named a concrete `Controller`, which is why the coordinator could not
//! be substituted for it. This extracts the surface the router actually calls — four
//! methods — so the authority becomes a choice made once at wiring time.
//!
//! The surface is deliberately identical to what the router calls today. Narrowing
//! it is a separate decision: `idle_stop` and `wait_terminal` together are the
//! router-owned eviction the A2d plan requires be removed at cutover, because
//! selecting a victim and driving its transition is the authority's job, not the
//! router's. Preserving that here is a mechanical extraction, not an endorsement —
//! doing both at once would hide a behaviour change inside a refactor.

use async_trait::async_trait;
use mllm_adapters::traits::WorkObservation;
use mllm_domain::LifecycleState;
use mllm_store::deployments::{DeploymentRow, OperationRow};
use crate::fault::LifecycleFault;

use crate::operations::{Controller, DeployRequest, OperationHandle};
use mllm_domain::LifecycleAction;

#[async_trait]
pub trait LifecyclePort: Send + Sync {
    // Reads. Named projections rather than a store handle: the coordinator owns
    // its store exclusively behind the controller lock, so an authority cannot
    // hand one out, and a handle hides which state the router actually depends on.

    /// Resolve a public route to its deployment.
    fn find_deployment_by_route(&self, route: &str) -> Result<Option<DeploymentRow>, LifecycleFault>;

    /// Public route ids currently eligible for admission.
    fn list_enabled_route_ids(&self) -> Result<Vec<String>, LifecycleFault>;

    /// One deployment's current record.
    fn get_deployment(&self, id: &str) -> Result<Option<DeploymentRow>, LifecycleFault>;

    /// The most recent operation accepted for a deployment.
    fn latest_operation(&self, deployment_id: &str) -> Result<Option<OperationRow>, LifecycleFault>;

    /// Deployments currently READY other than this one. One exclusive pool means
    /// any other READY deployment holds it and must be released first.
    fn ready_deployments_excluding(&self, deployment: &str) -> Result<Vec<String>, LifecycleFault>;

    // Writes. These exist only because the router currently drives eviction: it
    // selects a victim, stops it, and clears its suspension itself. That is the
    // authority's job, and the A2d plan requires it be removed at cutover. They are
    // listed here rather than hidden behind a store handle so the coupling is
    // visible and its removal is a reviewable deletion.

    /// Clear a suspension the router set while switching. To be removed with
    /// router-owned eviction.
    fn clear_suspension(&self, deployment: &str) -> Result<(), LifecycleFault>;

    /// Journal a switch outcome. To be removed with router-owned eviction.
    fn journal(
        &self,
        host_id: Option<&str>,
        operation_id: Option<&str>,
        state: Option<&str>,
        evidence: &str,
    ) -> Result<(), LifecycleFault>;

    /// What the engine can prove about work in flight for this deployment.
    async fn observe_adapter(
        &self,
        deployment: &str,
    ) -> Result<WorkObservation, LifecycleFault>;

    /// Request an idle stop. Distinct from an administrative stop: the deployment
    /// stays eligible for on-demand activation afterwards.
    async fn idle_stop(&self, deployment: &str) -> Result<OperationHandle, LifecycleFault>;

    /// Await a terminal state for an accepted operation. A caller giving up here
    /// never cancels the operation.
    async fn wait_terminal(
        &self,
        handle: &OperationHandle,
    ) -> Result<LifecycleState, LifecycleFault>;

    /// Activate on demand for an arriving request. Distinct from an administrative
    /// start: it must refuse a deployment that was explicitly stopped, so the next
    /// inference cannot undo that decision.
    async fn auto_activate(&self, deployment: &str) -> Result<OperationHandle, LifecycleFault>;

    /// Request an administrative transition.
    async fn request_transition(
        &self,
        deployment: &str,
        action: LifecycleAction,
    ) -> Result<OperationHandle, LifecycleFault>;

    /// Accept a deployment durably and return its id.
    async fn submit_deploy(&self, req: DeployRequest) -> Result<String, LifecycleFault>;
}

#[async_trait]
impl LifecyclePort for Controller {
    fn find_deployment_by_route(&self, route: &str) -> Result<Option<DeploymentRow>, LifecycleFault> {
        self.store_ref().lock().unwrap().find_deployment_by_route(route).map_err(Into::into)
    }
    fn list_enabled_route_ids(&self) -> Result<Vec<String>, LifecycleFault> {
        self.store_ref().lock().unwrap().list_enabled_route_ids().map_err(Into::into)
    }
    fn get_deployment(&self, id: &str) -> Result<Option<DeploymentRow>, LifecycleFault> {
        self.store_ref().lock().unwrap().get_deployment(id).map_err(Into::into)
    }
    fn latest_operation(&self, deployment_id: &str) -> Result<Option<OperationRow>, LifecycleFault> {
        self.store_ref().lock().unwrap().latest_operation(deployment_id).map_err(Into::into)
    }
    fn ready_deployments_excluding(&self, deployment: &str) -> Result<Vec<String>, LifecycleFault> {
        self.store_ref()
            .lock()
            .unwrap()
            .ready_deployments_excluding(deployment).map_err(Into::into)
    }
    fn clear_suspension(&self, deployment: &str) -> Result<(), LifecycleFault> {
        self.store_ref().lock().unwrap().set_suspended(deployment, false).map_err(Into::into)
    }
    fn journal(
        &self,
        host_id: Option<&str>,
        operation_id: Option<&str>,
        state: Option<&str>,
        evidence: &str,
    ) -> Result<(), LifecycleFault> {
        self.store_ref()
            .lock()
            .unwrap()
            .record_journal(host_id, operation_id, state, evidence).map_err(Into::into)
    }
    async fn observe_adapter(
        &self,
        deployment: &str,
    ) -> Result<WorkObservation, LifecycleFault> {
        Controller::observe_adapter(self, deployment).await.map_err(Into::into)
    }
    async fn idle_stop(&self, deployment: &str) -> Result<OperationHandle, LifecycleFault> {
        Controller::idle_stop(self, deployment).await.map_err(Into::into)
    }
    async fn wait_terminal(
        &self,
        handle: &OperationHandle,
    ) -> Result<LifecycleState, LifecycleFault> {
        Controller::wait_terminal(self, handle).await.map_err(Into::into)
    }
    async fn auto_activate(&self, deployment: &str) -> Result<OperationHandle, LifecycleFault> {
        Controller::auto_activate(self, deployment).await.map_err(Into::into)
    }
    async fn request_transition(
        &self,
        deployment: &str,
        action: LifecycleAction,
    ) -> Result<OperationHandle, LifecycleFault> {
        Controller::request_transition(self, deployment, action).await.map_err(Into::into)
    }
    async fn submit_deploy(&self, req: DeployRequest) -> Result<String, LifecycleFault> {
        Controller::submit_deploy(self, req).await.map_err(Into::into)
    }
}

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

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mllm_adapters::traits::{AdapterError, WorkObservation};
use mllm_domain::LifecycleState;
use mllm_store::Store;

use crate::operations::{Controller, ControllerError, DeployRequest, OperationHandle};
use mllm_domain::LifecycleAction;

#[async_trait]
pub trait LifecyclePort: Send + Sync {
    /// Shared store handle. The router reads deployment and route state through it.
    fn store(&self) -> Arc<Mutex<Store>>;

    /// What the engine can prove about work in flight for this deployment.
    async fn observe_adapter(
        &self,
        deployment: &str,
    ) -> Result<WorkObservation, AdapterError>;

    /// Request an idle stop. Distinct from an administrative stop: the deployment
    /// stays eligible for on-demand activation afterwards.
    async fn idle_stop(&self, deployment: &str) -> Result<OperationHandle, ControllerError>;

    /// Await a terminal state for an accepted operation. A caller giving up here
    /// never cancels the operation.
    async fn wait_terminal(
        &self,
        handle: &OperationHandle,
    ) -> Result<LifecycleState, ControllerError>;

    /// Activate on demand for an arriving request. Distinct from an administrative
    /// start: it must refuse a deployment that was explicitly stopped, so the next
    /// inference cannot undo that decision.
    async fn auto_activate(&self, deployment: &str) -> Result<OperationHandle, ControllerError>;

    /// Request an administrative transition.
    async fn request_transition(
        &self,
        deployment: &str,
        action: LifecycleAction,
    ) -> Result<OperationHandle, ControllerError>;

    /// Accept a deployment durably and return its id.
    async fn submit_deploy(&self, req: DeployRequest) -> Result<String, ControllerError>;
}

#[async_trait]
impl LifecyclePort for Controller {
    fn store(&self) -> Arc<Mutex<Store>> {
        self.store_ref()
    }
    async fn observe_adapter(
        &self,
        deployment: &str,
    ) -> Result<WorkObservation, AdapterError> {
        Controller::observe_adapter(self, deployment).await
    }
    async fn idle_stop(&self, deployment: &str) -> Result<OperationHandle, ControllerError> {
        Controller::idle_stop(self, deployment).await
    }
    async fn wait_terminal(
        &self,
        handle: &OperationHandle,
    ) -> Result<LifecycleState, ControllerError> {
        Controller::wait_terminal(self, handle).await
    }
    async fn auto_activate(&self, deployment: &str) -> Result<OperationHandle, ControllerError> {
        Controller::auto_activate(self, deployment).await
    }
    async fn request_transition(
        &self,
        deployment: &str,
        action: LifecycleAction,
    ) -> Result<OperationHandle, ControllerError> {
        Controller::request_transition(self, deployment, action).await
    }
    async fn submit_deploy(&self, req: DeployRequest) -> Result<String, ControllerError> {
        Controller::submit_deploy(self, req).await
    }
}

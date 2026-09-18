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

use crate::operations::{Controller, OperationHandle};
use mllm_domain::LifecycleAction;

/// Where a deployment's running engine can be reached, and as what.
///
/// Every field belongs to one launch rather than to the deployment. The endpoint is
/// a leased port, the key is minted fresh per launch, and the served name comes from
/// the revision that launch froze. `incarnation` names the launch the other three
/// were read from, so a caller can tell a cached answer apart from a current one
/// instead of discovering the difference as a connection refused or a 401.
///
/// SPEC §13.3: the key authenticates to that engine and nothing else. No Debug is
/// derived, deliberately — a credential that can be formatted is a credential that
/// reaches a log.
#[derive(Clone, PartialEq, Eq)]
pub struct RuntimeEndpoint {
    /// The base URL the engine was leased, without a path.
    pub endpoint: String,
    /// The model name the engine was launched to answer to, which is not
    /// necessarily the public alias a client asked for.
    pub served_model: String,
    /// The key that launch was given, hex-encoded as the engine received it.
    /// `None` when the runtime was started without one.
    pub engine_key: Option<String>,
    /// The launch the three fields above were read from.
    pub incarnation: String,
}

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

    /// Where this deployment's engine is running now, or `None` when no runtime is
    /// retained for it.
    ///
    /// SPEC §3: a port is leased per launch and a key is minted per launch, so this
    /// is a read the caller must repeat rather than a table it may build once. `None`
    /// is "nothing is running", which is a different answer from a failure to look:
    /// a caller that cannot distinguish them would report an unstarted deployment as
    /// a broken one.
    fn runtime_endpoint(
        &self,
        deployment: &str,
    ) -> Result<Option<RuntimeEndpoint>, LifecycleFault>;

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
    fn runtime_endpoint(
        &self,
        _deployment: &str,
    ) -> Result<Option<RuntimeEndpoint>, LifecycleFault> {
        // The F1 controller never launches an engine of its own: it drives an adapter
        // it was handed at construction, so there is no leased endpoint or per-launch
        // key for it to report. `None` is the truthful answer — nothing is retained —
        // and it is what keeps this authority from claiming a runtime it does not own.
        Ok(None)
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
}

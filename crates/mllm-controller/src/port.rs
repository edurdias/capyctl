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

use crate::fault::LifecycleFault;
use async_trait::async_trait;
use mllm_adapters::traits::WorkObservation;
use mllm_domain::LifecycleState;
use mllm_store::deployments::{DeploymentRow, OperationRow};

use crate::operations::{Controller, ControllerError, OperationHandle};
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

/// ADR 0013 §10 (unit I3): one instance of a deployment that holds a runtime,
/// with what the router's choice depends on.
///
/// Everything here is a hint for ranking. The lease grant re-checks the gate in
/// its own transaction, and the forwarder is resolved for exactly `generation`,
/// so a stale hint can cost a failover but never a request sent to the wrong
/// incarnation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServingInstance {
    pub instance_index: u32,
    /// Identifies this incarnation alone (ADR 0013 §5); fences its lease.
    pub generation: i64,
    /// The instance's placed host, for logs.
    pub host_id: Option<String>,
    /// The enrolled host whose ingress serves it; `None` for an engine the
    /// embedded role runs itself.
    pub remote_host: Option<String>,
    /// The launch the host reports this instance's load under.
    pub launch_command_id: Option<String>,
    /// Ready, admission and dispatch open, deployment not suspended.
    pub dispatch_open: bool,
    /// SPEC §13.2: a remote instance's host has a live control session. Always
    /// true for an embedded engine.
    pub host_live: bool,
    /// Owner decision 2026-09-23: the host's session is up but its heartbeats
    /// are silent past the suspend bound. Always false for an embedded engine.
    pub host_unresponsive: bool,
    /// SPEC §13.2 (W13): an owned process of this instance's engine exited;
    /// its dispatch is closed and its cleanup is being settled.
    pub engine_exited: bool,
    /// The latest fresh load sample reported for exactly this instance by the
    /// host that serves it, if any (W8). Absent is unknown load, never zero.
    pub load: Option<crate::load_table::LoadView>,
}

#[async_trait]
pub trait LifecyclePort: Send + Sync {
    // Reads. Named projections rather than a store handle: the coordinator owns
    // its store exclusively behind the controller lock, so an authority cannot
    // hand one out, and a handle hides which state the router actually depends on.

    /// Resolve a public route to its deployment.
    fn find_deployment_by_route(
        &self,
        route: &str,
    ) -> Result<Option<DeploymentRow>, LifecycleFault>;

    /// Public route ids currently eligible for admission.
    fn list_enabled_route_ids(&self) -> Result<Vec<String>, LifecycleFault>;

    /// One deployment's current record.
    fn get_deployment(&self, id: &str) -> Result<Option<DeploymentRow>, LifecycleFault>;

    /// The most recent operation accepted for a deployment.
    fn latest_operation(&self, deployment_id: &str)
        -> Result<Option<OperationRow>, LifecycleFault>;

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
    fn runtime_endpoint(&self, deployment: &str)
        -> Result<Option<RuntimeEndpoint>, LifecycleFault>;

    /// SPEC §10 (owner decision 2026-09-22): open a durable request lease for one
    /// dispatch before anything reaches the engine. `Ok(None)` means this
    /// authority keeps no durable request ledger at all; an authority that keeps
    /// one never answers `None`.
    async fn open_request_lease(
        &self,
        deployment: &str,
        max_per_deployment: usize,
    ) -> Result<Option<crate::request_leases::RequestLease>, crate::request_leases::LeaseRefused>;

    /// Close a request lease on evidence, or retain it as uncertain. Never
    /// called on a timer. An error leaves the lease charged.
    async fn close_request_lease(
        &self,
        lease: crate::request_leases::RequestLease,
        end: crate::request_leases::LeaseEnd,
    ) -> Result<(), crate::request_leases::LeaseRefused>;

    /// ADR 0013 §10 (I3): every instance of `deployment` that holds a runtime,
    /// for the router to choose among. `Ok(None)` means this authority has no
    /// instance view at all, and the router dispatches to the deployment as a
    /// whole (`runtime_endpoint`, `open_request_lease`).
    fn serving_instances(
        &self,
        _deployment: &str,
    ) -> Result<Option<Vec<ServingInstance>>, LifecycleFault> {
        Ok(None)
    }

    /// Where exactly the instance incarnation `generation` runs. `None` once it
    /// holds no runtime: a lease granted for it is then closed as not accepted
    /// and the request goes nowhere else under that lease.
    fn instance_endpoint(
        &self,
        deployment: &str,
        _generation: i64,
    ) -> Result<Option<RuntimeEndpoint>, LifecycleFault> {
        self.runtime_endpoint(deployment)
    }

    /// SPEC §10, ADR 0013 §10: open a durable lease charged to exactly the
    /// instance incarnation `generation` names, only while its gate is open.
    async fn open_instance_lease(
        &self,
        deployment: &str,
        _generation: i64,
        max_per_deployment: usize,
    ) -> Result<Option<crate::request_leases::RequestLease>, crate::request_leases::LeaseRefused>
    {
        self.open_request_lease(deployment, max_per_deployment).await
    }

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
    async fn observe_adapter(&self, deployment: &str) -> Result<WorkObservation, LifecycleFault>;

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

    /// SPEC §10 steps 2–7 (W10): bring `deployment` to a state a queued
    /// request can dispatch to, waiting through any transition in progress
    /// (SPEC §6.1: STARTING, WAKING, DRAINING and PARKING queue) and making
    /// room by switching when its activation does not fit. `Ok` once an
    /// instance serves. The router calls this once per waiting group, from a
    /// task a client disconnect does not cancel.
    ///
    /// The default is an authority without switching: one on-demand
    /// activation, awaited.
    async fn activate_for_request(&self, deployment: &str) -> Result<(), LifecycleFault> {
        let now_ready = self
            .get_deployment(deployment)?
            .is_some_and(|row| row.observed_state == LifecycleState::Ready);
        if now_ready {
            return Ok(());
        }
        let op = self.auto_activate(deployment).await?;
        self.wait_terminal(&op).await.map(|_| ())
    }
}

#[async_trait]
impl LifecyclePort for Controller {
    fn find_deployment_by_route(
        &self,
        route: &str,
    ) -> Result<Option<DeploymentRow>, LifecycleFault> {
        self.store_ref()
            .lock()
            .unwrap()
            .find_deployment_by_route(route)
            .map_err(Into::into)
    }
    fn list_enabled_route_ids(&self) -> Result<Vec<String>, LifecycleFault> {
        self.store_ref()
            .lock()
            .unwrap()
            .list_enabled_route_ids()
            .map_err(Into::into)
    }
    fn get_deployment(&self, id: &str) -> Result<Option<DeploymentRow>, LifecycleFault> {
        self.store_ref()
            .lock()
            .unwrap()
            .get_deployment(id)
            .map_err(Into::into)
    }
    fn latest_operation(
        &self,
        deployment_id: &str,
    ) -> Result<Option<OperationRow>, LifecycleFault> {
        self.store_ref()
            .lock()
            .unwrap()
            .latest_operation(deployment_id)
            .map_err(Into::into)
    }
    fn ready_deployments_excluding(&self, deployment: &str) -> Result<Vec<String>, LifecycleFault> {
        self.store_ref()
            .lock()
            .unwrap()
            .ready_deployments_excluding(deployment)
            .map_err(Into::into)
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
    /// The F1 controller is a test authority with no coordinator session, so it
    /// has no durable request ledger; it says so rather than pretending to one.
    async fn open_request_lease(
        &self,
        _deployment: &str,
        _max_per_deployment: usize,
    ) -> Result<Option<crate::request_leases::RequestLease>, crate::request_leases::LeaseRefused>
    {
        Ok(None)
    }
    async fn close_request_lease(
        &self,
        _lease: crate::request_leases::RequestLease,
        _end: crate::request_leases::LeaseEnd,
    ) -> Result<(), crate::request_leases::LeaseRefused> {
        // Unreachable: this authority never grants a lease to close.
        Ok(())
    }
    fn clear_suspension(&self, deployment: &str) -> Result<(), LifecycleFault> {
        self.store_ref()
            .lock()
            .unwrap()
            .set_suspended(deployment, false)
            .map_err(Into::into)
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
            .record_journal(host_id, operation_id, state, evidence)
            .map_err(Into::into)
    }
    async fn observe_adapter(&self, deployment: &str) -> Result<WorkObservation, LifecycleFault> {
        Controller::observe_adapter(self, deployment)
            .await
            .map_err(Into::into)
    }
    async fn idle_stop(&self, deployment: &str) -> Result<OperationHandle, LifecycleFault> {
        Controller::idle_stop(self, deployment)
            .await
            .map_err(Into::into)
    }
    async fn wait_terminal(
        &self,
        handle: &OperationHandle,
    ) -> Result<LifecycleState, LifecycleFault> {
        Controller::wait_terminal(self, handle)
            .await
            .map_err(Into::into)
    }
    async fn auto_activate(&self, deployment: &str) -> Result<OperationHandle, LifecycleFault> {
        Controller::auto_activate(self, deployment)
            .await
            .map_err(|error| match error {
                // SPEC §6.3: a suspended deployment is the operator's stop,
                // not a failed activation.
                ControllerError::OperationFailed { ref code, .. } if code == "suspended" => {
                    crate::fault::operator_stopped(deployment, false)
                }
                other => other.into(),
            })
    }
    async fn request_transition(
        &self,
        deployment: &str,
        action: LifecycleAction,
    ) -> Result<OperationHandle, LifecycleFault> {
        Controller::request_transition(self, deployment, action)
            .await
            .map_err(Into::into)
    }
}

/// The URL of a runtime that recorded its endpoint.
///
/// Spec §3: a binding records the authority it leased — `127.0.0.1:8100` — because
/// what it leases is a port, not a scheme. Everything that speaks to the engine
/// needs a URL, and every engine this project launches is reached over loopback
/// HTTP, so the scheme is supplied in one place rather than guessed by each caller.
/// An endpoint that already names a scheme is taken as written, so an attached
/// runtime someone else recorded is not rewritten.
pub fn engine_url(recorded: &str) -> Option<reqwest::Url> {
    if recorded.contains("://") {
        return recorded.parse().ok();
    }
    format!("http://{recorded}").parse().ok()
}

#[cfg(test)]
mod endpoint_tests {
    use super::engine_url;

    /// The form the store actually records. Parsing it as a URL fails, which is
    /// what made a launched engine unreachable.
    #[test]
    fn a_leased_socket_address_becomes_a_loopback_url() {
        let url = engine_url("127.0.0.1:8100").expect("a leased authority is addressable");
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert_eq!(url.port(), Some(8100));
    }

    /// An endpoint that already names a scheme is not rewritten.
    #[test]
    fn a_recorded_url_is_taken_as_written() {
        let url = engine_url("http://127.0.0.1:9100").expect("a URL is a URL");
        assert_eq!(url.port(), Some(9100));
    }

    /// Nothing addressable is nothing to send to, and inventing a default would
    /// point a request at whatever happens to be listening.
    #[test]
    fn an_endpoint_that_names_no_host_is_refused() {
        assert!(engine_url("").is_none());
    }
}

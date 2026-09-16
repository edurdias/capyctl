//! The owned coordinator as a lifecycle authority.
//!
//! This is the bridge the production cutover needs: it lets `roles.rs` name the
//! coordinator where it currently names the F1 controller. Reads go through the
//! coordinator's own store, which it owns exclusively behind the controller lock,
//! so nothing here hands out a store handle.
//!
//! Several port methods are answered with `Blocked` rather than guessed at. Each one
//! is a capability the coordinator genuinely lacks today, and refusing is the only
//! honest response: silently performing a neighbouring operation is how an
//! administrative stop gets undone or an unqualified transition gets attempted. The
//! refusals name what is missing so the gap is visible at the call site instead of
//! being discovered as wrong behaviour later.

use async_trait::async_trait;
use mllm_adapters::traits::WorkObservation;
use mllm_domain::{LifecycleAction, LifecycleState};
use mllm_store::deployments::{DeploymentRow, OperationRow};

use crate::coordinator::CoordinatorCommands;
use crate::fault::LifecycleFault;
use crate::operations::{DeployRequest, OperationHandle};
use crate::port::LifecyclePort;

/// How long a router-initiated activation may take before its receipt expires.
const ACTIVATION_DEADLINE_MS: i64 = 10 * 60 * 1000;

/// The principal a router-initiated activation acts as. Distinct from an operator so
/// history can tell an automatic wake from a deliberate one.
const ROUTER_PRINCIPAL: &str = "router";

pub struct CoordinatorLifecycle {
    commands: CoordinatorCommands,
}

impl CoordinatorLifecycle {
    pub fn new(commands: CoordinatorCommands) -> Self {
        Self { commands }
    }

    /// The idempotency key for an on-demand activation.
    ///
    /// Simultaneous requests for the same deployment must join one activation rather
    /// than start several (T15). Deriving the key from the deployment and the
    /// revision and generation it was observed at makes that the store's decision
    /// instead of the router's: two arrivals that saw the same state produce the same
    /// key and collapse, while an arrival that saw a later generation is asking about
    /// a different runtime and gets its own operation.
    fn activation_key(deployment: &str, revision: i64, generation: i64) -> String {
        format!("auto-activate:{deployment}:{revision}:{generation}")
    }

    fn current(&self, deployment: &str) -> Result<DeploymentRow, LifecycleFault> {
        self.commands
            .read(|store| store.get_deployment(deployment))?
            .ok_or_else(|| LifecycleFault::NotFound(deployment.to_string()))
    }

    fn unsupported(what: &str) -> LifecycleFault {
        LifecycleFault::Blocked(format!(
            "the coordinator cannot {what} yet; refusing rather than performing a \
             neighbouring operation"
        ))
    }
}

#[async_trait]
impl LifecyclePort for CoordinatorLifecycle {
    fn find_deployment_by_route(
        &self,
        route: &str,
    ) -> Result<Option<DeploymentRow>, LifecycleFault> {
        self.commands.read(|store| store.find_deployment_by_route(route))
    }

    fn list_enabled_route_ids(&self) -> Result<Vec<String>, LifecycleFault> {
        self.commands.read(|store| store.list_enabled_route_ids())
    }

    fn get_deployment(&self, id: &str) -> Result<Option<DeploymentRow>, LifecycleFault> {
        self.commands.read(|store| store.get_deployment(id))
    }

    fn latest_operation(
        &self,
        deployment_id: &str,
    ) -> Result<Option<OperationRow>, LifecycleFault> {
        self.commands.read(|store| store.latest_operation(deployment_id))
    }

    fn ready_deployments_excluding(
        &self,
        deployment: &str,
    ) -> Result<Vec<String>, LifecycleFault> {
        self.commands
            .read(|store| store.ready_deployments_excluding(deployment))
    }

    async fn observe_adapter(&self, _deployment: &str) -> Result<WorkObservation, LifecycleFault> {
        // The coordinator resolves a driver per binding inside its worker loop and
        // exposes none of them. Reporting Unknown here would be worse than refusing:
        // a caller draining before a park would read it as "nothing in flight".
        Err(Self::unsupported("observe engine work"))
    }

    async fn idle_stop(&self, _deployment: &str) -> Result<OperationHandle, LifecycleFault> {
        // The ordinary stop has no intent, so it cannot distinguish an eviction from
        // an operator's stop. Performing one as the other would either strand an
        // evicted deployment or let the next request undo a deliberate stop. Tracked
        // as milestone A1b.
        Err(Self::unsupported(
            "stop for idleness without suspending the deployment",
        ))
    }

    async fn wait_terminal(
        &self,
        _handle: &OperationHandle,
    ) -> Result<LifecycleState, LifecycleFault> {
        // The coordinator's observers are typed per command and are handed out at
        // acceptance, not looked up by operation id afterwards.
        Err(Self::unsupported("await a terminal state by operation id"))
    }

    async fn auto_activate(&self, deployment: &str) -> Result<OperationHandle, LifecycleFault> {
        let row = self.current(deployment)?;
        // An explicit stop must survive the next inference request (SPEC §6.3). The
        // coordinator's start would otherwise clear nothing and start it anyway.
        if row.desired_state == LifecycleState::Stopped
            && row.observed_state == LifecycleState::Stopped
        {
            let suspended = self
                .commands
                .read(|store| store.is_suspended(deployment))?;
            if suspended {
                return Err(LifecycleFault::Blocked(format!(
                    "deployment {deployment} was explicitly stopped"
                )));
            }
        }
        // Commands fence on the effective revision, which is not the row's schema
        // version; confusing them yields a revision conflict rather than a clear
        // failure.
        let revision = self
            .commands
            .read(|store| store.current_revision(deployment))?
            .ok_or_else(|| LifecycleFault::NotFound(deployment.to_string()))?;
        let key = Self::activation_key(deployment, revision, row.current_generation);
        let receipt = self.commands.start(
            ROUTER_PRINCIPAL,
            deployment,
            revision,
            &key,
            ACTIVATION_DEADLINE_MS,
        )?;
        Ok(OperationHandle {
            operation_id: mllm_domain::OperationId(receipt.operation_id().to_string()),
            deployment_id: deployment.to_string(),
        })
    }

    async fn request_transition(
        &self,
        deployment: &str,
        action: LifecycleAction,
    ) -> Result<OperationHandle, LifecycleFault> {
        match action {
            LifecycleAction::Start => self.auto_activate(deployment).await,
            // Every other transition is either absent from the ordinary lifecycle or
            // depends on the stop intent that does not exist yet.
            other => Err(Self::unsupported(&format!("perform {other:?}"))),
        }
    }

    async fn submit_deploy(&self, _req: DeployRequest) -> Result<String, LifecycleFault> {
        Err(Self::unsupported("accept a new deployment"))
    }

    fn clear_suspension(&self, _deployment: &str) -> Result<(), LifecycleFault> {
        // A write the router should not be making at all; it disappears with
        // router-owned eviction in A1b rather than being reimplemented here.
        Err(Self::unsupported("clear a suspension on a caller's behalf"))
    }

    fn journal(
        &self,
        _host_id: Option<&str>,
        _operation_id: Option<&str>,
        _state: Option<&str>,
        _evidence: &str,
    ) -> Result<(), LifecycleFault> {
        Err(Self::unsupported("journal on a caller's behalf"))
    }
}

#[cfg(test)]
mod tests;

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
use crate::operations::OperationHandle;
use crate::port::{LifecyclePort, RuntimeEndpoint};

/// How long a router-initiated activation may take before its receipt expires. The
/// store wants an absolute deadline on its own clock, so this window is added to the
/// coordinator's reading rather than passed as a duration.
const ACTIVATION_WINDOW_MS: i64 = 10 * 60 * 1000;

/// The principal a router-initiated activation acts as. Distinct from an operator so
/// history can tell an automatic wake from a deliberate one.
const ROUTER_PRINCIPAL: &str = "router";

/// How long a caller waits for an accepted operation to reach a terminal state, and
/// how often it re-reads. A caller giving up never cancels the operation.
const TERMINAL_WAIT: std::time::Duration = std::time::Duration::from_secs(600);
const TERMINAL_POLL: std::time::Duration = std::time::Duration::from_millis(200);

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

    /// Decide an operation's outcome from what was read, separated from the reading
    /// so every branch is reachable in a test without a database.
    ///
    /// `expired` means the caller's wait has run out, not that the operation has.
    fn classify(
        operation: &str,
        state: mllm_store::deployments::OpState,
        error_code: Option<String>,
        observed: Option<LifecycleState>,
        expired: bool,
    ) -> Option<Result<LifecycleState, LifecycleFault>> {
        use mllm_store::deployments::OpState;
        match state {
            OpState::Succeeded => Some(observed.ok_or_else(|| {
                LifecycleFault::Unavailable(format!(
                    "operation {operation} succeeded but its deployment has no observed state"
                ))
            })),
            OpState::Failed => Some(Err(LifecycleFault::Failed(format!(
                "operation {operation} failed with code {}",
                error_code.unwrap_or_else(|| "unknown".into())
            )))),
            // Still running. Exhausting the caller's wait is uncertainty, never
            // failure: the operation continues and the coordinator reconciles it.
            OpState::Pending | OpState::Running if expired => {
                Some(Err(LifecycleFault::Uncertain(format!(
                    "operation {operation} has not reached a terminal state; it is still \
                     running and the coordinator will reconcile it"
                ))))
            }
            OpState::Pending | OpState::Running => None,
        }
    }

    /// Accept a deployment durably and return its id.
    ///
    /// Not part of the lifecycle port: the router never creates deployments, and
    /// putting a management concern on the router's contract would widen what every
    /// authority must answer. The CLI calls it here directly.
    ///
    /// The idempotency key is derived from the context, name and manifest, so a
    /// retry after a lost response resolves to the same deployment rather than
    /// creating a second one (T09).
    pub fn submit_deploy(
        &self,
        context_id: &str,
        request: &crate::operations::DeployRequest,
    ) -> Result<String, LifecycleFault> {
        use sha2::{Digest as _, Sha256};
        let mut hash = Sha256::new();
        hash.update(context_id.as_bytes());
        hash.update([0u8]);
        hash.update(request.name.as_bytes());
        hash.update([0u8]);
        hash.update(&request.manifest);
        let key = format!("{:x}", hash.finalize());
        let accepted = self.commands.accept_deployment(
            mllm_store::deployments::AcceptDeployment {
                id: mllm_domain::DeploymentId::new(),
                name: request.name.clone(),
                kind: request.kind.clone(),
                route_model_id: request.route_model_id.clone(),
                // A deployment begins as durable intent, not as a running runtime.
                desired_state: LifecycleState::Stopped,
                schema_version: 1,
                idempotency_key: key,
                initial_operation_id: mllm_domain::OperationId(format!(
                    "op-{}",
                    ulid::Ulid::new()
                )),
            },
        )?;
        Ok(accepted.deployment_id.0.to_string())
    }

    /// The processes this deployment's retained runtime is recorded as owning.
    ///
    /// Replaces reading a live pid out of an in-memory handle map. That map does not
    /// survive a restart, so after one the controller could no longer say what it
    /// owned; these identities are durable, and each carries a boot id and start time,
    /// so a caller can establish whether the process is still the one recorded rather
    /// than trusting that a pid number still means what it did.
    ///
    /// An empty result means no retained binding, which is not the same as a runtime
    /// that has gone: use the absence proof for that.
    pub fn live_identities(
        &self,
        deployment: &str,
    ) -> Result<Vec<mllm_domain::completion::ProcessIdentity>, LifecycleFault> {
        let owner = self.commands.owner_for_read()?;
        let binding = owner
            .store()
            .runtime_binding(deployment)
            .map_err(LifecycleFault::from)?;
        Ok(binding.map(|b| b.identities).unwrap_or_default())
    }

    /// Publish the host's admission ceiling with its justifying observations.
    pub fn publish_resource_policy(
        &self,
        host: &mllm_config::effective::HostPolicy,
        observations: &[mllm_domain::resources::MemoryObservation],
    ) -> Result<(), LifecycleFault> {
        let now = self.commands.now_ms()?;
        self.commands.import_resource_policy(host, observations, now)
    }

    /// Create a deployment together with its effective configuration.
    ///
    /// Written together or not at all: a deployment without one can be named but
    /// never started.
    pub fn create_configuration(
        &self,
        principal: &str,
        key: &str,
        request_json: &str,
        trusted_host: &serde_json::Value,
    ) -> Result<mllm_store::managed_configuration::ManagedConfigurationReceipt, LifecycleFault>
    {
        let now = self.commands.now_ms()?;
        self.commands
            .create_managed_configuration(principal, key, request_json, trusted_host, now)
    }

    /// Reject a dispatch carrying a generation older than the deployment's current
    /// one (T18): an ingress gate must refuse late work after it closes.
    pub fn check_dispatch_generation(
        &self,
        deployment: &str,
        observed: i64,
    ) -> Result<i64, LifecycleFault> {
        self.commands
            .read(|store| store.check_generation(deployment, observed))
    }

    /// Accept a Stop against the deployment's current revision.
    ///
    /// `administrative` is the operator's intent from SPEC §6.3: it suspends
    /// automatic activation, and an idle eviction leaves it clear.
    fn stop(&self, deployment: &str, administrative: bool) -> Result<OperationHandle, LifecycleFault> {
        let row = self.current(deployment)?;
        let revision = self
            .commands
            .read(|store| store.current_revision(deployment))?
            .ok_or_else(|| LifecycleFault::NotFound(deployment.to_string()))?;
        let key = Self::stop_key(deployment, revision, row.current_generation, administrative);
        let deadline = self
            .commands
            .now_ms()?
            .checked_add(ACTIVATION_WINDOW_MS)
            .ok_or_else(|| LifecycleFault::Unavailable("clock overflow".into()))?;
        let receipt = if administrative {
            self.commands
                .administrative_stop(ROUTER_PRINCIPAL, deployment, revision, &key, deadline)
        } else {
            self.commands
                .stop(ROUTER_PRINCIPAL, deployment, revision, &key, deadline)
        }?;
        Ok(OperationHandle {
            operation_id: mllm_domain::OperationId(receipt.operation_id().to_string()),
            deployment_id: deployment.to_string(),
        })
    }

    /// Distinct per runtime and per intent, so a retry of the same stop replays its
    /// receipt while an operator's stop after an eviction is its own command.
    fn stop_key(deployment: &str, revision: i64, generation: i64, administrative: bool) -> String {
        let intent = if administrative { "admin" } else { "idle" };
        format!("stop:{intent}:{deployment}:{revision}:{generation}")
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

    /// Project the retained runtime's endpoint, served name and key.
    ///
    /// SPEC §3: all three belong to one launch. The binding names the incarnation,
    /// the key is sealed under that binding and incarnation together, and the served
    /// name is the first route of the revision the binding froze. Reading them as one
    /// answer is what stops a caller pairing this launch's port with the last
    /// launch's credential.
    ///
    /// A retained binding with no key is a runtime started without one, not an
    /// error: the field is optional in the launch plan too.
    fn runtime_endpoint(
        &self,
        deployment: &str,
    ) -> Result<Option<RuntimeEndpoint>, LifecycleFault> {
        let owner = self.commands.owner_for_read()?;
        let Some(binding) = owner
            .store()
            .runtime_binding(deployment)
            .map_err(LifecycleFault::from)?
        else {
            return Ok(None);
        };
        // The engine received the key hex-encoded, because it travels through an
        // environment variable; it is handed back in exactly that form so nothing
        // downstream has to guess at an encoding. It is never logged.
        let engine_key = owner
            .store()
            .engine_key(&binding.id, &binding.incarnation)?
            .map(hex::encode);
        let served_model = owner
            .store()
            .effective_routes(deployment)?
            .into_iter()
            .next()
            .ok_or_else(|| {
                LifecycleFault::Conflict(format!(
                    "deployment {deployment} has a retained runtime but serves no route, \
                     so there is no name to address its engine by"
                ))
            })?;
        Ok(Some(RuntimeEndpoint {
            endpoint: binding.endpoint,
            served_model,
            engine_key,
            incarnation: binding.incarnation,
        }))
    }

    async fn observe_adapter(&self, _deployment: &str) -> Result<WorkObservation, LifecycleFault> {
        // The coordinator resolves a driver per binding inside its worker loop and
        // exposes none of them. Reporting Unknown here would be worse than refusing:
        // a caller draining before a park would read it as "nothing in flight".
        Err(Self::unsupported("observe engine work"))
    }

    /// Stop an idle deployment, leaving it eligible for on-demand activation.
    async fn idle_stop(&self, deployment: &str) -> Result<OperationHandle, LifecycleFault> {
        self.stop(deployment, false)
    }

    /// Await a terminal state for an accepted operation.
    ///
    /// The coordinator's observers are typed per command and handed out at
    /// acceptance, so there is nothing to look up by operation id; the durable
    /// record is the shared truth and is what gets read here.
    ///
    /// Giving up does not cancel anything. The operation continues, and the
    /// coordinator's own loop reconciles it — which is why exhausting the wait is
    /// uncertainty rather than failure, and why nothing is aborted here. Treating it
    /// as a failure would tell a caller the activation did not happen while it is
    /// still running.
    async fn wait_terminal(
        &self,
        handle: &OperationHandle,
    ) -> Result<LifecycleState, LifecycleFault> {
        let deadline = std::time::Instant::now() + TERMINAL_WAIT;
        loop {
            let operation = handle.operation_id.0.clone();
            let deployment = handle.deployment_id.clone();
            let read = self
                .commands
                .read(move |store| {
                    let row = store.get_operation(&operation)?;
                    let observed =
                        store.get_deployment(&deployment)?.map(|r| r.observed_state);
                    Ok(row.map(|row| (row.state, row.error_code, observed)))
                })?
                .ok_or_else(|| LifecycleFault::NotFound(handle.operation_id.0.clone()))?;
            let (state, error_code, observed) = read;
            if let Some(outcome) = Self::classify(
                &handle.operation_id.0,
                state,
                error_code,
                observed,
                std::time::Instant::now() >= deadline,
            ) {
                return outcome;
            }
            tokio::time::sleep(TERMINAL_POLL).await;
        }
    }

    async fn auto_activate(&self, deployment: &str) -> Result<OperationHandle, LifecycleFault> {
        let row = self.current(deployment)?;
        // An explicit stop must survive the next inference request (SPEC §6.3). The
        // coordinator's start would otherwise clear nothing and start it anyway.
        if row.desired_state == LifecycleState::Stopped
            && row.observed_state == LifecycleState::Stopped
            && self
                .commands
                .read(|store| store.is_admin_stopped(deployment))?
        {
            return Err(LifecycleFault::Blocked(format!(
                "deployment {deployment} was explicitly stopped"
            )));
        }
        // Commands fence on the effective revision, which is not the row's schema
        // version; confusing them yields a revision conflict rather than a clear
        // failure.
        let revision = self
            .commands
            .read(|store| store.current_revision(deployment))?
            .ok_or_else(|| LifecycleFault::NotFound(deployment.to_string()))?;
        let key = Self::activation_key(deployment, revision, row.current_generation);
        let deadline = self
            .commands
            .now_ms()?
            .checked_add(ACTIVATION_WINDOW_MS)
            .ok_or_else(|| LifecycleFault::Unavailable("clock overflow".into()))?;
        let receipt =
            self.commands
                .start(ROUTER_PRINCIPAL, deployment, revision, &key, deadline)?;
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
            LifecycleAction::Start => {
                // SPEC §6.3: a start enables the deployment. An operator asking for
                // one is lifting their own earlier stop, so it is cleared rather
                // than refused — unlike an inference request, which must not.
                self.commands
                    .read(|store| store.set_admin_stopped(deployment, false))?;
                self.auto_activate(deployment).await
            }
            LifecycleAction::Stop => self.stop(deployment, true),
            // Park is absent from the ordinary lifecycle. It arrives with
            // eviction in A1b.
            other => Err(Self::unsupported(&format!("perform {other:?}"))),
        }
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

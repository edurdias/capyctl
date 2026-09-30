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
use capyctl_adapters::traits::WorkObservation;
use capyctl_domain::{LifecycleAction, LifecycleState};
use capyctl_store::deployments::{DeploymentRow, OperationRow};

use crate::coordinator::CoordinatorCommands;
use crate::fault::LifecycleFault;
use crate::operations::OperationHandle;
use crate::port::{LifecyclePort, RuntimeEndpoint};

/// How long a router-initiated activation may take before its receipt expires. The
/// store wants an absolute deadline on its own clock, so this window is added to the
/// coordinator's reading rather than passed as a duration.
///
/// ADR 0014 amendment A1: only the fallback for a revision with no readable
/// effective configuration; otherwise the deployment's own windows apply.
const ACTIVATION_WINDOW_MS: i64 = 10 * 60 * 1000;

/// ADR 0014 amendment A1: a router-initiated start is given the deployment's
/// Initialize timeout and a stop the Stop window, both within the request
/// deadline the store enforces (SPEC §6), rather than one fixed window.
fn lifecycle_deadline(
    commands: &CoordinatorCommands,
    deployment: &str,
    stop: bool,
) -> Result<i64, LifecycleFault> {
    let windows = commands.read(|store| store.lifecycle_windows(deployment))?;
    let window = windows.map_or(ACTIVATION_WINDOW_MS, |w| {
        if stop {
            w.stop_ms
        } else {
            w.initialize_ms
        }
    });
    commands
        .now_ms()?
        .checked_add(window)
        .ok_or_else(|| LifecycleFault::Unavailable("clock overflow".into()))
}

/// SPEC §6.3 (W5): a wake is given the deployment's `timeouts.wake` (its
/// Initialize window when none is recorded), within the request deadline.
fn wake_deadline(commands: &CoordinatorCommands, deployment: &str) -> Result<i64, LifecycleFault> {
    let windows = commands.read(|store| store.lifecycle_windows(deployment))?;
    let window = windows.map_or(ACTIVATION_WINDOW_MS, |w| {
        w.wake_ms.unwrap_or(w.initialize_ms)
    });
    commands
        .now_ms()?
        .checked_add(window)
        .ok_or_else(|| LifecycleFault::Unavailable("clock overflow".into()))
}

/// The principal a router-initiated activation acts as. Distinct from an operator so
/// history can tell an automatic wake from a deliberate one.
const ROUTER_PRINCIPAL: &str = "router";

/// How long a caller waits for an accepted operation to reach a terminal state, and
/// how often it re-reads. A caller giving up never cancels the operation.
const TERMINAL_WAIT: std::time::Duration = std::time::Duration::from_secs(600);
const TERMINAL_POLL: std::time::Duration = std::time::Duration::from_millis(200);

pub struct CoordinatorLifecycle {
    commands: CoordinatorCommands,
    /// SPEC §10: the durable request ledger, group-committed on its own thread.
    leases: crate::request_leases::RequestLeaseWriter,
    /// ADR 0013 §10 (I3): host liveness and engine load for the router's
    /// instance choice. `None` for the embedded role, which has no host
    /// sessions and no load reports: its choice rests on router in-flight.
    routing: Option<RoutingSignals>,
    /// SPEC §10, ADR 0013 §8 (W10): request-driven switching.
    switching: std::sync::Arc<crate::switching::Switcher>,
}

/// How one on-demand activation attempt ended before it was awaited.
enum Activation {
    Accepted(OperationHandle),
    /// No allowed host fits as the ledger stands (W10 makes room).
    Capacity,
    Fault(LifecycleFault),
}

/// ADR 0013 §10 (I3), owner decision D9: what the server knows about remote
/// instances beyond the store — whether each host's control session is live
/// (SPEC §13.2) and the load its agent last reported (W8).
#[derive(Clone)]
pub struct RoutingSignals {
    pub load: std::sync::Arc<crate::load_table::LoadTable>,
    /// Whether this host has a current authenticated control session.
    pub host_live: std::sync::Arc<dyn Fn(&str) -> bool + Send + Sync>,
    /// Owner decision 2026-09-23: whether this host's session is up but its
    /// heartbeats are silent past the suspend bound.
    pub host_unresponsive: std::sync::Arc<dyn Fn(&str) -> bool + Send + Sync>,
}

impl RoutingSignals {
    /// The production signals: the sessions' load table and presence.
    pub fn from_sessions(sessions: &crate::agent_sessions::AgentSessions) -> Self {
        let presence = sessions.clone();
        Self {
            load: sessions.load_table(),
            host_live: std::sync::Arc::new(move |host: &str| {
                presence.current_session(host).is_some()
            }),
            host_unresponsive: {
                let sessions = sessions.clone();
                std::sync::Arc::new(move |host: &str| sessions.unresponsive(host))
            },
        }
    }
}

impl CoordinatorLifecycle {
    pub fn new(commands: CoordinatorCommands) -> Self {
        let owner = commands.clone();
        let backend: crate::request_leases::LeaseBackend = std::sync::Arc::new(move |writes| {
            let owner = owner.owner_for_read()?;
            owner
                .store()
                .apply_request_lease_batch(owner.session(), writes)
                .map_err(|error| LifecycleFault::Unavailable(error.to_string()))
        });
        let switching = std::sync::Arc::new(crate::switching::Switcher::new(
            commands.clone(),
            crate::switching::SwitchOptions::default(),
        ));
        Self {
            commands,
            leases: crate::request_leases::RequestLeaseWriter::spawn(backend),
            routing: None,
            switching,
        }
    }

    /// SPEC §10 (W10): bound request-driven switches (drain timeout, polling).
    pub fn with_switch_options(mut self, options: crate::switching::SwitchOptions) -> Self {
        self.switching = std::sync::Arc::new(crate::switching::Switcher::new(
            self.commands.clone(),
            options,
        ));
        self
    }

    /// Owner decision 2026-09-23: share one switcher with the management
    /// API's `start --evict`, so both take the same per-host turns.
    pub fn with_switcher(mut self, switcher: std::sync::Arc<crate::switching::Switcher>) -> Self {
        self.switching = switcher;
        self
    }

    /// The switcher request-driven activation uses.
    pub fn switcher(&self) -> std::sync::Arc<crate::switching::Switcher> {
        self.switching.clone()
    }

    /// W10 gap (b): the router's waiting-request bounds from the host queue
    /// policies the store holds (`resource_policy.queue`, SPEC §16.2): the
    /// tightest of every published host's, since one router queue serves
    /// them all. `None` while no host has published a policy.
    pub fn queue_policy(
        &self,
    ) -> Result<Option<capyctl_config::effective::QueuePolicy>, LifecycleFault> {
        let owner = self.commands.owner_for_read()?;
        owner
            .store()
            .tightest_queue_policy()
            .map_err(|error| LifecycleFault::Unavailable(error.to_string()))
    }

    /// ADR 0013 §10 (I3): route with host liveness and reported engine load.
    pub fn with_routing(mut self, routing: RoutingSignals) -> Self {
        self.routing = Some(routing);
        self
    }

    /// Project one retained binding into where its engine is reached.
    ///
    /// SPEC §3: endpoint, key and served name belong to one launch, read as one
    /// answer so a caller never pairs this launch's port with another's key.
    fn endpoint_of(
        owner: &crate::ownership::OwnedCoordinatorState,
        deployment: &str,
        binding: capyctl_store::lifecycle::StoredRuntimeBinding,
        gate_open: bool,
    ) -> Result<RuntimeEndpoint, LifecycleFault> {
        // The engine received the key hex-encoded, because it travels through an
        // environment variable; it is handed back in exactly that form so nothing
        // downstream has to guess at an encoding. It is never logged.
        let engine_key = owner
            .store()
            .engine_key(
                &binding.id,
                &binding.incarnation,
                capyctl_store::secrets::SecretRole::Inference,
            )?
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
        // The binding recorded the authority it leased; a caller needs a URL to send
        // to, and forming it here is what keeps every caller from guessing a scheme.
        let remote = owner
            .store()
            .remote_ingress_endpoint(&binding.id)
            .map_err(|_| {
                LifecycleFault::Unavailable("remote ingress binding unavailable".into())
            })?;
        let endpoint = match remote {
            // SPEC §§6.1, 13.2 (G2): a remote engine's readiness belongs to the
            // host session that proved it. While that is unproven the host's
            // gate is closed, and forwarding would only fail after admission.
            Some(_) if !gate_open => {
                return Err(LifecycleFault::Unavailable(format!(
                    "deployment {deployment}: remote host readiness is being re-proven; \
                     dispatch is closed"
                )));
            }
            Some(endpoint) => endpoint,
            // SPEC §§4.3, 13.2 (P3): an embedded engine adopted by a restarted
            // standalone role is re-proven locally before anything is forwarded
            // to it; until then its dispatch is closed exactly like a remote one.
            // Only a Ready deployment is gated: before Ready nothing forwards, and
            // a launch in progress still names its own endpoint.
            None if !gate_open
                && owner
                    .store()
                    .get_deployment(deployment)?
                    .is_some_and(|row| {
                        row.observed_state == capyctl_domain::LifecycleState::Ready
                    }) =>
            {
                return Err(LifecycleFault::Unavailable(format!(
                    "deployment {deployment}: readiness of the adopted engine is being \
                     re-proven; dispatch is closed"
                )));
            }
            None => crate::port::engine_url(&binding.endpoint)
                .ok_or_else(|| {
                    LifecycleFault::Conflict(format!(
                        "deployment {deployment} recorded an endpoint that names no \
                     address: {}",
                        binding.endpoint
                    ))
                })?
                .to_string(),
        };
        Ok(RuntimeEndpoint {
            endpoint,
            served_model,
            engine_key,
            incarnation: binding.incarnation,
        })
    }

    /// The idempotency key for an on-demand activation.
    ///
    /// Simultaneous requests for the same deployment must join one activation rather
    /// than start several (T15). Deriving the key from the deployment and the
    /// revision and generation it was observed at makes that the store's decision
    /// instead of the router's: two arrivals that saw the same state produce the same
    /// key and collapse, while an arrival that saw a later generation is asking about
    /// a different runtime and gets its own operation.
    ///
    /// Found live on a 16 GB discrete GPU: a failed launch leaves the
    /// generation unchanged, so the key also names the deployment's latest
    /// operation (as [`Self::wake_key`] does). Without it every later request
    /// derived the failed attempt's key and was refused as a different command.
    fn activation_key(
        deployment: &str,
        revision: i64,
        generation: i64,
        latest_operation: &str,
    ) -> String {
        format!("auto-activate:{deployment}:{revision}:{generation}:{latest_operation}")
    }

    /// The deployment's latest operation id, for [`Self::activation_key`].
    fn latest_operation(&self, deployment: &str) -> Result<String, LifecycleFault> {
        Ok(self
            .commands
            .read(|store| store.latest_operation(deployment))?
            .map(|operation| operation.id)
            .unwrap_or_default())
    }

    /// W5: a wake's idempotency key. A parked instance keeps its generation, so
    /// the key also names the deployment's latest operation: one park cycle's
    /// wake is never replayed for the next, while requests that saw the same
    /// state still collapse into one restore (T15).
    fn wake_key(&self, base: &str, deployment: &str) -> Result<String, LifecycleFault> {
        let latest = self
            .commands
            .read(|store| store.latest_operation(deployment))?
            .map(|operation| operation.id)
            .unwrap_or_default();
        Ok(format!("wake:{base}:{latest}"))
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
        state: capyctl_store::deployments::OpState,
        error_code: Option<String>,
        observed: Option<LifecycleState>,
        expired: bool,
    ) -> Option<Result<LifecycleState, LifecycleFault>> {
        use capyctl_store::deployments::OpState;
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
        let accepted =
            self.commands
                .accept_deployment(capyctl_store::deployments::AcceptDeployment {
                    id: capyctl_domain::DeploymentId::new(),
                    name: request.name.clone(),
                    kind: request.kind.clone(),
                    route_model_id: request.route_model_id.clone(),
                    // A deployment begins as durable intent, not as a running runtime.
                    desired_state: LifecycleState::Stopped,
                    schema_version: 1,
                    idempotency_key: key,
                    initial_operation_id: capyctl_domain::OperationId(format!(
                        "op-{}",
                        ulid::Ulid::new()
                    )),
                })?;
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
    ) -> Result<Vec<capyctl_domain::completion::ProcessIdentity>, LifecycleFault> {
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
        host: &capyctl_config::effective::HostPolicy,
        observations: &[capyctl_domain::resources::MemoryObservation],
    ) -> Result<(), LifecycleFault> {
        let now = self.commands.now_ms()?;
        self.commands
            .import_resource_policy(host, observations, now)
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
    ) -> Result<capyctl_store::managed_configuration::ManagedConfigurationReceipt, LifecycleFault>
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
    fn stop(
        &self,
        deployment: &str,
        administrative: bool,
    ) -> Result<OperationHandle, LifecycleFault> {
        let row = self.current(deployment)?;
        let revision = self
            .commands
            .read(|store| store.current_revision(deployment))?
            .ok_or_else(|| LifecycleFault::NotFound(deployment.to_string()))?;
        let key = Self::stop_key(deployment, revision, row.current_generation, administrative);
        let deadline = lifecycle_deadline(&self.commands, deployment, true)?;
        let receipt = if administrative {
            self.commands.administrative_stop(
                ROUTER_PRINCIPAL,
                deployment,
                revision,
                &key,
                deadline,
            )
        } else {
            self.commands
                .stop(ROUTER_PRINCIPAL, deployment, revision, &key, deadline)
        }?;
        Ok(OperationHandle {
            operation_id: capyctl_domain::OperationId(receipt.operation_id().to_string()),
            deployment_id: deployment.to_string(),
        })
    }

    /// Distinct per runtime and per intent, so a retry of the same stop replays its
    /// receipt while an operator's stop after an eviction is its own command.
    fn stop_key(deployment: &str, revision: i64, generation: i64, administrative: bool) -> String {
        let intent = if administrative { "admin" } else { "idle" };
        format!("stop:{intent}:{deployment}:{revision}:{generation}")
    }

    /// One on-demand activation attempt (owner decision Q5): refuse an
    /// operator's stop, wake a parked instance in place, else start one
    /// instance cold. Capacity refusal is reported apart, for W10.
    fn activate_once(&self, deployment: &str) -> Result<Activation, LifecycleFault> {
        let row = self.refuse_operator_stop(deployment)?;
        // Commands fence on the effective revision, which is not the row's schema
        // version; confusing them yields a revision conflict rather than a clear
        // failure.
        let revision = self
            .commands
            .read(|store| store.current_revision(deployment))?
            .ok_or_else(|| LifecycleFault::NotFound(deployment.to_string()))?;
        self.activate_revision(deployment, &row, revision)
    }

    /// SPEC §6.3, owner decisions Q5 and Q7: an operator's stop survives the
    /// next inference request. Refused before anything else a request would
    /// do, including making room by switching: the M48 soak (2026-09-24) found
    /// a request for an explicitly stopped deployment parking a Ready
    /// incumbent on the tight host and only then being refused.
    fn refuse_operator_stop(&self, deployment: &str) -> Result<DeploymentRow, LifecycleFault> {
        let row = self.current(deployment)?;
        // An explicit stop must survive the next inference request (SPEC §6.3). The
        // coordinator's start would otherwise clear nothing and start it anyway.
        if row.desired_state == LifecycleState::Stopped
            && row.observed_state == LifecycleState::Stopped
            && self
                .commands
                .read(|store| store.is_admin_stopped(deployment))?
        {
            return Err(crate::fault::operator_stopped(deployment, false));
        }
        // Owner decisions Q5, Q7 (ADR 0013 as amended): on-demand activation never
        // lifts an operator's `stop instance`; it brings up the lowest-index
        // instance the operator left eligible, and refuses only when the
        // operator stopped every one.
        if self.commands.read(|store| {
            store
                .on_demand_instance_stopped(deployment)
                .map_err(Into::into)
        })? {
            return Err(crate::fault::operator_stopped(deployment, true));
        }
        Ok(row)
    }

    fn activate_revision(
        &self,
        deployment: &str,
        row: &DeploymentRow,
        revision: i64,
    ) -> Result<Activation, LifecycleFault> {
        let key = Self::activation_key(
            deployment,
            revision,
            row.current_generation,
            &self.latest_operation(deployment)?,
        );
        // Owner decision Q5, ADR 0013 §4 (W5): a parked instance is restored in
        // place, on the host it parked on, before anything starts cold; a
        // restore in flight is joined (T15).
        if let Some(receipt) = self.commands.wake(
            ROUTER_PRINCIPAL,
            deployment,
            capyctl_store::ordinary_lifecycle::park::WakeScope::OnDemand,
            revision,
            &self.wake_key(&key, deployment)?,
            wake_deadline(&self.commands, deployment)?,
        )? {
            return Ok(Activation::Accepted(OperationHandle {
                operation_id: capyctl_domain::OperationId(receipt.operation_id),
                deployment_id: deployment.to_string(),
            }));
        }
        let deadline = lifecycle_deadline(&self.commands, deployment, false)?;
        // Owner decision Q5 (ADR 0013 §9): one instance comes up for the
        // request; the rest only where they fit now, without eviction.
        let receipt = match self.commands.start_on_demand(
            ROUTER_PRINCIPAL,
            deployment,
            revision,
            &key,
            deadline,
        ) {
            Ok(receipt) => receipt,
            // ADR 0013 §8 rule 2 failed: no allowed host fits without
            // eviction. W10 makes room instead of refusing.
            Err(crate::coordinator::CoordinatorCommandError::Lifecycle(
                capyctl_store::lifecycle::LifecycleError::CapacityBlocked,
            )) => return Ok(Activation::Capacity),
            // Owner decision 2026-09-23: a solo first start empties its host
            // through the same switching rules a request uses.
            Err(crate::coordinator::CoordinatorCommandError::Lifecycle(
                capyctl_store::lifecycle::LifecycleError::StartupRequiresEmptyHost,
            )) => return Ok(Activation::Capacity),
            Err(error) => return Ok(Activation::Fault(error.into())),
        };
        Ok(Activation::Accepted(OperationHandle {
            operation_id: capyctl_domain::OperationId(receipt.operation_id().to_string()),
            deployment_id: deployment.to_string(),
        }))
    }

    /// Whether the deployment's current revision waits for its checkpoint
    /// digest before it can be sized (ADR 0014 §7: provisional and pending).
    fn measuring_checkpoint(&self, deployment: &str) -> Result<bool, LifecycleFault> {
        self.commands.read(|store| {
            let Some(revision) = store.current_revision(deployment)? else {
                return Ok(false);
            };
            Ok(store
                .checkpoint_digest(deployment, revision)
                .ok()
                .flatten()
                .is_some_and(|record| {
                    record.provisional
                        && record.state == capyctl_store::checkpoint_digests::DigestState::Pending
                }))
        })
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
        self.commands
            .read(|store| store.find_deployment_by_route(route))
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
        self.commands
            .read(|store| store.latest_operation(deployment_id))
    }

    fn ready_deployments_excluding(&self, deployment: &str) -> Result<Vec<String>, LifecycleFault> {
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
        // ADR 0013 §10: the instance a lease without a fence is charged to. The
        // router's instance choice uses `instance_endpoint` instead (I3).
        let Some((binding, gate_open)) = owner
            .store()
            .serving_binding(deployment)
            .map_err(LifecycleFault::from)?
        else {
            return Ok(None);
        };
        Self::endpoint_of(&owner, deployment, binding, gate_open).map(Some)
    }

    async fn open_request_lease(
        &self,
        deployment: &str,
        max_per_deployment: usize,
    ) -> Result<Option<crate::request_leases::RequestLease>, crate::request_leases::LeaseRefused>
    {
        let lease = self.leases.open(deployment, max_per_deployment).await?;
        // SPEC §6.5 (W5): router activity restarts the ready-idle timer.
        self.commands.note_activity(
            deployment,
            lease.instance().map(|(_, generation)| generation),
        );
        Ok(Some(lease))
    }

    async fn close_request_lease(
        &self,
        lease: crate::request_leases::RequestLease,
        end: crate::request_leases::LeaseEnd,
    ) -> Result<(), crate::request_leases::LeaseRefused> {
        // SPEC §6.5 (W5): idleness counts from the end of the last request.
        let instance = lease.instance();
        let closed = self.leases.close(lease, end).await;
        if let Some((deployment, generation)) = instance {
            self.commands.note_activity(&deployment, Some(generation));
        }
        closed
    }

    /// ADR 0013 §10 (I3): every instance holding a runtime, with its gate,
    /// its host's session liveness and its fresh engine load.
    fn serving_instances(
        &self,
        deployment: &str,
    ) -> Result<Option<Vec<crate::port::ServingInstance>>, LifecycleFault> {
        let rows = {
            let owner = self.commands.owner_for_read()?;
            owner
                .store()
                .serving_instances(deployment)
                .map_err(LifecycleFault::from)?
        };
        let now = capyctl_protocol::now_unix_ms();
        Ok(Some(
            rows.into_iter()
                .map(|row| {
                    let (host_live, host_unresponsive, load) =
                        match (&self.routing, &row.remote_host) {
                            // SPEC §13.2: a remote instance serves only while its
                            // host's control session is current.
                            (Some(routing), Some(host)) => (
                                (routing.host_live)(host),
                                (routing.host_unresponsive)(host),
                                routing.load.fresh_at(
                                    &crate::load_table::InstanceKey::new(
                                        deployment,
                                        row.generation,
                                    ),
                                    host,
                                    now,
                                ),
                            ),
                            // An embedded engine has no session to lose and no
                            // reported load; a server without routing signals
                            // relies on the dispatch gate alone.
                            _ => (true, false, None),
                        };
                    crate::port::ServingInstance {
                        instance_index: row.instance_index,
                        generation: row.generation,
                        host_id: row.host_id,
                        remote_host: row.remote_host,
                        launch_command_id: row.launch_command_id,
                        dispatch_open: row.dispatch_open,
                        host_live,
                        host_unresponsive,
                        engine_exited: row.engine_exited,
                        load,
                    }
                })
                .collect(),
        ))
    }

    /// ADR 0013 §10 (I3): the engine of exactly the incarnation a lease names.
    fn instance_endpoint(
        &self,
        deployment: &str,
        generation: i64,
    ) -> Result<Option<RuntimeEndpoint>, LifecycleFault> {
        let owner = self.commands.owner_for_read()?;
        let Some((binding, gate_open)) = owner
            .store()
            .serving_binding_at(deployment, generation)
            .map_err(LifecycleFault::from)?
        else {
            return Ok(None);
        };
        Self::endpoint_of(&owner, deployment, binding, gate_open).map(Some)
    }

    async fn open_instance_lease(
        &self,
        deployment: &str,
        generation: i64,
        max_per_deployment: usize,
    ) -> Result<Option<crate::request_leases::RequestLease>, crate::request_leases::LeaseRefused>
    {
        let lease = self
            .leases
            .open_instance(deployment, generation, max_per_deployment)
            .await?;
        self.commands.note_activity(deployment, Some(generation));
        Ok(Some(lease))
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
                    let observed = store.get_deployment(&deployment)?.map(|r| r.observed_state);
                    // SPEC §13.2: an uncertain run is retained, not finished. The
                    // caller learns that as soon as it is recorded, with the
                    // coordinator's own reason, instead of waiting out its bound.
                    let uncertain = if store.operation_is_uncertain(&operation)? {
                        Some(
                            store
                                .journal_evidence(&operation)?
                                .pop()
                                .unwrap_or_default(),
                        )
                    } else {
                        None
                    };
                    Ok(row.map(|row| (row.state, row.error_code, observed, uncertain)))
                })?
                .ok_or_else(|| LifecycleFault::NotFound(handle.operation_id.0.clone()))?;
            let (state, error_code, observed, uncertain) = read;
            if let Some(reason) = uncertain {
                return Err(LifecycleFault::Uncertain(format!(
                    "operation {} is retained as uncertain: {reason}",
                    handle.operation_id.0
                )));
            }
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
        match self.activate_once(deployment)? {
            Activation::Accepted(handle) => Ok(handle),
            Activation::Capacity => {
                Err(capyctl_store::lifecycle::LifecycleError::CapacityBlocked.into())
            }
            Activation::Fault(fault) => Err(fault),
        }
    }

    /// SPEC §10 steps 2–7, ADR 0013 §8–9 (W10). Queues through any transition
    /// in progress, activates one instance (Q5), and when it fits nowhere as
    /// the ledger stands, makes room on one host by switching and activates
    /// it there while holding that host's turn.
    async fn activate_for_request(&self, deployment: &str) -> Result<(), LifecycleFault> {
        use capyctl_store::ordinary_lifecycle::switching::RequestView;
        // ADR 0013 §8 rule 4: a deployment with a waiting group is never a
        // victim while it waits.
        let _waiting = self.switching.wait_for(deployment);
        // T15: one request-driven activation per deployment at a time.
        let _turn = self.switching.target_turn(deployment).await;
        let rounds = self.switching.options().max_rounds.max(1);
        let mut round = 0;
        loop {
            match self
                .commands
                .request_view(deployment)
                .map_err(LifecycleFault::from)?
            {
                RequestView::Serving => return Ok(()),
                // SPEC §6.1: STARTING, WAKING, DRAINING, PARKING and STOPPING
                // queue. Wait for the transition, then look again.
                RequestView::InFlight { operation_id } => {
                    self.wait_terminal(&OperationHandle {
                        operation_id: capyctl_domain::OperationId(operation_id),
                        deployment_id: deployment.to_string(),
                    })
                    .await?;
                    continue;
                }
                // SPEC §§6.1, 13.2: READY but closed with nothing moving (a
                // host re-proving readiness): retryable, never an eviction.
                RequestView::Closed => {
                    return Err(LifecycleFault::Unavailable(format!(
                        "dispatch to deployment {deployment} is closed; retry shortly"
                    )))
                }
                RequestView::Idle => {}
            }
            // Final review M11 (found live on the discrete-GPU laptop host): a
            // revision sized from a checkpoint not yet measured cannot be
            // placed or started until its digest is recorded (ADR 0014 §7).
            // Answering that as "no room could be made" (429) sent clients
            // looking for capacity; it is starting, and a retry succeeds.
            if self.measuring_checkpoint(deployment)? {
                return Err(LifecycleFault::Unavailable(format!(
                    "deployment {deployment} is starting: its checkpoint is being measured \
                     before it can be sized; retry shortly"
                )));
            }
            if round >= rounds {
                return Err(LifecycleFault::Blocked(format!(
                    "no room could be made for deployment {deployment} after {rounds} switch round(s)"
                )));
            }
            // SPEC §6.3: an operator's stop is refused before any victim is
            // chosen; no incumbent is evicted for a request that cannot run.
            self.refuse_operator_stop(deployment)?;
            let room = match self.switching.make_room(deployment).await {
                Ok(room) => room,
                Err(crate::switching::NoRoom::Moved) => continue,
                // A victim's park was refused on its own evidence (it serves
                // again): the next plan stops it instead (ADR 0013 §8 rule 8,
                // as the idle policy does), bounded like any other round.
                Err(crate::switching::NoRoom::Fault(LifecycleFault::Failed(reason))) => {
                    round += 1;
                    if round >= rounds {
                        return Err(LifecycleFault::Failed(reason));
                    }
                    continue;
                }
                Err(crate::switching::NoRoom::Fault(fault)) => return Err(fault),
            };
            // An earlier group on the host may have brought this deployment
            // up while this one waited its turn (T15): join, never restart.
            match self
                .commands
                .request_view(deployment)
                .map_err(LifecycleFault::from)?
            {
                RequestView::Serving => return Ok(()),
                RequestView::InFlight { .. } => continue,
                RequestView::Closed | RequestView::Idle => {}
            }
            match self.activate_once(deployment)? {
                Activation::Accepted(handle) => {
                    let outcome = self.wait_terminal(&handle).await;
                    if let Some(room) = room.as_ref().filter(|r| !r.switch_id.is_empty()) {
                        match &outcome {
                            Ok(_) => self.switching.completed(room, deployment),
                            Err(error) => self.switching.activation_failed(
                                room,
                                deployment,
                                &error.to_string(),
                            ),
                        }
                    }
                    outcome?;
                    // Loop once more: Ready is confirmed by the request view,
                    // and a restore or start that ended otherwise is seen.
                }
                // Another group took the room first: plan again, bounded.
                Activation::Capacity => {
                    round += 1;
                    continue;
                }
                Activation::Fault(fault) => return Err(fault),
            }
        }
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
                // Owner decision Q5: an explicit start targets all N instances,
                // so it also lifts every per-instance operator stop.
                self.commands.read(|store| {
                    store
                        .clear_instance_operator_stops(deployment)
                        .map_err(Into::into)
                })?;
                let row = self.current(deployment)?;
                let revision = self
                    .commands
                    .read(|store| store.current_revision(deployment))?
                    .ok_or_else(|| LifecycleFault::NotFound(deployment.to_string()))?;
                let key = format!(
                    "start:{}",
                    Self::activation_key(
                        deployment,
                        revision,
                        row.current_generation,
                        &self.latest_operation(deployment)?
                    )
                );
                let deadline = lifecycle_deadline(&self.commands, deployment, false)?;
                // SPEC §6.3 (W5): "Restore if parked, initialize if stopped."
                let woken = match self.commands.wake(
                    ROUTER_PRINCIPAL,
                    deployment,
                    capyctl_store::ordinary_lifecycle::park::WakeScope::All,
                    revision,
                    &self.wake_key(&key, deployment)?,
                    wake_deadline(&self.commands, deployment)?,
                ) {
                    Ok(woken) => woken,
                    // The start below answers for a worker not admitting work.
                    Err(crate::coordinator::CoordinatorCommandError::Coordinator(_)) => None,
                    Err(error) => return Err(error.into()),
                };
                // Owner decision Q5: an explicit start targets every instance.
                let operation = match self.commands.start(
                    ROUTER_PRINCIPAL,
                    deployment,
                    revision,
                    &key,
                    deadline,
                ) {
                    Ok(receipt) => receipt.operation_id().to_string(),
                    // Every instance was parked (now waking) or running.
                    Err(_) if woken.is_some() => woken.map(|w| w.operation_id).unwrap_or_default(),
                    Err(error) => return Err(error.into()),
                };
                Ok(OperationHandle {
                    operation_id: capyctl_domain::OperationId(operation),
                    deployment_id: deployment.to_string(),
                })
            }
            LifecycleAction::Stop => self.stop(deployment, true),
            // SPEC §6.3 (W5): drain and park every READY instance.
            LifecycleAction::Park => {
                let row = self.current(deployment)?;
                let revision = self
                    .commands
                    .read(|store| store.current_revision(deployment))?
                    .ok_or_else(|| LifecycleFault::NotFound(deployment.to_string()))?;
                let key = format!("park:{deployment}:{revision}:{}", row.current_generation);
                let deadline = lifecycle_deadline(&self.commands, deployment, true)?;
                let receipt =
                    self.commands
                        .park(ROUTER_PRINCIPAL, deployment, revision, &key, deadline)?;
                Ok(OperationHandle {
                    operation_id: capyctl_domain::OperationId(receipt.operation_id),
                    deployment_id: deployment.to_string(),
                })
            }
            // SPEC §6.3, §6.5 (W5): start, verify and park each instance in turn.
            LifecycleAction::Preinitialize => {
                let row = self.current(deployment)?;
                let revision = self
                    .commands
                    .read(|store| store.current_revision(deployment))?
                    .ok_or_else(|| LifecycleFault::NotFound(deployment.to_string()))?;
                let key = format!(
                    "preinitialize:{deployment}:{revision}:{}",
                    row.current_generation
                );
                let deadline = lifecycle_deadline(&self.commands, deployment, false)?;
                let receipt = self.commands.preinitialize(
                    ROUTER_PRINCIPAL,
                    deployment,
                    revision,
                    &key,
                    deadline,
                )?;
                Ok(OperationHandle {
                    operation_id: capyctl_domain::OperationId(receipt.operation_id),
                    deployment_id: deployment.to_string(),
                })
            }
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

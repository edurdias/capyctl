//! Authenticated command submission through the existing application-owned worker.
use crate::{
    configuration::{
        self, ConfigurationCommand, ConfigurationFailure, ConfigurationSource,
        SharedConfigurationSource,
    },
    events::EventSource,
    AppState, SnapshotSource, SnapshotUnavailable,
};
use axum::{
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use capyctl_controller::coordinator::{
    CoordinatorCommandError, CoordinatorCommands, CoordinatorError,
};
use capyctl_store::ordinary_lifecycle::park::WakeScope;
use capyctl_store::{
    events::{EventPage, EventReadError},
    lifecycle::LifecycleError,
    managed_configuration::ManagedConfigurationReceipt,
    snapshot::Snapshot,
};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionCommand {
    pub expected_revision: i64,
    pub action: Action,
    pub deadline_ms: i64,
    /// Owner decision 2026-09-23: `start deployment --evict` and `start
    /// instance --evict` run the W10 switch plan (same fairness window, drain
    /// timeout and victim order) before the start. Only a start accepts it; a
    /// default start never evicts. Absent means false, so an earlier body
    /// replays unchanged.
    #[serde(default)]
    pub evict: bool,
}
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Start,
    Stop,
    Park,
    Suspend,
    Resume,
    /// SPEC §6.3 (W6): `delete deployment`.
    Delete,
    /// SPEC §6.3, §6.5 (W5): start, verify and park each instance in turn.
    Preinitialize,
}

/// Owner decision 2026-09-23: the most `start --evict` commands one router
/// runs at once. Each holds its switch task until its start is accepted; one
/// past the bound is refused `queue_full` (retryable) rather than spawned.
pub const MAX_EVICTING_STARTS: usize = 4;

pub struct ActionReceipt {
    pub operation_id: String,
    pub deployment_id: String,
    pub revision: i64,
    pub joined: bool,
}
/// Owner decision Q7 (ADR 0013 as amended): the outcome of `stop instance` or
/// `start instance`. `operation_id` is absent when the instance holds no
/// runtime, so recording the operator's intent was the whole effect.
pub struct InstanceActionReceipt {
    pub deployment_id: String,
    pub instance: u32,
    pub operation_id: Option<String>,
    pub revision: i64,
    pub joined: bool,
}

pub trait ActionSource: Send + Sync + 'static {
    fn accept_action(
        &self,
        deployment: &str,
        key: &str,
        command: ActionCommand,
    ) -> Result<ActionReceipt, ConfigurationFailure>;

    /// Owner decision 2026-09-23: the switcher `start --evict` runs, shared
    /// with request-driven switching. `None`: eviction is unsupported here.
    fn switcher(&self) -> Option<Arc<capyctl_controller::switching::Switcher>> {
        None
    }

    /// SPEC §6.4 (owner decision 2026-09-23): check an evicting start before
    /// its switch releases anyone, so a start that would be refused (a stale
    /// revision, a deleted deployment, an unknown instance) evicts nothing.
    /// `Ok(true)` when the key already answered a command here: the start then
    /// replays that answer (or refuses the reused key) and no switch runs.
    fn check_evicting_start(
        &self,
        _deployment: &str,
        _instance: Option<u32>,
        _key: &str,
        _command: &ActionCommand,
    ) -> Result<bool, ConfigurationFailure> {
        Ok(false)
    }

    /// Owner decision Q7: a lifecycle action on one instance of a deployment.
    fn accept_instance_action(
        &self,
        _deployment: &str,
        _instance: u32,
        _key: &str,
        _command: ActionCommand,
    ) -> Result<InstanceActionReceipt, ConfigurationFailure> {
        Err(ConfigurationFailure::Unsupported)
    }
}

/// Construction rejects mismatched state before a router receives authority.
/// This handle does not own the worker shutdown task or create any new session.
pub struct OwnedActionSource {
    configuration: Arc<SharedConfigurationSource>,
    commands: CoordinatorCommands,
    /// Owner decision 2026-09-23: runs `start --evict`.
    switcher: Option<Arc<capyctl_controller::switching::Switcher>>,
}
impl OwnedActionSource {
    pub fn new(
        configuration: Arc<SharedConfigurationSource>,
        commands: CoordinatorCommands,
    ) -> Result<Self, ConfigurationFailure> {
        if !commands.shares_state(configuration.owned_state()) {
            return Err(ConfigurationFailure::Internal);
        }
        Ok(Self {
            configuration,
            commands,
            switcher: None,
        })
    }

    /// Owner decision 2026-09-23: accept `start --evict` through this
    /// switcher, the one request-driven activation uses.
    pub fn with_switcher(mut self, switcher: Arc<capyctl_controller::switching::Switcher>) -> Self {
        self.switcher = Some(switcher);
        self
    }

    /// The worker's command handle, for composed routers that act through the
    /// same owned state (the drain router).
    pub(crate) fn commands(&self) -> &CoordinatorCommands {
        &self.commands
    }

    /// SPEC §4.3, §6.3: the Stop an explicit drain issues is the ordinary one,
    /// so the deployment stays eligible for on-demand activation; its cleanup
    /// still completes only on evidence that the recorded processes are gone.
    ///
    /// ADR 0013 §5: a drain stops the instance on the drained host only.
    ///
    /// SPEC §6: `deadline_ms` is the drain's bound; each Stop is lowered to its
    /// own request-deadline window, so a drain longer than a deployment's
    /// request deadline still stops it, and a retry replays the same Stop.
    pub(crate) fn drain_stop(
        &self,
        deployment: &str,
        instance: u32,
        expected_revision: i64,
        key: &str,
        deadline_ms: i64,
    ) -> Result<Option<ActionReceipt>, ConfigurationFailure> {
        let receipt = self
            .commands
            .drain_stop_instance(
                self.configuration.principal(),
                deployment,
                instance,
                expected_revision,
                key,
                deadline_ms,
            )
            .map_err(command_failure)?;
        Ok(receipt.map(|receipt| ActionReceipt {
            operation_id: receipt.operation_id().into(),
            deployment_id: deployment.into(),
            revision: expected_revision,
            joined: false,
        }))
    }
}
impl ActionSource for OwnedActionSource {
    fn switcher(&self) -> Option<Arc<capyctl_controller::switching::Switcher>> {
        self.switcher.clone()
    }

    fn accept_action(
        &self,
        deployment: &str,
        key: &str,
        command: ActionCommand,
    ) -> Result<ActionReceipt, ConfigurationFailure> {
        self.deployment_action(deployment, key, command)
            .map_err(|failure| self.explain(deployment, failure))
    }

    fn check_evicting_start(
        &self,
        deployment: &str,
        instance: Option<u32>,
        key: &str,
        command: &ActionCommand,
    ) -> Result<bool, ConfigurationFailure> {
        let replay = self.evicting_start_checked(deployment, instance, key, command)?;
        // Owner decision 2026-09-25: a start no allowed host is eligible for
        // is refused as such before any victim is released.
        if !replay {
            if let Some(reason) = self.ineligible_detail(deployment) {
                return Err(ConfigurationFailure::HostIneligible(reason));
            }
        }
        Ok(replay)
    }

    fn accept_instance_action(
        &self,
        deployment: &str,
        instance: u32,
        key: &str,
        command: ActionCommand,
    ) -> Result<InstanceActionReceipt, ConfigurationFailure> {
        self.instance_action(deployment, instance, key, command)
            .map_err(|failure| self.explain(deployment, failure))
    }
}

impl OwnedActionSource {
    /// Owner decision 2026-09-25: a start refused because no allowed host is
    /// eligible names each host and why, instead of reporting capacity.
    fn explain(&self, deployment: &str, failure: ConfigurationFailure) -> ConfigurationFailure {
        match failure {
            ConfigurationFailure::HostIneligible(reason) if reason.is_empty() => {
                ConfigurationFailure::HostIneligible(
                    self.ineligible_detail(deployment).unwrap_or_default(),
                )
            }
            other => other,
        }
    }

    /// Owner decision 2026-09-25: when none of the allowed hosts that resolved
    /// the deployment's revision is eligible for placement now, one line per
    /// host saying why: the session's own reason (drain-only with both
    /// versions, draining, unresponsive, reconciling, a missing placement
    /// capability), else revoked, else no live control session. `None` when
    /// some allowed host is eligible, or this source has no notion of
    /// eligibility (the embedded host).
    fn ineligible_detail(&self, deployment: &str) -> Option<String> {
        // Read without the owner lock: the session source takes it itself.
        let (eligible, reasons) = self.commands.eligibility();
        let eligible = eligible?;
        let (hosts, enrolled) = {
            let owner = self.commands.owner_for_read().ok()?;
            (
                owner.store().resolved_hosts(deployment).ok()?,
                owner.store().enrolled_hosts().ok()?,
            )
        };
        if hosts.is_empty() || hosts.iter().any(|host| eligible.contains(host)) {
            return None;
        }
        let lines: Vec<String> = hosts
            .iter()
            .map(|host| {
                reasons.get(host).cloned().unwrap_or_else(|| {
                    if enrolled.iter().any(|e| e.host_id == *host && e.revoked) {
                        format!("host {host} is revoked")
                    } else {
                        format!("host {host} has no live control session (offline or not joined)")
                    }
                })
            })
            .collect();
        Some(format!(
            "no allowed host is eligible for placement: {}",
            lines.join("; ")
        ))
    }

    fn deployment_action(
        &self,
        deployment: &str,
        key: &str,
        command: ActionCommand,
    ) -> Result<ActionReceipt, ConfigurationFailure> {
        let principal = self.configuration.principal();
        match command.action {
            Action::Start => {
                // Owner decision Q5: an explicit start targets all N instances,
                // so it lifts every per-instance operator stop; restored if the
                // start is refused. SPEC §6.4: an exact retry (or a reused key)
                // is answered from history below and changes no mark first, so
                // a replayed start never lifts a stop made after it.
                let replay = self.start_key_used(deployment, None, key, &command)?;
                let previous = if replay.is_some() {
                    Vec::new()
                } else {
                    let previous = self.instance_rows(deployment)?;
                    self.commands
                        .read(|store| {
                            store
                                .clear_instance_operator_stops(deployment)
                                .map_err(Into::into)
                        })
                        .map_err(|_| ConfigurationFailure::Internal)?;
                    previous
                };
                // SPEC §6.3 (W5): "Restore if parked, initialize if stopped."
                // Parked instances wake in place, on the host each parked on.
                // A replayed start is answered by the start alone; a replayed
                // wake (the start was refused after it) by the wake alone.
                let woken = if replay == Some(StartReplay::Started) {
                    None
                } else {
                    match self.commands.wake(
                        principal,
                        deployment,
                        WakeScope::All,
                        command.expected_revision,
                        key,
                        command.deadline_ms,
                    ) {
                        Ok(woken) => woken,
                        // A worker not admitting work refuses the start too,
                        // and an exact start replay is still answered below.
                        Err(CoordinatorCommandError::Coordinator(_)) => None,
                        Err(error) => {
                            self.restore_marks(deployment, &previous);
                            return Err(command_failure(error));
                        }
                    }
                };
                if let (Some(StartReplay::Woken), Some(woken)) = (replay, &woken) {
                    return Ok(ActionReceipt {
                        operation_id: woken.operation_id.clone(),
                        deployment_id: woken.deployment_id.clone(),
                        revision: command.expected_revision,
                        joined: woken.joined,
                    });
                }
                let started = self.commands.start(
                    principal,
                    deployment,
                    command.expected_revision,
                    key,
                    command.deadline_ms,
                );
                match (started, woken) {
                    (Ok(receipt), _) => Ok(ActionReceipt {
                        operation_id: receipt.operation_id().into(),
                        deployment_id: receipt.deployment_id().into(),
                        revision: receipt.revision(),
                        joined: receipt.joined(),
                    }),
                    // Nothing left to start cold: every instance is waking
                    // (or already running). Only that refusal is benign.
                    (Err(error), Some(woken)) if nothing_left_to_start(&error) => {
                        Ok(ActionReceipt {
                            operation_id: woken.operation_id,
                            deployment_id: woken.deployment_id,
                            revision: command.expected_revision,
                            joined: woken.joined,
                        })
                    }
                    // SPEC §6.3: a start that could not start what it had to
                    // fails as that refusal. The accepted wake stands (its
                    // instances restore), so the marks it lifted stay lifted.
                    (Err(error), Some(_)) => Err(command_failure(error)),
                    (Err(error), None) => {
                        self.restore_marks(deployment, &previous);
                        Err(command_failure(error))
                    }
                }
            }
            // SPEC §6.3 `park deployment` (W5): drain and park every READY
            // instance at the declared tier; refused for restart-only.
            Action::Park => {
                let receipt = self
                    .commands
                    .park(
                        principal,
                        deployment,
                        command.expected_revision,
                        key,
                        command.deadline_ms,
                    )
                    .map_err(|error| self.gone_or(deployment, error))?;
                Ok(ActionReceipt {
                    operation_id: receipt.operation_id,
                    deployment_id: receipt.deployment_id,
                    revision: command.expected_revision,
                    joined: receipt.joined,
                })
            }
            // SPEC §6.3, §6.5 `preinitialize deployment` (W5).
            Action::Preinitialize => {
                let receipt = self
                    .commands
                    .preinitialize(
                        principal,
                        deployment,
                        command.expected_revision,
                        key,
                        command.deadline_ms,
                    )
                    .map_err(|error| self.gone_or(deployment, error))?;
                Ok(ActionReceipt {
                    operation_id: receipt.operation_id,
                    deployment_id: receipt.deployment_id,
                    revision: receipt.revision,
                    joined: receipt.joined,
                })
            }
            // SPEC §6.3 / T18: an operator stop suspends inference autoactivation.
            Action::Stop => {
                let receipt = self
                    .commands
                    .administrative_stop(
                        principal,
                        deployment,
                        command.expected_revision,
                        key,
                        command.deadline_ms,
                    )
                    .map_err(|error| self.gone_or(deployment, error))?;
                Ok(ActionReceipt {
                    operation_id: receipt.operation_id().into(),
                    deployment_id: deployment.into(),
                    revision: receipt.revision(),
                    joined: false,
                })
            }
            // SPEC §6.3 (W6): "Remove route and deployment after authorized
            // cleanup." Accepted only once every instance is stopped with
            // verified cleanup; it never stops anything or releases accounting.
            Action::Delete => {
                let receipt = self
                    .commands
                    .delete(
                        principal,
                        deployment,
                        command.expected_revision,
                        key,
                        command.deadline_ms,
                    )
                    .map_err(|error| match error {
                        CoordinatorCommandError::Lifecycle(LifecycleError::RuntimeRetained) => {
                            ConfigurationFailure::DeleteRequiresCleanup
                        }
                        other => command_failure(other),
                    })?;
                Ok(ActionReceipt {
                    operation_id: receipt.operation_id,
                    deployment_id: receipt.deployment_id,
                    revision: receipt.revision,
                    joined: false,
                })
            }
            _ => Err(ConfigurationFailure::Unsupported),
        }
    }

    fn evicting_start_checked(
        &self,
        deployment: &str,
        instance: Option<u32>,
        key: &str,
        command: &ActionCommand,
    ) -> Result<bool, ConfigurationFailure> {
        // An exact retry is answered from its receipt, even for a deployment
        // deleted since (SPEC §6.3, W6).
        if self
            .start_key_used(deployment, instance, key, command)?
            .is_some()
        {
            return Ok(true);
        }
        let (deleted, revision, rows) = self
            .commands
            .read(|store| {
                Ok((
                    store.is_deleted(deployment)?,
                    store.current_revision(deployment)?,
                    store.deployment_instances(deployment)?,
                ))
            })
            .map_err(|_| ConfigurationFailure::Internal)?;
        if deleted {
            return Err(ConfigurationFailure::NotFound);
        }
        if revision.ok_or(ConfigurationFailure::NotFound)? != command.expected_revision {
            return Err(ConfigurationFailure::RevisionConflict);
        }
        let active = |row: &capyctl_store::instances::InstanceRow| row.state == "active";
        let exists = match instance {
            None => rows.iter().any(active),
            Some(k) => rows.iter().any(|row| row.index == k && active(row)),
        };
        if !exists {
            return Err(ConfigurationFailure::NotFound);
        }
        // SPEC §6.4 / ADR 0014 §7: provisional sizing and unavailable
        // checkpoints refuse the start before any serving victim is released.
        self.commands
            .read(|store| {
                Ok(store.check_start_materialization(deployment, command.expected_revision))
            })
            .map_err(|_| ConfigurationFailure::Internal)?
            .map_err(|error| command_failure(CoordinatorCommandError::Lifecycle(error)))?;
        Ok(false)
    }

    fn instance_action(
        &self,
        deployment: &str,
        instance: u32,
        key: &str,
        command: ActionCommand,
    ) -> Result<InstanceActionReceipt, ConfigurationFailure> {
        let principal = self.configuration.principal();
        let stop = match command.action {
            Action::Stop => true,
            Action::Start => false,
            _ => return Err(ConfigurationFailure::Unsupported),
        };
        // SPEC §6.4: an exact retry is answered from its receipt before today's
        // revision or the operator's mark is consulted. Checking the revision
        // first turned a retry after an unrelated revision bump into a 409, and
        // setting the mark first let a replayed stop re-set a mark a later start
        // lifted (or a replayed start lift a later stop).
        let (replay, start_replay) = if stop {
            (
                self.stop_key_used(deployment, instance, key, &command)?,
                None,
            )
        } else {
            let found = self.start_key_used(deployment, Some(instance), key, &command)?;
            (found.is_some(), found)
        };
        let previous = if replay {
            None
        } else {
            let revision = self
                .commands
                .read(|store| store.current_revision(deployment))
                .map_err(|_| ConfigurationFailure::Internal)?
                .ok_or(ConfigurationFailure::NotFound)?;
            if revision != command.expected_revision {
                return Err(ConfigurationFailure::RevisionConflict);
            }
            // ADR 0013 §7 (I3 hand-off): the instance's existence and its
            // previous mark are read in the same transaction that sets the
            // mark. A separate lookup first raced compaction or retirement: an
            // instance moved or removed in between made this write fail, which
            // answered 500 for what is a 404.
            match self
                .commands
                .read(|store| Ok(store.set_instance_operator_stopped(deployment, instance, stop)))
            {
                Ok(Ok(previous)) => Some(previous),
                Ok(Err(capyctl_store::instances::InstanceError::NotFound)) => {
                    return Err(ConfigurationFailure::NotFound)
                }
                Ok(Err(_)) | Err(_) => return Err(ConfigurationFailure::Internal),
            }
        };
        let revision = command.expected_revision;
        let restore = || {
            if let Some(previous) = previous {
                let _ = self.commands.read(|store| {
                    store
                        .set_instance_operator_stopped(deployment, instance, previous)
                        .map_err(Into::into)
                });
            }
        };
        let receipt =
            |operation_id: Option<String>, revision: i64, joined: bool| InstanceActionReceipt {
                deployment_id: deployment.into(),
                instance,
                operation_id,
                revision,
                joined,
            };
        if stop {
            // An ordinary stop with verified cleanup: the deployment stays
            // eligible for on-demand activation of the instances the operator
            // did not stop (SPEC §6.3), and this instance stays stopped by its
            // mark. An instance holding nothing has nothing more to stop.
            let accepted = self
                .commands
                .stop_instance(
                    principal,
                    deployment,
                    instance,
                    command.expected_revision,
                    key,
                    command.deadline_ms,
                )
                .map_err(|error| {
                    restore();
                    command_failure(error)
                })?;
            Ok(match accepted {
                Some(accepted) => receipt(
                    Some(accepted.operation_id().into()),
                    command.expected_revision,
                    false,
                ),
                None => receipt(None, revision, false),
            })
        } else {
            // SPEC §6.3 (W5): a parked instance wakes in place, on its host.
            // A replayed start is answered by the start alone (SPEC §6.4).
            let woken = if start_replay == Some(StartReplay::Started) {
                None
            } else {
                match self.commands.wake(
                    principal,
                    deployment,
                    WakeScope::Instance(instance),
                    command.expected_revision,
                    key,
                    command.deadline_ms,
                ) {
                    Ok(woken) => woken,
                    // Answered by the start below, replay included.
                    Err(CoordinatorCommandError::Coordinator(_)) => None,
                    Err(error) => {
                        restore();
                        return Err(command_failure(error));
                    }
                }
            };
            if let Some(woken) = woken {
                return Ok(receipt(
                    Some(woken.operation_id),
                    command.expected_revision,
                    woken.joined,
                ));
            }
            // ADR 0013 §4: the instance is placed on an eligible allowed host.
            let accepted = self
                .commands
                .start_instance(
                    principal,
                    deployment,
                    instance,
                    command.expected_revision,
                    key,
                    command.deadline_ms,
                )
                .map_err(|error| {
                    restore();
                    command_failure(error)
                })?;
            Ok(receipt(
                Some(accepted.operation_id().into()),
                command.expected_revision,
                accepted.joined(),
            ))
        }
    }
}

impl OwnedActionSource {
    /// SPEC §6.4: whether `key` already answered a start of this scope, or the
    /// wake the start issues first: an exact retry, or a key reused for another
    /// request. Either way the commands answer it from history (a receipt or an
    /// idempotency conflict), so nothing may be changed before them.
    fn start_key_used(
        &self,
        deployment: &str,
        instance: Option<u32>,
        key: &str,
        command: &ActionCommand,
    ) -> Result<Option<StartReplay>, ConfigurationFailure> {
        let owner = self
            .commands
            .owner_for_read()
            .map_err(|_| ConfigurationFailure::Internal)?;
        let principal = self.configuration.principal();
        let started = owner.store().scoped_start_command_receipt(
            owner.session(),
            principal,
            deployment,
            instance,
            command.expected_revision,
            key,
            command.deadline_ms,
        );
        let woken = owner.store().restore_command_receipt(
            owner.session(),
            principal,
            deployment,
            instance.map_or(WakeScope::All, WakeScope::Instance),
            command.expected_revision,
            key,
            command.deadline_ms,
        );
        // The start's receipt is the answer whenever one exists: the first
        // response was the start's unless the start was refused after a wake.
        Ok(if key_used(started)? {
            Some(StartReplay::Started)
        } else if key_used(woken)? {
            Some(StartReplay::Woken)
        } else {
            None
        })
    }

    /// SPEC §6.4: as [`Self::start_key_used`], for one instance's stop.
    fn stop_key_used(
        &self,
        deployment: &str,
        instance: u32,
        key: &str,
        command: &ActionCommand,
    ) -> Result<bool, ConfigurationFailure> {
        let owner = self
            .commands
            .owner_for_read()
            .map_err(|_| ConfigurationFailure::Internal)?;
        key_used(owner.store().instance_stop_command_receipt(
            owner.session(),
            self.configuration.principal(),
            deployment,
            instance,
            command.expected_revision,
            key,
            command.deadline_ms,
        ))
    }

    fn instance_rows(
        &self,
        deployment: &str,
    ) -> Result<Vec<capyctl_store::instances::InstanceRow>, ConfigurationFailure> {
        let rows = self
            .commands
            .read(|store| store.deployment_instances(deployment).map_err(Into::into))
            .map_err(|_| ConfigurationFailure::Internal)?;
        if rows.is_empty() {
            return Err(ConfigurationFailure::NotFound);
        }
        Ok(rows)
    }

    /// SPEC §6.3 (W6): a refused command against a deleted deployment is
    /// `not_found`; an exact retry of an earlier receipt is still answered.
    /// A key reused for another command stays an idempotency conflict.
    fn gone_or(&self, deployment: &str, error: CoordinatorCommandError) -> ConfigurationFailure {
        let failure = command_failure(error);
        if failure == ConfigurationFailure::IdempotencyConflict {
            return failure;
        }
        match self.commands.read(|store| store.is_deleted(deployment)) {
            Ok(true) => ConfigurationFailure::NotFound,
            _ => failure,
        }
    }

    /// Put back the operator's per-instance stops a refused command lifted.
    fn restore_marks(&self, deployment: &str, rows: &[capyctl_store::instances::InstanceRow]) {
        for row in rows.iter().filter(|row| row.operator_stopped) {
            let _ = self.commands.read(|store| {
                store
                    .set_instance_operator_stopped(deployment, row.index, true)
                    .map_err(Into::into)
            });
        }
    }
}

/// Which earlier answer a start's key already holds (SPEC §6.4).
#[derive(Clone, Copy, PartialEq, Eq)]
enum StartReplay {
    /// The start itself was accepted (or the key answered another command).
    Started,
    /// Only the wake the start issued first was accepted.
    Woken,
}

/// A receipt lookup's answer to "was this key used here": a receipt or an
/// idempotency conflict is a use; a malformed request is left for the command
/// itself to refuse.
fn key_used<T>(lookup: Result<Option<T>, LifecycleError>) -> Result<bool, ConfigurationFailure> {
    match lookup {
        Ok(found) => Ok(found.is_some()),
        Err(LifecycleError::IdempotencyConflict) => Ok(true),
        Err(LifecycleError::Invalid) => Ok(false),
        Err(error) => Err(command_failure(CoordinatorCommandError::Lifecycle(error))),
    }
}

/// SPEC §6.3 "Restore if parked, initialize if stopped": after a wake was
/// accepted, the start's only benign refusal is that every instance already
/// holds a runtime (waking or running), so nothing is left to start cold.
fn nothing_left_to_start(error: &CoordinatorCommandError) -> bool {
    matches!(
        error,
        CoordinatorCommandError::Lifecycle(LifecycleError::RuntimeRetained)
    )
}

fn command_failure(error: CoordinatorCommandError) -> ConfigurationFailure {
    use ConfigurationFailure as F;
    match error {
        CoordinatorCommandError::Coordinator(CoordinatorError::Busy) => F::QueueFull,
        CoordinatorCommandError::Coordinator(CoordinatorError::Stopped(_)) => {
            F::ReconciliationRequired
        }
        CoordinatorCommandError::Coordinator(CoordinatorError::CallerTimeout) => {
            F::DeadlineExceeded
        }
        CoordinatorCommandError::Coordinator(_) => F::Internal,
        CoordinatorCommandError::Lifecycle(error) => match error {
            LifecycleError::Invalid => F::InvalidRequest,
            LifecycleError::NotFound => F::NotFound,
            LifecycleError::RevisionConflict => F::RevisionConflict,
            LifecycleError::IdempotencyConflict => F::IdempotencyConflict,
            LifecycleError::Conflict => F::LifecycleConflict,
            LifecycleError::RuntimeRetained => F::RuntimeRetained,
            LifecycleError::Unsupported => F::Unsupported,
            LifecycleError::Disabled | LifecycleError::HostPolicyDenied => F::HostPolicyDenied,
            // SPEC §14: the refusal names the limit it hit when placement
            // could say which.
            LifecycleError::CapacityBlocked(Some(detail)) => F::CapacityBlockedBecause(format!(
                "Capacity is unavailable: {detail}; wait, stop another deployment, or start with --evict"
            )),
            LifecycleError::CapacityBlocked(None) => F::CapacityBlocked,
            LifecycleError::StartupRequiresEmptyHost => F::StartupRequiresEmptyHost,
            // Explained with each host's reason by `OwnedActionSource::explain`.
            LifecycleError::HostIneligible => F::HostIneligible(String::new()),
            LifecycleError::QueueFull => F::QueueFull,
            LifecycleError::Stale | LifecycleError::ReconciliationRequired => {
                F::ReconciliationRequired
            }
            LifecycleError::Sql(_)
            | LifecycleError::CorruptStoredData
            | LifecycleError::Rejected(_) => F::Internal,
            // ADR 0014 §7 (WE3).
            LifecycleError::CheckpointDigestPending => F::CheckpointDigestPending,
            LifecycleError::CheckpointMismatch => F::CheckpointMismatch,
            // Discrete GPU design §11: `insufficient_device_memory: ...`
            // travels in the message, which the CLI maps to its exit.
            LifecycleError::CheckpointUnusable(reason) => F::CapacityBlockedBecause(reason),
            // ADR 0008.
            LifecycleError::ModelSourcePending => F::ModelSourcePending,
            LifecycleError::ModelSourceFailed => F::ModelSourceFailed,
        },
    }
}
impl SnapshotSource for OwnedActionSource {
    fn snapshot(&self) -> Result<Snapshot, SnapshotUnavailable> {
        self.configuration.snapshot()
    }
}
impl EventSource for OwnedActionSource {
    fn events_after(&self, after: Option<&str>, limit: usize) -> Result<EventPage, EventReadError> {
        self.configuration.events_after(after, limit)
    }
}
impl ConfigurationSource for OwnedActionSource {
    fn accept(
        &self,
        key: &str,
        command: ConfigurationCommand,
    ) -> Result<ManagedConfigurationReceipt, ConfigurationFailure> {
        self.configuration.accept(key, command)
    }
    fn checkpoint_digest_state(&self, deployment_id: &str, revision: i64) -> Option<String> {
        self.configuration
            .checkpoint_digest_state(deployment_id, revision)
    }
    fn effective_configuration(
        &self,
        deployment: &str,
    ) -> Result<Option<serde_json::Value>, ConfigurationFailure> {
        self.configuration.effective_configuration(deployment)
    }
}

/// What `start --evict` released before its start was accepted.
struct Evicted {
    switch_id: Option<String>,
    /// `<deployment>/<instance>` of every released victim.
    victims: Vec<String>,
}

fn receipt_json(mut body: serde_json::Value, evicted: Option<Evicted>) -> serde_json::Value {
    if let Some(evicted) = evicted {
        body["victims"] = serde_json::json!(evicted.victims);
        body["switch_id"] = serde_json::json!(evicted.switch_id);
    }
    body
}

fn switch_failure(fault: capyctl_controller::LifecycleFault) -> ConfigurationFailure {
    use capyctl_controller::LifecycleFault as L;
    match fault {
        L::NotFound(_) => ConfigurationFailure::NotFound,
        L::Conflict(_) => ConfigurationFailure::LifecycleConflict,
        // Nothing fits even after releasing every eligible READY instance;
        // the reason names the instance and each host's shortfall (owner
        // decision 2026-09-25).
        L::Blocked(reason) => ConfigurationFailure::CapacityBlockedBecause(reason),
        _ => ConfigurationFailure::SwitchFailed,
    }
}

/// Owner decision 2026-09-23 (`start --evict`): make room with the W10 switch
/// plan, then accept the ordinary start. The switch runs as its own task, so a
/// client that disconnects never abandons victims mid-drain; the host's turn
/// is held until the started operation ends, then the switch records its end.
async fn accept_evicting(
    source: Arc<dyn ActionSource>,
    id: String,
    instance: Option<u32>,
    key: String,
    command: ActionCommand,
    slot: tokio::sync::OwnedSemaphorePermit,
) -> Result<(ActionReceipt, Option<u32>, Evicted), ConfigurationFailure> {
    let switcher = source.switcher().ok_or(ConfigurationFailure::Unsupported)?;
    tokio::spawn(async move {
        // Held for the task's whole life, so evicting tasks stay bounded.
        let _slot = slot;
        let _waiting = switcher.wait_for(&id);
        // SPEC §6.4: validate the start (and find a replay) before any victim
        // is released; a replay answers from its receipt and evicts nothing.
        let replay = {
            let (source, id, key) = (source.clone(), id.clone(), key.clone());
            let check = command.clone();
            tokio::task::spawn_blocking(move || {
                source.check_evicting_start(&id, instance, &key, &check)
            })
            .await
            .map_err(|_| ConfigurationFailure::Internal)??
        };
        let _turn = switcher.target_turn(&id).await;
        // Owner decision 2026-09-25: `start deployment --evict` makes room for
        // every instance the start activates (planned whole before anyone is
        // released); `start instance --evict` for that instance.
        let rooms: Vec<capyctl_controller::switching::Room> = if replay {
            Vec::new()
        } else {
            match instance {
                None => switcher
                    .make_room_for_start(&id)
                    .await
                    .map_err(switch_failure)?,
                Some(k) => switcher
                    .make_room_explicit(&id, Some(k))
                    .await
                    .map_err(switch_failure)?
                    .into_iter()
                    .collect(),
            }
        };
        let accept = {
            let (source, id, key) = (source.clone(), id.clone(), key.clone());
            tokio::task::spawn_blocking(move || match instance {
                None => source.accept_action(&id, &key, command).map(|r| (r, None)),
                Some(k) => source
                    .accept_instance_action(&id, k, &key, command)
                    .map(|r| {
                        (
                            ActionReceipt {
                                operation_id: r.operation_id.unwrap_or_default(),
                                deployment_id: r.deployment_id,
                                revision: r.revision,
                                joined: r.joined,
                            },
                            Some(r.instance),
                        )
                    }),
            })
        };
        let accepted = accept.await.map_err(|_| ConfigurationFailure::Internal)?;
        let evicted = Evicted {
            switch_id: rooms
                .iter()
                .map(|r| r.switch_id.clone())
                .find(|id| !id.is_empty()),
            victims: rooms
                .iter()
                .flat_map(|r| {
                    r.victims
                        .iter()
                        .map(|v| format!("{}/{}", v.deployment_id, v.instance))
                })
                .collect(),
        };
        match accepted {
            Ok((receipt, index)) => {
                if !receipt.operation_id.is_empty() {
                    for room in rooms {
                        switcher.finish_in_background(
                            room,
                            id.clone(),
                            receipt.operation_id.clone(),
                        );
                    }
                }
                Ok((receipt, index, evicted))
            }
            Err(failure) => {
                // The victims stay released and on-demand eligible (ADR 0013
                // §8 rule 6); the refusal is the start's own.
                for room in &rooms {
                    if !room.switch_id.is_empty() {
                        switcher.activation_failed(
                            room,
                            &id,
                            &format!("start refused: {failure:?}"),
                        );
                    }
                }
                Err(failure)
            }
        }
    })
    .await
    .map_err(|_| ConfigurationFailure::Internal)?
}

pub(crate) async fn accept(State(state): State<Arc<AppState>>, request: Request) -> Response {
    match accept_inner(state, request).await {
        Ok((r, evicted)) => (StatusCode::ACCEPTED, Json(receipt_json(serde_json::json!({"api_version":"1","operation_id":r.operation_id,"deployment_id":r.deployment_id,"joined":r.joined,"revision":r.revision.to_string()}), evicted))).into_response(),
        Err(error) => error.response(),
    }
}
async fn accept_inner(
    state: Arc<AppState>,
    request: Request,
) -> Result<(ActionReceipt, Option<Evicted>), ConfigurationFailure> {
    use ConfigurationFailure::*;
    let id = request
        .uri()
        .path()
        .strip_prefix("/management/v1/deployments/")
        .and_then(|s| s.strip_suffix("/actions"))
        .ok_or(InvalidRequest)?;
    if !id
        .parse::<ulid::Ulid>()
        .is_ok_and(|parsed| parsed.to_string() == id)
    {
        return Err(InvalidRequest);
    }
    let id = id.to_owned();
    let (key, body, permit) = configuration::read_command(&state, request).await?;
    let command: ActionCommand = serde_json::from_slice(&body).map_err(|_| InvalidRequest)?;
    if command.expected_revision < 1 || command.deadline_ms < 1 {
        return Err(InvalidRequest);
    }
    if !matches!(
        command.action,
        Action::Start | Action::Stop | Action::Delete | Action::Park | Action::Preinitialize
    ) {
        return Err(Unsupported);
    }
    if command.evict && !matches!(command.action, Action::Start) {
        return Err(InvalidRequest);
    }
    let source = state.actions.clone().ok_or(Unsupported)?;
    let (result, evicted) = if command.evict {
        // The switch may drain for its whole bound: it does not hold one of
        // the bounded command slots meanwhile.
        drop(permit);
        let slot = state
            .evictions
            .clone()
            .try_acquire_owned()
            .map_err(|_| QueueFull)?;
        let (receipt, _, evicted) = accept_evicting(source, id, None, key, command, slot).await?;
        (receipt, Some(evicted))
    } else {
        (
            configuration::accept_blocking(permit, move || {
                source.accept_action(&id, &key, command)
            })
            .await?,
            None,
        )
    };
    if result.revision < 1
        || ![&result.operation_id, &result.deployment_id]
            .iter()
            .all(|id| {
                id.parse::<ulid::Ulid>()
                    .is_ok_and(|parsed| parsed.to_string() == **id)
            })
    {
        return Err(Internal);
    }
    Ok((result, evicted))
}

pub(crate) async fn accept_instance(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Response {
    match accept_instance_inner(state, request).await {
        Ok((r, evicted)) => (StatusCode::ACCEPTED, Json(receipt_json(serde_json::json!({"api_version":"1","operation_id":r.operation_id,"deployment_id":r.deployment_id,"instance":r.instance,"joined":r.joined,"revision":r.revision.to_string()}), evicted))).into_response(),
        Err(error) => error.response(),
    }
}

/// Owner decision Q7: `POST /management/v1/deployments/{id}/instances/{n}/actions`
/// with the deployment action body; only `start` and `stop` are accepted.
async fn accept_instance_inner(
    state: Arc<AppState>,
    request: Request,
) -> Result<(InstanceActionReceipt, Option<Evicted>), ConfigurationFailure> {
    use ConfigurationFailure::*;
    let (id, index) = request
        .uri()
        .path()
        .strip_prefix("/management/v1/deployments/")
        .and_then(|s| s.strip_suffix("/actions"))
        .and_then(|s| s.split_once("/instances/"))
        .ok_or(InvalidRequest)?;
    if !id
        .parse::<ulid::Ulid>()
        .is_ok_and(|parsed| parsed.to_string() == id)
    {
        return Err(InvalidRequest);
    }
    let index = index
        .parse::<u32>()
        .ok()
        .filter(|n| index == n.to_string() && *n < capyctl_config::instances::MAX_INSTANCES)
        .ok_or(InvalidRequest)?;
    let id = id.to_owned();
    let (key, body, permit) = configuration::read_command(&state, request).await?;
    let command: ActionCommand = serde_json::from_slice(&body).map_err(|_| InvalidRequest)?;
    if command.expected_revision < 1 || command.deadline_ms < 1 {
        return Err(InvalidRequest);
    }
    if !matches!(command.action, Action::Start | Action::Stop) {
        return Err(Unsupported);
    }
    if command.evict && !matches!(command.action, Action::Start) {
        return Err(InvalidRequest);
    }
    let source = state.actions.clone().ok_or(Unsupported)?;
    let (result, evicted) = if command.evict {
        drop(permit);
        let slot = state
            .evictions
            .clone()
            .try_acquire_owned()
            .map_err(|_| QueueFull)?;
        let (receipt, _, evicted) =
            accept_evicting(source, id, Some(index), key, command, slot).await?;
        (
            InstanceActionReceipt {
                deployment_id: receipt.deployment_id,
                instance: index,
                operation_id: Some(receipt.operation_id).filter(|op| !op.is_empty()),
                revision: receipt.revision,
                joined: receipt.joined,
            },
            Some(evicted),
        )
    } else {
        (
            configuration::accept_blocking(permit, move || {
                source.accept_instance_action(&id, index, &key, command)
            })
            .await?,
            None,
        )
    };
    if result.revision < 1
        || !result
            .deployment_id
            .parse::<ulid::Ulid>()
            .is_ok_and(|parsed| parsed.to_string() == result.deployment_id)
        || result.operation_id.as_ref().is_some_and(|op| {
            !op.parse::<ulid::Ulid>()
                .is_ok_and(|parsed| parsed.to_string() == *op)
        })
    {
        return Err(Internal);
    }
    Ok((result, evicted))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC §6.3: after an accepted wake, only "every instance already holds a
    /// runtime" lets a start answer with the wake's receipt. Every other
    /// refusal (capacity, a reused key, a stopped worker, a revision race) is
    /// the start's failure and must reach the caller.
    // T10
    #[test]
    fn only_nothing_left_to_start_is_benign_after_a_wake() {
        assert!(nothing_left_to_start(&CoordinatorCommandError::Lifecycle(
            LifecycleError::RuntimeRetained
        )));
        for error in [
            LifecycleError::CapacityBlocked(None),
            LifecycleError::IdempotencyConflict,
            LifecycleError::RevisionConflict,
            LifecycleError::StartupRequiresEmptyHost,
            LifecycleError::Disabled,
            LifecycleError::Conflict,
        ] {
            assert!(!nothing_left_to_start(&CoordinatorCommandError::Lifecycle(
                error
            )));
        }
        assert!(!nothing_left_to_start(
            &CoordinatorCommandError::Coordinator(CoordinatorError::Busy)
        ));
    }
}

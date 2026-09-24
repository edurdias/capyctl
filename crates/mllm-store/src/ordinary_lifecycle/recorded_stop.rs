//! An operator's Stop of a deployment that holds nothing (SPEC §6.1, §6.3).
//!
//! SPEC §6.3: "stop deployment: Suspend automatic activation, drain, and
//! terminate engine workers." A deployment whose launch failed and was already
//! released with evidence (observed FAILED, "recovery required", SPEC §6.1) has
//! no engine worker left to drain or terminate, but the operator's Stop still
//! has an effect: it suspends automatic activation and makes the desired state
//! `stopped`. Found live (M16, M53): that Stop was refused as `Lifecycle state
//! does not permit this action`, so a failed deployment could not be stopped.
//!
//! The Stop is accepted only when nothing at all is held: no unreleased runtime
//! binding, reservation owner, lifecycle claim, open run or unresolved step.
//! Anything held is the ordinary cleanup's (or the settlement's) and is never
//! released here: this module writes intent only and releases no accounting
//! (AGENTS.md: uncertainty retains accounting). The operation completes in the
//! acceptance transaction, and an exact retry returns the original receipt.
//!
//! Found live (M47): an operator's Stop while a launch was still in flight,
//! before its processes were associated, was refused the same way. Fencing
//! that launch would leave the processes it is starting untracked, so the Stop
//! is accepted as deferred: automatic activation is suspended at once, and the
//! owned worker carries the Stop out as an ordinary one when the launch settles
//! (see `resolve_deferred_stops`).
use super::cleanup::{hash_in, scope};
use super::unarmed_stop::OrdinaryStopReceipt;
use super::*;

/// The `operations.kind` of a Stop recorded against a deployment holding nothing.
pub(super) const KIND: &str = "administrative_stop_recorded";
/// The `operations.kind` of an operator's Stop deferred behind a launch in
/// flight without an association (live M47).
pub(super) const DEFERRED_KIND: &str = "administrative_stop_deferred";

/// Whether the deployment holds anything a Stop would have to clean up or
/// settle. Mirrors the retained set `delete` refuses on, less the instance
/// state flags this Stop itself writes.
fn holds_anything(tx: &Transaction<'_>, deployment: &str) -> Result<bool, LifecycleError> {
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM runtime_bindings WHERE deployment_id=?1 AND state!='released')
             OR EXISTS(SELECT 1 FROM endpoint_leases e JOIN runtime_bindings b ON b.id=e.binding_id WHERE b.deployment_id=?1)
             OR EXISTS(SELECT 1 FROM resource_owners WHERE deployment_id=?1)
             OR EXISTS(SELECT 1 FROM owners WHERE deployment_id=?1)
             OR EXISTS(SELECT 1 FROM lifecycle_claims WHERE deployment_id=?1)
             OR EXISTS(SELECT 1 FROM lifecycle_runs WHERE deployment_id=?1 AND state NOT IN ('succeeded','failed'))
             OR EXISTS(SELECT 1 FROM lifecycle_steps WHERE deployment_id=?1 AND state NOT IN ('completed','cancelled'))",
        [deployment],
        |r| r.get(0),
    )?)
}

/// Record an operator's Stop of a deployment that holds nothing, inside the
/// caller's acceptance transaction (which already wrote `admin_stopped`, checked
/// the revision and the managed target, and found no runtime to stop).
///
/// `Conflict` when anything is still held without a runtime binding the
/// ordinary Stop could take over: nothing is written and nothing is released.
pub(super) fn accept(
    tx: &Transaction<'_>,
    principal: &str,
    deployment: &str,
    revision: i64,
    key: &str,
    now: i64,
    deadline: i64,
) -> Result<OrdinaryStopReceipt, LifecycleError> {
    if deadline <= now {
        return Err(LifecycleError::Conflict);
    }
    // AGENTS.md: uncertainty retains accounting. Something held without a
    // runtime to stop is not this Stop's to settle.
    if holds_anything(tx, deployment)? {
        return Err(LifecycleError::Conflict);
    }
    // SPEC §6.3: the desired state is `stopped` on every instance, and nothing
    // is admitted or dispatched. The aggregate row follows by trigger. Like
    // every Stop, it fences: each instance moves to a generation of its own
    // (ADR 0013 §5), so the launch it ends reads superseded, not as the
    // instance's own closure.
    let instances: Vec<u32> = tx
        .prepare("SELECT instance_index FROM deployment_instances WHERE deployment_id=?1 ORDER BY instance_index")?
        .query_map([deployment], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for instance in instances {
        let generation = crate::instances::draw_generation(tx, deployment)?;
        tx.execute(
            "UPDATE deployment_instances SET generation=?1,desired_state='stopped',admission_enabled=0,dispatch_enabled=0
              WHERE deployment_id=?2 AND instance_index=?3",
            params![generation, deployment, instance],
        )?;
        tx.execute(
            "INSERT INTO generation_history(deployment_id,generation) VALUES (?1,?2)",
            params![deployment, generation],
        )?;
    }
    // W10 gap (a): every incarnation was replaced; its closure reasons are dead.
    crate::switch_state::prune_closures(tx, deployment)?;
    record(
        tx,
        false,
        principal,
        deployment,
        revision,
        key,
        now,
        deadline,
        serde_json::json!({
            "administrative_stop": 1,
            "deployment_id": deployment,
            "revision": revision,
            "held": "nothing",
            "principal": principal,
        }),
    )
}

/// Write the operation, its command receipt and its journal entry. A recorded
/// Stop's operation completes here; a deferred one stays pending until
/// [`crate::Store::resolve_deferred_stops`] closes it.
#[allow(clippy::too_many_arguments)]
fn record(
    tx: &Transaction<'_>,
    deferred: bool,
    principal: &str,
    deployment: &str,
    revision: i64,
    key: &str,
    now: i64,
    deadline: i64,
    evidence: serde_json::Value,
) -> Result<OrdinaryStopReceipt, LifecycleError> {
    let generation: i64 = tx.query_row(
        "SELECT current_generation FROM deployments WHERE id=?1",
        [deployment],
        |r| r.get(0),
    )?;
    let receipt = OrdinaryStopReceipt::recorded(
        deferred,
        ulid::Ulid::new().to_string(),
        revision,
        generation,
        now,
        deadline,
    );
    let (kind, state, journal) = if deferred {
        (DEFERRED_KIND, "pending", "stop_deferred")
    } else {
        (KIND, "succeeded", "stopped")
    };
    let command_scope = scope(deployment);
    tx.execute(
        "INSERT INTO operations(id,deployment_id,kind,state,error_code,idempotency_key) VALUES(?1,?2,?3,?4,NULL,NULL)",
        params![receipt.operation_id, deployment, kind, state],
    )?;
    tx.execute(
        "INSERT INTO command_receipts(principal_id,command_scope,idempotency_key,request_hash,operation_id,response_json) VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            principal,
            command_scope,
            key,
            hash_in(principal, &command_scope, revision, deadline)?,
            receipt.operation_id,
            encode(&receipt)?
        ],
    )?;
    // SPEC §17: the operator's Stop is recorded with what it found.
    journal_entry(tx, &receipt.operation_id, journal, &evidence)?;
    Ok(receipt)
}

fn journal_entry(
    tx: &Transaction<'_>,
    operation: &str,
    state: &str,
    evidence: &serde_json::Value,
) -> Result<(), LifecycleError> {
    tx.execute(
        "INSERT INTO journal_entries(id,host_id,operation_id,state,evidence) VALUES(?1,NULL,?2,?3,?4)",
        params![ulid::Ulid::new().to_string(), operation, state, encode(evidence)?],
    )?;
    Ok(())
}

/// SPEC §6.3 (live M47): whether a launch of the deployment is in flight with
/// no association yet (armed, its processes not recorded as a complete owned
/// group). Such a launch cannot be cleaned up with evidence until it settles.
pub(super) fn launching(tx: &Transaction<'_>, deployment: &str) -> Result<bool, LifecycleError> {
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id
                  JOIN runtime_bindings b ON b.id=s.binding_id
                 WHERE s.deployment_id=?1 AND o.kind='initialize' AND s.state='armed' AND b.state!='released'
                   AND NOT EXISTS(SELECT 1 FROM owned_launch_associations a WHERE a.step_id=s.id))",
        [deployment],
        |r| r.get(0),
    )?)
}

/// [`launching`] for one instance of the deployment.
pub(super) fn launching_instance(
    tx: &Transaction<'_>,
    deployment: &str,
    instance: u32,
) -> Result<bool, LifecycleError> {
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id
                  JOIN runtime_bindings b ON b.id=s.binding_id
                 WHERE s.deployment_id=?1 AND b.instance_index=?2 AND o.kind='initialize' AND s.state='armed' AND b.state!='released'
                   AND NOT EXISTS(SELECT 1 FROM owned_launch_associations a WHERE a.step_id=s.id))",
        params![deployment, instance],
        |r| r.get(0),
    )?)
}

/// Whether one instance already has a stop in flight (accepted, running or
/// retained uncertain).
pub(super) fn stopping(
    tx: &Transaction<'_>,
    deployment: &str,
    instance: u32,
) -> Result<bool, LifecycleError> {
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM lifecycle_runs r WHERE r.deployment_id=?1 AND r.instance_index=?2
                  AND r.action='stop' AND r.state IN ('queued','running','uncertain'))",
        params![deployment, instance],
        |r| r.get(0),
    )?)
}

/// Record an operator's Stop that waits for an in-flight launch (live M47).
/// The launching instance is not fenced and nothing is released: the caller
/// already suspended automatic activation and stopped every sibling it could
/// fence. The launch keeps its accounting and records its processes as it
/// would have; the Stop is carried out on it once it settles.
pub(super) fn defer(
    tx: &Transaction<'_>,
    principal: &str,
    deployment: &str,
    revision: i64,
    key: &str,
    now: i64,
    deadline: i64,
) -> Result<OrdinaryStopReceipt, LifecycleError> {
    if deadline <= now {
        return Err(LifecycleError::Conflict);
    }
    record(
        tx,
        true,
        principal,
        deployment,
        revision,
        key,
        now,
        deadline,
        serde_json::json!({
            "administrative_stop": 1,
            "deployment_id": deployment,
            "revision": revision,
            "deferred": "launch in flight without an association",
            "principal": principal,
        }),
    )
}

/// The exact receipt of an earlier recorded or deferred Stop. The caller has
/// already matched the request hash (principal, scope, revision and deadline).
pub(super) fn lookup(
    tx: &Transaction<'_>,
    deployment: &str,
    deadline: i64,
    operation: &str,
    raw: &str,
) -> Result<OrdinaryStopReceipt, LifecycleError> {
    let receipt: OrdinaryStopReceipt = decode(raw)?;
    let (kind, states) = match receipt.recorded_kind() {
        Some(false) => (KIND, "succeeded"),
        Some(true) => (DEFERRED_KIND, "pending,succeeded,failed"),
        None => return Err(LifecycleError::CorruptStoredData),
    };
    let recorded: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1 AND deployment_id=?2 AND kind=?3
                  AND instr(','||?4||',', ','||state||',')>0)",
        params![operation, deployment, kind, states],
        |r| r.get(0),
    )?;
    if !recorded || receipt.operation_id != operation || receipt.deadline_ms != deadline {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok(receipt)
}

/// Close a deferred Stop's operation with its outcome.
fn close(
    tx: &Transaction<'_>,
    operation: &str,
    state: &str,
    error_code: Option<&str>,
    evidence: &serde_json::Value,
) -> Result<(), LifecycleError> {
    let changed = tx.execute(
        "UPDATE operations SET state=?2,error_code=?3,updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')
          WHERE id=?1 AND kind=?4 AND state='pending'",
        params![operation, state, error_code, DEFERRED_KIND],
    )?;
    if changed != 1 {
        return Err(LifecycleError::CorruptStoredData);
    }
    journal_entry(tx, operation, "stop_resolved", evidence)
}

impl crate::Store {
    /// SPEC §6.3 (live M47): carry out every deferred operator Stop whose
    /// deployment no longer has a launch in flight without an association.
    /// Each is resolved in its own transaction by an ordinary administrative
    /// Stop under an idempotency key derived from the deferred operation, and
    /// closed `succeeded` naming that Stop's operation. A Start accepted since
    /// lifted the operator's Stop, so the deferred one closes `failed`
    /// (`superseded_by_start`) and stops nothing. A Stop the lifecycle refuses
    /// closes `failed` (`stop_refused`) with the reason journaled; nothing is
    /// released by either. Returns the operations closed.
    pub fn resolve_deferred_stops(
        &self,
        s: &CoordinatorSession,
        now: i64,
    ) -> Result<Vec<String>, LifecycleError> {
        let pending: Vec<(String, String, String, String)> = {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
            check_session(&tx, s)?;
            let rows = tx
                .prepare(
                    "SELECT o.id,o.deployment_id,c.principal_id,c.response_json FROM operations o
                       JOIN command_receipts c ON c.operation_id=o.id
                      WHERE o.kind=?1 AND o.state='pending' ORDER BY o.accepted_at,o.id",
                )?
                .query_map([DEFERRED_KIND], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        let mut closed = Vec::new();
        for (operation, deployment, principal, raw) in pending {
            let receipt: OrdinaryStopReceipt = decode(&raw)?;
            if receipt.recorded_kind() != Some(true) || receipt.operation_id != operation {
                return Err(LifecycleError::CorruptStoredData);
            }
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            check_session(&tx, s)?;
            if launching(&tx, &deployment)? {
                continue;
            }
            let admin: bool = tx.query_row(
                "SELECT admin_stopped=1 FROM deployments WHERE id=?1",
                [&deployment],
                |r| r.get(0),
            )?;
            if !admin {
                close(
                    &tx,
                    &operation,
                    "failed",
                    Some("superseded_by_start"),
                    &serde_json::json!({"deployment_id": deployment, "superseded_by": "start"}),
                )?;
                tx.commit()?;
                closed.push(operation);
                continue;
            }
            // The follow-up Stop keeps the window the operator's command had.
            let window = receipt
                .deadline_ms
                .checked_sub(receipt.accepted_at_ms)
                .ok_or(LifecycleError::CorruptStoredData)?;
            let deadline = now.checked_add(window).ok_or(LifecycleError::Invalid)?;
            let key = format!("deferred:{operation}");
            match Self::stop_in_transaction(
                &tx,
                s,
                &principal,
                &deployment,
                receipt.revision,
                &key,
                now,
                deadline,
                true,
                false,
            ) {
                Ok(stop) => {
                    close(
                        &tx,
                        &operation,
                        "succeeded",
                        None,
                        &serde_json::json!({
                            "deployment_id": deployment,
                            "follow_up_operation_id": stop.operation_id,
                        }),
                    )?;
                    tx.commit()?;
                }
                Err(
                    error @ (LifecycleError::Sql(_)
                    | LifecycleError::CorruptStoredData
                    | LifecycleError::Stale),
                ) => return Err(error),
                Err(error) => {
                    // Nothing the refused Stop wrote is kept.
                    drop(tx);
                    let tx =
                        Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
                    check_session(&tx, s)?;
                    close(
                        &tx,
                        &operation,
                        "failed",
                        Some("stop_refused"),
                        &serde_json::json!({
                            "deployment_id": deployment,
                            "reason": format!("{error:?}"),
                        }),
                    )?;
                    tx.commit()?;
                }
            }
            closed.push(operation);
        }
        Ok(closed)
    }
}

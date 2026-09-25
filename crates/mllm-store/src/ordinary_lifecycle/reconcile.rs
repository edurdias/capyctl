//! ADR 0013 §7, owner decision Q8 (unit I2): the scheduler's durable
//! reconciliation of instances against their deployment's declaration.
//!
//! Everything this acts on is recorded before it acts, so a controller restart
//! resumes exactly where the last pass stopped:
//!
//! - an instance a non-count revision left running an earlier runtime is
//!   stopped with verified cleanup (owner decision Q8); its restart was marked
//!   pending when the revision was accepted and is placed once it holds
//!   nothing;
//! - an instance a count decrease retired is drained and stopped the same way,
//!   and its row removed once it holds nothing; indices are then compacted over
//!   instances that hold nothing;
//! - a start that could not be placed stays pending until its deadline, is
//!   retried on every pass, and ends with its capacity diagnostic.
//!
//! Every stop is the ordinary one: cleanup completes only on evidence that the
//! recorded processes are gone, and an uncertain instance keeps its accounting
//! (SPEC §6.1, AGENTS.md). Each action runs in its own transaction, so one
//! instance that cannot be acted on now never holds up another.
use super::cleanup::{instance_scope, StopCommand};
use super::placement::{bounded_deadline, defer, prepare, Eligible, Prepared};
use super::*;

/// The principal the scheduler's own lifecycle commands act as.
pub const SCHEDULER_PRINCIPAL: &str = "scheduler";

/// The most actions one pass takes; the rest wait for the next pass.
const MAX_ACTIONS: usize = 16;

/// One thing a reconciliation pass did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reconciled {
    /// A stop was accepted for this instance (`reason`: `revision` for a
    /// non-count revision, `retire` for a count decrease).
    Stopped {
        deployment_id: String,
        instance: u32,
        operation_id: String,
        reason: &'static str,
    },
    /// A pending start was placed and accepted.
    Started {
        deployment_id: String,
        instance: u32,
        operation_id: String,
    },
    /// A pending start still fits nowhere.
    Deferred {
        deployment_id: String,
        instance: u32,
        code: &'static str,
    },
    /// A pending start reached its deadline unplaced.
    Expired {
        deployment_id: String,
        instance: u32,
    },
    /// A retired instance held nothing and its row was removed.
    Retired {
        deployment_id: String,
        instance: u32,
    },
}

struct Candidate {
    deployment_id: String,
    instance: u32,
    lifecycle: String,
    revision: Option<i64>,
    generation: Option<i64>,
    pending_until: Option<i64>,
    holds: bool,
    stopping: bool,
    starting: bool,
    blocked: bool,
    declared_revision: i64,
    declared_instances: u32,
}

fn candidates(tx: &Transaction<'_>) -> Result<Vec<Candidate>, LifecycleError> {
    let rows = tx
        .prepare(
            "SELECT i.deployment_id,i.instance_index,i.state,i.revision,i.generation,i.pending_start_until_ms,
                    EXISTS(SELECT 1 FROM runtime_bindings b WHERE b.deployment_id=i.deployment_id AND b.instance_index=i.instance_index AND b.state!='released'),
                    EXISTS(SELECT 1 FROM lifecycle_runs r WHERE r.deployment_id=i.deployment_id AND r.instance_index=i.instance_index AND r.action='stop' AND r.state IN ('queued','running','uncertain')),
                    EXISTS(SELECT 1 FROM lifecycle_runs r WHERE r.deployment_id=i.deployment_id AND r.instance_index=i.instance_index AND r.action='activate' AND r.state IN ('queued','running','uncertain')),
                    (d.admin_stopped=1 OR d.suspended=1 OR i.operator_stopped=1),
                    d.revision,n.instances
               FROM deployment_instances i JOIN deployments d ON d.id=i.deployment_id
               JOIN deployment_revision_instances n ON n.deployment_id=d.id AND n.revision=d.revision
              WHERE d.kind='model' AND (i.state='retiring' OR i.pending_start_until_ms IS NOT NULL
                    OR i.instance_index>=n.instances
                    OR (i.revision IS NOT NULL AND i.revision!=d.revision))
              ORDER BY i.deployment_id,i.instance_index",
        )?
        .query_map([], |r| {
            Ok(Candidate {
                deployment_id: r.get(0)?,
                instance: r.get(1)?,
                lifecycle: r.get(2)?,
                revision: r.get(3)?,
                generation: r.get(4)?,
                pending_until: r.get(5)?,
                holds: r.get(6)?,
                stopping: r.get(7)?,
                starting: r.get(8)?,
                blocked: r.get(9)?,
                declared_revision: r.get(10)?,
                declared_instances: r.get(11)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// ADR 0013 §7: whether an instance's runtime is still the declared one.
fn current_runtime(tx: &Transaction<'_>, c: &Candidate) -> Result<bool, LifecycleError> {
    let Some(revision) = c.revision else {
        return Ok(true);
    };
    Ok(
        crate::instances::runtime_revision(tx, &c.deployment_id, revision)?
            == crate::instances::runtime_revision(tx, &c.deployment_id, c.declared_revision)?,
    )
}

fn stop(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    c: &Candidate,
    reason: &'static str,
    now: i64,
) -> Result<Option<Reconciled>, LifecycleError> {
    let (Some(revision), Some(generation)) = (c.revision, c.generation) else {
        return Ok(None);
    };
    let fence = DeploymentFence {
        deployment_id: c.deployment_id.clone(),
        revision,
        generation,
    };
    let (_, e) = effective(tx, &fence)?;
    let key = format!("reconcile:{reason}:{}:{}", c.declared_revision, generation);
    let command = StopCommand {
        scope: Some(instance_scope(&c.deployment_id, c.instance)),
        revision: Some(c.declared_revision),
    };
    let receipt = crate::Store::accept_instance_stop_in_transaction(
        tx,
        s,
        SCHEDULER_PRINCIPAL,
        &fence,
        &key,
        now,
        now.saturating_add(e.request_deadline_ms),
        &command,
    )?;
    Ok(Some(Reconciled::Stopped {
        deployment_id: c.deployment_id.clone(),
        instance: c.instance,
        operation_id: receipt.operation_id,
        reason,
    }))
}

/// Owner decision Q8: a pending restart's window is the request deadline
/// counted from when the instance can actually be placed again, not from the
/// revision's acceptance. While the instance's stop is in flight the window is
/// pushed to at least `now` plus the request deadline, so a verified cleanup
/// slower than the deadline never expires the restart it precedes. Nothing is
/// released or advanced here; only the pending start's own deadline moves.
fn reanchor(tx: &Transaction<'_>, c: &Candidate, now: i64) -> Result<(), LifecycleError> {
    let (Some(until), Some(revision), Some(generation)) =
        (c.pending_until, c.revision, c.generation)
    else {
        return Ok(());
    };
    let fence = DeploymentFence {
        deployment_id: c.deployment_id.clone(),
        revision,
        generation,
    };
    let (_, e) = effective(tx, &fence)?;
    let floor = now.saturating_add(e.request_deadline_ms);
    if floor > until {
        tx.execute(
            "UPDATE deployment_instances SET pending_start_until_ms=?3
              WHERE deployment_id=?1 AND instance_index=?2 AND pending_start_until_ms IS NOT NULL",
            params![c.deployment_id, c.instance, floor],
        )?;
    }
    Ok(())
}

fn act(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    c: &Candidate,
    now: i64,
    eligible: Eligible<'_>,
    starts: bool,
) -> Result<Option<Reconciled>, LifecycleError> {
    let surplus = c.lifecycle == "retiring";
    if c.holds {
        if c.stopping || c.starting {
            if c.stopping {
                // Owner decision Q8: the restart waits on this stop, however
                // long its verified cleanup takes.
                reanchor(tx, c, now)?;
            }
            // A start in flight settles first; a stop in flight is waited on.
            return Ok(None);
        }
        if surplus {
            return stop(tx, s, c, "retire", now);
        }
        if !current_runtime(tx, c)? {
            let stopped = stop(tx, s, c, "revision", now)?;
            reanchor(tx, c, now)?;
            return Ok(stopped);
        }
        return Ok(None);
    }
    if c.starting || c.stopping {
        return Ok(None);
    }
    if surplus {
        one(tx.execute(
            "DELETE FROM deployment_instances WHERE deployment_id=?1 AND instance_index=?2 AND state='retiring'",
            params![c.deployment_id, c.instance],
        )?)?;
        crate::instances::compact(tx, &c.deployment_id, c.declared_instances).map_err(|error| {
            match error {
                crate::instances::InstanceError::Sql(error) => LifecycleError::Sql(error),
                _ => LifecycleError::CorruptStoredData,
            }
        })?;
        return Ok(Some(Reconciled::Retired {
            deployment_id: c.deployment_id.clone(),
            instance: c.instance,
        }));
    }
    if c.instance >= c.declared_instances {
        crate::instances::compact(tx, &c.deployment_id, c.declared_instances).map_err(|error| {
            match error {
                crate::instances::InstanceError::Sql(error) => LifecycleError::Sql(error),
                _ => LifecycleError::CorruptStoredData,
            }
        })?;
        return Ok(None);
    }
    let Some(until) = c.pending_until else {
        return Ok(None);
    };
    if c.blocked {
        // An operator stop, a suspension or a stop of this instance ends it.
        tx.execute(
            "UPDATE deployment_instances SET pending_start_until_ms=NULL WHERE deployment_id=?1 AND instance_index=?2",
            params![c.deployment_id, c.instance],
        )?;
        return Ok(None);
    }
    if now >= until {
        tx.execute(
            "UPDATE deployment_instances SET pending_start_until_ms=NULL,
                    last_error=COALESCE(last_error,'placement: no_host_fits')||'; start deadline passed'
              WHERE deployment_id=?1 AND instance_index=?2",
            params![c.deployment_id, c.instance],
        )?;
        return Ok(Some(Reconciled::Expired {
            deployment_id: c.deployment_id.clone(),
            instance: c.instance,
        }));
    }
    if !starts {
        return Ok(None);
    }
    match prepare(tx, &c.deployment_id, c.instance, eligible)? {
        Prepared::Placed(fence) => {
            let deadline = bounded_deadline(tx, &fence, now, until)?;
            let start =
                crate::Store::accept_start_in_transaction(tx, s, &fence, now, deadline, true)?;
            Ok(Some(Reconciled::Started {
                deployment_id: c.deployment_id.clone(),
                instance: c.instance,
                operation_id: start.operation_id,
            }))
        }
        Prepared::Joined(_) | Prepared::Running => {
            tx.execute(
                "UPDATE deployment_instances SET pending_start_until_ms=NULL WHERE deployment_id=?1 AND instance_index=?2",
                params![c.deployment_id, c.instance],
            )?;
            Ok(None)
        }
        Prepared::Unplaceable(code) => {
            let changed: bool = tx.query_row(
                "SELECT COALESCE(last_error,'')!=?3 FROM deployment_instances WHERE deployment_id=?1 AND instance_index=?2",
                params![c.deployment_id, c.instance, format!("placement: {code}")],
                |r| r.get(0),
            )?;
            defer(tx, &c.deployment_id, c.instance, code, None)?;
            Ok(changed.then(|| Reconciled::Deferred {
                deployment_id: c.deployment_id.clone(),
                instance: c.instance,
                code,
            }))
        }
    }
}

impl crate::Store {
    /// One bounded reconciliation pass (see the module documentation). Each
    /// action commits on its own; one that is refused now (an instance whose
    /// launch is still armed, for example) is left for a later pass and never
    /// fails the others. `starts` is false while the worker admits no
    /// Initialize (it waits on an uncertain launch); stops and retirements
    /// still proceed then.
    pub fn reconcile_instances(
        &self,
        s: &CoordinatorSession,
        now: i64,
        eligible: Eligible<'_>,
        starts: bool,
    ) -> Result<Vec<Reconciled>, LifecycleError> {
        if now < 0 {
            return Err(LifecycleError::Invalid);
        }
        let pending = {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
            check_session(&tx, s)?;
            candidates(&tx)?
        };
        let mut done = Vec::new();
        for candidate in pending {
            if done.len() >= MAX_ACTIONS {
                break;
            }
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            check_session(&tx, s)?;
            match act(&tx, s, &candidate, now, eligible, starts) {
                Ok(action) => {
                    tx.commit()?;
                    done.extend(action);
                }
                Err(LifecycleError::Sql(error)) => return Err(LifecycleError::Sql(error)),
                Err(LifecycleError::CorruptStoredData) => {
                    return Err(LifecycleError::CorruptStoredData)
                }
                // Refused now (a launch still armed, a stale fence, a queue
                // bound): nothing was written; a later pass retries.
                Err(_) => {}
            }
        }
        Ok(done)
    }
}

//! ADR 0028 §11 (decided 2026-10-06): the request-stall check, durably.
//!
//! A request forwarded to a group's head that got no first token within the
//! stall timeout makes the controller probe the head once. When that probe
//! fails too, the group failed (`group_stalled`), and
//! [`crate::Store::record_group_stall`] records it in one transaction: the
//! head's dispatch closes with its reason (so no switch or re-proof reopens
//! it), the plan keeps the failure (rank 0, the head the probe went through,
//! with its closed code) and the instance's status names it. Nothing is
//! released here. The group stop it owes is an ordinary stop of the
//! instance under the failure principal, retried from
//! [`crate::Store::group_stall_stops_due`] until one is accepted; that stop's
//! cleanup releases each member only on its own host's gone-evidence.
//!
//! ADR 0035 §7: the same rule applies to every instance on several GPUs of
//! one host (no group plan): a stalled request whose probe through the
//! instance's own endpoint failed too fails it `tp_stalled` (status only);
//! [`crate::Store::record_tp_stall`] closes its dispatch and names it, and
//! the ordinary stop it owes is retried from
//! [`crate::Store::tp_stall_stops_due`] until one is accepted, whose cleanup
//! releases every GPU only once every recorded process is gone.
use super::*;
use capyctl_domain::group::GroupPlan;

/// The closed code (spec §16) a stalled group fails with; no exit number.
pub const GROUP_STALLED: &str = "group_stalled";

/// ADR 0028 §11: the group generation a stall report names, while it is its
/// instance's current Ready group and has recorded no failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StalledGroup {
    pub deployment_id: String,
    pub instance_index: u32,
    /// The revision the group launched at.
    pub revision: i64,
    /// The Initialize operation that launched it.
    pub operation_id: String,
    /// That operation's completed step: the head's Launch command id, the
    /// launch the head's agent retained.
    pub step_id: String,
    pub plan: GroupPlan,
}

/// ADR 0028 §11: a stalled group whose stop is still owed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupStallStop {
    pub deployment_id: String,
    pub instance_index: u32,
    /// The stalled generation.
    pub generation: i64,
}

/// The current, Ready, unfailed group of the instance at `generation`: its
/// revision, Initialize operation and completed step.
fn serving_group(
    tx: &Transaction<'_>,
    deployment_id: &str,
    instance_index: u32,
    generation: i64,
) -> Result<Option<(i64, String, String)>, LifecycleError> {
    Ok(tx
        .query_row(
            "SELECT r.revision,r.operation_id,s.id FROM deployment_instances i
               JOIN group_plans g ON g.deployment_id=i.deployment_id AND g.instance_index=i.instance_index AND g.generation=i.generation
               JOIN lifecycle_runs r ON r.deployment_id=i.deployment_id AND r.instance_index=i.instance_index AND r.generation=i.generation
               JOIN operations o ON o.id=r.operation_id AND o.kind='initialize'
               JOIN lifecycle_steps s ON s.operation_id=o.id AND s.state='completed'
              WHERE i.deployment_id=?1 AND i.instance_index=?2 AND i.generation=?3
                AND i.desired_state='ready' AND i.observed_state='ready'
                AND g.state='active' AND g.failed_rank IS NULL
              ORDER BY s.id DESC LIMIT 1",
            params![deployment_id, instance_index, generation],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?)
}

/// ADR 0035 §7: the instance on several GPUs of one host a stall report
/// names, while `generation` is its current Ready launch, no group's, and
/// not already failed `tp_stalled`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StalledLocal {
    pub deployment_id: String,
    pub instance_index: u32,
    pub generation: i64,
    /// The revision it launched at.
    pub revision: i64,
    /// The Initialize operation that launched it.
    pub operation_id: String,
    /// That operation's completed step (the owned handle).
    pub step_id: String,
    pub binding_id: String,
    pub incarnation: String,
    /// The processes its launch recorded.
    pub identities: Vec<capyctl_domain::completion::ProcessIdentity>,
}

/// A serving launch on several GPUs: (revision, Initialize operation,
/// completed step, binding, incarnation).
type ServingLocal = (i64, String, String, String, String);

/// The current, Ready instance on several GPUs at `generation` (no group
/// plan, not already stalled): its revision, Initialize operation,
/// completed step and binding.
fn serving_local(
    tx: &Transaction<'_>,
    deployment_id: &str,
    instance_index: u32,
    generation: i64,
) -> Result<Option<ServingLocal>, LifecycleError> {
    Ok(tx
        .query_row(
            "SELECT r.revision,r.operation_id,s.id,b.id,b.incarnation FROM deployment_instances i
               JOIN lifecycle_runs r ON r.deployment_id=i.deployment_id AND r.instance_index=i.instance_index AND r.generation=i.generation
               JOIN operations o ON o.id=r.operation_id AND o.kind='initialize'
               JOIN lifecycle_steps s ON s.operation_id=o.id AND s.state='completed'
               JOIN runtime_bindings b ON b.id=s.binding_id AND b.state!='released'
              WHERE i.deployment_id=?1 AND i.instance_index=?2 AND i.generation=?3
                AND i.desired_state='ready' AND i.observed_state='ready'
                AND COALESCE(json_array_length(i.device_json),0)>1
                AND COALESCE(i.last_error,'')!=?4
                AND NOT EXISTS(SELECT 1 FROM group_plans g WHERE g.deployment_id=i.deployment_id
                      AND g.instance_index=i.instance_index AND g.generation=i.generation)
              ORDER BY s.id DESC LIMIT 1",
            params![deployment_id, instance_index, generation, TP_STALLED],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?)
}

/// ADR 0035 §7: the closed code (spec §16) an instance on several GPUs
/// fails with when a stalled request's probe failed too; status only.
pub use capyctl_domain::local_tp::TP_STALLED;

impl crate::Store {
    /// ADR 0028 §11: the group a stall report for `instance_index` of
    /// `deployment_id` at `generation` names, when that generation is still
    /// the instance's current Ready group and no failure is recorded for it.
    /// `None` for any other report (a past or future generation, a
    /// single-host instance, a group already failing), which is ignored.
    pub fn stalled_group(
        &self,
        deployment_id: &str,
        instance_index: u32,
        generation: i64,
    ) -> Result<Option<StalledGroup>, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let Some((revision, operation_id, step_id)) =
            serving_group(&tx, deployment_id, instance_index, generation)?
        else {
            return Ok(None);
        };
        let plan = crate::groups::plan_at(&tx, deployment_id, instance_index, generation)
            .map_err(|_| LifecycleError::CorruptStoredData)?
            .map(|(plan, _)| plan)
            .ok_or(LifecycleError::CorruptStoredData)?;
        tx.commit()?;
        Ok(Some(StalledGroup {
            deployment_id: deployment_id.to_owned(),
            instance_index,
            revision,
            operation_id,
            step_id,
            plan,
        }))
    }

    /// ADR 0028 §11 (decided 2026-10-06): the group at `generation` stalled
    /// and its head failed the probe. In one transaction its dispatch closes
    /// with a recorded reason, the plan records the failure at rank 0 with
    /// [`GROUP_STALLED`] and status names it; every member stays charged.
    /// Returns whether it was recorded: `false` when that generation is no
    /// longer the instance's current Ready, unfailed group.
    pub fn record_group_stall(
        &self,
        s: &CoordinatorSession,
        deployment_id: &str,
        instance_index: u32,
        generation: i64,
    ) -> Result<bool, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let Some((_, operation_id, _)) =
            serving_group(&tx, deployment_id, instance_index, generation)?
        else {
            return Ok(false);
        };
        // SPEC §13.2: dispatch to this incarnation closes first; ownership,
        // every member's charge and request leases all stay.
        tx.execute(
            "UPDATE deployment_instances SET dispatch_enabled=0
              WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3",
            params![deployment_id, instance_index, generation],
        )?;
        // W10 gap (a): recorded, so no switch or re-proof reopens it. The
        // closure reasons are a closed set; a group no longer serving is the
        // exit case, as for a failed park (R42).
        crate::switch_state::record_closure(
            &tx,
            deployment_id,
            instance_index,
            generation,
            crate::switch_state::ClosureReason::EngineExit,
        )?;
        crate::groups::record_failure_code(
            &tx,
            deployment_id,
            instance_index,
            generation,
            0,
            GROUP_STALLED,
        )
        .map_err(|_| LifecycleError::CorruptStoredData)?;
        let head: Option<String> = tx
            .query_row(
                "SELECT host_id FROM group_members
                  WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 AND rank=0",
                params![deployment_id, instance_index, generation],
                |r| r.get(0),
            )
            .optional()?;
        tx.execute(
            "INSERT INTO journal_entries(id,host_id,operation_id,state,evidence) VALUES(?1,?2,?3,?4,?5)",
            params![
                ulid::Ulid::new().to_string(),
                head,
                operation_id,
                GROUP_STALLED,
                format!(
                    "deployment {deployment_id} instance {instance_index} generation \
                     {generation}: a request got no first token within the stall timeout \
                     and the completion probe through the head failed; dispatch closed, \
                     every member stays charged until the group stop proves it gone on \
                     its own host"
                ),
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// ADR 0028 §11 (R42 pattern): every instance whose current group
    /// stalled (recorded by [`Self::record_group_stall`]) and that still
    /// reads desired ready: no stop has been accepted for it since (an
    /// accepted stop draws a new generation). The coordinator retries each
    /// one's stop until it is accepted.
    pub fn group_stall_stops_due(
        &self,
        s: &CoordinatorSession,
    ) -> Result<Vec<GroupStallStop>, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, s)?;
        let due = tx
            .prepare(
                "SELECT i.deployment_id,i.instance_index,i.generation FROM deployment_instances i
                   JOIN group_plans g ON g.deployment_id=i.deployment_id AND g.instance_index=i.instance_index AND g.generation=i.generation
                  WHERE g.failure_code=?1 AND g.state='active' AND i.desired_state='ready'
                  ORDER BY i.deployment_id,i.instance_index",
            )?
            .query_map([GROUP_STALLED], |r| {
                Ok(GroupStallStop {
                    deployment_id: r.get(0)?,
                    instance_index: r.get(1)?,
                    generation: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        tx.commit()?;
        Ok(due)
    }

    /// ADR 0035 §7: the instance on several GPUs a stall report for
    /// `instance_index` of `deployment_id` at `generation` names, when that
    /// generation is still its current Ready launch (no group's) and it has
    /// not failed `tp_stalled` already. `None` for any other report (a past
    /// or future generation, an instance on one GPU, a group), which is
    /// ignored.
    pub fn stalled_local(
        &self,
        deployment_id: &str,
        instance_index: u32,
        generation: i64,
    ) -> Result<Option<StalledLocal>, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let Some((revision, operation_id, step_id, binding_id, incarnation)) =
            serving_local(&tx, deployment_id, instance_index, generation)?
        else {
            return Ok(None);
        };
        tx.commit()?;
        let identities = self.runtime_binding_identities(&binding_id)?;
        Ok(Some(StalledLocal {
            deployment_id: deployment_id.to_owned(),
            instance_index,
            generation,
            revision,
            operation_id,
            step_id,
            binding_id,
            incarnation,
            identities,
        }))
    }

    /// ADR 0035 §7: the instance on several GPUs at `generation` stalled
    /// and failed its probe. In one transaction its dispatch closes with a
    /// recorded reason, its status names [`TP_STALLED`] and the journal says
    /// why; every GPU stays charged. Returns whether it was recorded:
    /// `false` when that generation is no longer its current Ready launch
    /// or already failed so.
    pub fn record_tp_stall(
        &self,
        s: &CoordinatorSession,
        deployment_id: &str,
        instance_index: u32,
        generation: i64,
    ) -> Result<bool, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let Some((_, operation_id, _, binding_id, _)) =
            serving_local(&tx, deployment_id, instance_index, generation)?
        else {
            return Ok(false);
        };
        // SPEC §13.2: dispatch to this incarnation closes first; ownership,
        // every GPU's charge and request leases all stay.
        tx.execute(
            "UPDATE deployment_instances SET dispatch_enabled=0,last_error=?4
              WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3",
            params![deployment_id, instance_index, generation, TP_STALLED],
        )?;
        // W10 gap (a): recorded, so no switch or re-proof reopens it (the
        // closed set's case for an instance no longer serving).
        crate::switch_state::record_closure(
            &tx,
            deployment_id,
            instance_index,
            generation,
            crate::switch_state::ClosureReason::EngineExit,
        )?;
        let host: Option<String> = tx
            .query_row(
                "SELECT host_id FROM remote_binding_ingress WHERE binding_id=?1",
                [&binding_id],
                |r| r.get(0),
            )
            .optional()?;
        tx.execute(
            "INSERT INTO journal_entries(id,host_id,operation_id,state,evidence) VALUES(?1,?2,?3,?4,?5)",
            params![
                ulid::Ulid::new().to_string(),
                host,
                operation_id,
                TP_STALLED,
                format!(
                    "deployment {deployment_id} instance {instance_index} generation \
                     {generation}: a request got no first token within the stall timeout \
                     and the completion probe through the instance's own endpoint failed; \
                     dispatch closed, every GPU stays charged until the stop proves every \
                     recorded process gone"
                ),
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// ADR 0035 §7 (R42 pattern): every instance on several GPUs that
    /// failed `tp_stalled` at its current generation and still reads desired
    /// ready: no stop has been accepted for it since (an accepted stop draws
    /// a new generation). The coordinator retries each one's stop until it
    /// is accepted.
    pub fn tp_stall_stops_due(
        &self,
        s: &CoordinatorSession,
    ) -> Result<Vec<GroupStallStop>, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, s)?;
        let due = tx
            .prepare(
                "SELECT i.deployment_id,i.instance_index,i.generation FROM deployment_instances i
                  WHERE i.last_error=?1 AND i.desired_state='ready'
                    AND EXISTS(SELECT 1 FROM lifecycle_runs r
                                 JOIN journal_entries j ON j.operation_id=r.operation_id
                                WHERE r.deployment_id=i.deployment_id AND r.instance_index=i.instance_index
                                  AND r.generation=i.generation AND j.state=?1)
                  ORDER BY i.deployment_id,i.instance_index",
            )?
            .query_map([TP_STALLED], |r| {
                Ok(GroupStallStop {
                    deployment_id: r.get(0)?,
                    instance_index: r.get(1)?,
                    generation: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        tx.commit()?;
        Ok(due)
    }
}

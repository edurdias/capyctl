//! ADR 0032 §7 (owner decision 7 of 2026-10-21): restart when full.
//!
//! A cancelled request the engine never answered, or one on a custom engine
//! that reports no idle signal, settles only at its instance's next verified
//! exit (SPEC §10, amended 2026-10-21; ADR 0031 §6). When such requests are a
//! deployment's only open leases and fill its limit, nothing else would ever
//! free a place, so each instance holding them is restarted: an ordinary stop
//! with verified cleanup, whose exit settles them (`cleanup::complete`, with
//! its own receipt), and then a start again, the pending start the scheduler's
//! reconciliation places once the instance holds nothing (the same restart a
//! non-count revision asks for, owner decision Q8).
//!
//! The trigger is the count reaching the limit, never a timer. An uncertain
//! lease is not such a request, so one keeps the restart from being
//! triggered, as does any live lease. A stop that cannot verify the exit keeps
//! the charges and the reservation, as every stop does. The classification,
//! the stops and the journal are one transaction, so concurrent callers never
//! stop an instance twice: an instance already stopping is not asked again.
use std::collections::BTreeMap;

use super::cleanup::{instance_scope, StopCommand};
use super::*;
use crate::request_lease_cancellations::cancelling_settled_at_stop;

/// The principal a restart for held requests acts as.
pub const HELD_RESTART_PRINCIPAL: &str = "system:held_requests";

/// One instance whose restart a call accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldInstanceRestart {
    pub instance_index: u32,
    /// The incarnation stopped; the held leases are charged to it.
    pub generation: i64,
    /// The held requests its verified exit settles.
    pub count: usize,
    /// The stop operation.
    pub operation_id: String,
}

/// What [`crate::Store::restart_for_held_requests`] found and did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeldRestartDecision {
    /// Not every open lease is a cancellation held until the next stop, or
    /// they do not fill the limit: nothing was accepted.
    NotHeld,
    /// Every instance holding them is restarting: `accepted` names the ones
    /// this call stopped (empty when every one was already stopping).
    Restarting { accepted: Vec<HeldInstanceRestart> },
}

impl crate::Store {
    /// ADR 0032 §7: when the open leases of `deployment` number at least
    /// `limit` (its per-deployment bound) and every one is an in-flight
    /// cancellation of this session held until its instance's next stop
    /// (`settled_at_stop`), accept a restart of each instance holding them
    /// that is not already stopping, and journal
    /// `instance_restarted_for_held_requests` for each, in one transaction.
    pub fn restart_for_held_requests(
        &self,
        s: &CoordinatorSession,
        deployment: &str,
        limit: usize,
        now: i64,
    ) -> Result<HeldRestartDecision, LifecycleError> {
        if deployment.is_empty() || limit == 0 || now < 0 {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        // Every open row of the deployment, uncertain ones included: the same
        // rows the grant's bound counts (`dispatch::outstanding`).
        let rows: Vec<(u32, i64, bool)> = tx
            .prepare(&format!(
                "SELECT l.instance_index,l.generation,
                        (l.disposition='inflight' AND l.session_id=?2 AND {})
                   FROM request_leases l WHERE l.deployment_id=?1",
                cancelling_settled_at_stop("l")
            ))?
            .query_map(params![deployment, s.id()], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<Result<_, _>>()?;
        if rows.len() < limit || rows.iter().any(|(_, _, held)| !held) {
            return Ok(HeldRestartDecision::NotHeld);
        }
        // (generation, held count) per instance.
        let mut holding: BTreeMap<u32, (i64, usize)> = BTreeMap::new();
        for (instance, generation, _) in rows {
            let entry = holding.entry(instance).or_insert((generation, 0));
            // A lease on an earlier incarnation of the instance is not proven
            // by the current one's exit (`cleanup::complete` refuses it).
            if entry.0 != generation {
                return Ok(HeldRestartDecision::NotHeld);
            }
            entry.1 += 1;
        }
        let fences = super::receipt::runtime_fences(&tx, deployment)?;
        let mut stops = Vec::new();
        for (instance, (generation, count)) in holding {
            if super::recorded_stop::stopping(&tx, deployment, instance)? {
                continue;
            }
            // The stop must be of exactly the incarnation the leases are
            // charged to, or its exit settles none of them.
            let Some(fence) = fences
                .iter()
                .find(|(index, fence)| *index == instance && fence.generation == generation)
                .map(|(_, fence)| fence.clone())
            else {
                return Ok(HeldRestartDecision::NotHeld);
            };
            stops.push((instance, fence, count));
        }
        let revision: i64 = tx
            .query_row(
                "SELECT revision FROM deployments WHERE id=?1",
                [deployment],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(LifecycleError::Conflict)?;
        let mut accepted = Vec::with_capacity(stops.len());
        for (instance, fence, count) in stops {
            // The stop and the restart's placement each get the request
            // deadline, as the reconciliation's restart does; a slower
            // verified cleanup pushes the pending start on (`reanchor`).
            let (_, e) = effective(&tx, &fence)?;
            let deadline = now.saturating_add(e.request_deadline_ms);
            let receipt = Self::accept_instance_stop_in_transaction(
                &tx,
                s,
                HELD_RESTART_PRINCIPAL,
                &fence,
                &format!("held-restart:{instance}:{}", fence.generation),
                now,
                deadline,
                &StopCommand {
                    scope: Some(instance_scope(deployment, instance)),
                    revision: Some(revision),
                },
            )?;
            tx.execute(
                "UPDATE deployment_instances SET pending_start_until_ms=?3,last_error=NULL
                  WHERE deployment_id=?1 AND instance_index=?2",
                params![deployment, instance, deadline],
            )?;
            crate::events::append_event(
                &tx,
                &crate::events::EventMetadata::InstanceRestartedForHeldRequests {
                    deployment_id: deployment.to_owned(),
                    operation_id: receipt.operation_id.clone(),
                    instance_index: instance,
                    generation: fence.generation,
                    count,
                },
            )
            .map_err(|error| match error {
                crate::events::EventWriteError::Sql(error) => LifecycleError::Sql(error),
                _ => LifecycleError::CorruptStoredData,
            })?;
            accepted.push(HeldInstanceRestart {
                instance_index: instance,
                generation: fence.generation,
                count,
                operation_id: receipt.operation_id,
            });
        }
        tx.commit()?;
        Ok(HeldRestartDecision::Restarting { accepted })
    }
}

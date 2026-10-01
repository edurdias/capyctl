use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoordinatorSession {
    epoch: i64,
    id: String,
}

impl CoordinatorSession {
    pub fn epoch(&self) -> i64 {
        self.epoch
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error("stale coordinator session")]
    StaleSession,
    #[error("deployment revision or generation changed")]
    Conflict,
    #[error("dispatch admission is closed")]
    Closed,
    #[error("outstanding work limit reached")]
    Full,
    #[error("invalid dispatch parameters or stored data")]
    Invalid,
}

impl crate::Store {
    pub fn begin_coordinator_session(&self) -> Result<CoordinatorSession, DispatchError> {
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let old: i64 = transaction.query_row(
            "SELECT epoch FROM coordinator_session WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        let epoch = old
            .checked_add(1)
            .filter(|e| *e > 0)
            .ok_or(DispatchError::Invalid)?;
        let session = CoordinatorSession {
            epoch,
            id: ulid::Ulid::new().to_string(),
        };
        transaction.execute(
            "UPDATE coordinator_session SET epoch=?1,session_id=?2 WHERE singleton=1",
            params![session.epoch, session.id],
        )?;
        transaction.execute("UPDATE deployments SET dispatch_enabled=0", [])?;
        // ADR 0013 §5: every instance's dispatch closes with its session.
        transaction.execute(
            "UPDATE deployment_instances SET dispatch_enabled=0 WHERE dispatch_enabled=1",
            [],
        )?;
        // W10: no switch survives the process that ran it; its closures go
        // with it (every gate is closed above until re-proven anyway).
        crate::switch_state::clear_switches(&transaction)?;
        transaction.execute("UPDATE request_leases SET disposition='uncertain'", [])?;
        transaction.execute("UPDATE lifecycle_runs SET state='uncertain' WHERE operation_id IN (SELECT operation_id FROM lifecycle_steps WHERE state='armed') AND state IN ('queued','running')", [])?;
        transaction.execute(
            "UPDATE lifecycle_steps SET state='uncertain' WHERE state='armed'",
            [],
        )?;
        crate::events::append_event(
            &transaction,
            &crate::events::EventMetadata::CoordinatorSessionStarted {
                session_epoch: epoch,
            },
        )
        .map_err(|error| match error {
            crate::events::EventWriteError::Sql(error) => DispatchError::Sql(error),
            _ => DispatchError::Invalid,
        })?;
        transaction.commit()?;
        Ok(session)
    }
}

pub(crate) fn check_session(
    conn: &Connection,
    session: &CoordinatorSession,
) -> Result<(), DispatchError> {
    let current: (i64, String) = conn.query_row(
        "SELECT epoch,session_id FROM coordinator_session WHERE singleton=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if session.epoch <= 0 || session.id.is_empty() || current != (session.epoch, session.id.clone())
    {
        return Err(DispatchError::StaleSession);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct DispatchRequest<'a> {
    pub deployment_id: &'a str,
    pub revision: i64,
    pub generation: i64,
    pub max_per_deployment: usize,
    pub max_total: usize,
}

#[must_use = "a dispatch ticket represents retained backend work"]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchTicket {
    id: String,
    deployment_id: String,
    revision: i64,
    generation: i64,
    session_id: String,
}
impl DispatchTicket {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn deployment_id(&self) -> &str {
        &self.deployment_id
    }
    pub fn revision(&self) -> i64 {
        self.revision
    }
    pub fn generation(&self) -> i64 {
        self.generation
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDispatch {
    pub id: String,
    pub revision: i64,
    pub generation: i64,
    pub session_id: String,
    pub uncertain: bool,
}

fn settle_ticket(
    conn: &Connection,
    session: &CoordinatorSession,
    ticket: &DispatchTicket,
    complete: bool,
) -> Result<bool, DispatchError> {
    let transaction = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    check_session(&transaction, session)?;
    let settled = settle_in(&transaction, session, ticket, complete)?;
    transaction.commit()?;
    Ok(settled)
}

impl crate::Store {
    pub fn pending_dispatches(
        &self,
        deployment: &str,
    ) -> Result<Vec<PendingDispatch>, DispatchError> {
        let mut statement = self.conn.prepare(
            "SELECT id,revision,generation,session_id,disposition FROM request_leases
             WHERE deployment_id=?1 ORDER BY id",
        )?;
        let rows = statement.query_map([deployment], |r| {
            let disposition: String = r.get(4)?;
            Ok(PendingDispatch {
                id: r.get(0)?,
                revision: r.get(1)?,
                generation: r.get(2)?,
                session_id: r.get(3)?,
                uncertain: disposition != "inflight",
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn mark_dispatch_uncertain(
        &self,
        session: &CoordinatorSession,
        ticket: &DispatchTicket,
    ) -> Result<bool, DispatchError> {
        settle_ticket(&self.conn, session, ticket, false)
    }

    pub fn finish_dispatch(
        &self,
        session: &CoordinatorSession,
        ticket: &DispatchTicket,
    ) -> Result<bool, DispatchError> {
        settle_ticket(&self.conn, session, ticket, true)
    }
}

fn outstanding(conn: &Connection, deployment: &str) -> Result<usize, DispatchError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM request_leases WHERE deployment_id=?1",
        [deployment],
        |r| r.get(0),
    )?;
    usize::try_from(count).map_err(|_| DispatchError::Invalid)
}

/// Which instance a grant is charged to.
#[derive(Debug, Clone, Copy)]
enum GrantFence {
    /// The lowest-index instance whose gate is open (a caller that does not
    /// choose an instance: the single-instance and legacy paths).
    Current,
    /// Exactly this revision and generation.
    Exact(i64, i64),
    /// ADR 0013 §10 (I3): exactly the instance incarnation the router chose,
    /// by its generation, on whatever revision it runs.
    Instance(i64),
}

/// Grant one lease inside an open, session-checked transaction. `fence` names
/// the instance the lease is charged to. Every refusal is decided before the
/// insert, so a refused grant leaves nothing behind in a shared batch
/// transaction.
fn grant_in(
    transaction: &Connection,
    session: &CoordinatorSession,
    deployment_id: &str,
    fence: GrantFence,
    max_per_deployment: usize,
    max_total: usize,
) -> Result<DispatchTicket, DispatchError> {
    let bad_fence = match fence {
        GrantFence::Current => false,
        GrantFence::Exact(revision, generation) => revision < 1 || generation < 1,
        GrantFence::Instance(generation) => generation < 1,
    };
    if deployment_id.is_empty() || bad_fence || max_per_deployment == 0 || max_total == 0 {
        return Err(DispatchError::Invalid);
    }
    // ADR 0013 §5, §10: a lease is charged to one instance. With a fence it is
    // the instance that fence names; without one, the lowest-index instance
    // whose gate is open.
    let row: Option<(i64, i64, String, i64, i64, u32)> = match fence {
        GrantFence::Instance(generation) => transaction
            .query_row(
                "SELECT revision,current_generation,observed_state,
                CASE WHEN admission_enabled=1 AND dispatch_enabled=1 THEN 1 ELSE 0 END,suspended,instance_index
             FROM instance_runtime WHERE id=?1 AND revision IS NOT NULL AND current_generation=?2",
                params![deployment_id, generation],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .optional()?,
        GrantFence::Exact(revision, generation) => transaction
            .query_row(
                "SELECT revision,current_generation,observed_state,
                CASE WHEN admission_enabled=1 AND dispatch_enabled=1 THEN 1 ELSE 0 END,suspended,instance_index
             FROM instance_runtime WHERE id=?1 AND revision=?2 AND current_generation=?3",
                params![deployment_id, revision, generation],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .optional()?,
        GrantFence::Current => transaction
            .query_row(
                "SELECT revision,current_generation,observed_state,
                CASE WHEN admission_enabled=1 AND dispatch_enabled=1 THEN 1 ELSE 0 END,suspended,instance_index
             FROM instance_runtime WHERE id=?1 AND revision IS NOT NULL AND current_generation IS NOT NULL
             ORDER BY (observed_state='ready' AND admission_enabled=1 AND dispatch_enabled=1) DESC, instance_index LIMIT 1",
                [deployment_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .optional()?,
    };
    let Some((revision, generation, state, enabled, suspended, instance)) = row else {
        return Err(DispatchError::Conflict);
    };
    // SPEC §10, T18: late work is refused once the gate closes, whatever the
    // router believed when it resolved the route.
    if state != "ready" || enabled != 1 || suspended != 0 {
        return Err(DispatchError::Closed);
    }
    let total: i64 =
        transaction.query_row("SELECT COUNT(*) FROM request_leases", [], |r| r.get(0))?;
    if outstanding(transaction, deployment_id)? >= max_per_deployment
        || usize::try_from(total).map_err(|_| DispatchError::Invalid)? >= max_total
    {
        return Err(DispatchError::Full);
    }
    let ticket = DispatchTicket {
        id: ulid::Ulid::new().to_string(),
        deployment_id: deployment_id.into(),
        revision,
        generation,
        session_id: session.id.clone(),
    };
    transaction.execute("INSERT INTO request_leases(id,deployment_id,revision,generation,session_id,disposition,instance_index)
        VALUES (?1,?2,?3,?4,?5,'inflight',?6)",
        params![ticket.id, ticket.deployment_id, ticket.revision, ticket.generation, ticket.session_id, instance])?;
    Ok(ticket)
}

/// Settle one lease inside an open, session-checked transaction.
fn settle_in(
    transaction: &Connection,
    session: &CoordinatorSession,
    ticket: &DispatchTicket,
    complete: bool,
) -> Result<bool, DispatchError> {
    if ticket.session_id != session.id {
        return Err(DispatchError::StaleSession);
    }
    let sql = if complete {
        "DELETE FROM request_leases WHERE id=?1 AND deployment_id=?2 AND revision=?3
         AND generation=?4 AND session_id=?5"
    } else {
        "UPDATE request_leases SET disposition='uncertain' WHERE id=?1 AND deployment_id=?2
         AND revision=?3 AND generation=?4 AND session_id=?5"
    };
    let changed = transaction.execute(
        sql,
        params![
            ticket.id,
            ticket.deployment_id,
            ticket.revision,
            ticket.generation,
            ticket.session_id
        ],
    )?;
    Ok(changed == 1)
}

/// SPEC §10 (amended 2026-10-01): mark this session's in-flight lease as
/// cancelling. It stays `inflight`; only the engine's quiescence closes it.
fn cancel_in(
    transaction: &Connection,
    session: &CoordinatorSession,
    ticket: &DispatchTicket,
) -> Result<bool, DispatchError> {
    if ticket.session_id != session.id {
        return Err(DispatchError::StaleSession);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64;
    let changed = transaction.execute(
        "INSERT OR IGNORE INTO request_lease_cancellations(lease_id,cancelled_at_ms)
         SELECT id,?6 FROM request_leases WHERE id=?1 AND deployment_id=?2 AND revision=?3
         AND generation=?4 AND session_id=?5 AND disposition='inflight'",
        params![
            ticket.id,
            ticket.deployment_id,
            ticket.revision,
            ticket.generation,
            ticket.session_id,
            now
        ],
    )?;
    Ok(changed == 1)
}

/// One durable request-lease write in a group commit (SPEC §10).
#[derive(Debug, Clone)]
pub enum LeaseWrite {
    /// Open a lease against the deployment's current fence, only while its
    /// dispatch gate is open and the bounds allow it.
    Grant {
        deployment_id: String,
        max_per_deployment: usize,
        max_total: usize,
    },
    /// ADR 0013 §10 (I3): open a lease charged to exactly the instance
    /// incarnation `generation` names, only while that instance's gate is open.
    /// A router that chose an instance forwards to that instance alone, so the
    /// lease and the forward can never name different incarnations.
    GrantInstance {
        deployment_id: String,
        generation: i64,
        max_per_deployment: usize,
        max_total: usize,
    },
    /// Close a lease on completion observation, cancellation acknowledgement,
    /// or evidence the request never reached the engine.
    Finish(DispatchTicket),
    /// Keep a lease charged with its outcome unknown.
    Uncertain(DispatchTicket),
    /// SPEC §10 (amended 2026-10-01): the client hung up and the engine
    /// connection closed. The lease stays `inflight` and charged until the
    /// engine reports quiescence.
    Cancel(DispatchTicket),
}

/// What one `LeaseWrite` in a batch produced.
#[derive(Debug)]
pub enum LeaseWriteOutcome {
    Granted(DispatchTicket),
    /// Whether the lease was present to settle.
    Settled(bool),
}

impl crate::Store {
    /// Apply a batch of request-lease writes in one transaction: one commit and
    /// one fsync for the whole group. Each write's refusal is its own and leaves
    /// the others untouched; a storage failure fails the batch as a whole, and
    /// then nothing in it was written.
    pub fn apply_request_lease_batch(
        &self,
        session: &CoordinatorSession,
        writes: &[LeaseWrite],
    ) -> Result<Vec<Result<LeaseWriteOutcome, DispatchError>>, DispatchError> {
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&transaction, session)?;
        let mut outcomes = Vec::with_capacity(writes.len());
        for write in writes {
            let outcome = match write {
                LeaseWrite::Grant {
                    deployment_id,
                    max_per_deployment,
                    max_total,
                } => grant_in(
                    &transaction,
                    session,
                    deployment_id,
                    GrantFence::Current,
                    *max_per_deployment,
                    *max_total,
                )
                .map(LeaseWriteOutcome::Granted),
                LeaseWrite::GrantInstance {
                    deployment_id,
                    generation,
                    max_per_deployment,
                    max_total,
                } => grant_in(
                    &transaction,
                    session,
                    deployment_id,
                    GrantFence::Instance(*generation),
                    *max_per_deployment,
                    *max_total,
                )
                .map(LeaseWriteOutcome::Granted),
                LeaseWrite::Finish(ticket) => {
                    settle_in(&transaction, session, ticket, true).map(LeaseWriteOutcome::Settled)
                }
                LeaseWrite::Uncertain(ticket) => {
                    settle_in(&transaction, session, ticket, false).map(LeaseWriteOutcome::Settled)
                }
                LeaseWrite::Cancel(ticket) => {
                    cancel_in(&transaction, session, ticket).map(LeaseWriteOutcome::Settled)
                }
            };
            // A storage error inside the transaction poisons the whole group.
            if let Err(DispatchError::Sql(error)) = outcome {
                return Err(DispatchError::Sql(error));
            }
            outcomes.push(outcome);
        }
        transaction.commit()?;
        Ok(outcomes)
    }

    pub fn grant_dispatch(
        &self,
        session: &CoordinatorSession,
        request: DispatchRequest<'_>,
    ) -> Result<DispatchTicket, DispatchError> {
        if request.revision < 1 || request.generation < 1 {
            return Err(DispatchError::Invalid);
        }
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&transaction, session)?;
        let ticket = grant_in(
            &transaction,
            session,
            request.deployment_id,
            GrantFence::Exact(request.revision, request.generation),
            request.max_per_deployment,
            request.max_total,
        )?;
        transaction.commit()?;
        Ok(ticket)
    }

    pub fn close_dispatch(
        &self,
        session: &CoordinatorSession,
        deployment: &str,
        revision: i64,
        generation: i64,
    ) -> Result<usize, DispatchError> {
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&transaction, session)?;
        let changed = transaction.execute(
            "UPDATE deployment_instances SET dispatch_enabled=0
            WHERE deployment_id=?1 AND revision=?2 AND generation=?3",
            params![deployment, revision, generation],
        )?;
        if changed != 1 {
            return Err(DispatchError::Conflict);
        }
        let count = outstanding(&transaction, deployment)?;
        transaction.commit()?;
        Ok(count)
    }
}

#[cfg(test)]
mod tests;

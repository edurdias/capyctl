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

/// Verified disappearance may settle old sessions only for the single-binding candidate lane.
pub(crate) fn settle_verified_candidate_cleanup(
    tx: &rusqlite::Transaction<'_>,
    deployment: &str,
    binding: &str,
) -> Result<(), crate::lifecycle::LifecycleError> {
    let exact:bool=tx.query_row("SELECT COUNT(*)=1 AND COALESCE(SUM(id=?2),0)=1 FROM runtime_bindings WHERE deployment_id=?1",params![deployment,binding],|r|r.get(0))?;
    if !exact {
        return Err(crate::lifecycle::LifecycleError::Conflict);
    }
    tx.execute(
        "DELETE FROM request_leases WHERE deployment_id=?1",
        [deployment],
    )?;
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
    transaction.commit()?;
    Ok(changed == 1)
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

impl crate::Store {
    pub fn grant_dispatch(
        &self,
        session: &CoordinatorSession,
        request: DispatchRequest<'_>,
    ) -> Result<DispatchTicket, DispatchError> {
        if request.deployment_id.is_empty()
            || request.revision < 1
            || request.generation < 1
            || request.max_per_deployment == 0
            || request.max_total == 0
        {
            return Err(DispatchError::Invalid);
        }
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&transaction, session)?;
        let row: Option<(i64, i64, String, i64, i64)> = transaction
            .query_row(
                "SELECT revision,current_generation,observed_state,
                CASE WHEN admission_enabled=1 AND dispatch_enabled=1 THEN 1 ELSE 0 END,suspended
             FROM deployments WHERE id=?1",
                [request.deployment_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let Some((revision, generation, state, enabled, suspended)) = row else {
            return Err(DispatchError::Conflict);
        };
        if revision != request.revision || generation != request.generation {
            return Err(DispatchError::Conflict);
        }
        if state != "ready" || enabled != 1 || suspended != 0 {
            return Err(DispatchError::Closed);
        }
        let total: i64 =
            transaction.query_row("SELECT COUNT(*) FROM request_leases", [], |r| r.get(0))?;
        if outstanding(&transaction, request.deployment_id)? >= request.max_per_deployment
            || usize::try_from(total).map_err(|_| DispatchError::Invalid)? >= request.max_total
        {
            return Err(DispatchError::Full);
        }
        let ticket = DispatchTicket {
            id: ulid::Ulid::new().to_string(),
            deployment_id: request.deployment_id.into(),
            revision,
            generation,
            session_id: session.id.clone(),
        };
        transaction.execute("INSERT INTO request_leases(id,deployment_id,revision,generation,session_id,disposition)
            VALUES (?1,?2,?3,?4,?5,'inflight')",
            params![ticket.id, ticket.deployment_id, ticket.revision, ticket.generation, ticket.session_id])?;
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
            "UPDATE deployments SET dispatch_enabled=0
            WHERE id=?1 AND revision=?2 AND current_generation=?3",
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

use rusqlite::{params, Transaction, TransactionBehavior};

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
        transaction.commit()?;
        Ok(session)
    }
}

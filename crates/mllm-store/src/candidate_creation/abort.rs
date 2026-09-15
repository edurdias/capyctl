//! Abort closes admission. It conveys no release, completion or cleanup authority.
use super::*;
type AbortResult<T> = std::result::Result<T, LifecycleError>;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    expected_revision: i64,
    action: Abort,
    deadline_ms: i64,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Abort {
    Abort,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateAbortReceipt {
    version: u8,
    source: ReceiptV1,
    operation_id: String,
    session_id: String,
    session_epoch: i64,
    idempotency_key: String,
    request_hash: String,
    source_state: String,
    accepted_at_ms: i64,
    deadline_ms: i64,
}
impl CandidateAbortReceipt {
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }
    pub fn deployment_id(&self) -> &str {
        &self.source.deployment_id
    }
    pub fn run_id(&self) -> &str {
        &self.source.run_id
    }
    pub fn revision(&self) -> i64 {
        self.source.revision
    }
}
fn scope(run: &str) -> String {
    format!("POST /management/v1/qualification-runs/{run}/actions")
}
fn error(e: CandidateCreationError) -> LifecycleError {
    initialize::CandidateInitializeError::from(e).into()
}
fn session(tx: &Transaction<'_>, session: &CoordinatorSession) -> AbortResult<()> {
    let (epoch, id) = tx.query_row(
        "SELECT epoch,session_id FROM coordinator_session WHERE singleton=1",
        [],
        |r| Ok((r.get::<_, i64>(0)?, bounded(r, 1, 26))),
    )?;
    if epoch != session.epoch() || epoch < 1 || id? != session.id() {
        return Err(LifecycleError::Stale);
    }
    Ok(())
}
fn command(principal: &str, run: &str, key: &str, text: &str) -> AbortResult<Command> {
    if !valid_id(principal) || !ulid(run) || !valid_id(key) || text.len() > MAX_BYTES {
        return Err(LifecycleError::Invalid);
    }
    let c: Command = serde_json::from_str(text).map_err(|_| LifecycleError::Invalid)?;
    if c.expected_revision < 1 || c.deadline_ms < 1 {
        return Err(LifecycleError::Invalid);
    }
    Ok(c)
}
fn hash(principal: &str, run: &str, c: &Command) -> AbortResult<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(
            encode(&(1_u8, "candidate_abort", principal, scope(run), c)).map_err(error)?
        )
    ))
}
fn bounded(row: &rusqlite::Row<'_>, index: usize, max: usize) -> AbortResult<String> {
    let rusqlite::types::ValueRef::Text(bytes) = row.get_ref(index)? else {
        return Err(LifecycleError::CorruptStoredData);
    };
    if bytes.len() > max {
        return Err(LifecycleError::CorruptStoredData);
    }
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| LifecycleError::CorruptStoredData)
}
// Bound the legacy snapshot decoder's inputs before it allocates any source DTO.
fn source(tx: &Transaction<'_>, principal: &str, run: &str) -> AbortResult<CandidateRunSnapshot> {
    for query in [
        "SELECT host_id,deployment_id,binding_id,incarnation,operation_id,recipe_digest,authorization_json,state,cleanup_state,coalesce(cleanup_step_id,'') FROM qualification_runs WHERE id=?1 AND principal_id=?2",
        "SELECT c.principal_id,c.command_scope,c.idempotency_key,c.request_hash,c.response_json FROM command_receipts c JOIN qualification_runs q ON q.operation_id=c.operation_id WHERE q.id=?1 AND q.principal_id=?2 LIMIT 2",
        "SELECT e.effective_json,e.fingerprint FROM effective_revisions e JOIN qualification_runs q ON q.deployment_id=e.deployment_id AND q.revision=e.revision WHERE q.id=?1 AND q.principal_id=?2",
        "SELECT b.deployment_id,b.incarnation,b.ownership,b.binding_json,b.state FROM runtime_bindings b JOIN qualification_runs q ON q.binding_id=b.id WHERE q.id=?1 AND q.principal_id=?2",
        "SELECT e.host FROM endpoint_leases e JOIN qualification_runs q ON q.binding_id=e.binding_id WHERE q.id=?1 AND q.principal_id=?2 LIMIT 2",
    ] {
        let mut statement = tx.prepare(query)?;
        let columns = statement.column_count();
        let mut rows = statement.query(params![run, principal])?;
        let mut count = 0;
        while let Some(row) = rows.next()? {
            count += 1;
            if count > 1 { return Err(LifecycleError::CorruptStoredData); }
            for index in 0..columns {
                let rusqlite::types::ValueRef::Text(bytes) = row.get_ref(index)? else { return Err(LifecycleError::CorruptStoredData); };
                if bytes.len() > MAX_BYTES || std::str::from_utf8(bytes).is_err() { return Err(LifecycleError::CorruptStoredData); }
            }
        }
    }
    read_snapshot(tx, principal, run)
        .map_err(error)?
        .ok_or(LifecycleError::NotFound)
}
fn event(r: &CandidateAbortReceipt) -> AbortResult<EventMetadata> {
    let id = |s: &str| {
        s.parse()
            .map(EventOperationId::generated)
            .map_err(|_| LifecycleError::CorruptStoredData)
    };
    Ok(EventMetadata::CandidateAbortAccepted {
        operation_id: id(&r.operation_id)?,
        deployment_id: id(r.deployment_id())?,
        run_id: id(r.run_id())?,
        session_epoch: r.session_epoch,
    })
}
fn prior(
    tx: &Transaction<'_>,
    principal: &str,
    run: &str,
    key: &str,
    expected: &str,
) -> AbortResult<Option<CandidateAbortReceipt>> {
    let row = tx.query_row("SELECT request_hash,operation_id,response_json FROM command_receipts WHERE principal_id=?1 AND command_scope=?2 AND idempotency_key=?3",params![principal,scope(run),key], |r| Ok((bounded(r,0,64),bounded(r,1,26),bounded(r,2,MAX_BYTES)))).optional()?;
    let Some((h, op, raw)) = row else {
        return Ok(None);
    };
    let (h, op, raw) = (h?, op?, raw?);
    if h != expected {
        return Err(LifecycleError::IdempotencyConflict);
    }
    let r: CandidateAbortReceipt = decode(&raw).map_err(error)?;
    let snapshot = source(tx, principal, run)?;
    let c = Command {
        expected_revision: r.revision(),
        action: Abort::Abort,
        deadline_ms: r.deadline_ms,
    };
    let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1 AND deployment_id=?2 AND kind='candidate_abort_v1' AND state='succeeded' AND error_code IS NULL AND idempotency_key IS NULL) AND (SELECT count(*) FROM command_receipts WHERE operation_id=?1)=1",params![op,r.deployment_id()],|row|row.get(0))?;
    let (epoch, session) = tx.query_row(
        "SELECT epoch,session_id FROM coordinator_session WHERE singleton=1",
        [],
        |row| Ok((row.get::<_, i64>(0)?, bounded(row, 1, 26))),
    )?;
    if r.version != 1
        || r.source != snapshot.receipt().inner
        || r.operation_id != op
        || !ulid(&op)
        || r.idempotency_key != key
        || r.request_hash != h
        || hash(principal, run, &c)? != h
        || !ulid(&r.session_id)
        || r.session_epoch < 1
        || r.session_epoch > epoch
        || (r.session_epoch == epoch && r.session_id != session?)
        || !matches!(
            r.source_state.as_str(),
            "accepted" | "running" | "uncertain"
        )
        || snapshot.state() != CandidateRunState::Aborted
        || r.accepted_at_ms < r.source.accepted_at_ms
        || r.deadline_ms > r.source.deadline_ms
        || r.accepted_at_ms >= r.deadline_ms
        || !valid
        || encode(&r).map_err(error)? != raw
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok(Some(r))
}
impl crate::Store {
    pub fn candidate_abort_command_receipt(
        &self,
        session: &CoordinatorSession,
        principal: &str,
        run: &str,
        key: &str,
        text: &str,
    ) -> AbortResult<Option<CandidateAbortReceipt>> {
        let c = command(principal, run, key, text)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        self::session(&tx, session)?;
        prior(&tx, principal, run, key, &hash(principal, run, &c)?)
    }
    /// Accepted/running/uncertain retained runs may transition. New keys after
    /// terminal completion conflict; exact durable retries remain observations.
    pub fn abort_candidate_run_with_clock(
        &self,
        session: &CoordinatorSession,
        principal: &str,
        run: &str,
        key: &str,
        text: &str,
        mut clock: impl FnMut() -> AbortResult<i64>,
    ) -> AbortResult<CandidateAbortReceipt> {
        let c = command(principal, run, key, text)?;
        let h = hash(principal, run, &c)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        self::session(&tx, session)?;
        if let Some(r) = prior(&tx, principal, run, key, &h)? {
            return Ok(r);
        }
        let snapshot = source(&tx, principal, run)?;
        if snapshot.receipt().revision() != c.expected_revision {
            return Err(LifecycleError::RevisionConflict);
        }
        let now = clock()?;
        if !matches!(
            snapshot.state(),
            CandidateRunState::Accepted | CandidateRunState::Running | CandidateRunState::Uncertain
        ) || snapshot.cleanup_state() != CandidateCleanupState::Retained
            || now < snapshot.receipt().accepted_at_ms()
            || now >= c.deadline_ms
            || c.deadline_ms > snapshot.receipt().deadline_ms()
        {
            return Err(LifecycleError::Conflict);
        }
        let fenced: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM deployments WHERE id=?1 AND revision=?2 AND current_generation=?3 AND desired_state='stopped' AND admission_enabled=0 AND dispatch_enabled=0) AND NOT EXISTS(SELECT 1 FROM qualifications WHERE source_run_id=?4)",params![snapshot.receipt().deployment_id(),c.expected_revision,snapshot.receipt().generation(),run],|r|r.get(0))?;
        if !fenced {
            return Err(LifecycleError::Stale);
        }
        let state: String = tx.query_row(
            "SELECT state FROM qualification_runs WHERE id=?1",
            [run],
            |r| r.get(0),
        )?;
        let r = CandidateAbortReceipt {
            version: 1,
            source: snapshot.receipt().inner.clone(),
            operation_id: ulid::Ulid::new().to_string(),
            session_id: session.id().into(),
            session_epoch: session.epoch(),
            idempotency_key: key.into(),
            request_hash: h,
            source_state: state,
            accepted_at_ms: now,
            deadline_ms: c.deadline_ms,
        };
        tx.execute("INSERT INTO operations(id,deployment_id,kind,state) VALUES(?1,?2,'candidate_abort_v1','succeeded')",params![r.operation_id,r.deployment_id()])?;
        tx.execute(
            "INSERT INTO command_receipts VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                principal,
                scope(run),
                key,
                r.request_hash,
                r.operation_id,
                encode(&r).map_err(error)?
            ],
        )?;
        tx.execute(
            "UPDATE qualification_runs SET state='aborted' WHERE id=?1",
            [run],
        )?;
        append_event(&tx, &event(&r)?).map_err(|e| match e {
            EventWriteError::Sql(e) => LifecycleError::Sql(e),
            _ => LifecycleError::CorruptStoredData,
        })?;
        prior(&tx, principal, run, key, &r.request_hash)?
            .ok_or(LifecycleError::CorruptStoredData)?;
        let committed = clock()?;
        if committed < now || committed >= c.deadline_ms {
            return Err(LifecycleError::Conflict);
        }
        tx.commit()?;
        Ok(r)
    }
}

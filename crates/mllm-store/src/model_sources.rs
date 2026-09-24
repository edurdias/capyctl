//! ADR 0008: the materialization state of each revision's declared remote
//! model source, per host.
//!
//! A revision whose `model.source` is `huggingface` or `http` is accepted with
//! its source `pending` on the host it resolved on. A host asked to
//! materialize it (`MaterializeSource`) reports `downloading` progress, then
//! `verified` or `failed` with a closed reason; the server records each
//! answer here. Until one host holds a verified copy the revision's
//! activation waits (`model_source_pending`), and its checkpoint digest is not
//! measured: there is nothing on disk to measure. A terminal failure (a hash
//! mismatch, a size over the host's limit, a policy refusal) refuses the
//! activation (`model_source_failed`) until a new revision is deployed.
//!
//! Deleting a deployment never deletes a materialized copy (SPEC §6.3). The
//! rows of a deleted deployment stop counting as references, which is what
//! lets `mllm prune sources` reclaim the copy explicitly.

use crate::dispatch::{check_session, CoordinatorSession};
use crate::lifecycle::LifecycleError;
use mllm_config::effective::EffectiveDeployment;
use mllm_config::model_source::reason;
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum ModelSourceError {
    #[error("invalid model source input")]
    Invalid,
    #[error("stale coordinator session")]
    StaleSession,
    #[error("deployment revision not found")]
    NotFound,
    #[error("corrupt stored model source state")]
    CorruptStoredData,
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}
type Result<T> = std::result::Result<T, ModelSourceError>;

/// Where one host's copy of a revision's source stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceState {
    Pending,
    Downloading,
    Verified,
    Failed,
}

impl SourceState {
    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "pending" => Self::Pending,
            "downloading" => Self::Downloading,
            "verified" => Self::Verified,
            "failed" => Self::Failed,
            _ => return Err(ModelSourceError::CorruptStoredData),
        })
    }
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Downloading => "downloading",
            Self::Verified => "verified",
            Self::Failed => "failed",
        }
    }
}

/// One host's source record, as status reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelSourceRecord {
    pub host_id: String,
    /// The directory, relative to the host's model store.
    pub source_key: String,
    pub state: SourceState,
    pub bytes_done: u64,
    pub bytes_total: u64,
    /// `failed` only: a closed category (`hash_mismatch`, `too_large`, ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `failed` only: the same declaration will fail again; no retry.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub terminal: bool,
}

/// A host's answer to one MaterializeSource request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceReport {
    pub state: SourceState,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub reason: Option<String>,
}

/// A current revision whose source a host must still materialize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSource {
    pub deployment_id: String,
    pub revision: i64,
    pub generation: i64,
    pub host_id: String,
    pub source_key: String,
    pub effective: EffectiveDeployment,
}

/// Record a newly accepted revision's remote source as pending on the host
/// it resolved on. A local source has no row.
pub(crate) fn insert_accepted(
    tx: &Transaction<'_>,
    deployment: &str,
    revision: i64,
    effective: &EffectiveDeployment,
    now_ms: i64,
) -> rusqlite::Result<()> {
    let Some(key) = effective.model.source.store_key() else {
        return Ok(());
    };
    tx.execute(
        "INSERT OR IGNORE INTO model_sources(deployment_id,revision,host_id,source_key,state,bytes_done,bytes_total,reason,terminal,updated_at_ms) VALUES(?1,?2,?3,?4,'pending',0,0,NULL,0,?5)",
        params![deployment, revision, effective.host.name, key, now_ms],
    )?;
    Ok(())
}

/// ADR 0008: activation waits until some host holds a verified copy, and is
/// refused once every host's attempt failed terminally.
pub(crate) fn admit_start(
    tx: &Transaction<'_>,
    deployment: &str,
    revision: i64,
) -> std::result::Result<(), LifecycleError> {
    let states: Vec<(String, bool)> = tx
        .prepare("SELECT state,terminal FROM model_sources WHERE deployment_id=?1 AND revision=?2")?
        .query_map(params![deployment, revision], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    if states.is_empty() || states.iter().any(|(state, _)| state == "verified") {
        return Ok(());
    }
    if states.iter().all(|(state, terminal)| state == "failed" && *terminal) {
        return Err(LifecycleError::ModelSourceFailed);
    }
    Err(LifecycleError::ModelSourcePending)
}

/// Whether the digest of this revision may be measured on `host`: a revision
/// with a remote source is measured only where its copy is verified.
pub(crate) const DIGEST_READY_CLAUSE: &str = "NOT EXISTS(SELECT 1 FROM model_sources s WHERE s.deployment_id=c.deployment_id AND s.revision=c.revision AND s.host_id=c.host_id AND s.state<>'verified')";

/// One stored row: host, key, state, done, total, reason, terminal.
type StoredRow = (String, String, String, i64, i64, Option<String>, bool);

fn read_all(tx: &Transaction<'_>, deployment: &str, revision: i64) -> Result<Vec<ModelSourceRecord>> {
    let rows: Vec<StoredRow> = tx
        .prepare("SELECT host_id,source_key,state,bytes_done,bytes_total,reason,terminal FROM model_sources WHERE deployment_id=?1 AND revision=?2 ORDER BY host_id")?
        .query_map(params![deployment, revision], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    rows.into_iter()
        .map(|(host_id, source_key, state, done, total, reason, terminal)| {
            Ok(ModelSourceRecord {
                host_id,
                source_key,
                state: SourceState::parse(&state)?,
                bytes_done: u64::try_from(done).map_err(|_| ModelSourceError::CorruptStoredData)?,
                bytes_total: u64::try_from(total).map_err(|_| ModelSourceError::CorruptStoredData)?,
                reason,
                terminal,
            })
        })
        .collect()
}

impl crate::Store {
    /// Every host's record of one revision's source.
    pub fn model_sources(&self, deployment: &str, revision: i64) -> Result<Vec<ModelSourceRecord>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        read_all(&tx, deployment, revision)
    }

    /// One host's record of one revision's source.
    pub fn model_source(
        &self,
        deployment: &str,
        revision: i64,
        host_id: &str,
    ) -> Result<Option<ModelSourceRecord>> {
        Ok(self
            .model_sources(deployment, revision)?
            .into_iter()
            .find(|record| record.host_id == host_id))
    }

    /// Current revisions whose source is still to be materialized on its host
    /// (pending, downloading, or failed with a retryable reason), oldest first.
    pub fn pending_model_sources(&self) -> Result<Vec<PendingSource>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let rows: Vec<(String, i64, i64, String, String)> = tx
            .prepare("SELECT s.deployment_id,s.revision,COALESCE((SELECT i.generation FROM deployment_instances i WHERE i.deployment_id=s.deployment_id AND i.host_id=s.host_id AND i.generation IS NOT NULL ORDER BY i.generation DESC LIMIT 1),d.current_generation),s.host_id,s.source_key FROM model_sources s JOIN deployments d ON d.id=s.deployment_id AND d.revision=s.revision WHERE s.state<>'verified' AND NOT (s.state='failed' AND s.terminal=1) ORDER BY s.updated_at_ms,s.deployment_id LIMIT 256")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
            .collect::<rusqlite::Result<_>>()?;
        rows.into_iter()
            .map(|(deployment_id, revision, generation, host_id, source_key)| {
                let raw: String = tx
                    .query_row(
                        "SELECT effective_json FROM effective_revisions WHERE deployment_id=?1 AND revision=?2",
                        params![deployment_id, revision],
                        |r| r.get(0),
                    )
                    .optional()?
                    .ok_or(ModelSourceError::NotFound)?;
                let effective = mllm_config::effective::decode_effective_snapshot(&raw)
                    .map_err(|_| ModelSourceError::CorruptStoredData)?;
                Ok(PendingSource {
                    deployment_id,
                    revision,
                    generation,
                    host_id,
                    source_key,
                    effective,
                })
            })
            .collect()
    }

    /// Record a host's answer about one revision's source. A host with no
    /// row yet (a placement on another allowed host) gets one, provided the
    /// revision resolved there. A verified record is never downgraded by a
    /// later answer from the same host.
    #[allow(clippy::too_many_arguments)]
    pub fn record_model_source(
        &self,
        session: &CoordinatorSession,
        deployment: &str,
        revision: i64,
        host_id: &str,
        source_key: &str,
        report: &SourceReport,
        now_ms: i64,
    ) -> Result<()> {
        let valid_reason = match (report.state, report.reason.as_deref()) {
            (SourceState::Failed, Some(reason)) => reason::ALL.contains(&reason),
            (SourceState::Failed, None) => false,
            (_, None) => true,
            (_, Some(_)) => false,
        };
        if host_id.is_empty()
            || !source_key.starts_with("sources/")
            || now_ms < 0
            || !valid_reason
            || report.bytes_done > report.bytes_total && report.bytes_total > 0
        {
            return Err(ModelSourceError::Invalid);
        }
        let done = i64::try_from(report.bytes_done).map_err(|_| ModelSourceError::Invalid)?;
        let total = i64::try_from(report.bytes_total).map_err(|_| ModelSourceError::Invalid)?;
        let terminal = report
            .reason
            .as_deref()
            .is_some_and(reason::terminal);
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session).map_err(|_| ModelSourceError::StaleSession)?;
        let known: Option<(String, String)> = tx
            .query_row(
                "SELECT source_key,state FROM model_sources WHERE deployment_id=?1 AND revision=?2 AND host_id=?3",
                params![deployment, revision, host_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match known {
            Some((key, _)) if key != source_key => return Err(ModelSourceError::Invalid),
            Some((_, state)) if state == "verified" => {
                tx.commit()?;
                return Ok(());
            }
            Some(_) => {}
            None => {
                // ADR 0013 §3: only a host this revision resolved on.
                let (_, effective) = crate::checkpoint_digests::frozen_revision(&tx, deployment, revision)
                    .map_err(|_| ModelSourceError::NotFound)?;
                if effective.model.source.store_key().as_deref() != Some(source_key)
                    || !crate::checkpoint_digests::is_resolved_host(&tx, deployment, revision, &effective, host_id)
                        .map_err(|_| ModelSourceError::CorruptStoredData)?
                {
                    return Err(ModelSourceError::Invalid);
                }
            }
        }
        tx.execute(
            "INSERT INTO model_sources(deployment_id,revision,host_id,source_key,state,bytes_done,bytes_total,reason,terminal,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
             ON CONFLICT(deployment_id,revision,host_id) DO UPDATE SET state=excluded.state,bytes_done=excluded.bytes_done,bytes_total=excluded.bytes_total,reason=excluded.reason,terminal=excluded.terminal,updated_at_ms=excluded.updated_at_ms",
            params![deployment, revision, host_id, source_key, report.state.as_str(), done, total, report.reason, terminal, now_ms],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// SPEC §6.3, ADR 0008: every store key a deployment that still exists
    /// references (any of its revisions), for `mllm prune sources`.
    pub fn referenced_model_sources(&self) -> Result<Vec<String>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let keys = tx
            .prepare("SELECT DISTINCT s.source_key FROM model_sources s JOIN deployments d ON d.id=s.deployment_id ORDER BY s.source_key")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(keys)
    }
}

#[cfg(test)]
mod tests;

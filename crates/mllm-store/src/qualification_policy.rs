use crate::dispatch::{check_session, CoordinatorSession, DispatchError};
use crate::events::{
    append_event, EventMetadata, EventWriteError, HostQualificationPolicyChangeKind,
};
use mllm_config::effective::{HostPolicy, QualificationPolicy};
use rusqlite::{params, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};

const MAX_STORED_POLICY_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualificationPolicyState {
    Unconfigured,
    Configured,
    Removed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QualificationPolicyImport {
    pub state: QualificationPolicyState,
    pub revision: Option<i64>,
    pub changed: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum QualificationImportError {
    #[error("stale coordinator session")]
    StaleSession,
    #[error("qualification policy revision or contents conflict")]
    Conflict,
    #[error("invalid qualification policy input")]
    Invalid,
    #[error("corrupt stored qualification policy")]
    CorruptStoredPolicy,
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPolicy {
    version: u8,
    host_id: String,
    revision: i64,
    state: StoredState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum StoredState {
    Configured {
        hardware_fingerprint: String,
        environment_fingerprint: String,
        policy: StoredPolicyBody,
    },
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPolicyBody {
    allow_qualification_runs: bool,
    allow_experimental_controls: bool,
    allowed_manifest_digests: Vec<String>,
    max_run_duration_ms: i64,
    max_cleanup_duration_ms: i64,
    max_cases: u32,
    max_requests: u32,
    max_request_body_bytes: i64,
    max_input_tokens_per_request: u32,
    max_output_tokens_per_request: u32,
}

impl StoredPolicyBody {
    fn from_effective(policy: &QualificationPolicy) -> Self {
        Self {
            allow_qualification_runs: policy.allow_qualification_runs,
            allow_experimental_controls: policy.allow_experimental_controls,
            allowed_manifest_digests: policy.allowed_manifest_digests.clone(),
            max_run_duration_ms: policy.max_run_duration_ms,
            max_cleanup_duration_ms: policy.max_cleanup_duration_ms,
            max_cases: policy.max_cases,
            max_requests: policy.max_requests,
            max_request_body_bytes: policy.max_request_body_bytes,
            max_input_tokens_per_request: policy.max_input_tokens_per_request,
            max_output_tokens_per_request: policy.max_output_tokens_per_request,
        }
    }

    fn validate(&self, revision: i64) -> Result<(), QualificationImportError> {
        QualificationPolicy {
            revision,
            allow_qualification_runs: self.allow_qualification_runs,
            allow_experimental_controls: self.allow_experimental_controls,
            allowed_manifest_digests: self.allowed_manifest_digests.clone(),
            max_run_duration_ms: self.max_run_duration_ms,
            max_cleanup_duration_ms: self.max_cleanup_duration_ms,
            max_cases: self.max_cases,
            max_requests: self.max_requests,
            max_request_body_bytes: self.max_request_body_bytes,
            max_input_tokens_per_request: self.max_input_tokens_per_request,
            max_output_tokens_per_request: self.max_output_tokens_per_request,
        }
        .validate()
        .map_err(|_| QualificationImportError::CorruptStoredPolicy)
    }
}

fn read_current(
    tx: &Transaction<'_>,
    host_id: &str,
) -> Result<Option<StoredPolicy>, QualificationImportError> {
    let rows: Vec<(String, i64, String)> = tx
        .prepare("SELECT host_id,revision,policy_json FROM host_qualification_policies")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<Result<_, _>>()?;
    if rows.len() > 1 {
        return Err(QualificationImportError::CorruptStoredPolicy);
    }
    let Some((column_host_id, column_revision, json)) = rows.into_iter().next() else {
        return Ok(None);
    };
    if json.len() > MAX_STORED_POLICY_BYTES {
        return Err(QualificationImportError::CorruptStoredPolicy);
    }
    let stored: StoredPolicy =
        serde_json::from_str(&json).map_err(|_| QualificationImportError::CorruptStoredPolicy)?;
    if stored.version != 1
        || stored.host_id != column_host_id
        || stored.revision != column_revision
        || stored.revision <= 0
    {
        return Err(QualificationImportError::CorruptStoredPolicy);
    }
    if column_host_id != host_id {
        return Err(QualificationImportError::Conflict);
    }
    if let StoredState::Configured {
        hardware_fingerprint,
        environment_fingerprint,
        policy,
    } = &stored.state
    {
        if hardware_fingerprint.is_empty() || environment_fingerprint.is_empty() {
            return Err(QualificationImportError::CorruptStoredPolicy);
        }
        policy.validate(stored.revision)?;
    }
    Ok(Some(stored))
}

fn serialize(stored: &StoredPolicy) -> Result<String, QualificationImportError> {
    let json = serde_json::to_string(stored).map_err(|_| QualificationImportError::Invalid)?;
    if json.len() > MAX_STORED_POLICY_BYTES {
        return Err(QualificationImportError::Invalid);
    }
    Ok(json)
}

fn map_session(error: DispatchError) -> QualificationImportError {
    match error {
        DispatchError::StaleSession => QualificationImportError::StaleSession,
        DispatchError::Sql(error) => QualificationImportError::Sql(error),
        _ => QualificationImportError::Invalid,
    }
}

fn map_event(error: EventWriteError) -> QualificationImportError {
    match error {
        EventWriteError::Sql(error) => QualificationImportError::Sql(error),
        _ => QualificationImportError::Invalid,
    }
}

impl crate::Store {
    pub fn import_qualification_policy(
        &self,
        session: &CoordinatorSession,
        host: &HostPolicy,
    ) -> Result<QualificationPolicyImport, QualificationImportError> {
        if host.name.is_empty()
            || host.hardware_fingerprint.is_empty()
            || host.environment_fingerprint.is_empty()
        {
            return Err(QualificationImportError::Invalid);
        }
        if let Some(policy) = &host.qualification_policy {
            policy
                .validate()
                .map_err(|_| QualificationImportError::Invalid)?;
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session).map_err(map_session)?;
        let current = read_current(&tx, &host.name)?;
        let proposed = host.qualification_policy.as_ref();

        let (next, change_kind, result) = match (&current, proposed) {
            (None, None) => {
                tx.commit()?;
                return Ok(QualificationPolicyImport {
                    state: QualificationPolicyState::Unconfigured,
                    revision: None,
                    changed: false,
                });
            }
            (None, Some(policy)) => {
                if policy.revision != 1 {
                    return Err(QualificationImportError::Conflict);
                }
                let next = configured(host, policy);
                (
                    next,
                    HostQualificationPolicyChangeKind::Imported,
                    configured_result(policy.revision, true),
                )
            }
            (Some(stored), None) if matches!(stored.state, StoredState::Removed) => {
                tx.commit()?;
                return Ok(QualificationPolicyImport {
                    state: QualificationPolicyState::Removed,
                    revision: Some(stored.revision),
                    changed: false,
                });
            }
            (Some(stored), None) => (
                StoredPolicy {
                    version: 1,
                    host_id: host.name.clone(),
                    revision: stored.revision,
                    state: StoredState::Removed,
                },
                HostQualificationPolicyChangeKind::Removed,
                QualificationPolicyImport {
                    state: QualificationPolicyState::Removed,
                    revision: Some(stored.revision),
                    changed: true,
                },
            ),
            (Some(stored), Some(policy)) => {
                if policy.revision == stored.revision {
                    let identical = matches!(stored.state, StoredState::Configured { .. })
                        && *stored == configured(host, policy);
                    if identical {
                        tx.commit()?;
                        return Ok(configured_result(policy.revision, false));
                    }
                    return Err(QualificationImportError::Conflict);
                }
                if stored.revision.checked_add(1) != Some(policy.revision) {
                    return Err(QualificationImportError::Conflict);
                }
                let kind = if matches!(stored.state, StoredState::Removed) {
                    HostQualificationPolicyChangeKind::Readded
                } else {
                    HostQualificationPolicyChangeKind::Updated
                };
                (
                    configured(host, policy),
                    kind,
                    configured_result(policy.revision, true),
                )
            }
        };

        let json = serialize(&next)?;
        tx.execute(
            "INSERT INTO host_qualification_policies(host_id,revision,policy_json) VALUES(?1,?2,?3)
             ON CONFLICT(host_id) DO UPDATE SET revision=excluded.revision,policy_json=excluded.policy_json",
            params![next.host_id, next.revision, json],
        )?;
        append_event(
            &tx,
            &EventMetadata::HostQualificationPolicyChanged {
                change_kind,
                previous_revision: current.as_ref().map(|policy| policy.revision),
                current_revision: next.revision,
                session_epoch: session.epoch(),
            },
        )
        .map_err(map_event)?;
        tx.commit()?;
        Ok(result)
    }
}

fn configured(host: &HostPolicy, policy: &QualificationPolicy) -> StoredPolicy {
    StoredPolicy {
        version: 1,
        host_id: host.name.clone(),
        revision: policy.revision,
        state: StoredState::Configured {
            hardware_fingerprint: host.hardware_fingerprint.clone(),
            environment_fingerprint: host.environment_fingerprint.clone(),
            policy: StoredPolicyBody::from_effective(policy),
        },
    }
}

fn configured_result(revision: i64, changed: bool) -> QualificationPolicyImport {
    QualificationPolicyImport {
        state: QualificationPolicyState::Configured,
        revision: Some(revision),
        changed,
    }
}

#[cfg(test)]
mod tests;

//! ADR 0014 §7 (WE3): the recorded checkpoint digest of each deployment
//! revision.
//!
//! A deployment is accepted with its digest `pending` (R12
//! `checkpoint_digest_pending`); a host holding the checkpoint measures it and
//! the server records the digest here, with the weights bytes the manifest
//! supplies (ADR 0014 §5). A declared `model.content_fingerprint` in digest form
//! is the expectation the measurement must meet. Every later launch and wake
//! carries the recorded digest to its host, which refuses a checkpoint that
//! does not measure to it.
//!
//! A revision whose memory request is derived from the weights cannot be
//! resolved before the digest exists. It is frozen `provisional` (resolved with
//! zero weights, a bound nothing may reserve) and its activation waits; the
//! recorded weights then re-resolve it exactly and replace the frozen revision
//! before anything references it. Any other revision may activate while its
//! digest is pending: its first placement measures and records the digest
//! before the launch is sent.
//!
//! Owner decision 2026-09-22: state from before WE3 keeps working. A revision
//! accepted before v20 has no row; running launches of it are adopted
//! unchanged, and its next launch records the digest on first placement.

use crate::dispatch::{check_session, CoordinatorSession};
use crate::lifecycle::LifecycleError;
use mllm_config::effective::{
    declared_checkpoint_digest, is_checkpoint_digest, resolve_snapshot_with_checkpoint,
    CheckpointFacts, EffectiveDeployment,
};
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use serde::Serialize;

/// The closed diagnostic categories a pending digest may carry.
const REASONS: &[&str] = &[
    "invalid_root",
    "unsafe_file",
    "too_large",
    "changed",
    "io_error",
    "unauthorized",
    "not_materializable",
    "host_unavailable",
];

#[derive(Debug, thiserror::Error)]
pub enum CheckpointDigestError {
    #[error("invalid checkpoint digest input")]
    Invalid,
    #[error("stale coordinator session")]
    StaleSession,
    #[error("deployment revision not found")]
    NotFound,
    #[error("corrupt stored checkpoint digest")]
    CorruptStoredData,
    /// ADR 0014 §7: the reporting host is not one the revision resolved on.
    #[error("the reporting host did not resolve this revision")]
    UnresolvedHost,
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}
type Result<T> = std::result::Result<T, CheckpointDigestError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DigestState {
    Pending,
    Recorded,
    Mismatch,
    Unusable,
}

impl DigestState {
    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "pending" => Self::Pending,
            "recorded" => Self::Recorded,
            "mismatch" => Self::Mismatch,
            "unusable" => Self::Unusable,
            _ => return Err(CheckpointDigestError::CorruptStoredData),
        })
    }
}

/// One revision's checkpoint digest record, as status reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckpointDigest {
    pub state: DigestState,
    pub host_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weights_bytes: Option<i64>,
    /// The frozen revision waits for this digest before it may activate.
    pub provisional: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

/// A current revision whose digest a host must still measure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDigest {
    pub deployment_id: String,
    pub revision: i64,
    pub generation: i64,
    pub host_id: String,
    pub expected: Option<String>,
    pub effective: EffectiveDeployment,
    /// Owner decision 2026-09-23 (solo first start): the weights a sizing
    /// already recorded while the digest is pending, if any.
    pub weights_bytes: Option<i64>,
}

/// What recording a measured digest did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordOutcome {
    /// The digest is (now, or already) the revision's recorded one.
    Recorded { digest: String, weights_bytes: i64 },
    /// The measured digest is not the declared expectation, or not the one
    /// already recorded; nothing on this checkpoint may launch.
    Mismatch,
    /// The measured weights do not resolve the revision's derived memory.
    Unusable,
}

/// Record a newly accepted revision's digest as pending. `provisional` marks a
/// revision frozen before the weights it derives its memory from were known.
pub(crate) fn insert_accepted(
    tx: &Transaction<'_>,
    deployment: &str,
    revision: i64,
    effective: &EffectiveDeployment,
    provisional: bool,
    now_ms: i64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO checkpoint_digests(deployment_id,revision,state,host_id,expected,digest,weights_bytes,provisional,diagnostic,updated_at_ms) VALUES(?1,?2,'pending',?3,?4,NULL,NULL,?5,NULL,?6)",
        params![
            deployment,
            revision,
            effective.host.name,
            declared_checkpoint_digest(&effective.model.content_fingerprint),
            provisional,
            now_ms
        ],
    )?;
    Ok(())
}

/// SPEC §6, ADR 0014 §7: activation of a revision waits while its frozen
/// resources depend on a digest still pending, and is refused while its
/// checkpoint is known not to be the declared or recorded one.
pub(crate) fn admit_start(
    tx: &Transaction<'_>,
    deployment: &str,
    revision: i64,
) -> std::result::Result<(), LifecycleError> {
    let row: Option<(String, bool, Option<String>)> = tx
        .query_row(
            "SELECT state,provisional,diagnostic FROM checkpoint_digests WHERE deployment_id=?1 AND revision=?2",
            params![deployment, revision],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    match row {
        None => Ok(()),
        Some((state, provisional, diagnostic)) => match state.as_str() {
            "recorded" => Ok(()),
            "pending" if !provisional => Ok(()),
            "pending" => Err(LifecycleError::CheckpointDigestPending),
            // Discrete GPU design §11: a closed refusal the measured weights
            // met is the start's refusal (exit 4 for insufficient_device_memory).
            "unusable" => match diagnostic.filter(|d| closed_refusal(d)) {
                Some(reason) => Err(LifecycleError::CheckpointUnusable(reason)),
                None => Err(LifecycleError::CheckpointMismatch),
            },
            "mismatch" => Err(LifecycleError::CheckpointMismatch),
            _ => Err(LifecycleError::CorruptStoredData),
        },
    }
}

/// Discrete GPU design §11: the resolution refusals a measured checkpoint can
/// meet that name their own closed code as a `<code>: ...` prefix.
const CLOSED_REFUSALS: &[&str] = &["insufficient_device_memory:", "host_backed_unavailable:"];

fn closed_refusal(text: &str) -> bool {
    CLOSED_REFUSALS.iter().any(|code| text.starts_with(code))
}

/// One stored row: state, host, expected, digest, weights, provisional, diagnostic.
type StoredRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<i64>,
    bool,
    Option<String>,
);

fn read(tx: &Transaction<'_>, deployment: &str, revision: i64) -> Result<Option<CheckpointDigest>> {
    let row: Option<StoredRow> = tx
        .query_row(
            "SELECT state,host_id,expected,digest,weights_bytes,provisional,diagnostic FROM checkpoint_digests WHERE deployment_id=?1 AND revision=?2",
            params![deployment, revision],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
        )
        .optional()?;
    row.map(
        |(state, host_id, expected, digest, weights_bytes, provisional, diagnostic)| {
            Ok(CheckpointDigest {
                state: DigestState::parse(&state)?,
                host_id,
                expected,
                digest,
                weights_bytes,
                provisional,
                diagnostic,
            })
        },
    )
    .transpose()
}

fn frozen(
    tx: &Transaction<'_>,
    deployment: &str,
    revision: i64,
) -> Result<(String, EffectiveDeployment)> {
    let raw: String = tx
        .query_row(
            "SELECT effective_json FROM effective_revisions WHERE deployment_id=?1 AND revision=?2",
            params![deployment, revision],
            |r| r.get(0),
        )
        .optional()?
        .ok_or(CheckpointDigestError::NotFound)?;
    let effective = mllm_config::effective::decode_effective_snapshot(&raw)
        .map_err(|_| CheckpointDigestError::CorruptStoredData)?;
    Ok((raw, effective))
}

/// Whether `host_id` is a host `revision` resolved on: the host its frozen
/// revision names, the host its digest was requested from, or an allowed host
/// the revision resolved on (ADR 0013 §3), by host id or enrolled host name.
fn resolved_host(
    tx: &Transaction<'_>,
    deployment: &str,
    revision: i64,
    effective: &EffectiveDeployment,
    host_id: &str,
) -> Result<bool> {
    if effective.host.name == host_id {
        return Ok(true);
    }
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM checkpoint_digests WHERE deployment_id=?1 AND revision=?2 AND host_id=?3)
             OR EXISTS(SELECT 1 FROM host_effective_revisions h WHERE h.deployment_id=?1 AND h.revision=?2
                          AND h.outcome='resolved'
                          AND (h.host_id=?3 OR EXISTS(SELECT 1 FROM enrolled_hosts e WHERE e.host_id=h.host_id AND e.host_name=?3)))",
        params![deployment, revision, host_id],
        |r| r.get(0),
    )?)
}

/// ADR 0008: the frozen revision, for the model-source records.
pub(crate) fn frozen_revision(
    tx: &Transaction<'_>,
    deployment: &str,
    revision: i64,
) -> Result<(String, EffectiveDeployment)> {
    frozen(tx, deployment, revision)
}

/// ADR 0008: whether `host_id` is a host `revision` resolved on.
pub(crate) fn is_resolved_host(
    tx: &Transaction<'_>,
    deployment: &str,
    revision: i64,
    effective: &EffectiveDeployment,
    host_id: &str,
) -> Result<bool> {
    resolved_host(tx, deployment, revision, effective, host_id)
}

impl crate::Store {
    /// The digest record of one revision, if it has one.
    pub fn checkpoint_digest(
        &self,
        deployment: &str,
        revision: i64,
    ) -> Result<Option<CheckpointDigest>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        read(&tx, deployment, revision)
    }

    /// Current revisions whose digest is still pending, oldest first.
    pub fn pending_checkpoint_digests(&self) -> Result<Vec<PendingDigest>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let rows = tx
            // ADR 0013 §5: a host agent fences each deployment's commands by the
            // generation it last saw. A measurement carries the generation of
            // the instance placed on the measuring host, so it never moves that
            // host past the launch it runs; with none placed there, the
            // deployment's counter.
            // ADR 0008: a remote source is measured only once its copy on the
            // measuring host is verified; before that there is nothing to hash.
            .prepare(&format!("SELECT c.deployment_id,c.revision,COALESCE((SELECT i.generation FROM deployment_instances i WHERE i.deployment_id=c.deployment_id AND i.host_id=c.host_id AND i.generation IS NOT NULL ORDER BY i.generation DESC LIMIT 1),d.current_generation),c.host_id,c.expected,COALESCE(c.weights_bytes,(SELECT z.weights_bytes FROM checkpoint_sizes z WHERE z.deployment_id=c.deployment_id AND z.revision=c.revision)) FROM checkpoint_digests c JOIN deployments d ON d.id=c.deployment_id AND d.revision=c.revision WHERE c.state='pending' AND {} ORDER BY c.updated_at_ms,c.deployment_id LIMIT 256", crate::model_sources::DIGEST_READY_CLAUSE))?
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?, r.get::<_, Option<String>>(4)?, r.get::<_, Option<i64>>(5)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(
                |(deployment_id, revision, generation, host_id, expected, weights_bytes)| {
                    let (_, effective) = frozen(&tx, &deployment_id, revision)?;
                    Ok(PendingDigest {
                        deployment_id,
                        revision,
                        generation,
                        host_id,
                        expected,
                        effective,
                        weights_bytes,
                    })
                },
            )
            .collect()
    }

    /// The digest a launch of this revision must carry: the recorded digest
    /// and the weights bytes the frozen revision was resolved with (ADR 0014
    /// §5: both sides resolve with the same facts). `None` while not recorded.
    pub fn recorded_checkpoint(&self, deployment: &str, revision: i64) -> Result<Option<String>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        Ok(read(&tx, deployment, revision)?
            .filter(|record| record.state == DigestState::Recorded)
            .and_then(|record| record.digest))
    }

    /// Note why a pending digest could not be measured yet. Only a closed
    /// category is kept; the digest stays pending.
    pub fn note_checkpoint_digest_refusal(
        &self,
        session: &CoordinatorSession,
        deployment: &str,
        revision: i64,
        reason: &str,
        now_ms: i64,
    ) -> Result<()> {
        if !REASONS.contains(&reason) || now_ms < 0 {
            return Err(CheckpointDigestError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session).map_err(|_| CheckpointDigestError::StaleSession)?;
        tx.execute(
            "UPDATE checkpoint_digests SET diagnostic=?3,updated_at_ms=?4 WHERE deployment_id=?1 AND revision=?2 AND state='pending'",
            params![deployment, revision, reason, now_ms],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Owner decision 2026-09-23 (solo first start): record the weights a host
    /// sized (a stat walk, no hash) for a revision whose digest is pending, so
    /// a first start's startup estimate uses them before the digest exists.
    /// The digest stays pending; its own measurement later records the
    /// weights it hashed. Returns whether anything was recorded (only a
    /// pending revision with no weights yet takes them).
    pub fn record_checkpoint_weights(
        &self,
        session: &CoordinatorSession,
        deployment: &str,
        revision: i64,
        host_id: &str,
        weights_bytes: i64,
        now_ms: i64,
    ) -> Result<bool> {
        if weights_bytes < 0 || now_ms < 0 || host_id.is_empty() {
            return Err(CheckpointDigestError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session).map_err(|_| CheckpointDigestError::StaleSession)?;
        let changed = tx.execute(
            "INSERT OR IGNORE INTO checkpoint_sizes(deployment_id,revision,host_id,weights_bytes,sized_at_ms)
             SELECT ?1,?2,?4,?3,?5 WHERE EXISTS(SELECT 1 FROM checkpoint_digests
               WHERE deployment_id=?1 AND revision=?2 AND host_id=?4 AND state='pending')",
            params![deployment, revision, weights_bytes, host_id, now_ms],
        )?;
        tx.commit()?;
        Ok(changed == 1)
    }

    /// ADR 0014 §7: record the digest a host measured for one revision.
    ///
    /// The first measurement becomes the recorded digest unless it differs
    /// from a declared expectation. Once recorded, a different measurement is
    /// a mismatch for the host that made it and changes nothing recorded. A
    /// provisional revision is re-resolved with the measured weights and its
    /// frozen revision (and acceptance receipt) replaced; nothing can reference
    /// it yet because its activation waited for this.
    #[allow(clippy::too_many_arguments)]
    pub fn record_checkpoint_digest(
        &self,
        session: &CoordinatorSession,
        deployment: &str,
        revision: i64,
        host_id: &str,
        digest: &str,
        weights_bytes: i64,
        now_ms: i64,
    ) -> Result<RecordOutcome> {
        if !is_checkpoint_digest(digest) || weights_bytes < 0 || now_ms < 0 || host_id.is_empty() {
            return Err(CheckpointDigestError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session).map_err(|_| CheckpointDigestError::StaleSession)?;
        let (raw, effective) = frozen(&tx, deployment, revision)?;
        // ADR 0014 §7, SPEC §8: a digest is a measurement by a host this
        // revision resolved on; a report from any other host records nothing.
        if !resolved_host(&tx, deployment, revision, &effective, host_id)? {
            return Err(CheckpointDigestError::UnresolvedHost);
        }
        let existing = match read(&tx, deployment, revision)? {
            Some(record) => record,
            None => {
                // Accepted before v20: record on first placement.
                insert_accepted(&tx, deployment, revision, &effective, false, now_ms)?;
                read(&tx, deployment, revision)?.ok_or(CheckpointDigestError::CorruptStoredData)?
            }
        };
        match existing.state {
            DigestState::Recorded => {
                let same = existing.digest.as_deref() == Some(digest)
                    && existing.weights_bytes == Some(weights_bytes);
                tx.commit()?;
                return Ok(if same {
                    RecordOutcome::Recorded {
                        digest: digest.into(),
                        weights_bytes,
                    }
                } else {
                    RecordOutcome::Mismatch
                });
            }
            DigestState::Mismatch => {
                tx.commit()?;
                return Ok(RecordOutcome::Mismatch);
            }
            DigestState::Unusable => {
                tx.commit()?;
                return Ok(RecordOutcome::Unusable);
            }
            DigestState::Pending => {}
        }
        if existing
            .expected
            .as_deref()
            .is_some_and(|expected| expected != digest)
        {
            tx.execute(
                "UPDATE checkpoint_digests SET state='mismatch',digest=?3,weights_bytes=?4,host_id=?5,diagnostic=NULL,updated_at_ms=?6 WHERE deployment_id=?1 AND revision=?2",
                params![deployment, revision, digest, weights_bytes, host_id, now_ms],
            )?;
            tx.commit()?;
            return Ok(RecordOutcome::Mismatch);
        }
        if existing.provisional {
            let referenced: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_bindings WHERE deployment_id=?1 AND revision=?2) OR EXISTS(SELECT 1 FROM lifecycle_runs WHERE deployment_id=?1 AND revision=?2)",
                params![deployment, revision],
                |r| r.get(0),
            )?;
            if referenced {
                return Err(CheckpointDigestError::CorruptStoredData);
            }
            let facts = CheckpointFacts {
                weights_bytes: Some(weights_bytes),
                ..Default::default()
            };
            // Final review I7 (design §7): each GPU of a multi-GPU host is
            // re-resolved on its own, and one the measured weights no longer
            // fit is dropped as a placement option for this revision, never
            // the whole record. A host whose GPUs all drop, or whose own row
            // no longer resolves, is refused for the revision with a
            // diagnostic; the revision is unusable only when no host resolves.
            let devices: Vec<(String, String, String, String)> = tx
                .prepare("SELECT host_id,device,effective_json,source_json FROM host_device_effective_revisions WHERE deployment_id=?1 AND revision=?2 ORDER BY host_id,device")?
                .query_map(params![deployment, revision], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })?
                .collect::<rusqlite::Result<_>>()?;
            let mut on_devices: std::collections::BTreeMap<
                String,
                Vec<(EffectiveDeployment, String)>,
            > = Default::default();
            let mut multi_gpu = std::collections::BTreeSet::new();
            // Written only once the revision is known to resolve somewhere.
            type DeviceWrite = (String, String, Option<(String, String)>);
            let mut device_writes: Vec<DeviceWrite> = Vec::new();
            type HostWrite = (String, Option<(String, String, Option<String>)>);
            let mut host_writes: Vec<HostWrite> = Vec::new();
            for (host, device, frozen_json, source_json) in devices {
                multi_gpu.insert(host.clone());
                match resolve_snapshot_with_checkpoint(&frozen_json, facts) {
                    Ok(mut on_device) => {
                        on_device.routes.sort();
                        let device_json = serde_json::to_string(&on_device)
                            .map_err(|_| CheckpointDigestError::CorruptStoredData)?;
                        device_writes.push((
                            host.clone(),
                            device,
                            Some((device_json, on_device.recipe_fingerprint.clone())),
                        ));
                        on_devices
                            .entry(host)
                            .or_default()
                            .push((on_device, source_json));
                    }
                    Err(_) => device_writes.push((host, device, None)),
                }
            }
            // ADR 0013 §3: every host the revision resolved on was frozen with
            // the same placeholder; each is re-resolved with the same
            // measured facts. A multi-GPU host's own row is its first GPU
            // that still resolves.
            let hosts: Vec<(String, String)> = tx
                .prepare("SELECT host_id,effective_json FROM host_effective_revisions WHERE deployment_id=?1 AND revision=?2 AND outcome='resolved' ORDER BY host_id")?
                .query_map(params![deployment, revision], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let any_hosts = !hosts.is_empty();
            let mut canonical: Option<EffectiveDeployment> = None;
            for (host, frozen_json) in hosts {
                let resolved = if multi_gpu.contains(&host) {
                    on_devices
                        .get(&host)
                        .and_then(|resolved| resolved.first())
                        .map(|(on_device, source)| (on_device.clone(), Some(source.clone())))
                } else {
                    resolve_snapshot_with_checkpoint(&frozen_json, facts)
                        .ok()
                        .map(|mut on_host| {
                            on_host.routes.sort();
                            (on_host, None)
                        })
                };
                match resolved {
                    Some((on_host, source)) => {
                        let host_json = serde_json::to_string(&on_host)
                            .map_err(|_| CheckpointDigestError::CorruptStoredData)?;
                        host_writes.push((
                            host,
                            Some((host_json, on_host.recipe_fingerprint.clone(), source)),
                        ));
                        canonical.get_or_insert(on_host);
                    }
                    None => host_writes.push((host, None)),
                }
            }
            let canonical = match canonical {
                Some(resolved) => Ok(resolved),
                // A revision frozen before per-host rows existed.
                None if !any_hosts => {
                    resolve_snapshot_with_checkpoint(&raw, facts).map(|mut resolved| {
                        resolved.routes.sort();
                        resolved
                    })
                }
                None => resolve_snapshot_with_checkpoint(&raw, facts).and_then(|_| {
                    Err(mllm_config::ConfigError::new(
                        mllm_config::ConfigErrorCode::UnsupportedCombination,
                        "engine_config.memory.request",
                        "insufficient_device_memory: the measured weights fit none of the \
                         allowed hosts' GPUs",
                    ))
                }),
            };
            match canonical {
                Ok(resolved) => {
                    for (host, device, write) in &device_writes {
                        match write {
                            Some((json, fingerprint)) => tx.execute(
                                "UPDATE host_device_effective_revisions SET effective_json=?5,fingerprint=?6 WHERE deployment_id=?1 AND revision=?2 AND host_id=?3 AND device=?4",
                                params![deployment, revision, host, device, json, fingerprint],
                            )?,
                            None => tx.execute(
                                "DELETE FROM host_device_effective_revisions WHERE deployment_id=?1 AND revision=?2 AND host_id=?3 AND device=?4",
                                params![deployment, revision, host, device],
                            )?,
                        };
                    }
                    for (host, write) in &host_writes {
                        match write {
                            Some((json, fingerprint, source)) => tx.execute(
                                "UPDATE host_effective_revisions SET effective_json=?4,fingerprint=?5,source_json=COALESCE(?6,source_json) WHERE deployment_id=?1 AND revision=?2 AND host_id=?3",
                                params![deployment, revision, host, json, fingerprint, source],
                            )?,
                            None => tx.execute(
                                "DELETE FROM host_device_effective_revisions WHERE deployment_id=?1 AND revision=?2 AND host_id=?3",
                                params![deployment, revision, host],
                            )? + tx.execute(
                                "UPDATE host_effective_revisions SET outcome='refused',effective_json=NULL,fingerprint=NULL,diagnostic='does_not_resolve' WHERE deployment_id=?1 AND revision=?2 AND host_id=?3",
                                params![deployment, revision, host],
                            )?,
                        };
                    }
                    let json = serde_json::to_string(&resolved)
                        .map_err(|_| CheckpointDigestError::CorruptStoredData)?;
                    tx.execute(
                        "UPDATE effective_revisions SET effective_json=?3,fingerprint=?4 WHERE deployment_id=?1 AND revision=?2",
                        params![deployment, revision, json, resolved.recipe_fingerprint],
                    )?;
                    crate::managed_configuration::reseal_migrated_receipt(
                        &tx, deployment, revision, &json, None, None,
                    )
                    .map_err(|_| CheckpointDigestError::CorruptStoredData)?;
                }
                Err(error) => {
                    // Discrete GPU design §11: a closed refusal keeps its code
                    // and numbers (bounded) so a start can be refused with it.
                    let diagnostic = Some(error.detail.chars().take(512).collect::<String>())
                        .filter(|detail| closed_refusal(detail))
                        .unwrap_or_else(|| {
                            "the derived memory request does not resolve with the measured \
                             weights; replace the configuration"
                                .to_owned()
                        });
                    tx.execute(
                        "UPDATE checkpoint_digests SET state='unusable',digest=?3,host_id=?4,diagnostic=?6,updated_at_ms=?5 WHERE deployment_id=?1 AND revision=?2",
                        params![deployment, revision, digest, host_id, now_ms, diagnostic],
                    )?;
                    tx.commit()?;
                    return Ok(RecordOutcome::Unusable);
                }
            }
        }
        tx.execute(
            "UPDATE checkpoint_digests SET state='recorded',digest=?3,weights_bytes=?4,host_id=?5,provisional=0,diagnostic=NULL,updated_at_ms=?6 WHERE deployment_id=?1 AND revision=?2",
            params![deployment, revision, digest, weights_bytes, host_id, now_ms],
        )?;
        tx.commit()?;
        Ok(RecordOutcome::Recorded {
            digest: digest.into(),
            weights_bytes,
        })
    }
}

#[cfg(test)]
mod tests;

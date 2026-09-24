use rusqlite::{params, Transaction, TransactionBehavior};
use serde::Serialize;

const MAX_EVENTS: i64 = 100_000;
const MAX_BYTES: i64 = 64 * 1024 * 1024;
const MAX_AGE_MS: i64 = 7 * 24 * 60 * 60 * 1000;
const MAX_PAYLOAD: usize = 16 * 1024;
pub const MAX_REPLAY_PAGE_SIZE: usize = 1_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventCursor {
    pub incarnation: String,
    pub sequence: i64,
}
impl std::fmt::Display for EventCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.incarnation, self.sequence)
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagementEvent {
    pub cursor: EventCursor,
    pub recorded_at_ms: i64,
    pub kind: String,
    pub deployment_id: Option<String>,
    pub operation_id: Option<String>,
    pub payload_json: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventPage {
    pub events: Vec<ManagementEvent>,
    pub high_water: EventCursor,
}

#[derive(Debug, thiserror::Error)]
pub enum EventReadError {
    #[error("malformed event cursor")]
    MalformedCursor,
    #[error("event cursor belongs to another database incarnation")]
    WrongIncarnation,
    #[error("event cursor is ahead of the database")]
    FutureCursor,
    #[error("event cursor has expired")]
    ExpiredCursor,
    #[error("invalid event page limit")]
    InvalidLimit,
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}
#[derive(Debug, thiserror::Error)]
pub(crate) enum EventWriteError {
    #[error("invalid event transition metadata")]
    InvalidMetadata,
    #[error("event payload exceeds 16 KiB")]
    PayloadTooLarge,
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Serialization(#[from] serde_json::Error),
}
#[derive(Serialize)]
#[serde(tag = "version")]
pub(crate) enum EventMetadata {
    #[serde(rename = "1")]
    UnarmedStopRecorded {
        transition: UnarmedStopTransition,
        operation_id: EventOperationId,
        deployment_id: EventOperationId,
        step_id: EventOperationId,
        session_epoch: i64,
        committed_epoch: Option<u64>,
    },
    #[serde(rename = "1")]
    OrdinaryCleanupRecorded {
        transition: OrdinaryCleanupTransition,
        operation_id: EventOperationId,
        deployment_id: EventOperationId,
        step_id: EventOperationId,
        session_epoch: i64,
        committed_epoch: Option<u64>,
    },
    #[serde(rename = "1")]
    LifecycleRecorded {
        transition: LifecycleTransition,
        operation_id: EventOperationId,
        deployment_id: EventOperationId,
        step_id: EventOperationId,
        session_epoch: i64,
        committed_epoch: Option<u64>,
    },
    #[serde(rename = "1")]
    ManagedConfigurationAccepted {
        operation_id: EventOperationId,
        deployment_id: EventOperationId,
        revision: i64,
        generation: i64,
        session_epoch: i64,
    },
    /// SPEC §6.3 (W6): the deployment's routes and instances were removed
    /// after verified cleanup; its row remains as a tombstone.
    #[serde(rename = "1")]
    DeploymentDeleted {
        operation_id: EventOperationId,
        deployment_id: EventOperationId,
        revision: i64,
        session_epoch: i64,
    },
    /// SPEC §§6.1, 6.3, 9.1 (W5): one park or restore of an instance's
    /// retained launch, per persisted transition.
    #[serde(rename = "1")]
    ResidencyRecorded {
        transition: ResidencyTransition,
        operation_id: EventOperationId,
        deployment_id: EventOperationId,
        step_id: EventOperationId,
        session_epoch: i64,
        committed_epoch: Option<u64>,
    },
    #[serde(rename = "1")]
    CoordinatorSessionStarted { session_epoch: i64 },
    #[serde(rename = "1")]
    HostResourcePolicyBootstrapped {
        revision: i64,
        ledger_epoch: u64,
        session_epoch: i64,
    },
    #[serde(rename = "1")]
    HostResourcePolicyUpdated {
        operation_id: EventOperationId,
        previous_revision: i64,
        current_revision: i64,
        ledger_epoch: u64,
        session_epoch: i64,
    },
    /// ADR 0008 (owner decision 2026-09-23): a host reported that a launch
    /// measured one of its engine installations to a different fingerprint
    /// than the one registered at agent start. Evidence only; whether the
    /// launch was refused is the host policy's (`installation_drift`).
    #[serde(rename = "1")]
    InstallationDriftFlagged {
        host_id: String,
        installation: String,
        registered_digest: String,
        observed_digest: String,
    },
    /// SPEC §§4.1, 13.3: an administrator revoked the host's identity. Its
    /// control session closes and new work stops; nothing it owns is released
    /// by this record.
    #[serde(rename = "1")]
    HostRevoked { host_id: String, host_name: String },
    /// SPEC §10, ADR 0013 §8 (W10): one transition of a request-driven switch.
    /// Victims are `deployment/instance`; each victim's park or stop is its
    /// own operation with its own events.
    #[serde(rename = "1")]
    SwitchRecorded {
        phase: SwitchPhase,
        switch_id: String,
        target_deployment: String,
        host: Option<String>,
        victims: Vec<String>,
        detail: String,
    },
}

/// SPEC §10 (W10): what a request-driven switch recorded.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SwitchPhase {
    /// A plan was chosen: the host and the victims to release there.
    Planned,
    /// The fairness window ended and the victims' admission closed.
    AdmissionClosed,
    /// Every victim drained and its park or stop completed on evidence.
    Released,
    /// The waiting deployment's instance is READY and dispatch is open.
    Completed,
    /// The switch failed; victims whose release was not accepted serve again.
    Failed,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LifecycleTransition {
    Accepted,
    Armed,
    OwnedLaunchAssociated,
    Ready,
    Uncertain,
    ExpiredUnarmed,
    /// Spec §6: a launch that failed after arm, released against gone evidence.
    LaunchFailed,
}
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OrdinaryCleanupTransition {
    #[serde(rename = "cleanup_accepted")]
    Accepted,
    #[serde(rename = "cleanup_armed")]
    Armed,
    #[serde(rename = "cleanup_completed")]
    Completed,
    /// Owner decision 2026-09-22: a drain Stop closed at its deadline before it
    /// was ever armed, so no effect was sent.
    #[serde(rename = "cleanup_expired_unarmed")]
    ExpiredUnarmed,
}
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UnarmedStopTransition {
    #[serde(rename = "unarmed_stop_accepted")]
    Accepted,
    #[serde(rename = "unarmed_stop_completed")]
    Completed,
}
/// SPEC §§6.1, 9.1 (W5): what a residency step recorded.
#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResidencyTransition {
    ParkAccepted,
    ParkArmed,
    Parked,
    /// Refused before any effect; the launch stays Ready.
    ParkRefused,
    /// The effect's outcome is unknown; accounting is retained.
    ParkUncertain,
    RestoreAccepted,
    RestoreArmed,
    Restored,
    /// Refused before any effect; the launch stays parked.
    RestoreRefused,
    RestoreUncertain,
    /// Closed without any effect (deadline, or superseded by a stop).
    Cancelled,
}
#[derive(Clone)]
pub(crate) struct EventOperationId(String);
impl EventOperationId {
    pub(crate) fn generated(value: ulid::Ulid) -> Self {
        Self(value.to_string())
    }
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}
impl Serialize for EventOperationId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}
impl EventMetadata {
    fn kind(&self) -> &'static str {
        match self {
            Self::UnarmedStopRecorded { transition, .. } => match transition {
                UnarmedStopTransition::Accepted => "ordinary_unarmed_stop_accepted",
                UnarmedStopTransition::Completed => "ordinary_unarmed_stop_completed",
            },
            Self::OrdinaryCleanupRecorded { transition, .. } => match transition {
                OrdinaryCleanupTransition::Accepted => "ordinary_cleanup_accepted",
                OrdinaryCleanupTransition::Armed => "ordinary_cleanup_armed",
                OrdinaryCleanupTransition::Completed => "ordinary_cleanup_completed",
                OrdinaryCleanupTransition::ExpiredUnarmed => "ordinary_cleanup_expired_unarmed",
            },
            Self::LifecycleRecorded { transition, .. } => match transition {
                LifecycleTransition::Accepted => "initialize_accepted",
                LifecycleTransition::Armed => "initialize_armed",
                LifecycleTransition::OwnedLaunchAssociated => "owned_launch_associated",
                LifecycleTransition::Ready => "ready_committed",
                LifecycleTransition::Uncertain => "initialize_uncertain",
                LifecycleTransition::ExpiredUnarmed => "initialize_expired_unarmed",
                LifecycleTransition::LaunchFailed => "initialize_failed_released",
            },
            Self::ResidencyRecorded { transition, .. } => match transition {
                ResidencyTransition::ParkAccepted => "park_accepted",
                ResidencyTransition::ParkArmed => "park_armed",
                ResidencyTransition::Parked => "parked_committed",
                ResidencyTransition::ParkRefused => "park_refused",
                ResidencyTransition::ParkUncertain => "park_uncertain",
                ResidencyTransition::RestoreAccepted => "restore_accepted",
                ResidencyTransition::RestoreArmed => "restore_armed",
                ResidencyTransition::Restored => "restored_committed",
                ResidencyTransition::RestoreRefused => "restore_refused",
                ResidencyTransition::RestoreUncertain => "restore_uncertain",
                ResidencyTransition::Cancelled => "residency_cancelled",
            },
            Self::ManagedConfigurationAccepted { .. } => "managed_configuration_accepted",
            Self::DeploymentDeleted { .. } => "deployment_deleted",
            Self::CoordinatorSessionStarted { .. } => "coordinator_session_started",
            Self::HostResourcePolicyBootstrapped { .. } => "host_resource_policy_bootstrapped",
            Self::HostResourcePolicyUpdated { .. } => "host_resource_policy_updated",
            Self::InstallationDriftFlagged { .. } => "installation_drift_flagged",
            Self::HostRevoked { .. } => "host_revoked",
            Self::SwitchRecorded { phase, .. } => match phase {
                SwitchPhase::Planned => "switch_planned",
                SwitchPhase::AdmissionClosed => "switch_admission_closed",
                SwitchPhase::Released => "switch_released",
                SwitchPhase::Completed => "switch_completed",
                SwitchPhase::Failed => "switch_failed",
            },
        }
    }

    fn identifiers(&self) -> (Option<&str>, Option<&str>) {
        match self {
            Self::UnarmedStopRecorded {
                operation_id,
                deployment_id,
                ..
            }
            | Self::OrdinaryCleanupRecorded {
                operation_id,
                deployment_id,
                ..
            } => (Some(deployment_id.as_str()), Some(operation_id.as_str())),
            Self::LifecycleRecorded {
                operation_id,
                deployment_id,
                ..
            }
            | Self::ResidencyRecorded {
                operation_id,
                deployment_id,
                ..
            } => (Some(deployment_id.as_str()), Some(operation_id.as_str())),
            Self::ManagedConfigurationAccepted {
                operation_id,
                deployment_id,
                ..
            }
            | Self::DeploymentDeleted {
                operation_id,
                deployment_id,
                ..
            } => (Some(deployment_id.as_str()), Some(operation_id.as_str())),
            Self::CoordinatorSessionStarted { .. } => (None, None),
            Self::HostResourcePolicyBootstrapped { .. } => (None, None),
            Self::HostResourcePolicyUpdated { operation_id, .. } => {
                (None, Some(operation_id.as_str()))
            }
            Self::InstallationDriftFlagged { .. }
            | Self::HostRevoked { .. } => (None, None),
            Self::SwitchRecorded {
                target_deployment, ..
            } => (Some(target_deployment.as_str()), None),
        }
    }
}

impl crate::Store {
    /// ADR 0008 (owner decision 2026-09-23): journal one installation drift a
    /// host reported. Fields are bounded tokens (profile name, `sha256:`
    /// digests or `unmeasured`); anything else is refused.
    pub fn record_installation_drift(
        &self,
        host_id: &str,
        installation: &str,
        registered_digest: &str,
        observed_digest: &str,
    ) -> Result<(), crate::StoreError> {
        let token = |value: &str| {
            !value.is_empty()
                && value.len() <= 256
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.:/".contains(&b))
        };
        if ![host_id, installation, registered_digest, observed_digest]
            .into_iter()
            .all(token)
        {
            return Err(crate::StoreError::Conflict);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        append_event(
            &tx,
            &EventMetadata::InstallationDriftFlagged {
                host_id: host_id.into(),
                installation: installation.into(),
                registered_digest: registered_digest.into(),
                observed_digest: observed_digest.into(),
            },
        )
        .map_err(|_| crate::StoreError::Conflict)?;
        tx.commit()?;
        Ok(())
    }
}

pub(crate) fn append_event(
    tx: &Transaction<'_>,
    event: &EventMetadata,
) -> Result<i64, EventWriteError> {
    append_event_at(tx, event, now_ms())
}
fn append_event_at(
    tx: &Transaction<'_>,
    event: &EventMetadata,
    at: i64,
) -> Result<i64, EventWriteError> {
    if let EventMetadata::UnarmedStopRecorded {
        session_epoch,
        committed_epoch,
        ..
    } = event
    {
        if *session_epoch <= 0 || committed_epoch.is_some() {
            return Err(EventWriteError::InvalidMetadata);
        }
    }
    if let EventMetadata::OrdinaryCleanupRecorded {
        transition,
        committed_epoch,
        session_epoch,
        ..
    } = event
    {
        if *session_epoch <= 0
            || match transition {
                OrdinaryCleanupTransition::Completed => {
                    committed_epoch.is_none_or(|epoch| epoch == 0)
                }
                _ => committed_epoch.is_some(),
            }
        {
            return Err(EventWriteError::InvalidMetadata);
        }
    }
    let payload = serialize_bounded(event)?;
    let (deployment_id, operation_id) = event.identifiers();
    tx.execute("INSERT INTO management_events(recorded_at_ms,kind,deployment_id,operation_id,payload_json) VALUES(?1,?2,?3,?4,?5)", params![at,event.kind(),deployment_id,operation_id,payload])?;
    prune(tx, at)?;
    Ok(tx.last_insert_rowid())
}

fn serialize_bounded(value: &impl Serialize) -> Result<String, EventWriteError> {
    let payload = serde_json::to_string(value)?;
    if payload.len() > MAX_PAYLOAD {
        return Err(EventWriteError::PayloadTooLarge);
    }
    Ok(payload)
}
fn prune(tx: &Transaction<'_>, now: i64) -> rusqlite::Result<()> {
    let floor: Option<i64> = tx.query_row(
        "WITH kept AS (SELECT sequence,ROW_NUMBER() OVER(ORDER BY sequence DESC) n,SUM(length(CAST(payload_json AS BLOB))) OVER(ORDER BY sequence DESC) bytes FROM management_events WHERE recorded_at_ms>?1) SELECT MAX(sequence) FROM management_events WHERE sequence NOT IN(SELECT sequence FROM kept WHERE n<=?2 AND bytes<=?3)",
        params![now-MAX_AGE_MS,MAX_EVENTS,MAX_BYTES], |r| r.get(0))?;
    if let Some(floor) = floor {
        tx.execute("DELETE FROM management_events WHERE sequence<=?1", [floor])?;
        tx.execute(
            "UPDATE event_meta SET retained_after=MAX(retained_after,?1) WHERE singleton=1",
            [floor],
        )?;
    }
    Ok(())
}

impl crate::Store {
    pub fn events_after(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<EventPage, EventReadError> {
        if limit == 0 || limit > MAX_REPLAY_PAGE_SIZE {
            return Err(EventReadError::InvalidLimit);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        prune(&tx, now_ms())?;
        let (incarnation, floor): (String, i64) = tx.query_row(
            "SELECT incarnation,retained_after FROM event_meta WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let high: i64 = tx.query_row(
            "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name='management_events'),0)",
            [],
            |r| r.get(0),
        )?;
        let after = match cursor {
            None => floor,
            Some(c) => parse_cursor(c, &incarnation, floor, high)?,
        };
        let events = {
            let mut stmt = tx.prepare("SELECT sequence,recorded_at_ms,kind,deployment_id,operation_id,payload_json FROM management_events WHERE sequence>?1 ORDER BY sequence LIMIT ?2")?;
            let rows = stmt
                .query_map(params![after, limit as i64], |r| {
                    Ok(ManagementEvent {
                        cursor: EventCursor {
                            incarnation: incarnation.clone(),
                            sequence: r.get(0)?,
                        },
                        recorded_at_ms: r.get(1)?,
                        kind: r.get(2)?,
                        deployment_id: r.get(3)?,
                        operation_id: r.get(4)?,
                        payload_json: r.get(5)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        tx.commit()?;
        Ok(EventPage {
            events,
            high_water: EventCursor {
                incarnation,
                sequence: high,
            },
        })
    }
    pub fn rotate_event_incarnation_after_restore(&self) -> Result<EventCursor, EventReadError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let incarnation = ulid::Ulid::new().to_string();
        let high = tx.query_row(
            "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name='management_events'),0)",
            [],
            |r| r.get(0),
        )?;
        tx.execute(
            "UPDATE event_meta SET incarnation=?1 WHERE singleton=1",
            [&incarnation],
        )?;
        tx.commit()?;
        Ok(EventCursor {
            incarnation,
            sequence: high,
        })
    }
}
fn parse_cursor(
    value: &str,
    incarnation: &str,
    floor: i64,
    high: i64,
) -> Result<i64, EventReadError> {
    let (i, s) = value
        .rsplit_once(':')
        .ok_or(EventReadError::MalformedCursor)?;
    if i.parse::<ulid::Ulid>().is_err() || s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(EventReadError::MalformedCursor);
    }
    let s: i64 = s.parse().map_err(|_| EventReadError::MalformedCursor)?;
    if i != incarnation {
        return Err(EventReadError::WrongIncarnation);
    }
    if s > high {
        return Err(EventReadError::FutureCursor);
    }
    if s < floor {
        return Err(EventReadError::ExpiredCursor);
    }
    Ok(s)
}
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    #[test]
    fn ordinary_cleanup_writer_rejects_epoch_without_completion_and_missing_commit() {
        use super::*;
        for (transition, epoch, valid) in [
            (OrdinaryCleanupTransition::Accepted, None, true),
            (OrdinaryCleanupTransition::Armed, None, true),
            (OrdinaryCleanupTransition::Completed, Some(7), true),
            (OrdinaryCleanupTransition::Accepted, Some(7), false),
            (OrdinaryCleanupTransition::Armed, Some(7), false),
            (OrdinaryCleanupTransition::Completed, None, false),
            (OrdinaryCleanupTransition::Completed, Some(0), false),
        ] {
            let store = crate::Store::open_in_memory().unwrap();
            let tx =
                Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate).unwrap();
            let id = ulid::Ulid::new();
            let result = append_event(
                &tx,
                &EventMetadata::OrdinaryCleanupRecorded {
                    transition,
                    operation_id: EventOperationId::generated(id),
                    deployment_id: EventOperationId::generated(id),
                    step_id: EventOperationId::generated(id),
                    session_epoch: 1,
                    committed_epoch: epoch,
                },
            );
            assert_eq!(result.is_ok(), valid);
            assert_eq!(
                tx.query_row("SELECT COUNT(*) FROM management_events", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                i64::from(valid)
            );
        }
    }
    use super::*;
    use crate::Store;
    fn seed(s: &Store, count: i64, bytes: usize, at: i64) {
        let tx = Transaction::new_unchecked(&s.conn, TransactionBehavior::Immediate).unwrap();
        let p = "x".repeat(bytes);
        for n in 1..=count {
            tx.execute("INSERT INTO management_events(sequence,recorded_at_ms,kind,payload_json) VALUES(?1,?2,'fixture',?3)",params![n,at,p]).unwrap();
        }
        tx.commit().unwrap()
    }
    #[test]
    fn limit_and_cursor_validation_are_distinct() {
        let s = Store::open_in_memory().unwrap();
        seed(&s, 3, 1, now_ms());
        assert!(matches!(
            s.events_after(None, 0),
            Err(EventReadError::InvalidLimit)
        ));
        assert!(matches!(
            s.events_after(None, 1001),
            Err(EventReadError::InvalidLimit)
        ));
        let p = s.events_after(None, 2).unwrap();
        assert_eq!(
            p.events
                .iter()
                .map(|e| e.cursor.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        let i = p.high_water.incarnation;
        assert!(matches!(
            s.events_after(Some("bad"), 1),
            Err(EventReadError::MalformedCursor)
        ));
        assert!(matches!(
            s.events_after(Some("00000000000000000000000000:0"), 1),
            Err(EventReadError::WrongIncarnation)
        ));
        assert!(matches!(
            s.events_after(Some(&format!("{i}:4")), 1),
            Err(EventReadError::FutureCursor)
        ));
        s.conn
            .execute("UPDATE event_meta SET retained_after=2", [])
            .unwrap();
        assert!(matches!(
            s.events_after(Some(&format!("{i}:1")), 1),
            Err(EventReadError::ExpiredCursor)
        ))
    }
    #[test]
    fn exact_retention_and_high_water() {
        let now = now_ms();
        let s = Store::open_in_memory().unwrap();
        seed(&s, 100_001, 1, now);
        let p = s.events_after(None, 1).unwrap();
        assert_eq!(
            (p.events[0].cursor.sequence, p.high_water.sequence),
            (2, 100_001)
        );
        s.conn.execute("DELETE FROM management_events", []).unwrap();
        assert_eq!(
            s.events_after(None, 1).unwrap().high_water.sequence,
            100_001
        );
        let s = Store::open_in_memory().unwrap();
        seed(&s, 4097, MAX_PAYLOAD, now);
        assert_eq!(
            s.events_after(None, 1).unwrap().events[0].cursor.sequence,
            2
        );
        let s = Store::open_in_memory().unwrap();
        seed(&s, 2, 1, now - 1);
        s.conn
            .execute(
                "UPDATE management_events SET recorded_at_ms=?1 WHERE sequence=1",
                [now - MAX_AGE_MS],
            )
            .unwrap();
        assert_eq!(
            s.events_after(None, 10)
                .unwrap()
                .events
                .iter()
                .map(|e| e.cursor.sequence)
                .collect::<Vec<_>>(),
            vec![2]
        )
    }
    #[test]
    fn rollback_restores_event_pruning_and_floor() {
        let s = Store::open_in_memory().unwrap();
        seed(&s, 100_000, 1, now_ms());
        {
            let tx = Transaction::new_unchecked(&s.conn, TransactionBehavior::Immediate).unwrap();
            append_event_at(
                &tx,
                &EventMetadata::CoordinatorSessionStarted { session_epoch: 1 },
                now_ms(),
            )
            .unwrap();
        }
        let got: (i64, i64) = s
            .conn
            .query_row(
                "SELECT COUNT(*),(SELECT retained_after FROM event_meta) FROM management_events",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(got, (100_000, 0))
    }
    #[test]
    fn rotation_invalidates_old_cursor_after_file_restore() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.sqlite3");
        let restored = dir.path().join("restored.sqlite3");
        let s = Store::open(&source).unwrap();
        s.begin_coordinator_session().unwrap();
        let old = s.events_after(None, 1).unwrap().high_water.to_string();
        drop(s);
        std::fs::copy(&source, &restored).unwrap();
        let restored = Store::open(&restored).unwrap();
        restored.rotate_event_incarnation_after_restore().unwrap();
        assert!(matches!(
            restored.events_after(Some(&old), 1),
            Err(EventReadError::WrongIncarnation)
        ))
    }
    #[test]
    fn non_monotonic_expiry_advances_a_prefix_floor() {
        let s = Store::open_in_memory().unwrap();
        let now = now_ms();
        seed(&s, 3, 1, now);
        s.conn
            .execute(
                "UPDATE management_events SET recorded_at_ms=?1 WHERE sequence IN(1,3)",
                [now - MAX_AGE_MS],
            )
            .unwrap();
        let p = s.events_after(None, 10).unwrap();
        assert!(p.events.is_empty());
        assert_eq!(
            (
                p.high_water.sequence,
                s.conn
                    .query_row("SELECT retained_after FROM event_meta", [], |r| r
                        .get::<_, i64>(0))
                    .unwrap()
            ),
            (3, 3)
        )
    }
    #[test]
    fn session_event_is_typed_redacted_and_versioned() {
        let s = Store::open_in_memory().unwrap();
        let session = s.begin_coordinator_session().unwrap();
        let e = s.events_after(None, 10).unwrap().events.pop().unwrap();
        assert_eq!(e.kind, "coordinator_session_started");
        assert_eq!(
            e.payload_json,
            format!(r#"{{"version":"1","session_epoch":{}}}"#, session.epoch())
        );
        assert_eq!((e.deployment_id, e.operation_id), (None, None))
    }
    #[test]
    fn session_reset_rolls_back_when_event_append_fails() {
        let s = Store::open_in_memory().unwrap();
        s.conn.execute_batch("CREATE TRIGGER reject_event BEFORE INSERT ON management_events BEGIN SELECT RAISE(ABORT,'reject'); END;").unwrap();
        assert!(s.begin_coordinator_session().is_err());
        let state: (i64, String) = s
            .conn
            .query_row(
                "SELECT epoch,session_id FROM coordinator_session WHERE singleton=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, (0, String::new()));
        assert_eq!(s.events_after(None, 1).unwrap().high_water.sequence, 0)
    }
    #[test]
    fn malformed_ulid_and_sequence_overflow_are_rejected() {
        let s = Store::open_in_memory().unwrap();
        let i = s.events_after(None, 1).unwrap().high_water.incarnation;
        assert!(matches!(
            s.events_after(Some("not-a-ulid:0"), 1),
            Err(EventReadError::MalformedCursor)
        ));
        assert!(matches!(
            s.events_after(Some(&format!("{i}:9223372036854775808")), 1),
            Err(EventReadError::MalformedCursor)
        ))
    }
    #[test]
    // T21 T22: ADR 0008. A reported installation drift is journaled as a
    // typed event; malformed fields are refused.
    fn installation_drift_is_journaled_as_a_typed_event() {
        let s = Store::open_in_memory().unwrap();
        let digest = format!("sha256:{}", "a".repeat(64));
        s.record_installation_drift("host-1", "local", &digest, "unmeasured")
            .unwrap();
        let page = s.events_after(None, 10).unwrap();
        let event = page.events.last().unwrap();
        assert_eq!(event.kind, "installation_drift_flagged");
        let payload: serde_json::Value = serde_json::from_str(&event.payload_json).unwrap();
        assert_eq!(payload["installation"], "local");
        assert_eq!(payload["observed_digest"], "unmeasured");
        assert!(s
            .record_installation_drift("host-1", "local", &digest, "bad value")
            .is_err());
    }
    #[test]
    fn payload_limit_accepts_exact_bytes_and_rejects_one_more() {
        #[derive(Serialize)]
        struct Fixture {
            body: String,
        }

        let overhead = serde_json::to_string(&Fixture {
            body: String::new(),
        })
        .unwrap()
        .len();
        let exact = Fixture {
            body: "x".repeat(MAX_PAYLOAD - overhead),
        };
        assert_eq!(serialize_bounded(&exact).unwrap().len(), MAX_PAYLOAD);
        let oversized = Fixture {
            body: "x".repeat(MAX_PAYLOAD - overhead + 1),
        };
        assert!(matches!(
            serialize_bounded(&oversized),
            Err(EventWriteError::PayloadTooLarge)
        ))
    }
}

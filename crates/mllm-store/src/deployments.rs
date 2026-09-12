//! Deployment acceptance and per-deployment operation records.

use rusqlite::{params, OptionalExtension};

use mllm_domain::{DeploymentId, LifecycleState, OperationId};

use crate::StoreError;

/// A request to accept (admit) a new deployment into the store.
pub struct AcceptDeployment {
    pub id: DeploymentId,
    pub name: String,
    pub kind: String,
    pub route_model_id: Option<String>,
    pub desired_state: LifecycleState,
    pub schema_version: i64,
    pub idempotency_key: String,
    pub initial_operation_id: OperationId,
}

/// The outcome of a successful `accept_deployment`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    pub deployment_id: DeploymentId,
    pub operation_id: OperationId,
}

/// A stored deployment as read back from the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentRow {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub route_model_id: Option<String>,
    pub desired_state: LifecycleState,
    pub observed_state: LifecycleState,
    pub schema_version: i64,
}

/// Request to record a new control-plane operation on a deployment.
pub struct NewOperation {
    pub id: OperationId,
    pub deployment_id: String,
    pub kind: String,
    pub idempotency_key: Option<String>,
}

/// State of a control-plane operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpState {
    Pending,
    Running,
    Succeeded,
    Failed,
}

impl OpState {
    pub fn as_str(self) -> &'static str {
        match self {
            OpState::Pending => "pending",
            OpState::Running => "running",
            OpState::Succeeded => "succeeded",
            OpState::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Result<OpState, StoreError> {
        match s {
            "pending" => Ok(OpState::Pending),
            "running" => Ok(OpState::Running),
            "succeeded" => Ok(OpState::Succeeded),
            "failed" => Ok(OpState::Failed),
            _ => Err(invalid_column("state")),
        }
    }
}

/// A stored operation record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationRow {
    pub id: String,
    pub deployment_id: String,
    pub kind: String,
    pub state: OpState,
    pub error_code: Option<String>,
    pub accepted_at: String,
    pub updated_at: String,
}

fn invalid_column(name: &str) -> StoreError {
    StoreError::Sql(rusqlite::Error::InvalidColumnType(
        0,
        name.to_string(),
        rusqlite::types::Type::Text,
    ))
}

/// Raw deployment row: id, name, kind, route_model_id, desired_state,
/// observed_state, schema_version.
type RawDeploymentRow = (
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    i64,
);

/// Raw operation row: id, deployment_id, kind, state, error_code,
/// accepted_at, updated_at.
type RawOperationRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    String,
);

fn parse_deployment_id(s: &str) -> Result<DeploymentId, StoreError> {
    ulid::Ulid::from_string(s)
        .map(DeploymentId)
        .map_err(|_| invalid_column("id"))
}

fn lifecycle_to_str(state: LifecycleState) -> &'static str {
    match state {
        LifecycleState::Stopped => "stopped",
        LifecycleState::Starting => "starting",
        LifecycleState::Ready => "ready",
        LifecycleState::Draining => "draining",
        LifecycleState::Parking => "parking",
        LifecycleState::Parked => "parked",
        LifecycleState::Waking => "waking",
        LifecycleState::Stopping => "stopping",
        LifecycleState::Reconciling => "reconciling",
        LifecycleState::Failed => "failed",
    }
}

fn lifecycle_from_str(s: &str) -> Result<LifecycleState, StoreError> {
    match s {
        "stopped" => Ok(LifecycleState::Stopped),
        "starting" => Ok(LifecycleState::Starting),
        "ready" => Ok(LifecycleState::Ready),
        "draining" => Ok(LifecycleState::Draining),
        "parking" => Ok(LifecycleState::Parking),
        "parked" => Ok(LifecycleState::Parked),
        "waking" => Ok(LifecycleState::Waking),
        "stopping" => Ok(LifecycleState::Stopping),
        "reconciling" => Ok(LifecycleState::Reconciling),
        "failed" => Ok(LifecycleState::Failed),
        _ => Err(invalid_column("desired_state")),
    }
}

fn is_unique_violation(err: &StoreError) -> bool {
    matches!(
        err,
        StoreError::Sql(rusqlite::Error::SqliteFailure(e, _))
            if e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
    )
}

/// Acceptance, reads, and operation recording (T08/T09 flow).
impl crate::Store {
    /// One transaction: insert the deployment and its initial operation.
    /// A retry carrying the same `idempotency_key` with the same content
    /// resolves to the existing deployment; with different content it is
    /// an `IdempotencyConflict`, never a silent reuse.
    pub fn accept_deployment(&self, req: AcceptDeployment) -> Result<Accepted, StoreError> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        match self.insert_acceptance(&req) {
            Ok(accepted) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(accepted)
            }
            Err(ref e) if is_unique_violation(e) => {
                self.conn.execute_batch("ROLLBACK")?;
                self.resolve_idempotent(&req)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    fn insert_acceptance(&self, req: &AcceptDeployment) -> Result<Accepted, StoreError> {
        self.conn.execute(
            "INSERT INTO deployments
                (id, name, kind, route_model_id, desired_state,
                 admission_enabled, suspended, current_generation, schema_version)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, 0, 1, ?6)",
            params![
                req.id.to_string(),
                req.name,
                req.kind,
                req.route_model_id,
                lifecycle_to_str(req.desired_state),
                req.schema_version,
            ],
        )?;
        self.conn.execute(
            "INSERT INTO operations
                (id, deployment_id, kind, state, error_code, idempotency_key)
             VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
            params![
                req.initial_operation_id.0,
                req.id.to_string(),
                "deploy",
                OpState::Pending.as_str(),
                req.idempotency_key,
            ],
        )?;
        Ok(Accepted {
            deployment_id: req.id,
            operation_id: req.initial_operation_id.clone(),
        })
    }

    /// Same key + same content => idempotent success with the existing
    /// deployment; same key + different content => conflict.
    fn resolve_idempotent(&self, req: &AcceptDeployment) -> Result<Accepted, StoreError> {
        let existing: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT deployment_id, id FROM operations WHERE idempotency_key = ?1",
                [&req.idempotency_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((deployment_id, operation_id)) = existing else {
            // The constraint that fired was not the idempotency key
            // (e.g. the name is already taken by another deployment).
            return Err(StoreError::Conflict);
        };
        let row = self
            .get_deployment(&deployment_id)?
            .ok_or(StoreError::Conflict)?;
        let route_matches = row.route_model_id == req.route_model_id;
        if row.name == req.name && row.kind == req.kind && route_matches {
            Ok(Accepted {
                deployment_id: parse_deployment_id(&deployment_id)?,
                operation_id: OperationId(operation_id),
            })
        } else {
            Err(StoreError::IdempotencyConflict)
        }
    }

    pub fn get_deployment(&self, id: &str) -> Result<Option<DeploymentRow>, StoreError> {
        let raw: Option<RawDeploymentRow> = self
            .conn
            .query_row(
                "SELECT id, name, kind, route_model_id, desired_state, observed_state,
                        schema_version
                 FROM deployments WHERE id = ?1",
                [id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .optional()?;
        raw.map(
            |(id, name, kind, route_model_id, desired_state, observed_state, schema_version)| {
                Ok(DeploymentRow {
                    id,
                    name,
                    kind,
                    route_model_id,
                    desired_state: lifecycle_from_str(&desired_state)?,
                    observed_state: lifecycle_from_str(&observed_state)?,
                    schema_version,
                })
            },
        )
        .transpose()
    }

    /// Record the observed half of the lifecycle state (controller-owned).
    pub fn set_observed_state(&self, id: &str, state: LifecycleState) -> Result<(), StoreError> {
        let updated = self.conn.execute(
            "UPDATE deployments
             SET observed_state = ?2,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
            params![id, lifecycle_to_str(state)],
        )?;
        if updated == 0 {
            return Err(StoreError::Conflict);
        }
        Ok(())
    }

    /// Append a journal entry (no inference bodies, ever — evidence only).
    pub fn record_journal(
        &self,
        host_id: Option<&str>,
        operation_id: Option<&str>,
        state: Option<&str>,
        evidence: &str,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO journal_entries (id, host_id, operation_id, state, evidence)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                ulid::Ulid::new().to_string(),
                host_id,
                operation_id,
                state,
                evidence,
            ],
        )?;
        Ok(())
    }

    /// All journal evidence recorded for an operation, in insertion order.
    pub fn journal_evidence(&self, operation_id: &str) -> Result<Vec<String>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT evidence FROM journal_entries WHERE operation_id = ?1 ORDER BY rowid",
        )?;
        let rows = stmt.query_map([operation_id], |r| r.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// The most recent operation recorded for a deployment.
    pub fn latest_operation(
        &self,
        deployment_id: &str,
    ) -> Result<Option<OperationRow>, StoreError> {
        let raw: Option<RawOperationRow> = self
            .conn
            .query_row(
                "SELECT id, deployment_id, kind, state, error_code, accepted_at, updated_at
                 FROM operations WHERE deployment_id = ?1
                 ORDER BY accepted_at DESC, rowid DESC LIMIT 1",
                [deployment_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .optional()?;
        raw.map(
            |(id, deployment_id, kind, state, error_code, accepted_at, updated_at)| {
                Ok(OperationRow {
                    id,
                    deployment_id,
                    kind,
                    state: OpState::parse(&state)?,
                    error_code,
                    accepted_at,
                    updated_at,
                })
            },
        )
        .transpose()
    }

    /// Fetch one operation record by id.
    pub fn get_operation(&self, id: &str) -> Result<Option<OperationRow>, StoreError> {
        let raw: Option<RawOperationRow> = self
            .conn
            .query_row(
                "SELECT id, deployment_id, kind, state, error_code, accepted_at, updated_at
                 FROM operations WHERE id = ?1",
                [id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .optional()?;
        raw.map(
            |(id, deployment_id, kind, state, error_code, accepted_at, updated_at)| {
                Ok(OperationRow {
                    id,
                    deployment_id,
                    kind,
                    state: OpState::parse(&state)?,
                    error_code,
                    accepted_at,
                    updated_at,
                })
            },
        )
        .transpose()
    }

    pub fn record_operation(&self, op: NewOperation) -> Result<OperationRow, StoreError> {
        self.conn.execute(
            "INSERT INTO operations (id, deployment_id, kind, state, error_code, idempotency_key)
             VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
            params![
                op.id.0,
                op.deployment_id,
                op.kind,
                OpState::Pending.as_str(),
                op.idempotency_key,
            ],
        )?;
        self.latest_operation(&op.deployment_id)?
            .filter(|row| row.id == op.id.0)
            .ok_or(StoreError::Conflict)
    }

    pub fn update_operation_state(
        &self,
        id: &str,
        state: OpState,
        error_code: Option<&str>,
    ) -> Result<(), StoreError> {
        let updated = self.conn.execute(
            "UPDATE operations
             SET state = ?2, error_code = ?3,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
            params![id, state.as_str(), error_code],
        )?;
        if updated == 0 {
            return Err(StoreError::Conflict);
        }
        Ok(())
    }

    pub fn deployment_count(&self) -> Result<i64, StoreError> {
        self.conn
            .query_row("SELECT COUNT(*) FROM deployments", [], |row| row.get(0))
            .map_err(StoreError::from)
    }

    /// All enabled route ids for `/v1/models` (F1 design §5): enabled
    /// routes are listed without waking anything.
    pub fn list_enabled_route_ids(&self) -> Result<Vec<String>, StoreError> {
        self.conn
            .prepare(
                "SELECT route_model_id FROM deployments \
                 WHERE admission_enabled = 1 AND suspended = 0 AND route_model_id IS NOT NULL \
                 ORDER BY route_model_id",
            )?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Alias resolution: route_model_id → the deployment serving it (no
    /// model-name guessing — SPEC §10).
    pub fn find_deployment_by_route(&self, route: &str) -> Result<Option<DeploymentRow>, StoreError> {
        let raw: Option<RawDeploymentRow> = self
            .conn
            .query_row(
                "SELECT id, name, kind, route_model_id, desired_state, observed_state,
                        schema_version
                 FROM deployments WHERE route_model_id = ?1 ORDER BY updated_at DESC LIMIT 1",
                [route],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .optional()?;
        raw.map(
            |(id, name, kind, route_model_id, desired_state, observed_state, schema_version)| {
                Ok(DeploymentRow {
                    id,
                    name,
                    kind,
                    route_model_id,
                    desired_state: lifecycle_from_str(&desired_state)?,
                    observed_state: lifecycle_from_str(&observed_state)?,
                    schema_version,
                })
            },
        )
        .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AcceptDeployment, Store};

    fn req(name: &str, key: &str) -> AcceptDeployment {
        AcceptDeployment {
            id: DeploymentId::new(),
            name: name.to_string(),
            kind: "model".to_string(),
            route_model_id: None,
            desired_state: LifecycleState::Stopped,
            schema_version: 1,
            idempotency_key: key.to_string(),
            initial_operation_id: OperationId(format!("op-{}", ulid::Ulid::new())),
        }
    }

    #[test]
    fn op_state_round_trips() {
        for state in [
            OpState::Pending,
            OpState::Running,
            OpState::Succeeded,
            OpState::Failed,
        ] {
            assert_eq!(OpState::parse(state.as_str()).unwrap(), state);
        }
        assert!(OpState::parse("bogus").is_err());
    }

    #[test]
    fn lifecycle_strings_round_trip() {
        for state in LifecycleState::ALL {
            assert_eq!(
                lifecycle_from_str(lifecycle_to_str(*state)).unwrap(),
                *state
            );
        }
    }

    #[test]
    fn unknown_operation_update_is_conflict() {
        let s = Store::open_in_memory().unwrap();
        assert!(matches!(
            s.update_operation_state("missing", OpState::Running, None),
            Err(StoreError::Conflict)
        ));
    }

    #[test]
    fn record_operation_appends_and_orders() {
        let s = Store::open_in_memory().unwrap();
        let accepted = s.accept_deployment(req("d1", "k1")).unwrap();
        let dep = accepted.deployment_id.to_string();
        let op = s
            .record_operation(NewOperation {
                id: OperationId("op-2".into()),
                deployment_id: dep.clone(),
                kind: "start".into(),
                idempotency_key: None,
            })
            .unwrap();
        assert_eq!(op.state, OpState::Pending);
        let latest = s.latest_operation(&dep).unwrap().unwrap();
        assert_eq!(latest.id, "op-2");
        s.update_operation_state("op-2", OpState::Failed, Some("boom"))
            .unwrap();
        let latest = s.latest_operation(&dep).unwrap().unwrap();
        assert_eq!(latest.state, OpState::Failed);
        assert_eq!(latest.error_code.as_deref(), Some("boom"));
    }

    #[test]
    fn fresh_deployments_observe_stopped_and_observed_state_round_trips() {
        let s = Store::open_in_memory().unwrap();
        let accepted = s.accept_deployment(req("d1", "k1")).unwrap();
        let dep = accepted.deployment_id.to_string();
        assert_eq!(
            s.get_deployment(&dep).unwrap().unwrap().observed_state,
            LifecycleState::Stopped
        );
        s.set_observed_state(&dep, LifecycleState::Ready).unwrap();
        assert_eq!(
            s.get_deployment(&dep).unwrap().unwrap().observed_state,
            LifecycleState::Ready
        );
        assert!(matches!(
            s.set_observed_state("missing", LifecycleState::Ready),
            Err(StoreError::Conflict)
        ));
    }

    #[test]
    fn journal_entries_record_evidence() {
        let s = Store::open_in_memory().unwrap();
        s.record_journal(Some("local"), Some("op-1"), Some("ready"), "{}")
            .unwrap();
        let (op, evidence): (Option<String>, String) = s
            .conn
            .query_row(
                "SELECT operation_id, evidence FROM journal_entries",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(op.as_deref(), Some("op-1"));
        assert_eq!(evidence, "{}");
    }
}

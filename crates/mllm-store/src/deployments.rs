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
/// SPEC §8.2: a deployment's current effective configuration (raw, unredacted).
#[derive(Debug, Clone)]
pub struct EffectiveConfiguration {
    pub deployment_id: String,
    pub revision: i64,
    pub effective: serde_json::Value,
    /// ADR 0013 §3: each allowed host's resolution of this revision.
    pub hosts: Vec<HostEffectiveConfiguration>,
}

/// One allowed host's resolution of a revision.
#[derive(Debug, Clone)]
pub struct HostEffectiveConfiguration {
    pub host_id: String,
    /// `resolved` or `refused`.
    pub outcome: String,
    pub effective: Option<serde_json::Value>,
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentRow {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub route_model_id: Option<String>,
    pub desired_state: LifecycleState,
    pub observed_state: LifecycleState,
    pub schema_version: i64,
    pub current_generation: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationRow {
    pub generation: i64,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub outcome: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationRow {
    pub owner_id: String,
    pub domain_id: Option<String>,
    pub bytes: i64,
    pub phase: String,
    pub exclusive_devices: Vec<String>,
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

    pub(crate) fn parse(s: &str) -> Result<OpState, StoreError> {
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

    /// The deployment's current effective revision.
    ///
    /// Commands fence on this, and it is not the same as `schema_version`: a caller
    /// that confuses them gets a revision conflict rather than an obvious error.
    pub fn current_revision(&self, id: &str) -> Result<Option<i64>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT revision FROM deployments WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn get_deployment(&self, id: &str) -> Result<Option<DeploymentRow>, StoreError> {
        let raw: Option<RawDeploymentRow> = self
            .conn
            .query_row(
                "SELECT id, name, kind, route_model_id, desired_state, observed_state,
                        schema_version, current_generation
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
                        row.get(7)?,
                    ))
                },
            )
            .optional()?;
        raw.map(
            |(
                id,
                name,
                kind,
                route_model_id,
                desired_state,
                observed_state,
                schema_version,
                current_generation,
            )| {
                Ok(DeploymentRow {
                    id,
                    name,
                    kind,
                    route_model_id,
                    desired_state: lifecycle_from_str(&desired_state)?,
                    observed_state: lifecycle_from_str(&observed_state)?,
                    schema_version,
                    current_generation,
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

    /// Whether the coordinator currently permits dispatch to this deployment's
    /// engine. SPEC §13.2: a remote engine whose readiness is being re-proven
    /// keeps its observed state but must not receive work. Absent reads false.
    pub fn dispatch_enabled(&self, id: &str) -> Result<bool, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT dispatch_enabled=1 FROM deployments WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false))
    }

    /// Close or open one deployment's own admission.
    ///
    /// ADR 0011 decision 4: a failed deployment stops itself. The coordinator's
    /// process-wide `accepting` flag is for shutdown; one deployment's bad
    /// configuration or failed step must not touch it.
    pub fn set_admission_enabled(&self, id: &str, enabled: bool) -> Result<(), StoreError> {
        // One savepoint: the instances and the deployment row change together or
        // not at all, whether or not the caller already holds a transaction.
        self.conn.execute_batch("SAVEPOINT set_admission_enabled")?;
        let result = (|| {
            // ADR 0013 §6: the deployment's admission switch covers every instance.
            self.conn.execute(
                "UPDATE deployment_instances SET admission_enabled=?2 WHERE deployment_id=?1",
                params![id, enabled as i64],
            )?;
            let updated = self.conn.execute(
                "UPDATE deployments
                 SET admission_enabled = ?2,
                     updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                 WHERE id = ?1",
                params![id, enabled as i64],
            )?;
            if updated == 0 {
                return Err(StoreError::Conflict);
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.conn.execute_batch("RELEASE set_admission_enabled")?;
                Ok(())
            }
            Err(error) => {
                let _ = self.conn.execute_batch(
                    "ROLLBACK TO set_admission_enabled; RELEASE set_admission_enabled",
                );
                Err(error)
            }
        }
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

    /// Whether the lifecycle run behind an operation is retained as uncertain.
    ///
    /// An uncertain run is not terminal: the operation stays running, its arm is
    /// kept, and the coordinator resolves it only against a gone-proof. It is still
    /// a durable, observable condition, and a caller waiting on the operation must
    /// be told it at once rather than after its own wait runs out (SPEC §13.2).
    pub fn operation_is_uncertain(&self, operation_id: &str) -> Result<bool, StoreError> {
        self.conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM lifecycle_runs WHERE operation_id=?1 AND state='uncertain')",
                [operation_id],
                |row| row.get(0),
            )
            .map_err(StoreError::from)
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
                 FROM operations WHERE id = ?1 AND deployment_id IS NOT NULL",
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

    /// The routes a deployment's effective revision serves, in the order the frozen
    /// configuration records them.
    ///
    /// SPEC §3: the first of them is the name the engine was launched to answer to,
    /// so anything addressing that engine must use it rather than the public alias a
    /// client asked for. The two are the same for a single-route deployment and
    /// differ as soon as one carries aliases, which is exactly when guessing would
    /// send an engine a model name it does not serve.
    ///
    /// Read by decoding the frozen revision's `routes` array, which is the same
    /// JSON the launch plan renders `--served-model-name` from, so the router and
    /// the engine cannot disagree about the name. `deployment_routes` cannot answer
    /// this: its rows carry no ordinal, so the only order a query can impose on
    /// them is route text, which is the configured order only by accident. Only the
    /// `routes` field is decoded; nothing here re-resolves the model against the
    /// host's store, which is work a request path should not do.
    ///
    /// A deployment created before managed configuration has no frozen revision and
    /// keeps its single route in the column on its own row, so both forms are read.
    /// An unknown deployment yields an empty list.
    /// SPEC §8.2 (`inspect deployment --effective-config`): the current
    /// revision's effective configuration, found by id or name, with each
    /// allowed host's resolution of it (ADR 0013 §3). Raw stored values: the
    /// management boundary redacts before anything leaves the service.
    pub fn effective_configuration(
        &self,
        deployment: &str,
    ) -> Result<Option<EffectiveConfiguration>, StoreError> {
        let undecodable = |id: &str, e: serde_json::Error| {
            StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("deployment {id} has an undecodable frozen revision: {e}"),
            ))
        };
        let row: Option<(String, i64, String)> = self
            .conn
            .query_row(
                "SELECT d.id, d.revision, e.effective_json FROM deployments d \
                 JOIN effective_revisions e \
                   ON e.deployment_id=d.id AND e.revision=d.revision \
                 WHERE d.id=?1 OR d.name=?1 ORDER BY d.id=?1 DESC LIMIT 1",
                params![deployment],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((deployment_id, revision, effective_json)) = row else {
            return Ok(None);
        };
        let effective =
            serde_json::from_str(&effective_json).map_err(|e| undecodable(&deployment_id, e))?;
        let mut hosts = Vec::new();
        let mut statement = self.conn.prepare(
            "SELECT host_id, outcome, effective_json, diagnostic FROM host_effective_revisions \
             WHERE deployment_id=?1 AND revision=?2 ORDER BY host_id",
        )?;
        let rows = statement.query_map(params![deployment_id, revision], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?;
        for row in rows {
            let (host_id, outcome, json, diagnostic) = row?;
            let effective = json
                .map(|json| serde_json::from_str(&json))
                .transpose()
                .map_err(|e| undecodable(&deployment_id, e))?;
            hosts.push(HostEffectiveConfiguration {
                host_id,
                outcome,
                effective,
                diagnostic,
            });
        }
        Ok(Some(EffectiveConfiguration {
            deployment_id,
            revision,
            effective,
            hosts,
        }))
    }

    pub fn effective_routes(&self, deployment_id: &str) -> Result<Vec<String>, StoreError> {
        let frozen: Option<String> = self
            .conn
            .query_row(
                "SELECT e.effective_json FROM deployments d \
                 JOIN effective_revisions e \
                   ON e.deployment_id=d.id AND e.revision=d.revision \
                 WHERE d.id=?1",
                params![deployment_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(effective_json) = frozen {
            let effective: serde_json::Value =
                serde_json::from_str(&effective_json).map_err(|e| {
                    StoreError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "deployment {deployment_id} has an undecodable frozen revision: {e}"
                        ),
                    ))
                })?;
            let routes = effective
                .get("routes")
                .and_then(serde_json::Value::as_array)
                .map(|routes| {
                    routes
                        .iter()
                        .map(|route| route.as_str().map(str::to_string))
                        .collect::<Option<Vec<_>>>()
                })
                .ok_or_else(|| {
                    StoreError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "deployment {deployment_id} froze a revision without a routes list"
                        ),
                    ))
                })?
                .ok_or_else(|| {
                    StoreError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("deployment {deployment_id} froze a route that is not a string"),
                    ))
                })?;
            return Ok(routes);
        }
        let legacy: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT route_model_id FROM deployments WHERE id=?1",
                params![deployment_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(legacy.flatten().into_iter().collect())
    }

    /// All enabled route ids for `/v1/models` (F1 design §5): enabled
    /// routes are listed without waking anything.
    pub fn list_enabled_route_ids(&self) -> Result<Vec<String>, StoreError> {
        self.conn
            .prepare(
                // A managed configuration records its routes in `deployment_routes`
                // and clears the column, so both have to be read. The gate is the
                // same for either: admission open and the deployment not suspended.
                "SELECT route FROM (\
                   SELECT route_model_id AS route, id AS deployment_id FROM deployments \
                     WHERE route_model_id IS NOT NULL \
                   UNION \
                   SELECT route, deployment_id FROM deployment_routes\
                 ) r JOIN deployments d ON d.id = r.deployment_id \
                 WHERE d.admission_enabled = 1 AND d.suspended = 0 \
                 ORDER BY route",
            )?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Alias resolution: route_model_id → the deployment serving it (no
    /// model-name guessing — SPEC §10).
    pub fn find_deployment_by_route(
        &self,
        route: &str,
    ) -> Result<Option<DeploymentRow>, StoreError> {
        let raw: Option<RawDeploymentRow> = self
            .conn
            .query_row(
                // Either route form resolves to the same deployment. Managed
                // creation refuses a route already claimed elsewhere, so a route
                // cannot name two deployments across the two tables.
                "SELECT d.id, d.name, d.kind, d.route_model_id, d.desired_state, d.observed_state,
                        d.schema_version, d.current_generation
                 FROM deployments d
                 WHERE d.route_model_id = ?1
                    OR EXISTS(SELECT 1 FROM deployment_routes
                              WHERE route = ?1 AND deployment_id = d.id)
                 ORDER BY d.updated_at DESC LIMIT 1",
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
                        row.get(7)?,
                    ))
                },
            )
            .optional()?;
        raw.map(
            |(
                id,
                name,
                kind,
                route_model_id,
                desired_state,
                observed_state,
                schema_version,
                current_generation,
            )| {
                Ok(DeploymentRow {
                    id,
                    name,
                    kind,
                    route_model_id,
                    desired_state: lifecycle_from_str(&desired_state)?,
                    observed_state: lifecycle_from_str(&observed_state)?,
                    schema_version,
                    current_generation,
                })
            },
        )
        .transpose()
    }

    /// Monotonic generation bump: increments and records history (F1 G3).
    /// Generations are never reset — a fresh controller continues from the
    /// persisted value.
    pub fn bump_generation(&self, deployment_id: &str) -> Result<i64, StoreError> {
        let conn = &self.conn;
        conn.execute(
            "UPDATE deployments SET current_generation = current_generation + 1,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
            [deployment_id],
        )
        .map_err(StoreError::from)?;
        let gen: i64 = conn
            .query_row(
                "SELECT current_generation FROM deployments WHERE id = ?1",
                [deployment_id],
                |row| row.get(0),
            )
            .map_err(StoreError::from)?;
        // ADR 0013 §5: the F1 bump fences the deployment's first instance, the
        // one a single-instance deployment runs.
        conn.execute(
            "UPDATE deployment_instances SET generation=?2 WHERE deployment_id=?1 AND instance_index=0",
            params![deployment_id, gen],
        )
        .map_err(StoreError::from)?;
        conn.execute(
            "INSERT INTO generation_history(deployment_id, generation) VALUES (?1, ?2)",
            params![deployment_id, gen],
        )
        .map_err(StoreError::from)?;
        Ok(gen)
    }

    /// Generation history for a deployment (asc order).
    pub fn generation_history(
        &self,
        deployment_id: &str,
    ) -> Result<Vec<GenerationRow>, StoreError> {
        let conn = &self.conn;
        let mut stmt = conn.prepare(
            "SELECT generation, started_at, ended_at, outcome FROM generation_history
             WHERE deployment_id = ?1 ORDER BY generation ASC",
        )?;
        let rows = stmt
            .query_map([deployment_id], |row| {
                Ok(GenerationRow {
                    generation: row.get(0)?,
                    started_at: row.get(1)?,
                    ended_at: row.get(2)?,
                    outcome: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Whether an operator has stopped this deployment (T10, SPEC §6.3).
    ///
    /// Read only where automatic activation is decided. It is deliberately not the
    /// `suspended` flag: that one means "eligible to proceed" everywhere in the
    /// ordinary lifecycle, and setting it on a stop breaks the completion, replay
    /// and expiry of the stop itself.
    pub fn is_admin_stopped(&self, id: &str) -> Result<bool, StoreError> {
        self.conn
            .query_row(
                "SELECT admin_stopped FROM deployments WHERE id = ?1",
                [id],
                |row| row.get::<_, i64>(0),
            )
            .map(|value| value != 0)
            .map_err(StoreError::from)
    }

    /// Clear or set the operator's stop intent outside an acceptance.
    ///
    /// An explicit Start uses this: SPEC §6.3 says a start enables the deployment,
    /// so it must lift a previous administrative stop rather than be refused by it.
    pub fn set_admin_stopped(&self, id: &str, stopped: bool) -> Result<(), StoreError> {
        let updated = self.conn.execute(
            "UPDATE deployments SET admin_stopped = ?2 WHERE id = ?1",
            params![id, stopped as i64],
        )?;
        if updated == 0 {
            return Err(invalid_column("deployments"));
        }
        Ok(())
    }

    /// The F1 suspension flag. Nothing in production writes it; the ordinary
    /// lifecycle reads it as an eligibility gate.
    pub fn set_suspended(&self, id: &str, suspended: bool) -> Result<(), StoreError> {
        let updated = self.conn.execute(
            "UPDATE deployments SET suspended = ?2 WHERE id = ?1",
            params![id, suspended as i64],
        )?;
        if updated == 0 {
            return Err(invalid_column("deployments"));
        }
        Ok(())
    }

    pub fn is_suspended(&self, id: &str) -> Result<bool, StoreError> {
        let v: i64 = self
            .conn
            .query_row(
                "SELECT suspended FROM deployments WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .map_err(StoreError::from)?;
        Ok(v != 0)
    }

    /// Operations of one kind for a deployment (asc order) — used by the
    /// switching engine and tests (T15: exactly one wake).
    pub fn operations_of_kind(
        &self,
        deployment_id: &str,
        kind: &str,
    ) -> Result<Vec<OperationRow>, StoreError> {
        let conn = &self.conn;
        let mut stmt = conn.prepare(
            "SELECT id, deployment_id, kind, state, error_code, accepted_at, updated_at
             FROM operations WHERE deployment_id = ?1 AND kind = ?2 ORDER BY accepted_at ASC",
        )?;
        let rows = stmt
            .query_map(params![deployment_id, kind], |row| {
                Ok(OperationRow {
                    id: row.get(0)?,
                    deployment_id: row.get(1)?,
                    kind: row.get(2)?,
                    state: OpState::parse(&row.get::<_, String>(3)?).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            3,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?,
                    error_code: row.get(4)?,
                    accepted_at: row.get(5)?,
                    updated_at: row.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Journal evidence entries for a deployment (bounded, evidence only).
    pub fn journal_evidence_of(&self, deployment_id: &str) -> Result<Vec<String>, StoreError> {
        let conn = &self.conn;
        let mut stmt = conn.prepare(
            "SELECT j.evidence FROM journal_entries j
             JOIN operations o ON j.operation_id = o.id
             WHERE o.deployment_id = ?1 ORDER BY j.id ASC",
        )?;
        let rows = stmt
            .query_map([deployment_id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Deployments currently READY, excluding the named one — the pool
    /// holders the switching engine must drain (F1 design §5).
    pub fn ready_deployments_excluding(&self, exclude: &str) -> Result<Vec<String>, StoreError> {
        let conn = &self.conn;
        let mut stmt =
            conn.prepare("SELECT id FROM deployments WHERE observed_state = 'ready' AND id != ?1")?;
        let rows = stmt
            .query_map([exclude], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Whether a supervisor integration guarantees restart behavior for a
    /// deployment (SPEC §5.2: attached without one marks guarantees
    /// unavailable). F1: only managed deployments carry the guarantee.
    pub fn has_supervisor_guarantee(&self, id: &str) -> Result<bool, StoreError> {
        let kind: String = self
            .conn
            .query_row("SELECT kind FROM deployments WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .map_err(StoreError::from)?;
        Ok(kind != "attached")
    }

    /// ADR 0011 decision 4, ADR 0013 §6: close one instance's own admission.
    /// An instance that gave up is a per-instance failure; its siblings keep
    /// being admitted. `generation` names the instance incarnation that failed
    /// (ADR 0013 §5: deployment and generation identify one incarnation).
    ///
    /// T18: when that incarnation is no longer current (a stop or a later
    /// activation fenced it) the report is stale and nothing closes. There is
    /// deliberately no deployment-wide fallback: one instance's failure must
    /// never close its siblings. Prefer [`Self::close_instance_admission_at`],
    /// which also names the instance.
    pub fn close_instance_admission(
        &self,
        deployment_id: &str,
        generation: i64,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE deployment_instances SET admission_enabled=0 WHERE deployment_id=?1 AND generation=?2",
            params![deployment_id, generation],
        )?;
        Ok(())
    }

    /// ADR 0011 decision 4, ADR 0013 §6: close exactly instance
    /// `instance_index` of the deployment, fenced by the generation its plan
    /// carried. Returns whether the instance was closed; `false` means the
    /// incarnation is stale (T18) and nothing changed. Siblings and the
    /// deployment-level switch are never touched.
    pub fn close_instance_admission_at(
        &self,
        deployment_id: &str,
        instance_index: i64,
        generation: i64,
    ) -> Result<bool, StoreError> {
        let updated = self.conn.execute(
            "UPDATE deployment_instances SET admission_enabled=0
             WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3",
            params![deployment_id, instance_index, generation],
        )?;
        Ok(updated == 1)
    }

    /// Stale-generation check (T18): observed must be >= current. ADR 0013 §5:
    /// a generation that is some instance's current one is current.
    pub fn check_generation(&self, deployment_id: &str, observed: i64) -> Result<i64, StoreError> {
        let instance: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM deployment_instances WHERE deployment_id=?1 AND generation=?2)",
            params![deployment_id, observed],
            |row| row.get(0),
        )?;
        if instance {
            return Ok(observed);
        }
        let current: i64 = self
            .conn
            .query_row(
                "SELECT current_generation FROM deployments WHERE id = ?1",
                [deployment_id],
                |row| row.get(0),
            )
            .map_err(StoreError::from)?;
        if observed >= current {
            Ok(current)
        } else {
            Err(StoreError::StaleGeneration)
        }
    }

    /// Persist the initial reservation intent at acceptance: an owner
    /// account (deployment allocation) plus its physical reservation.
    pub fn insert_reservation(&self, row: &ReservationRow) -> Result<(), StoreError> {
        let conn = &self.conn;
        conn.execute(
            "INSERT OR IGNORE INTO owners(id, kind, deployment_id) VALUES (?1, 'deployment_allocation', ?2)",
            params![row.owner_id, row.owner_id],
        )
        .map_err(StoreError::from)?;
        conn.execute(
            "INSERT INTO reservations(owner_id, domain_id, bytes, phase, exclusive_devices)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                row.owner_id,
                row.domain_id,
                row.bytes,
                row.phase,
                serde_json::to_string(&row.exclusive_devices).map_err(|e| StoreError::Sql(
                    rusqlite::Error::ToSqlConversionFailure(Box::new(e))
                ))?
            ],
        )
        .map_err(StoreError::from)?;
        Ok(())
    }

    pub fn reservations_for_owner(
        &self,
        owner_id: &str,
    ) -> Result<Vec<ReservationRow>, StoreError> {
        let conn = &self.conn;
        let mut stmt = conn.prepare(
            "SELECT owner_id, domain_id, bytes, phase, exclusive_devices
             FROM reservations WHERE owner_id = ?1 ORDER BY rowid ASC",
        )?;
        let rows = stmt
            .query_map([owner_id], |row| {
                let devices: String = row.get(4)?;
                Ok(ReservationRow {
                    owner_id: row.get(0)?,
                    domain_id: row.get(1)?,
                    bytes: row.get(2)?,
                    phase: row.get(3)?,
                    exclusive_devices: serde_json::from_str(&devices).unwrap_or_default(),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
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

    /// A managed deployment's routes live in `deployment_routes`; the column on
    /// the deployment row is the legacy single-route form and managed creation
    /// deliberately clears it. A router that reads only the column serves nothing
    /// a managed configuration created, which is every deployment the CLI makes.
    #[test]
    fn a_managed_route_is_servable() {
        let store = Store::open_in_memory().unwrap();
        let accepted = store.accept_deployment(req("managed", "managed")).unwrap();
        let id = accepted.deployment_id.to_string();
        store
            .conn
            .execute(
                "UPDATE deployments SET admission_enabled=1,observed_state='ready' WHERE id=?1",
                [&id],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO deployment_routes(route,deployment_id) VALUES('managed-route',?1)",
                [&id],
            )
            .unwrap();

        assert_eq!(
            store.list_enabled_route_ids().unwrap(),
            vec!["managed-route".to_string()],
            "a managed route is listed"
        );
        let found = store
            .find_deployment_by_route("managed-route")
            .unwrap()
            .expect("a managed route resolves to its deployment");
        assert_eq!(found.id, id);
    }

    /// Suspension and closed admission still hide a managed route, exactly as they
    /// hide a legacy one. The route's storage changed; the gate did not.
    #[test]
    fn a_suspended_managed_route_is_not_listed() {
        let store = Store::open_in_memory().unwrap();
        let accepted = store.accept_deployment(req("managed", "managed")).unwrap();
        let id = accepted.deployment_id.to_string();
        store
            .conn
            .execute(
                "INSERT INTO deployment_routes(route,deployment_id) VALUES('managed-route',?1)",
                [&id],
            )
            .unwrap();
        for sql in [
            "UPDATE deployments SET admission_enabled=1,suspended=1 WHERE id=?1",
            "UPDATE deployments SET admission_enabled=0,suspended=0 WHERE id=?1",
        ] {
            store.conn.execute(sql, [&id]).unwrap();
            assert!(
                store.list_enabled_route_ids().unwrap().is_empty(),
                "a closed or suspended deployment offers no route"
            );
        }
    }

    /// SPEC §3: the engine is launched to answer to the frozen revision's first
    /// route, so the name the router addresses it by has to come from that same
    /// list in that same order. `deployment_routes` carries no ordinal, so a query
    /// over it can only order by route text; here that would name "alpha" and every
    /// request would reach an engine serving "zeta".
    // T19
    #[test]
    fn the_served_name_is_the_frozen_revisions_first_route_not_the_alphabetical_one() {
        let store = Store::open_in_memory().unwrap();
        let accepted = store.accept_deployment(req("ordered", "ordered")).unwrap();
        let id = accepted.deployment_id.to_string();
        store
            .conn
            .execute(
                "INSERT INTO effective_revisions(deployment_id,revision,effective_json,fingerprint) \
                 VALUES(?1,1,?2,'fp')",
                params![&id, r#"{"routes":["zeta","alpha"]}"#],
            )
            .unwrap();
        for route in ["alpha", "zeta"] {
            store
                .conn
                .execute(
                    "INSERT INTO deployment_routes(route,deployment_id) VALUES(?1,?2)",
                    params![route, &id],
                )
                .unwrap();
        }

        assert_eq!(
            store.effective_routes(&id).unwrap(),
            vec!["zeta".to_string(), "alpha".to_string()],
            "the frozen revision's order is preserved"
        );
    }

    /// A deployment that predates managed configuration has no frozen revision and
    /// keeps its one route in the column, which is still the answer.
    // T19
    #[test]
    fn a_legacy_deployment_without_a_frozen_revision_still_reports_its_route() {
        let store = Store::open_in_memory().unwrap();
        let mut request = req("legacy", "legacy");
        request.route_model_id = Some("legacy-route".to_string());
        let accepted = store.accept_deployment(request).unwrap();
        assert_eq!(
            store
                .effective_routes(&accepted.deployment_id.to_string())
                .unwrap(),
            vec!["legacy-route".to_string()]
        );
        assert!(store.effective_routes("missing").unwrap().is_empty());
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

    /// A two-instance managed deployment: instance 0 at generation 4 and
    /// instance 1 at generation 5, both admitted.
    fn two_admitted_instances(s: &Store) {
        s.conn
            .execute_batch(
                r#"INSERT INTO deployments(id,name,kind,desired_state,observed_state,admission_enabled,dispatch_enabled,suspended,current_generation,schema_version,revision) VALUES('d','d','model','ready','ready',1,1,0,5,1,1);
                INSERT INTO effective_revisions VALUES('d',1,'{}','f');
                INSERT INTO deployment_revision_instances(deployment_id,revision,instances,placement_json) VALUES('d',1,2,'{"hosts":null,"selector":{},"strategy":"spread","max_per_host":null}');
                INSERT INTO deployment_instances(deployment_id,instance_index) VALUES('d',1);
                UPDATE deployment_instances SET revision=1,generation=4,desired_state='ready',observed_state='ready',admission_enabled=1,dispatch_enabled=1 WHERE deployment_id='d' AND instance_index=0;
                UPDATE deployment_instances SET revision=1,generation=5,desired_state='ready',observed_state='ready',admission_enabled=1,dispatch_enabled=1 WHERE deployment_id='d' AND instance_index=1;"#,
            )
            .unwrap();
    }

    fn instance_admission(s: &Store) -> Vec<(i64, i64)> {
        let mut stmt = s
            .conn
            .prepare("SELECT instance_index,admission_enabled FROM deployment_instances WHERE deployment_id='d' ORDER BY instance_index")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    /// ADR 0011 decision 4, ADR 0013 §6: one instance that gave up closes only
    /// its own admission; its sibling and the deployment keep being admitted.
    // T16 T29
    #[test]
    fn one_instance_failure_closes_only_that_instance() {
        let s = Store::open_in_memory().unwrap();
        two_admitted_instances(&s);
        assert!(s.close_instance_admission_at("d", 1, 5).unwrap());
        assert_eq!(instance_admission(&s), vec![(0, 1), (1, 0)]);
        let deployment: i64 = s
            .conn
            .query_row(
                "SELECT admission_enabled FROM deployments WHERE id='d'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            deployment, 1,
            "the deployment keeps admitting through instance 0"
        );
    }

    /// T18: a failure reported for an incarnation a later activation or a stop
    /// already replaced is stale. It closes nothing, and never falls back to
    /// closing every instance of the deployment.
    // T16 T18
    #[test]
    fn a_stale_instance_failure_closes_nothing() {
        let s = Store::open_in_memory().unwrap();
        two_admitted_instances(&s);
        // Generation 4 is instance 0's, not instance 1's.
        assert!(!s.close_instance_admission_at("d", 1, 4).unwrap());
        assert!(!s.close_instance_admission_at("d", 1, 3).unwrap());
        assert_eq!(instance_admission(&s), vec![(0, 1), (1, 1)]);
        // The generation-only form has no deployment-wide fallback either.
        s.close_instance_admission("d", 3).unwrap();
        assert_eq!(instance_admission(&s), vec![(0, 1), (1, 1)]);
        s.close_instance_admission("d", 5).unwrap();
        assert_eq!(instance_admission(&s), vec![(0, 1), (1, 0)]);
    }

    /// The deployment-level switch writes the instances and the deployment row
    /// together or not at all.
    // T16
    #[test]
    fn deployment_admission_switch_is_atomic() {
        let s = Store::open_in_memory().unwrap();
        two_admitted_instances(&s);
        s.conn
            .execute_batch(
                "CREATE TEMP TRIGGER refuse_deployment_write BEFORE UPDATE OF updated_at ON deployments
                 BEGIN SELECT RAISE(ABORT,'refused'); END;",
            )
            .unwrap();
        assert!(s.set_admission_enabled("d", false).is_err());
        assert_eq!(instance_admission(&s), vec![(0, 1), (1, 1)]);
        s.conn
            .execute_batch("DROP TRIGGER refuse_deployment_write")
            .unwrap();
        s.set_admission_enabled("d", false).unwrap();
        assert_eq!(instance_admission(&s), vec![(0, 0), (1, 0)]);
    }
}

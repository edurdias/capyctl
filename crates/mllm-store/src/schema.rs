//! The F0 durable-store schema (design §4), verbatim.
//!
//! Tables not yet exercised by acceptance are created now so the v1
//! migration is the single, forward-only baseline.

/// v1 DDL: deployments, operations, generation_history, owners,
/// reservations, domains, hosts, journal_entries, schema_migrations.
pub const SCHEMA_V1: &str = r#"
CREATE TABLE deployments(
    id TEXT PRIMARY KEY,
    name TEXT UNIQUE NOT NULL,
    kind TEXT NOT NULL,
    route_model_id TEXT,
    desired_state TEXT NOT NULL,
    admission_enabled INTEGER NOT NULL,
    suspended INTEGER NOT NULL,
    current_generation INTEGER NOT NULL,
    schema_version INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

-- revision preconditions are intentionally absent until the
-- revision-aware update design exists (SPEC §19 defers it)
CREATE TABLE operations(
    id TEXT PRIMARY KEY,
    deployment_id TEXT REFERENCES deployments,
    kind TEXT NOT NULL,
    state TEXT NOT NULL,
    error_code TEXT,
    idempotency_key TEXT UNIQUE,
    accepted_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE generation_history(
    deployment_id TEXT NOT NULL REFERENCES deployments,
    generation INTEGER NOT NULL,
    started_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    ended_at TEXT,
    outcome TEXT
);

CREATE TABLE owners(
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    deployment_id TEXT NULL REFERENCES deployments
);

CREATE TABLE reservations(
    owner_id TEXT NOT NULL REFERENCES owners,
    domain_id TEXT,
    bytes INTEGER NOT NULL,
    phase TEXT NOT NULL,
    exclusive_devices TEXT -- JSON array
);

CREATE TABLE domains(
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL, -- system | device_memory | filesystem | remote_storage
    observed_bytes INTEGER NULL,
    observed_at TEXT
);

CREATE TABLE hosts(
    id TEXT PRIMARY KEY,
    name TEXT,
    state TEXT
); -- inventory skeleton; F3 enrolls

CREATE TABLE journal_entries(
    id TEXT PRIMARY KEY,
    host_id TEXT,
    operation_id TEXT,
    state TEXT,
    evidence TEXT,
    recorded_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
); -- no inference bodies, ever

CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY);
"#;

/// v2: the observed half of the lifecycle state. v1 shipped before any
/// writer needed it; the controller operation engine records it (T12).
pub const SCHEMA_V2: &str =
    "ALTER TABLE deployments ADD COLUMN observed_state TEXT NOT NULL DEFAULT 'stopped';";

pub const SCHEMA_V3: &str = r#"
ALTER TABLE deployments ADD COLUMN revision INTEGER NOT NULL DEFAULT 1 CHECK(revision >= 1);
CREATE TABLE resource_ledger_meta(
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    epoch INTEGER NOT NULL CHECK(epoch >= 0)
);
INSERT INTO resource_ledger_meta(singleton, epoch) VALUES (1, 0);
CREATE TABLE resource_owners(
    owner_id TEXT PRIMARY KEY REFERENCES deployments(id),
    footprint_json TEXT NOT NULL
);
CREATE TABLE resource_grants(
    id TEXT PRIMARY KEY,
    deployment_id TEXT NOT NULL REFERENCES deployments(id),
    operation_id TEXT NOT NULL REFERENCES operations(id),
    request_json TEXT NOT NULL,
    committed_epoch INTEGER NOT NULL UNIQUE CHECK(committed_epoch > 0)
);
"#;

pub const SCHEMA_V4: &str = r#"
ALTER TABLE deployments ADD COLUMN dispatch_enabled INTEGER NOT NULL DEFAULT 0 CHECK(dispatch_enabled IN (0,1));
CREATE TABLE coordinator_session(
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    epoch INTEGER NOT NULL CHECK(epoch>=0),
    session_id TEXT NOT NULL
);
INSERT INTO coordinator_session(singleton,epoch,session_id) VALUES (1,0,'');
CREATE TABLE request_leases(
    id TEXT PRIMARY KEY,
    deployment_id TEXT NOT NULL REFERENCES deployments(id),
    revision INTEGER NOT NULL CHECK(revision>=1),
    generation INTEGER NOT NULL CHECK(generation>=1),
    session_id TEXT NOT NULL,
    disposition TEXT NOT NULL CHECK(disposition IN ('inflight','uncertain'))
);
CREATE INDEX request_leases_deployment ON request_leases(deployment_id);
"#;

pub const SCHEMA_V5: &str = r#"
CREATE TABLE runtime_bindings(
  id TEXT PRIMARY KEY,
  deployment_id TEXT NOT NULL REFERENCES deployments(id),
  revision INTEGER NOT NULL CHECK(revision>0),
  incarnation TEXT NOT NULL UNIQUE,
  ownership TEXT NOT NULL CHECK(ownership IN ('managed','attached')),
  binding_json TEXT NOT NULL,
  identities_json TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('reserved','live','uncertain','released'))
);
CREATE UNIQUE INDEX one_retained_binding ON runtime_bindings(deployment_id)
  WHERE state!='released';
CREATE TABLE endpoint_leases(
  host TEXT NOT NULL,
  port INTEGER NOT NULL CHECK(port BETWEEN 1 AND 65535),
  binding_id TEXT NOT NULL REFERENCES runtime_bindings(id),
  PRIMARY KEY(host,port)
);
CREATE TABLE lifecycle_runs(
  operation_id TEXT PRIMARY KEY REFERENCES operations(id),
  deployment_id TEXT NOT NULL REFERENCES deployments(id),
  revision INTEGER NOT NULL CHECK(revision>0),
  generation INTEGER NOT NULL CHECK(generation>0),
  session_id TEXT NOT NULL,
  action TEXT NOT NULL CHECK(action IN ('activate','park','stop','prepare','reconcile')),
  state TEXT NOT NULL CHECK(state IN ('queued','running','uncertain','succeeded','failed')),
  deadline_ms INTEGER NOT NULL,
  plan_json TEXT NOT NULL
);
CREATE UNIQUE INDEX one_activation ON lifecycle_runs(deployment_id,revision,generation)
  WHERE action='activate' AND state IN ('queued','running','uncertain');
CREATE TABLE lifecycle_claims(
  deployment_id TEXT PRIMARY KEY REFERENCES deployments(id),
  operation_id TEXT NOT NULL REFERENCES lifecycle_runs(operation_id),
  revision INTEGER NOT NULL CHECK(revision>0),
  generation INTEGER NOT NULL CHECK(generation>0)
);
CREATE TABLE lifecycle_steps(
  id TEXT PRIMARY KEY,
  operation_id TEXT NOT NULL REFERENCES lifecycle_runs(operation_id),
  ordinal INTEGER NOT NULL CHECK(ordinal>=0),
  deployment_id TEXT NOT NULL REFERENCES deployments(id),
  binding_id TEXT NOT NULL REFERENCES runtime_bindings(id),
  session_id TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('planned','armed','uncertain','completed','cancelled')),
  step_json TEXT NOT NULL,
  grant_id TEXT UNIQUE REFERENCES resource_grants(id),
  UNIQUE(operation_id,ordinal)
);
CREATE TABLE lifecycle_evidence(
  step_id TEXT PRIMARY KEY REFERENCES lifecycle_steps(id),
  evidence_json TEXT NOT NULL,
  committed_epoch INTEGER NOT NULL CHECK(committed_epoch>=0)
);
"#;

pub const SCHEMA_V6: &str = r#"
CREATE TABLE deployment_routes(
  route TEXT PRIMARY KEY,
  deployment_id TEXT NOT NULL REFERENCES deployments(id)
);
CREATE TABLE command_receipts(
  principal_id TEXT NOT NULL,
  command_scope TEXT NOT NULL,
  idempotency_key TEXT NOT NULL,
  request_hash TEXT NOT NULL,
  operation_id TEXT NOT NULL REFERENCES operations(id),
  response_json TEXT NOT NULL,
  PRIMARY KEY(principal_id,command_scope,idempotency_key)
);
CREATE TABLE effective_revisions(
  deployment_id TEXT NOT NULL REFERENCES deployments(id),
  revision INTEGER NOT NULL CHECK(revision>0),
  effective_json TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  PRIMARY KEY(deployment_id,revision)
);
CREATE TABLE host_resource_policies(
  host_id TEXT PRIMARY KEY,
  revision INTEGER NOT NULL CHECK(revision>0),
  policy_json TEXT NOT NULL
);
CREATE TABLE host_qualification_policies(
  host_id TEXT PRIMARY KEY,
  revision INTEGER NOT NULL CHECK(revision>0),
  policy_json TEXT NOT NULL
);
CREATE TABLE qualification_runs(
  id TEXT PRIMARY KEY,
  host_id TEXT NOT NULL REFERENCES host_qualification_policies(host_id),
  deployment_id TEXT NOT NULL REFERENCES deployments(id),
  revision INTEGER NOT NULL CHECK(revision>0),
  binding_id TEXT NOT NULL REFERENCES runtime_bindings(id),
  incarnation TEXT NOT NULL,
  operation_id TEXT NOT NULL REFERENCES operations(id),
  principal_id TEXT NOT NULL,
  recipe_digest TEXT NOT NULL,
  authorization_json TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN
    ('accepted','running','passed','failed','uncertain','aborted','expired')),
  deadline_ms INTEGER NOT NULL,
  requests_used INTEGER NOT NULL DEFAULT 0 CHECK(requests_used>=0),
  cleanup_state TEXT NOT NULL DEFAULT 'retained'
    CHECK(cleanup_state IN ('retained','verified_gone')),
  cleanup_step_id TEXT REFERENCES lifecycle_steps(id),
  CHECK((cleanup_state='retained' AND cleanup_step_id IS NULL) OR
        (cleanup_state='verified_gone' AND cleanup_step_id IS NOT NULL))
);
CREATE TABLE qualifications(
  id TEXT PRIMARY KEY,
  source_run_id TEXT NOT NULL UNIQUE REFERENCES qualification_runs(id),
  recipe_fingerprint TEXT NOT NULL,
  record_json TEXT NOT NULL
);
CREATE INDEX qualifications_recipe ON qualifications(recipe_fingerprint);
CREATE TABLE qualification_evidence_refs(
  id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL REFERENCES qualification_runs(id),
  case_id TEXT NOT NULL,
  evidence_digest TEXT NOT NULL,
  metadata_json TEXT NOT NULL,
  UNIQUE(run_id,case_id,evidence_digest)
);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn schema_v1_applies_cleanly() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        let tables: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        for expected in [
            "deployments",
            "domains",
            "generation_history",
            "hosts",
            "journal_entries",
            "operations",
            "owners",
            "reservations",
            "schema_migrations",
        ] {
            assert!(
                tables.iter().any(|t| t == expected),
                "missing table {expected}"
            );
        }
    }

    #[test]
    fn operations_has_no_revision_precondition_column() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        let cols: Vec<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(operations)").unwrap();
            stmt.query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert!(!cols.iter().any(|c| c.contains("revision")));
    }
}

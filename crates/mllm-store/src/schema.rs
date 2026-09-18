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

pub const SCHEMA_V10: &str = r#"
CREATE TABLE qualification_ready_probes(
  run_id TEXT NOT NULL REFERENCES qualification_runs(id),
  case_id TEXT NOT NULL,
  parent_operation_id TEXT NOT NULL REFERENCES lifecycle_runs(operation_id),
  parent_step_id TEXT NOT NULL UNIQUE REFERENCES lifecycle_steps(id),
  probe_step_id TEXT NOT NULL UNIQUE REFERENCES lifecycle_steps(id),
  linkage_json TEXT NOT NULL CHECK(length(CAST(linkage_json AS BLOB)) <= 1048576),
  PRIMARY KEY(run_id,case_id),
  CHECK(parent_step_id != probe_step_id)
);
CREATE TABLE qualification_request_attempts(
  run_id TEXT NOT NULL REFERENCES qualification_runs(id),
  case_id TEXT NOT NULL,
  item_ordinal INTEGER NOT NULL CHECK(item_ordinal BETWEEN 0 AND 4095),
  subcheck_id TEXT NOT NULL,
  request_operation_id TEXT NOT NULL UNIQUE REFERENCES operations(id),
  lease_id TEXT NOT NULL UNIQUE,
  principal_id TEXT NOT NULL,
  command_scope TEXT NOT NULL,
  idempotency_key TEXT NOT NULL,
  parent_operation_id TEXT REFERENCES operations(id),
  child_step_id TEXT UNIQUE REFERENCES lifecycle_steps(id),
  receipt_json TEXT NOT NULL CHECK(length(CAST(receipt_json AS BLOB)) <= 1048576),
  PRIMARY KEY(run_id,case_id,item_ordinal,subcheck_id),
  FOREIGN KEY(principal_id,command_scope,idempotency_key) REFERENCES command_receipts(principal_id,command_scope,idempotency_key),
  CHECK((parent_operation_id IS NULL) = (child_step_id IS NULL)),
  CHECK(request_operation_id != parent_operation_id)
);
CREATE TABLE qualification_request_results(
  request_operation_id TEXT PRIMARY KEY REFERENCES qualification_request_attempts(request_operation_id),
  evidence_json TEXT NOT NULL CHECK(length(CAST(evidence_json AS BLOB)) <= 1048576),
  committed_epoch INTEGER NOT NULL
);
CREATE TABLE qualification_parked_status(
  parent_step_id TEXT PRIMARY KEY REFERENCES lifecycle_steps(id),
  evidence_json TEXT NOT NULL CHECK(length(CAST(evidence_json AS BLOB)) <= 1048576),
  committed_epoch INTEGER NOT NULL
);
"#;

/// SPEC §6.3 requires an administrative stop to suspend automatic activation, and
/// requires an automatic idle stop not to. `suspended` cannot carry that intent: its
/// nine readers all use it to mean "eligible to proceed", so writing it on a stop
/// breaks completion, replay and expiry for the very operation that wrote it. This
/// column carries the intent alone, and nothing else reads it.
pub const SCHEMA_V11: &str =
    "ALTER TABLE deployments ADD COLUMN admin_stopped INTEGER NOT NULL DEFAULT 0;";

/// ADR 0011 decision 5: a deployment's failed attempts are counted against the exact
/// configuration that failed. A new revision is a new configuration and starts fresh,
/// so the key is the fence rather than the deployment alone.
pub const SCHEMA_V12: &str = r#"
CREATE TABLE deployment_attempts(
  deployment_id TEXT NOT NULL REFERENCES deployments(id),
  revision INTEGER NOT NULL CHECK(revision > 0),
  generation INTEGER NOT NULL CHECK(generation > 0),
  attempts INTEGER NOT NULL CHECK(attempts >= 0),
  last_attempt_ms INTEGER NOT NULL CHECK(last_attempt_ms >= 0),
  PRIMARY KEY(deployment_id, revision, generation)
);
"#;

/// ADR 0011: qualification is not an mllm concept. The tables that held candidate
/// runs, their catalog, evidence, probes, budgets and parked-status records are
/// dropped in foreign-key order. Nothing wrote them outside tests; a v12 store from
/// this branch has no rows in them. State directories older than 2026-09-16 must
/// already be deleted for the resource-policy shape, so no data path is preserved.
///
/// Stored kind strings of the ordinary path were renamed in the same change without
/// a data migration; a v12 state directory that holds lifecycle history is recreated,
/// as the design keeps no compatibility.
pub const SCHEMA_V13: &str = r#"
DROP TABLE qualification_evidence_refs;
DROP TABLE qualification_ready_probes;
DROP TABLE qualification_request_results;
DROP TABLE qualification_request_attempts;
DROP TABLE qualification_case_actions;
DROP TABLE qualification_parked_status;
DROP TABLE candidate_cleanup_actions;
DROP TABLE qualifications;
DROP TABLE qualification_runs;
DROP TABLE host_qualification_policies;
"#;

// Spec §3: the per-launch engine key, encrypted at rest with XChaCha20-Poly1305 under
// the identity key file, with binding id and incarnation as associated data so a row
// copied between bindings does not authenticate. Deleted when the binding releases.
pub const SCHEMA_V14: &str = r#"
CREATE TABLE engine_secrets(
  binding_id TEXT PRIMARY KEY REFERENCES runtime_bindings(id),
  incarnation TEXT NOT NULL,
  nonce BLOB NOT NULL CHECK(length(nonce)=24),
  ciphertext BLOB NOT NULL
);
"#;

pub const SCHEMA_V9: &str = r#"
CREATE TABLE owned_launch_associations(
  step_id TEXT PRIMARY KEY REFERENCES lifecycle_steps(id),
  binding_id TEXT NOT NULL UNIQUE REFERENCES runtime_bindings(id),
  incarnation TEXT NOT NULL UNIQUE,
  association_json TEXT NOT NULL CHECK(length(CAST(association_json AS BLOB)) <= 1048576)
);
CREATE TABLE candidate_cleanup_actions(
  operation_id TEXT PRIMARY KEY REFERENCES lifecycle_runs(operation_id),
  run_id TEXT NOT NULL REFERENCES qualification_runs(id),
  step_id TEXT NOT NULL UNIQUE REFERENCES lifecycle_steps(id),
  predecessor_cleanup_operation_id TEXT UNIQUE REFERENCES candidate_cleanup_actions(operation_id),
  CHECK(predecessor_cleanup_operation_id IS NULL OR predecessor_cleanup_operation_id != operation_id)
);
CREATE UNIQUE INDEX one_initial_candidate_cleanup ON candidate_cleanup_actions(run_id)
  WHERE predecessor_cleanup_operation_id IS NULL;
"#;

pub const SCHEMA_V8: &str = r#"
CREATE TABLE qualification_case_actions(
  run_id TEXT NOT NULL REFERENCES qualification_runs(id),
  case_id TEXT NOT NULL,
  operation_id TEXT NOT NULL UNIQUE REFERENCES operations(id),
  step_id TEXT NOT NULL UNIQUE REFERENCES lifecycle_steps(id),
  PRIMARY KEY(run_id,case_id)
);
"#;

pub const SCHEMA_V7: &str = r#"
CREATE TABLE event_meta(
  singleton INTEGER PRIMARY KEY CHECK(singleton=1),
  incarnation TEXT NOT NULL,
  retained_after INTEGER NOT NULL CHECK(retained_after>=0)
);
CREATE TABLE management_events(
  sequence INTEGER PRIMARY KEY AUTOINCREMENT,
  recorded_at_ms INTEGER NOT NULL,
  kind TEXT NOT NULL,
  deployment_id TEXT,
  operation_id TEXT,
  payload_json TEXT NOT NULL
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

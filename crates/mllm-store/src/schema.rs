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

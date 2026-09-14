//! Forward-only migrations, versioned in `schema_migrations`.

use rusqlite::{Connection, OptionalExtension};

use crate::schema::{
    SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6, SCHEMA_V7, SCHEMA_V8,
    SCHEMA_V9, SCHEMA_V10,
};

/// One entry per version; `MIGRATIONS[0]` is version 1.
pub const MIGRATIONS: &[&str] = &[
    SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6, SCHEMA_V7, SCHEMA_V8,
    SCHEMA_V9, SCHEMA_V10,
];

/// Applies every migration newer than the recorded schema version.
/// Each migration runs in its own transaction together with its
/// version stamp, so a failed apply leaves the store untouched.
/// The `schema_migrations` table itself is created by v1, so on a
/// fresh (unmigrated) store its absence means version 0.
pub fn apply(conn: &Connection) -> Result<(), rusqlite::Error> {
    let migrations_table: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations'",
            [],
            |_| Ok(true),
        )
        .optional()?
        .is_some();
    let current: i64 = if migrations_table {
        conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )?
    } else {
        0
    };
    for (index, sql) in MIGRATIONS.iter().enumerate() {
        let version = (index + 1) as i64;
        if version <= current {
            continue;
        }
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        if version == 7 {
            tx.execute(
                "INSERT INTO event_meta(singleton,incarnation,retained_after) VALUES(1,?1,0)",
                [ulid::Ulid::new().to_string()],
            )?;
        }
        tx.execute(
            "INSERT INTO schema_migrations(version) VALUES (?1)",
            [version],
        )?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v9_preserves_history_and_enforces_association_foreign_keys() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        for (index, sql) in MIGRATIONS.iter().take(8).enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations(version) VALUES(?1)",
                [(index + 1) as i64],
            )
            .unwrap();
        }
        conn.execute_batch("INSERT INTO event_meta VALUES(1,'known-incarnation',12);
            INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version,revision) VALUES('retained','retained','model','stopped',0,0,7,1,3);
            UPDATE resource_ledger_meta SET epoch=19;").unwrap();
        apply(&conn).unwrap();
        apply(&conn).unwrap();
        let tables:i64=conn.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('owned_launch_associations','candidate_cleanup_actions')",[],|r|r.get(0)).unwrap();
        assert_eq!(tables, 2);
        let state:(i64,i64,i64,String,i64)=conn.query_row("SELECT revision,current_generation,(SELECT epoch FROM resource_ledger_meta),(SELECT incarnation FROM event_meta),(SELECT retained_after FROM event_meta) FROM deployments WHERE id='retained'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
        assert_eq!(state, (3, 7, 19, "known-incarnation".into(), 12));
        let stamps: Vec<i64> = conn
            .prepare("SELECT version FROM schema_migrations ORDER BY version")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(stamps, (1..=10).collect::<Vec<_>>());
        assert!(conn
            .execute(
                "INSERT INTO owned_launch_associations VALUES('missing','missing','missing','{}')",
                []
            )
            .is_err());
    }

    #[test]
    fn v8_preserves_existing_database_and_adds_permanent_case_keys() {
        let conn = Connection::open_in_memory().unwrap();
        for (index, sql) in MIGRATIONS.iter().take(7).enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations(version) VALUES(?1)",
                [(index + 1) as i64],
            )
            .unwrap();
        }
        conn.execute_batch("INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('retained','retained','model','stopped',0,0,1,1); UPDATE resource_ledger_meta SET epoch=7;").unwrap();
        apply(&conn).unwrap();
        apply(&conn).unwrap();
        let retained:(String,i64)=conn.query_row("SELECT name,(SELECT epoch FROM resource_ledger_meta) FROM deployments WHERE id='retained'",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(retained, ("retained".into(), 7));
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM qualification_case_actions", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
        let foreign_keys:i64=conn.query_row("SELECT COUNT(*) FROM pragma_foreign_key_list('qualification_case_actions') WHERE on_delete='NO ACTION'",[],|r|r.get(0)).unwrap();
        assert_eq!(foreign_keys, 3);
    }

    #[test]
    fn migrations_apply_once_and_are_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        apply(&conn).unwrap();
        apply(&conn).unwrap();
        let (max, count): (i64, i64) = conn
            .query_row(
                "SELECT MAX(version), COUNT(*) FROM schema_migrations",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(max, MIGRATIONS.len() as i64);
        assert_eq!(count, MIGRATIONS.len() as i64);
    }

    #[test]
    fn v3_upgrade_preserves_legacy_rows_and_sets_revision() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::schema::SCHEMA_V1).unwrap();
        conn.execute_batch(crate::schema::SCHEMA_V2).unwrap();
        conn.execute_batch(
            "INSERT INTO schema_migrations VALUES (1), (2);
            INSERT INTO deployments(id, name, kind, desired_state, admission_enabled,
              suspended, current_generation, schema_version)
            VALUES ('a', 'a', 'model', 'stopped', 1, 0, 1, 1);
            INSERT INTO owners(id, kind, deployment_id) VALUES ('a', 'model', 'a');
            INSERT INTO reservations(owner_id, domain_id, bytes, phase)
            VALUES ('a', 'system', 64, 'activation');",
        )
        .unwrap();
        apply(&conn).unwrap();
        apply(&conn).unwrap();
        let revision: i64 = conn
            .query_row("SELECT revision FROM deployments WHERE id='a'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let bytes: i64 = conn
            .query_row(
                "SELECT bytes FROM reservations WHERE owner_id='a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let epoch: i64 = conn
            .query_row(
                "SELECT epoch FROM resource_ledger_meta WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!((revision, bytes, epoch), (1, 64, 0));
    }

    #[test]
    fn v4_dispatch_schema() {
        let conn = Connection::open_in_memory().unwrap();
        apply(&conn).unwrap();
        apply(&conn).unwrap();
        let state: (i64, String) = conn
            .query_row(
                "SELECT epoch, session_id FROM coordinator_session WHERE singleton=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, (0, String::new()));
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM request_leases", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn v5_lifecycle_schema() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        apply(&conn).unwrap();
        apply(&conn).unwrap();
        for table in [
            "runtime_bindings",
            "endpoint_leases",
            "lifecycle_runs",
            "lifecycle_claims",
            "lifecycle_steps",
            "lifecycle_evidence",
        ] {
            let count: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "{table}");
        }
    }

    #[test]
    fn v6_management_schema() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        apply(&conn).unwrap();
        apply(&conn).unwrap();
        for table in [
            "deployment_routes",
            "command_receipts",
            "effective_revisions",
            "host_resource_policies",
            "host_qualification_policies",
            "qualification_runs",
            "qualifications",
            "qualification_evidence_refs",
        ] {
            let count: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "{table}");
        }
    }

    #[test]
    fn v7_event_schema_initializes_one_stable_incarnation() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        apply(&conn).unwrap();
        let first: (i64, String, i64) = conn
            .query_row(
                "SELECT COUNT(*),incarnation,retained_after FROM event_meta",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        apply(&conn).unwrap();
        let second: (i64, String, i64) = conn
            .query_row(
                "SELECT COUNT(*),incarnation,retained_after FROM event_meta",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(first, second);
        assert_eq!((first.0, first.2), (1, 0));
        assert_eq!(first.1.len(), 26);
    }
}

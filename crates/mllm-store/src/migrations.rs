//! Forward-only migrations, versioned in `schema_migrations`.

use rusqlite::{Connection, OptionalExtension};

use crate::schema::{
    SCHEMA_V1, SCHEMA_V10, SCHEMA_V11, SCHEMA_V12, SCHEMA_V13, SCHEMA_V14, SCHEMA_V15, SCHEMA_V16, SCHEMA_V17, SCHEMA_V18, SCHEMA_V19, SCHEMA_V2, SCHEMA_V20, SCHEMA_V21, SCHEMA_V22, SCHEMA_V23, SCHEMA_V24, SCHEMA_V25, SCHEMA_V26, SCHEMA_V27, SCHEMA_V28, SCHEMA_V29, SCHEMA_V30, SCHEMA_V31,
    SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6, SCHEMA_V7, SCHEMA_V8, SCHEMA_V9,
};

/// One entry per version; `MIGRATIONS[0]` is version 1.
pub const MIGRATIONS: &[&str] = &[
    SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6, SCHEMA_V7, SCHEMA_V8,
    SCHEMA_V9, SCHEMA_V10, SCHEMA_V11, SCHEMA_V12, SCHEMA_V13, SCHEMA_V14, SCHEMA_V15, SCHEMA_V16, SCHEMA_V17, SCHEMA_V18, SCHEMA_V19,
    // ADR 0014 §7 (WE3): recorded checkpoint digests.
    SCHEMA_V20,
    // SPEC §3: endpoint port leases keyed per host.
    SCHEMA_V21,
    // ADR 0013 §5: deployment instances.
    SCHEMA_V22,
    // ADR 0013 §4, §6, §7 (I2): the instance is the unit of runtime.
    SCHEMA_V23,
    // SPEC §4.3, owner decision 4: durable host drain markers.
    SCHEMA_V24,
    // SPEC §§3.1, 7.3: hosts advertising per-launch journal claims.
    SCHEMA_V25,
    // ADR 0013 §4 (P1): hosts advertising per-instance fencing.
    SCHEMA_V26,
    // Owner decision 2026-09-23: measured startup peaks.
    SCHEMA_V27,
    // W10 gaps: dispatch closure reasons, switches in progress, warm residency.
    SCHEMA_V28,
    // Owner decision 2026-09-23: checkpoint weights sized before the digest.
    SCHEMA_V29,
    // SPEC §4.3 (router review item 14): drain intents written before any Stop.
    SCHEMA_V30,
    // SPEC §4.3: drain intent deadlines, so an abandoned intent expires.
    SCHEMA_V31,
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
        if version == 16 {
            crate::resource_namespace::migrate(&tx)?;
        }
        if version == 19 {
            // Owner decision 2026-09-22: carry pre-E1 state into the ADR 0014 shape.
            crate::ordinary_lifecycle::legacy_engine_config::migrate(&tx)?;
        }
        if version == 22 {
            // ADR 0013 §5: every existing deployment becomes instance 0.
            crate::instances::migrate(&tx)?;
        }
        if version == 23 {
            // ADR 0013 (I2): instance 0 takes its deployment's runtime state.
            crate::instances::migrate_v23(&tx)?;
        }
        if version == 28 {
            // SPEC §6.5 (ADR 0013 amendment): the warm-residency flag.
            crate::switch_state::migrate(&tx)?;
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
        let tables:i64=conn.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='owned_launch_associations'",[],|r|r.get(0)).unwrap();
        assert_eq!(tables, 1);
        let state:(i64,i64,i64,String,i64)=conn.query_row("SELECT revision,current_generation,(SELECT epoch FROM resource_ledger_meta),(SELECT incarnation FROM event_meta),(SELECT retained_after FROM event_meta) FROM deployments WHERE id='retained'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
        assert_eq!(state, (3, 7, 19, "known-incarnation".into(), 12));
        let stamps: Vec<i64> = conn
            .prepare("SELECT version FROM schema_migrations ORDER BY version")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            stamps,
            (1..=MIGRATIONS.len() as i64).collect::<Vec<_>>(),
            "every migration is stamped once"
        );
        assert!(conn
            .execute(
                "INSERT INTO owned_launch_associations VALUES('missing','missing','missing','{}')",
                []
            )
            .is_err());
    }

    #[test]
    fn v8_preserves_existing_database() {
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
    }

    /// An existing deployment keeps its rows and is not administratively stopped by
    /// the upgrade. A migration that defaulted the flag the other way would suspend
    /// automatic activation for every deployment already running.
    #[test]
    fn v11_preserves_rows_and_leaves_them_activatable() {
        let conn = Connection::open_in_memory().unwrap();
        for (index, sql) in MIGRATIONS.iter().take(10).enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations(version) VALUES(?1)",
                [(index + 1) as i64],
            )
            .unwrap();
        }
        conn.execute_batch("INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('kept','kept','model','ready',1,0,1,1);").unwrap();
        apply(&conn).unwrap();
        apply(&conn).unwrap();
        let (name, stopped): (String, bool) = conn
            .query_row(
                "SELECT name,admin_stopped FROM deployments WHERE id='kept'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(name, "kept");
        assert!(!stopped, "an upgrade must not suspend what was running");
    }

    /// An existing deployment keeps its rows and starts with no attempts recorded.
    #[test]
    fn v12_adds_attempts_and_preserves_rows() {
        let conn = Connection::open_in_memory().unwrap();
        for (index, sql) in MIGRATIONS.iter().take(11).enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations(version) VALUES(?1)",
                [(index + 1) as i64],
            )
            .unwrap();
        }
        conn.execute_batch("INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('kept','kept','model','ready',1,0,1,1);").unwrap();
        apply(&conn).unwrap();
        apply(&conn).unwrap();
        let name: String = conn
            .query_row("SELECT name FROM deployments WHERE id='kept'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(name, "kept");
        let attempts: i64 = conn
            .query_row("SELECT COUNT(*) FROM deployment_attempts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            attempts, 0,
            "an upgrade records no attempts against anything"
        );
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

    /// SPEC §3 (v21): endpoint leases are keyed per host. Existing leases keep
    /// their binding, address and port and take their binding's placed host;
    /// afterwards two hosts may lease the same port, one host may not twice.
    // T24 T29
    #[test]
    fn v21_keys_endpoint_leases_per_host_and_carries_rows_across() {
        let conn = Connection::open_in_memory().unwrap();
        for (index, sql) in MIGRATIONS.iter().take(20).enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations(version) VALUES(?1)",
                [(index + 1) as i64],
            )
            .unwrap();
        }
        conn.execute_batch(
            r#"INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version,revision) VALUES('a','a','model','ready',1,0,1,1,1),('b','b','model','ready',1,0,1,1,1),('f1','f1','model','ready',1,0,1,1,1);
            INSERT INTO effective_revisions VALUES('a',1,'{"host":{"name":"host-a"}}','fa'),('b',1,'{"host":{"name":"host-b"}}','fb');
            INSERT INTO runtime_bindings(id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state) VALUES('binding-a','a',1,'ia','managed','{}','[]','live'),('binding-b','b',1,'ib','managed','{}','[]','live'),('binding-f1','f1',1,'if1','managed','{}','[]','live');
            INSERT INTO endpoint_leases VALUES('127.0.0.1',20000,'binding-a'),('127.0.0.1',20001,'binding-b'),('127.0.0.1',20002,'binding-f1');"#,
        )
        .unwrap();
        apply(&conn).unwrap();
        apply(&conn).unwrap();
        let rows: Vec<(String, String, i64, String)> = conn
            .prepare("SELECT host_id,host,port,binding_id FROM endpoint_leases ORDER BY binding_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                ("host-a".into(), "127.0.0.1".into(), 20000, "binding-a".into()),
                ("host-b".into(), "127.0.0.1".into(), 20001, "binding-b".into()),
                ("".into(), "127.0.0.1".into(), 20002, "binding-f1".into()),
            ]
        );
        // Another host's range is its own: host-b may lease host-a's port.
        conn.execute(
            "INSERT INTO runtime_bindings(id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state) VALUES('binding-b2','b',1,'ib2','managed','{}','[]','released')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO endpoint_leases(host_id,host,port,binding_id) VALUES('host-b','127.0.0.1',20000,'binding-b2')",
            [],
        )
        .unwrap();
        assert!(conn
            .execute(
                "INSERT INTO endpoint_leases(host_id,host,port,binding_id) VALUES('host-a','127.0.0.1',20000,'binding-b2')",
                [],
            )
            .is_err());
    }

    /// ADR 0013 §4 (v26): the host launch-claims record is rebuilt to accept
    /// `per_instance`; a v25 row is carried across unchanged, and any other
    /// mode is still refused.
    // T24 T33
    #[test]
    fn v26_widens_host_launch_claims_and_carries_rows_across() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        for (index, sql) in MIGRATIONS.iter().take(25).enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations(version) VALUES(?1)",
                [(index + 1) as i64],
            )
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO enrolled_hosts(host_id,host_name,key_digest,revoked) VALUES('a','a','k',0),('b','b','k',0);
             INSERT INTO host_launch_claims VALUES('a','per_launch',7);",
        )
        .unwrap();
        assert!(conn
            .execute("INSERT INTO host_launch_claims VALUES('b','per_instance',8)", [])
            .is_err());
        apply(&conn).unwrap();
        apply(&conn).unwrap();
        let rows: Vec<(String, String, i64)> = conn
            .prepare("SELECT host_id,mode,recorded_at_ms FROM host_launch_claims ORDER BY host_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows, vec![("a".into(), "per_launch".into(), 7)]);
        conn.execute("INSERT INTO host_launch_claims VALUES('b','per_instance',8)", [])
            .unwrap();
        assert!(conn
            .execute("UPDATE host_launch_claims SET mode='per_host' WHERE host_id='a'", [])
            .is_err());
        assert!(conn
            .execute("INSERT INTO host_launch_claims VALUES('missing','per_launch',9)", [])
            .is_err());
    }

    /// v30 (router review item 14): drain intents are added beside the v24
    /// markers; a v29 store's markers are carried across unchanged, and an
    /// intent needs no operation.
    // T10 T33
    #[test]
    fn v30_adds_drain_intents_and_keeps_v24_markers() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        for (index, sql) in MIGRATIONS.iter().take(29).enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations(version) VALUES(?1)",
                [(index + 1) as i64],
            )
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO operations(id,kind,state) VALUES('op','ordinary_cleanup','running');
             INSERT INTO host_drains VALUES('lab','op',7);",
        )
        .unwrap();
        apply(&conn).unwrap();
        apply(&conn).unwrap();
        let version: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        let marker: (String, String, i64) = conn
            .query_row("SELECT host_id,operation_id,recorded_at_ms FROM host_drains", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(marker, ("lab".into(), "op".into(), 7));
        conn.execute(
            "INSERT INTO host_drain_intents(host_id,drain_key,recorded_at_ms) VALUES('lab','k',8)",
            [],
        )
        .unwrap();
        assert!(conn
            .execute(
                "INSERT INTO host_drain_intents(host_id,drain_key,recorded_at_ms) VALUES('','k',8)",
                []
            )
            .is_err());
        assert!(conn
            .execute(
                "UPDATE host_drain_intents SET completed_at_ms=-1 WHERE host_id='lab'",
                []
            )
            .is_err());
    }

    /// ADR 0011: the qualification tables are dropped. A v12 store with rows in the
    /// surviving tables keeps them; none of the dropped tables remain.
    #[test]
    fn v13_drops_qualification_tables_and_preserves_rows() {
        let conn = Connection::open_in_memory().unwrap();
        for (index, sql) in MIGRATIONS.iter().take(12).enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations(version) VALUES(?1)",
                [(index + 1) as i64],
            )
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('kept','kept','model','ready',1,0,1,1);",
        )
        .unwrap();
        apply(&conn).unwrap();
        apply(&conn).unwrap();
        let name: String = conn
            .query_row("SELECT name FROM deployments WHERE id='kept'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(name, "kept");
        for table in [
            "qualification_evidence_refs",
            "qualification_ready_probes",
            "qualification_request_attempts",
            "qualification_request_results",
            "qualification_case_actions",
            "qualification_parked_status",
            "candidate_cleanup_actions",
            "qualifications",
            "qualification_runs",
            "host_qualification_policies",
        ] {
            let present: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(!present, "{table} must be dropped");
        }
        for table in [
            "owned_launch_associations",
            "request_leases",
            "deployment_attempts",
        ] {
            let present: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(present, "{table} must stay");
        }
    }

    /// Spec §3: an existing deployment and its binding keep their rows, and the new
    /// `engine_secrets` table exists for the encrypted-key path to use.
    // T39
    #[test]
    fn v14_adds_engine_secrets_and_preserves_rows() {
        let conn = Connection::open_in_memory().unwrap();
        for (index, sql) in MIGRATIONS.iter().take(13).enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations(version) VALUES(?1)",
                [(index + 1) as i64],
            )
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('kept','kept','model','ready',1,0,1,1);
            INSERT INTO runtime_bindings(id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state) VALUES('binding','kept',1,'incarnation','managed','{}','[]','reserved');",
        )
        .unwrap();
        apply(&conn).unwrap();
        apply(&conn).unwrap();
        let name: String = conn
            .query_row("SELECT name FROM deployments WHERE id='kept'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(name, "kept");
        let binding: String = conn
            .query_row(
                "SELECT id FROM runtime_bindings WHERE id='binding'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(binding, "binding");
        let present: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='engine_secrets')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(present, "engine_secrets must exist");
    }

    /// Spec §4.2 (ordinary launch design): a v14 store's engine secret was a
    /// vLLM inference key; the v15 rebuild carries it across under the
    /// `inference` role with its sealed bytes intact, and the table then
    /// accepts one row per role for a binding.
    // T39
    #[test]
    fn v15_roles_engine_secrets_and_carries_rows_across() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        for (index, sql) in MIGRATIONS.iter().take(14).enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations(version) VALUES(?1)",
                [(index + 1) as i64],
            )
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('kept','kept','model','ready',1,0,1,1);
            INSERT INTO runtime_bindings(id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state) VALUES('binding','kept',1,'incarnation','managed','{}','[]','reserved');
            INSERT INTO engine_secrets VALUES('binding','incarnation',zeroblob(24),x'00');",
        )
        .unwrap();
        apply(&conn).unwrap();
        apply(&conn).unwrap();
        let row: (String, String, i64) = conn
            .query_row(
                "SELECT role,incarnation,length(nonce) FROM engine_secrets WHERE binding_id='binding'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row, ("inference".into(), "incarnation".into(), 24));
        // One row per role: the second insert for the other role succeeds, and
        // the check constraint refuses a role outside the pair.
        conn.execute(
            "INSERT INTO engine_secrets VALUES('binding','admin','incarnation',zeroblob(24),x'01')",
            [],
        )
        .unwrap();
        assert!(conn
            .execute(
                "INSERT INTO engine_secrets VALUES('binding','root','incarnation',zeroblob(24),x'02')",
                []
            )
            .is_err());
        // The foreign key to runtime_bindings survives the rebuild.
        assert!(conn
            .execute(
                "INSERT INTO engine_secrets VALUES('missing','inference','i',zeroblob(24),x'03')",
                []
            )
            .is_err());
    }
}

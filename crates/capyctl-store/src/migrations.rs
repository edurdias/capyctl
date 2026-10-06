//! Forward-only migrations, versioned in `schema_migrations`.

use rusqlite::{Connection, OptionalExtension};

use crate::StoreError;

use crate::schema::{
    SCHEMA_V1, SCHEMA_V10, SCHEMA_V11, SCHEMA_V12, SCHEMA_V13, SCHEMA_V14, SCHEMA_V15, SCHEMA_V16,
    SCHEMA_V17, SCHEMA_V18, SCHEMA_V19, SCHEMA_V2, SCHEMA_V20, SCHEMA_V21, SCHEMA_V22, SCHEMA_V23,
    SCHEMA_V24, SCHEMA_V25, SCHEMA_V26, SCHEMA_V27, SCHEMA_V28, SCHEMA_V29, SCHEMA_V3, SCHEMA_V30,
    SCHEMA_V31, SCHEMA_V32, SCHEMA_V33, SCHEMA_V34, SCHEMA_V35, SCHEMA_V36, SCHEMA_V37, SCHEMA_V38,
    SCHEMA_V39, SCHEMA_V4, SCHEMA_V40, SCHEMA_V41, SCHEMA_V5, SCHEMA_V6, SCHEMA_V7, SCHEMA_V8,
    SCHEMA_V9,
};

/// One entry per version; `MIGRATIONS[0]` is version 1. Not formatted by
/// rustfmt, which moves a comment onto the previous entry's line.
#[rustfmt::skip]
pub const MIGRATIONS: &[&str] = &[
    SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6, SCHEMA_V7, SCHEMA_V8,
    SCHEMA_V9, SCHEMA_V10, SCHEMA_V11, SCHEMA_V12, SCHEMA_V13, SCHEMA_V14, SCHEMA_V15, SCHEMA_V16,
    SCHEMA_V17, SCHEMA_V18, SCHEMA_V19,
    // ADR 0014 §7 (WE3): recorded checkpoint digests.
    SCHEMA_V20, // SPEC §3: endpoint port leases keyed per host.
    SCHEMA_V21, // ADR 0013 §5: deployment instances.
    SCHEMA_V22,
    // ADR 0013 §4, §6, §7 (I2): the instance is the unit of runtime.
    SCHEMA_V23, // SPEC §4.3, owner decision 4: durable host drain markers.
    SCHEMA_V24, // SPEC §§3.1, 7.3: hosts advertising per-launch journal claims.
    SCHEMA_V25, // ADR 0013 §4 (P1): hosts advertising per-instance fencing.
    SCHEMA_V26, // Owner decision 2026-09-23: measured startup peaks.
    SCHEMA_V27,
    // W10 gaps: dispatch closure reasons, switches in progress, warm residency.
    SCHEMA_V28,
    // Owner decision 2026-09-23: checkpoint weights sized before the digest.
    SCHEMA_V29,
    // SPEC §4.3 (router review item 14): drain intents written before any Stop.
    SCHEMA_V30,
    // SPEC §4.3: drain intent deadlines, so an abandoned intent expires.
    SCHEMA_V31,
    // ADR 0016: host recovery invitations and per-certificate revocation.
    SCHEMA_V32,
    // ADR 0008: materialization state of declared remote model sources.
    SCHEMA_V33,
    // ADR 0017: each host's declared version, capabilities and skew verdict.
    SCHEMA_V34,
    // ADR 0018 §4: durable profile retirements and their stops.
    SCHEMA_V35,
    // ADR 0018 §4, §5: the profiles standalone's embedded host publishes.
    SCHEMA_V36,
    // ADR 0019: per-GPU resolutions and each instance's placed GPU.
    SCHEMA_V37,
    // SPEC §10 (2026-10-01): cancellations of hung-up requests.
    SCHEMA_V38,
    // ADR 0014 amendment A13: measured parked residue per revision.
    SCHEMA_V39,
    // ADR 0014 amendment A16: the hybrid state slot beside the weights.
    SCHEMA_V40,
    // ADR 0028 §5, §6: multi-node group plans and per-host digests.
    SCHEMA_V41,
];

/// The newest schema version this binary knows how to read and write.
pub fn latest_version() -> i64 {
    MIGRATIONS.len() as i64
}

/// Applies every migration newer than the recorded schema version.
/// Each migration runs in its own transaction together with its
/// version stamp, so a failed apply leaves the store untouched.
/// The `schema_migrations` table itself is created by v1, so on a
/// fresh (unmigrated) store its absence means version 0.
///
/// SPEC §13.2 / T33, ADR 0002: a store stamped with a version newer than
/// [`latest_version`] was migrated by a newer capyctl. This binary does not know
/// that schema, so it refuses to open the store rather than read or write it
/// under assumptions that no longer hold. Nothing is written before the
/// refusal.
pub fn apply(conn: &Connection) -> Result<(), StoreError> {
    apply_through(conn, latest_version())
}

/// [`apply`], stopping after version `last`. Tests use it to build a store as
/// an older binary left it.
fn apply_through(conn: &Connection, last: i64) -> Result<(), StoreError> {
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
    if current > latest_version() {
        return Err(StoreError::FromNewerVersion {
            found: current,
            supported: latest_version(),
        });
    }
    for (index, sql) in MIGRATIONS.iter().enumerate() {
        let version = (index + 1) as i64;
        if version <= current || version > last {
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
        if version == 37 {
            // ADR 0019: the GPU each instance was placed on.
            crate::instances::migrate_v37(&tx)?;
        }
        if version == 40 {
            // ADR 0014 amendment A16: the hybrid state slot beside the weights.
            crate::checkpoint_digests::migrate_v40(&tx)?;
        }
        if version == 41 {
            // ADR 0028 §5, §6: group plans, member owners, per-host digests.
            crate::groups::migrate_v41(&tx)?;
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

    /// ADR 0028 §5, §6 (v41): resource owners, endpoint leases and recorded
    /// digests from v40 are carried across; members get their own owners and
    /// worker leases, and each host's digest has its own row.
    // T14 T27 T33
    #[test]
    fn v41_adds_group_tables_and_carries_rows_across() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        apply_through(&conn, 40).unwrap();
        let digest = format!("sha256:{}", "a".repeat(64));
        conn.execute_batch(
            r#"INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version,revision) VALUES('a','a','model','ready',1,0,1,1,1);
            INSERT INTO effective_revisions VALUES('a',1,'{"host":{"name":"host-a"}}','fa');
            INSERT INTO resource_owners(owner_id,footprint_json,deployment_id,instance_index) VALUES('a','{}','a',0),('deployment:a/instance:1','{}','a',1);
            INSERT INTO runtime_bindings(id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state) VALUES('binding-a','a',1,'ia','managed','{}','[]','live');
            INSERT INTO endpoint_leases(host_id,host,port,binding_id) VALUES('host-a','127.0.0.1',20000,'binding-a');"#,
        )
        .unwrap();
        conn.execute(
            "INSERT INTO checkpoint_digests(deployment_id,revision,state,host_id,expected,digest,weights_bytes,provisional,diagnostic,updated_at_ms) VALUES('a',1,'recorded','host-a',NULL,?1,10,0,NULL,5)",
            [&digest],
        )
        .unwrap();
        apply(&conn).unwrap();
        apply(&conn).unwrap();
        let owners: Vec<(String, u32, Option<u32>)> = conn
            .prepare(
                "SELECT owner_id,instance_index,member_rank FROM resource_owners ORDER BY owner_id",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            owners,
            vec![
                ("a".into(), 0, None),
                ("deployment:a/instance:1".into(), 1, None)
            ]
        );
        let lease: (String, Option<String>) = conn
            .query_row(
                "SELECT binding_id,group_owner FROM endpoint_leases WHERE port=20000",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(lease, ("binding-a".into(), None));
        let host_digest: (String, String, i64) = conn
            .query_row("SELECT host_id,digest,recorded_at_ms FROM checkpoint_host_digests WHERE deployment_id='a' AND revision=1", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(host_digest, ("host-a".into(), digest, 5));
        // Members of one instance each get an owner; a second instance-form
        // owner of the same instance, or a member owner in the wrong form, is refused.
        conn.execute("INSERT INTO resource_owners(owner_id,footprint_json,deployment_id,instance_index,member_rank) VALUES('deployment:a/instance:2/member:0','{}','a',2,0),('deployment:a/instance:2/member:1','{}','a',2,1)", []).unwrap();
        assert!(conn.execute("INSERT INTO resource_owners(owner_id,footprint_json,deployment_id,instance_index,member_rank) VALUES('deployment:a/instance:2/member:3','{}','a',2,2)", []).is_err());
        assert!(conn.execute("INSERT INTO resource_owners(owner_id,footprint_json,deployment_id,instance_index) VALUES('a2','{}','a',0)", []).is_err());
        // A lease is held by a binding or by a group member, never both or neither.
        conn.execute("INSERT INTO endpoint_leases(host_id,host,port,binding_id,group_owner) VALUES('host-b','127.0.0.1',8100,NULL,'deployment:a/instance:2/member:1')", []).unwrap();
        assert!(conn.execute("INSERT INTO endpoint_leases(host_id,host,port,binding_id,group_owner) VALUES('host-b','127.0.0.1',8101,NULL,NULL)", []).is_err());
        assert!(conn.execute("INSERT INTO endpoint_leases(host_id,host,port,binding_id,group_owner) VALUES('host-b','127.0.0.1',8100,'binding-a',NULL)", []).is_err());
        // Group member states are closed.
        conn.execute(
            "INSERT INTO group_plans VALUES('a',2,1,'{}','host-a',25000,'active')",
            [],
        )
        .unwrap();
        assert!(conn.execute("INSERT INTO group_members(deployment_id,instance_index,generation,rank,host_id,owner_id,state) VALUES('a',2,1,0,'host-a','deployment:a/instance:2/member:0','gone')", []).is_err());
        conn.execute("INSERT INTO group_members(deployment_id,instance_index,generation,rank,host_id,owner_id,state) VALUES('a',2,1,0,'host-a','deployment:a/instance:2/member:0','reserved')", []).unwrap();
        // ADR 0028 §8, §11: a dispatched member is marked so durably; a
        // reserved one was never dispatched, and identities are recorded only
        // for a dispatched member.
        for (state, dispatched, identities) in [
            ("reserved", 1, None),
            ("dispatching", 0, None),
            ("dispatching", 1, Some("[]")),
            ("launched", 1, None),
            ("uncertain", 0, Some("[]")),
        ] {
            assert!(
                conn.execute(
                    "INSERT INTO group_members(deployment_id,instance_index,generation,rank,host_id,owner_id,state,dispatched,identities_json) VALUES('a',2,1,1,'host-b','deployment:a/instance:2/member:1',?1,?2,?3)",
                    rusqlite::params![state, dispatched, identities],
                )
                .is_err(),
                "{state} {dispatched} {identities:?}"
            );
        }
        conn.execute("INSERT INTO group_members(deployment_id,instance_index,generation,rank,host_id,owner_id,state,dispatched) VALUES('a',2,1,1,'host-b','deployment:a/instance:2/member:1','uncertain',1)", []).unwrap();
        // One unsettled plan holds a rendezvous port on its head.
        assert!(conn
            .execute(
                "INSERT INTO group_plans VALUES('a',3,1,'{}','host-a',25000,'active')",
                []
            )
            .is_err());
    }

    /// ADR 0028 §6 (v41): every measured digest is carried into the per-host
    /// table, `unusable` included (the measurement path writes its digest,
    /// host and time when the derived memory request cannot be resolved);
    /// only a `pending` row, which has measured nothing, is not.
    // T14
    #[test]
    fn v41_carries_every_measured_digest_including_unusable() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        apply_through(&conn, 40).unwrap();
        conn.execute_batch(
            r#"INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version,revision) VALUES('a','a','model','ready',1,0,1,1,3);
            INSERT INTO effective_revisions VALUES('a',1,'{"host":{"name":"host-a"}}','fa'),('a',2,'{"host":{"name":"host-b"}}','fb'),('a',3,'{"host":{"name":"host-c"}}','fc');"#,
        )
        .unwrap();
        let measured = |c: char| format!("sha256:{}", c.to_string().repeat(64));
        for (revision, state, host, digest, weights, at) in [
            (1, "unusable", "host-a", Some(measured('a')), None, 7),
            (2, "mismatch", "host-b", Some(measured('b')), Some(10), 8),
            (3, "pending", "host-c", None, None, 9),
        ] {
            conn.execute(
                "INSERT INTO checkpoint_digests(deployment_id,revision,state,host_id,expected,digest,weights_bytes,provisional,diagnostic,updated_at_ms) VALUES('a',?1,?2,?3,NULL,?4,?5,0,NULL,?6)",
                rusqlite::params![revision, state, host, digest, weights, at],
            )
            .unwrap();
        }
        apply(&conn).unwrap();
        apply(&conn).unwrap();
        let rows: Vec<(i64, String, String, i64)> = conn
            .prepare("SELECT revision,host_id,digest,recorded_at_ms FROM checkpoint_host_digests WHERE deployment_id='a' ORDER BY revision")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                (1, "host-a".into(), measured('a'), 7),
                (2, "host-b".into(), measured('b'), 8),
            ]
        );
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
                (
                    "host-a".into(),
                    "127.0.0.1".into(),
                    20000,
                    "binding-a".into()
                ),
                (
                    "host-b".into(),
                    "127.0.0.1".into(),
                    20001,
                    "binding-b".into()
                ),
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
            .execute(
                "INSERT INTO host_launch_claims VALUES('b','per_instance',8)",
                []
            )
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
        conn.execute(
            "INSERT INTO host_launch_claims VALUES('b','per_instance',8)",
            [],
        )
        .unwrap();
        assert!(conn
            .execute(
                "UPDATE host_launch_claims SET mode='per_host' WHERE host_id='a'",
                []
            )
            .is_err());
        assert!(conn
            .execute(
                "INSERT INTO host_launch_claims VALUES('missing','per_launch',9)",
                []
            )
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
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        let marker: (String, String, i64) = conn
            .query_row(
                "SELECT host_id,operation_id,recorded_at_ms FROM host_drains",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
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

    /// ADR 0018 §4: v35 adds profile retirements to an existing v34 store in
    /// place. The v34 host version rows are carried across, the new tables
    /// enforce their bounds, and a retirement's stops go with it.
    // T33
    #[test]
    fn v35_adds_profile_retirements_and_keeps_v34_rows() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        for (index, sql) in MIGRATIONS.iter().take(34).enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations(version) VALUES(?1)",
                [(index + 1) as i64],
            )
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO enrolled_hosts(host_id,host_name,key_digest,revoked) VALUES('lab','lab','key',0);
             INSERT INTO host_versions VALUES('lab','0.4.0','supported','','[]',7);
             INSERT INTO operations(id,kind,state) VALUES('op','ordinary_cleanup','running');",
        )
        .unwrap();
        apply(&conn).unwrap();
        apply(&conn).unwrap();
        let version: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(version, latest_version());
        let kept: (String, i64) = conn
            .query_row(
                "SELECT binary_version,recorded_at_ms FROM host_versions WHERE host_id='lab'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(kept, ("0.4.0".into(), 7));
        conn.execute_batch(
            "INSERT INTO profile_retirements VALUES('lab','local','k','retiring',8,9);
             INSERT INTO profile_retirement_stops VALUES('lab','local','op');",
        )
        .unwrap();
        for bad in [
            "INSERT INTO profile_retirements VALUES('','p','k','retiring',8,9)",
            "INSERT INTO profile_retirements VALUES('lab','p','k','gone',8,9)",
            "INSERT INTO profile_retirements VALUES('lab','p','k','retiring',8,7)",
            "INSERT INTO profile_retirement_stops VALUES('lab','absent','op')",
            "INSERT INTO profile_retirement_stops VALUES('lab','local','no-such-op')",
        ] {
            assert!(conn.execute(bad, []).is_err(), "{bad}");
        }
        conn.execute("DELETE FROM profile_retirements", []).unwrap();
        let stops: i64 = conn
            .query_row("SELECT COUNT(*) FROM profile_retirement_stops", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(stops, 0, "a retirement's stops are removed with it");
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

//! Forward-only migrations, versioned in `schema_migrations`.

use rusqlite::{Connection, OptionalExtension};

use crate::schema::{SCHEMA_V1, SCHEMA_V2, SCHEMA_V3};

/// One entry per version; `MIGRATIONS[0]` is version 1.
pub const MIGRATIONS: &[&str] = &[SCHEMA_V1, SCHEMA_V2, SCHEMA_V3];

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
        conn.execute_batch("INSERT INTO schema_migrations VALUES (1), (2);
            INSERT INTO deployments(id, name, kind, desired_state, admission_enabled,
              suspended, current_generation, schema_version)
            VALUES ('a', 'a', 'model', 'stopped', 1, 0, 1, 1);
            INSERT INTO owners(id, kind, deployment_id) VALUES ('a', 'model', 'a');
            INSERT INTO reservations(owner_id, domain_id, bytes, phase)
            VALUES ('a', 'system', 64, 'activation');").unwrap();
        apply(&conn).unwrap();
        apply(&conn).unwrap();
        let revision: i64 = conn.query_row(
            "SELECT revision FROM deployments WHERE id='a'", [], |r| r.get(0)).unwrap();
        let bytes: i64 = conn.query_row(
            "SELECT bytes FROM reservations WHERE owner_id='a'", [], |r| r.get(0)).unwrap();
        let epoch: i64 = conn.query_row(
            "SELECT epoch FROM resource_ledger_meta WHERE singleton=1", [], |r| r.get(0)).unwrap();
        assert_eq!((revision, bytes, epoch), (1, 64, 0));
    }
}

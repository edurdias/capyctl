//! Forward-only migrations, versioned in `schema_migrations`.

use rusqlite::{Connection, OptionalExtension};

use crate::schema::{SCHEMA_V1, SCHEMA_V2};

/// One entry per version; `MIGRATIONS[0]` is version 1.
pub const MIGRATIONS: &[&str] = &[SCHEMA_V1, SCHEMA_V2];

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
}

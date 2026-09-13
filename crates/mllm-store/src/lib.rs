//! Embedded transactional SQLite store (ADR 0002): forward-only
//! migrations, transactional deployment acceptance with derived
//! idempotency, and owner-only file permissions.

pub mod deployments;
pub mod dispatch;
pub mod events;
pub mod lifecycle;
pub mod migrations;
pub mod resource_ledger;
pub mod schema;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

pub use deployments::{
    AcceptDeployment, Accepted, DeploymentRow, GenerationRow, NewOperation, OpState,
    OperationRow, ReservationRow,
};
 
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("conflict: the request clashes with existing store state")]
    Conflict,
    #[error("idempotency conflict: same key with different content")]
    IdempotencyConflict,
    #[error("stale generation")]
    StaleGeneration,
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// File-backed or in-memory (for tests) durable store.
pub struct Store {
    pub(crate) conn: Connection,
}

impl Store {
    /// Opens (creating if needed) a file-backed store, applies missing
    /// migrations, and enforces owner-only permissions on the store
    /// file, its WAL/journal sidecars, and the containing directory.
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            }
        }
        let conn = Connection::open(path)?;
        set_pragmas(&conn)?;
        migrations::apply(&conn)?;
        set_owner_only(path)?;
        Ok(Store { conn })
    }

    /// In-memory store with the same schema and pragmas.
    pub fn open_in_memory() -> Result<Store, StoreError> {
        let conn = Connection::open_in_memory()?;
        set_pragmas(&conn)?;
        migrations::apply(&conn)?;
        Ok(Store { conn })
    }
}

fn set_pragmas(conn: &Connection) -> Result<(), StoreError> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

fn set_owner_only(path: &Path) -> Result<(), StoreError> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = PathBuf::from(format!("{}{suffix}", path.display()));
        if sidecar.exists() {
            fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_store_is_migrated() {
        let s = Store::open_in_memory().unwrap();
        let version: i64 = s
            .conn
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(version as usize, migrations::MIGRATIONS.len());
        assert_eq!(s.deployment_count().unwrap(), 0);
    }

    #[test]
    fn wal_and_foreign_keys_are_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("srv.sqlite3")).unwrap();
        let mode: String = s
            .conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let fk: i64 = s
            .conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1);
    }

    #[test]
    fn sidecars_are_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("srv.sqlite3");
        let s = Store::open(&path).unwrap();
        s.conn.execute_batch("PRAGMA wal_checkpoint;").unwrap();
        let s2 = Store::open(&path).unwrap();
        drop(s2);
        drop(s);
        for candidate in [
            format!("{}-wal", path.display()),
            format!("{}-shm", path.display()),
        ] {
            if let Ok(meta) = fs::metadata(&candidate) {
                assert_eq!(meta.permissions().mode() & 0o077, 0, "{candidate}");
            }
        }
    }
}

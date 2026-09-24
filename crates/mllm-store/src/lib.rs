//! Embedded transactional SQLite store (ADR 0002): forward-only
//! migrations, transactional deployment acceptance with derived
//! idempotency, and owner-only file permissions.

pub mod attempts;
// ADR 0014 §7 (WE3): recorded checkpoint digests.
pub mod checkpoint_digests;
pub mod deployments;
pub mod development_controls;
pub mod dispatch;
pub mod events;
pub mod enrollment;
pub mod host_drain;
pub mod host_publication;
// ADR 0013 §5: deployment instances.
pub mod instances;
pub mod lifecycle;
// ADR 0014 amendment A1: default Initialize and Stop windows.
pub mod lifecycle_windows;
pub mod managed_configuration;
pub mod migrations;
// ADR 0008: declared remote model sources, materialized per host.
pub mod model_sources;
pub mod ordinary_lifecycle;
pub mod residency;
pub mod resource_ledger;
// ADR 0007: resident floors attributed by process identity.
mod resident_floors;
pub mod resource_policy;
mod resource_namespace;
pub mod schema;
pub mod secrets;
// ADR 0013 §10 (I3): the router's read of serving instances.
pub mod serving;
pub mod snapshot;
// W10 gaps: dispatch closure reasons, switch in progress, warm residency.
pub mod switch_state;
// SPEC §6.3 (W6): `delete deployment` after verified cleanup, leaving a tombstone.
pub mod delete;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

pub use deployments::{
    AcceptDeployment, Accepted, DeploymentRow, GenerationRow, NewOperation, OpState, OperationRow,
    ReservationRow,
};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("conflict: the request clashes with existing store state")]
    Conflict,
    #[error("idempotency conflict: same key with different content")]
    IdempotencyConflict,
    #[error("stale generation")]
    StaleGeneration,
    /// SPEC §13.2 / T33: the store was migrated by a newer mllm. An older
    /// binary never opens it, because it would read and write a schema it does
    /// not know.
    #[error(
        "the state store has schema version {found}, newer than the {supported} this mllm \
         supports; it was written by a newer mllm. Run that newer mllm, or restore the \
         store from a backup taken before the upgrade. The store was not modified"
    )]
    FromNewerVersion { found: i64, supported: i64 },
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// File-backed or in-memory (for tests) durable store.
pub struct Store {
    pub(crate) conn: Connection,
    // Spec §3: the identity key that seals engine keys at rest. Absent until the
    // caller installs one with `set_secrets_key`; engine-key reads and writes fail
    // closed (`StoreError::Conflict`) until then.
    pub(crate) secrets: Option<secrets::SecretsKey>,
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
        Ok(Store {
            conn,
            secrets: None,
        })
    }

    /// In-memory store with the same schema and pragmas.
    pub fn open_in_memory() -> Result<Store, StoreError> {
        let conn = Connection::open_in_memory()?;
        set_pragmas(&conn)?;
        migrations::apply(&conn)?;
        Ok(Store {
            conn,
            secrets: None,
        })
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

    // T33: a store migrated by a newer mllm is refused by an older binary, and
    // the refusal writes nothing, so the newer binary can still open it.
    #[test]
    fn store_from_a_newer_version_is_refused_and_left_unmodified() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("srv.sqlite3");
        let latest = migrations::latest_version();
        {
            let s = Store::open(&path).unwrap();
            // What a newer binary leaves behind: its own stamp and a table this
            // binary has never heard of.
            s.conn
                .execute_batch("CREATE TABLE from_the_future(x INTEGER);")
                .unwrap();
            s.conn
                .execute(
                    "INSERT INTO schema_migrations(version) VALUES (?1)",
                    [latest + 1],
                )
                .unwrap();
        }
        match Store::open(&path) {
            Err(StoreError::FromNewerVersion { found, supported }) => {
                assert_eq!((found, supported), (latest + 1, latest));
            }
            Err(other) => panic!("expected FromNewerVersion, got {other}"),
            Ok(_) => panic!("an older binary opened a newer store"),
        }
        let message = StoreError::FromNewerVersion {
            found: latest + 1,
            supported: latest,
        }
        .to_string();
        assert!(message.contains("newer mllm"), "{message}");
        assert!(message.contains("backup"), "{message}");
        let conn = Connection::open(&path).unwrap();
        let (max, stamps): (i64, i64) = conn
            .query_row(
                "SELECT MAX(version), COUNT(*) FROM schema_migrations",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((max, stamps), (latest + 1, latest + 1));
        let future: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='from_the_future'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(future, 1);
    }

    // T33: the store at exactly this binary's version reopens normally.
    #[test]
    fn store_at_the_latest_version_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("srv.sqlite3");
        drop(Store::open(&path).unwrap());
        drop(Store::open(&path).unwrap());
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
        for sidecar in [
            format!("{}-wal", path.display()),
            format!("{}-shm", path.display()),
        ] {
            if let Ok(meta) = fs::metadata(&sidecar) {
                assert_eq!(meta.permissions().mode() & 0o077, 0, "{sidecar}");
            }
        }
    }
}

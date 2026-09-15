//! Lock-before-session composition for a protected, service-owned state directory.

use mllm_launchers::ControllerLock;
use mllm_store::{
    Store, StoreError,
    dispatch::{CoordinatorSession, DispatchError},
};
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum OwnedStateError {
    #[error("controller state ownership failed: {0}")]
    Ownership(#[from] io::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Session(#[from] DispatchError),
}

/// One store/session pair whose process lock outlives its SQLite connection.
/// Fields are private, non-extractable, and drop in declaration order.
pub struct OwnedCoordinatorState {
    store: Store,
    session: CoordinatorSession,
    _lock: ControllerLock,
}

impl OwnedCoordinatorState {
    /// The directory must already exist, be canonical, service-owned and 0700.
    /// Existing SQLite files must be service-owned, single-link regular 0600
    /// files. No repair is attempted. Root and the service UID are trusted not
    /// to replace paths concurrently; this is not a sandbox against that UID.
    /// This constructor establishes ownership, not runtime reconciliation.
    pub fn open(state_dir: &Path) -> Result<Self, OwnedStateError> {
        let uid = fs::metadata("/proc/self")?.uid();
        validate_directory(state_dir, uid)?;
        let lock = ControllerLock::acquire(&state_dir.join("controller.lock"))?;
        // Only the lock winner may open SQLite (including migrations) or change
        // the durable session. Validate every SQLite leaf before any DB effect.
        for name in [
            "srv.sqlite3",
            "srv.sqlite3-wal",
            "srv.sqlite3-shm",
            "srv.sqlite3-journal",
        ] {
            validate_database_file(&state_dir.join(name), uid)?;
        }
        let store = Store::open(&state_dir.join("srv.sqlite3"))?;
        let session = store.begin_coordinator_session()?;
        Ok(Self {
            store,
            session,
            _lock: lock,
        })
    }

    pub fn store(&self) -> &Store {
        &self.store
    }
    pub fn session(&self) -> &CoordinatorSession {
        &self.session
    }
}

fn unsafe_path() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "unsafe controller state path",
    )
}

fn validate_directory(path: &Path, uid: u32) -> io::Result<()> {
    let text = path.to_str().ok_or_else(unsafe_path)?;
    if !text.starts_with('/')
        || text.len() > 4000
        || text.chars().any(char::is_control)
        || text[1..]
            .split('/')
            .any(|part| matches!(part, "" | "." | ".."))
    {
        return Err(unsafe_path());
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.uid() != uid
        || metadata.mode() & 0o7777 != 0o700
        || path.canonicalize()? != path
    {
        return Err(unsafe_path());
    }
    // ControllerLock additionally verifies every canonical ancestor before
    // creating the lock, so shared/writable parent directories are rejected.
    Ok(())
}

fn validate_database_file(path: &Path, uid: u32) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != uid
        || metadata.mode() & 0o7777 != 0o600
    {
        return Err(unsafe_path());
    }
    Ok(())
}

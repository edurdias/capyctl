//! Lock-before-session composition for a protected, service-owned state directory.

use capyctl_launchers::ControllerLock;
use capyctl_store::{
    dispatch::{CoordinatorSession, DispatchError},
    secrets::SecretsKey,
    Store, StoreError,
};
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// Shared application ownership; guards must never cross an external await.
pub type SharedCoordinatorState = std::sync::Arc<std::sync::Mutex<OwnedCoordinatorState>>;

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
    worker_claimed: bool,
    _lock: ControllerLock,
}

impl OwnedCoordinatorState {
    /// For tests only: opens the state directory under an identity key that is
    /// generated here and dies with the process. Nothing it seals can be opened
    /// again after a restart, so a store opened this way could hold engine keys
    /// that S1r can never recover. `open_with_secrets` is the product
    /// constructor; this one is hidden rather than renamed because the name is
    /// spelled in a great many tests.
    ///
    /// The directory must already exist, be canonical, service-owned and 0700.
    /// Existing SQLite files must be service-owned, single-link regular 0600
    /// files. No repair is attempted. Root and the service UID are trusted not
    /// to replace paths concurrently; this is not a sandbox against that UID.
    /// This constructor establishes ownership, not runtime reconciliation.
    #[doc(hidden)]
    pub fn open(state_dir: &Path) -> Result<Self, OwnedStateError> {
        // Spec §3: a store with no identity key cannot seal an engine key, and a
        // caller that never launches an engine still needs one installed rather
        // than a store that fails the first time it is asked. An ephemeral key
        // lives only for this process, so nothing it seals survives a restart;
        // a server that must outlive one opens with `open_with_secrets`.
        Self::open_with_secrets(state_dir, SecretsKey::generate_ephemeral())
    }

    /// Open the state directory and install the identity key that seals every
    /// per-launch engine key (Spec §3). The key is read from a file outside the
    /// database, so the database alone never recovers an engine key.
    pub fn open_with_secrets(
        state_dir: &Path,
        secrets: SecretsKey,
    ) -> Result<Self, OwnedStateError> {
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
        let mut store = Store::open(&state_dir.join("srv.sqlite3"))?;
        store.set_secrets_key(secrets);
        let session = store.begin_coordinator_session()?;
        Ok(Self {
            store,
            session,
            worker_claimed: false,
            _lock: lock,
        })
    }

    pub fn store(&self) -> &Store {
        &self.store
    }
    pub fn session(&self) -> &CoordinatorSession {
        &self.session
    }

    /// One worker for this session, including after it conservatively halts.
    pub(crate) fn claim_worker(&mut self) -> bool {
        if self.worker_claimed {
            return false;
        }
        self.worker_claimed = true;
        true
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

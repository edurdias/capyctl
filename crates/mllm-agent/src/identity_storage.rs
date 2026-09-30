//! SPEC §§4.1 and 13.3: atomic private identity bundles, protected before RPC.
//! The caller validates the bundle's schema and cryptographic content. Absence
//! alone permits initialization; unreadable or partial state never does.
//! Root and the service UID are trusted, as in controller state ownership.
use sha2::{Digest, Sha256};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const MAX_BYTES: usize = 128 * 1024;
const LOCK: &str = "identity.lock";

#[derive(Clone, Copy, Debug, thiserror::Error)]
pub enum StorageError {
    #[error("invalid identity storage")]
    Invalid,
    #[error("identity storage is in use")]
    Busy,
    #[error("identity already exists")]
    Exists,
    #[error("identity revision changed")]
    Conflict,
    #[error("identity storage unavailable")]
    Unavailable,
}

/// One exclusive local identity writer. The directory must already exist;
/// explicit initialization owns its creation, never an implicit startup repair.
pub struct IdentityDirectory {
    path: PathBuf,
    directory: File,
    metadata: Metadata,
    lock: File,
    lock_metadata: Metadata,
    uid: u32,
    writer: std::sync::Mutex<()>,
    owner_pid: libc::pid_t,
}

impl Drop for IdentityDirectory {
    fn drop(&mut self) {
        // SPEC §13.1: flock belongs to this writer's lifetime. A fork inherits
        // its open description until exec, so close alone can retain ownership.
        // An inherited Rust value must never unlock the original process's lock.
        if unsafe { libc::getpid() } == self.owner_pid {
            unsafe {
                libc::flock(self.lock.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

impl IdentityDirectory {
    /// Recheck retained directory and lock custody before using adjacent state.
    /// This does not validate other files; their owner must check each leaf.
    pub fn validate(&self) -> Result<(), StorageError> {
        self.revalidate()
    }

    pub fn open(path: &Path) -> Result<Self, StorageError> {
        let uid = unsafe { libc::geteuid() };
        validate_directory(path, uid)?;
        let directory = open_file(path, true)?;
        let metadata = directory.metadata().map_err(unavailable)?;
        if !same(&metadata, &fs::symlink_metadata(path).map_err(unavailable)?) {
            return Err(StorageError::Invalid);
        }
        let lock_path = path.join(LOCK);
        if let Some(meta) = existing(&lock_path)? {
            validate_leaf(&meta, uid, true)?;
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&lock_path)
            .map_err(unavailable)?;
        let lock_metadata = lock.metadata().map_err(unavailable)?;
        validate_leaf(&lock_metadata, uid, true)?;
        // SPEC §13.1: competing local writers cannot rotate identity underneath
        // an enrollment transaction. Drop explicitly releases this writer's flock.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(StorageError::Busy);
        }
        let storage = Self {
            path: path.into(),
            directory,
            metadata,
            lock,
            lock_metadata,
            uid,
            writer: std::sync::Mutex::new(()),
            owner_pid: unsafe { libc::getpid() },
        };
        storage.revalidate()?;
        Ok(storage)
    }

    pub fn read_bundle(&self, name: &str) -> Result<Option<Vec<u8>>, StorageError> {
        let path = self.bundle_path(name)?;
        self.revalidate()?;
        let Some(before) = existing(&path)? else {
            return Ok(None);
        };
        validate_leaf(&before, self.uid, false)?;
        let mut file = open_file(&path, false)?;
        let opened = file.metadata().map_err(unavailable)?;
        if !same(&before, &opened) {
            return Err(StorageError::Invalid);
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take((MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(unavailable)?;
        valid_bytes(&bytes)?;
        if bytes.len() as u64 != opened.len()
            || !same(&opened, &file.metadata().map_err(unavailable)?)
            || !same(&opened, &fs::symlink_metadata(&path).map_err(unavailable)?)
        {
            return Err(StorageError::Invalid);
        }
        self.revalidate()?;
        Ok(Some(bytes))
    }

    /// Publish a fully serialized bundle atomically without replacing any leaf.
    /// A lost success reply is recoverable with read_bundle, never new keys.
    pub fn create_bundle(&self, name: &str, bytes: &[u8]) -> Result<(), StorageError> {
        let _writer = self.writer.lock().map_err(|_| StorageError::Unavailable)?;
        let path = self.bundle_path(name)?;
        valid_bytes(bytes)?;
        self.revalidate()?;
        if existing(&path)?.is_some() {
            return Err(StorageError::Exists);
        }
        let temp = self.temporary(bytes)?;
        self.revalidate()?;
        temp.persist_noclobber(&path).map_err(|error| {
            if error.error.kind() == std::io::ErrorKind::AlreadyExists {
                StorageError::Exists
            } else {
                StorageError::Unavailable
            }
        })?;
        self.directory.sync_all().map_err(unavailable)?;
        Ok(())
    }

    /// SPEC §13.3: remove a bundle whose launch is settled. An absent bundle is
    /// already removed. The directory is synced so the removal is durable.
    pub fn remove_bundle(&self, name: &str) -> Result<(), StorageError> {
        let _writer = self.writer.lock().map_err(|_| StorageError::Unavailable)?;
        let path = self.bundle_path(name)?;
        self.revalidate()?;
        let Some(before) = existing(&path)? else {
            return Ok(());
        };
        validate_leaf(&before, self.uid, false)?;
        fs::remove_file(&path).map_err(unavailable)?;
        self.directory.sync_all().map_err(unavailable)?;
        Ok(())
    }

    /// Replace an exact previous bundle while holding the enrollment lock.
    /// expected_digest is lowercase SHA-256 of the complete previously read bytes.
    pub fn replace_bundle(
        &self,
        name: &str,
        expected_digest: &str,
        bytes: &[u8],
    ) -> Result<(), StorageError> {
        let _writer = self.writer.lock().map_err(|_| StorageError::Unavailable)?;
        let path = self.bundle_path(name)?;
        valid_bytes(bytes)?;
        let previous = self.read_bundle(name)?.ok_or(StorageError::Conflict)?;
        if hex::encode(Sha256::digest(&previous)) != expected_digest {
            return Err(StorageError::Conflict);
        }
        let temp = self.temporary(bytes)?;
        self.revalidate()?;
        // The lock serializes cooperating writers; root/service UID are trusted.
        temp.persist(&path).map_err(|_| StorageError::Unavailable)?;
        self.directory.sync_all().map_err(unavailable)?;
        Ok(())
    }

    fn bundle_path(&self, name: &str) -> Result<PathBuf, StorageError> {
        if name.is_empty()
            || name.len() > 128
            || matches!(name, "." | ".." | LOCK)
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        {
            return Err(StorageError::Invalid);
        }
        Ok(self.path.join(name))
    }

    fn temporary(&self, bytes: &[u8]) -> Result<tempfile::NamedTempFile, StorageError> {
        let mut file = tempfile::Builder::new()
            .prefix(".identity-")
            .tempfile_in(&self.path)
            .map_err(unavailable)?;
        validate_leaf(
            &file.as_file().metadata().map_err(unavailable)?,
            self.uid,
            true,
        )?;
        file.write_all(bytes).map_err(unavailable)?;
        file.as_file().sync_all().map_err(unavailable)?;
        Ok(file)
    }

    fn revalidate(&self) -> Result<(), StorageError> {
        if unsafe { libc::getpid() } != self.owner_pid {
            return Err(StorageError::Invalid);
        }
        validate_directory(&self.path, self.uid)?;
        if !same(
            &self.metadata,
            &self.directory.metadata().map_err(unavailable)?,
        ) || !same(
            &self.metadata,
            &fs::symlink_metadata(&self.path).map_err(unavailable)?,
        ) || !same(
            &self.lock_metadata,
            &self.lock.metadata().map_err(unavailable)?,
        ) || !same(
            &self.lock_metadata,
            &fs::symlink_metadata(self.path.join(LOCK)).map_err(unavailable)?,
        ) {
            return Err(StorageError::Invalid);
        }
        Ok(())
    }
}

fn unavailable(_: std::io::Error) -> StorageError {
    StorageError::Unavailable
}
fn existing(path: &Path) -> Result<Option<Metadata>, StorageError> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(unavailable(error)),
    }
}
fn valid_bytes(bytes: &[u8]) -> Result<(), StorageError> {
    if bytes.is_empty() || bytes.len() > MAX_BYTES {
        Err(StorageError::Invalid)
    } else {
        Ok(())
    }
}
fn validate_leaf(meta: &Metadata, uid: u32, allow_empty: bool) -> Result<(), StorageError> {
    if !meta.is_file()
        || meta.uid() != uid
        || meta.nlink() != 1
        || meta.mode() & 0o7777 != 0o600
        || meta.len() > MAX_BYTES as u64
        || (!allow_empty && meta.len() == 0)
    {
        Err(StorageError::Invalid)
    } else {
        Ok(())
    }
}
/// Why [`IdentityDirectory::open`] refused `path`, in words an operator can act
/// on. The checks are the ones `validate_directory` applies, run again only to
/// name the first that fails; the refusal itself never depends on this text.
pub fn describe_refusal(path: &Path, error: &StorageError) -> String {
    let shown = path.display();
    match error {
        StorageError::Busy => format!("{shown} is in use by another mllm process"),
        StorageError::Invalid => {
            let uid = unsafe { libc::geteuid() };
            match fs::symlink_metadata(path) {
                Err(_) => return format!("{shown} does not exist"),
                Ok(meta) if !meta.is_dir() => return format!("{shown} is not a directory"),
                Ok(meta) if meta.uid() != uid => {
                    return format!("{shown} is owned by another user")
                }
                Ok(meta) if meta.mode() & 0o7777 != 0o700 => {
                    return format!(
                        "{shown} has mode {:04o}; it must be 0700",
                        meta.mode() & 0o7777
                    )
                }
                Ok(_) => {}
            }
            for ancestor in path.ancestors().skip(1) {
                if let Ok(meta) = fs::symlink_metadata(ancestor) {
                    if ![0, uid].contains(&meta.uid()) || meta.mode() & 0o022 != 0 {
                        return format!(
                            "{} (a parent of {shown}) can be written by other users; \
                             use a directory whose parents only you or root can write, \
                             for example under your home directory",
                            ancestor.display()
                        );
                    }
                }
            }
            format!("{shown} is not a canonical path or holds an unsafe entry")
        }
        other => format!("{shown}: {other}"),
    }
}

fn validate_directory(path: &Path, uid: u32) -> Result<(), StorageError> {
    let text = path.to_str().ok_or(StorageError::Invalid)?;
    if !path.is_absolute()
        || text.len() > 4000
        || text.bytes().any(|b| b.is_ascii_control())
        || text[1..]
            .split('/')
            .any(|part| matches!(part, "" | "." | ".."))
        || path.canonicalize().map_err(unavailable)? != path
    {
        return Err(StorageError::Invalid);
    }
    let meta = fs::symlink_metadata(path).map_err(unavailable)?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o7777 != 0o700 {
        return Err(StorageError::Invalid);
    }
    for ancestor in path.ancestors() {
        let meta = fs::symlink_metadata(ancestor).map_err(unavailable)?;
        if !meta.is_dir() || ![0, uid].contains(&meta.uid()) || meta.mode() & 0o022 != 0 {
            return Err(StorageError::Invalid);
        }
    }
    Ok(())
}
fn open_file(path: &Path, directory: bool) -> Result<File, StorageError> {
    OpenOptions::new()
        .read(true)
        .custom_flags(
            libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | libc::O_CLOEXEC
                | if directory { libc::O_DIRECTORY } else { 0 },
        )
        .open(path)
        .map_err(unavailable)
}
fn same(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.mode() == b.mode()
        && a.uid() == b.uid()
        && a.gid() == b.gid()
        && (a.is_dir()
            || (a.nlink() == b.nlink()
                && a.len() == b.len()
                && a.mtime() == b.mtime()
                && a.mtime_nsec() == b.mtime_nsec()
                && a.ctime() == b.ctime()
                && a.ctime_nsec() == b.ctime_nsec()))
}

//! ADR 0014 §7 (WE3): checkpoint identity, measured on the host that holds it.
//!
//! The checkpoint digest is `sha256:` over a canonical manifest: every regular
//! file under the checkpoint directory, sorted by relative path, each with its
//! size and SHA-256. It is host independent: modification times, inode numbers
//! and the store's own location never enter it, so two hosts holding the same
//! bytes compute the same digest.
//!
//! Owner decision Q9 (2026-09-22): a checkpoint is hashed in full on its first
//! placement on a host and whenever any file's stat identity changes; every
//! launch and wake re-checks the stat identity of every file and rehashes the
//! small files (configuration, tokenizer, index). The stat identity here is
//! device, inode, size, modification time and change time: change time is
//! stricter than Q9's size, mtime and inode, and catches content rewritten in
//! place with its modification time set back.
//!
//! Every path is opened through descriptors (the retired
//! `runtime/checkpoint_preflight.py` open chain, ported): the model store and
//! checkpoint are canonicalized once, containment is checked on that text, and
//! from then on every component is opened with `O_NOFOLLOW` relative to its
//! already-open parent, so no component is re-resolved between check and hash.
//! A component swapped for a symbolic link afterwards fails closed.
//!
//! Symbolic links: a link to a regular file is followed only when its target
//! resolves inside the host's model store (the Hugging Face snapshot layout,
//! `snapshots/<rev>/<file> -> ../../blobs/<hash>`); it enters the manifest
//! under the link's own path with the target's bytes. A link that escapes the
//! store, a link to a directory and a link to a link are refused.
//!
//! Names starting with `.` are not part of a checkpoint: they are tool
//! metadata (`.cache/huggingface/`, `.gitattributes`, lock files) that differ
//! between downloads of the same bytes, and no engine loads weights from them.
//!
//! Open issue (ADR 0014 §7, 3): a file can still change between this check and
//! the engine's read. The mitigation is a model store other users cannot write.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::{CStr, CString},
    io::Read,
    mem::MaybeUninit,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::ffi::OsStrExt,
    },
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

/// Domain separation for the canonical manifest encoding.
pub const MANIFEST_DOMAIN: &[u8] = b"capyctl/checkpoint-manifest/v1\n";
/// Files up to this size are rehashed on every launch and wake even when their
/// stat identity is unchanged (configuration, tokenizer, index files).
pub const SMALL_FILE_LIMIT: u64 = 64 << 20;
/// Bounds on one checkpoint: files, directories held open, depth and path
/// length. A checkpoint beyond them is refused rather than partially measured.
pub const MAX_FILES: usize = 65_536;
const MAX_DIRECTORIES: usize = 1_024;
/// ADR 0014 §7 (A5): the longest link chain a checkpoint file may use.
const MAX_LINK_HOPS: usize = 8;
const MAX_DEPTH: usize = 16;
const MAX_PATH_BYTES: usize = 4_096;
/// Hashing runs on at most this many threads (one file per thread at a time).
const MAX_WORKERS: usize = 8;
const CHUNK: usize = 1 << 20;
/// File suffixes whose sizes count as weights for ADR 0014 §5.
const WEIGHT_SUFFIXES: &[&str] = &[".safetensors", ".bin", ".pt", ".pth", ".gguf"];
const CACHE_VERSION: u32 = 1;

/// A closed failure category. It never carries a path, file name or OS text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CheckpointError {
    /// The model store or checkpoint path is not an absolute directory inside
    /// the host's model store.
    #[error("invalid_root")]
    InvalidRoot,
    /// A file that is not regular, a link that escapes the store or names a
    /// directory, or a name the manifest cannot encode.
    #[error("unsafe_file")]
    UnsafeFile,
    /// The checkpoint exceeds the file, directory, depth or path bounds.
    #[error("too_large")]
    TooLarge,
    /// A file changed while it was being measured.
    #[error("changed")]
    Changed,
    /// The measured digest is not the expected one.
    #[error("mismatch")]
    Mismatch,
    #[error("io_error")]
    Io,
}

impl CheckpointError {
    /// The closed wire category (`reason` of the DigestCheckpoint result).
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidRoot => "invalid_root",
            Self::UnsafeFile => "unsafe_file",
            Self::TooLarge => "too_large",
            Self::Changed => "changed",
            Self::Mismatch => "mismatch",
            Self::Io => "io_error",
        }
    }
}

fn os_error(error: std::io::Error) -> CheckpointError {
    match error.raw_os_error() {
        Some(libc::ELOOP | libc::ENOTDIR | libc::ENXIO) => CheckpointError::UnsafeFile,
        Some(libc::ENOENT) => CheckpointError::Changed,
        _ => CheckpointError::Io,
    }
}

/// One file's stat identity. Any difference forces a full rehash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
    pub size: u64,
    pub mtime_sec: i64,
    pub mtime_nsec: i64,
    pub ctime_sec: i64,
    pub ctime_nsec: i64,
}

impl FileIdentity {
    #[allow(clippy::unnecessary_cast)]
    fn of(st: &libc::stat) -> Self {
        Self {
            device: st.st_dev as u64,
            inode: st.st_ino as u64,
            size: st.st_size.max(0) as u64,
            mtime_sec: st.st_mtime as i64,
            mtime_nsec: st.st_mtime_nsec as i64,
            ctime_sec: st.st_ctime as i64,
            ctime_nsec: st.st_ctime_nsec as i64,
        }
    }
}

/// One manifest line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// Relative path with `/` separators, as UTF-8.
    pub path: String,
    pub size: u64,
    /// Lowercase hexadecimal SHA-256 of the file's bytes.
    pub sha256: String,
}

/// A measured checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointManifest {
    pub entries: Vec<ManifestEntry>,
    /// `sha256:<64 hex>` over the canonical encoding.
    pub digest: String,
    /// ADR 0014 §5: the sum of the weight files' sizes.
    pub weights_bytes: i64,
    pub total_bytes: u64,
}

impl CheckpointManifest {
    /// Build the manifest (and its digest) from entries in any order.
    pub fn from_entries(mut entries: Vec<ManifestEntry>) -> Result<Self, CheckpointError> {
        entries.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        if entries.windows(2).any(|w| w[0].path == w[1].path) {
            return Err(CheckpointError::UnsafeFile);
        }
        let mut digest = Sha256::new();
        digest.update(MANIFEST_DOMAIN);
        let mut weights: i64 = 0;
        let mut total: u64 = 0;
        for entry in &entries {
            if !encodable(&entry.path)
                || entry.sha256.len() != 64
                || !entry
                    .sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(CheckpointError::UnsafeFile);
            }
            digest.update(entry.path.as_bytes());
            digest.update([0]);
            digest.update(entry.size.to_string().as_bytes());
            digest.update([0]);
            digest.update(entry.sha256.as_bytes());
            digest.update(b"\n");
            total = total
                .checked_add(entry.size)
                .ok_or(CheckpointError::TooLarge)?;
            if is_weight(&entry.path) {
                weights = i64::try_from(entry.size)
                    .ok()
                    .and_then(|size| weights.checked_add(size))
                    .ok_or(CheckpointError::TooLarge)?;
            }
        }
        Ok(Self {
            digest: format!(
                "{}{}",
                capyctl_config::effective::CHECKPOINT_DIGEST_PREFIX,
                hex::encode(digest.finalize())
            ),
            entries,
            weights_bytes: weights,
            total_bytes: total,
        })
    }
}

fn encodable(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_PATH_BYTES
        && !path.bytes().any(|b| b == b'\n' || b == 0)
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn is_weight(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    WEIGHT_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
}

/// A checkpoint sized without hashing (`CheckpointVerifier::size`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointSize {
    /// ADR 0014 §5: the sum of the weight files' sizes, as a manifest counts it.
    pub weights_bytes: i64,
    pub file_count: u64,
    pub total_bytes: u64,
}

/// The result of one verification: the manifest, and whether every file had to
/// be hashed (first placement on this host, or a changed file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verification {
    pub manifest: CheckpointManifest,
    pub full_rehash: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct CachedFile {
    path: String,
    identity: FileIdentity,
    sha256: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct CacheRecord {
    version: u32,
    store: String,
    checkpoint: String,
    root: (u64, u64),
    files: Vec<CachedFile>,
}

/// The largest cache record read back (a manifest of many files is small).
const MAX_CACHE_RECORD_BYTES: u64 = 64 << 20;

/// Measures and verifies checkpoints, keeping a per-host cache of each file's
/// stat identity and hash. The cache lives in the host's private state
/// directory; without one it is kept in memory only, so the first placement
/// after a restart hashes in full.
pub struct CheckpointVerifier {
    cache_dir: Option<PathBuf>,
    memory: Mutex<BTreeMap<String, CacheRecord>>,
    /// Serializes measurements of the same checkpoint so a concurrent second
    /// caller reuses the first one's full hash.
    locks: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
}

impl CheckpointVerifier {
    /// A verifier whose cache persists under `cache_dir` (created private).
    pub fn with_cache_dir(cache_dir: PathBuf) -> Self {
        Self {
            cache_dir: Some(cache_dir),
            memory: Mutex::default(),
            locks: Mutex::default(),
        }
    }

    /// A verifier with an in-memory cache only.
    pub fn in_memory() -> Self {
        Self {
            cache_dir: None,
            memory: Mutex::default(),
            locks: Mutex::default(),
        }
    }

    /// Measure the checkpoint and require `expected` (ADR 0014 §7). A mismatch
    /// is refused; the caller must not have started anything yet.
    pub fn verify(
        &self,
        model_store: &Path,
        checkpoint: &Path,
        expected: &str,
    ) -> Result<Verification, CheckpointError> {
        if !capyctl_config::effective::is_checkpoint_digest(expected) {
            return Err(CheckpointError::Mismatch);
        }
        let verification = self.measure(model_store, checkpoint)?;
        if verification.manifest.digest != expected {
            return Err(CheckpointError::Mismatch);
        }
        Ok(verification)
    }

    /// Owner decision 2026-09-23 (solo first start): size the checkpoint with
    /// the same bounded, confined walk `measure` uses, hashing nothing. Cheap
    /// (a stat per file), so the startup estimate a placement uses is known
    /// before a first start instead of only after the full digest.
    pub fn size(
        &self,
        model_store: &Path,
        checkpoint: &Path,
    ) -> Result<CheckpointSize, CheckpointError> {
        let opened = open_checkpoint(model_store, checkpoint)?;
        let walked = walk(&opened)?;
        let mut size = CheckpointSize {
            weights_bytes: 0,
            file_count: walked.len() as u64,
            total_bytes: 0,
        };
        for file in &walked {
            size.total_bytes = size
                .total_bytes
                .checked_add(file.identity.size)
                .ok_or(CheckpointError::TooLarge)?;
            if is_weight(&file.path) {
                size.weights_bytes = i64::try_from(file.identity.size)
                    .ok()
                    .and_then(|bytes| size.weights_bytes.checked_add(bytes))
                    .ok_or(CheckpointError::TooLarge)?;
            }
        }
        Ok(size)
    }

    /// Measure the checkpoint: stat every file, reuse cached hashes of files
    /// whose identity is unchanged after rehashing every small file, and hash
    /// everything when anything differs or nothing is cached.
    pub fn measure(
        &self,
        model_store: &Path,
        checkpoint: &Path,
    ) -> Result<Verification, CheckpointError> {
        let opened = open_checkpoint(model_store, checkpoint)?;
        let key = opened.checkpoint.to_string_lossy().into_owned();
        let lock = {
            let mut locks = self.locks.lock().map_err(|_| CheckpointError::Io)?;
            locks.entry(key.clone()).or_default().clone()
        };
        let _serialized = lock.lock().map_err(|_| CheckpointError::Io)?;
        let walked = walk(&opened)?;
        if let Some(cached) = self.cached(&key) {
            if let Some(manifest) = reuse(&opened, &walked, &cached)? {
                return Ok(Verification {
                    manifest,
                    full_rehash: false,
                });
            }
        }
        let files: Vec<&Walked> = walked.iter().collect();
        let hashes = hash_files(&files)?;
        let record = CacheRecord {
            version: CACHE_VERSION,
            store: opened.store.to_string_lossy().into_owned(),
            checkpoint: key.clone(),
            root: opened.root,
            files: walked
                .iter()
                .zip(&hashes)
                .map(|(file, hash)| CachedFile {
                    path: file.path.clone(),
                    identity: file.identity,
                    sha256: hex::encode(hash),
                })
                .collect(),
        };
        let manifest = CheckpointManifest::from_entries(
            record
                .files
                .iter()
                .map(|file| ManifestEntry {
                    path: file.path.clone(),
                    size: file.identity.size,
                    sha256: file.sha256.clone(),
                })
                .collect(),
        )?;
        self.remember(&key, record);
        Ok(Verification {
            manifest,
            full_rehash: true,
        })
    }

    fn cache_file(&self, key: &str) -> Option<PathBuf> {
        self.cache_dir.as_ref().map(|dir| {
            dir.join(format!(
                "{}.json",
                hex::encode(Sha256::digest(key.as_bytes()))
            ))
        })
    }

    /// SPEC §13.3 / T37: the cache directory, only when it is private: a real
    /// directory (never a symlink) owned by this user with no group or other
    /// access. A missing one is created 0700 (its parent must exist); an owned
    /// one with wider access is narrowed. Anything else is not used at all.
    fn private_cache_dir(&self) -> Option<&Path> {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        let dir = self.cache_dir.as_deref()?;
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return None,
        }
        let metadata = std::fs::symlink_metadata(dir).ok()?;
        if !metadata.file_type().is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
            return None;
        }
        if metadata.mode() & 0o077 != 0 {
            // Another account may already have written here: nothing in it is
            // trusted, even once narrowed.
            return None;
        }
        Some(dir)
    }

    fn cached(&self, key: &str) -> Option<CacheRecord> {
        use std::io::Read;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        if let Some(record) = self.memory.lock().ok()?.get(key) {
            return Some(record.clone());
        }
        self.private_cache_dir()?;
        // Never through a symlink, and only a private regular file of ours.
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.cache_file(key)?)
            .ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.file_type().is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || metadata.len() > MAX_CACHE_RECORD_BYTES
        {
            return None;
        }
        let mut bytes = Vec::new();
        file.by_ref()
            .take(MAX_CACHE_RECORD_BYTES + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        let record: CacheRecord = serde_json::from_slice(&bytes).ok()?;
        (record.version == CACHE_VERSION && record.checkpoint == key).then_some(record)
    }

    /// Best effort: a cache that cannot be written only costs a full hash later.
    /// SPEC §13.3 / T37: written only into a private directory, through a new
    /// 0600 file created exclusively without following links, then renamed.
    fn remember(&self, key: &str, record: CacheRecord) {
        if let (Some(path), Some(dir)) = (self.cache_file(key), self.private_cache_dir()) {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let unique = format!(
                "{}.{}.{}.tmp",
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("record"),
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            );
            let temporary = dir.join(unique);
            let written = serde_json::to_vec(&record)
                .map_err(std::io::Error::other)
                .and_then(|bytes| {
                    let mut file = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                        .open(&temporary)?;
                    file.write_all(&bytes)?;
                    file.sync_all()?;
                    std::fs::rename(&temporary, &path)
                });
            if written.is_err() {
                let _ = std::fs::remove_file(&temporary);
            }
        }
        if let Ok(mut memory) = self.memory.lock() {
            memory.insert(key.to_owned(), record);
        }
    }
}

/// The opened store and checkpoint, with the canonical text containment was
/// checked on.
struct Opened {
    store: PathBuf,
    checkpoint: PathBuf,
    store_fd: Arc<OwnedFd>,
    checkpoint_fd: Arc<OwnedFd>,
    root: (u64, u64),
}

/// One file to measure: how to open it again (its parent's descriptor and its
/// leaf name) and the identity it had when walked.
struct Walked {
    path: String,
    parent: Arc<OwnedFd>,
    name: CString,
    identity: FileIdentity,
}

fn open_checkpoint(model_store: &Path, checkpoint: &Path) -> Result<Opened, CheckpointError> {
    if !model_store.is_absolute() || !checkpoint.is_absolute() {
        return Err(CheckpointError::InvalidRoot);
    }
    // The store's own location is the operator's trusted host configuration;
    // canonicalizing it once is the only resolution anything here does.
    let store = model_store
        .canonicalize()
        .map_err(|_| CheckpointError::InvalidRoot)?;
    let canonical = checkpoint
        .canonicalize()
        .map_err(|_| CheckpointError::InvalidRoot)?;
    let relative = canonical
        .strip_prefix(&store)
        .map_err(|_| CheckpointError::InvalidRoot)?;
    let parts: Vec<&std::ffi::OsStr> = relative
        .components()
        .map(|c| match c {
            Component::Normal(part) => Ok(part),
            _ => Err(CheckpointError::InvalidRoot),
        })
        .collect::<Result<_, _>>()?;
    if parts.is_empty() || store.to_str().is_none() || canonical.to_str().is_none() {
        return Err(CheckpointError::InvalidRoot);
    }
    let store_fd = Arc::new(open_chain_from_root(&store)?);
    let mut fd = store_fd.clone();
    for part in parts {
        let name = CString::new(part.as_bytes()).map_err(|_| CheckpointError::InvalidRoot)?;
        fd = Arc::new(
            open_directory(fd.as_raw_fd(), &name).map_err(|_| CheckpointError::InvalidRoot)?,
        );
    }
    let st = fstat(fd.as_raw_fd()).map_err(os_error)?;
    let identity = FileIdentity::of(&st);
    Ok(Opened {
        store,
        checkpoint: canonical,
        store_fd,
        checkpoint_fd: fd,
        root: (identity.device, identity.inode),
    })
}

/// Port of `_open_chain`: open an absolute path one component at a time from
/// `/`, never following a link.
fn open_chain_from_root(path: &Path) -> Result<OwnedFd, CheckpointError> {
    let root = CString::new("/").expect("static");
    let mut fd = open_at(
        libc::AT_FDCWD,
        &root,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
    )
    .map_err(|_| CheckpointError::InvalidRoot)?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(part) => {
                let name =
                    CString::new(part.as_bytes()).map_err(|_| CheckpointError::InvalidRoot)?;
                fd = open_directory(fd.as_raw_fd(), &name)
                    .map_err(|_| CheckpointError::InvalidRoot)?;
            }
            _ => return Err(CheckpointError::InvalidRoot),
        }
    }
    Ok(fd)
}

fn open_directory(dir: RawFd, name: &CStr) -> std::io::Result<OwnedFd> {
    open_at(
        dir,
        name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
    )
}

fn open_at(dir: RawFd, name: &CStr, flags: i32) -> std::io::Result<OwnedFd> {
    // SAFETY: `name` is a valid NUL-terminated string and the returned
    // descriptor is owned exactly once.
    let fd = unsafe { libc::openat(dir, name.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh descriptor this function owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn fstat(fd: RawFd) -> std::io::Result<libc::stat> {
    let mut st = MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: `st` is valid writable storage for one `stat`.
    if unsafe { libc::fstat(fd, st.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fstat succeeded and initialized it.
    Ok(unsafe { st.assume_init() })
}

fn lstat_at(dir: RawFd, name: &CStr) -> std::io::Result<libc::stat> {
    let mut st = MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: valid name and storage; no link is followed.
    if unsafe {
        libc::fstatat(
            dir,
            name.as_ptr(),
            st.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fstatat succeeded and initialized it.
    Ok(unsafe { st.assume_init() })
}

fn read_link_at(dir: RawFd, name: &CStr) -> Result<Vec<u8>, CheckpointError> {
    let mut buffer = vec![0u8; MAX_PATH_BYTES + 1];
    // SAFETY: the buffer is valid for its length.
    let length =
        unsafe { libc::readlinkat(dir, name.as_ptr(), buffer.as_mut_ptr().cast(), buffer.len()) };
    if length < 0 {
        return Err(os_error(std::io::Error::last_os_error()));
    }
    let length = length as usize;
    if length == 0 || length > MAX_PATH_BYTES {
        return Err(CheckpointError::UnsafeFile);
    }
    buffer.truncate(length);
    Ok(buffer)
}

/// The names in one open directory, without `.` and `..`.
fn list(dir: &OwnedFd) -> Result<Vec<Vec<u8>>, CheckpointError> {
    // SAFETY: duplicating an open descriptor; the duplicate is handed to
    // `fdopendir`, which owns it until `closedir`.
    let duplicate = unsafe { libc::fcntl(dir.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(CheckpointError::Io);
    }
    // SAFETY: `duplicate` is a valid directory descriptor.
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        // SAFETY: fdopendir failed, so the duplicate is still ours to close.
        unsafe { libc::close(duplicate) };
        return Err(CheckpointError::Io);
    }
    // SAFETY: `stream` is a valid directory stream.
    unsafe { libc::rewinddir(stream) };
    let mut names = Vec::new();
    let failed = loop {
        // SAFETY: errno is thread-local; clearing it tells end from error.
        unsafe { *libc::__errno_location() = 0 };
        // SAFETY: `stream` is valid until closedir below.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            // SAFETY: reading this thread's errno.
            break unsafe { *libc::__errno_location() } != 0;
        }
        // SAFETY: `d_name` is NUL-terminated within the entry.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            names.push(name.to_vec());
        }
        if names.len() > MAX_FILES + MAX_DIRECTORIES {
            break true;
        }
    };
    // SAFETY: closes the stream and its duplicated descriptor exactly once.
    unsafe { libc::closedir(stream) };
    if failed {
        return Err(CheckpointError::Io);
    }
    names.sort();
    Ok(names)
}

/// Lexically normalize an absolute path. Sound here only because every
/// component is then opened with `O_NOFOLLOW`: a link anywhere fails closed.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::ParentDir => {
                out.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    out
}

struct Walker<'a> {
    opened: &'a Opened,
    files: Vec<Walked>,
    directories: usize,
    /// Target directories opened for links, keyed by their store-relative path.
    targets: BTreeMap<PathBuf, Arc<OwnedFd>>,
}

fn walk(opened: &Opened) -> Result<Vec<Walked>, CheckpointError> {
    let mut walker = Walker {
        opened,
        files: Vec::new(),
        directories: 1,
        targets: BTreeMap::new(),
    };
    walker.directory(opened.checkpoint_fd.clone(), &opened.checkpoint, "", 0)?;
    let mut files = walker.files;
    files.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    Ok(files)
}

impl Walker<'_> {
    fn directory(
        &mut self,
        fd: Arc<OwnedFd>,
        canonical: &Path,
        prefix: &str,
        depth: usize,
    ) -> Result<(), CheckpointError> {
        if depth > MAX_DEPTH {
            return Err(CheckpointError::TooLarge);
        }
        for name in list(&fd)? {
            if name.first() == Some(&b'.') {
                continue;
            }
            let text = std::str::from_utf8(&name).map_err(|_| CheckpointError::UnsafeFile)?;
            let path = if prefix.is_empty() {
                text.to_owned()
            } else {
                format!("{prefix}/{text}")
            };
            if !encodable(&path) {
                return Err(CheckpointError::UnsafeFile);
            }
            let leaf = CString::new(name.clone()).map_err(|_| CheckpointError::UnsafeFile)?;
            let st = lstat_at(fd.as_raw_fd(), &leaf).map_err(os_error)?;
            match st.st_mode & libc::S_IFMT {
                libc::S_IFDIR => {
                    self.directories += 1;
                    if self.directories > MAX_DIRECTORIES {
                        return Err(CheckpointError::TooLarge);
                    }
                    let child = open_directory(fd.as_raw_fd(), &leaf).map_err(os_error)?;
                    let opened = FileIdentity::of(&fstat(child.as_raw_fd()).map_err(os_error)?);
                    let seen = FileIdentity::of(&st);
                    if (opened.device, opened.inode) != (seen.device, seen.inode) {
                        return Err(CheckpointError::Changed);
                    }
                    self.directory(Arc::new(child), &canonical.join(text), &path, depth + 1)?;
                }
                libc::S_IFREG => self.push(path, fd.clone(), leaf, FileIdentity::of(&st))?,
                libc::S_IFLNK => {
                    // ADR 0014 §7 (A5): each hop resolves against the directory holding the
                    // link and must stay inside the store; the chain ends at a regular file.
                    let mut holder = canonical.to_path_buf();
                    let mut target = read_link_at(fd.as_raw_fd(), &leaf)?;
                    let mut hops = 1;
                    loop {
                        let resolved = normalize(
                            &holder.join(Path::new(std::ffi::OsStr::from_bytes(&target))),
                        );
                        let (parent, name) = self.link_target(&resolved)?;
                        let st = lstat_at(parent.as_raw_fd(), &name).map_err(os_error)?;
                        match st.st_mode & libc::S_IFMT {
                            libc::S_IFREG => {
                                self.push(path, parent, name, FileIdentity::of(&st))?;
                                break;
                            }
                            libc::S_IFLNK if hops < MAX_LINK_HOPS => {
                                target = read_link_at(parent.as_raw_fd(), &name)?;
                                holder = resolved
                                    .parent()
                                    .ok_or(CheckpointError::UnsafeFile)?
                                    .to_path_buf();
                                hops += 1;
                            }
                            _ => return Err(CheckpointError::UnsafeFile),
                        }
                    }
                }
                _ => return Err(CheckpointError::UnsafeFile),
            }
        }
        Ok(())
    }

    fn push(
        &mut self,
        path: String,
        parent: Arc<OwnedFd>,
        name: CString,
        identity: FileIdentity,
    ) -> Result<(), CheckpointError> {
        if self.files.len() >= MAX_FILES {
            return Err(CheckpointError::TooLarge);
        }
        self.files.push(Walked {
            path,
            parent,
            name,
            identity,
        });
        Ok(())
    }

    /// Open the directory holding a link's target, which must lie inside the
    /// model store, through descriptors from the store's own.
    fn link_target(&mut self, resolved: &Path) -> Result<(Arc<OwnedFd>, CString), CheckpointError> {
        let relative = resolved
            .strip_prefix(&self.opened.store)
            .map_err(|_| CheckpointError::UnsafeFile)?;
        let leaf = relative.file_name().ok_or(CheckpointError::UnsafeFile)?;
        let leaf = CString::new(leaf.as_bytes()).map_err(|_| CheckpointError::UnsafeFile)?;
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        if let Some(fd) = self.targets.get(parent) {
            return Ok((fd.clone(), leaf));
        }
        let mut fd = self.opened.store_fd.clone();
        for component in parent.components() {
            let Component::Normal(part) = component else {
                return Err(CheckpointError::UnsafeFile);
            };
            let name = CString::new(part.as_bytes()).map_err(|_| CheckpointError::UnsafeFile)?;
            fd = Arc::new(open_directory(fd.as_raw_fd(), &name).map_err(os_error)?);
        }
        self.directories += 1;
        if self.directories > MAX_DIRECTORIES {
            return Err(CheckpointError::TooLarge);
        }
        self.targets.insert(parent.to_path_buf(), fd.clone());
        Ok((fd, leaf))
    }
}

/// Reuse the cached hashes when the store, the checkpoint directory, the file
/// set and every file's identity are unchanged and every small file still
/// hashes to its cached value. `None` means a full rehash is required.
fn reuse(
    opened: &Opened,
    walked: &[Walked],
    cached: &CacheRecord,
) -> Result<Option<CheckpointManifest>, CheckpointError> {
    if cached.store != opened.store.to_string_lossy()
        || cached.root != opened.root
        || cached.files.len() != walked.len()
        || cached
            .files
            .iter()
            .zip(walked)
            .any(|(cached, file)| cached.path != file.path || cached.identity != file.identity)
    {
        return Ok(None);
    }
    let small: Vec<usize> = walked
        .iter()
        .enumerate()
        .filter(|(_, file)| file.identity.size <= SMALL_FILE_LIMIT)
        .map(|(index, _)| index)
        .collect();
    let files: Vec<&Walked> = small.iter().map(|index| &walked[*index]).collect();
    let hashes = match hash_files(&files) {
        Ok(hashes) => hashes,
        // A file that changed under the check is not reusable; hash in full.
        Err(CheckpointError::Changed) => return Ok(None),
        Err(error) => return Err(error),
    };
    if small
        .iter()
        .zip(&hashes)
        .any(|(index, hash)| cached.files[*index].sha256 != hex::encode(hash))
    {
        return Ok(None);
    }
    Ok(Some(CheckpointManifest::from_entries(
        cached
            .files
            .iter()
            .map(|file| ManifestEntry {
                path: file.path.clone(),
                size: file.identity.size,
                sha256: file.sha256.clone(),
            })
            .collect(),
    )?))
}

/// Hash files in parallel, largest first. Each file is opened from its
/// parent's descriptor without following links, and must keep exactly the
/// identity it was walked with from before the first read to after the last.
fn hash_files(files: &[&Walked]) -> Result<Vec<[u8; 32]>, CheckpointError> {
    let mut order: Vec<usize> = (0..files.len()).collect();
    order.sort_by_key(|index| std::cmp::Reverse(files[*index].identity.size));
    let results: Vec<Mutex<Option<[u8; 32]>>> = files.iter().map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let error = Mutex::new(None);
    let workers = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(MAX_WORKERS)
        .min(files.len())
        .max(1);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                if failed.load(Ordering::Acquire) {
                    return;
                }
                let slot = next.fetch_add(1, Ordering::AcqRel);
                let Some(index) = order.get(slot).copied() else {
                    return;
                };
                match hash_one(files[index]) {
                    Ok(hash) => {
                        if let Ok(mut result) = results[index].lock() {
                            *result = Some(hash);
                        }
                    }
                    Err(cause) => {
                        failed.store(true, Ordering::Release);
                        if let Ok(mut first) = error.lock() {
                            first.get_or_insert(cause);
                        }
                        return;
                    }
                }
            });
        }
    });
    if let Some(cause) = error.into_inner().map_err(|_| CheckpointError::Io)? {
        return Err(cause);
    }
    results
        .into_iter()
        .map(|slot| slot.into_inner().ok().flatten().ok_or(CheckpointError::Io))
        .collect()
}

fn hash_one(file: &Walked) -> Result<[u8; 32], CheckpointError> {
    let fd = open_at(
        file.parent.as_raw_fd(),
        &file.name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
    )
    .map_err(os_error)?;
    let before = fstat(fd.as_raw_fd()).map_err(os_error)?;
    if before.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(CheckpointError::UnsafeFile);
    }
    if FileIdentity::of(&before) != file.identity {
        return Err(CheckpointError::Changed);
    }
    let mut reader = std::fs::File::from(fd);
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; CHUNK];
    let mut remaining = file.identity.size;
    while remaining > 0 {
        let want = usize::try_from(remaining.min(CHUNK as u64)).map_err(|_| CheckpointError::Io)?;
        let read = reader.read(&mut buffer[..want]).map_err(os_error)?;
        if read == 0 {
            return Err(CheckpointError::Changed);
        }
        digest.update(&buffer[..read]);
        remaining -= read as u64;
    }
    let after = fstat(reader.as_raw_fd()).map_err(os_error)?;
    if FileIdentity::of(&after) != file.identity {
        return Err(CheckpointError::Changed);
    }
    Ok(digest.finalize().into())
}

#[cfg(test)]
mod tests;

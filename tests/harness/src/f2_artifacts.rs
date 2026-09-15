//! Descriptor-relative private artifact storage for Linux qualification runs.
//!
//! The caller supplies an already-open, trusted private parent directory and
//! reviewed metadata. This module does not redact bytes, validate a manifest,
//! authorize effects or certify run completion. Same-user malicious filesystem
//! mutation is outside this boundary. Failed artifacts are retained, not deleted.
use std::{
    ffi::CString,
    fs::File,
    io::{self, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::MetadataExt,
    },
    sync::{Arc, Mutex},
};
#[derive(Debug, PartialEq, Eq)]
pub enum ArtifactError {
    Invalid,
    Exists,
    Io,
    Stopped,
}
pub enum Artifact {
    Manifest,
    Requests,
    Results,
}
struct Budget {
    remaining: u64,
    stopped: bool,
}
pub struct ArtifactDirectory {
    directory: Arc<File>,
    budget: Arc<Mutex<Budget>>,
}
/// Append-only interface with no raw descriptor or seek access. All files in a
/// directory share one cap. Dropping a handle does not sync or remove evidence.
pub struct ArtifactFile {
    file: File,
    directory: Arc<File>,
    budget: Arc<Mutex<Budget>>,
}

fn private(file: &File, directory: bool) -> bool {
    let Ok(m) = file.metadata() else {
        return false;
    };
    // SAFETY: geteuid takes no pointers and does not mutate Rust memory.
    m.uid() == unsafe { libc::geteuid() }
        && if directory {
            m.is_dir() && m.mode() & 0o7777 == 0o700
        } else {
            m.is_file() && m.mode() & 0o7777 == 0o600 && m.nlink() == 1
        }
}

fn last_error() -> ArtifactError {
    if io::Error::last_os_error().kind() == io::ErrorKind::AlreadyExists {
        ArtifactError::Exists
    } else {
        ArtifactError::Io
    }
}

impl ArtifactDirectory {
    /// Create exclusively in a current-user mode0700 parent. Components are
    /// bounded ASCII letters/digits/hyphens/underscores, never paths. Sync the
    /// new directory and its parent before returning; retain on any failure.
    pub fn create(parent: &File, name: &str, budget: u64) -> Result<Self, ArtifactError> {
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            || budget == 0
            || budget > 100 * 1024 * 1024
            || !private(parent, true)
        {
            return Err(ArtifactError::Invalid);
        }
        let name = CString::new(name).map_err(|_| ArtifactError::Invalid)?;
        // SAFETY: parent remains open and name is a live, NUL-terminated string.
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
            return Err(last_error());
        }
        // SAFETY: same valid descriptor/string; no following links or inheritance.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(last_error());
        }
        // SAFETY: successful openat returned a new descriptor owned only here.
        let directory = unsafe { File::from_raw_fd(fd) };
        if !private(&directory, true) || !private(parent, true) {
            return Err(ArtifactError::Invalid);
        }
        directory.sync_all().map_err(|_| ArtifactError::Io)?;
        parent.sync_all().map_err(|_| ArtifactError::Io)?;
        Ok(Self {
            directory: Arc::new(directory),
            budget: Arc::new(Mutex::new(Budget {
                remaining: budget,
                stopped: false,
            })),
        })
    }

    /// Fixed names prevent traversal; exclusive creation prevents overwrites.
    /// A creation failure stops existing writers as well. No automatic retry.
    pub fn create_file(&self, artifact: Artifact) -> Result<ArtifactFile, ArtifactError> {
        let mut budget = self.budget.lock().map_err(|_| ArtifactError::Stopped)?;
        if budget.stopped {
            return Err(ArtifactError::Stopped);
        }
        budget.stopped = true;
        if !private(&self.directory, true) {
            return Err(ArtifactError::Invalid);
        }
        let name = match artifact {
            Artifact::Manifest => c"manifest.json",
            Artifact::Requests => c"requests.jsonl",
            Artifact::Results => c"results.json",
        };
        // SAFETY: directory remains open and name is a static NUL-terminated string.
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(last_error());
        }
        // SAFETY: successful openat returned a fresh descriptor owned only here.
        let file = unsafe { File::from_raw_fd(fd) };
        if !private(&file, false) {
            return Err(ArtifactError::Invalid);
        }
        budget.stopped = false;
        Ok(ArtifactFile {
            file,
            directory: self.directory.clone(),
            budget: self.budget.clone(),
        })
    }
}
impl ArtifactFile {
    /// Sync this file's bytes and directory entry. Every other file needs its
    /// own sync; this is not a whole-run completion marker.
    pub fn sync(&self) -> Result<(), ArtifactError> {
        let mut budget = self.budget.lock().map_err(|_| ArtifactError::Stopped)?;
        if budget.stopped {
            return Err(ArtifactError::Stopped);
        }
        budget.stopped = true;
        if !private(&self.directory, true) || !private(&self.file, false) {
            return Err(ArtifactError::Invalid);
        }
        self.file.sync_all().map_err(|_| ArtifactError::Io)?;
        self.directory.sync_all().map_err(|_| ArtifactError::Io)?;
        budget.stopped = false;
        Ok(())
    }
}
impl Write for ArtifactFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut budget = self
            .budget
            .lock()
            .map_err(|_| io::Error::other("artifact_stopped"))?;
        if budget.stopped {
            return Err(io::Error::other("artifact_stopped"));
        }
        budget.stopped = true;
        if !private(&self.directory, true) || !private(&self.file, false) {
            return Err(io::Error::other("artifact_invalid"));
        }
        if bytes.len() as u64 > budget.remaining {
            return Err(io::Error::other("artifact_limit"));
        }
        // Keep the shared lock during I/O. On any partial failure, retain the
        // prefix and the stopped state; no other handle can spend that budget.
        self.file
            .write_all(bytes)
            .map_err(|_| io::Error::other("artifact_io"))?;
        budget.remaining -= bytes.len() as u64;
        budget.stopped = false;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        // File has no userspace buffer. Use sync for durable persistence.
        let budget = self
            .budget
            .lock()
            .map_err(|_| io::Error::other("artifact_stopped"))?;
        if budget.stopped {
            Err(io::Error::other("artifact_stopped"))
        } else {
            Ok(())
        }
    }
}

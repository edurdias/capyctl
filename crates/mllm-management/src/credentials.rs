//! Read-only protected service file resolution. No secret-bearing Debug types.
use crate::ManagementCredentials;
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

const DENIED: &str = "invalid management credentials";

impl ManagementCredentials {
    /// Resolve two explicit, independent service-owned credential files.
    ///
    /// Paths must be absolute, canonical UTF-8 (at most 4096 bytes), without
    /// aliases. Every ancestor must be root/service owned and not group/world
    /// writable. Files must be service owned, regular, single-link, exactly
    /// mode 0600, containing 32–256 bearer characters and optionally ONE LF
    /// (at most 257 bytes). No trimming, fallback, creation or repair occurs.
    /// Both retained files and all ancestors are revalidated after both reads.
    ///
    /// The service supplies paths, never HTTP/deployment input. This checks
    /// provenance at resolution time, not entropy or future rotation. Root and
    /// the service UID are trusted; this is not protection from either actor
    /// or a hostile filesystem/kernel. Raw tokens are temporary local buffers;
    /// only the management digest survives in the returned object.
    pub fn from_protected_files(management: &Path, inference: &Path) -> Result<Self, &'static str> {
        let management = ProtectedToken::read(management).map_err(|_| DENIED)?;
        let inference = ProtectedToken::read(inference).map_err(|_| DENIED)?;
        let result = Self::from_trusted_resolver(management.token()?, inference.token()?)?;
        management.revalidate().map_err(|_| DENIED)?;
        inference.revalidate().map_err(|_| DENIED)?;
        Ok(result)
    }
}

struct Observed {
    path: PathBuf,
    file: File,
    metadata: Metadata,
}
struct ProtectedToken {
    leaf: Observed,
    ancestors: Vec<Observed>,
    bytes: Vec<u8>,
}
impl ProtectedToken {
    fn read(path: &Path) -> Result<Self, ()> {
        let text = path.to_str().ok_or(())?;
        if !path.is_absolute()
            || text.len() > 4096
            || text.bytes().any(|b| b.is_ascii_control())
            || text
                .split('/')
                .skip(1)
                .any(|part| part.is_empty() || part == "." || part == "..")
            || fs::canonicalize(path).map_err(|_| ())? != path
        {
            return Err(());
        }
        // UID is read from the running service, never caller-supplied.
        let uid = unsafe { libc::geteuid() };
        let mut ancestors = Vec::new();
        for parent in path.ancestors().skip(1) {
            let observed = observe(parent, true)?;
            let m = &observed.metadata;
            if !m.is_dir() || (m.uid() != 0 && m.uid() != uid) || m.mode() & 0o022 != 0 {
                return Err(());
            }
            ancestors.push(observed);
        }
        let mut leaf = observe(path, false)?;
        let m = &leaf.metadata;
        if !m.is_file()
            || m.uid() != uid
            || m.nlink() != 1
            || m.mode() & 0o7777 != 0o600
            || m.len() > 257
        {
            return Err(());
        }
        let mut bytes = Vec::with_capacity(258);
        (&mut leaf.file)
            .take(258)
            .read_to_end(&mut bytes)
            .map_err(|_| ())?;
        if bytes.len() > 257 || bytes.len() as u64 != leaf.metadata.len() {
            return Err(());
        }
        let value = Self {
            leaf,
            ancestors,
            bytes,
        };
        value.revalidate()?;
        Ok(value)
    }
    fn token(&self) -> Result<&str, &'static str> {
        let bytes = self.bytes.strip_suffix(b"\n").unwrap_or(&self.bytes);
        std::str::from_utf8(bytes).map_err(|_| DENIED)
    }
    fn revalidate(&self) -> Result<(), ()> {
        for observed in self.ancestors.iter().chain(std::iter::once(&self.leaf)) {
            if !same(
                &observed.metadata,
                &observed.file.metadata().map_err(|_| ())?,
            ) || !same(
                &observed.metadata,
                &fs::symlink_metadata(&observed.path).map_err(|_| ())?,
            ) {
                return Err(());
            }
        }
        if fs::canonicalize(&self.leaf.path).map_err(|_| ())? != self.leaf.path {
            return Err(());
        }
        Ok(())
    }
}
fn observe(path: &Path, directory: bool) -> Result<Observed, ()> {
    let before = fs::symlink_metadata(path).map_err(|_| ())?;
    if (directory && !before.is_dir()) || (!directory && !before.is_file()) {
        return Err(());
    }
    let flags = libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | libc::O_CLOEXEC
        | if directory { libc::O_DIRECTORY } else { 0 };
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)
        .map_err(|_| ())?;
    let metadata = file.metadata().map_err(|_| ())?;
    if !same(&before, &metadata) {
        return Err(());
    }
    Ok(Observed {
        path: path.to_owned(),
        file,
        metadata,
    })
}
fn same(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.mode() == b.mode()
        && a.uid() == b.uid()
        && a.gid() == b.gid()
        // Unrelated sibling creation must not invalidate an otherwise unchanged
        // protected ancestor. Leaf files retain full content-change checks.
        && (a.is_dir()
            || (a.nlink() == b.nlink()
                && a.len() == b.len()
                && a.mtime() == b.mtime()
                && a.mtime_nsec() == b.mtime_nsec()
                && a.ctime() == b.ctime()
                && a.ctime_nsec() == b.ctime_nsec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::{fd::AsRawFd, unix::fs::PermissionsExt};

    #[test]
    fn retained_file_detects_content_mode_and_path_replacement_and_has_safe_flags() {
        let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("token");
        fs::write(&path, "a".repeat(32)).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let original = ProtectedToken::read(&path).unwrap();
        let fd = original.leaf.file.as_raw_fd();
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFL) } & libc::O_NONBLOCK,
            0
        );
        fs::write(&path, "b".repeat(33)).unwrap();
        assert!(original.revalidate().is_err());
        let original = ProtectedToken::read(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(original.revalidate().is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let original = ProtectedToken::read(&path).unwrap();
        fs::rename(&path, dir.path().join("old")).unwrap();
        fs::write(&path, "b".repeat(33)).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(original.revalidate().is_err());
    }
}

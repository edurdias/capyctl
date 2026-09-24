use std::fs::File;
use std::io;
use std::path::Path;

/// Exclusive controller-process ownership held for this guard's lifetime.
pub struct ControllerLock {
    _file: nix::fcntl::Flock<File>,
}

impl ControllerLock {
    pub fn acquire(path: &Path) -> io::Result<Self> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

        let reject = || {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe controller lock path",
            )
        };
        let text = path.to_str().ok_or_else(reject)?;
        if !text.starts_with('/')
            || text.len() > 4096
            || text.chars().any(char::is_control)
            || text[1..]
                .split('/')
                .any(|part| matches!(part, "" | "." | ".."))
        {
            return Err(reject());
        }
        let parent = path.parent().ok_or_else(reject)?;
        if parent.canonicalize()? != parent {
            return Err(reject());
        }
        let service_uid = std::fs::metadata("/proc/self")?.uid();
        for component in parent.ancestors() {
            let metadata = std::fs::symlink_metadata(component)?;
            if !metadata.is_dir()
                || ![0, service_uid].contains(&metadata.uid())
                || metadata.mode() & 0o022 != 0
            {
                return Err(reject());
            }
        }

        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || ![0, service_uid].contains(&metadata.uid())
            || metadata.mode() & 0o022 != 0
        {
            return Err(reject());
        }
        let locked = nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock)
            .map_err(|(_, error)| io::Error::from_raw_os_error(error as i32))?;
        let retained = std::fs::symlink_metadata(path)?;
        if retained.dev() != metadata.dev() || retained.ino() != metadata.ino() {
            return Err(reject());
        }
        Ok(Self { _file: locked })
    }
}

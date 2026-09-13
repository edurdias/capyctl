use std::fs::File;
use std::io;
use std::path::Path;

/// Exclusive controller-process ownership held for this guard's lifetime.
pub struct ControllerLock {
    _file: nix::fcntl::Flock<File>,
}

impl ControllerLock {
    pub fn acquire(path: &Path) -> io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;

        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        let locked = nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock)
            .map_err(|(_, error)| io::Error::from_raw_os_error(error as i32))?;
        Ok(Self { _file: locked })
    }
}

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

        // Found live 2026-10-03: the refusal names the path and the rule it
        // breaks, so a state directory under `/tmp` or under a directory made
        // with umask 002 says what to change.
        let reject = |at: &Path, why: &str| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("unsafe controller lock path {}: {why}", at.display()),
            )
        };
        let text = path
            .to_str()
            .ok_or_else(|| reject(path, "the path is not valid UTF-8"))?;
        if !text.starts_with('/')
            || text.len() > 4096
            || text.chars().any(char::is_control)
            || text[1..]
                .split('/')
                .any(|part| matches!(part, "" | "." | ".."))
        {
            return Err(reject(path, "the path must be absolute and normalized"));
        }
        let parent = path
            .parent()
            .ok_or_else(|| reject(path, "the path has no parent directory"))?;
        if parent.canonicalize()? != parent {
            return Err(reject(
                parent,
                "the directory is reached through a symbolic link; name its real path",
            ));
        }
        let service_uid = std::fs::metadata("/proc/self")?.uid();
        for component in parent.ancestors() {
            let metadata = std::fs::symlink_metadata(component)?;
            if !metadata.is_dir() {
                return Err(reject(component, "not a directory"));
            }
            if ![0, service_uid].contains(&metadata.uid()) {
                return Err(reject(
                    component,
                    "owned by another user; capyctl keeps its state only under directories \
                     owned by you or root",
                ));
            }
            if metadata.mode() & 0o022 != 0 {
                return Err(reject(
                    component,
                    &format!(
                        "writable by group or other users (mode {:o}); capyctl keeps its \
                         state only under directories only you or root can write: use a \
                         state directory elsewhere (the default is ~/.local/state/capyctl), \
                         or remove group and other write from this directory",
                        metadata.mode() & 0o7777
                    ),
                ));
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
            return Err(reject(
                path,
                "not a single-link regular file owned by you that only you can write",
            ));
        }
        let locked = nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock)
            .map_err(|(_, error)| io::Error::from_raw_os_error(error as i32))?;
        let retained = std::fs::symlink_metadata(path)?;
        if retained.dev() != metadata.dev() || retained.ino() != metadata.ino() {
            return Err(reject(path, "the file was replaced while it was opened"));
        }
        Ok(Self { _file: locked })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    // T37 (found live 2026-10-03): a lock under a directory others may write
    // is refused naming that directory and the rule, not a bare "unsafe path".
    #[test]
    fn an_unsafe_lock_path_names_the_directory_and_the_rule() {
        let root = tempfile::tempdir_in(std::env::var_os("HOME").unwrap_or_default()).unwrap();
        let shared = root.path().join("shared");
        let state = shared.join("state");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o775)).unwrap();
        let error = ControllerLock::acquire(&state.join("controller.lock"))
            .err()
            .expect("refused");
        let text = error.to_string();
        assert!(text.contains(&shared.display().to_string()), "{text}");
        assert!(
            text.contains("writable by group or other users (mode 775)"),
            "{text}"
        );
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
        ControllerLock::acquire(&state.join("controller.lock")).expect("a private path locks");
    }
}

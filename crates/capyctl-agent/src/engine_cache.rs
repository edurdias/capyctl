//! ADR 0023 §3, SPEC §13.3: TensorFold builds CUDA extensions into
//! `TORCH_EXTENSIONS_DIR` and loads them on every later start, so whoever can
//! write that directory can run code in the engine. Each version gets its own
//! directory under the role's private state, owned by the service user, mode
//! 0700 at every level capyctl creates; anything else found there is refused.
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineCacheError {
    #[error("the build fingerprint is not a plain path component")]
    Fingerprint,
    #[error("{0} is not a private directory of the service user")]
    NotPrivate(PathBuf),
    #[error("{0} could not be created")]
    Create(PathBuf),
}

/// The role's private engine cache root (`<state>/engines`, 0700).
#[derive(Clone, Debug)]
pub struct EngineCacheRoot {
    dir: PathBuf,
}

fn component(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
}

fn private(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| {
        meta.is_dir() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o7777 == 0o700
    })
}

fn ensure(path: &Path) -> Result<(), EngineCacheError> {
    if std::fs::symlink_metadata(path).is_err() {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|_| EngineCacheError::Create(path.to_path_buf()))?;
    }
    private(path)
        .then_some(())
        .ok_or_else(|| EngineCacheError::NotPrivate(path.to_path_buf()))
}

impl EngineCacheRoot {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// `<root>/tensorfold/<fingerprint>/torch_extensions`, created private.
    pub fn torch_extensions(&self, fingerprint: &str) -> Result<PathBuf, EngineCacheError> {
        if !component(fingerprint) {
            return Err(EngineCacheError::Fingerprint);
        }
        ensure(&self.dir)?;
        let mut path = self.dir.clone();
        for part in ["tensorfold", fingerprint, "torch_extensions"] {
            path.push(part);
            ensure(&path)?;
        }
        Ok(path)
    }
}

/// ADR 0023 §4: whether an earlier start finished a build here. torch's
/// `cpp_extension` links `<dir>/<name>/<name>.so` and holds `<name>/lock`
/// while it builds, so an extension counts only once linked and unlocked; a
/// start killed mid build leaves the directory and its objects behind.
pub fn has_build(dir: &Path) -> bool {
    let finished = |extension: &Path| {
        let Some(name) = extension.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        std::fs::symlink_metadata(extension.join(format!("{name}.so"))).is_ok_and(|m| m.is_file())
            && std::fs::symlink_metadata(extension.join("lock")).is_err()
    };
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            entry.file_type().is_ok_and(|kind| kind.is_dir()) && finished(&entry.path())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn root() -> (tempfile::TempDir, EngineCacheRoot) {
        let dir = tempfile::tempdir().unwrap();
        let engines = dir.path().join("engines");
        std::fs::create_dir(&engines).unwrap();
        std::fs::set_permissions(&engines, std::fs::Permissions::from_mode(0o700)).unwrap();
        (dir, EngineCacheRoot::new(engines))
    }

    // T41 T37 (ADR 0023 §3): the extensions directory is created private on
    // every level and reused.
    #[test]
    fn the_extensions_directory_is_private_and_per_version() {
        let (_dir, root) = root();
        let path = root.torch_extensions("0.6.0").unwrap();
        assert!(path.ends_with("tensorfold/0.6.0/torch_extensions"));
        for level in [
            path.parent().unwrap().parent().unwrap(),
            path.parent().unwrap(),
            &path,
        ] {
            let meta = std::fs::symlink_metadata(level).unwrap();
            assert_eq!(meta.mode() & 0o7777, 0o700, "{}", level.display());
            assert_eq!(meta.uid(), unsafe { libc::geteuid() });
        }
        assert_eq!(root.torch_extensions("0.6.0").unwrap(), path);
        assert!(!has_build(&path));
    }

    // T41 (ADR 0023 §4): only a finished build counts. torch's
    // `cpp_extension` builds `<dir>/<name>/<name>.so`; a start killed mid
    // build leaves the directory, its `lock`, `build.ninja` and objects.
    #[test]
    fn only_a_finished_build_counts() {
        let (_dir, root) = root();
        let path = root.torch_extensions("0.6.0").unwrap();
        let extension = path.join("tensorfold_qmm_v3");
        std::fs::create_dir(&extension).unwrap();
        assert!(!has_build(&path), "an empty extension directory");
        for partial in ["lock", "build.ninja", "qmm.cuda.o"] {
            std::fs::write(extension.join(partial), "").unwrap();
        }
        std::fs::write(path.join("lock"), "").unwrap();
        assert!(!has_build(&path), "a build that never linked");
        std::fs::write(extension.join("tensorfold_qmm_v3.so"), "ELF").unwrap();
        assert!(!has_build(&path), "linked, but a rebuild holds the lock");
        std::fs::remove_file(extension.join("lock")).unwrap();
        assert!(has_build(&path));
        std::fs::create_dir(path.join("other")).unwrap();
        std::fs::create_dir(path.join("other/other.so")).unwrap();
        assert!(has_build(&path), "one finished extension is a build");
    }

    // T41 T37: a fingerprint is a path component, never a path; a widened mode or
    // a symlink anywhere on the way is refused, not repaired.
    #[test]
    fn unsafe_fingerprints_and_directories_are_refused() {
        let (dir, root) = root();
        for bad in ["", "..", "a/b", "0.6.0/../x", &"x".repeat(200)] {
            assert!(root.torch_extensions(bad).is_err(), "{bad:?}");
        }
        let path = root.torch_extensions("0.6.0").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(root.torch_extensions("0.6.0").is_err());
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.path().join("engines/tensorfold/0.7.0"))
            .unwrap();
        assert!(root.torch_extensions("0.7.0").is_err());
    }
}

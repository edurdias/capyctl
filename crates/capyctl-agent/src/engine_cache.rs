//! ADR 0023 §3, SPEC §13.3: TensorFold builds CUDA extensions into
//! `TORCH_EXTENSIONS_DIR` and loads them on every later start, so whoever can
//! write that directory can run code in the engine. Each version gets its own
//! directory under the role's private state, owned by the service user, mode
//! 0700 at every level capyctl creates; anything else found there is refused.
//! ADR 0029 §6: llama.cpp's configuration and cache directories live here too,
//! under the same rule.
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

use capyctl_domain::completion::ProcessIdentity;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineCacheError {
    #[error("the build fingerprint is not a plain path component")]
    Fingerprint,
    #[error("{0} is not a private directory of the service user")]
    NotPrivate(PathBuf),
    #[error("{0} could not be created")]
    Create(PathBuf),
    /// ADR 0029 §6: llama-server would read a `config.ini` found here.
    #[error("{0} is not empty")]
    NotEmpty(PathBuf),
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

    /// ADR 0029 §6: `<root>/llamacpp/config` and `<root>/llamacpp/cache`,
    /// created private, for llama-server's `XDG_CONFIG_HOME` and
    /// `LLAMA_CACHE`. The configuration directory must be empty: llama-server
    /// reads `llama.cpp/config.ini` under it for every option the command
    /// line leaves unset. Nothing writes the cache with CapyCTL's options
    /// (model sources are reserved), so it is only kept private.
    pub fn llamacpp_dirs(
        &self,
    ) -> Result<capyctl_adapters::llamacpp::LlamacppDirs, EngineCacheError> {
        ensure(&self.dir)?;
        let engine = self.dir.join("llamacpp");
        ensure(&engine)?;
        let (config, cache) = (engine.join("config"), engine.join("cache"));
        for dir in [&config, &cache] {
            ensure(dir)?;
        }
        let empty = std::fs::read_dir(&config)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false);
        if !empty {
            return Err(EngineCacheError::NotEmpty(config));
        }
        Ok(capyctl_adapters::llamacpp::LlamacppDirs { config, cache })
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

/// ADR 0023 §3: remove the `<name>/lock` files a killed build left in this
/// version's private `dir`, so the next start does not wait on them forever.
/// torch's `cpp_extension` holds that file while it builds and nothing else
/// records who holds it, so the only evidence that no build is running is
/// that no recorded process of any launch this host still retains may be
/// running: `recorded` is every such process, and unless each one is proved
/// gone nothing is removed (a concurrent start of the same version shares the
/// directory, and its lock must stand). Only plain files named `lock` one
/// level down are removed. Returns how many were.
pub fn clear_stale_locks(dir: &Path, recorded: &[ProcessIdentity]) -> usize {
    use capyctl_launchers::process_absence::{presence, Presence};
    if !private(dir) || recorded.iter().any(|p| presence(p) != Presence::Gone) {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path().join("lock"))
        .filter(|lock| std::fs::symlink_metadata(lock).is_ok_and(|m| m.is_file()))
        .filter(|lock| std::fs::remove_file(lock).is_ok())
        .count()
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

    // T42 T37 (ADR 0029 §6): llama.cpp's configuration and cache directories
    // are private on every level and reused; a configuration directory that
    // holds anything, or one opened to others, is refused.
    #[test]
    fn the_llamacpp_directories_are_private_and_the_config_one_empty() {
        let (_dir, root) = root();
        let dirs = root.llamacpp_dirs().unwrap();
        assert!(dirs.config.ends_with("engines/llamacpp/config"));
        assert!(dirs.cache.ends_with("engines/llamacpp/cache"));
        for level in [dirs.config.parent().unwrap(), &dirs.config, &dirs.cache] {
            let meta = std::fs::symlink_metadata(level).unwrap();
            assert_eq!(meta.mode() & 0o7777, 0o700, "{}", level.display());
            assert_eq!(meta.uid(), unsafe { libc::geteuid() });
        }
        assert_eq!(root.llamacpp_dirs().unwrap(), dirs);
        std::fs::create_dir(dirs.config.join("llama.cpp")).unwrap();
        std::fs::write(dirs.config.join("llama.cpp/config.ini"), "[*]\n").unwrap();
        assert_eq!(
            root.llamacpp_dirs(),
            Err(EngineCacheError::NotEmpty(dirs.config.clone()))
        );
        std::fs::remove_dir_all(dirs.config.join("llama.cpp")).unwrap();
        std::fs::set_permissions(&dirs.cache, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            root.llamacpp_dirs(),
            Err(EngineCacheError::NotPrivate(dirs.cache.clone()))
        );
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

    fn own_identity() -> ProcessIdentity {
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
        let close = stat.rfind(')').unwrap();
        ProcessIdentity {
            role: "api".into(),
            pid: std::process::id(),
            boot_id: std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .unwrap()
                .trim()
                .to_owned(),
            start_ticks: stat[close + 2..]
                .split_whitespace()
                .nth(19)
                .unwrap()
                .parse()
                .unwrap(),
        }
    }

    // T41 (ADR 0023 §3, found live 2026-10-02): a build killed mid way leaves
    // `<name>/lock`, and the next start waits on it forever. With no recorded
    // process of a retained launch still possibly running, the locks go and
    // nothing else does.
    #[test]
    fn a_killed_builds_lock_is_cleared_when_no_launch_may_run() {
        let (_dir, root) = root();
        let path = root.torch_extensions("0.6.1").unwrap();
        let extension = path.join("tensorfold_qmm_v5");
        std::fs::create_dir(&extension).unwrap();
        for file in ["lock", "build.ninja", "qmm.cuda.o"] {
            std::fs::write(extension.join(file), "").unwrap();
        }
        let linked = path.join("tensorfold_gdn_v2");
        std::fs::create_dir(&linked).unwrap();
        std::fs::create_dir(linked.join("lock")).unwrap();
        let mut gone = own_identity();
        gone.start_ticks += 1;
        assert_eq!(clear_stale_locks(&path, &[]), 1);
        assert!(!extension.join("lock").exists());
        assert!(extension.join("build.ninja").exists());
        assert!(
            linked.join("lock").is_dir(),
            "only a plain lock file is removed"
        );
        std::fs::write(extension.join("lock"), "").unwrap();
        assert_eq!(
            clear_stale_locks(&path, &[gone]),
            1,
            "a gone launch holds nothing"
        );
    }

    // T41 (ADR 0023 §3): a launch that may still run may be building in the
    // same directory (two starts of one version share it), so its lock stays.
    #[test]
    fn a_lock_stays_while_a_retained_launch_may_run() {
        let (_dir, root) = root();
        let path = root.torch_extensions("0.6.1").unwrap();
        let extension = path.join("tensorfold_qmm_v5");
        std::fs::create_dir(&extension).unwrap();
        std::fs::write(extension.join("lock"), "").unwrap();
        let mut unknown = own_identity();
        unknown.boot_id = String::new();
        let mut gone = own_identity();
        gone.start_ticks += 1;
        assert_eq!(clear_stale_locks(&path, &[gone, own_identity()]), 0);
        assert_eq!(
            clear_stale_locks(&path, &[unknown]),
            0,
            "unknown is not gone"
        );
        assert!(extension.join("lock").exists());
    }
}

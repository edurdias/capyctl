//! ADR 0033 §4 (lifecycle design §1.8): the private directory each launch's
//! LMCache keeps SGLang's connector configuration in.
//!
//! SGLang reads `--lmcache-config-file`, an owner-only file holding the
//! server's loopback request port. It lives in `<root>/<incarnation>` (0700),
//! inside a private root this service user owns, never in the runtime
//! directory (shared helper source). The adapter writes it at Initialize; the
//! host removes the directory once its own evidence proves the launch's
//! processes gone (the launch's cleanup, as for the rendezvous directory), and
//! a role start removes every directory no retained launch owns.
use std::path::{Path, PathBuf};

/// A host's private LMCache root (0700, this service user), with one directory
/// per launch.
#[derive(Clone, Debug)]
pub struct LmcacheDirs {
    root: crate::rendezvous::RendezvousRoot,
}

impl LmcacheDirs {
    /// `dir` is the host's private root; the role creates it 0700 first.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            root: crate::rendezvous::RendezvousRoot::new(dir),
        }
    }

    /// The directory of the launch `incarnation`, which the adapter creates
    /// (0700) when it writes the connector configuration. `None` when the
    /// root is not private or the incarnation is not a plain token.
    pub fn launch_dir(&self, incarnation: &str) -> Option<PathBuf> {
        self.root.launch_dir(incarnation)
    }

    /// The private root itself.
    pub fn dir(&self) -> &Path {
        self.root.dir()
    }

    /// Remove the directory of a launch whose processes are proved gone: only
    /// this user's own directory for exactly that incarnation, never a
    /// symlink. Returns whether one was removed.
    pub fn retire(&self, incarnation: &str) -> bool {
        self.root.retire(incarnation)
    }

    /// At a role start, remove every directory in the root that belongs to
    /// no retained launch (`keep` holds the incarnations still claimed). Only
    /// this user's own real directories directly in a private root; nothing
    /// is followed. Returns how many were removed.
    pub fn sweep(&self, keep: &std::collections::BTreeSet<String>) -> usize {
        self.root.sweep(keep)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};

    const INCARNATION: &str = "01K00000000000000000000002";

    fn dirs() -> (tempfile::TempDir, LmcacheDirs) {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("lmcache");
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        (temp, LmcacheDirs::new(dir))
    }

    // T45 (ADR 0033 §4): the adapter writes SGLang's configuration into the
    // launch's own directory; Terminate's gone evidence removes it, and a
    // role start sweeps the directories no retained launch owns.
    #[test]
    fn a_launch_directory_is_written_retired_and_swept() {
        let (_temp, dirs) = dirs();
        let dir = dirs.launch_dir(INCARNATION).unwrap();
        let deep = capyctl_config::lmcache::TransferMode::EngineDriven;
        let file = capyctl_adapters::lmcache::write_sglang_config(&dir, 8100, deep).unwrap();
        assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
        assert!(dirs.retire(INCARNATION));
        assert!(!dir.exists());
        assert!(!dirs.retire(INCARNATION));

        let kept = dirs.launch_dir(INCARNATION).unwrap();
        capyctl_adapters::lmcache::write_sglang_config(&kept, 8100, deep).unwrap();
        let stale = dirs.launch_dir("01K00000000000000000000003").unwrap();
        capyctl_adapters::lmcache::write_sglang_config(&stale, 8102, deep).unwrap();
        let keep = [INCARNATION.to_owned()].into_iter().collect();
        assert_eq!(dirs.sweep(&keep), 1);
        assert!(kept.exists());
        assert!(!stale.exists());
        for name in ["", "..", "a/b"] {
            assert!(dirs.launch_dir(name).is_none(), "{name:?}");
        }
    }
}

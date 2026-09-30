//! SPEC §8.2 / T21: the per-launch directory an SGLang single-rank launch keeps
//! its file rendezvous in (`runtime/loopback_rendezvous.py`).
//!
//! Found live 2026-09-23 (matrix): the entry removed its rendezvous directory
//! only at interpreter exit, and a signalled stop never runs exit handlers, so
//! every stopped SGLang launch left an owner-only `/tmp/capyctl-rdzv-*` directory
//! with its store file on the host. The host now names the directory itself,
//! inside a private root it owns (`<root>/<incarnation>`), and removes it once
//! its own evidence proves the launch's group gone, the same point a gone
//! launch's saver enrollment is retired.
use std::{
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

/// The environment variable that hands the entry its rendezvous directory.
pub const ENV_DIR: &str = "CAPYCTL_RENDEZVOUS_DIR";

/// A host's private rendezvous root (0700, this service user).
#[derive(Clone, Debug)]
pub struct RendezvousRoot {
    dir: PathBuf,
}

fn token(value: &str) -> bool {
    (1..=128).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn uid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

fn owned_dir(path: &Path, exact_mode: bool) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| {
        meta.is_dir() && meta.uid() == uid() && (!exact_mode || meta.mode() & 0o7777 == 0o700)
    })
}

impl RendezvousRoot {
    /// `dir` is the host's private root; the role creates it 0700 first.
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The directory the launch `incarnation` keeps its rendezvous in. The
    /// entry creates it (0700) and refuses one that already exists. `None`
    /// when the root is not private or the incarnation is not a plain token,
    /// so the entry falls back to its own temporary directory.
    pub fn launch_dir(&self, incarnation: &str) -> Option<PathBuf> {
        (token(incarnation) && owned_dir(&self.dir, true)).then(|| self.dir.join(incarnation))
    }

    /// The private root itself.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// SPEC §8.2 / T21 (owner decision 2026-09-25): at start, remove every
    /// directory in the root that belongs to no recorded launch (`keep` holds
    /// the incarnations the store still retains). Only this user's own real
    /// directories directly in a private root are removed: a symlink or any
    /// other entry is left alone and never followed, and nothing outside the
    /// root is touched. Returns how many were removed.
    pub fn sweep(&self, keep: &std::collections::BTreeSet<String>) -> usize {
        if !owned_dir(&self.dir, true) {
            return 0;
        }
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return 0;
        };
        let mut removed = 0;
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_str().is_some_and(|name| keep.contains(name)) {
                continue;
            }
            let path = self.dir.join(&name);
            // `symlink_metadata` never follows a link, so a symlink is not a
            // directory here; `remove_dir_all` does not follow links inside.
            if owned_dir(&path, false) && std::fs::remove_dir_all(&path).is_ok() {
                removed += 1;
            }
        }
        removed
    }

    /// Remove the rendezvous directory of a launch whose group is proved gone.
    /// Only this user's own directory for exactly that incarnation, never a
    /// symlink or anything else in the root. Returns whether it was removed.
    pub fn retire(&self, incarnation: &str) -> bool {
        let Some(path) = self.launch_dir(incarnation) else {
            return false;
        };
        owned_dir(&path, false) && std::fs::remove_dir_all(&path).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    const INCARNATION: &str = "01K00000000000000000000002";

    fn root() -> (tempfile::TempDir, RendezvousRoot) {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("rendezvous");
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        (temp, RendezvousRoot::new(dir))
    }

    // T21: found live 2026-09-23, a signalled SGLang stop left its rendezvous
    // directory and store file behind.
    #[test]
    fn a_gone_launch_loses_its_rendezvous_directory_and_store() {
        let (_temp, root) = root();
        let dir = root.launch_dir(INCARNATION).unwrap();
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        std::fs::write(dir.join("store"), b"rendezvous").unwrap();
        let other = root.launch_dir("01K00000000000000000000003").unwrap();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&other)
            .unwrap();

        assert!(root.retire(INCARNATION));
        assert!(!dir.exists());
        // Another launch's directory is untouched, and a repeat is a no-op.
        assert!(other.exists());
        assert!(!root.retire(INCARNATION));
    }

    #[test]
    fn nothing_outside_one_token_named_directory_is_removed() {
        let (temp, root) = root();
        let outside = temp.path().join("keep");
        std::fs::create_dir(&outside).unwrap();
        for name in ["", "..", "../keep", "a/b", "keep.json"] {
            assert!(root.launch_dir(name).is_none(), "{name:?}");
            assert!(!root.retire(name));
        }
        // A symlink in the root is never followed.
        std::os::unix::fs::symlink(&outside, root.dir.join(INCARNATION)).unwrap();
        assert!(!root.retire(INCARNATION));
        assert!(outside.exists());
    }

    // T21 T37: owner decision 2026-09-25, a start removes the directories no
    // recorded launch owns, never a recorded one, a symlink's target, a file,
    // or anything outside the root.
    #[test]
    fn a_start_sweeps_only_unrecorded_directories_in_the_root() {
        let (temp, root) = root();
        let make = |name: &str| {
            let dir = root.dir.join(name);
            std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
            std::fs::write(dir.join("store"), b"rendezvous").unwrap();
            dir
        };
        let recorded = make(INCARNATION);
        let leftover = make("01K00000000000000000000003");
        let stray = make("not-a-token");
        let outside = temp.path().join("keep");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("data"), b"kept").unwrap();
        std::os::unix::fs::symlink(&outside, root.dir.join("01K00000000000000000000004")).unwrap();
        std::fs::write(root.dir.join("file"), b"kept").unwrap();
        let keep = [INCARNATION.to_owned()].into_iter().collect();

        assert_eq!(root.sweep(&keep), 2);
        assert!(recorded.join("store").exists());
        assert!(!leftover.exists() && !stray.exists());
        assert!(outside.join("data").exists());
        assert!(root
            .dir
            .join("01K00000000000000000000004")
            .symlink_metadata()
            .is_ok());
        assert!(root.dir.join("file").exists());
        // A root that is not private is never swept.
        make("01K00000000000000000000005");
        std::fs::set_permissions(&root.dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(root.sweep(&keep), 0);
    }

    #[test]
    fn a_root_that_is_not_private_names_no_directory() {
        let (_temp, root) = root();
        std::fs::set_permissions(&root.dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(root.launch_dir(INCARNATION).is_none());
        let missing = RendezvousRoot::new(root.dir.join("missing"));
        assert!(missing.launch_dir(INCARNATION).is_none());
    }
}

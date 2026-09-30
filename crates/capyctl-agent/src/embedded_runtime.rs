//! SPEC §3.3 / ADR 0001 (owner decision 2026-09-24): the release is one
//! self-contained executable, so capyctl's Python runtime helpers travel inside
//! it. `build.rs` compiles every `runtime/*.py` (never `runtime/tests`) into
//! the binary with a manifest of SHA-256 digests; this module writes them into
//! the role's managed runtime directory (`<state_dir>/runtime`).
//!
//! The managed directory is what this binary put there and nothing else:
//!
//! - It is created 0700 with every module 0600, owned by the running user, so
//!   it passes the owner-only rule the launch path enforces
//!   (`runtime_integrity`, SPEC §9.1, §13.3 / T21 T37).
//! - A marker file (`MARKER`) names the manifest it was written from. A
//!   directory without the marker is not capyctl's: it is refused, never
//!   overwritten, so an operator's own copy cannot be clobbered.
//! - On every start the tree is compared with the embedded manifest. A
//!   different manifest (a binary upgrade) refreshes it; a changed, missing
//!   or extra file (tampering, stray bytecode) restores it from the embedded
//!   copy. Either way the new tree is built beside the old one and renamed
//!   into place, so a launch never sees a half-written directory.
//!
//! An operator-declared runtime directory (a host document's `runtime_dir`,
//! standalone's `CAPYCTL_RUNTIME_DIR`) is never touched here; the launch path's
//! integrity check is its only gate.

use std::fmt;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// One module compiled into the binary.
#[derive(Debug)]
pub struct EmbeddedFile {
    pub name: &'static str,
    pub sha256: &'static str,
    pub contents: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/embedded_runtime.rs"));

/// The file that marks a directory as capyctl-managed. Not a `.py` module, so
/// Python never imports it and the integrity walk only requires it be regular.
pub const MARKER: &str = ".capyctl-managed-runtime";

const MARKER_MAGIC: &str = "capyctl-managed-runtime 1";

/// The modules this binary carries.
pub fn files() -> &'static [EmbeddedFile] {
    FILES
}

/// The digest over the embedded manifest (`<sha256>  <name>\n` per module,
/// sorted by name). Two binaries with the same digest ship the same runtime.
pub fn manifest_digest() -> &'static str {
    MANIFEST_DIGEST
}

/// What [`materialize`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Materialized {
    /// The directory did not exist; it was written.
    Created,
    /// The directory already matched the embedded manifest.
    Current,
    /// The directory was written by a binary with another manifest (an
    /// upgrade or a rollback); it was replaced. Carries the old digest.
    Refreshed { previous: String },
    /// The directory carried this binary's manifest but its contents
    /// differed; it was restored. Names what differed, never contents.
    Restored { changed: Vec<String> },
}

impl Materialized {
    /// A line for the operator when something was rewritten.
    pub fn notice(&self, dir: &Path) -> Option<String> {
        match self {
            Materialized::Created | Materialized::Current => None,
            Materialized::Refreshed { .. } => Some(format!(
                "runtime directory {} refreshed from this binary's embedded runtime",
                dir.display()
            )),
            Materialized::Restored { changed } => Some(format!(
                "runtime directory {} did not match this binary's embedded runtime \
                 ({}); restored from the embedded copy",
                dir.display(),
                changed.join(", ")
            )),
        }
    }
}

/// Why the managed directory could not be written.
#[derive(Debug)]
pub enum MaterializeError {
    /// The path exists but is not a directory capyctl manages (no marker, not a
    /// directory, or a symlink). Nothing was changed.
    Unmanaged(PathBuf),
    /// The directory is owned by another account. Nothing was changed.
    NotOwned(PathBuf),
    /// A file-system operation failed.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for MaterializeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MaterializeError::Unmanaged(path) => write!(
                f,
                "{} exists and is not a runtime directory capyctl manages (no {MARKER}); \
                 remove it, or declare it as the runtime directory explicitly",
                path.display()
            ),
            MaterializeError::NotOwned(path) => {
                write!(f, "{} is not owned by this user", path.display())
            }
            MaterializeError::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for MaterializeError {}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> MaterializeError + '_ {
    move |source| MaterializeError::Io {
        path: path.to_owned(),
        source,
    }
}

/// Write, check or repair the managed runtime directory `dir`.
///
/// `dir`'s parent is created (0700) when missing. The result is a directory
/// holding exactly the embedded modules and the marker, owner-only.
pub fn materialize(dir: &Path) -> Result<Materialized, MaterializeError> {
    materialize_from(dir, FILES, MANIFEST_DIGEST)
}

fn materialize_from(
    dir: &Path,
    files: &[EmbeddedFile],
    digest: &str,
) -> Result<Materialized, MaterializeError> {
    let metadata = match fs::symlink_metadata(dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = dir.parent() {
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(parent)
                    .map_err(io(parent))?;
            }
            let staged = write_tree(dir, files, digest)?;
            return match fs::rename(&staged, dir) {
                Ok(()) => {
                    sync_parent(dir);
                    Ok(Materialized::Created)
                }
                Err(error) => {
                    let _ = fs::remove_dir_all(&staged);
                    // Another start created it meanwhile: check what it wrote.
                    if fs::symlink_metadata(dir).is_ok() {
                        materialize_from(dir, files, digest)
                    } else {
                        Err(io(dir)(error))
                    }
                }
            };
        }
        Err(error) => return Err(io(dir)(error)),
    };
    if !metadata.file_type().is_dir() {
        return Err(MaterializeError::Unmanaged(dir.to_owned()));
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(MaterializeError::NotOwned(dir.to_owned()));
    }
    let Some(previous) = read_marker(dir) else {
        return Err(MaterializeError::Unmanaged(dir.to_owned()));
    };
    if previous != digest {
        replace(dir, files, digest)?;
        return Ok(Materialized::Refreshed { previous });
    }
    let changed = differences(dir, &metadata, files)?;
    if changed.is_empty() {
        return Ok(Materialized::Current);
    }
    replace(dir, files, digest)?;
    Ok(Materialized::Restored { changed })
}

/// The manifest digest a managed directory's marker names, or `None` when
/// the marker is absent or is not one capyctl wrote.
fn read_marker(dir: &Path) -> Option<String> {
    let path = dir.join(MARKER);
    let metadata = fs::symlink_metadata(&path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > 64 * 1024 {
        return None;
    }
    let text = fs::read_to_string(&path).ok()?;
    let mut lines = text.lines();
    if lines.next()? != MARKER_MAGIC {
        return None;
    }
    lines
        .find_map(|line| line.strip_prefix("manifest sha256:"))
        .map(str::to_owned)
}

fn marker_text(files: &[EmbeddedFile], digest: &str) -> String {
    let mut text = format!(
        "{MARKER_MAGIC}\n# Written by capyctl {}; rewritten on every start. Do not edit.\nmanifest sha256:{digest}\n",
        env!("CARGO_PKG_VERSION")
    );
    for file in files {
        text.push_str(&format!("{}  {}\n", file.sha256, file.name));
    }
    text
}

/// Everything in `dir` that is not exactly the embedded tree: a module whose
/// bytes, owner or mode differ, a missing module, any extra entry, or a
/// directory mode other than 0700.
fn differences(
    dir: &Path,
    metadata: &fs::Metadata,
    files: &[EmbeddedFile],
) -> Result<Vec<String>, MaterializeError> {
    let euid = unsafe { libc::geteuid() };
    let mut changed = Vec::new();
    if metadata.mode() & 0o7777 != 0o700 {
        changed.push("directory mode".to_owned());
    }
    for entry in fs::read_dir(dir).map_err(io(dir))? {
        let entry = entry.map_err(io(dir))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == MARKER {
            continue;
        }
        let Some(expected) = files.iter().find(|file| file.name == name) else {
            changed.push(format!("unexpected {name}"));
            continue;
        };
        let path = entry.path();
        let meta = fs::symlink_metadata(&path).map_err(io(&path))?;
        if !meta.file_type().is_file() || meta.uid() != euid || meta.mode() & 0o7777 != 0o600 {
            changed.push(format!("{} (type, owner or mode)", expected.name));
            continue;
        }
        let bytes = fs::read(&path).map_err(io(&path))?;
        if hex(&Sha256::digest(&bytes)) != expected.sha256 {
            changed.push(format!("{} (contents)", expected.name));
        }
    }
    for file in files {
        if fs::symlink_metadata(dir.join(file.name)).is_err() {
            changed.push(format!("missing {}", file.name));
        }
    }
    let marker = fs::symlink_metadata(dir.join(MARKER)).map_err(io(dir))?;
    if marker.uid() != euid || marker.mode() & 0o7777 != 0o600 {
        changed.push(format!("{MARKER} (owner or mode)"));
    }
    changed.sort();
    Ok(changed)
}

/// Build the embedded tree in a fresh sibling of `dir` and return its path.
fn write_tree(
    dir: &Path,
    files: &[EmbeddedFile],
    digest: &str,
) -> Result<PathBuf, MaterializeError> {
    let staged = sibling(dir, "new");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&staged)
        .map_err(io(&staged))?;
    let result = (|| {
        // The umask can only clear bits; set the exact modes explicitly.
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o700)).map_err(io(&staged))?;
        for file in files {
            write_file(&staged.join(file.name), file.contents)?;
        }
        // The marker last: a staged tree without it is never adopted.
        write_file(&staged.join(MARKER), marker_text(files, digest).as_bytes())?;
        fs::File::open(&staged)
            .and_then(|handle| handle.sync_all())
            .map_err(io(&staged))
    })();
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&staged);
        return Err(error);
    }
    Ok(staged)
}

fn write_file(path: &Path, contents: &[u8]) -> Result<(), MaterializeError> {
    let mut handle = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(io(path))?;
    handle.write_all(contents).map_err(io(path))?;
    handle
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(io(path))?;
    handle.sync_all().map_err(io(path))
}

/// Swap a freshly written tree in for `dir`, then remove the old one.
fn replace(dir: &Path, files: &[EmbeddedFile], digest: &str) -> Result<(), MaterializeError> {
    let staged = write_tree(dir, files, digest)?;
    let retired = sibling(dir, "old");
    if let Err(error) = fs::rename(dir, &retired) {
        let _ = fs::remove_dir_all(&staged);
        return Err(io(dir)(error));
    }
    if let Err(error) = fs::rename(&staged, dir) {
        // Put the previous tree back rather than leave no directory at all.
        let _ = fs::rename(&retired, dir);
        let _ = fs::remove_dir_all(&staged);
        return Err(io(dir)(error));
    }
    sync_parent(dir);
    // The retired tree may hold anything (it is what failed the check), so
    // make it removable before removing it.
    let _ = make_owner_writable(&retired);
    let _ = fs::remove_dir_all(&retired);
    Ok(())
}

fn make_owner_writable(dir: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(dir)?;
    if metadata.file_type().is_dir() {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        for entry in fs::read_dir(dir)? {
            make_owner_writable(&entry?.path())?;
        }
    }
    Ok(())
}

/// A hidden, unique name beside `dir`, so staging never escapes its parent
/// (same file system, so the rename is atomic).
fn sibling(dir: &Path, purpose: &str) -> PathBuf {
    let name = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "runtime".to_owned());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    dir.with_file_name(format!(".{name}.{purpose}-{}-{nanos}", std::process::id()))
}

fn sync_parent(dir: &Path) {
    if let Some(parent) = dir.parent() {
        let _ = fs::File::open(parent).and_then(|handle| handle.sync_all());
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use capyctl_config::effective::Engine;

    fn managed(root: &Path) -> PathBuf {
        root.join("state").join("runtime")
    }

    fn file_digest(contents: &[u8]) -> &'static str {
        Box::leak(hex(&Sha256::digest(contents)).into_boxed_str())
    }

    // T21 T37: the embedded manifest is the checkout's runtime modules, with
    // their digests, and never the tests.
    #[test]
    fn the_embedded_manifest_is_the_shipped_runtime() {
        let checkout = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime");
        let mut expected: Vec<String> = fs::read_dir(&checkout)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".py"))
            .collect();
        expected.sort();
        let names: Vec<&str> = files().iter().map(|file| file.name).collect();
        assert_eq!(names, expected);
        assert!(!names.iter().any(|name| name.contains('/')));
        for file in files() {
            assert_eq!(
                file.sha256,
                hex(&Sha256::digest(file.contents)),
                "{}",
                file.name
            );
            assert_eq!(file.contents, fs::read(checkout.join(file.name)).unwrap());
        }
        assert_eq!(manifest_digest().len(), 64);
    }

    // T21 T37: a fresh directory is written owner-only and passes the launch
    // path's integrity check for every engine family.
    #[test]
    fn materializing_writes_an_owner_only_tree_the_launch_path_accepts() {
        let root = tempfile::tempdir().unwrap();
        let dir = managed(root.path());
        assert_eq!(materialize(&dir).unwrap(), Materialized::Created);
        let meta = fs::symlink_metadata(&dir).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o700);
        assert_eq!(
            fs::symlink_metadata(dir.parent().unwrap()).unwrap().mode() & 0o7777,
            0o700
        );
        for file in files() {
            let path = dir.join(file.name);
            let meta = fs::symlink_metadata(&path).unwrap();
            assert_eq!(meta.mode() & 0o7777, 0o600, "{}", file.name);
            assert_eq!(fs::read(&path).unwrap(), file.contents);
        }
        let mut entries: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        let mut expected: Vec<String> = files().iter().map(|f| f.name.to_owned()).collect();
        expected.push(MARKER.to_owned());
        expected.sort();
        assert_eq!(entries, expected, "no staging directory is left behind");
        for (engine, sleep) in [
            (Engine::Vllm, false),
            (Engine::Vllm, true),
            (Engine::Sglang, false),
        ] {
            crate::runtime_integrity::verify(
                &dir,
                crate::runtime_integrity::required_files(engine, sleep),
            )
            .unwrap();
        }
        // A second start finds it current and changes nothing.
        assert_eq!(materialize(&dir).unwrap(), Materialized::Current);
        let siblings = fs::read_dir(dir.parent().unwrap()).unwrap().count();
        assert_eq!(siblings, 1);
    }

    // T21 T37: a binary upgrade (another manifest) refreshes the tree.
    #[test]
    fn a_different_embedded_manifest_refreshes_the_tree() {
        let root = tempfile::tempdir().unwrap();
        let dir = managed(root.path());
        let old = [EmbeddedFile {
            name: "vllm_entry.py",
            sha256: file_digest(b"# old entry\n"),
            contents: b"# old entry\n",
        }];
        assert_eq!(
            materialize_from(&dir, &old, "0ld").unwrap(),
            Materialized::Created
        );
        assert_eq!(
            fs::read(dir.join("vllm_entry.py")).unwrap(),
            b"# old entry\n"
        );
        assert_eq!(
            materialize(&dir).unwrap(),
            Materialized::Refreshed {
                previous: "0ld".into()
            }
        );
        assert_eq!(read_marker(&dir).as_deref(), Some(manifest_digest()));
        for file in files() {
            assert_eq!(fs::read(dir.join(file.name)).unwrap(), file.contents);
        }
        assert_eq!(materialize(&dir).unwrap(), Materialized::Current);
        assert_eq!(fs::read_dir(dir.parent().unwrap()).unwrap().count(), 1);
    }

    // T21 T37: a managed tree whose contents drifted (edited module, stray
    // bytecode, loosened mode, deleted module) is detected and restored.
    #[test]
    fn a_tampered_managed_tree_is_detected_and_restored() {
        let root = tempfile::tempdir().unwrap();
        let dir = managed(root.path());
        materialize(&dir).unwrap();
        let guard = dir.join("capyctl_vllm_guard.py");
        fs::write(&guard, b"import os  # rewritten\n").unwrap();
        fs::create_dir(dir.join("__pycache__")).unwrap();
        fs::set_permissions(dir.join("vllm_entry.py"), fs::Permissions::from_mode(0o644)).unwrap();
        fs::remove_file(dir.join("sglang_entry.py")).unwrap();

        let Materialized::Restored { changed } = materialize(&dir).unwrap() else {
            panic!("tampering must be reported as a restore");
        };
        assert_eq!(
            changed,
            vec![
                "capyctl_vllm_guard.py (contents)".to_owned(),
                "missing sglang_entry.py".to_owned(),
                "unexpected __pycache__".to_owned(),
                "vllm_entry.py (type, owner or mode)".to_owned(),
            ]
        );
        let notice = Materialized::Restored { changed }.notice(&dir).unwrap();
        assert!(notice.contains("restored"), "{notice}");
        assert!(
            !notice.contains("import os"),
            "a notice never carries contents"
        );
        for file in files() {
            let path = dir.join(file.name);
            assert_eq!(fs::read(&path).unwrap(), file.contents);
            assert_eq!(fs::symlink_metadata(&path).unwrap().mode() & 0o7777, 0o600);
        }
        assert!(!dir.join("__pycache__").exists());
        assert_eq!(materialize(&dir).unwrap(), Materialized::Current);
    }

    // T21 T37: a directory without the marker is the operator's, not capyctl's;
    // it is refused and left exactly as it was.
    #[test]
    fn an_unmanaged_directory_is_refused_and_left_untouched() {
        let root = tempfile::tempdir().unwrap();
        let dir = managed(root.path());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("vllm_entry.py"), b"# operator copy\n").unwrap();
        let error = materialize(&dir).unwrap_err();
        assert!(matches!(error, MaterializeError::Unmanaged(_)), "{error}");
        assert!(error.to_string().contains(MARKER));
        assert_eq!(
            fs::read(dir.join("vllm_entry.py")).unwrap(),
            b"# operator copy\n"
        );
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);

        // A forged or foreign marker does not make it capyctl's either.
        fs::write(dir.join(MARKER), b"something else\nmanifest sha256:00\n").unwrap();
        assert!(matches!(
            materialize(&dir).unwrap_err(),
            MaterializeError::Unmanaged(_)
        ));

        // Nor does a file or a symlink at the path.
        let file = root.path().join("file-runtime");
        fs::write(&file, b"").unwrap();
        assert!(matches!(
            materialize(&file).unwrap_err(),
            MaterializeError::Unmanaged(_)
        ));
        let link = root.path().join("link-runtime");
        let target = root.path().join("target");
        materialize(&target).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(matches!(
            materialize(&link).unwrap_err(),
            MaterializeError::Unmanaged(_)
        ));
    }
}

//! SPEC §9.1, §13.3 / T21 T37: the integrity of capyctl's runtime directory.
//!
//! The engine imports capyctl's own Python from the runtime directory: the vLLM
//! guard that keys the development routes (`capyctl_vllm_guard.py`), the protected
//! vLLM entry (`vllm_entry.py`), and SGLang's entry and its helpers
//! (`sglang_*.py`, `pinned_file_observation.py`, ...). Any of them rewritten by
//! another account runs as the engine, behind the controls it is meant to
//! guard. Live Phase B found the guard mode 0664 on host-a.
//!
//! So a launch is admitted only when the directory and every Python module in
//! it are what this account put there: not a symlink, owned by the effective
//! user, never writable by other, and writable by group only when that group
//! is the owning user's private group (owner decision 2026-09-22: the check is
//! owner-only, and a private group adds no writer; 2026-09-23: the rule lives
//! once, in `capyctl_adapters::owner_only`). A group whose membership cannot be
//! established is refused. The engine's required modules must also
//! exist. The rule applies to the whole tree: every subdirectory (each one is
//! an importable package) and its modules, no symlink anywhere, and nothing
//! importable besides checked `.py` source (a cached `.pyc`, an extension
//! module, a path hook or an archive would load code the check never read).
//! Engines and probes run with `-B` and `PYTHONDONTWRITEBYTECODE=1`, so none is
//! written there again; a host whose directory still holds `__pycache__`
//! bytecode must remove it once. The check reads metadata and the account database only and changes
//! nothing.

use std::fmt;
use std::path::{Path, PathBuf};

use capyctl_adapters::owner_only::PrivateGroup;
use capyctl_config::effective::Engine;

/// The modules a vLLM launch imports from the runtime directory.
pub const VLLM_RUNTIME_FILES: &[&str] = &["capyctl_vllm_guard.py", "vllm_entry.py"];

/// ADR 0008 (owner decision 2026-09-23): the capability probes capyctl's entries
/// import by shape. SGLang's protected entry imports it on every launch (the
/// `core` check); vLLM's imports it only when sleep mode is on (`deep_park`).
pub const CAPABILITY_PROBE: &str = "engine_capabilities.py";

/// The modules a vLLM launch with sleep mode on imports.
pub const VLLM_SLEEP_RUNTIME_FILES: &[&str] =
    &["capyctl_vllm_guard.py", "vllm_entry.py", CAPABILITY_PROBE];

/// The modules an SGLang launch requires in the runtime directory. Every other
/// `*.py` there (the remaining `sglang_*.py` helpers) is checked as well.
pub const SGLANG_RUNTIME_FILES: &[&str] = &[
    "sglang_entry.py",
    "pinned_file_observation.py",
    CAPABILITY_PROBE,
];

/// The files one engine requires. `sleep_mode` is whether a vLLM launch renders
/// sleep mode (deep parking); an SGLang launch always needs its probes.
pub fn required_files(engine: Engine, sleep_mode: bool) -> &'static [&'static str] {
    match engine {
        Engine::Vllm if sleep_mode => VLLM_SLEEP_RUNTIME_FILES,
        Engine::Vllm => VLLM_RUNTIME_FILES,
        Engine::Sglang => SGLANG_RUNTIME_FILES,
        // ADR 0023 §2: TensorFold needs only the capability probe.
        Engine::Tensorfold => &[CAPABILITY_PROBE],
    }
}

/// The files one resolved launch requires: sleep mode is what its effective
/// configuration renders (`enable_sleep_mode`, derived from its residency and
/// the host's deep-park switch).
pub fn launch_required_files(
    effective: &capyctl_config::effective::EffectiveDeployment,
) -> &'static [&'static str] {
    let sleep_mode = matches!(
        &effective.engine_config,
        capyctl_domain::launch::LaunchSettings::Vllm(settings) if settings.enable_sleep_mode
    );
    required_files(effective.profile.engine, sleep_mode)
}

/// Why a runtime directory is not trusted. Names the path and the defect;
/// never file contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrityError {
    pub path: PathBuf,
    pub problem: &'static str,
}

impl fmt::Display for IntegrityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.problem)
    }
}

impl std::error::Error for IntegrityError {}

/// Verify `runtime_dir` for a launch that needs `required`.
pub fn verify(runtime_dir: &Path, required: &[&str]) -> Result<(), IntegrityError> {
    verify_with(
        runtime_dir,
        required,
        &capyctl_adapters::owner_only::system_private_group,
    )
}

fn verify_with(
    runtime_dir: &Path,
    required: &[&str],
    private_group: &PrivateGroup,
) -> Result<(), IntegrityError> {
    let fail = |path: &Path, problem| IntegrityError {
        path: path.to_owned(),
        problem,
    };
    let euid = unsafe { libc::geteuid() };
    let dir = std::fs::symlink_metadata(runtime_dir).map_err(|_| fail(runtime_dir, "missing"))?;
    if !dir.file_type().is_dir() {
        return Err(fail(runtime_dir, "not a directory"));
    }
    trusted(runtime_dir, &dir, euid, private_group)?;
    for name in required {
        let path = runtime_dir.join(name);
        std::fs::symlink_metadata(&path).map_err(|_| fail(&path, "missing"))?;
    }
    walk(runtime_dir, euid, private_group, 0)
}

/// SPEC §9.1 / T21: Python imports these besides `.py` source: bytecode (a
/// cached `.pyc` is loaded instead of its source when it looks current, and an
/// unchecked-hash one always), extension modules, path hooks and archives.
const FOREIGN_IMPORTABLE: &[&str] = &["pyc", "pyo", "so", "pyd", "pth", "zip", "egg", "whl"];

/// How deep the runtime tree may nest; capyctl's own is two levels.
const MAX_DEPTH: usize = 8;

/// Every entry of one directory, recursively: modules and subdirectories are
/// owner-only, symlinks and special files are refused, and nothing importable
/// other than checked `.py` source may be present anywhere in the tree.
fn walk(
    dir: &Path,
    euid: u32,
    private_group: &PrivateGroup,
    depth: usize,
) -> Result<(), IntegrityError> {
    let fail = |path: &Path, problem| IntegrityError {
        path: path.to_owned(),
        problem,
    };
    if depth > MAX_DEPTH {
        return Err(fail(dir, "nested too deeply"));
    }
    let entries = std::fs::read_dir(dir).map_err(|_| fail(dir, "unreadable"))?;
    for entry in entries {
        let entry = entry.map_err(|_| fail(dir, "unreadable"))?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path).map_err(|_| fail(&path, "unreadable"))?;
        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let file_type = metadata.file_type();
        if file_type.is_dir() {
            trusted(&path, &metadata, euid, private_group)?;
            walk(&path, euid, private_group, depth + 1)?;
        } else if extension == "py" {
            if !file_type.is_file() {
                return Err(fail(&path, "not a regular file"));
            }
            trusted(&path, &metadata, euid, private_group)?;
        } else if !file_type.is_file() {
            return Err(fail(&path, "not a regular file or directory"));
        } else if FOREIGN_IMPORTABLE.contains(&extension) {
            return Err(fail(&path, "compiled or foreign importable file"));
        }
    }
    Ok(())
}

fn trusted(
    path: &Path,
    metadata: &std::fs::Metadata,
    euid: u32,
    private_group: &PrivateGroup,
) -> Result<(), IntegrityError> {
    // Owner decisions 2026-09-22 and 2026-09-23: the one owner-only rule for
    // capyctl's runtime helpers (`capyctl_adapters::owner_only`), with the agent user
    // as the only accepted owner of the runtime directory and its modules.
    use capyctl_adapters::owner_only::{check, Problem};
    check(metadata, &[euid], private_group).map_err(|problem| IntegrityError {
        path: path.to_owned(),
        problem: match problem {
            Problem::NotOwned => "not owned by the agent user",
            Problem::WritableByOther => "writable by other",
            Problem::WritableBySharedGroup => "writable by a shared group",
            Problem::GroupUndetermined => "group membership undetermined",
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use capyctl_adapters::owner_only::system_private_group;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn runtime(files: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        for name in files {
            let path = dir.path().join(name);
            std::fs::write(&path, "# module\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        dir
    }

    fn chmod(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    // T21 T37
    #[test]
    fn a_private_runtime_directory_is_trusted() {
        let dir = runtime(&["capyctl_vllm_guard.py", "vllm_entry.py", "README.md"]);
        verify(dir.path(), required_files(Engine::Vllm, false)).unwrap();
        // A non-module file's mode is not the engine's import surface.
        chmod(&dir.path().join("README.md"), 0o666);
        verify(dir.path(), required_files(Engine::Vllm, false)).unwrap();
    }

    fn private(_gid: u32, _uid: u32) -> Option<bool> {
        Some(true)
    }

    fn shared(_gid: u32, _uid: u32) -> Option<bool> {
        Some(false)
    }

    fn undetermined(_gid: u32, _uid: u32) -> Option<bool> {
        None
    }

    // T21 T22 T37: ADR 0008. The capability probes are required where an
    // entry imports them: every SGLang launch, and a vLLM launch only with
    // sleep mode on. A restart-only vLLM launch never needs them.
    #[test]
    fn the_capability_probe_is_required_where_an_entry_imports_it() {
        let vllm = runtime(&["capyctl_vllm_guard.py", "vllm_entry.py"]);
        verify(vllm.path(), required_files(Engine::Vllm, false)).unwrap();
        let error = verify(vllm.path(), required_files(Engine::Vllm, true)).unwrap_err();
        assert_eq!(error.path, vllm.path().join(CAPABILITY_PROBE));
        assert_eq!(error.problem, "missing");
        std::fs::write(vllm.path().join(CAPABILITY_PROBE), "# probes\n").unwrap();
        chmod(&vllm.path().join(CAPABILITY_PROBE), 0o644);
        verify(vllm.path(), required_files(Engine::Vllm, true)).unwrap();

        let sglang = runtime(&["sglang_entry.py", "pinned_file_observation.py"]);
        for sleep_mode in [false, true] {
            let error =
                verify(sglang.path(), required_files(Engine::Sglang, sleep_mode)).unwrap_err();
            assert_eq!(error.path, sglang.path().join(CAPABILITY_PROBE));
        }
        std::fs::write(sglang.path().join(CAPABILITY_PROBE), "# probes\n").unwrap();
        chmod(&sglang.path().join(CAPABILITY_PROBE), 0o644);
        verify(sglang.path(), required_files(Engine::Sglang, false)).unwrap();
    }

    // T21 T37: owner decision 2026-09-22. A module group-writable only by the
    // owning user's private group is writable by that user alone, so it is
    // trusted; the same mode under a shared group is refused.
    #[test]
    fn group_write_is_trusted_only_under_the_owners_private_group() {
        for mode in [0o664, 0o620] {
            let dir = runtime(&["capyctl_vllm_guard.py", "vllm_entry.py"]);
            let guard = dir.path().join("capyctl_vllm_guard.py");
            chmod(&guard, mode);
            verify_with(dir.path(), VLLM_RUNTIME_FILES, &private).unwrap();
            let error = verify_with(dir.path(), VLLM_RUNTIME_FILES, &shared).unwrap_err();
            assert_eq!(error.path, guard);
            assert_eq!(error.problem, "writable by a shared group");
        }
        // A private group's directory write is the owner's too.
        let dir = runtime(&["capyctl_vllm_guard.py", "vllm_entry.py"]);
        chmod(dir.path(), 0o775);
        verify_with(dir.path(), VLLM_RUNTIME_FILES, &private).unwrap();
        assert_eq!(
            verify_with(dir.path(), VLLM_RUNTIME_FILES, &shared)
                .unwrap_err()
                .problem,
            "writable by a shared group"
        );
    }

    // T21 T37: a group whose membership cannot be established is not assumed
    // private.
    #[test]
    fn group_write_under_an_undetermined_group_is_refused() {
        let dir = runtime(&["capyctl_vllm_guard.py", "vllm_entry.py"]);
        let guard = dir.path().join("capyctl_vllm_guard.py");
        chmod(&guard, 0o664);
        let error = verify_with(dir.path(), VLLM_RUNTIME_FILES, &undetermined).unwrap_err();
        assert_eq!(error.path, guard);
        assert_eq!(error.problem, "group membership undetermined");
        // Without group write the group is never consulted.
        chmod(&guard, 0o644);
        verify_with(dir.path(), VLLM_RUNTIME_FILES, &undetermined).unwrap();
    }

    // T21 T37: Phase B found the guard 0664 on host-a; other-write is
    // refused whatever the group.
    #[test]
    fn an_other_writable_module_is_refused() {
        for mode in [0o646, 0o666, 0o602] {
            let dir = runtime(&["capyctl_vllm_guard.py", "vllm_entry.py"]);
            let guard = dir.path().join("capyctl_vllm_guard.py");
            chmod(&guard, mode);
            for lookup in [private, shared, undetermined] {
                let error = verify_with(dir.path(), VLLM_RUNTIME_FILES, &lookup).unwrap_err();
                assert_eq!(error.path, guard);
                assert_eq!(error.problem, "writable by other");
            }
            let error = verify(dir.path(), VLLM_RUNTIME_FILES).unwrap_err();
            assert_eq!(error.problem, "writable by other");
        }
    }

    // T21 T37: the system lookup against this host's real account database.
    // A file under the test user's own group is trusted at 0664 exactly when
    // that group is private; under any other group the user belongs to that is
    // not private, it is refused.
    #[test]
    fn the_system_group_lookup_decides_group_write() {
        let uid = unsafe { libc::geteuid() };
        let dir = runtime(&["capyctl_vllm_guard.py", "vllm_entry.py"]);
        let guard = dir.path().join("capyctl_vllm_guard.py");
        chmod(&guard, 0o664);
        let gid = std::fs::metadata(&guard).unwrap().gid();
        match system_private_group(gid, uid) {
            Some(true) => verify(dir.path(), VLLM_RUNTIME_FILES).unwrap(),
            _ => assert!(verify(dir.path(), VLLM_RUNTIME_FILES).is_err()),
        }
        // A supplementary group that is not the user's private group.
        let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        let mut groups = vec![0 as libc::gid_t; count.max(0) as usize];
        let count = unsafe { libc::getgroups(count, groups.as_mut_ptr()) };
        groups.truncate(count.max(0) as usize);
        let Some(other) = groups
            .into_iter()
            .find(|&group| group != gid && system_private_group(group, uid) == Some(false))
        else {
            return;
        };
        let path = std::ffi::CString::new(guard.as_os_str().as_encoded_bytes()).unwrap();
        if unsafe { libc::chown(path.as_ptr(), u32::MAX, other) } != 0 {
            return;
        }
        chmod(&guard, 0o664);
        assert_eq!(
            verify(dir.path(), VLLM_RUNTIME_FILES).unwrap_err().problem,
            "writable by a shared group"
        );
    }

    // T21 T37: every module the engine can import is checked, not only the
    // required ones.
    #[test]
    fn any_writable_module_in_the_directory_is_refused() {
        let dir = runtime(&[
            "sglang_entry.py",
            "pinned_file_observation.py",
            "engine_capabilities.py",
            "sglang_saver_binding.py",
        ]);
        verify(dir.path(), required_files(Engine::Sglang, true)).unwrap();
        chmod(&dir.path().join("sglang_saver_binding.py"), 0o664);
        assert!(verify_with(dir.path(), required_files(Engine::Sglang, true), &shared).is_err());
        chmod(&dir.path().join("sglang_saver_binding.py"), 0o644);
        chmod(&dir.path().join("pinned_file_observation.py"), 0o666);
        assert!(verify(dir.path(), required_files(Engine::Sglang, true)).is_err());
    }

    // T21 T37: SPEC §9.1. Python imports compiled and extension files, and
    // any subdirectory is an importable package, so neither may carry what the
    // `.py` check never saw: a cached `.pyc` loads instead of its checked
    // source, and an extension module is native code.
    #[test]
    fn compiled_or_foreign_importable_files_are_refused_anywhere() {
        for foreign in [
            "__pycache__/vllm_entry.cpython-312.pyc",
            "evil.cpython-312-aarch64-linux-gnu.so",
            "hook.pth",
            "legacy.pyo",
            "tests/__pycache__/test_x.cpython-312.pyc",
            "bundle.zip",
        ] {
            let dir = runtime(&["capyctl_vllm_guard.py", "vllm_entry.py"]);
            let path = dir.path().join(foreign);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            // Owner-only whatever the umask, so only the file's kind is judged.
            for ancestor in path.parent().unwrap().ancestors() {
                if ancestor == dir.path() {
                    break;
                }
                chmod(ancestor, 0o755);
            }
            std::fs::write(&path, "x").unwrap();
            let error = verify(dir.path(), VLLM_RUNTIME_FILES).unwrap_err();
            assert_eq!(error.path, path, "{foreign}");
            assert_eq!(
                error.problem, "compiled or foreign importable file",
                "{foreign}"
            );
        }
        // An empty cache directory and plain documents are no import surface.
        let dir = runtime(&["capyctl_vllm_guard.py", "vllm_entry.py", "README.md"]);
        std::fs::create_dir(dir.path().join("__pycache__")).unwrap();
        std::fs::create_dir(dir.path().join("patches")).unwrap();
        chmod(&dir.path().join("__pycache__"), 0o755);
        chmod(&dir.path().join("patches"), 0o755);
        std::fs::write(dir.path().join("patches/fix.patch"), "x").unwrap();
        verify(dir.path(), VLLM_RUNTIME_FILES).unwrap();
    }

    // T21 T37: a subdirectory is held to the same owner-only rule as the
    // directory itself, and so is every module in it; a symlink anywhere is
    // refused, since it would import from outside the checked tree.
    #[test]
    fn subdirectories_follow_the_same_rule() {
        let dir = runtime(&["capyctl_vllm_guard.py", "vllm_entry.py"]);
        let sub = dir.path().join("tests");
        std::fs::create_dir(&sub).unwrap();
        chmod(&sub, 0o755);
        std::fs::write(sub.join("helper.py"), "# helper\n").unwrap();
        chmod(&sub.join("helper.py"), 0o644);
        verify(dir.path(), VLLM_RUNTIME_FILES).unwrap();
        chmod(&sub.join("helper.py"), 0o666);
        assert_eq!(
            verify(dir.path(), VLLM_RUNTIME_FILES).unwrap_err().path,
            sub.join("helper.py")
        );
        chmod(&sub.join("helper.py"), 0o644);
        chmod(&sub, 0o777);
        assert_eq!(
            verify(dir.path(), VLLM_RUNTIME_FILES).unwrap_err().path,
            sub
        );
        chmod(&sub, 0o755);
        let outside = runtime(&["other.py"]);
        std::os::unix::fs::symlink(outside.path(), dir.path().join("linked")).unwrap();
        assert_eq!(
            verify(dir.path(), VLLM_RUNTIME_FILES).unwrap_err().problem,
            "not a regular file or directory"
        );
    }

    // T21 T37
    #[test]
    fn a_writable_directory_a_symlink_or_a_missing_module_is_refused() {
        let dir = runtime(&["capyctl_vllm_guard.py", "vllm_entry.py"]);
        chmod(dir.path(), 0o777);
        assert_eq!(
            verify(dir.path(), VLLM_RUNTIME_FILES).unwrap_err().problem,
            "writable by other"
        );
        chmod(dir.path(), 0o755);

        let outside = runtime(&["capyctl_vllm_guard.py"]);
        let guard = dir.path().join("capyctl_vllm_guard.py");
        std::fs::remove_file(&guard).unwrap();
        std::os::unix::fs::symlink(outside.path().join("capyctl_vllm_guard.py"), &guard).unwrap();
        assert_eq!(
            verify(dir.path(), VLLM_RUNTIME_FILES).unwrap_err().problem,
            "not a regular file"
        );
        std::fs::remove_file(&guard).unwrap();
        assert_eq!(
            verify(dir.path(), VLLM_RUNTIME_FILES).unwrap_err().problem,
            "missing"
        );
        assert!(verify(&dir.path().join("absent"), VLLM_RUNTIME_FILES).is_err());
    }
}

//! Helpers for tests across the workspace. Nothing in a role calls these.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Writes an executable that a test runs next, so the run never fails with
/// `ETXTBSY` ("Text file busy").
///
/// A file written by this process is open for writing while it is written;
/// a process another test thread forks in that moment inherits the
/// descriptor and holds it until it execs, and running the file meanwhile
/// fails. Under a parallel test run that window is often open. So the
/// contents are staged in a sibling file, copied by a `cp` child (this
/// process never opens the copy for writing), and the copy is renamed over
/// `path`: the inode at `path` was never written by this process.
pub fn write_executable(path: &Path, contents: impl AsRef<[u8]>, mode: u32) -> std::io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("an executable path names a file"))?
        .to_string_lossy()
        .into_owned();
    let staged = path.with_file_name(format!(".{name}.staged"));
    let copied = path.with_file_name(format!(".{name}.copied"));
    std::fs::write(&staged, contents)?;
    let status = std::process::Command::new("cp")
        .arg("--")
        .arg(&staged)
        .arg(&copied)
        .status();
    std::fs::remove_file(&staged)?;
    if !status?.success() {
        return Err(std::io::Error::other(
            "cp could not copy the staged executable",
        ));
    }
    std::fs::set_permissions(&copied, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&copied, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_executable_runs_with_its_contents_and_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");
        write_executable(&path, "#!/bin/sh\necho first\n", 0o700).unwrap();
        write_executable(&path, "#!/bin/sh\necho second\n", 0o755).unwrap();
        let out = std::process::Command::new(&path).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "second\n");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["tool"], "nothing staged is left behind");
    }
}

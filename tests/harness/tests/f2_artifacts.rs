#![cfg(target_os = "linux")]

#[path = "../src/f2_artifacts.rs"]
mod f2_artifacts;
#[path = "../src/f2_records.rs"]
mod f2_records;
#[path = "../src/f2_timing.rs"]
mod f2_timing;
use f2_artifacts::{Artifact, ArtifactDirectory, ArtifactError};
use std::{
    fs::{self, File},
    io::Write,
    os::unix::fs::{symlink, PermissionsExt},
};

fn parent() -> (tempfile::TempDir, File) {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let file = File::open(dir.path()).unwrap();
    (dir, file)
}

#[test]
fn creates_private_files_and_syncs_real_metadata_without_overwrite() {
    let (dir, parent) = parent();
    let output = ArtifactDirectory::create(&parent, "run-001", 1024).unwrap();
    let mut manifest = output.create_file(Artifact::Manifest).unwrap();
    manifest.write_all(b"{\"schema\":1}\n").unwrap();
    manifest.sync().unwrap();
    let mut requests = output.create_file(Artifact::Requests).unwrap();
    requests.write_all(b"{}\n").unwrap();
    requests.sync().unwrap();
    let mut results = output.create_file(Artifact::Results).unwrap();
    results.write_all(b"{}\n").unwrap();
    results.sync().unwrap();
    let run = dir.path().join("run-001");
    assert_eq!(
        fs::metadata(&run).unwrap().permissions().mode() & 0o7777,
        0o700
    );
    for name in ["manifest.json", "requests.jsonl", "results.json"] {
        assert_eq!(
            fs::metadata(run.join(name)).unwrap().permissions().mode() & 0o7777,
            0o600
        );
    }
    assert_eq!(
        fs::read(run.join("manifest.json")).unwrap(),
        b"{\"schema\":1}\n"
    );
    assert!(matches!(
        ArtifactDirectory::create(&parent, "run-001", 1024),
        Err(ArtifactError::Exists)
    ));
    assert!(matches!(
        output.create_file(Artifact::Manifest),
        Err(ArtifactError::Exists)
    ));
    assert_eq!(
        fs::read(run.join("manifest.json")).unwrap(),
        b"{\"schema\":1}\n"
    );
    assert!(requests.write_all(b"more").is_err());
}

#[test]
fn shared_limit_stops_all_handles_before_exceeding_cap() {
    let (dir, parent) = parent();
    let output = ArtifactDirectory::create(&parent, "bounded", 5).unwrap();
    let mut a = output.create_file(Artifact::Manifest).unwrap();
    let mut b = output.create_file(Artifact::Requests).unwrap();
    a.write_all(b"abc").unwrap();
    b.write_all(b"de").unwrap();
    a.sync().unwrap();
    b.sync().unwrap();
    assert_eq!(b.write_all(b"f").unwrap_err().to_string(), "artifact_limit");
    assert_eq!(
        a.write_all(b"g").unwrap_err().to_string(),
        "artifact_stopped"
    );
    assert!(matches!(
        output.create_file(Artifact::Results),
        Err(ArtifactError::Stopped)
    ));
    assert_eq!(
        fs::read(dir.path().join("bounded/manifest.json")).unwrap(),
        b"abc"
    );
    assert_eq!(
        fs::read(dir.path().join("bounded/requests.jsonl")).unwrap(),
        b"de"
    );
}

#[test]
fn rejects_invalid_parent_names_and_budgets_without_creating_output() {
    let (dir, parent) = parent();
    for name in [
        "",
        ".",
        "..",
        "../escape",
        "/absolute",
        "two/names",
        "line\nname",
        "nul\0name",
        "snow☃",
    ] {
        assert!(matches!(
            ArtifactDirectory::create(&parent, name, 100),
            Err(ArtifactError::Invalid)
        ));
    }
    assert!(ArtifactDirectory::create(&parent, &"x".repeat(65), 100).is_err());
    for budget in [0, 104_857_601, u64::MAX] {
        assert!(matches!(
            ArtifactDirectory::create(&parent, "invalid", budget),
            Err(ArtifactError::Invalid)
        ));
    }
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        ArtifactDirectory::create(&parent, "public-parent", 100),
        Err(ArtifactError::Invalid)
    ));
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let regular = File::create(dir.path().join("file")).unwrap();
    assert!(matches!(
        ArtifactDirectory::create(&regular, "not-dir", 100),
        Err(ArtifactError::Invalid)
    ));
}

#[test]
fn refuses_directory_and_file_symlinks_without_following_them() {
    let (dir, parent) = parent();
    let elsewhere = tempfile::tempdir().unwrap();
    symlink(elsewhere.path(), dir.path().join("linked")).unwrap();
    assert!(matches!(
        ArtifactDirectory::create(&parent, "linked", 100),
        Err(ArtifactError::Exists)
    ));
    let output = ArtifactDirectory::create(&parent, "run", 100).unwrap();
    let victim = elsewhere.path().join("victim");
    fs::write(&victim, b"untouched").unwrap();
    symlink(&victim, dir.path().join("run/manifest.json")).unwrap();
    assert!(matches!(
        output.create_file(Artifact::Manifest),
        Err(ArtifactError::Exists)
    ));
    assert_eq!(fs::read(&victim).unwrap(), b"untouched");
    assert_eq!(fs::read_dir(elsewhere.path()).unwrap().count(), 1);
}

#[test]
fn descriptor_relative_creation_does_not_switch_to_replacement_parent_path() {
    let container = tempfile::tempdir().unwrap();
    let path = container.path().join("parent");
    fs::create_dir(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    let parent = File::open(&path).unwrap();
    fs::rename(&path, container.path().join("original")).unwrap();
    fs::create_dir(&path).unwrap();
    let output = ArtifactDirectory::create(&parent, "run", 100).unwrap();
    output
        .create_file(Artifact::Manifest)
        .unwrap()
        .sync()
        .unwrap();
    assert!(container
        .path()
        .join("original/run/manifest.json")
        .is_file());
    assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
}

#[test]
fn changed_permissions_or_added_hardlink_stop_writes_and_preserve_prefix() {
    for hardlink in [false, true] {
        let (dir, parent) = parent();
        let output = ArtifactDirectory::create(&parent, "run", 100).unwrap();
        let mut file = output.create_file(Artifact::Manifest).unwrap();
        file.write_all(b"original").unwrap();
        let path = dir.path().join("run/manifest.json");
        if hardlink {
            fs::hard_link(&path, dir.path().join("alias")).unwrap();
        } else {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        }
        assert_eq!(
            file.write_all(b"later").unwrap_err().to_string(),
            "artifact_invalid"
        );
        assert!(matches!(file.sync(), Err(ArtifactError::Stopped)));
        assert_eq!(fs::read(path).unwrap(), b"original");
    }
}

#[test]
fn concurrent_handles_cannot_spend_the_same_remaining_bytes() {
    let (dir, parent) = parent();
    let output = ArtifactDirectory::create(&parent, "run", 5).unwrap();
    let mut a = output.create_file(Artifact::Manifest).unwrap();
    let mut b = output.create_file(Artifact::Requests).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let other = barrier.clone();
    let first = std::thread::spawn(move || {
        barrier.wait();
        a.write_all(b"abc").is_ok()
    });
    let second = std::thread::spawn(move || {
        other.wait();
        b.write_all(b"def").is_ok()
    });
    assert_ne!(first.join().unwrap(), second.join().unwrap());
    let total = fs::metadata(dir.path().join("run/manifest.json"))
        .unwrap()
        .len()
        + fs::metadata(dir.path().join("run/requests.jsonl"))
            .unwrap()
            .len();
    assert_eq!(total, 3);
}

#[test]
fn closed_request_journal_writes_into_private_storage_after_manifest_sync() {
    use f2_records::{Engine, Mode, Outcome, RequestJournal, RequestRecord};
    let (dir, parent) = parent();
    let output = ArtifactDirectory::create(&parent, "run", 4096).unwrap();
    let mut manifest = output.create_file(Artifact::Manifest).unwrap();
    manifest
        .write_all(b"{\"test_metadata_only\":true}\n")
        .unwrap();
    manifest.sync().unwrap();
    let mut file = output.create_file(Artifact::Requests).unwrap();
    let mut journal = RequestJournal::new(&mut file, 2048).unwrap();
    journal
        .append(RequestRecord {
            case: 1,
            request: 0,
            engine: Engine::Sglang,
            mode: Mode::Streaming,
            outcome: Outcome::Failed,
        })
        .unwrap();
    journal
        .append(RequestRecord {
            case: 1,
            request: 1,
            engine: Engine::Vllm,
            mode: Mode::NonStreaming,
            outcome: Outcome::TimedOut,
        })
        .unwrap();
    journal
        .append(RequestRecord {
            case: 1,
            request: 2,
            engine: Engine::Sglang,
            mode: Mode::NonStreaming,
            outcome: Outcome::Completed(f2_timing::RequestTimeline {
                accepted_ns: 0,
                dispatched_ns: 1,
                backend_terminal_ns: 2,
                delivery_end_ns: 3,
                queue: None,
                activation: None,
                first_token_ns: None,
            }),
        })
        .unwrap();
    journal.finish().unwrap();
    file.sync().unwrap();
    let bytes = fs::read(dir.path().join("run/requests.jsonl")).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert_eq!(text.lines().count(), 3);
    assert!(text.starts_with("{\"sequence\":0,\"case\":1,\"request\":0,"));
    assert!(text.contains("\"outcome\":\"timed_out\"}"));
    assert!(text.contains("\"outcome\":\"completed\",\"total_ns\":3,"));
}

#[test]
fn retained_artifact_descriptors_are_close_on_exec() {
    let (dir, parent) = parent();
    let output = ArtifactDirectory::create(&parent, "run", 100).unwrap();
    let _file = output.create_file(Artifact::Manifest).unwrap();
    let run = dir.path().join("run");
    let manifest = run.join("manifest.json");
    let mut seen = 0;
    for entry in fs::read_dir("/proc/self/fd").unwrap() {
        let entry = entry.unwrap();
        let Ok(target) = fs::read_link(entry.path()) else {
            continue;
        };
        if target != run && target != manifest {
            continue;
        }
        let fd: i32 = entry.file_name().to_str().unwrap().parse().unwrap();
        // SAFETY: querying descriptor flags neither closes nor changes the fd.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0);
        seen += 1;
    }
    assert_eq!(seen, 2);
}

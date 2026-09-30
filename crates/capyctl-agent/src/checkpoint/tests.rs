use super::*;
use std::os::unix::fs::{symlink, PermissionsExt};

struct Store {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Store {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("models");
        std::fs::create_dir_all(&root).unwrap();
        Self { _dir: dir, root }
    }
    fn checkpoint(&self, name: &str, files: &[(&str, &[u8])]) -> PathBuf {
        let path = self.root.join(name);
        for (file, bytes) in files {
            let file = path.join(file);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, bytes).unwrap();
        }
        std::fs::create_dir_all(&path).unwrap();
        path
    }
}

const FILES: &[(&str, &[u8])] = &[
    ("config.json", b"{\"model_type\":\"toy\"}"),
    ("model-00001-of-00002.safetensors", b"weights-one"),
    ("model-00002-of-00002.safetensors", b"weights-two!"),
    ("tokenizer.json", b"{}"),
    ("nested/extra.txt", b"extra"),
];

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Set a file's modification time back to what it was, leaving its change time
/// at "now": what an in-place rewrite that hides itself looks like.
fn restore_mtime(path: &Path, st: &std::fs::Metadata) {
    use std::os::unix::fs::MetadataExt;
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let times = [
        libc::timespec {
            tv_sec: st.atime(),
            tv_nsec: st.atime_nsec(),
        },
        libc::timespec {
            tv_sec: st.mtime(),
            tv_nsec: st.mtime_nsec(),
        },
    ];
    // SAFETY: valid path and two timespecs.
    assert_eq!(
        unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), 0) },
        0
    );
}

// T14: the digest is the documented canonical encoding over sorted relative
// paths, sizes and file hashes; weights are the weight files' sizes.
#[test]
fn the_digest_is_the_canonical_manifest_encoding() {
    let store = Store::new();
    let checkpoint = store.checkpoint("toy", FILES);
    let measured = CheckpointVerifier::in_memory()
        .measure(&store.root, &checkpoint)
        .unwrap();
    let mut sorted: Vec<_> = FILES.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let mut canonical = MANIFEST_DOMAIN.to_vec();
    for (path, bytes) in &sorted {
        canonical.extend_from_slice(path.as_bytes());
        canonical.push(0);
        canonical.extend_from_slice(bytes.len().to_string().as_bytes());
        canonical.push(0);
        canonical.extend_from_slice(sha(bytes).as_bytes());
        canonical.push(b'\n');
    }
    assert_eq!(
        measured.manifest.digest,
        format!("sha256:{}", sha(&canonical))
    );
    assert!(capyctl_config::effective::is_checkpoint_digest(
        &measured.manifest.digest
    ));
    assert_eq!(measured.manifest.weights_bytes, 11 + 12);
    assert_eq!(measured.manifest.entries.len(), FILES.len());
    assert!(measured.full_rehash, "first placement hashes in full");
}

// T22: two hosts holding the same bytes agree, whatever their stat identity,
// store location or tool metadata.
#[test]
fn the_same_bytes_give_the_same_digest_on_any_host() {
    let first = Store::new();
    let second = Store::new();
    let a = first.checkpoint("toy", FILES);
    let b = second.checkpoint("elsewhere/toy", FILES);
    std::fs::create_dir_all(b.join(".cache/huggingface")).unwrap();
    std::fs::write(b.join(".cache/huggingface/download.metadata"), "etag 1").unwrap();
    std::fs::write(b.join(".gitattributes"), "*.safetensors lfs").unwrap();
    let verifier = CheckpointVerifier::in_memory();
    assert_eq!(
        verifier.measure(&first.root, &a).unwrap().manifest.digest,
        verifier.measure(&second.root, &b).unwrap().manifest.digest
    );
}

// T34 (Q9): an unchanged checkpoint is verified by stat identity plus a small
// file rehash; a changed file forces a full rehash and a stale digest is refused.
#[test]
fn a_changed_file_forces_a_full_rehash_and_the_stale_digest_is_refused() {
    let store = Store::new();
    let checkpoint = store.checkpoint("toy", FILES);
    let verifier = CheckpointVerifier::in_memory();
    let recorded = verifier
        .measure(&store.root, &checkpoint)
        .unwrap()
        .manifest
        .digest;
    let again = verifier
        .verify(&store.root, &checkpoint, &recorded)
        .unwrap();
    assert!(!again.full_rehash, "unchanged files reuse the cache");
    // Same size, modification time set back: only the change time moves.
    let shard = checkpoint.join("model-00001-of-00002.safetensors");
    let before = std::fs::metadata(&shard).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&shard, b"WEIGHTS-ONE").unwrap();
    restore_mtime(&shard, &before);
    assert_eq!(
        verifier
            .verify(&store.root, &checkpoint, &recorded)
            .unwrap_err(),
        CheckpointError::Mismatch
    );
    let measured = verifier.measure(&store.root, &checkpoint).unwrap();
    assert_ne!(measured.manifest.digest, recorded);
    // A changed small file is caught even when it keeps its whole identity in
    // the cache: small files are always rehashed.
    let current = measured.manifest.digest;
    let key = checkpoint
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    {
        let mut memory = verifier.memory.lock().unwrap();
        let record = memory.get_mut(&key).unwrap();
        let config = record
            .files
            .iter_mut()
            .find(|f| f.path == "config.json")
            .unwrap();
        config.sha256 = sha(b"a forged cached hash");
    }
    let rehashed = verifier.verify(&store.root, &checkpoint, &current).unwrap();
    assert!(rehashed.full_rehash);
}

// T34: added or removed files change the manifest and force a full rehash.
#[test]
fn an_added_or_removed_file_changes_the_digest() {
    let store = Store::new();
    let checkpoint = store.checkpoint("toy", FILES);
    let verifier = CheckpointVerifier::in_memory();
    let recorded = verifier
        .measure(&store.root, &checkpoint)
        .unwrap()
        .manifest
        .digest;
    std::fs::write(checkpoint.join("generation_config.json"), "{}").unwrap();
    let added = verifier.measure(&store.root, &checkpoint).unwrap();
    assert!(added.full_rehash);
    assert_ne!(added.manifest.digest, recorded);
    std::fs::remove_file(checkpoint.join("generation_config.json")).unwrap();
    std::fs::remove_file(checkpoint.join("nested/extra.txt")).unwrap();
    assert_ne!(
        verifier
            .measure(&store.root, &checkpoint)
            .unwrap()
            .manifest
            .digest,
        recorded
    );
}

// T33 (Q9): the per-host cache survives an agent restart.
#[test]
fn the_cache_persists_across_verifier_restarts() {
    let store = Store::new();
    let checkpoint = store.checkpoint("toy", FILES);
    let cache = tempfile::tempdir().unwrap();
    let first = CheckpointVerifier::with_cache_dir(cache.path().join("checkpoints"));
    let recorded = first
        .measure(&store.root, &checkpoint)
        .unwrap()
        .manifest
        .digest;
    let mode = std::fs::metadata(cache.path().join("checkpoints"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
    let restarted = CheckpointVerifier::with_cache_dir(cache.path().join("checkpoints"));
    let verified = restarted
        .verify(&store.root, &checkpoint, &recorded)
        .unwrap();
    assert!(!verified.full_rehash);
}

// T37 T33: ADR 0014 §7. The stat cache is trusted evidence (a cached hash
// skips rehashing), so it is used only from a private directory this user owns
// that is not a symlink, and only from private files: a cache another account
// could plant or redirect is ignored, never believed, and never written.
#[test]
fn the_cache_is_used_only_from_a_private_directory_and_files() {
    let store = Store::new();
    let checkpoint = store.checkpoint("toy", FILES);
    let cache = tempfile::tempdir().unwrap();
    // A symlinked cache directory is neither written nor read.
    let real = cache.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
    let linked = cache.path().join("linked");
    std::os::unix::fs::symlink(&real, &linked).unwrap();
    let verifier = CheckpointVerifier::with_cache_dir(linked.clone());
    let recorded = verifier
        .measure(&store.root, &checkpoint)
        .unwrap()
        .manifest
        .digest;
    assert_eq!(
        std::fs::read_dir(&real).unwrap().count(),
        0,
        "nothing written through a link"
    );
    // A group- or other-writable directory is not trusted either.
    let shared = cache.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
    CheckpointVerifier::with_cache_dir(shared.clone())
        .measure(&store.root, &checkpoint)
        .unwrap();
    assert_eq!(std::fs::read_dir(&shared).unwrap().count(), 0);
    // A private directory is used, and a planted cache file that is a symlink
    // or writable by others is ignored on read (a full rehash follows).
    let private = cache.path().join("private");
    CheckpointVerifier::with_cache_dir(private.clone())
        .measure(&store.root, &checkpoint)
        .unwrap();
    let file = std::fs::read_dir(&private)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o666)).unwrap();
    let restarted = CheckpointVerifier::with_cache_dir(private.clone());
    assert!(
        restarted
            .verify(&store.root, &checkpoint, &recorded)
            .unwrap()
            .full_rehash
    );
    let moved = cache.path().join("moved.json");
    std::fs::rename(&file, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, &file).unwrap();
    let restarted = CheckpointVerifier::with_cache_dir(private);
    assert!(
        restarted
            .verify(&store.root, &checkpoint, &recorded)
            .unwrap()
            .full_rehash
    );
}

// T37, ADR 0014 §7: links are followed only inside the model store (the Hugging
// Face snapshot layout); the manifest names the link, with the target's bytes.
#[test]
fn links_inside_the_store_are_followed_as_the_hugging_face_layout() {
    let store = Store::new();
    let blobs = store.root.join("hub/models--toy/blobs");
    std::fs::create_dir_all(&blobs).unwrap();
    let snapshot = store.root.join("hub/models--toy/snapshots/abc");
    std::fs::create_dir_all(&snapshot).unwrap();
    for (name, bytes) in FILES.iter().filter(|(name, _)| !name.contains('/')) {
        std::fs::write(blobs.join(sha(bytes)), bytes).unwrap();
        symlink(format!("../../blobs/{}", sha(bytes)), snapshot.join(name)).unwrap();
    }
    let plain = Store::new();
    let copy = plain.checkpoint(
        "toy",
        &FILES
            .iter()
            .copied()
            .filter(|(name, _)| !name.contains('/'))
            .collect::<Vec<_>>(),
    );
    let verifier = CheckpointVerifier::in_memory();
    assert_eq!(
        verifier
            .measure(&store.root, &snapshot)
            .unwrap()
            .manifest
            .digest,
        verifier
            .measure(&plain.root, &copy)
            .unwrap()
            .manifest
            .digest
    );
    // A checkpoint path that is itself a link inside the store is fine.
    symlink(&snapshot, store.root.join("toy")).unwrap();
    assert!(verifier
        .measure(&store.root, &store.root.join("toy"))
        .is_ok());
}

// T37, ADR 0014 §7: escapes are refused, never followed.
#[test]
fn links_that_escape_the_store_or_name_directories_are_refused() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "not a checkpoint").unwrap();
    type Case<'a> = &'a dyn Fn(&Path, &Path);
    let cases: &[Case] = &[
        &|checkpoint, _| {
            symlink(
                outside.path().join("secret"),
                checkpoint.join("escape.json"),
            )
            .unwrap()
        },
        &|checkpoint, _| {
            symlink(
                "../../../../../../../../etc/hostname",
                checkpoint.join("up.json"),
            )
            .unwrap()
        },
        &|checkpoint, store| {
            std::fs::create_dir_all(store.join("other")).unwrap();
            symlink(store.join("other"), checkpoint.join("dir")).unwrap()
        },
        &|checkpoint, _| {
            std::fs::write(checkpoint.join("real.json"), "{}").unwrap();
            symlink(checkpoint.join("real.json"), checkpoint.join("hop1")).unwrap();
            symlink(checkpoint.join("hop1"), checkpoint.join("hop2")).unwrap()
        },
        &|checkpoint, _| symlink(checkpoint.join("missing"), checkpoint.join("dangling")).unwrap(),
    ];
    for (index, case) in cases.iter().enumerate() {
        let store = Store::new();
        let checkpoint = store.checkpoint("toy", FILES);
        case(&checkpoint, &store.root);
        let error = CheckpointVerifier::in_memory()
            .measure(&store.root, &checkpoint)
            .unwrap_err();
        assert!(
            matches!(
                error,
                CheckpointError::UnsafeFile | CheckpointError::Changed
            ),
            "case {index}: {error:?}"
        );
    }
}

// T37, SPEC §13.3: the checkpoint must be a directory inside the host's model store.
#[test]
fn a_checkpoint_outside_the_model_store_is_refused() {
    let store = Store::new();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("config.json"), "{}").unwrap();
    let verifier = CheckpointVerifier::in_memory();
    for checkpoint in [
        outside.path().to_path_buf(),
        store.root.clone(),
        store.root.join("missing"),
        PathBuf::from("relative/toy"),
    ] {
        assert_eq!(
            verifier.measure(&store.root, &checkpoint).unwrap_err(),
            CheckpointError::InvalidRoot,
            "{checkpoint:?}"
        );
    }
    // A link from inside the store to a directory outside it resolves outside.
    symlink(outside.path(), store.root.join("escape")).unwrap();
    assert_eq!(
        verifier
            .measure(&store.root, &store.root.join("escape"))
            .unwrap_err(),
        CheckpointError::InvalidRoot
    );
}

// T37: the manifest cannot encode names with newlines, and special files are not
// checkpoint content.
#[test]
fn unencodable_names_and_special_files_are_refused() {
    let store = Store::new();
    let checkpoint = store.checkpoint("toy", FILES);
    std::fs::write(checkpoint.join("bad\nname"), "x").unwrap();
    assert_eq!(
        CheckpointVerifier::in_memory()
            .measure(&store.root, &checkpoint)
            .unwrap_err(),
        CheckpointError::UnsafeFile
    );
    let store = Store::new();
    let checkpoint = store.checkpoint("toy", FILES);
    let fifo = CString::new(checkpoint.join("pipe").as_os_str().as_bytes()).unwrap();
    // SAFETY: valid path.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert_eq!(
        CheckpointVerifier::in_memory()
            .measure(&store.root, &checkpoint)
            .unwrap_err(),
        CheckpointError::UnsafeFile
    );
}

#[test]
fn verify_refuses_an_expectation_that_is_not_a_digest() {
    let store = Store::new();
    let checkpoint = store.checkpoint("toy", FILES);
    assert_eq!(
        CheckpointVerifier::in_memory()
            .verify(&store.root, &checkpoint, "sha256:model")
            .unwrap_err(),
        CheckpointError::Mismatch
    );
}

// Performance evidence (not a gate): single-file hash throughput of this build.
#[test]
#[ignore = "throughput measurement; run with --ignored --nocapture in release"]
fn hash_throughput() {
    let store = Store::new();
    let checkpoint = store.root.join("big");
    std::fs::create_dir_all(&checkpoint).unwrap();
    let block = vec![7u8; 1 << 20];
    for shard in 0..4 {
        let mut file =
            std::fs::File::create(checkpoint.join(format!("s{shard}.safetensors"))).unwrap();
        for _ in 0..512 {
            std::io::Write::write_all(&mut file, &block).unwrap();
        }
    }
    let verifier = CheckpointVerifier::in_memory();
    let started = std::time::Instant::now();
    let measured = verifier.measure(&store.root, &checkpoint).unwrap();
    let full = started.elapsed();
    let started = std::time::Instant::now();
    verifier
        .verify(&store.root, &checkpoint, &measured.manifest.digest)
        .unwrap();
    let cached = started.elapsed();
    let gib = measured.manifest.total_bytes as f64 / f64::from(1u32 << 30);
    println!(
        "full: {gib:.1} GiB in {full:?} = {:.2} GiB/s; cached re-verify {cached:?}",
        gib / full.as_secs_f64()
    );
}

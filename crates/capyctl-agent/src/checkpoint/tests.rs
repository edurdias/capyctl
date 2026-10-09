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
        config.sha256 = Some(sha(b"a forged cached hash"));
    }
    let rehashed = verifier.verify(&store.root, &checkpoint, &current).unwrap();
    assert!(rehashed.full_rehash);
}

// T30 (ADR 0028 §7): Prepare reads the digest the cache already proves and
// never hashes a checkpoint in full: nothing measured, or a changed file,
// has no known digest; measuring again makes it known.
#[test]
fn a_known_digest_comes_from_the_cache_only() {
    let store = Store::new();
    let checkpoint = store.checkpoint("toy", FILES);
    let verifier = CheckpointVerifier::in_memory();
    assert_eq!(verifier.known_digest(&store.root, &checkpoint), None);
    let recorded = verifier
        .measure(&store.root, &checkpoint)
        .unwrap()
        .manifest
        .digest;
    assert_eq!(
        verifier.known_digest(&store.root, &checkpoint),
        Some(recorded.clone())
    );
    let shard = checkpoint.join("model-00001-of-00002.safetensors");
    let before = std::fs::metadata(&shard).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&shard, b"WEIGHTS-ONE").unwrap();
    restore_mtime(&shard, &before);
    assert_eq!(verifier.known_digest(&store.root, &checkpoint), None);
    let measured = verifier.measure(&store.root, &checkpoint).unwrap();
    assert_ne!(measured.manifest.digest, recorded);
    assert_eq!(
        verifier.known_digest(&store.root, &checkpoint),
        Some(measured.manifest.digest)
    );
}

// T30 (ADR 0028 §7, owner decision 10): reading a known digest changes nothing
// on the host: an absent cache directory stays absent; measuring creates it,
// and a fresh verifier then reads the persisted record without writing.
#[test]
fn a_known_digest_never_creates_the_cache_directory() {
    let store = Store::new();
    let checkpoint = store.checkpoint("toy", FILES);
    let state = tempfile::tempdir().unwrap();
    let cache = state.path().join("checkpoint-cache");
    let verifier = CheckpointVerifier::with_cache_dir(cache.clone());
    assert_eq!(verifier.known_digest(&store.root, &checkpoint), None);
    assert!(!cache.exists());
    let recorded = verifier
        .measure(&store.root, &checkpoint)
        .unwrap()
        .manifest
        .digest;
    assert!(cache.is_dir());
    let restarted = CheckpointVerifier::with_cache_dir(cache.clone());
    let entries = || std::fs::read_dir(&cache).unwrap().count();
    let before = entries();
    assert_eq!(
        restarted.known_digest(&store.root, &checkpoint),
        Some(recorded)
    );
    assert_eq!(entries(), before);
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

// T37, ADR 0014 §7: escapes are refused, never followed (chains: A5).
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

// T37, SPEC §13.3: the checkpoint must be a directory; the model store itself,
// a missing path and a relative path are refused.
#[test]
fn a_checkpoint_that_is_not_a_directory_below_a_root_is_refused() {
    let store = Store::new();
    let verifier = CheckpointVerifier::in_memory();
    for checkpoint in [
        store.root.clone(),
        store.root.join("missing"),
        PathBuf::from("relative/toy"),
        PathBuf::from("/"),
    ] {
        assert_eq!(
            verifier.measure(&store.root, &checkpoint).unwrap_err(),
            CheckpointError::InvalidRoot,
            "{checkpoint:?}"
        );
    }
}

// T37 (found live 2026-10-03: the guides name an absolute path as a model): a
// checkpoint outside the model store is measured inside its own root, the
// Hugging Face repository directory of a cache snapshot, else its parent, with
// links confined there. It measures as the same bytes stored plainly.
#[test]
fn a_checkpoint_outside_the_model_store_is_measured_inside_its_own_root() {
    let store = Store::new();
    let verifier = CheckpointVerifier::in_memory();
    let plain = Store::new();
    let flat: Vec<_> = FILES
        .iter()
        .copied()
        .filter(|(name, _)| !name.contains('/'))
        .collect();
    let copy = plain.checkpoint("toy", &flat);
    let expected = verifier
        .measure(&plain.root, &copy)
        .unwrap()
        .manifest
        .digest;
    // A Hugging Face cache outside the store: snapshot files link to the
    // repository's blobs, and a large one on to the hub's shared blobs.
    let cache = tempfile::tempdir().unwrap();
    let hub = cache.path().join("hub");
    let repository = hub.join("models--org--toy");
    let blobs = repository.join("blobs");
    let snapshot = repository.join("snapshots/abc");
    std::fs::create_dir_all(&blobs).unwrap();
    std::fs::create_dir_all(&snapshot).unwrap();
    for (index, (name, bytes)) in flat.iter().enumerate() {
        let digest = sha(bytes);
        if index == 1 {
            let shared = hub.join("blobs").join(&digest[..2]);
            std::fs::create_dir_all(&shared).unwrap();
            std::fs::write(shared.join(&digest), bytes).unwrap();
            symlink(
                format!("../../blobs/{}/{digest}", &digest[..2]),
                blobs.join(&digest),
            )
            .unwrap();
        } else {
            std::fs::write(blobs.join(&digest), bytes).unwrap();
        }
        symlink(format!("../../blobs/{digest}"), snapshot.join(name)).unwrap();
    }
    assert_eq!(
        verifier
            .measure(&store.root, &snapshot)
            .unwrap()
            .manifest
            .digest,
        expected
    );
    assert_eq!(
        verifier.size(&store.root, &snapshot).unwrap().weights_bytes,
        verifier.size(&plain.root, &copy).unwrap().weights_bytes
    );
    // A plain directory outside the store.
    let outside = tempfile::tempdir().unwrap();
    let elsewhere = outside.path().join("toy");
    std::fs::create_dir_all(&elsewhere).unwrap();
    for (name, bytes) in &flat {
        std::fs::write(elsewhere.join(name), bytes).unwrap();
    }
    assert_eq!(
        verifier
            .measure(&store.root, &elsewhere)
            .unwrap()
            .manifest
            .digest,
        expected
    );
    // A link out of its own root is refused, as one out of the store is.
    let other = tempfile::tempdir().unwrap();
    std::fs::write(other.path().join("secret"), "not a checkpoint").unwrap();
    symlink(other.path().join("secret"), elsewhere.join("escape.json")).unwrap();
    assert_eq!(
        verifier.measure(&store.root, &elsewhere).unwrap_err(),
        CheckpointError::UnsafeFile
    );
}

// T37 (ADR 0014 open issue 3): a checkpoint outside the model store whose root
// other users may write is refused, and a draft model stays inside its own
// approved root.
#[test]
fn an_outside_root_others_may_write_or_a_drafter_outside_its_root_is_refused() {
    let store = Store::new();
    let verifier = CheckpointVerifier::in_memory();
    let outside = tempfile::tempdir().unwrap();
    let shared = outside.path().join("shared");
    let checkpoint = shared.join("toy");
    std::fs::create_dir_all(&checkpoint).unwrap();
    std::fs::write(checkpoint.join("config.json"), "{}").unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(
        verifier.measure(&store.root, &checkpoint).unwrap_err(),
        CheckpointError::InvalidRoot
    );
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(verifier.measure(&store.root, &checkpoint).is_ok());
    let drafter = capyctl_config::effective::DrafterLocation {
        root: store.root.clone(),
        path: checkpoint.clone(),
        outside_root_allowed: false,
    };
    assert_eq!(
        verifier.drafter_weights(Some(&drafter)).unwrap_err(),
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

/// The real `huggingface_hub` shared-blob layout: a snapshot file links to a
/// per-model blob that links again into the hub-wide blob store.
fn chained_cache(store: &Store) -> PathBuf {
    let hub = store.root.join("hub");
    let snapshot = hub.join("models--toy/snapshots/abc");
    std::fs::create_dir_all(&snapshot).unwrap();
    std::fs::create_dir_all(hub.join("models--toy/blobs")).unwrap();
    for (name, bytes) in FILES {
        let digest = sha(bytes);
        let shared = hub.join("blobs").join(&digest[..2]);
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join(&digest), bytes).unwrap();
        symlink(
            format!("../../blobs/{}/{digest}", &digest[..2]),
            hub.join("models--toy/blobs").join(&digest),
        )
        .unwrap();
        let link = snapshot.join(name);
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        let up = "../".repeat(name.matches('/').count());
        symlink(format!("{up}../../blobs/{digest}"), &link).unwrap();
    }
    snapshot
}

// T37, ADR 0014 §7 (A5): a two-hop chain inside the store measures to the
// digest of the same bytes stored plainly.
#[test]
fn a_two_hop_hugging_face_chain_measures_like_a_plain_copy() {
    let store = Store::new();
    let snapshot = chained_cache(&store);
    let plain = Store::new();
    let copy = plain.checkpoint("toy", FILES);
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
            .digest,
    );
}

// T37, ADR 0014 §7 (A5): the second hop is resolved against the directory
// holding the first link's target, not against the snapshot.
#[test]
fn a_relative_second_hop_resolves_against_its_own_directory() {
    let store = Store::new();
    let snapshot = chained_cache(&store);
    // A decoy where a snapshot-relative resolver would land.
    let decoy = snapshot.join("../../blobs");
    for (_, bytes) in FILES {
        let digest = sha(bytes);
        std::fs::create_dir_all(decoy.join(&digest[..2])).unwrap();
        std::fs::write(decoy.join(&digest[..2]).join(&digest), b"decoy").unwrap();
    }
    let plain = Store::new();
    let copy = plain.checkpoint("toy", FILES);
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
            .digest,
    );
}

// T22, ADR 0014 §7 (A5): following chains leaves the digests of plain files
// and one-hop links exactly as they were recorded before.
#[test]
fn plain_and_one_hop_digests_are_unchanged() {
    const RECORDED: &str =
        "sha256:cea4cc18b4de2565d4f575cbfea2b281b111015101885a5f2b311845363b4c00";
    let plain = Store::new();
    let copy = plain.checkpoint("toy", FILES);
    let linked = Store::new();
    let blobs = linked.root.join("blobs");
    std::fs::create_dir_all(&blobs).unwrap();
    let snapshot = linked.root.join("toy");
    for (name, bytes) in FILES {
        std::fs::write(blobs.join(sha(bytes)), bytes).unwrap();
        let link = snapshot.join(name);
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(blobs.join(sha(bytes)), &link).unwrap();
    }
    let verifier = CheckpointVerifier::in_memory();
    assert_eq!(
        verifier
            .measure(&plain.root, &copy)
            .unwrap()
            .manifest
            .digest,
        RECORDED
    );
    assert_eq!(
        verifier
            .measure(&linked.root, &snapshot)
            .unwrap()
            .manifest
            .digest,
        RECORDED
    );
}

fn chain(checkpoint: &Path, links: usize) {
    std::fs::write(checkpoint.join("hop0.json"), "{}").unwrap();
    for hop in 1..=links {
        symlink(
            format!("hop{}.json", hop - 1),
            checkpoint.join(format!("hop{hop}.json")),
        )
        .unwrap();
    }
}

// T37, ADR 0014 §7 (A5): eight hops are followed; a ninth is refused.
#[test]
fn eight_hops_are_followed_and_a_ninth_is_refused() {
    for (links, ok) in [(8, true), (9, false)] {
        let store = Store::new();
        let checkpoint = store.checkpoint("toy", FILES);
        chain(&checkpoint, links);
        let result = CheckpointVerifier::in_memory().measure(&store.root, &checkpoint);
        if ok {
            assert!(result.is_ok(), "{links} links: {result:?}");
        } else {
            assert!(
                matches!(result, Err(CheckpointError::UnsafeFile)),
                "{links} links: {result:?}"
            );
        }
    }
}

// T37, ADR 0014 §7 (A5): a loop, a chain that leaves the store and a chain
// that ends at a directory are refused.
#[test]
fn chains_that_loop_escape_or_end_at_a_directory_are_refused() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "x").unwrap();
    type Case<'a> = &'a dyn Fn(&Path, &Path);
    let cases: &[Case] = &[
        &|c, _| {
            symlink("b.json", c.join("a.json")).unwrap();
            symlink("a.json", c.join("b.json")).unwrap();
        },
        &|c, _| {
            symlink(outside.path().join("secret"), c.join("out1")).unwrap();
            symlink("out1", c.join("out2")).unwrap();
        },
        &|c, s| {
            std::fs::create_dir_all(s.join("other")).unwrap();
            symlink(s.join("other"), c.join("d1")).unwrap();
            symlink("d1", c.join("d2")).unwrap();
        },
    ];
    for (index, case) in cases.iter().enumerate() {
        let store = Store::new();
        let checkpoint = store.checkpoint("toy", FILES);
        case(&checkpoint, &store.root);
        let error = CheckpointVerifier::in_memory()
            .measure(&store.root, &checkpoint)
            .unwrap_err();
        assert!(
            matches!(error, CheckpointError::UnsafeFile),
            "case {index}: {error:?}"
        );
    }
}

// ADR 0014 §5 amendment A6 (found live 2026-10-02): a draft model's weight
// files are counted with the same confined walk; none counts nothing, and a
// draft directory that escapes its approved root is refused, not counted.
#[test]
fn a_draft_models_weights_are_sized_inside_its_approved_root() {
    let store = Store::new();
    let drafter = store.checkpoint("drafter", FILES);
    let verifier = CheckpointVerifier::in_memory();
    let location = capyctl_config::effective::DrafterLocation {
        root: store.root.clone(),
        path: drafter.clone(),
        outside_root_allowed: false,
    };
    assert_eq!(verifier.drafter_weights(Some(&location)).unwrap(), 11 + 12);
    assert_eq!(verifier.drafter_weights(None).unwrap(), 0);
    let outside = Store::new();
    let escaping = capyctl_config::effective::DrafterLocation {
        root: outside.root.clone(),
        path: drafter.clone(),
        outside_root_allowed: false,
    };
    assert!(verifier.drafter_weights(Some(&escaping)).is_err());
    // ADR 0008 amendment 2026-10-08: a declared local drafter outside the
    // model store is sized inside its own root, as a local model would be.
    let declared = capyctl_config::effective::DrafterLocation {
        outside_root_allowed: true,
        ..escaping
    };
    assert_eq!(verifier.drafter_weights(Some(&declared)).unwrap(), 11 + 12);
}

/// The manifest digest a fresh verifier measures by reading every file.
fn measured_in_full(store: &Store, checkpoint: &Path) -> String {
    let measured = CheckpointVerifier::in_memory()
        .measure(&store.root, checkpoint)
        .unwrap();
    assert!(measured.full_rehash);
    measured.manifest.digest
}

/// Files up to 5 bytes count as small here: `tokenizer.json` (2) and
/// `nested/extra.txt` (5), 7 bytes in all. The rest (61 bytes) are large.
const SMALL: u64 = 5;
const SMALL_BYTES: u64 = 7;

// T34 (ADR 0014 §7, amendment of 2026-10-08): a local checkpoint's declared
// canonical digest is trusted only when the host's policy allows it, and then
// only the small files are read. Without the policy, or for a declaration
// that is not a canonical digest, the checkpoint is measured in full.
#[test]
fn a_declared_digest_is_trusted_only_when_the_host_allows_it() {
    let store = Store::new();
    let checkpoint = store.checkpoint("toy", FILES);
    let declared = measured_in_full(&store, &checkpoint);
    let total: u64 = FILES.iter().map(|(_, bytes)| bytes.len() as u64).sum();

    let refusing = CheckpointVerifier::in_memory().with_small_file_limit(SMALL);
    let measured = refusing
        .measure_declared(&store.root, &checkpoint, Some(&declared))
        .unwrap();
    assert_eq!(measured.provenance, DigestProvenance::Measured);
    assert!(measured.full_rehash);
    assert_eq!(refusing.bytes_hashed(), total, "every file was read");

    let trusting = CheckpointVerifier::in_memory().with_small_file_limit(SMALL);
    trusting.set_declared_trust(true);
    let trusted = trusting
        .measure_declared(&store.root, &checkpoint, Some(&declared))
        .unwrap();
    assert_eq!(trusted.provenance, DigestProvenance::DeclaredTrusted);
    assert!(!trusted.full_rehash);
    assert_eq!(trusted.manifest.digest, declared);
    assert_eq!(trusted.manifest.weights_bytes, 11 + 12);
    assert_eq!(
        trusting.bytes_hashed(),
        SMALL_BYTES,
        "only small files read"
    );
    // A launch verifies it from the cache, still reading only small files.
    let launch = trusting
        .verify_declared(&store.root, &checkpoint, &declared, Some(&declared))
        .unwrap();
    assert_eq!(launch.provenance, DigestProvenance::DeclaredTrusted);
    assert_eq!(trusting.bytes_hashed(), 2 * SMALL_BYTES);
    assert_eq!(
        trusting.known_digest(&store.root, &checkpoint),
        Some(declared.clone())
    );

    // A label is not a digest: nothing to trust, so it is measured.
    let other = store.checkpoint("other", FILES);
    let labelled = trusting
        .measure_declared(&store.root, &other, Some("sha256:toy"))
        .unwrap();
    assert_eq!(labelled.provenance, DigestProvenance::Measured);
    assert!(labelled.full_rehash);

    // The host turns the policy off: the trusted digest no longer stands and
    // the checkpoint is measured in full.
    trusting.set_declared_trust(false);
    assert_eq!(trusting.known_digest(&store.root, &checkpoint), None);
    let off = trusting
        .measure_declared(&store.root, &checkpoint, Some(&declared))
        .unwrap();
    assert_eq!(off.provenance, DigestProvenance::Measured);
    assert!(off.full_rehash);
    assert_eq!(off.manifest.digest, declared);
}

// T34 (ADR 0014 §7, amendment of 2026-10-08): a trusted declaration seeds the
// stat cache, so a file changed afterwards forces a full measurement and the
// recorded digest is refused; the declaration is never trusted again for the
// changed checkpoint.
#[test]
fn a_trusted_declaration_still_catches_a_changed_file() {
    let store = Store::new();
    let checkpoint = store.checkpoint("toy", FILES);
    let declared = measured_in_full(&store, &checkpoint);
    let verifier = CheckpointVerifier::in_memory().with_small_file_limit(SMALL);
    verifier.set_declared_trust(true);
    let trusted = verifier
        .measure_declared(&store.root, &checkpoint, Some(&declared))
        .unwrap();
    assert_eq!(trusted.provenance, DigestProvenance::DeclaredTrusted);
    // Same size, modification time set back: only the change time moves.
    let shard = checkpoint.join("model-00001-of-00002.safetensors");
    let before = std::fs::metadata(&shard).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&shard, b"WEIGHTS-ONE").unwrap();
    restore_mtime(&shard, &before);
    assert_eq!(
        verifier
            .verify_declared(&store.root, &checkpoint, &declared, Some(&declared))
            .unwrap_err(),
        CheckpointError::Mismatch
    );
    let measured = verifier
        .measure_declared(&store.root, &checkpoint, Some(&declared))
        .unwrap();
    assert_eq!(measured.provenance, DigestProvenance::Measured);
    assert_ne!(measured.manifest.digest, declared);
    assert_eq!(
        measured.manifest.digest,
        measured_in_full(&store, &checkpoint)
    );
}

use capyctl_agent::identity_storage::IdentityDirectory;
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};

fn directory() -> tempfile::TempDir {
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

// T04, T33: an unrelated fork must not prolong the parent's ownership lifetime.
#[test]
fn inherited_descriptor_does_not_retain_a_dropped_identity_owner() {
    let dir = directory();
    let storage = IdentityDirectory::open(dir.path()).unwrap();
    let mut pipe = [0; 2];
    assert_eq!(
        unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) },
        0
    );
    let child = unsafe { libc::fork() };
    assert!(child >= 0);
    if child == 0 {
        // Only async-signal-safe syscalls in the child of this multithreaded test.
        unsafe {
            libc::close(pipe[1]);
            let mut byte = 0u8;
            libc::read(pipe[0], (&mut byte as *mut u8).cast(), 1);
            libc::_exit(0);
        }
    }
    unsafe {
        libc::close(pipe[0]);
    }
    drop(storage);
    let reopened = IdentityDirectory::open(dir.path());
    // Always reap before asserting, including the intentionally witnessed red.
    unsafe {
        libc::close(pipe[1]);
        libc::waitpid(child, std::ptr::null_mut(), 0);
    }
    assert!(
        reopened.is_ok(),
        "forked child retained the parent's released lock"
    );
}
fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

// T05/T06: identity publication survives reopen without regenerating a key.
#[test]
fn exclusive_bundle_survives_reopen_and_never_overwrites() {
    let dir = directory();
    let storage = IdentityDirectory::open(dir.path()).unwrap();
    assert!(storage.read_bundle("host.json").unwrap().is_none());
    storage
        .create_bundle("host.json", b"persisted identity")
        .unwrap();
    assert!(storage.create_bundle("host.json", b"replacement").is_err());
    assert_eq!(
        fs::metadata(dir.path().join("host.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o600
    );
    drop(storage);
    let storage = IdentityDirectory::open(dir.path()).unwrap();
    assert_eq!(
        storage.read_bundle("host.json").unwrap().unwrap(),
        b"persisted identity"
    );
}

// T06: renewal replaces only the identity revision the caller read.
#[test]
fn replacement_requires_exact_previous_bundle() {
    let dir = directory();
    let storage = IdentityDirectory::open(dir.path()).unwrap();
    storage.create_bundle("host.json", b"pending").unwrap();
    assert!(storage
        .replace_bundle("host.json", &digest(b"other"), b"issued")
        .is_err());
    assert_eq!(
        storage.read_bundle("host.json").unwrap().unwrap(),
        b"pending"
    );
    storage
        .replace_bundle("host.json", &digest(b"pending"), b"issued")
        .unwrap();
    assert!(storage
        .replace_bundle("host.json", &digest(b"pending"), b"replay")
        .is_err());
    assert_eq!(
        storage.read_bundle("host.json").unwrap().unwrap(),
        b"issued"
    );
}

// T06: two renewal callers sharing one handle cannot both replace one revision.
#[test]
fn concurrent_replacement_has_one_winner() {
    let dir = directory();
    let storage = std::sync::Arc::new(IdentityDirectory::open(dir.path()).unwrap());
    storage.create_bundle("host.json", b"pending").unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let threads: Vec<_> = (0..8)
        .map(|index| {
            let storage = storage.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                storage
                    .replace_bundle(
                        "host.json",
                        &digest(b"pending"),
                        format!("issued-{index}").as_bytes(),
                    )
                    .is_ok()
            })
        })
        .collect();
    assert_eq!(
        threads
            .into_iter()
            .filter_map(|thread| thread.join().unwrap().then_some(()))
            .count(),
        1
    );
}

// T05/T37: unsafe existing files never become a request to mint another key.
#[test]
fn unsafe_leaf_and_partial_files_are_rejected_without_repair() {
    let dir = directory();
    let storage = IdentityDirectory::open(dir.path()).unwrap();
    storage.create_bundle("host.json", b"identity").unwrap();
    let path = dir.path().join("host.json");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    assert!(storage.read_bundle("host.json").is_err());
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
        0o640
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::hard_link(&path, dir.path().join("alias.json")).unwrap();
    assert!(storage.read_bundle("host.json").is_err());
    fs::remove_file(dir.path().join("alias.json")).unwrap();
    symlink(&path, dir.path().join("link.json")).unwrap();
    assert!(storage.read_bundle("link.json").is_err());
    assert!(storage
        .replace_bundle("link.json", &digest(b"identity"), b"changed")
        .is_err());
    fs::write(&path, []).unwrap();
    assert!(storage.read_bundle("host.json").is_err());
    assert!(storage.create_bundle("host.json", b"fresh key").is_err());
}

// T37: role files cannot escape the private identity directory or amplify reads.
#[test]
fn bounds_and_names_fail_before_publication() {
    let dir = directory();
    let storage = IdentityDirectory::open(dir.path()).unwrap();
    for name in [
        "",
        ".",
        "..",
        "../escape",
        "/absolute",
        "sub/key",
        "identity.lock",
        "key\n",
    ] {
        assert!(
            storage.create_bundle(name, b"identity").is_err(),
            "{name:?}"
        );
        assert!(storage.read_bundle(name).is_err(), "{name:?}");
    }
    assert!(storage.create_bundle("empty.json", b"").is_err());
    assert!(storage
        .create_bundle("large.json", &vec![b'x'; 128 * 1024 + 1])
        .is_err());
    assert!(storage.read_bundle("large.json").unwrap().is_none());
}

// T06/T37: only one local enrollment/renewal writer can hold the identity state.
#[test]
fn directory_lock_excludes_another_writer_and_unsafe_directories() {
    let dir = directory();
    let storage = IdentityDirectory::open(dir.path()).unwrap();
    assert!(IdentityDirectory::open(dir.path()).is_err());
    drop(storage);
    assert!(IdentityDirectory::open(dir.path()).is_ok());
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o750)).unwrap();
    assert!(IdentityDirectory::open(dir.path()).is_err());
    assert_eq!(
        fs::metadata(dir.path()).unwrap().permissions().mode() & 0o7777,
        0o750
    );
}

// T37: retaining the directory cannot authorize a replacement at the same path.
#[test]
fn directory_replacement_invalidates_the_retained_handle() {
    let dir = directory();
    let storage = IdentityDirectory::open(dir.path()).unwrap();
    let moved = dir.path().with_extension("moved");
    fs::rename(dir.path(), &moved).unwrap();
    fs::create_dir(dir.path()).unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    assert!(storage.create_bundle("host.json", b"identity").is_err());
    assert!(storage.validate().is_err());
    assert!(storage.read_bundle("host.json").is_err());
    drop(storage);
    fs::remove_dir_all(moved).unwrap();
}

// T06/T37: a refused directory is explained by path and failing check: in use,
// a wrong mode, or a parent other users can write (such as one under /tmp).
#[test]
fn a_refused_identity_directory_is_explained() {
    use capyctl_agent::identity_storage::describe_refusal;
    let dir = directory();
    let storage = IdentityDirectory::open(dir.path()).unwrap();
    let busy = IdentityDirectory::open(dir.path()).err().unwrap();
    assert!(describe_refusal(dir.path(), &busy).contains("in use"));
    drop(storage);

    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o750)).unwrap();
    let invalid = IdentityDirectory::open(dir.path()).err().unwrap();
    assert!(
        describe_refusal(dir.path(), &invalid).contains("mode 0750; it must be 0700"),
        "{}",
        describe_refusal(dir.path(), &invalid)
    );
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();

    let open_parent = dir.path().join("shared");
    fs::create_dir(&open_parent).unwrap();
    fs::set_permissions(&open_parent, fs::Permissions::from_mode(0o777)).unwrap();
    let identity = open_parent.join("identity");
    fs::create_dir(&identity).unwrap();
    fs::set_permissions(&identity, fs::Permissions::from_mode(0o700)).unwrap();
    let invalid = IdentityDirectory::open(&identity).err().unwrap();
    let said = describe_refusal(&identity, &invalid);
    assert!(
        said.starts_with(&format!("{} (a parent of", open_parent.display()))
            && said.contains("can be written by other users"),
        "{said}"
    );
}

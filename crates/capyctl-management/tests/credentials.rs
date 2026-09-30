use capyctl_management::ManagementCredentials;
use std::{
    fs,
    os::unix::fs::{symlink, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

const MANAGEMENT: &str = "management-token-01234567890123456789";
const INFERENCE: &str = "inference-token-012345678901234567890";
struct Fixture {
    _dir: tempfile::TempDir,
    management: PathBuf,
    inference: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("capyctl-credential-test-")
            .tempdir_in(std::env::var_os("HOME").unwrap())
            .unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let management = dir.path().join("management");
        let inference = dir.path().join("inference");
        for (path, value) in [(&management, MANAGEMENT), (&inference, INFERENCE)] {
            fs::write(path, value).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        Self {
            _dir: dir,
            management,
            inference,
        }
    }
    fn rejects(&self, path: &Path) {
        assert_eq!(
            ManagementCredentials::from_protected_files(path, &self.inference).err(),
            Some("invalid management credentials")
        );
    }
}
#[test]
fn valid_files_and_one_optional_lf_are_read_without_mutation() {
    let f = Fixture::new();
    for token in [
        MANAGEMENT.to_owned(),
        format!("{MANAGEMENT}\n"),
        format!("{}\n", "a".repeat(256)),
    ] {
        fs::write(&f.management, &token).unwrap();
        let before = fs::metadata(&f.management).unwrap();
        assert!(ManagementCredentials::from_protected_files(&f.management, &f.inference).is_ok());
        let after = fs::metadata(&f.management).unwrap();
        assert_eq!(
            (
                before.ino(),
                before.mode(),
                before.mtime(),
                before.mtime_nsec()
            ),
            (after.ino(), after.mode(), after.mtime(), after.mtime_nsec())
        );
        assert_eq!(fs::read(&f.management).unwrap(), token.as_bytes());
    }
}
#[test]
fn rejects_malformed_equal_oversize_and_missing_without_repair() {
    let f = Fixture::new();
    for value in [
        "".to_owned(),
        "x".repeat(31),
        "x".repeat(258),
        format!("{MANAGEMENT}\r\n"),
        format!("{MANAGEMENT}\n\n"),
        format!(" {MANAGEMENT}"),
        INFERENCE.to_owned(),
        "é".repeat(32),
    ] {
        fs::write(&f.management, &value).unwrap();
        f.rejects(&f.management);
        assert_eq!(fs::read(&f.management).unwrap(), value.as_bytes());
    }
    let missing = f._dir.path().join("missing");
    f.rejects(&missing);
    assert!(!missing.exists());
}
#[test]
fn rejects_path_aliases_links_and_special_files() {
    let f = Fixture::new();
    f.rejects(Path::new("relative"));
    f.rejects(&f._dir.path().join("./management"));
    f.rejects(&f._dir.path().join("../management"));
    f.rejects(Path::new(&format!(
        "{}//management",
        f._dir.path().display()
    )));
    let directory_link = f._dir.path().join("directory-link");
    symlink(f._dir.path(), &directory_link).unwrap();
    f.rejects(&directory_link.join("management"));
    let link = f._dir.path().join("link");
    symlink(&f.management, &link).unwrap();
    f.rejects(&link);
    let hard = f._dir.path().join("hard");
    fs::hard_link(&f.management, &hard).unwrap();
    f.rejects(&hard);
    f.rejects(&f.management);
    let fifo = f._dir.path().join("fifo");
    let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    f.rejects(&fifo);
    f.rejects(f._dir.path());
}

#[test]
fn inference_file_gets_identical_protection_and_no_fallback() {
    let f = Fixture::new();
    fs::set_permissions(&f.inference, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        ManagementCredentials::from_protected_files(&f.management, &f.inference).err(),
        Some("invalid management credentials")
    );
    assert_eq!(fs::metadata(&f.inference).unwrap().mode() & 0o777, 0o644);
    assert_eq!(fs::read(&f.inference).unwrap(), INFERENCE.as_bytes());
}
#[test]
fn rejects_unsafe_permissions_and_ancestors_without_chmod() {
    let f = Fixture::new();
    for mode in [0o400, 0o640, 0o644, 0o666, 0o700, 0o4600] {
        fs::set_permissions(&f.management, fs::Permissions::from_mode(mode)).unwrap();
        f.rejects(&f.management);
        assert_eq!(fs::metadata(&f.management).unwrap().mode() & 0o7777, mode);
    }
    fs::set_permissions(&f.management, fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(f._dir.path(), fs::Permissions::from_mode(0o770)).unwrap();
    f.rejects(&f.management);
    assert_eq!(fs::metadata(f._dir.path()).unwrap().mode() & 0o777, 0o770);
}

//! ADR 0018 §2: `engines.yaml`, the mllm-owned file engine registration
//! writes beside the role's document. The role's own document is never
//! rewritten. CPU tests only; they are not qualification.
use mllm_config::registration::*;
use mllm_config::remote_roles::HostConfig;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn host_doc(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("host.yaml");
    std::fs::write(
        &path,
        format!(
            "# operator notes\n{}",
            HostConfig::template(&dir.join("state"))
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

fn profile() -> serde_json::Value {
    serde_json::json!({"engine":"vllm","revision":1,"executable":"/opt/v/bin/vllm",
        "build_fingerprint":"0.29.0","args":[],"env":{},
        "log_policy":{"max_file_bytes":"16MiB","retained_files":3},
        "security":{"deep_park":"enabled","trust_remote_code":false,
            "credential_ref":"secret://engine-key","admin_credential_ref":"secret://admin-key"}})
}

fn host_value(path: &Path) -> serde_json::Value {
    mllm_config::parse_strict(
        mllm_config::ConfigKind::Host,
        &std::fs::read_to_string(path).unwrap(),
    )
    .unwrap()
}

// ADR 0018 §2 (owner decision 2026-09-25): the engines file sits beside the
// role's configuration file; without one, under the user's config home.
#[test]
fn the_engines_file_sits_beside_the_role_document() {
    assert_eq!(
        engines_path(
            Some(Path::new("/etc/mllm/x.yaml")),
            Path::new("/home/u/.config")
        ),
        Path::new("/etc/mllm/engines.yaml")
    );
    assert_eq!(
        engines_path(None, Path::new("/home/u/.config")),
        Path::new("/home/u/.config/mllm/engines.yaml")
    );
    assert_eq!(
        engines_beside(Path::new("/r/host.yaml")),
        Path::new("/r/engines.yaml")
    );
    let env = |k: &str| (k == "HOME").then(|| "/home/u".to_string());
    assert_eq!(config_home(&env).unwrap(), Path::new("/home/u/.config"));
    let xdg = |k: &str| (k == "XDG_CONFIG_HOME").then(|| "/x".to_string());
    assert_eq!(config_home(&xdg).unwrap(), Path::new("/x"));
}

// T04 T03: the first write creates the file at revision 1 with mode 0600;
// the host document is never touched; the merged host document parses.
#[test]
fn a_write_creates_the_engines_file_and_never_touches_the_host_document() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_doc(dir.path());
    let before = std::fs::read(&host).unwrap();
    let path = engines_beside(&host);
    let mut engines = EnginesFile::load(&path).unwrap();
    assert_eq!((engines.revision, engines.profiles.len()), (0, 0));
    engines.profiles.insert("vllm".into(), profile());
    let lock = lock_engines(&path).unwrap();
    assert_eq!(
        write_engines(&engines, &lock, Some(&host_value(&host))).unwrap(),
        1
    );
    drop(lock);
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(
        written.starts_with("# mllm-document-revision: 1\n"),
        "{written}"
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::read(&host).unwrap(),
        before,
        "host.yaml is never rewritten"
    );
    let config = HostConfig::load(&host).unwrap();
    assert!(config.profiles.contains_key("vllm"));
    assert_eq!(config.document["runtime_profiles"]["vllm"], profile());
    let lock = lock_engines(&path).unwrap();
    assert_eq!(
        write_engines(&EnginesFile::load(&path).unwrap(), &lock, None).unwrap(),
        2
    );
}

// T03 (owner decision 2026-09-25): the same profile name in the host document
// and the engines file is refused, at load and at write.
#[test]
fn a_name_in_both_files_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_doc(dir.path());
    let mut document = host_value(&host);
    document["runtime_profiles"]["vllm"] = profile();
    std::fs::write(&host, document.to_string()).unwrap();
    let path = engines_beside(&host);
    let mut engines = EnginesFile::load(&path).unwrap();
    engines.profiles.insert("vllm".into(), profile());
    let lock = lock_engines(&path).unwrap();
    let refused = write_engines(&engines, &lock, Some(&document)).unwrap_err();
    assert!(
        refused.detail.contains("vllm") && refused.detail.contains("both"),
        "{refused:?}"
    );
    assert!(!path.exists(), "nothing was written");
    std::fs::write(
        &path,
        format!(
            "kind: engines\nschema_version: 1\nruntime_profiles:\n  vllm: {}\n",
            profile()
        ),
    )
    .unwrap();
    let error = HostConfig::load(&host).unwrap_err();
    assert_eq!(error.path, "runtime_profiles.vllm");
}

// T04: the lock serializes writers, so no update is lost.
#[test]
fn concurrent_writers_never_lose_an_update() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_doc(dir.path());
    let path = engines_beside(&host);
    let workers: Vec<_> = ["a", "b", "c", "d"]
        .into_iter()
        .map(|name| {
            let (path, host) = (path.clone(), host_value(&host));
            std::thread::spawn(move || {
                let lock = lock_engines(&path).unwrap();
                let mut engines = EnginesFile::load(&path).unwrap();
                engines.profiles.insert(name.into(), profile());
                write_engines(&engines, &lock, Some(&host)).unwrap()
            })
        })
        .collect();
    let mut revisions: Vec<u64> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    revisions.sort();
    assert_eq!(revisions, vec![1, 2, 3, 4]);
    assert_eq!(EnginesFile::load(&path).unwrap().profiles.len(), 4);
}

// T03 (Review Focus 4): an engines file whose merged host document would
// exceed the publication bound is refused before anything is written.
#[test]
fn a_document_over_the_publication_bound_is_not_written() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_doc(dir.path());
    let path = engines_beside(&host);
    let mut engines = EnginesFile::load(&path).unwrap();
    for i in 0..80 {
        let mut p = profile();
        p["args"] = serde_json::json!(["--served-model-name", "x".repeat(300)]);
        engines.profiles.insert(format!("p{i}"), p);
    }
    let lock = lock_engines(&path).unwrap();
    let error = write_engines(&engines, &lock, Some(&host_value(&host))).unwrap_err();
    assert!(error.detail.contains("32768"), "{error:?}");
    assert!(!path.exists());
}

// T03: an engines file is strict: unknown fields and another kind are refused.
#[test]
fn the_engines_file_is_strict() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("engines.yaml");
    for text in [
        "kind: host\nschema_version: 1\nname: x\n",
        "kind: engines\nschema_version: 1\nruntime_profiles: {}\nextra: 1\n",
    ] {
        assert!(EnginesFile::parse(&path, text).is_err(), "{text}");
    }
    let ok = EnginesFile::parse(
        &path,
        "# mllm-document-revision: 7\nkind: engines\nschema_version: 1\nruntime_profiles: {}\n",
    )
    .unwrap();
    assert_eq!(ok.revision, 7);
}

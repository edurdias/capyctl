//! ADR 0018 §2: `engines.yaml`, the capyctl-owned file engine registration
//! writes beside the role's document. The role's own document is never
//! rewritten. CPU tests only; they are not qualification.
use capyctl_config::registration::*;
use capyctl_config::remote_roles::HostConfig;
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
    capyctl_config::parse_strict(
        capyctl_config::ConfigKind::Host,
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
            Some(Path::new("/etc/capyctl/x.yaml")),
            Path::new("/home/u/.config")
        ),
        Path::new("/etc/capyctl/engines.yaml")
    );
    assert_eq!(
        engines_path(None, Path::new("/home/u/.config")),
        Path::new("/home/u/.config/capyctl/engines.yaml")
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
        written.starts_with("# capyctl-document-revision: 1\n"),
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
        "# capyctl-document-revision: 7\nkind: engines\nschema_version: 1\nruntime_profiles: {}\n",
    )
    .unwrap();
    assert_eq!(ok.revision, 7);
}

use capyctl_config::effective::InstallationDrift;
use capyctl_config::engine_policy::Engine;

fn spec(engine: Engine) -> ProfileSpec {
    ProfileSpec {
        engine,
        executable: match engine {
            Engine::Vllm => "/home/u/venv/bin/vllm".into(),
            Engine::Sglang => "/home/u/venv/bin/python3".into(),
            Engine::Tensorfold => "/home/u/venv/bin/tensorfold".into(),
        },
        build_fingerprint: "0.29.0".into(),
        deep_park: true,
        installation_drift: InstallationDrift::Warn,
        args: vec![],
        cuda_home: None,
    }
}

// T14 T21: a built profile carries both per-launch key references, deep
// park as asked, drift only when refused, and passes the resolution rules.
#[test]
fn a_built_profile_passes_the_resolution_rules() {
    for engine in [Engine::Vllm, Engine::Sglang] {
        let profile = profile_document(&spec(engine));
        assert_eq!(profile["revision"], 1);
        assert_eq!(profile["security"]["credential_ref"], "secret://engine-key");
        assert_eq!(
            profile["security"]["admin_credential_ref"],
            "secret://admin-key"
        );
        assert_eq!(profile["security"]["deep_park"], "enabled");
        assert!(profile["security"].get("installation_drift").is_none());
        check_profile("vllm", &profile).unwrap();
    }
    let mut refusing = spec(Engine::Vllm);
    refusing.installation_drift = InstallationDrift::Refuse;
    refusing.deep_park = false;
    let profile = profile_document(&refusing);
    assert_eq!(profile["security"]["installation_drift"], "refuse");
    assert_eq!(profile["security"]["deep_park"], "disabled");
}

// T14: reserved arguments stay reserved; SGLang takes no host-fixed args.
#[test]
fn profile_arguments_follow_the_existing_rules() {
    let mut vllm = spec(Engine::Vllm);
    vllm.args = vec!["--port".into(), "1".into()];
    assert!(check_profile("vllm", &profile_document(&vllm)).is_err());
    vllm.args = vec!["--max-num-seqs".into(), "8".into()];
    check_profile("vllm", &profile_document(&vllm)).unwrap();
    let mut sglang = spec(Engine::Sglang);
    sglang.args = vec!["--mem-fraction-static".into(), "0.5".into()];
    assert!(check_profile("sglang", &profile_document(&sglang)).is_err());
}

// T01 T03: names are short lowercase identifiers.
#[test]
fn profile_names_are_bounded_identifiers() {
    for good in ["vllm", "sglang", "vllm-patched", "v2_exl3"] {
        assert!(valid_profile_name(good), "{good}");
    }
    for bad in ["", "Vllm", "-x", "a.b", "a/b", &"x".repeat(65)] {
        assert!(!valid_profile_name(bad), "{bad}");
    }
    assert!(check_profile("Bad Name", &profile_document(&spec(Engine::Vllm))).is_err());
}

// T22: the verified set; anything else is `custom`.
#[test]
fn the_verified_set_marks_custom_builds() {
    assert!(is_verified(Engine::Vllm, "0.29.0"));
    assert!(is_verified(Engine::Vllm, "0.30.0"));
    assert!(!is_verified(Engine::Vllm, "0.30.1"));
    assert!(is_verified(Engine::Sglang, "0.5.20"));
    assert!(!is_verified(Engine::Sglang, "0.5.20+custom"));
    assert!(!is_verified(Engine::Vllm, "0.5.20"));
}

// ADR 0018 §3: only a change confined to runtime profiles is live.
#[test]
fn profile_only_changes_are_recognised() {
    let dir = tempfile::tempdir().unwrap();
    let old: serde_json::Value =
        serde_json::from_str(&HostConfig::template(&dir.path().join("state"))).unwrap();
    let mut new = old.clone();
    new["runtime_profiles"]["vllm"] = profile_document(&spec(Engine::Vllm));
    assert!(only_profiles_differ(&old, &new));
    assert_eq!(added_profiles(&old, &new), vec!["vllm".to_string()]);
    assert_eq!(removed_profiles(&new, &old), vec!["vllm".to_string()]);
    let mut edited = new.clone();
    edited["load_report_interval"] = "9s".into();
    assert!(!only_profiles_differ(&old, &edited));
}

// T04 T37 (ADR 0018 §2; review decision C1): the CLI is the only writer of
// engines.yaml and may run as root (`sudo capyctl engine …`) under the system
// units. A rewrite keeps the file's owner and mode, so the service user can
// still read it; a new file is created for the owner the CLI names (the
// role's service user), mode 0600, and so is its lock.
#[test]
fn a_rewrite_keeps_the_owner_and_mode_and_a_new_file_takes_the_named_owner() {
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let host = host_doc(dir.path());
    let path = engines_beside(&host);
    let me = std::fs::metadata(dir.path()).unwrap();
    let owner = Some((me.uid(), me.gid()));
    let lock = lock_engines_for(&path, owner).unwrap();
    let mut engines = EnginesFile::load(&path).unwrap();
    engines.profiles.insert("vllm".into(), profile());
    write_engines(&engines, &lock, None).unwrap();
    drop(lock);
    for file in [path.clone(), dir.path().join("engines.yaml.lock")] {
        let meta = std::fs::metadata(&file).unwrap();
        assert_eq!((meta.uid(), meta.gid()), (me.uid(), me.gid()), "{file:?}");
        assert_eq!(meta.permissions().mode() & 0o777, 0o600, "{file:?}");
    }
    // The operator made it group-readable for the service's group: kept.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    let lock = lock_engines(&path).unwrap();
    let mut engines = EnginesFile::load(&path).unwrap();
    engines.profiles.remove("vllm");
    write_engines(&engines, &lock, None).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o640
    );
}

// ADR 0018 §2 hardening (2026-09-25): a hard link planted at `engines.yaml.lock`
// before the CLI runs as root must not let its `fchown` reach the file the
// link really points at (they share one inode). `lock_engines_for` refuses
// it instead of chowning the victim.
#[test]
fn lock_refuses_a_hard_linked_lock_file() {
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let victim = dir.path().join("victim.txt");
    std::fs::write(&victim, b"do not touch").unwrap();
    std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).unwrap();
    let before = std::fs::metadata(&victim).unwrap();
    let path = dir.path().join("engines.yaml");
    let lock_path = dir.path().join("engines.yaml.lock");
    std::fs::hard_link(&victim, &lock_path).unwrap();
    let me = std::fs::metadata(dir.path()).unwrap();
    let err = match lock_engines_for(&path, Some((me.uid(), me.gid()))) {
        Ok(_) => panic!("expected the hard-linked lock file to be refused"),
        Err(e) => e,
    };
    assert!(err.detail.contains("one link"), "{}", err.detail);
    let after = std::fs::metadata(&victim).unwrap();
    assert_eq!((before.uid(), before.gid()), (after.uid(), after.gid()));
    assert_eq!(
        before.permissions().mode() & 0o777,
        after.permissions().mode() & 0o777
    );
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "do not touch");
}

// As above, for `engines.yaml` itself: a hard link there must not have its
// owner/mode copied onto the freshly written file, nor its inode touched.
#[test]
fn write_refuses_a_hard_linked_engines_file() {
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let victim = dir.path().join("victim.txt");
    let seed = EnginesFile {
        path: victim.clone(),
        revision: 0,
        profiles: Default::default(),
    };
    std::fs::write(&victim, seed.render(0)).unwrap();
    std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).unwrap();
    let before = std::fs::metadata(&victim).unwrap();
    let before_text = std::fs::read_to_string(&victim).unwrap();
    let path = dir.path().join("engines.yaml");
    std::fs::hard_link(&victim, &path).unwrap();
    let lock = lock_engines(&path).unwrap();
    let mut engines = EnginesFile::load(&path).unwrap();
    engines.profiles.insert("vllm".into(), profile());
    let err = write_engines(&engines, &lock, None).unwrap_err();
    assert!(err.detail.contains("one link"), "{}", err.detail);
    let after = std::fs::metadata(&victim).unwrap();
    assert_eq!((before.uid(), before.gid()), (after.uid(), after.gid()));
    assert_eq!(
        before.permissions().mode() & 0o777,
        after.permissions().mode() & 0o777
    );
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), before_text);
}

/// SPEC §13.3 amendment (owner decision 2026-09-25): `engine add` records the
/// CUDA toolkit: `CUDA_HOME` when it holds `bin/nvcc`, else `/usr/local/cuda`
/// when it does, else nothing; the profile it writes passes the resolution
/// rules and names `cuda_home` only when detected.
// T03 T21
#[test]
fn engine_add_detects_and_writes_the_cuda_home() {
    use capyctl_config::registration::{check_profile, detect_cuda_home, profile_document};
    use std::path::{Path, PathBuf};
    let nvcc_in = |homes: &'static [&'static str]| {
        move |nvcc: &Path| {
            homes
                .iter()
                .any(|home| nvcc == Path::new(home).join("bin/nvcc"))
        }
    };
    assert_eq!(
        detect_cuda_home(
            Some("/opt/cuda-13.0/"),
            nvcc_in(&["/opt/cuda-13.0", "/usr/local/cuda"])
        ),
        Some(PathBuf::from("/opt/cuda-13.0"))
    );
    // A CUDA_HOME without nvcc, or not absolute, falls back to /usr/local/cuda.
    for env in [Some("/opt/empty"), Some("cuda"), Some("/opt/../cuda"), None] {
        assert_eq!(
            detect_cuda_home(env, nvcc_in(&["/usr/local/cuda", "/opt/../cuda"])),
            Some(PathBuf::from("/usr/local/cuda")),
            "{env:?}"
        );
    }
    assert_eq!(detect_cuda_home(None, nvcc_in(&[])), None);

    let mut with = spec(Engine::Vllm);
    with.cuda_home = Some(PathBuf::from("/usr/local/cuda"));
    let document = profile_document(&with);
    assert_eq!(document["cuda_home"], "/usr/local/cuda");
    check_profile("vllm", &document).expect("a detected cuda_home is valid");
    assert!(profile_document(&spec(Engine::Vllm))
        .get("cuda_home")
        .is_none());
}

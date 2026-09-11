//! Integration tests for the no-config startup matrix (SPEC §15.2)
//! and atomic owner-protected generation: T02 and the concurrent-init
//! credential-once behavior (T04).

mod common;
use common::*;

use std::fs;
use std::os::unix::fs::PermissionsExt;

#[test]
fn t02_missing_implicit_generates_once_with_protected_files() {
    let d = temp_state_dir();
    let out = resolve_startup(ConfigKind::Standalone, None, &d).unwrap();
    let LoadOutcome::Generated {
        config_path,
        created_identity,
    } = out
    else {
        panic!()
    };
    assert!(created_identity);
    assert!(config_path.exists());
    let mode = config_path.metadata().unwrap().permissions().mode();
    assert_eq!(mode & 0o077, 0, "no group/other bits on generated config");
    // Second call loads, not regenerates: mtime unchanged
    let m1 = fs::metadata(&config_path).unwrap().modified().unwrap();
    let out2 = resolve_startup(ConfigKind::Standalone, None, &d).unwrap();
    assert!(matches!(out2, LoadOutcome::Loaded(_)));
    let m2 = fs::metadata(&config_path).unwrap().modified().unwrap();
    assert_eq!(m1, m2);
    assert!(
        !engine_executed_marker(&d),
        "no engine execution at startup"
    );
}

#[test]
fn concurrent_starts_do_not_clobber() {
    let d = temp_state_dir();
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| resolve_startup(ConfigKind::Standalone, None, &d).unwrap());
        }
    });
    assert_eq!(
        credential_fingerprint(&d).count(),
        1,
        "credentials created once"
    );
}

#[test]
fn implicit_existing_invalid_is_an_error_not_a_reset() {
    let d = temp_state_dir();
    let config_path = d.join("config").join("standalone.yaml");
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    let broken = "schema_version: 1\nkind: standalone\nname: a\nfrobnicate: true\n";
    fs::write(&config_path, broken).unwrap();
    let err = resolve_startup(ConfigKind::Standalone, None, &d).unwrap_err();
    assert!(err.detail.contains("unknown field"), "{err}");
    // No reset: the invalid file is untouched.
    assert_eq!(fs::read_to_string(&config_path).unwrap(), broken);
}

#[test]
fn explicit_missing_path_fails() {
    let d = temp_state_dir();
    let missing = d.join("nope.yaml");
    let err = resolve_startup(ConfigKind::Standalone, Some(&missing), &d).unwrap_err();
    assert!(err.detail.contains("does not exist"), "{err}");
    // No silent generation through the explicit path.
    assert!(credential_fingerprint(&d).count() == 0);
}

#[test]
fn explicit_invalid_path_fails() {
    let d = temp_state_dir();
    let bad = d.join("bad.yaml");
    fs::write(&bad, "schema_version: 1\nkind: server\nname: a\n").unwrap();
    let err = resolve_startup(ConfigKind::Standalone, Some(&bad), &d).unwrap_err();
    assert!(err.detail.contains("does not match expected"), "{err}");
}

#[test]
fn explicit_valid_path_loads_without_generating() {
    let d = temp_state_dir();
    let cfg = d.join("my.yaml");
    fs::write(
        &cfg,
        "schema_version: 1\nkind: standalone\nname: lab\nserver: {}\nhost: {}\n",
    )
    .unwrap();
    let out = resolve_startup(ConfigKind::Standalone, Some(&cfg), &d).unwrap();
    assert!(matches!(out, LoadOutcome::Loaded(_)));
    assert!(
        credential_fingerprint(&d).count() == 0,
        "no credentials for explicit load"
    );
}

#[test]
fn generated_config_passes_task3_validate() {
    let d = temp_state_dir();
    let (path, bytes) =
        mllm_config::defaults::generate_default(ConfigKind::Standalone, &d).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    mllm_config::validate(&text, ConfigKind::Standalone)
        .unwrap_or_else(|e| panic!("generated config must validate: {e}"));
    assert_eq!(fs::read_to_string(&path).unwrap(), text);
    let creds = fs::read_to_string(d.join("identity").join("credentials")).unwrap();
    assert!(creds.contains("admin_token: "));
    assert!(creds.contains("api_key: "));
    let mode = fs::metadata(d.join("identity").join("credentials"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o077, 0, "credentials are owner-only");
}

#[test]
fn non_standalone_generation_is_rejected() {
    let d = temp_state_dir();
    let err = mllm_config::defaults::generate_default(ConfigKind::Server, &d).unwrap_err();
    assert!(matches!(
        err.code,
        mllm_config::ConfigErrorCode::UnsupportedCombination
    ));
}

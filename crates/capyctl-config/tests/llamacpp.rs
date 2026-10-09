//! ADR 0029: the llama.cpp engine kind and its registration rules. CPU tests
//! only; none of this qualifies llama.cpp (only the live rows LC1–LC6 do).

use capyctl_config::effective::resolve_effective;
use capyctl_config::engine_policy::{validate_profile_args, Engine};
use capyctl_config::llamacpp::{system_config_file, system_config_refusal, LlamacppBuild};
use capyctl_config::registration::{
    check_profile, is_verified, is_verified_in, listed_version, profile_document, ProfileSpec,
    ENVIRONMENT_PROFILES,
};
use capyctl_config::ConfigErrorCode;
use serde_json::{json, Value};

fn spec(deep_park: bool) -> ProfileSpec {
    ProfileSpec {
        engine: Engine::Llamacpp,
        executable: "/opt/llama.cpp/bin/llama-server".into(),
        build_fingerprint: "0.6.0+d812350".into(),
        deep_park,
        installation_drift: capyctl_config::effective::InstallationDrift::Warn,
        args: vec![],
        cuda_home: None,
        approved_options: vec![],
        approved_paths: vec![],
        env: Default::default(),
        approved_env: vec![],
    }
}

// T42 T01 (ADR 0029 §1): the serde name, the closed name list and the
// environment profile name.
#[test]
fn llamacpp_is_a_named_engine() {
    assert_eq!(Engine::from_name("llamacpp"), Some(Engine::Llamacpp));
    assert_eq!(Engine::Llamacpp.name(), "llamacpp");
    assert_eq!(serde_json::to_value(Engine::Llamacpp).unwrap(), "llamacpp");
    assert_eq!(
        serde_json::from_value::<Engine>(json!("llamacpp")).unwrap(),
        Engine::Llamacpp
    );
    for spelling in ["llama.cpp", "llama-cpp", "LlamaCpp", "llama-server"] {
        assert_eq!(Engine::from_name(spelling), None, "{spelling}");
    }
    assert_eq!(Engine::ALL.last(), Some(&Engine::Llamacpp));
    assert!(ENVIRONMENT_PROFILES.contains(&"local-llamacpp"));
}

// T42 (ADR 0029 §2): the version line of `--version`'s standard error; the
// build number is not kept, and the fingerprint is `<version>+<commit>`.
#[test]
fn the_version_line_parses_and_the_build_number_is_dropped() {
    let text = "version: 0.6.0 (build 1, commit d812350)\nbuilt with GNU 15.2.0 for Linux x86_64\n";
    let build = LlamacppBuild::parse_version_output(text).unwrap();
    assert_eq!(build.version, "0.6.0");
    assert_eq!(build.commit, "d812350");
    assert_eq!(build.fingerprint(), "0.6.0+d812350");
    let other_clone = LlamacppBuild::parse_version_output(
        "ggml_cuda_init: found 1 CUDA devices\nversion: 0.6.0 (build 9137, commit d812350)\n",
    )
    .unwrap();
    assert_eq!(other_clone.fingerprint(), build.fingerprint());
    for broken in [
        "",
        "0.6.0\n",
        "llama-server 0.6.0\n",
        "version: 0.6.0\n",
        "version: 0.6.0 (build 1)\n",
        "version: 0.6.0 (build x, commit d812350)\n",
        "version: 0.6.0 (build 1, commit d812350\n",
        "version: 0.6 0 (build 1, commit d812350)\n",
        "version: 0.6.0 (build 1, commit d81+350)\n",
    ] {
        assert_eq!(
            LlamacppBuild::parse_version_output(broken),
            None,
            "{broken:?}"
        );
    }
    assert_eq!(
        LlamacppBuild::from_fingerprint("0.6.0+d812350"),
        Some(build.clone())
    );
}

// T42 (ADR 0029 §2, ruling 2): under a verified table naming 0.6.0, a
// `0.6.0-dev` build of the tag's commit lists as 0.6.0; another commit, or
// 0.5.0, is custom. The real table gains 0.6.0 only with the live rows.
#[test]
fn a_dev_build_of_the_tag_commit_lists_as_the_release() {
    let table = [(Engine::Llamacpp, "0.6.0")];
    let verified = |version: &str| is_verified_in(&table, Engine::Llamacpp, version);
    assert!(verified("0.6.0+d812350"));
    assert!(verified("0.6.0-dev+d812350"));
    assert!(verified("0.6.0-dev+D812350"));
    assert!(
        verified("0.6.0-dev+d8123501234abcd"),
        "a longer abbreviation"
    );
    assert!(verified("0.6.0"), "a version alone, as a role may state it");
    assert!(!verified("0.6.0-dev+abc1234"));
    assert!(
        !verified("0.6.0-dev+d81235"),
        "shorter than any abbreviation"
    );
    assert!(!verified("0.6.0-dev"));
    assert!(!verified("0.5.0+d812350"));
    assert!(!verified("0.5.0-dev+d812350"));
    assert!(!verified("unknown"));
    assert!(!is_verified_in(&table, Engine::Vllm, "0.6.0-dev+d812350"));
    assert!(!is_verified(Engine::Llamacpp, "0.6.0+d812350"));
    assert!(!is_verified(Engine::Llamacpp, "0.6.0-dev+d812350"));
}

// T42 (ADR 0029 §2): a llama.cpp listing shows the fingerprint `engine add`
// read, not the library version the host reports; the other engines keep
// the reported version.
#[test]
fn a_llamacpp_listing_shows_its_fingerprint() {
    assert_eq!(
        listed_version(
            Some(Engine::Llamacpp),
            Some("0.6.0"),
            Some("0.6.0-dev+abc1234")
        ),
        "0.6.0-dev+abc1234"
    );
    assert_eq!(
        listed_version(Some(Engine::Vllm), Some("0.30.0"), Some("0.29.0")),
        "0.30.0"
    );
    assert_eq!(
        listed_version(Some(Engine::Vllm), Some(""), Some("0.29.0")),
        "0.29.0"
    );
    assert_eq!(listed_version(None, None, None), "unknown");
}

// T42 T21 (ADR 0029 §2): the profile `engine add` writes has deep parking
// disabled and passes; one that parks is refused `capability_missing`.
#[test]
fn a_llamacpp_profile_never_parks() {
    let profile = profile_document(&spec(false));
    assert_eq!(profile["engine"], "llamacpp");
    assert_eq!(profile["build_fingerprint"], "0.6.0+d812350");
    assert_eq!(profile["security"]["deep_park"], "disabled");
    check_profile("llamacpp", &profile).unwrap();
    let error = check_profile("llamacpp", &profile_document(&spec(true))).unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::UnsupportedCombination);
    assert!(error.detail.contains("capability_missing"), "{error}");
}

// T42 T14 (ADR 0029 §6): until llama.cpp's exact-name option tables are in
// the release, every host-fixed llama.cpp argument is refused (fail closed),
// and the other engines' prefix tables are not applied to it.
#[test]
fn llamacpp_options_fail_closed() {
    assert!(validate_profile_args(Engine::Llamacpp, &[], false).is_ok());
    for args in [
        vec!["--cache-reuse", "256"],
        vec!["--api-key", "x"],
        vec!["--ctx-size=4096"],
    ] {
        let args: Vec<String> = args.into_iter().map(str::to_owned).collect();
        assert!(
            validate_profile_args(Engine::Llamacpp, &args, false).is_err(),
            "{args:?}"
        );
    }
    let mut spec = spec(false);
    spec.args = vec!["--threads".into(), "8".into()];
    let error = check_profile("llamacpp", &profile_document(&spec)).unwrap_err();
    assert_eq!(error.path, "runtime_profiles.args");
}

// T42 T37 (ADR 0029 §2, §6): a system config.ini under the root is named in
// the refusal; without one there is none.
#[test]
fn the_system_config_file_is_named() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(system_config_refusal(dir.path()), None);
    let file = system_config_file(dir.path());
    assert_eq!(file, dir.path().join("etc/llama.cpp/config.ini"));
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/nonexistent", &file).unwrap();
    let refusal = system_config_refusal(dir.path()).expect("even a dangling link counts");
    assert!(refusal.contains(&file.display().to_string()), "{refusal}");
}

// T42 (ADR 0029 §1): a llama.cpp profile registers, but a deployment on it is
// refused at resolution until its launch settings are in the release.
#[test]
fn a_llamacpp_deployment_is_refused_at_resolution() {
    let all: Value = serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    let (mut deployment, mut host) = (all["deployment"].clone(), all["host"].clone());
    host["runtime_profiles"]["local"] = profile_document(&spec(false));
    deployment["residency"] = "restart_only".into();
    deployment["engine_config"] = json!({"context_length": 8192});
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::UnsupportedCombination);
    assert_eq!(error.path, "runtime_profile");
    assert!(error.detail.contains("llama.cpp"), "{error}");
}

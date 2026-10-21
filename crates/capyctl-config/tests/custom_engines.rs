//! ADR 0031: a custom engine's profile, how its deployments resolve and what
//! refuses them. CPU tests only; none of this starts a program.

use capyctl_config::custom::{CustomManifest, CustomProfile};
use capyctl_config::effective::{
    decode_effective_snapshot, derived_initialize_ms, resolve_effective_with_checkpoint,
    CheckpointFacts, DeploymentApi, EffectiveDeployment, InstallationDrift, Residency,
    TimeoutSource,
};
use capyctl_config::engine_env::EnvSource;
use capyctl_config::registration::{
    check_profile, custom_engines_beside, custom_profile_document, lock_engines, write_engines,
    CustomProfileSpec, EnginesFile, CUSTOM_ENGINES_FILE, ENGINES_FILE,
};
use capyctl_config::{ConfigError, ConfigErrorCode};
use capyctl_domain::launch::{LaunchSettings, SettingSource};
use serde_json::{json, Value};

const GIB: i64 = 1 << 30;
const GB: i64 = 1_000_000_000;

/// A manifest serving `api`, with one `env` entry of its own.
fn manifest(api: &str) -> CustomManifest {
    CustomManifest::from_value(json!({
        "kind": "custom_engine",
        "schema_version": 1,
        "api": api,
        "version": "1.2.0",
        "args": ["serve", "--host", "{host}", "--port", "{port}", "--model", "{model_dir}"],
        "env": {"BASAL_THREADS": "4", "BASAL_NAME": "{served_name}"},
        "health": "/health",
    }))
    .unwrap()
}

/// The profile `engine add --custom` writes for `manifest`, with the
/// profile's own `env` and approvals.
fn profile(manifest: CustomManifest, env: &[(&str, &str)], approved: &[&str]) -> Value {
    custom_profile_document(&CustomProfileSpec {
        executable: "/opt/basal/bin/basal-serve".into(),
        custom: CustomProfile::new(manifest),
        installation_drift: InstallationDrift::Warn,
        env: env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        approved_env: approved.iter().map(|s| (*s).to_owned()).collect(),
    })
}

/// The F2 fixture on a custom engine serving `api`: the deployment states
/// its `resources` and no residency, and names the profile's revision.
fn fixture(api: &str) -> (Value, Value) {
    let all: Value = serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    let (mut deployment, mut host) = (all["deployment"].clone(), all["host"].clone());
    host["runtime_profiles"]["local"] = profile(manifest(api), &[], &[]);
    deployment["runtime_profile_revision"] = 1.into();
    deployment["request_deadline"] = "600s".into();
    deployment["api"] = api.into();
    deployment["engine_config"] = json!({});
    deployment.as_object_mut().unwrap().remove("residency");
    (deployment, host)
}

fn facts() -> CheckpointFacts {
    CheckpointFacts {
        weights_bytes: Some(3 * GB),
        ..CheckpointFacts::default()
    }
}

fn resolve(deployment: &Value, host: &Value) -> Result<EffectiveDeployment, ConfigError> {
    resolve_effective_with_checkpoint(deployment, host, facts())
}

/// Refused at `path`, its detail starting with the closed `code`.
fn refused(error: &ConfigError, path: &str, code: &str) {
    assert_eq!(error.path, path, "{error}");
    assert!(error.detail.starts_with(code), "expected {code}: {error}");
}

// T44 T43 T14 (ADR 0031 §4, §5): a decisions deployment with declared
// resources resolves to custom launch settings: restart_only by default, the
// declared Ready total as its memory request, nothing derived beside the
// context, and the Initialize timeout from ADR 0014 A1 alone.
#[test]
fn a_custom_deployment_resolves_from_its_declared_resources() {
    let (deployment, host) = fixture("decisions");
    let effective = resolve(&deployment, &host).unwrap();
    assert_eq!(effective.api, DeploymentApi::Decisions);
    assert_eq!(effective.residency, Residency::RestartOnly);
    let LaunchSettings::Custom(settings) = &effective.engine_config else {
        panic!("not custom: {:?}", effective.engine_config);
    };
    assert_eq!(settings.memory.request_bytes, 8 * GIB);
    assert_eq!(
        settings.provenance.get("residency"),
        Some(&SettingSource::CapyctlDefault)
    );
    assert!(effective.engine_config.extra_args().is_empty());
    assert_eq!(
        effective.profile.custom,
        Some(CustomProfile::new(manifest("decisions")))
    );
    // ADR 0031 §5: no warm-up allowance, unlike the named engines.
    assert_eq!(
        effective.timeouts.initialize_ms,
        derived_initialize_ms(Some(3 * GB)).min(600_000)
    );
    assert_eq!(
        effective.timeouts.provenance["initialize"],
        TimeoutSource::Derived
    );

    // A declared Initialize timeout is kept as declared.
    let mut declared = deployment.clone();
    declared["timeouts"] = json!({"initialize": "7m"});
    let effective = resolve(&declared, &host).unwrap();
    assert_eq!(effective.timeouts.initialize_ms, 420_000);
    assert_eq!(
        effective.timeouts.provenance["initialize"],
        TimeoutSource::Declared
    );

    // A stated restart_only is the operator's.
    let mut stated = deployment.clone();
    stated["residency"] = "restart_only".into();
    assert_eq!(
        resolve(&stated, &host).unwrap().residency,
        Residency::RestartOnly
    );
}

// T44 (ADR 0031 §4): CapyCTL derives nothing for a program it does not know;
// the deployment states its resources.
#[test]
fn a_custom_deployment_without_resources_is_refused() {
    let (mut deployment, host) = fixture("decisions");
    deployment.as_object_mut().unwrap().remove("resources");
    let error = resolve(&deployment, &host).unwrap_err();
    assert_eq!(
        (error.code, error.path.as_str()),
        (ConfigErrorCode::MissingRequired, "resources"),
        "{error}"
    );
}

// T44 T43 (ADR 0031 §4, ADR 0030 §7): a custom engine never parks; a chat
// deployment asking for a parking residency is `capability_missing:deep_park`,
// and a decisions deployment is refused for it by ADR 0030 first.
#[test]
fn a_parking_residency_is_refused() {
    for tier in ["deep", "host_backed"] {
        let (mut deployment, host) = fixture("chat");
        deployment["residency"] = tier.into();
        refused(
            &resolve(&deployment, &host).unwrap_err(),
            "residency",
            "capability_missing:deep_park",
        );
        let (mut deployment, host) = fixture("decisions");
        deployment["residency"] = tier.into();
        let error = resolve(&deployment, &host).unwrap_err();
        assert_eq!(
            (error.code, error.path.as_str()),
            (ConfigErrorCode::UnsupportedCombination, "residency"),
            "{tier}: {error}"
        );
    }
}

// T44 T43 (ADR 0031 §4, ADR 0030 §7): a custom engine runs one process per
// instance on one host: a chat deployment with a topology is
// `group_shape_unsupported:custom`; a decisions deployment is refused for its
// topology by ADR 0030's document check first.
#[test]
fn a_topology_is_refused() {
    let (mut deployment, host) = fixture("chat");
    deployment["topology"] = json!({"tensor_parallel": 2});
    deployment["placement"] = json!({"hosts": ["host-a", "host-b"]});
    refused(
        &resolve(&deployment, &host).unwrap_err(),
        "topology",
        "group_shape_unsupported:custom",
    );
    let (mut deployment, host) = fixture("decisions");
    deployment["topology"] = json!({"tensor_parallel": 2});
    deployment["placement"] = json!({"hosts": ["host-a", "host-b"]});
    let error = resolve(&deployment, &host).unwrap_err();
    assert_eq!(
        (error.code, error.path.as_str()),
        (ConfigErrorCode::UnsupportedCombination, "topology"),
        "{error}"
    );
}

// T44 T43 (ADR 0031 §2): every deployment on a custom engine serves the API
// its manifest names.
#[test]
fn the_api_must_match_the_manifest() {
    let (mut deployment, host) = fixture("decisions");
    deployment.as_object_mut().unwrap().remove("api");
    refused(
        &resolve(&deployment, &host).unwrap_err(),
        "api",
        "capability_missing:chat",
    );
    let (mut deployment, host) = fixture("chat");
    deployment["api"] = "decisions".into();
    refused(
        &resolve(&deployment, &host).unwrap_err(),
        "api",
        "capability_missing:decisions",
    );
    let (deployment, host) = fixture("chat");
    assert_eq!(
        resolve(&deployment, &host).unwrap().api,
        DeploymentApi::Chat
    );
}

// T44 T14 (ADR 0031 §4, §10): CapyCTL renders none of a custom engine's
// options, so every engine_config field but `context_length` and `env` is
// refused by name, an explicit `false` or `[]` too.
#[test]
fn every_option_field_is_refused_by_name() {
    for (config, field) in [
        (json!({"extra_args": []}), "extra_args"),
        (json!({"extra_args": ["--threads", "4"]}), "extra_args"),
        (json!({"accept_extra_args": false}), "accept_extra_args"),
        (
            json!({"vllm": {"tool_call_parser": "hermes"}}),
            "tool_call_parser",
        ),
        (
            json!({"vllm": {"reasoning_parser": "qwen3"}}),
            "reasoning_parser",
        ),
        (
            json!({"sglang": {"tool_call_parser": "qwen"}}),
            "tool_call_parser",
        ),
        (
            json!({"sglang": {"reasoning_parser": "qwen3"}}),
            "reasoning_parser",
        ),
        (json!({"memory": {"kv_cache": "4GiB"}}), "memory"),
        (json!({"dtype": "bfloat16"}), "dtype"),
        (json!({"quantization": "fp8"}), "quantization"),
        (json!({"kv_cache_dtype": "fp8"}), "kv_cache_dtype"),
        (
            json!({"max_concurrent_requests": 4}),
            "max_concurrent_requests",
        ),
        (json!({"cuda_graphs": false}), "cuda_graphs"),
        (json!({"language_model_only": false}), "language_model_only"),
        (json!({"trust_remote_code": false}), "trust_remote_code"),
        (json!({"vllm": {}}), "vllm"),
        (json!({"sglang": {}}), "sglang"),
        (json!({"tensorfold": {}}), "tensorfold"),
        (json!({"llamacpp": {}}), "llamacpp"),
    ] {
        let (mut deployment, host) = fixture("chat");
        deployment["engine_config"] = config.clone();
        let error = resolve(&deployment, &host).expect_err(&format!("{config} must be refused"));
        refused(
            &error,
            &format!("engine_config.{field}"),
            &format!("custom_field_unsupported:{field}"),
        );
    }
    let (mut deployment, host) = fixture("chat");
    deployment["engine_config"] = json!({"context_length": 8192});
    let effective = resolve(&deployment, &host).unwrap();
    let LaunchSettings::Custom(settings) = &effective.engine_config else {
        panic!("not custom");
    };
    assert_eq!(settings.common.context_length, Some(8192));
}

// T44 (ADR 0031 §2): a manifest rendering `{context_length}`, in `args` or in
// an `env` value, needs the deployment to state it.
#[test]
fn an_unset_context_length_placeholder_is_refused() {
    let mut in_args = manifest("chat");
    in_args
        .args
        .extend(["--ctx".to_owned(), "{context_length}".to_owned()]);
    let mut in_env = manifest("chat");
    in_env
        .env
        .insert("BASAL_CTX".into(), "ctx={context_length}".into());
    for manifest in [in_args, in_env] {
        manifest.validate().unwrap();
        let (mut deployment, mut host) = fixture("chat");
        host["runtime_profiles"]["local"] = profile(manifest, &[], &[]);
        refused(
            &resolve(&deployment, &host).unwrap_err(),
            "engine_config.context_length",
            "custom_placeholder_unset:context_length",
        );
        deployment["engine_config"] = json!({"context_length": 4096});
        resolve(&deployment, &host).unwrap();
    }
}

// T44 T37 (ADR 0031 §2, ADR 0028 §2.1): the manifest's env is the lowest
// layer, the profile's overrides it and the deployment's overrides both; a
// deployment name still needs the profile's approval.
#[test]
fn the_engine_env_layers_manifest_profile_and_deployment() {
    let (deployment, host) = fixture("chat");
    let effective = resolve(&deployment, &host).unwrap();
    let vars = &effective.engine_env.vars;
    // Manifest values stay templates; the launch renders them.
    assert_eq!(
        vars["BASAL_NAME"],
        ("{served_name}".to_owned(), EnvSource::Manifest)
    );
    assert_eq!(vars["BASAL_THREADS"], ("4".to_owned(), EnvSource::Manifest));
    let shown = serde_json::to_value(&effective).unwrap();
    assert_eq!(shown["engine_env"]["vars"]["BASAL_THREADS"][1], "manifest");

    let (mut deployment, mut host) = fixture("chat");
    deployment["engine_config"] = json!({"env": {"BASAL_DEBUG": "1"}});
    refused(
        &resolve(&deployment, &host).unwrap_err(),
        "engine_config.env",
        "engine_env_not_approved:BASAL_DEBUG",
    );

    host["runtime_profiles"]["local"] = profile(
        manifest("chat"),
        &[("BASAL_THREADS", "8"), ("BASAL_NAME", "fixed")],
        &["BASAL_*"],
    );
    let effective = resolve(&deployment, &host).unwrap();
    let vars = &effective.engine_env.vars;
    assert_eq!(vars["BASAL_THREADS"], ("8".to_owned(), EnvSource::Profile));
    assert_eq!(vars["BASAL_DEBUG"], ("1".to_owned(), EnvSource::Deployment));
    deployment["engine_config"] = json!({"env": {"BASAL_THREADS": "16"}});
    let effective = resolve(&deployment, &host).unwrap();
    let vars = &effective.engine_env.vars;
    assert_eq!(
        vars["BASAL_THREADS"],
        ("16".to_owned(), EnvSource::Deployment)
    );
    assert_eq!(vars["BASAL_NAME"], ("fixed".to_owned(), EnvSource::Profile));
}

// T44 T37 (ADR 0031 §2): a name CapyCTL owns is refused in the manifest's
// env (when it validates and again at every resolution) and in the
// profile's.
#[test]
fn a_reserved_env_name_is_refused() {
    let error = CustomManifest::from_value(json!({
        "kind": "custom_engine", "schema_version": 1, "api": "chat",
        "args": ["{host}", "{port}", "{model_dir}"],
        "env": {"LD_PRELOAD": "/tmp/x.so"},
    }))
    .unwrap_err();
    assert_eq!(error.code, "engine_env_reserved:LD_PRELOAD");

    // A profile written before the name was reserved: its manifest digests
    // correctly, and resolution still refuses it.
    let mut stale = manifest("chat");
    stale.env.insert("CUDA_VISIBLE_DEVICES".into(), "0".into());
    let (deployment, mut host) = fixture("chat");
    host["runtime_profiles"]["local"] = profile(stale, &[], &[]);
    let error = resolve(&deployment, &host).unwrap_err();
    assert!(
        error
            .detail
            .contains("engine_env_reserved:CUDA_VISIBLE_DEVICES"),
        "{error}"
    );

    let (deployment, mut host) = fixture("chat");
    host["runtime_profiles"]["local"] = profile(manifest("chat"), &[("LD_PRELOAD", "/x")], &[]);
    let error = resolve(&deployment, &host).unwrap_err();
    assert!(
        error.detail.contains("engine_env_reserved:LD_PRELOAD"),
        "{error}"
    );
}

// T44 T39 (ADR 0031 §3): the manifest's digest is part of the recipe: a
// changed manifest is another revision. (The non-custom golden fingerprints
// are pinned by tests/effective_snapshot.rs and tests/decisions.rs.)
#[test]
fn the_manifest_digest_is_in_the_recipe_fingerprint() {
    let (deployment, host) = fixture("chat");
    let before = resolve(&deployment, &host).unwrap();
    let mut changed = manifest("chat");
    changed.args.push("--quiet".into());
    let (_, mut other) = fixture("chat");
    other["runtime_profiles"]["local"] = profile(changed, &[], &[]);
    let after = resolve(&deployment, &other).unwrap();
    assert_ne!(before.recipe_fingerprint, after.recipe_fingerprint);
    // The same manifest, the same revision.
    assert_eq!(
        resolve(&deployment, &host).unwrap().recipe_fingerprint,
        before.recipe_fingerprint
    );
}

// T44 T39 (ADR 0031 §3): a frozen custom revision decodes exactly, and a
// snapshot whose manifest or digest was altered is refused.
#[test]
fn a_custom_snapshot_round_trips_and_refuses_tampering() {
    let (mut deployment, mut host) = fixture("chat");
    host["runtime_profiles"]["local"] =
        profile(manifest("chat"), &[("BASAL_THREADS", "8")], &["BASAL_*"]);
    deployment["engine_config"] = json!({"context_length": 4096, "env": {"BASAL_DEBUG": "1"}});
    let effective = resolve(&deployment, &host).unwrap();
    let text = serde_json::to_string(&effective).unwrap();
    assert_eq!(decode_effective_snapshot(&text).unwrap(), effective);

    let frozen: Value = serde_json::from_str(&text).unwrap();
    let mut args = frozen.clone();
    args["profile"]["custom"]["manifest"]["args"]
        .as_array_mut()
        .unwrap()
        .push("--quiet".into());
    assert!(decode_effective_snapshot(&args.to_string()).is_err());
    let mut digest = frozen.clone();
    digest["profile"]["custom"]["manifest_digest"] = format!("sha256:{}", "0".repeat(64)).into();
    assert!(decode_effective_snapshot(&digest.to_string()).is_err());
    // Both changed together still claim another revision.
    let mut both = args.clone();
    let mut tampered = manifest("chat");
    tampered.args.push("--quiet".into());
    both["profile"]["custom"]["manifest_digest"] = tampered.digest().into();
    assert!(decode_effective_snapshot(&both.to_string()).is_err());
    let mut dropped = frozen.clone();
    dropped["profile"].as_object_mut().unwrap().remove("custom");
    assert!(decode_effective_snapshot(&dropped.to_string()).is_err());
}

// T44 (ADR 0031 §1, §3): a profile carries the manifest with a digest that
// matches it, a custom engine's profile always carries one, and no other
// engine's does.
#[test]
fn the_custom_block_belongs_to_custom_profiles_only() {
    let (deployment, mut host) = fixture("chat");
    host["runtime_profiles"]["local"]["custom"]["manifest_digest"] =
        format!("sha256:{}", "0".repeat(64)).into();
    let error = resolve(&deployment, &host).unwrap_err();
    refused(&error, "runtime_profiles.custom", "custom_manifest_invalid");
    assert!(check_profile("basal", &host["runtime_profiles"]["local"]).is_err());

    let (deployment, mut host) = fixture("chat");
    host["runtime_profiles"]["local"]["custom"]["manifest"]["args"]
        .as_array_mut()
        .unwrap()
        .push("--quiet".into());
    refused(
        &resolve(&deployment, &host).unwrap_err(),
        "runtime_profiles.custom",
        "custom_manifest_invalid",
    );

    let (deployment, mut host) = fixture("chat");
    host["runtime_profiles"]["local"]
        .as_object_mut()
        .unwrap()
        .remove("custom");
    let error = resolve(&deployment, &host).unwrap_err();
    assert_eq!(
        (error.code, error.path.as_str()),
        (ConfigErrorCode::MissingRequired, "runtime_profiles.custom"),
        "{error}"
    );
    assert!(check_profile("basal", &host["runtime_profiles"]["local"]).is_err());

    let all: Value = serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    let (deployment, mut host) = (all["deployment"].clone(), all["host"].clone());
    host["runtime_profiles"]["local"]["custom"] =
        serde_json::to_value(CustomProfile::new(manifest("chat"))).unwrap();
    let error = resolve(&deployment, &host).unwrap_err();
    assert_eq!(error.path, "runtime_profiles.custom", "{error}");
    assert!(check_profile("local", &host["runtime_profiles"]["local"]).is_err());

    let (_, host) = fixture("chat");
    check_profile("basal", &host["runtime_profiles"]["local"]).unwrap();
}

fn engines_text(kind: &str, profiles: Value) -> String {
    json!({"schema_version": 1, "kind": kind, "runtime_profiles": profiles}).to_string()
}

fn vllm_profile() -> Value {
    let all: Value = serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    all["host"]["runtime_profiles"]["local"].clone()
}

// T44 (ADR 0031 amendment A1): engines.yaml keeps the shape earlier releases
// read and holds no custom engine; custom-engines.yaml holds nothing else.
#[test]
fn each_engines_file_holds_its_own_kinds() {
    let dir = tempfile::tempdir().unwrap();
    let engines = dir.path().join(ENGINES_FILE);
    let custom = dir.path().join(CUSTOM_ENGINES_FILE);
    assert_eq!(custom_engines_beside(&engines), custom);
    let basal = profile(manifest("chat"), &[], &[]);

    let error = EnginesFile::parse(&engines, &engines_text("engines", json!({"basal": basal})))
        .unwrap_err();
    assert!(error.path.starts_with("runtime_profiles.basal"), "{error}");
    let mut bare = basal.clone();
    bare.as_object_mut().unwrap().remove("custom");
    let error =
        EnginesFile::parse(&engines, &engines_text("engines", json!({"basal": bare}))).unwrap_err();
    assert!(error.path.starts_with("runtime_profiles.basal"), "{error}");

    let error = EnginesFile::parse(
        &custom,
        &engines_text("custom_engines", json!({"local": vllm_profile()})),
    )
    .unwrap_err();
    assert_eq!(error.path, "runtime_profiles.local.engine", "{error}");

    let parsed = EnginesFile::parse(
        &custom,
        &engines_text("custom_engines", json!({"basal": basal})),
    )
    .unwrap();
    assert_eq!(parsed.profiles["basal"], basal);
    EnginesFile::parse(
        &engines,
        &engines_text("engines", json!({"local": vllm_profile()})),
    )
    .unwrap();
}

// T44 (ADR 0031 amendment A1): a role reads both files as one; a name in both
// is refused when read, and refused when either file is written.
#[test]
fn the_two_engines_files_merge_and_share_one_namespace() {
    let dir = tempfile::tempdir().unwrap();
    let engines = dir.path().join(ENGINES_FILE);
    let custom = dir.path().join(CUSTOM_ENGINES_FILE);
    let basal = profile(manifest("chat"), &[], &[]);
    std::fs::write(
        &engines,
        engines_text("engines", json!({"local": vllm_profile()})),
    )
    .unwrap();
    std::fs::write(
        &custom,
        engines_text("custom_engines", json!({"basal": basal})),
    )
    .unwrap();
    let merged = EnginesFile::load_with_custom(&engines).unwrap();
    assert_eq!(merged.path, engines);
    assert_eq!(
        merged.profiles.keys().collect::<Vec<_>>(),
        ["basal", "local"]
    );
    assert_eq!(merged.profiles["basal"], basal);

    // Writing a name the sibling file holds is refused, either way round.
    let mut file = EnginesFile::load(&custom).unwrap();
    file.profiles.insert("local".into(), basal.clone());
    let lock = lock_engines(&custom).unwrap();
    let error = write_engines(&file, &lock, None).unwrap_err();
    assert_eq!(error.path, "runtime_profiles.local", "{error}");
    drop(lock);
    let mut file = EnginesFile::load(&engines).unwrap();
    let mut other = vllm_profile();
    other["revision"] = 1.into();
    file.profiles.insert("basal".into(), other);
    let lock = lock_engines(&engines).unwrap();
    let error = write_engines(&file, &lock, None).unwrap_err();
    assert_eq!(error.path, "runtime_profiles.basal", "{error}");
    drop(lock);

    // A name in both files (written by hand) is refused when read.
    std::fs::write(
        &custom,
        engines_text("custom_engines", json!({"basal": basal, "local": basal})),
    )
    .unwrap();
    let error = EnginesFile::load_with_custom(&engines).unwrap_err();
    assert_eq!(error.path, "runtime_profiles.local", "{error}");

    // A file written under the lock is read back.
    std::fs::remove_file(&custom).unwrap();
    let mut file = EnginesFile::load(&custom).unwrap();
    file.profiles.insert("basal".into(), basal.clone());
    let lock = lock_engines(&custom).unwrap();
    assert_eq!(write_engines(&file, &lock, None).unwrap(), 1);
    drop(lock);
    let reread = EnginesFile::load_with_custom(&engines).unwrap();
    assert_eq!(reread.profiles["basal"], basal);
}

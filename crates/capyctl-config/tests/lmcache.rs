//! ADR 0033 (T45): LMCache's configuration surface. The deployment fields
//! `lmcache`, `lmcache_pool` and `retention`; the host settings `lmcache` and
//! `retention`; the LMCache connector and environment CapyCTL owns; the
//! profile's LMCache registration; and `lmcache: required` refused at
//! resolution. CPU tests only; they are not qualification.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use capyctl_config::effective::{
    decode_effective_snapshot, deployment_command_fingerprint, resolve_effective,
};
use capyctl_config::engine_env::{
    env_table, resolve_engine_env, resolve_engine_env_with_manifest, ApprovedEnv, EnvRefusal,
};
use capyctl_config::engine_policy::{
    flag_table, kv_transfer_config_set, validate_extra_args, validate_profile_args, Engine,
    ExtraArgsContext, FlagClass, ProfileArgError,
};
use capyctl_config::lmcache::{HostLmcache, LmcacheMode, LmcacheRegistration, Retention, GIB};
use capyctl_config::remote_roles::{EngineLogs, HostConfig};
use capyctl_config::setting_overrides::SettingOverrides;
use capyctl_config::ConfigKind;
use serde_json::{json, Value};

const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

const MP_CONNECTOR: &str = r#"{"kv_connector":"LMCacheMPConnector","kv_connector_module_path":"lmcache.integration.vllm.lmcache_mp_connector","kv_role":"kv_both"}"#;
const V1_CONNECTOR: &str = r#"{"kv_connector":"LMCacheConnectorV1","kv_role":"kv_both"}"#;
const MODULE_ONLY: &str = r#"{"kv_connector":"SomethingElse","kv_connector_module_path":"lmcache.integration.vllm.lmcache_mp_connector","kv_role":"kv_both"}"#;
const MULTI_CONNECTOR: &str = r#"{"kv_connector":"MultiConnector","kv_role":"kv_both","kv_connector_extra_config":{"connectors":[{"kv_connector":"NixlConnector","kv_role":"kv_both"},{"kv_connector":"LMCacheConnectorV1","kv_role":"kv_both"}]}}"#;
const MULTI_MODULE: &str = r#"{"kv_connector":"MultiConnector","kv_role":"kv_both","kv_connector_extra_config":{"connectors":[{"kv_connector":"LMCacheMPConnector","kv_connector_module_path":"lmcache.integration.vllm.lmcache_mp_connector"}]}}"#;
const NIXL: &str = r#"{"kv_connector":"NixlConnector","kv_role":"kv_both"}"#;

/// The f2 fixture on vLLM 0.32.0 with LMCache 0.5.6 registered, restarting
/// instead of parking: LMCache can be on.
fn fixture() -> (Value, Value) {
    let all: Value = serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    let (mut deployment, mut host) = (all["deployment"].clone(), all["host"].clone());
    deployment["residency"] = json!("restart_only");
    let profile = &mut host["runtime_profiles"]["local"];
    profile["build_fingerprint"] = json!("0.32.0");
    profile["lmcache"] = json!({"version": "0.5.6", "digest": DIGEST});
    (deployment, host)
}

fn with(deployment: &Value, fields: Value) -> Value {
    let mut deployment = deployment.clone();
    for (key, value) in fields.as_object().unwrap() {
        deployment[key] = value.clone();
    }
    deployment
}

fn round_trips(effective: &capyctl_config::effective::EffectiveDeployment) -> Value {
    let snapshot = serde_json::to_value(effective).unwrap();
    assert_eq!(
        &decode_effective_snapshot(&snapshot.to_string()).unwrap(),
        effective
    );
    snapshot
}

// T45 T43 (ADR 0033, contract decision 1): the declared settings are frozen
// only away from their defaults, so a revision without them (or stating the
// defaults) encodes and fingerprints exactly as before; stated, they join the
// revision, its recipe fingerprint and the command fingerprint, and survive a
// snapshot round trip that refuses a forged value.
#[test]
fn the_deployment_fields_are_frozen_only_when_not_default() {
    let (deployment, host) = fixture();
    let plain = resolve_effective(&deployment, &host).unwrap();
    let encoded = round_trips(&plain);
    for field in ["lmcache", "lmcache_pool_bytes", "retention"] {
        assert!(encoded.get(field).is_none(), "{field}");
    }
    assert_eq!(plain.lmcache, LmcacheMode::Auto);
    assert_eq!(plain.lmcache_pool_bytes, None);
    assert_eq!(plain.retention, Retention::Standard);
    let defaults = with(
        &deployment,
        json!({"lmcache": "auto", "retention": "standard"}),
    );
    assert_eq!(resolve_effective(&defaults, &host).unwrap(), plain);
    let command = deployment_command_fingerprint(&deployment, 300_000).unwrap();
    assert_eq!(
        deployment_command_fingerprint(&defaults, 300_000).unwrap(),
        command
    );

    let mut fingerprints = BTreeSet::from([plain.recipe_fingerprint.clone()]);
    let mut commands = BTreeSet::from([command]);
    for (fields, lmcache, pool, retention) in [
        (
            json!({"lmcache": "required", "lmcache_pool": "8GiB"}),
            LmcacheMode::Required,
            Some(8 * GIB),
            Retention::Standard,
        ),
        (
            json!({"lmcache_pool": "4GiB"}),
            LmcacheMode::Auto,
            Some(4 * GIB),
            Retention::Standard,
        ),
        (
            json!({"lmcache": "off"}),
            LmcacheMode::Off,
            None,
            Retention::Standard,
        ),
        (
            json!({"retention": "zero"}),
            LmcacheMode::Auto,
            None,
            Retention::Zero,
        ),
    ] {
        let stated = with(&deployment, fields.clone());
        let effective = resolve_effective(&stated, &host).unwrap();
        assert_eq!(
            (
                effective.lmcache,
                effective.lmcache_pool_bytes,
                effective.retention
            ),
            (lmcache, pool, retention),
            "{fields}"
        );
        let snapshot = round_trips(&effective);
        assert!(
            fingerprints.insert(effective.recipe_fingerprint.clone()),
            "{fields}"
        );
        assert!(
            commands.insert(deployment_command_fingerprint(&stated, 300_000).unwrap()),
            "{fields}"
        );
        // A snapshot claiming another value is not the validated revision.
        let mut forged = snapshot.clone();
        forged["lmcache_pool_bytes"] = json!(3 * GIB + 1);
        assert!(decode_effective_snapshot(&forged.to_string()).is_err());
        let mut forged = snapshot.clone();
        forged["recipe_fingerprint"] = json!(plain.recipe_fingerprint);
        assert!(decode_effective_snapshot(&forged.to_string()).is_err());
    }
    // The schema names the fields at the deployment root.
    for fields in [
        json!({"lmcache": "required", "lmcache_pool": "8GiB", "retention": "standard"}),
        json!({"lmcache": "off"}),
    ] {
        capyctl_config::validate(
            &with(&deployment, fields).to_string(),
            ConfigKind::Deployment,
        )
        .unwrap();
    }
}

// T45 (ADR 0034 §4): `lmcache_pool: auto` is the default: it resolves to no
// stated pool, freezes and fingerprints as an omitted one, and may stand
// beside `lmcache: off`.
#[test]
fn an_auto_pool_is_the_default() {
    let (deployment, host) = fixture();
    let plain = resolve_effective(&deployment, &host).unwrap();
    let auto = with(&deployment, json!({"lmcache_pool": "auto"}));
    capyctl_config::validate(&auto.to_string(), ConfigKind::Deployment).unwrap();
    let effective = resolve_effective(&auto, &host).unwrap();
    assert_eq!(effective.lmcache_pool_bytes, None);
    assert_eq!(effective.recipe_fingerprint, plain.recipe_fingerprint);
    assert_eq!(
        deployment_command_fingerprint(&auto, 300_000).unwrap(),
        deployment_command_fingerprint(&deployment, 300_000).unwrap()
    );
    let off = with(&deployment, json!({"lmcache": "off", "lmcache_pool": "auto"}));
    assert_eq!(resolve_effective(&off, &host).unwrap().lmcache_pool_bytes, None);
}

// T45 (ADR 0034 §4): B and C come from the checkpoint the launch reads: the
// context fit's KV bytes per token (Qwen3-4B's 144 KiB at BF16) and the
// declared context, else the fitted one; an unreadable checkpoint is an
// unknown basis, which sizes the 4 GiB default.
#[test]
fn the_pool_basis_comes_from_the_checkpoint_and_the_context() {
    use capyctl_config::lmcache::{pool_basis_for_effective, PoolBasis};
    let (deployment, mut host) = fixture();
    // The fixture's installation fixes `--max-model-len`; the fit needs it free.
    host["runtime_profiles"]["local"]["args"] = json!([]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.json"),
        json!({"model_type": "qwen3", "num_hidden_layers": 36, "num_attention_heads": 32,
            "num_key_value_heads": 8, "head_dim": 128, "hidden_size": 2560,
            "max_position_embeddings": 40960, "torch_dtype": "bfloat16"})
        .to_string(),
    )
    .unwrap();
    let resolve = |engine_config: Value| {
        let mut effective =
            resolve_effective(&with(&deployment, json!({"engine_config": engine_config})), &host)
                .unwrap();
        effective.model.resolved_path = Some(dir.path().to_string_lossy().into_owned());
        effective
    };
    let declared = resolve(json!({"memory": {"kv_cache": "5GiB"}, "context_length": 32768}));
    assert_eq!(
        pool_basis_for_effective(&declared),
        Ok(PoolBasis {
            bytes_per_token: 147_456,
            context_tokens: 32_768
        })
    );
    // Fitted from the 4 GiB grant, as the launch fits it.
    let fitted = resolve(json!({"memory": {"kv_cache": "4GiB"}}));
    let basis = pool_basis_for_effective(&fitted).unwrap();
    assert_eq!(basis.bytes_per_token, 147_456);
    assert_eq!(
        u64::from(
            capyctl_config::context_fit::fit_for_effective(&fitted)
                .tokens
                .unwrap()
        ),
        basis.context_tokens
    );
    let mut missing = fitted.clone();
    missing.model.resolved_path = Some("/nonexistent/capyctl/checkpoint".into());
    assert!(pool_basis_for_effective(&missing).is_err());
}

// T45 (ADR 0033 §6, §9, ADR 0034 §4, §7): a pool that is neither `auto` nor
// a whole number of GiB of at least 1 GiB, a mode or retention outside the
// closed set, and a stated pool beside `lmcache: off` are refused
// (invalid_config), naming the field, at resolution and in the command
// fingerprint alike.
#[test]
fn invalid_pools_modes_and_a_pool_beside_off_are_refused() {
    let (deployment, host) = fixture();
    for pool in [
        "512MiB", "0GiB", "1.5GiB", "1536MiB", "8 GiB", "8GB", "8", "lots", "Auto",
    ] {
        let stated = with(&deployment, json!({ "lmcache_pool": pool }));
        let error = resolve_effective(&stated, &host).unwrap_err();
        assert_eq!(error.path, "lmcache_pool", "{pool}: {error}");
        assert!(error.detail.contains("`auto` or a whole number of GiB"), "{error}");
        assert!(deployment_command_fingerprint(&stated, 300_000).is_err());
    }
    for fields in [json!({"lmcache": "on"}), json!({"retention": "none"})] {
        let error = resolve_effective(&with(&deployment, fields.clone()), &host).unwrap_err();
        let field = fields.as_object().unwrap().keys().next().unwrap().clone();
        assert_eq!(error.path, format!("deployment.{field}"), "{error}");
    }
    let off = with(
        &deployment,
        json!({"lmcache": "off", "lmcache_pool": "8GiB"}),
    );
    let error = resolve_effective(&off, &host).unwrap_err();
    assert_eq!(error.path, "lmcache_pool");
    assert!(error.detail.contains("`lmcache: off`"), "{error}");
    assert!(deployment_command_fingerprint(&off, 300_000).is_err());
}

// T45 T37 (ADR 0033 §10, §14): `lmcache: required` beside the deployment's
// `retention: zero` is refused `retention_zero:lmcache`, which the CLI maps
// to exit 5.
#[test]
fn required_beside_zero_retention_is_refused() {
    let (deployment, host) = fixture();
    let stated = with(
        &deployment,
        json!({"lmcache": "required", "retention": "zero"}),
    );
    let error = resolve_effective(&stated, &host).unwrap_err();
    assert_eq!(error.path, "lmcache");
    assert!(
        error.detail.starts_with("retention_zero:lmcache: "),
        "{error}"
    );
    assert!(deployment_command_fingerprint(&stated, 300_000).is_err());
}

// T45 T37 (ADR 0033 §9, §14): `required` is refused at resolution with
// `lmcache_unavailable:<reason>` where the deployment and its profile alone
// keep LMCache off; on a supported, registered, restarting deployment it
// resolves. ADR 0034 §3a: a parking residency is never refused for being
// outside the qualified table; it runs uncertified.
#[test]
fn required_is_refused_at_resolution_for_what_the_profile_decides() {
    let (deployment, host) = fixture();
    let required = with(&deployment, json!({"lmcache": "required"}));
    resolve_effective(&required, &host).unwrap();
    let mut deep = required.clone();
    deep["residency"] = json!("deep");
    resolve_effective(&deep, &host).unwrap();
    type Edit<'a> = &'a dyn Fn(&mut Value, &mut Value);
    let edit = |change: Edit<'_>| {
        let (mut deployment, mut host) = (required.clone(), host.clone());
        change(&mut deployment, &mut host);
        resolve_effective(&deployment, &host).unwrap_err()
    };
    let cases: [(&str, Edit<'_>); 4] = [
        ("lmcache_unavailable:engine_version_unsupported", &|_, h| {
            h["runtime_profiles"]["local"]["build_fingerprint"] = json!("0.31.0")
        }),
        ("lmcache_unavailable:not_installed", &|_, h| {
            h["runtime_profiles"]["local"]
                .as_object_mut()
                .unwrap()
                .remove("lmcache");
        }),
        ("lmcache_unavailable:version_unsupported", &|_, h| {
            h["runtime_profiles"]["local"]["lmcache"]["version"] = json!("0.5.5")
        }),
        ("lmcache_unavailable:kv_transfer_config_set", &|d, h| {
            h["runtime_profiles"]["local"]["security"]["approved_options"] =
                json!(["--kv-transfer-config"]);
            d["engine_config"] = json!({"memory": {"kv_cache": "4GiB"},
                "accept_extra_args": true, "extra_args": ["--kv-transfer-config", NIXL]});
        }),
    ];
    for (code, change) in cases {
        let error = edit(change);
        assert_eq!(error.path, "lmcache", "{code}: {error}");
        assert!(error.detail.starts_with(&format!("{code}: ")), "{error}");
    }
    let error = edit(&|_, h| h["runtime_profiles"]["local"]["build_fingerprint"] = json!("0.31.0"));
    assert!(
        error.detail.contains("LMCache needs vLLM 0.32.0 or newer"),
        "{error}"
    );
    // `auto` runs without LMCache instead: every case above resolves.
    let (mut auto, mut old) = (deployment.clone(), host.clone());
    auto["residency"] = json!("deep");
    old["runtime_profiles"]["local"]["build_fingerprint"] = json!("0.31.0");
    resolve_effective(&auto, &old).unwrap();
}

fn context<'a>(
    approved: &'a BTreeSet<String>,
    none: &'a BTreeSet<String>,
    paths: &'a [PathBuf],
) -> ExtraArgsContext<'a> {
    ExtraArgsContext {
        engine: Engine::Vllm,
        sleep_mode: false,
        approved_options: approved,
        approved_paths: paths,
        checkpoint_root: None,
        host_fixed: none,
    }
}

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// Every spelling of an LMCache connector in vLLM's `--kv-transfer-config`.
fn lmcache_spellings() -> Vec<Vec<String>> {
    let mut lists = Vec::new();
    for value in [
        MP_CONNECTOR,
        V1_CONNECTOR,
        MODULE_ONLY,
        MULTI_CONNECTOR,
        MULTI_MODULE,
    ] {
        for name in [
            "--kv-transfer-config",
            "--kv_transfer_config",
            "--KV-Transfer-Config",
            "--kv-transfer",
            "--kv-t",
        ] {
            lists.push(args(&[name, value]));
            lists.push(vec![format!("{name}={value}")]);
        }
    }
    for list in [
        &["--kv-transfer-config.kv_connector", "LMCacheConnectorV1"][..],
        &["--kv-transfer-config.kv_connector=LMCacheMPConnector"],
        &["--kv_transfer_config.kv_connector", "LMCacheMPConnector"],
        &[
            "--kv-transfer-config.kv_connector_module_path",
            "lmcache.integration.vllm.lmcache_mp_connector",
        ],
        &[
            "--kv-transfer-config.kv_connector_extra_config.connectors",
            r#"[{"kv_connector":"LMCacheConnectorV1"}]"#,
        ],
        // Not JSON, and plainly naming LMCache.
        &["--kv-transfer-config", "LMCacheConnectorV1"],
    ] {
        lists.push(args(list));
    }
    lists
}

// T45 T21 T14 (ADR 0033 §11): a `--kv-transfer-config` naming an LMCache
// connector (either name, a module path under `lmcache.`, or either inside a
// MultiConnector) is refused in every spelling, approved or not, from a
// deployment's extra arguments and a profile's, naming `lmcache`.
#[test]
fn an_lmcache_connector_is_refused_in_every_spelling() {
    let (none, paths) = (BTreeSet::new(), Vec::new());
    let approved = BTreeSet::from(["--kv-transfer-config".to_owned()]);
    for list in lmcache_spellings() {
        for approvals in [&none, &approved] {
            let error = validate_extra_args(&list, &context(approvals, &none, &paths)).unwrap_err();
            assert!(
                matches!(error, ProfileArgError::Lmcache(_)),
                "{list:?}: {error}"
            );
            assert!(error.to_string().contains("lmcache"), "{error}");
        }
        assert!(
            matches!(
                validate_profile_args(Engine::Vllm, &list, false),
                Err(ProfileArgError::Lmcache(_))
            ),
            "{list:?}"
        );
    }
    // Through resolution: invalid_config at the extra arguments, naming it.
    let (deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["security"]["approved_options"] =
        json!(["--kv-transfer-config"]);
    let stated = with(
        &deployment,
        json!({"engine_config": {"memory": {"kv_cache": "4GiB"}, "accept_extra_args": true,
            "extra_args": ["--kv-transfer-config", MULTI_CONNECTOR]}}),
    );
    let error = resolve_effective(&stated, &host).unwrap_err();
    assert_eq!(error.path, "engine_config.extra_args", "{error}");
    assert!(error.detail.contains("lmcache"), "{error}");
    let mut profile_args = host.clone();
    profile_args["runtime_profiles"]["local"]["args"] =
        json!(["--kv-transfer-config", V1_CONNECTOR]);
    let error = resolve_effective(&deployment, &profile_args).unwrap_err();
    assert_eq!(error.path, "runtime_profiles.args", "{error}");
    assert!(error.detail.contains("lmcache"), "{error}");
}

// T45 T21 (ADR 0033 §11): an approved value that is not a JSON object cannot
// be shown not to name LMCache and is refused; unapproved, it is the
// sensitive option it always was; a profile cannot pass one either.
#[test]
fn an_unreadable_connector_value_is_refused() {
    let (none, paths) = (BTreeSet::new(), Vec::new());
    let approved = BTreeSet::from(["--kv-transfer-config".to_owned()]);
    for list in [
        args(&["--kv-transfer-config", "{not json"]),
        args(&["--kv-transfer-config", r#""NixlConnector""#]),
        args(&["--kv-transfer-config=[1]"]),
        args(&["--kv-transfer-config"]),
    ] {
        let error = validate_extra_args(&list, &context(&approved, &none, &paths)).unwrap_err();
        assert!(
            matches!(error, ProfileArgError::LmcacheUnreadable(_)),
            "{list:?}: {error}"
        );
        assert!(error.to_string().contains("lmcache"), "{error}");
        assert!(matches!(
            validate_extra_args(&list, &context(&none, &none, &paths)),
            Err(ProfileArgError::Sensitive(_))
        ));
        assert!(matches!(
            validate_profile_args(Engine::Vllm, &list, false),
            Err(ProfileArgError::LmcacheUnreadable(_))
        ));
    }
}

// T45 T21 (ADR 0033 §11): another connector stays an approval option, and
// its presence (any spelling, deployment or profile arguments) is what the
// plan reads as `kv_transfer_config_set`; an `auto` deployment with one
// resolves.
#[test]
fn another_connector_stays_an_approval_option_and_is_flagged() {
    let (none, paths) = (BTreeSet::new(), Vec::new());
    let approved = BTreeSet::from(["--kv-transfer-config".to_owned()]);
    for list in [
        args(&["--kv-transfer-config", NIXL]),
        vec![format!("--kv_transfer_config={NIXL}")],
        args(&["--kv-transfer-config.kv_connector", "NixlConnector"]),
    ] {
        validate_extra_args(&list, &context(&approved, &none, &paths)).unwrap();
        assert!(matches!(
            validate_extra_args(&list, &context(&none, &none, &paths)),
            Err(ProfileArgError::Sensitive(_))
        ));
        assert!(kv_transfer_config_set(Engine::Vllm, &list), "{list:?}");
        let mut with_others = args(&["--max-num-seqs", "8"]);
        with_others.extend(list.clone());
        assert!(kv_transfer_config_set(Engine::Vllm, &with_others));
    }
    assert!(!kv_transfer_config_set(
        Engine::Vllm,
        &args(&["--max-num-seqs", "8"])
    ));
    assert!(!kv_transfer_config_set(Engine::Vllm, &[]));
    assert!(!kv_transfer_config_set(
        Engine::Sglang,
        &args(&["--kv-transfer-config", NIXL])
    ));
    let (deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["security"]["approved_options"] =
        json!(["--kv-transfer-config"]);
    let stated = with(
        &deployment,
        json!({"engine_config": {"memory": {"kv_cache": "4GiB"}, "accept_extra_args": true,
            "extra_args": ["--kv-transfer-config", NIXL]}}),
    );
    resolve_effective(&stated, &host).unwrap();
}

// T45 T21 (ADR 0033 §11, ADR 0021): `capyctl engine flags` shows vLLM's
// connector option owned with an LMCache value and sensitive otherwise, and
// SGLang's `--enable-lmcache` and every `--lmcache-*` option owned, each with
// an LMCache reason.
#[test]
fn the_flag_tables_show_the_lmcache_options_owned() {
    let vllm = flag_table(Engine::Vllm);
    let rows: Vec<_> = vllm
        .iter()
        .filter(|row| row.name == "--kv-transfer-config")
        .collect();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].class, FlagClass::Owned);
    assert!(rows[0].reason.contains("LMCache"), "{rows:?}");
    let value = rows[0].value.expect("the owned row names its value");
    assert!(value.contains("LMCacheMPConnector"));
    assert_eq!(rows[1].class, FlagClass::Sensitive);
    assert_eq!(rows[1].value, None);
    assert_eq!(rows[1].approval, Some("listener_or_egress"));
    let json = serde_json::to_value(&vllm).unwrap();
    let owned = json
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "--kv-transfer-config" && row["class"] == "owned")
        .unwrap();
    assert_eq!(owned["value"], value);

    let sglang = flag_table(Engine::Sglang);
    for name in ["--enable-lmcache", "--lmcache-config-file", "--lmcache-"] {
        let row = sglang.iter().find(|row| row.name == name).unwrap();
        assert_eq!(row.class, FlagClass::Owned, "{name}");
        assert!(row.reason.contains("LMCache"), "{name}: {}", row.reason);
    }
}

fn map(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

// T45 T21 (ADR 0033 §11): every `LMCACHE_*` engine variable is reserved on
// every engine, in any case, from a deployment, a profile and a custom
// engine's manifest, whatever an approval says; a glob that reaches only
// LMCache's family is not an approval; the environment table shows it
// reserved with the ADR's reason.
#[test]
fn lmcache_environment_names_are_reserved() {
    let wide = ApprovedEnv::parse(&["L*".to_owned()]).unwrap();
    for name in [
        "LMCACHE_CHUNK_SIZE",
        "LMCACHE_CONFIG_FILE",
        "lmcache_local_cpu",
        "LmCache_Max_Local_Cpu_Size",
    ] {
        let code = format!("engine_env_reserved:{name}");
        let deployment = resolve_engine_env(&map(&[]), &wide, &map(&[(name, "1")]));
        assert_eq!(deployment.unwrap_err().code(), code);
        let profile = resolve_engine_env(&map(&[(name, "1")]), &wide, &map(&[]));
        assert_eq!(profile.unwrap_err().code(), code);
        let manifest =
            resolve_engine_env_with_manifest(&map(&[(name, "1")]), &map(&[]), &wide, &map(&[]));
        assert_eq!(manifest.unwrap_err().code(), code);
    }
    for glob in ["LMCACHE_*", "LMCACHE_CHUNK*"] {
        assert!(matches!(
            ApprovedEnv::parse(&[glob.to_owned()]),
            Err(EnvRefusal::Reserved(_))
        ));
    }
    // A neighbouring name stays approvable.
    assert!(resolve_engine_env(&map(&[]), &wide, &map(&[("LMCACHED", "1")])).is_ok());
    let row = env_table()
        .into_iter()
        .find(|row| row.name == "LMCACHE_")
        .unwrap();
    assert_eq!(row.class, FlagClass::Reserved);
    // The reason names what the operator sets instead; runtime messages cite
    // no requirement.
    assert!(
        row.reason.contains("the deployment's `lmcache` setting") && !row.reason.contains("ADR"),
        "{}",
        row.reason
    );
    // Through resolution, on every engine's profile env and a deployment's.
    let (deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["security"]["approved_env"] = json!(["L*"]);
    let stated = with(
        &deployment,
        json!({"engine_config": {"memory": {"kv_cache": "4GiB"},
            "env": {"LMCACHE_CHUNK_SIZE": "256"}}}),
    );
    let error = resolve_effective(&stated, &host).unwrap_err();
    assert_eq!(error.path, "engine_config.env");
    assert_eq!(error.detail, "engine_env_reserved:LMCACHE_CHUNK_SIZE");
    let mut profile_env = host.clone();
    profile_env["runtime_profiles"]["local"]["env"] = json!({"lmcache_use_experimental": "1"});
    let error = resolve_effective(&deployment, &profile_env).unwrap_err();
    assert_eq!(error.path, "runtime_profiles.env");
    assert_eq!(error.detail, "engine_env_reserved:lmcache_use_experimental");
}

// T45 T04 (ADR 0033 §1, contract decision 3): `engine add` writes the
// LMCache registration on the profile only when there is one; engines.yaml
// keeps it, the host document it merges into carries it, and a revision
// resolved against that profile freezes it in its profile and its recipe
// fingerprint. A malformed registration is refused.
#[test]
fn the_lmcache_registration_round_trips_through_engines_yaml() {
    use capyctl_config::registration::{
        check_profile, engines_beside, lock_engines, profile_document, write_engines, EnginesFile,
        ProfileSpec,
    };
    let spec = ProfileSpec {
        engine: Engine::Vllm,
        executable: "/home/u/venv/bin/vllm".into(),
        build_fingerprint: "0.32.0".into(),
        deep_park: true,
        installation_drift: capyctl_config::effective::InstallationDrift::Warn,
        args: vec![],
        cuda_home: None,
        approved_options: vec![],
        approved_paths: vec![],
        env: Default::default(),
        approved_env: vec![],
        trust_remote_code: false,
        lmcache: Some(LmcacheRegistration::new("0.5.6", DIGEST)),
    };
    let profile = profile_document(&spec);
    assert_eq!(
        profile["lmcache"],
        json!({"version": "0.5.6", "digest": DIGEST})
    );
    check_profile("vllm", &profile).unwrap();
    let without = profile_document(&ProfileSpec {
        lmcache: None,
        ..spec.clone()
    });
    assert!(without.get("lmcache").is_none());
    for bad in [
        json!({"version": "0.5.6", "digest": "sha256:abc"}),
        json!({"version": "", "digest": DIGEST}),
        json!({"version": "0.5.6"}),
        json!({"version": "0.5.6", "digest": DIGEST, "extra": 1}),
    ] {
        let mut broken = profile.clone();
        broken["lmcache"] = bad.clone();
        assert!(check_profile("vllm", &broken).is_err(), "{bad}");
    }

    let dir = tempfile::tempdir().unwrap();
    let host_path = dir.path().join("host.yaml");
    std::fs::write(&host_path, HostConfig::template(&dir.path().join("state"))).unwrap();
    let path = engines_beside(&host_path);
    let mut engines = EnginesFile::load(&path).unwrap();
    engines.profiles.insert("vllm".into(), profile.clone());
    let lock = lock_engines(&path).unwrap();
    write_engines(&engines, &lock, None).unwrap();
    drop(lock);
    assert_eq!(EnginesFile::load(&path).unwrap().profiles["vllm"], profile);
    let config = HostConfig::load(&host_path).unwrap();
    assert_eq!(config.document["runtime_profiles"]["vllm"], profile);

    let (deployment, host) = fixture();
    let registered = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(
        registered.profile.lmcache,
        Some(LmcacheRegistration::new("0.5.6", DIGEST))
    );
    let snapshot = round_trips(&registered);
    assert_eq!(snapshot["profile"]["lmcache"]["version"], "0.5.6");
    let mut unregistered = host.clone();
    unregistered["runtime_profiles"]["local"]
        .as_object_mut()
        .unwrap()
        .remove("lmcache");
    let plain = resolve_effective(&deployment, &unregistered).unwrap();
    assert!(serde_json::to_value(&plain).unwrap()["profile"]
        .get("lmcache")
        .is_none());
    assert_ne!(plain.recipe_fingerprint, registered.recipe_fingerprint);
}

fn host_document() -> Value {
    serde_json::from_str(&HostConfig::template(std::path::Path::new("/srv/capyctl"))).unwrap()
}

// T45 T03 (ADR 0033 §9, §10, §14): a host's `lmcache` and `retention` parse
// with their defaults; any other value, and `retention: zero` beside an
// explicitly stated `engine_logs: files`, refuse the start (invalid_config);
// zero retention keeps engine logs in memory.
#[test]
fn host_settings_parse_with_defaults_and_refusals() {
    let parse = |document: &Value| HostConfig::parse(&document.to_string());
    let plain = parse(&host_document()).unwrap();
    assert_eq!(
        (plain.lmcache, plain.retention, plain.engine_logs),
        (HostLmcache::Auto, Retention::Standard, EngineLogs::Files)
    );
    let mut document = host_document();
    document["lmcache"] = json!("off");
    document["retention"] = json!("zero");
    let config = parse(&document).unwrap();
    assert_eq!(
        (config.lmcache, config.retention, config.engine_logs),
        (HostLmcache::Off, Retention::Zero, EngineLogs::Memory)
    );
    document["engine_logs"] = json!("memory");
    assert_eq!(parse(&document).unwrap().engine_logs, EngineLogs::Memory);
    document["engine_logs"] = json!("files");
    let error = parse(&document).unwrap_err();
    assert_eq!(error.path, "retention", "{error}");
    assert!(error.detail.contains("engine_logs: files"), "{error}");
    for (field, value) in [
        ("lmcache", json!("required")),
        ("lmcache", json!("on")),
        ("lmcache", json!(false)),
        ("retention", json!("none")),
        ("retention", json!(0)),
    ] {
        let mut document = host_document();
        document[field] = value.clone();
        let error = parse(&document).unwrap_err();
        assert_eq!(error.path, field, "{value}: {error}");
    }
}

// T45 T03 (ADR 0033 §9, §13): the host's settings stay in the published
// document, reach the resolved host policy and every revision's frozen host,
// round-trip through a snapshot, and leave the recipe fingerprint alone.
#[test]
fn host_settings_are_published_and_frozen_with_the_host() {
    let mut document = host_document();
    document["lmcache"] = json!("off");
    document["retention"] = json!("zero");
    let local = capyctl_config::remote_resources::local_host_document(&document).unwrap();
    assert_eq!(local["lmcache"], "off");
    assert_eq!(local["retention"], "zero");

    let (deployment, host) = fixture();
    let plain = resolve_effective(&deployment, &host).unwrap();
    assert!(serde_json::to_value(&plain).unwrap()["host"]
        .get("lmcache")
        .is_none());
    let mut stated = host.clone();
    stated["lmcache"] = json!("off");
    stated["retention"] = json!("zero");
    let effective = resolve_effective(&deployment, &stated).unwrap();
    assert_eq!(effective.host.lmcache, HostLmcache::Off);
    assert_eq!(effective.host.retention, Retention::Zero);
    assert_eq!(effective.recipe_fingerprint, plain.recipe_fingerprint);
    let snapshot = round_trips(&effective);
    assert_eq!(snapshot["host"]["lmcache"], "off");
    assert_eq!(snapshot["host"]["retention"], "zero");
    let policy = capyctl_config::effective::normalize_host_policy(&stated).unwrap();
    assert_eq!(
        (policy.lmcache, policy.retention),
        (HostLmcache::Off, Retention::Zero)
    );
}

// T45 T03 (owner decision 2026-09-25, ADR 0033 §9, §10): `--set
// lmcache=off` and `CAPYCTL_SET__RETENTION=zero` on a host, and the
// `host.`-prefixed paths on standalone, set the settings as YAML would; the
// role's own refusals apply to an override; `config show` lists the defaults.
#[test]
fn host_settings_follow_the_overrides() {
    let applied = |kind: ConfigKind, document: Value, set: &[&str], env: &[(&str, &str)]| {
        let set: Vec<String> = set.iter().map(|item| (*item).to_owned()).collect();
        let env: Vec<(String, String)> = env
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        SettingOverrides::parse(kind, &set, &env)
            .unwrap()
            .apply_and_validate(document)
    };
    let document = applied(
        ConfigKind::Host,
        host_document(),
        &["lmcache=off"],
        &[("CAPYCTL_SET__RETENTION", "zero")],
    )
    .unwrap();
    let config = HostConfig::parse(&document.to_string()).unwrap();
    assert_eq!(
        (config.lmcache, config.retention, config.engine_logs),
        (HostLmcache::Off, Retention::Zero, EngineLogs::Memory)
    );
    let refused = applied(
        ConfigKind::Host,
        host_document(),
        &["retention=zero", "engine_logs=files"],
        &[],
    )
    .and_then(|document| HostConfig::parse(&document.to_string()));
    assert!(refused.is_err());
    let standalone = json!({"schema_version": 1, "kind": "standalone", "name": "local"});
    let document = applied(
        ConfigKind::Standalone,
        standalone,
        &["host.lmcache=off"],
        &[("CAPYCTL_SET__HOST__RETENTION", "zero")],
    )
    .unwrap();
    assert_eq!(
        capyctl_config::remote_roles::host_lmcache(&document["host"]).unwrap(),
        HostLmcache::Off
    );
    assert_eq!(
        capyctl_config::remote_roles::engine_logs(&document["host"]).unwrap(),
        EngineLogs::Memory
    );
    let root = std::path::Path::new("/srv/capyctl");
    let host_defaults = capyctl_config::setting_overrides::defaults(ConfigKind::Host, None, root);
    for (path, value) in [("lmcache", "auto"), ("retention", "standard")] {
        assert!(
            host_defaults.contains(&(path.to_owned(), json!(value))),
            "{path}"
        );
    }
    let standalone_defaults =
        capyctl_config::setting_overrides::defaults(ConfigKind::Standalone, None, root);
    for (path, value) in [("host.lmcache", "auto"), ("host.retention", "standard")] {
        assert!(
            standalone_defaults.contains(&(path.to_owned(), json!(value))),
            "{path}"
        );
    }
}

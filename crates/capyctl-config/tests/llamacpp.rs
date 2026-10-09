//! ADR 0029: the llama.cpp engine kind, its registration rules, its option
//! policy and its resolution. CPU tests only; none of this qualifies
//! llama.cpp (only the live rows LC1–LC6 do).

use capyctl_config::checkpoint_layout::{pick_gguf, projector_file};
use capyctl_config::context_fit::{
    fit_for_effective, fit_on_remote_host, max_running_for_effective, MaxRunningSource,
    StreamsSource,
};
use capyctl_config::effective::{
    decode_effective_snapshot, resolve_effective, resolve_effective_with_checkpoint,
    validate_declared_resources, CheckpointFacts,
};
use capyctl_config::engine_policy::{
    sensitivity, validate_extra_args, validate_profile_args, Engine, ExtraArgsContext,
    ProfileArgError, Sensitivity,
};
use capyctl_config::llamacpp::{
    system_config_file, system_config_refusal, LlamacppBuild, RESERVED_NEVER_RENDERED,
    RESERVED_RENDERED, SENSITIVE,
};
use capyctl_config::registration::{
    check_profile, is_verified, is_verified_in, listed_version, profile_document, ProfileSpec,
    ENVIRONMENT_PROFILES,
};
use capyctl_config::ConfigErrorCode;
use capyctl_domain::launch::{LaunchSettings, LlamacppGpuLayers, SettingSource};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

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

/// The long spellings of every llama-server 0.6.0 option, one option a line.
const OPTIONS: &str = include_str!("fixtures/llama-server-0.6.0-options.txt");

fn options() -> Vec<Vec<&'static str>> {
    OPTIONS
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| line.split_whitespace().collect())
        .collect()
}

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// `--a-b-c` as `--a_b_c`, which llama-server reads as the same option.
fn underscored(name: &str) -> String {
    format!("--{}", name.trim_start_matches("--").replace('-', "_"))
}

fn extra(
    list: &[&str],
    approved: &[&str],
    paths: &[&str],
    fixed: &[&str],
) -> Result<(), ProfileArgError> {
    let approved: BTreeSet<String> = approved.iter().map(|name| (*name).to_owned()).collect();
    let paths: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
    let fixed = capyctl_config::engine_policy::option_names(&args(fixed)).unwrap();
    validate_extra_args(
        &args(list),
        &ExtraArgsContext {
            engine: Engine::Llamacpp,
            sleep_mode: false,
            approved_options: &approved,
            approved_paths: &paths,
            checkpoint_root: None,
            host_fixed: &fixed,
        },
    )
}

fn reserved_refusal(result: Result<(), ProfileArgError>) -> bool {
    matches!(
        result,
        Err(ProfileArgError::Reserved(_) | ProfileArgError::ReservedField { .. })
    )
}

// T42 T14 (ADR 0029 §6): every reserved option is refused in the extra and
// host-fixed arguments, in each of its spellings and each `_` spelling.
#[test]
fn every_reserved_spelling_is_refused() {
    for row in RESERVED_RENDERED.iter().chain(RESERVED_NEVER_RENDERED) {
        for name in *row {
            for spelling in [(*name).to_owned(), underscored(name)] {
                let refused = |list: &[&str]| {
                    reserved_refusal(extra(list, &[], &[], &[]))
                        && reserved_refusal(validate_profile_args(
                            Engine::Llamacpp,
                            &args(list),
                            false,
                        ))
                };
                assert!(refused(&[&spelling]), "{spelling}");
                assert!(refused(&[&spelling, "1"]), "{spelling} 1");
            }
        }
    }
}

// T42 T14 (ADR 0029 §6, review focus 1): a hand-written command's context
// and slot count are refused, naming the fields that set them.
#[test]
fn a_hand_written_context_and_parallel_are_refused_with_the_fields_named() {
    for (list, field) in [
        (["--ctx-size", "32768"], "context_length"),
        (["--parallel", "4"], "max_concurrent_requests"),
    ] {
        let error = extra(&list, &[], &[], &[]).unwrap_err();
        assert_eq!(
            error,
            ProfileArgError::ReservedField {
                option: list[0].into(),
                field: field.into()
            }
        );
        assert!(
            error
                .to_string()
                .contains(&format!("engine_config.{field}")),
            "{error}"
        );
    }
    let (mut deployment, host) = fixture();
    deployment["engine_config"] = json!({"context_length": 8192, "accept_extra_args": true,
        "extra_args": ["--ctx-size", "32768", "--parallel", "4"]});
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(error.path, "engine_config.extra_args");
    assert!(error.detail.contains("context_length"), "{error}");
}

// T42 T14 (ADR 0029 §6, review focus 2): the `_` spelling of an alias of a
// reserved option, upper case and negative forms are reserved too.
#[test]
fn underscore_and_alias_spellings_are_reserved() {
    for list in [
        &["--n_gpu_layers", "20"][..],
        &["--n-gpu-layers", "20"],
        &["--gpu_layers", "all"],
        &["--webui"],
        &["--no_ui"],
        &["--no-slots"],
        &["--kv_unified"],
        &["--no-context-shift"],
        &["--CTX-SIZE", "4096"],
        &["--log_verbosity", "4"],
        &["--webui-mcp-proxy"],
        &["--embeddings"],
        &["--hf_repo_draft", "org/model"],
    ] {
        assert!(reserved_refusal(extra(list, &[], &[], &[])), "{list:?}");
    }
    let error = extra(&["--n_gpu_layers", "20"], &[], &[], &[]).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("engine_config.llamacpp.n_gpu_layers"),
        "{error}"
    );
}

// T42 T14 (ADR 0029 §6): llama-server refuses `--name=value`, so CapyCTL
// refuses it at deploy time, saying so, for reserved and ordinary options.
#[test]
fn the_equals_spelling_is_refused() {
    for list in [&["--ctx-size=4096"][..], &["--cache-reuse=256"]] {
        let error = extra(list, &[], &[], &[]).unwrap_err();
        assert!(
            matches!(error, ProfileArgError::EqualsSpelling(_)),
            "{error}"
        );
        assert!(error.to_string().contains("llama.cpp refuses"), "{error}");
        assert!(validate_profile_args(Engine::Llamacpp, &args(list), false).is_err());
    }
}

// T42 T14 (ADR 0029 §6, review focus 3): ordinary options pass, beside a
// reserved one they share a prefix with; the other engines' prefix rule is
// not applied.
#[test]
fn cache_reuse_is_ordinary() {
    for list in [
        &["--cache-reuse", "256"][..],
        &["--spec-type", "ngram-mod"],
        &["--spec-type", "draft-mtp"],
        &["--override-tensor", "blk\\.[0-9]+\\.ffn_.*=CPU"],
        &["--reasoning-format", "none"],
        &["--reasoning", "off"],
        &["--flash-attn", "on"],
        &["--batch-size", "2048"],
        &["--ubatch_size", "512"],
        &["--chat-template", "chatml"],
        &["--load-mode", "mmap"],
        &["--cpu-moe"],
        &["--n-cpu-moe", "4"],
        &["--no-host"],
        &["--threads", "8"],
        &["--jinja"],
        &["--cache-prompt"],
    ] {
        extra(list, &[], &[], &[]).unwrap_or_else(|error| panic!("{list:?}: {error}"));
        validate_profile_args(Engine::Llamacpp, &args(list), false)
            .unwrap_or_else(|error| panic!("{list:?}: {error}"));
    }
    // One option, however it is spelled, is passed once.
    for (list, fixed) in [
        (&["--temp", "1", "--temp", "2"][..], &[][..]),
        (&["--no-warmup"], &["--warmup"]),
        (
            &[
                "--model-draft",
                "/d/a.gguf",
                "--spec-draft-model",
                "/d/b.gguf",
            ],
            &[],
        ),
    ] {
        let result = extra(list, &["--model-draft"], &["/d"], fixed);
        assert!(
            matches!(result, Err(ProfileArgError::Duplicate(_))),
            "{list:?} beside {fixed:?}: {result:?}"
        );
    }
}

// T42 T37 (ADR 0029 §6, §8): sensitive options need the host's approval by
// name (any spelling approves the option), and every path a path option's
// value names lies inside security.approved_paths; a repository id is not
// a path.
#[test]
fn sensitive_options_need_approval_and_approved_paths() {
    let drafts = ["/srv/drafts"];
    assert_eq!(
        extra(&["--model-draft", "/srv/drafts/d.gguf"], &[], &drafts, &[]),
        Err(ProfileArgError::Sensitive("--model-draft".into()))
    );
    for approval in ["--model-draft", "--spec-draft-model"] {
        for name in ["--model-draft", "--spec-draft-model", "--spec_draft_model"] {
            extra(&[name, "/srv/drafts/d.gguf"], &[approval], &drafts, &[])
                .unwrap_or_else(|error| panic!("{name} approved as {approval}: {error}"));
        }
    }
    for value in [
        "/srv/other/d.gguf",
        "/srv/drafts/../etc/d.gguf",
        "ggml-org/Qwen3-0.6B-GGUF",
        "d.gguf",
    ] {
        assert_eq!(
            extra(&["--model-draft", value], &["--model-draft"], &drafts, &[]),
            Err(ProfileArgError::PathNotApproved("--model-draft".into())),
            "{value}"
        );
    }
    let lora = ["/srv/lora"];
    for (name, value, admitted) in [
        ("--lora", "/srv/lora/a.gguf,/srv/lora/b.gguf", true),
        ("--lora", "/srv/lora/a.gguf,/etc/b.gguf", false),
        ("--lora", "\"/srv/lora/a.gguf\"", false),
        (
            "--lora-scaled",
            "/srv/lora/a.gguf:0.5,/srv/lora/b.gguf:1",
            true,
        ),
        ("--lora-scaled", "/srv/lora/a.gguf:0.5,/etc/b.gguf:1", false),
        ("--lora-scaled", "/srv/lora/a.gguf", false),
        ("--control-vector", "/srv/lora/v.gguf,/tmp/v.gguf", false),
        ("--control-vector-scaled", "/srv/lora/v.gguf:0.8", true),
        ("--chat-template-file", "/srv/lora/t.jinja", true),
        ("--grammar-file", "/tmp/g.gbnf", false),
        ("--json-schema-file", "/srv/lora/s.json", true),
        ("--lookup-cache-static", "/srv/lora/l.bin", true),
        ("--lookup-cache-dynamic", "/var/tmp/l.bin", false),
        ("--media-path", "/srv/lora/media", true),
    ] {
        assert_eq!(
            extra(&[name, value], &[name], &lora, &[]).is_ok(),
            admitted,
            "{name} {value}"
        );
        assert_eq!(
            extra(&[name, value], &[], &lora, &[]),
            Err(ProfileArgError::Sensitive(name.into())),
            "{name} unapproved"
        );
    }
    assert_eq!(
        sensitivity(Engine::Llamacpp, "--video-ffmpeg-dir"),
        Some(Sensitivity::Code)
    );
    extra(
        &["--video-ffmpeg-dir", "/opt/ff/bin"],
        &["--video-ffmpeg-dir"],
        &[],
        &[],
    )
    .unwrap();
    // The name shapes still catch an unlisted path or configuration option.
    assert!(matches!(
        extra(&["--ui-config-file", "/tmp/c.json"], &[], &[], &[]),
        Err(ProfileArgError::Sensitive(_))
    ));
    // The host writes its own arguments: no approval needed there.
    validate_profile_args(
        Engine::Llamacpp,
        &args(&["--model-draft", "/anywhere/d.gguf"]),
        false,
    )
    .unwrap();
}

// T42 T37 (ADR 0029 §6): the built-in agent surface, the engine key, model
// sources and RPC are reserved: listing them in approved_options does not
// admit them.
#[test]
fn forbidden_options_are_refused_even_when_approved() {
    for list in [
        &["--tools", "all"][..],
        &["--agent"],
        &["--api-key", "k"],
        &["--hf-repo", "org/model"],
        &["--rpc", "10.0.0.2:50052"],
    ] {
        assert!(
            reserved_refusal(extra(list, &[list[0]], &["/"], &[])),
            "{list:?}"
        );
    }
}

// T42 T14 (ADR 0029 §6): the tables name real llama-server 0.6.0 options,
// and an option reserved or sensitive in one spelling is so in all of them.
#[test]
fn the_tables_cover_every_spelling_of_their_options() {
    let known: BTreeSet<&str> = options().into_iter().flatten().collect();
    let tables: Vec<&[&str]> = RESERVED_RENDERED
        .iter()
        .chain(RESERVED_NEVER_RENDERED)
        .copied()
        .chain(SENSITIVE.iter().map(|(row, _)| *row))
        .collect();
    for row in &tables {
        for name in *row {
            assert!(known.contains(name), "{name} is not a llama-server option");
        }
    }
    for option in options() {
        let classes: BTreeSet<String> = option
            .iter()
            .map(|name| {
                if reserved_refusal(extra(&[name], &[], &[], &[])) {
                    "reserved".to_owned()
                } else {
                    format!("{:?}", sensitivity(Engine::Llamacpp, name))
                }
            })
            .collect();
        assert_eq!(classes.len(), 1, "{option:?} is classed {classes:?}");
        let row = tables.iter().find(|row| row.contains(&option[0]));
        if let Some(row) = row {
            assert_eq!(
                row.iter().collect::<BTreeSet<_>>(),
                option.iter().collect::<BTreeSet<_>>(),
                "{option:?}"
            );
        }
    }
}

// T42 (ADR 0029 §6): host-fixed arguments follow the same exact-name policy.
#[test]
fn host_fixed_arguments_follow_the_exact_name_policy() {
    let mut spec = spec(false);
    spec.args = args(&["--threads", "8", "--cache-reuse", "256"]);
    check_profile("llamacpp", &profile_document(&spec)).unwrap();
    spec.args = args(&["--api_key", "k"]);
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

fn fixture() -> (Value, Value) {
    let all: Value = serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    let (mut deployment, mut host) = (all["deployment"].clone(), all["host"].clone());
    let profile = &mut host["runtime_profiles"]["local"];
    profile["engine"] = "llamacpp".into();
    profile["executable"] = "/opt/llama.cpp/bin/llama-server".into();
    profile["build_fingerprint"] = "0.6.0+d812350".into();
    profile["args"] = json!([]);
    profile["security"]["deep_park"] = "disabled".into();
    profile["security"]["approved_paths"] = json!(["/srv/drafts"]);
    deployment["residency"] = "restart_only".into();
    deployment["engine_config"] = json!({"context_length": 8192});
    (deployment, host)
}

fn llamacpp(
    effective: &capyctl_config::effective::EffectiveDeployment,
) -> &capyctl_domain::launch::LlamacppLaunchSettings {
    match &effective.engine_config {
        LaunchSettings::Llamacpp(settings) => settings,
        other => panic!("llama.cpp settings, not {other:?}"),
    }
}

const GIB: i64 = 1 << 30;

// T42 T14 (ADR 0029 §1, §5): a deployment resolves restart-only with
// CapyCTL's defaults named as such, and its snapshot re-resolves to itself.
#[test]
fn a_llamacpp_deployment_resolves_with_its_defaults() {
    let (mut deployment, mut host) = fixture();
    let effective = resolve_effective(&deployment, &host).unwrap();
    let settings = llamacpp(&effective);
    assert_eq!(settings.common.context_length, Some(8192));
    assert_eq!(settings.common.kv_cache_dtype.as_deref(), Some("f16"));
    assert_eq!(settings.n_gpu_layers, LlamacppGpuLayers::All);
    assert_eq!(settings.slots(), 4);
    for field in ["kv_cache_dtype", "llamacpp.n_gpu_layers"] {
        assert_eq!(
            settings.provenance.get(field),
            Some(&SettingSource::CapyctlDefault),
            "{field}"
        );
    }
    // ADR 0029 §9: until the KV is derived from the GGUF header, the declared
    // reservation bounds it.
    assert_eq!(settings.memory.kv_cache_bytes, 8 * GIB);
    let text = serde_json::to_string(&effective).unwrap();
    assert!(text.contains("\"engine\":\"llamacpp\""), "{text}");
    assert_eq!(decode_effective_snapshot(&text).unwrap(), effective);
    // Declared values are kept, and restated by the snapshot.
    deployment["engine_config"] = json!({"context_length": 15000, "max_concurrent_requests": 2,
        "kv_cache_dtype": "q8_0", "llamacpp": {"n_gpu_layers": 20,
        "gguf_file": "Q4_K_M/model-00001-of-00002.gguf", "mmproj_file": "mmproj-F16.gguf"}});
    let declared = resolve_effective(&deployment, &host).unwrap();
    let settings = llamacpp(&declared);
    assert_eq!(settings.n_gpu_layers, LlamacppGpuLayers::Count(20));
    assert_eq!(settings.slots(), 2);
    assert_eq!(settings.common.kv_cache_dtype.as_deref(), Some("q8_0"));
    assert!(!settings.provenance.contains_key("kv_cache_dtype"));
    let text = serde_json::to_string(&declared).unwrap();
    assert_eq!(decode_effective_snapshot(&text).unwrap(), declared);
    assert_ne!(declared.recipe_fingerprint, effective.recipe_fingerprint);
    // ADR 0029 §1: a profile that would allow deep parking still defaults
    // a llama.cpp deployment to restart_only.
    host["runtime_profiles"]["local"]["security"]["deep_park"] = "enabled".into();
    deployment.as_object_mut().unwrap().remove("residency");
    let defaulted = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(
        defaulted.residency,
        capyctl_config::effective::Residency::RestartOnly
    );
}

// T42 (ADR 0029 §5, plan ruling 3): the slots are the declared count or 4,
// with that source, in status and `validate config` alike, and they are the
// load read's running limit.
#[test]
fn the_slots_are_shown_with_their_source() {
    let (mut deployment, host) = fixture();
    for (declared, count, source, running) in [
        (None, 4, StreamsSource::Default, MaxRunningSource::Default),
        (
            Some(2),
            2,
            StreamsSource::Declared,
            MaxRunningSource::Declared,
        ),
    ] {
        deployment["engine_config"] = json!({"context_length": 8192});
        if let Some(declared) = declared {
            deployment["engine_config"]["max_concurrent_requests"] = json!(declared);
        }
        let effective = resolve_effective(&deployment, &host).unwrap();
        for (fit, remote) in [
            (fit_for_effective(&effective), false),
            (fit_on_remote_host(&effective), true),
        ] {
            assert_eq!(fit.tokens, Some(8192));
            let streams = fit.streams.clone().expect("llama.cpp slots");
            assert_eq!((streams.count, streams.source), (Some(count), source));
            let limit = max_running_for_effective(&effective, &fit, remote);
            assert_eq!((limit.count, limit.source), (Some(count), running));
        }
    }
}

// T42 (ADR 0029 §5): context_length is required; the cache type is one of
// llama.cpp's; the common fields llama.cpp has no flag for are refused
// naming their path; deep and host_backed are capability_missing.
#[test]
fn llamacpp_resolution_refuses_what_llama_cpp_cannot_do() {
    let (mut deployment, host) = fixture();
    deployment["engine_config"] = json!({});
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(
        (error.code, error.path.as_str()),
        (
            ConfigErrorCode::MissingRequired,
            "engine_config.context_length"
        )
    );
    deployment["engine_config"] = json!({"context_length": 8192, "kv_cache_dtype": "fp8"});
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(error.path, "engine_config.kv_cache_dtype");
    assert!(
        error.detail.contains("q8_0") && error.detail.contains("iq4_nl"),
        "{error}"
    );
    for (field, value) in [
        ("dtype", json!("bfloat16")),
        ("quantization", json!("awq")),
        ("cuda_graphs", json!(false)),
        ("trust_remote_code", json!(true)),
        ("language_model_only", json!(true)),
    ] {
        deployment["engine_config"] = json!({"context_length": 8192});
        deployment["engine_config"][field] = value;
        let error = resolve_effective(&deployment, &host).unwrap_err();
        assert_eq!(error.path, format!("engine_config.{field}"), "{error}");
        assert!(error.detail.contains("llama.cpp"), "{error}");
    }
    deployment["engine_config"] = json!({"context_length": 8192, "language_model_only": false,
        "trust_remote_code": false});
    resolve_effective(&deployment, &host).unwrap();
    for residency in ["deep", "host_backed"] {
        deployment["residency"] = residency.into();
        let error = resolve_effective(&deployment, &host).unwrap_err();
        assert_eq!(error.code, ConfigErrorCode::UnsupportedCombination);
        assert!(error.detail.contains("capability_missing"), "{error}");
    }
    deployment["residency"] = "restart_only".into();
    // A window times the slots that llama.cpp's context cannot hold.
    deployment["engine_config"] =
        json!({"context_length": 1_000_000_000, "max_concurrent_requests": 4});
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(error.path, "engine_config.context_length");
    // Another family's block, and llama.cpp's on another family.
    deployment["engine_config"] = json!({"context_length": 8192, "tensorfold": {"thinking": true}});
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(error.path, "engine_config.tensorfold");
    let all: Value = serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    let mut vllm = all["deployment"].clone();
    vllm["engine_config"]["llamacpp"] = json!({"n_gpu_layers": 10});
    let error = resolve_effective(&vllm, &all["host"]).unwrap_err();
    assert_eq!(error.path, "engine_config.llamacpp");
    // `n_gpu_layers` is a count or `all`.
    deployment["engine_config"] =
        json!({"context_length": 8192, "llamacpp": {"n_gpu_layers": "most"}});
    assert!(resolve_effective(&deployment, &host).is_err());
}

// T42 (ADR 0029 §9, plan slice L4): until the request is derived from the
// GGUF header, `resources` are required, offline too; the measured weights
// are taken from the declared reservation, which must hold them.
#[test]
fn a_llamacpp_deployment_states_its_resources() {
    let (mut deployment, host) = fixture();
    let mut bare = deployment.clone();
    bare.as_object_mut().unwrap().remove("resources");
    let error = resolve_effective(&bare, &host).unwrap_err();
    assert_eq!(
        (error.code, error.path.as_str()),
        (ConfigErrorCode::MissingRequired, "resources")
    );
    for offline in [
        json!({"runtime_profile": "llamacpp"}),
        json!({"engine_config": {"llamacpp": {}}}),
    ] {
        let error = validate_declared_resources(&offline).unwrap_err();
        assert_eq!(error.path, "resources");
    }
    let measured = |weights| CheckpointFacts {
        weights_bytes: Some(weights),
        ..CheckpointFacts::default()
    };
    let effective =
        resolve_effective_with_checkpoint(&deployment, &host, measured(2 * GIB)).unwrap();
    assert_eq!(llamacpp(&effective).memory.kv_cache_bytes, 6 * GIB);
    let error =
        resolve_effective_with_checkpoint(&deployment, &host, measured(9 * GIB)).unwrap_err();
    assert_eq!(error.path, "resources");
    // A declared KV cache is kept as declared.
    deployment["engine_config"]["memory"] = json!({"kv_cache": "1GiB"});
    let effective = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(llamacpp(&effective).memory.kv_cache_bytes, GIB);
}

// T42 T37 (ADR 0029 §6): LLAMA_ARG_*, LLAMA_API_KEY, LLAMA_SERVER_SLOTS_DEBUG,
// LLAMA_CACHE and XDG_CONFIG_HOME in a profile or deployment `env` are
// refused, even when approved_env lists them.
#[test]
fn hidden_inputs_in_the_environment_are_refused() {
    for name in [
        "LLAMA_ARG_CTX_SIZE",
        "LLAMA_API_KEY",
        "LLAMA_SERVER_SLOTS_DEBUG",
        "LLAMA_CACHE",
        "XDG_CONFIG_HOME",
        "llama_arg_n_parallel",
    ] {
        let (deployment, mut host) = fixture();
        host["runtime_profiles"]["local"]["env"] = json!({ name: "1" });
        let error = resolve_effective(&deployment, &host).unwrap_err();
        assert_eq!(error.path, "runtime_profiles.env", "{name}");
        assert_eq!(error.detail, format!("engine_env_reserved:{name}"));

        let (mut deployment, mut host) = fixture();
        host["runtime_profiles"]["local"]["security"]["approved_env"] =
            json!(["LLAMA_*", "XDG_CONFIG_HOME", "LLAMA_ARG_CTX_SIZE"]);
        deployment["engine_config"]["env"] = json!({ name.to_ascii_uppercase(): "1" });
        let error = resolve_effective(&deployment, &host).unwrap_err();
        assert_eq!(error.path, "engine_config.env", "{name}");
        assert!(error.detail.starts_with("engine_env_reserved:"), "{error}");
    }
    let mut spec = spec(false);
    spec.env = [("LLAMA_ARG_CTX_SIZE".to_owned(), "4096".to_owned())].into();
    let error = check_profile("llamacpp", &profile_document(&spec)).unwrap_err();
    assert_eq!(error.path, "runtime_profiles.env");
    // Another engine's environment is not llama.cpp's business.
    let all: Value = serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    let mut host = all["host"].clone();
    host["runtime_profiles"]["local"]["env"] = json!({"XDG_CONFIG_HOME": "/x"});
    resolve_effective(&all["deployment"], &host).unwrap();
}

// T42 (ADR 0029 §9): `gguf_file` and `mmproj_file` are relative `.gguf`
// paths inside the checkpoint.
#[test]
fn checkpoint_files_stay_inside_the_checkpoint() {
    let (mut deployment, host) = fixture();
    for field in ["gguf_file", "mmproj_file"] {
        for value in ["../x.gguf", "/srv/models/x.gguf", "a/./x.gguf", "x.bin", ""] {
            deployment["engine_config"] = json!({"context_length": 8192});
            deployment["engine_config"]["llamacpp"] = json!({ field: value });
            let error = resolve_effective(&deployment, &host).unwrap_err();
            assert_eq!(
                error.path,
                format!("engine_config.llamacpp.{field}"),
                "{value}"
            );
        }
    }
}

fn touch(root: &Path, files: &[&str]) {
    for file in files {
        let path = root.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"GGUF").unwrap();
    }
}

// T42 (ADR 0029 §9, review focus 4): the GGUF a launch renders: the one
// model file, the first shard of the one split set, or the file
// `gguf_file` names; several quantizations without it are refused naming
// each; a projector is never the model.
#[test]
fn several_gguf_candidates_need_gguf_file() {
    let one = tempfile::tempdir().unwrap();
    touch(
        one.path(),
        &["model-Q4_K_M.gguf", "mmproj-F16.gguf", "README.md"],
    );
    let pick = pick_gguf(one.path(), None).unwrap();
    assert_eq!(pick.file, Path::new("model-Q4_K_M.gguf"));
    assert_eq!(pick.files, [PathBuf::from("model-Q4_K_M.gguf")]);
    assert_eq!(
        projector_file(one.path(), "mmproj-F16.gguf").unwrap(),
        Path::new("mmproj-F16.gguf")
    );
    assert!(projector_file(one.path(), "absent.gguf").is_err());
    assert!(pick_gguf(one.path(), Some("mmproj-F16.gguf")).is_err());

    let split = tempfile::tempdir().unwrap();
    touch(
        split.path(),
        &[
            "Q8_0/m-00002-of-00003.gguf",
            "Q8_0/m-00001-of-00003.gguf",
            "Q8_0/m-00003-of-00003.gguf",
        ],
    );
    let pick = pick_gguf(split.path(), None).unwrap();
    assert_eq!(pick.file, Path::new("Q8_0/m-00001-of-00003.gguf"));
    assert_eq!(pick.files.len(), 3);
    let error = pick_gguf(split.path(), Some("Q8_0/m-00002-of-00003.gguf")).unwrap_err();
    assert!(error.contains("Q8_0/m-00001-of-00003.gguf"), "{error}");
    std::fs::remove_file(split.path().join("Q8_0/m-00003-of-00003.gguf")).unwrap();
    assert!(pick_gguf(split.path(), None)
        .unwrap_err()
        .contains("missing shards"));

    let two = tempfile::tempdir().unwrap();
    touch(two.path(), &["m-Q4_K_M.gguf", "m-Q8_0.gguf"]);
    let error = pick_gguf(two.path(), None).unwrap_err();
    assert!(
        error.contains("`m-Q4_K_M.gguf`") && error.contains("`m-Q8_0.gguf`"),
        "{error}"
    );
    assert!(
        error.contains("engine_config.llamacpp.gguf_file"),
        "{error}"
    );
    assert_eq!(
        pick_gguf(two.path(), Some("m-Q8_0.gguf")).unwrap().file,
        Path::new("m-Q8_0.gguf")
    );
    assert!(pick_gguf(two.path(), Some("m-Q2_K.gguf")).is_err());
    assert!(pick_gguf(two.path(), Some("../m-Q8_0.gguf")).is_err());

    let none = tempfile::tempdir().unwrap();
    touch(none.path(), &["mmproj-F16.gguf", "config.json"]);
    assert!(pick_gguf(none.path(), None)
        .unwrap_err()
        .contains("no GGUF"));
}

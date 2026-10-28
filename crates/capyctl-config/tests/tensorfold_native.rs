//! ADR 0039: resolution of a deployment on TensorFold's native line (1.x):
//! the exact-name option policy, the native refusals, zero retention, the
//! engine environment, the start bound and the uncertified notes. CPU tests
//! only; none of this qualifies TensorFold native (the TF rows do).

use capyctl_config::effective::{
    check_runtime_profile, resolve_effective, EffectiveDeployment, TENSORFOLD_FIRST_BUILD_MS,
};
use capyctl_config::engine_policy::{validate_extra_args, Engine, ExtraArgsContext};
use capyctl_config::model_source::ModelSource;
use capyctl_config::tensorfold_native::{
    self, deployment_notes, TensorfoldLine, CHECKPOINT_NOTE, DRAFTER_REFUSAL,
    KV_CACHE_DTYPE_REFUSAL, RESERVED_FLAGS, RETENTION_ZERO_COMPACTION, RETENTION_ZERO_ENGINE_DISK,
};
use serde_json::{json, Value};
use std::collections::BTreeSet;

/// A TensorFold 0.6 deployment, as `tests/tensorfold.rs` builds it.
fn python() -> (Value, Value) {
    let all: Value =
        serde_json::from_str(include_str!("fixtures/f2-deployment.json")).expect("fixture JSON");
    let (mut deployment, mut host) = (all["deployment"].clone(), all["host"].clone());
    let profile = &mut host["runtime_profiles"]["local"];
    profile["engine"] = "tensorfold".into();
    profile["executable"] = "/opt/tf/bin/tensorfold".into();
    profile["build_fingerprint"] = "0.6.0".into();
    profile["args"] = json!([]);
    profile["security"]["deep_park"] = "disabled".into();
    deployment["residency"] = "restart_only".into();
    deployment["engine_config"] = json!({"context_length": 8192});
    (deployment, host)
}

/// The same deployment on a native profile at an uncertified 1.0.3 whose
/// capabilities document listed Nemotron-H in MLX 4-bit, group 64.
fn native() -> (Value, Value) {
    let (deployment, mut host) = python();
    let profile = &mut host["runtime_profiles"]["local"];
    profile["line"] = "native".into();
    profile["executable"] = "/opt/tensorfold-1.0.3/bin/tensorfold-native".into();
    profile["build_fingerprint"] = "1.0.3".into();
    profile["native"] = json!({
        "families": {"nemotron_h": ["mlx-q4g64"]},
        "checksums_verified": true,
    });
    (deployment, host)
}

fn with_extra(deployment: &mut Value, extra: &[&str]) {
    deployment["engine_config"]["accept_extra_args"] = true.into();
    deployment["engine_config"]["extra_args"] = json!(extra);
}

fn strings(tokens: &[&str]) -> Vec<String> {
    tokens.iter().map(|t| (*t).to_owned()).collect()
}

fn context<'a>(line: TensorfoldLine, none: &'a BTreeSet<String>) -> ExtraArgsContext<'a> {
    ExtraArgsContext {
        engine: Engine::Tensorfold,
        line,
        sleep_mode: false,
        approved_options: none,
        approved_paths: &[],
        checkpoint_root: None,
        host_fixed: none,
    }
}

// T41 T08: a native deployment resolves, carries its line into the frozen
// revision, and `--compact-at auto` is an ordinary extra argument (owner
// decision 2026-10-27, question 4).
#[test]
fn a_native_deployment_resolves_with_compaction_in_extra_args() {
    let (mut deployment, host) = native();
    with_extra(&mut deployment, &["--compact-at", "auto"]);
    let effective = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(effective.profile.line, TensorfoldLine::Native);
    let raw = serde_json::to_value(&effective).unwrap().to_string();
    assert_eq!(
        capyctl_config::effective::decode_effective_snapshot(&raw).unwrap(),
        effective
    );
    for extra in [
        &["--compact-keep", "4"][..],
        &["--compact-memory"],
        &["--record"],
        &["--learn-dir", "/srv/learn"],
        &["--recall"],
        &["--no-drafts"],
        &["--segments", "2"],
    ] {
        let (mut deployment, host) = native();
        with_extra(&mut deployment, extra);
        resolve_effective(&deployment, &host).unwrap_or_else(|e| panic!("{extra:?}: {e}"));
    }
}

// T41 T21 T37 (ADR 0039 §4, ADR 0033 §10): under the deployment's
// `retention: zero`, compaction is refused `retention_zero:compaction` and an
// option that writes request state to disk `retention_zero:engine_disk`, in
// the extra and in the host-fixed arguments. The 0.6 line is unchanged.
#[test]
fn zero_retention_refuses_compaction_and_disk_options() {
    for (args, code) in [
        (&["--compact-at", "auto"][..], RETENTION_ZERO_COMPACTION),
        (&["--compact-keep=4"], RETENTION_ZERO_COMPACTION),
        (&["--compact-memory"], RETENTION_ZERO_COMPACTION),
        (&["--record"], RETENTION_ZERO_ENGINE_DISK),
        (&["--learn"], RETENTION_ZERO_ENGINE_DISK),
        (&["--learn-dir", "/srv/learn"], RETENTION_ZERO_ENGINE_DISK),
        (&["--learn-conversations"], RETENTION_ZERO_ENGINE_DISK),
        (&["--recall"], RETENTION_ZERO_ENGINE_DISK),
        (&["--recall-dir", "/srv/recall"], RETENTION_ZERO_ENGINE_DISK),
    ] {
        let (mut deployment, host) = native();
        deployment["retention"] = "zero".into();
        with_extra(&mut deployment, args);
        let error = resolve_effective(&deployment, &host).unwrap_err();
        assert_eq!(error.path, "engine_config.extra_args", "{args:?}");
        assert!(
            error.detail.starts_with(&format!("{code}: `")),
            "{args:?}: {error}"
        );
        assert!(error.detail.contains("TensorFold native"), "{error}");

        let (mut deployment, mut host) = native();
        deployment["retention"] = "zero".into();
        host["runtime_profiles"]["local"]["args"] = json!(args);
        let error = resolve_effective(&deployment, &host).unwrap_err();
        assert_eq!(error.path, "runtime_profiles.args", "{args:?}");
        assert!(error.detail.starts_with(code), "{args:?}: {error}");

        // Standard retention runs them.
        let (mut deployment, host) = native();
        with_extra(&mut deployment, args);
        resolve_effective(&deployment, &host).unwrap_or_else(|e| panic!("{args:?}: {e}"));
    }
    // The 0.6 line has no such refusal.
    let (mut deployment, host) = python();
    deployment["retention"] = "zero".into();
    with_extra(&mut deployment, &["--compact-at", "auto"]);
    resolve_effective(&deployment, &host).expect("0.6 unchanged");
}

// T41 T21 (ADR 0039 §4): every reserved option is refused by its exact name
// and with `=value`, in extra and host-fixed arguments; an abbreviation is
// not refused on the native line (its parser expands none) while the 0.6
// line still refuses it.
#[test]
fn reserved_native_options_are_refused_by_exact_name() {
    let none = BTreeSet::new();
    for name in RESERVED_FLAGS.iter().copied().chain(["--capyctl-x"]) {
        for args in [strings(&[name, "x"]), strings(&[&format!("{name}=x")])] {
            let error = validate_extra_args(&args, &context(TensorfoldLine::Native, &none))
                .expect_err(&format!("{args:?}"));
            assert!(error.to_string().contains("reserved"), "{args:?}: {error}");
            let (deployment, mut host) = native();
            host["runtime_profiles"]["local"]["args"] = json!(args);
            let error = resolve_effective(&deployment, &host).unwrap_err();
            assert_eq!(error.path, "runtime_profiles.args", "{args:?}");
            let mut profile = host["runtime_profiles"]["local"].clone();
            profile["revision"] = 1.into();
            assert!(check_runtime_profile(&profile).is_err(), "{args:?}");
        }
    }
    // `--idle-unload 60` in extra_args (TF12) is refused at resolution.
    let (mut deployment, host) = native();
    with_extra(&mut deployment, &["--idle-unload", "60"]);
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert!(error.detail.contains("--idle-unload"), "{error}");

    let cont = strings(&["--cont", "4096"]);
    validate_extra_args(&cont, &context(TensorfoldLine::Native, &none))
        .expect("no prefix rule on the native line");
    assert!(validate_extra_args(&cont, &context(TensorfoldLine::Python, &none)).is_err());
    let (mut deployment, host) = native();
    with_extra(&mut deployment, &["--cont", "4096"]);
    resolve_effective(&deployment, &host).expect("the binary judges `--cont` at start");
    let (deployment, mut host) = native();
    host["runtime_profiles"]["local"]["args"] = json!(["--cont", "4096"]);
    resolve_effective(&deployment, &host).expect("host-fixed `--cont` on native");
    let mut profile = host["runtime_profiles"]["local"].clone();
    profile["revision"] = 1.into();
    check_runtime_profile(&profile).expect("engine add takes `--cont` on native");
    let (deployment, mut host) = python();
    host["runtime_profiles"]["local"]["args"] = json!(["--cont", "4096"]);
    assert!(resolve_effective(&deployment, &host).is_err());
    // 0.6's sensitive table does not apply: `--vision-urls` needs no approval
    // here (the 1.0 binary refuses it at start).
    validate_extra_args(
        &strings(&["--vision-urls"]),
        &context(TensorfoldLine::Native, &none),
    )
    .expect("no 0.6 sensitive table");
}

// T41 T03 (ADR 0039 §6): the native refusals, each with its code.
#[test]
fn native_refusals_carry_their_codes() {
    let (mut deployment, host) = native();
    deployment["engine_config"]["kv_cache_dtype"] = "int8".into();
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(error.path, "engine_config.kv_cache_dtype");
    assert_eq!(error.detail, KV_CACHE_DTYPE_REFUSAL);

    let (mut deployment, host) = native();
    deployment["model"]["draft"] = json!({"type": "local", "path": "/srv/drafters/d"});
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(error.path, "model.draft", "{error}");
    assert_eq!(error.detail, DRAFTER_REFUSAL);

    let (mut deployment, host) = native();
    with_extra(&mut deployment, &["--drafter", "/srv/drafters/d"]);
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert!(error.detail.starts_with(DRAFTER_REFUSAL), "{error}");
    let (deployment, mut host) = native();
    host["runtime_profiles"]["local"]["args"] = json!(["--drafter=none"]);
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert!(error.detail.starts_with("capability_missing:"), "{error}");

    for topology in [json!({"tensor_parallel": 2}), json!("auto")] {
        let (mut deployment, host) = native();
        deployment["topology"] = topology.clone();
        // A claim on one GPU contradicts several; leave the choice to the host.
        deployment.as_object_mut().unwrap().remove("devices");
        if topology == "auto" {
            // `auto` sizes the GPUs itself; a resources block naming one is
            // refused before the engine is looked at.
            deployment.as_object_mut().unwrap().remove("resources");
        }
        let error = resolve_effective(&deployment, &host).unwrap_err();
        assert_eq!(error.path, "topology", "{topology}: {error}");
        assert!(
            error
                .detail
                .starts_with("multi_gpu_unsupported: the TensorFold native line"),
            "{topology}: {error}"
        );
    }
    // A group (ADR 0028) is refused with the group code, naming the line.
    let (mut deployment, host) = native();
    deployment["topology"] = json!({"tensor_parallel": 2});
    deployment["placement"] = json!({"hosts": ["a", "b"]});
    deployment.as_object_mut().unwrap().remove("devices");
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert!(
        error
            .detail
            .starts_with("group_shape_unsupported:tensorfold: the TensorFold native line"),
        "{error}"
    );

    let (mut deployment, host) = native();
    deployment["api"] = "decisions".into();
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert!(
        error.detail.starts_with("capability_missing:decisions"),
        "{error}"
    );

    for residency in ["deep", "host_backed"] {
        let (mut deployment, host) = native();
        deployment["residency"] = residency.into();
        let error = resolve_effective(&deployment, &host).unwrap_err();
        assert!(error.detail.starts_with("capability_missing"), "{error}");
    }
}

// T41 T37 (ADR 0039 §5): the names the native engine reads and CapyCTL never
// carries are refused with why, as the profile's env, its approvals or a
// deployment's env; the reserve CapyCTL renders is owned on the native line
// only.
#[test]
fn refused_native_environment_names() {
    for (name, why) in tensorfold_native::REFUSED_ENV {
        let (deployment, mut host) = native();
        host["runtime_profiles"]["local"]["env"] = json!({*name: "x"});
        let error = resolve_effective(&deployment, &host).unwrap_err();
        assert_eq!(error.path, "runtime_profiles.env", "{name}");
        assert!(
            error
                .detail
                .starts_with(&format!("engine_env_reserved:{name}: ")),
            "{error}"
        );
        assert!(error.detail.contains(why), "{error}");

        let (deployment, mut host) = native();
        host["runtime_profiles"]["local"]["security"]["approved_env"] = json!([name]);
        let error = resolve_effective(&deployment, &host).unwrap_err();
        assert_eq!(error.path, "runtime_profiles.security.approved_env");

        // A glob approval does not carry it either.
        let stem = &name[..name.find('_').unwrap() + 1];
        let (mut deployment, mut host) = native();
        host["runtime_profiles"]["local"]["security"]["approved_env"] = json!([format!("{stem}*")]);
        deployment["engine_config"]["env"] = json!({*name: "x"});
        let error = resolve_effective(&deployment, &host).unwrap_err();
        assert_eq!(error.path, "engine_config.env", "{name}");
        assert!(error.detail.contains(why), "{error}");
    }
    let reserve = "TENSORFOLD_MEMORY_RESERVE_GIB";
    let approve = |(mut deployment, mut host): (Value, Value)| {
        host["runtime_profiles"]["local"]["security"]["approved_env"] = json!(["TENSORFOLD_*"]);
        deployment["engine_config"]["env"] = json!({reserve: "8"});
        resolve_effective(&deployment, &host)
    };
    let error = approve(native()).unwrap_err();
    assert!(
        error
            .detail
            .starts_with(&format!("engine_env_reserved:{reserve}")),
        "{error}"
    );
    approve(python()).expect("0.6 keeps the reserve approvable");
}

// T41 T14 (ADR 0039 §6): the native line's start bound is the weights-based
// one and its request deadline gets no 1800 s floor; 0.6 keeps both.
#[test]
fn the_native_line_has_no_first_build_bound_or_deadline_floor() {
    let undeclared = |(mut deployment, host): (Value, Value)| {
        deployment
            .as_object_mut()
            .unwrap()
            .remove("request_deadline");
        resolve_effective(&deployment, &host).unwrap()
    };
    let python = undeclared(python());
    assert_eq!(python.timeouts.initialize_ms, TENSORFOLD_FIRST_BUILD_MS);
    assert_eq!(python.request_deadline_ms, TENSORFOLD_FIRST_BUILD_MS);
    let native_effective = undeclared(native());
    assert!(native_effective.request_deadline_ms < TENSORFOLD_FIRST_BUILD_MS);
    assert_ne!(
        native_effective.timeouts.initialize_ms,
        TENSORFOLD_FIRST_BUILD_MS
    );
    assert!(native_effective.timeouts.initialize_ms <= native_effective.request_deadline_ms);
    // A declared deadline above the host's may not reach the old bound.
    let (mut deployment, host) = native();
    deployment["request_deadline"] = "1800s".into();
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(error.path, "request_deadline", "{error}");
}

// T41 (ADR 0039 §4, Evidence §2, §6): `--parallel` is found by its exact
// name; the native `auto` is up to eight streams, and the 0.6.3-0.6.5
// Nemotron-H one-at-a-time clamp is the Python line's only.
#[test]
fn native_streams_read_parallel_exactly() {
    use capyctl_config::context_fit::{fit_for_launch, StreamsSource};
    let (mut deployment, host) = native();
    deployment["engine_config"]["max_concurrent_requests"] = 4.into();
    with_extra(&mut deployment, &["--parallel", "8"]);
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(error.path, "engine_config.max_concurrent_requests");
    // An abbreviation is not `--parallel` to the native parser.
    with_extra(&mut deployment, &["--par", "8"]);
    resolve_effective(&deployment, &host).expect("`--par` is not `--parallel` on native");

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("config.json"),
        r#"{"model_type": "nemotron_h"}"#,
    )
    .unwrap();
    let streams = |deployment: &Value, host: &Value| {
        let effective = resolve_effective(deployment, host).unwrap();
        fit_for_launch(
            Engine::Tensorfold,
            effective.profile.line,
            &effective.engine_config,
            &effective.profile.args,
            Some(tmp.path()),
            None,
        )
        .streams
        .unwrap()
    };
    let (mut deployment, host) = native();
    with_extra(&mut deployment, &["--parallel=auto"]);
    let native_auto = streams(&deployment, &host);
    assert_eq!(
        (native_auto.count, native_auto.source),
        (Some(8), StreamsSource::ExtraArgs)
    );
    let (deployment, host) = native();
    let native_default = streams(&deployment, &host);
    assert_eq!(
        native_default.count,
        Some(capyctl_domain::launch::TENSORFOLD_DEFAULT_PARALLEL)
    );
    assert_eq!(native_default.reason, None);
    let (deployment, host) = python();
    assert_eq!(streams(&deployment, &host).count, Some(1));
}

fn with_checkpoint(
    effective: &mut EffectiveDeployment,
    config: Option<&Value>,
) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    if let Some(config) = config {
        std::fs::write(tmp.path().join("config.json"), config.to_string()).unwrap();
    }
    effective.model.resolved_path = Some(tmp.path().display().to_string());
    tmp
}

// T41 (ADR 0039 §3, §7, owner decisions 2026-10-27 questions 1 and 2): an
// uncertified version and checkpoint run with notes, never a refusal: a
// Qwen3 checkpoint the installation does not list adds the unlisted-family
// note, a fine-tune of the listed family and format has the checkpoint note
// alone, and the certified checkpoint has none. The 0.6 line has no notes.
#[test]
fn uncertified_versions_and_checkpoints_run_with_notes() {
    let (deployment, host) = native();
    let resolved = resolve_effective(&deployment, &host).expect("1.0.3 is never refused");
    let version = tensorfold_native::version_note("1.0.3").unwrap();
    assert!(version.contains("1.0.3"), "{version}");

    let mut qwen = resolved.clone();
    let _dir = with_checkpoint(&mut qwen, Some(&json!({"model_type": "qwen3"})));
    assert_eq!(
        deployment_notes(&qwen, true),
        vec![
            version.clone(),
            CHECKPOINT_NOTE.to_owned(),
            "this installation does not list family qwen3; the engine is expected to refuse it \
             at start"
                .to_owned(),
        ]
    );
    // A server never reads a remote host's checkpoint: the notes without it.
    assert_eq!(
        deployment_notes(&qwen, false),
        vec![version.clone(), CHECKPOINT_NOTE.to_owned()]
    );

    let mut fine_tune = resolved.clone();
    let _dir = with_checkpoint(
        &mut fine_tune,
        Some(&json!({"model_type": "nemotron_h",
            "quantization": {"bits": 4, "group_size": 64, "mode": "affine"}})),
    );
    assert_eq!(
        deployment_notes(&fine_tune, true),
        vec![version.clone(), CHECKPOINT_NOTE.to_owned()]
    );

    let mut certified = resolved.clone();
    certified.profile.build_fingerprint = "1.0.0".into();
    certified.model.source = ModelSource::HuggingFace {
        repo: tensorfold_native::CERTIFIED_CHECKPOINT_REPOS[0].into(),
        revision: tensorfold_native::CERTIFIED_CHECKPOINT_REVISION.into(),
        files: Vec::new(),
        token_ref: None,
    };
    let _dir = with_checkpoint(
        &mut certified,
        Some(&json!({"model_type": "nemotron_h",
            "quantization": {"bits": 4, "group_size": 64}})),
    );
    let notes = deployment_notes(&certified, true);
    assert!(
        !notes.iter().any(|note| note == CHECKPOINT_NOTE),
        "{notes:?}"
    );
    // 1.0.0 is certified once TF1 to TF10 pass; until then its note says so.
    assert_eq!(
        notes,
        tensorfold_native::version_note("1.0.0")
            .into_iter()
            .collect::<Vec<_>>()
    );

    let (deployment, host) = python();
    let python = resolve_effective(&deployment, &host).unwrap();
    assert!(deployment_notes(&python, true).is_empty());
}

// T41 (ADR 0039 §1, §2): a host role document (the remote agent's own
// document) holds a native profile with its registration record, and the
// deployment resolves on it from that document as from a host document.
// Before the role schema took `line` and `native`, the agent refused the
// whole document (`unknown field line`).
#[test]
fn a_host_role_document_holds_a_native_profile() {
    use capyctl_config::remote_roles::HostConfig;
    let root = std::path::Path::new("/home/operator/host");
    let mut document: Value = serde_json::from_str(&HostConfig::template(root)).unwrap();
    let (deployment, host) = native();
    for field in [
        "hardware_fingerprint",
        "environment_fingerprint",
        "resource_policy",
        "runtime_profiles",
    ] {
        document[field] = host[field].clone();
    }
    let config = HostConfig::parse(&document.to_string()).unwrap();
    let profile = &config.profiles["local"];
    assert_eq!(profile["line"], "native");
    assert_eq!(
        profile["native"]["families"]["nemotron_h"],
        json!(["mlx-q4g64"])
    );
    let local = capyctl_config::remote_resources::local_host_document(&config.document).unwrap();
    let effective = resolve_effective(&deployment, &local).unwrap();
    assert_eq!(effective.profile.line, TensorfoldLine::Native);
}

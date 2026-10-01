//! ADR 0023 §3: the TensorFold command and its closed environment. CPU tests
//! only; they prove the rendering, not that TensorFold starts.
use capyctl_adapters::tensorfold::{
    engine_environment, plan_from_effective, render_command, PlanInputTensorfold,
    TensorfoldPlanError,
};
use capyctl_config::effective::{derived_initialize_ms, resolve_effective};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn plan() -> PlanInputTensorfold {
    PlanInputTensorfold {
        engine_bin: "/opt/tf/bin/tensorfold".into(),
        engine_path_extra: Some("/opt/tf/bin".into()),
        model_path: "/srv/models/nemotron".into(),
        served_model_name: "nemotron".into(),
        port: 8101,
        context_length: 32768,
        extensions_dir: Some("/var/lib/capyctl/engines/tensorfold/0.6.0/torch_extensions".into()),
        engine_log: Some("/var/lib/capyctl/logs/i.log".into()),
        ..PlanInputTensorfold::default()
    }
}

fn fixture() -> (Value, Value) {
    let all: Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/f2-deployment.json"
    ))
    .expect("fixture JSON");
    let (mut deployment, mut host) = (all["deployment"].clone(), all["host"].clone());
    let profile = &mut host["runtime_profiles"]["local"];
    profile["engine"] = "tensorfold".into();
    profile["executable"] = "/opt/tf/bin/tensorfold".into();
    profile["build_fingerprint"] = "0.6.0".into();
    profile["args"] = json!([]);
    profile["security"]["deep_park"] = "disabled".into();
    profile["security"]["approved_paths"] = json!(["/srv/drafters"]);
    deployment["residency"] = "restart_only".into();
    deployment["engine_config"] = json!({"context_length": 8192});
    (deployment, host)
}

// T41 T14: ADR 0023 §3's command line, its fixed flags in order.
#[test]
fn the_command_is_serve_with_the_reserved_settings_rendered() {
    let cmd = render_command(&plan()).unwrap();
    assert_eq!(
        cmd.argv,
        [
            "/opt/tf/bin/tensorfold",
            "serve",
            "/srv/models/nemotron",
            "--name",
            "nemotron",
            "--host",
            "127.0.0.1",
            "--port",
            "8101",
            "--no-update-check",
            "--backend",
            "cuda",
            "--snapshot-dir",
            "none",
            "--context",
            "32768",
            "--drafter",
            "none",
        ]
    );
    assert_eq!(cmd.env["TENSORFOLD_NO_UPDATE_CHECK"], "1");
    assert_eq!(
        cmd.env["TORCH_EXTENSIONS_DIR"],
        "/var/lib/capyctl/engines/tensorfold/0.6.0/torch_extensions"
    );
    assert_eq!(cmd.env["HF_HUB_OFFLINE"], "1");
    assert!(!cmd.argv.iter().any(|a| a.contains("key")));
}

// T41 T14: typed fields render in TensorFold's spelling; then host-fixed
// arguments, then extra arguments.
#[test]
fn typed_fields_host_args_and_extras_render_in_order() {
    let mut input = plan();
    input.kv_dtype = Some("int8".into());
    input.max_tokens = Some(1024);
    input.thinking = Some(false);
    input.engine_args = vec!["--parallel".into(), "2".into()];
    // ADR 0023 §5: an approved `--drafter` extra replaces capyctl's `--drafter none`.
    input.extra_args = vec![
        "--vision".into(),
        "--drafter".into(),
        "/srv/drafters/d".into(),
    ];
    let argv = render_command(&input).unwrap().argv;
    // argv[0..14] is the fixed head (binary through `--snapshot-dir none`).
    let tail: Vec<&str> = argv[14..].iter().map(String::as_str).collect();
    assert_eq!(
        tail,
        [
            "--context",
            "32768",
            "--kv-dtype",
            "int8",
            "--max-tokens",
            "1024",
            "--no-thinking",
            "--parallel",
            "2",
            "--vision",
            "--drafter",
            "/srv/drafters/d",
        ]
    );
    input.thinking = Some(true);
    assert!(render_command(&input)
        .unwrap()
        .argv
        .contains(&"--thinking".to_string()));
}

// T41 T14: a reserved or typed name in the pass-through vector is refused
// again at render time, abbreviations included.
#[test]
fn reserved_names_are_refused_again_when_rendering() {
    for extra in [
        vec!["--host", "0.0.0.0"],
        vec!["--snap", "/x"],
        vec!["--no-update-check"],
        vec!["--kv-dtype", "int4"],
        vec!["--kv_dtype", "int4"],
        vec!["--max-tok", "8"],
        vec!["--thinking"],
        vec!["--no-thinking"],
        vec!["--capyctl-x"],
    ] {
        let mut input = plan();
        input.extra_args = extra.iter().map(|s| s.to_string()).collect();
        assert!(render_command(&input).is_err(), "{extra:?}");
    }
    // ADR 0023 §4: a host-fixed argument may set a typed option the deployment
    // left unset, never one it set, in any spelling.
    let mut input = plan();
    input.engine_args = vec!["--max-tokens".into(), "512".into()];
    render_command(&input).unwrap();
    input.max_tokens = Some(1024);
    assert!(render_command(&input).is_err(), "typed field set twice");
    input.engine_args = vec!["--max_tok=512".into()];
    assert!(render_command(&input).is_err(), "abbreviated typed field");
    let mut input = plan();
    input.thinking = Some(true);
    input.engine_args = vec!["--no-thinking".into()];
    assert!(render_command(&input).is_err(), "negated typed field");

    let mut input = plan();
    input.context_length = 0;
    assert!(render_command(&input).is_err(), "a context is required");
}

// T41 T37: the environment is a closed allowlist with the closed PATH.
#[test]
fn the_engine_environment_is_closed() {
    let mut input = plan();
    input.cuda_home = Some("/usr/local/cuda".into());
    let rendered = render_command(&input).unwrap().env;
    let inherited = |name: &str| match name {
        "HOME" => Some("/home/svc".to_string()),
        "CUDA_VISIBLE_DEVICES" => Some("0".to_string()),
        "LD_PRELOAD" | "PYTHONPATH" | "HF_TOKEN" => Some("bad".to_string()),
        _ => None,
    };
    let toolchain = BTreeMap::from([
        ("CUDA_HOME".to_string(), "/usr/local/cuda".to_string()),
        ("MAX_JOBS".to_string(), "4".to_string()),
    ]);
    let env = engine_environment(&rendered, &input, &inherited, &toolchain);
    assert_eq!(
        env["PATH"],
        "/opt/tf/bin:/usr/local/cuda/bin:/usr/local/bin:/usr/bin:/bin"
    );
    assert_eq!(env["HOME"], "/home/svc");
    assert_eq!(env["CUDA_VISIBLE_DEVICES"], "0");
    assert_eq!(env["MAX_JOBS"], "4");
    for absent in ["LD_PRELOAD", "PYTHONPATH", "HF_TOKEN", "VLLM_API_KEY"] {
        assert!(!env.contains_key(absent), "{absent}");
    }
    assert_eq!(env["CAPYCTL_ENGINE_LOG"], "/var/lib/capyctl/logs/i.log");
}

// T41
#[test]
fn the_plan_comes_from_the_resolved_deployment() {
    let (deployment, host) = fixture();
    let effective = resolve_effective(&deployment, &host).unwrap();
    let plan = plan_from_effective(&effective, 8101, "/var/log/i.log".into(), None).unwrap();
    assert_eq!(plan.engine_bin, "/opt/tf/bin/tensorfold");
    assert_eq!(plan.engine_path_extra.as_deref(), Some("/opt/tf/bin"));
    assert_eq!(plan.served_model_name, "toy");
    assert_eq!(plan.model_path, "/srv/models/toy");
    assert_eq!(plan.context_length, 8192);
    assert_eq!(plan.port, 8101);
    assert_eq!(plan.engine_log.as_deref(), Some("/var/log/i.log"));
    // ADR 0023 §4: the derived bound is capped by the request deadline.
    assert_eq!(
        plan.warm_startup_ms,
        derived_initialize_ms(None).min(effective.request_deadline_ms)
    );
    render_command(&plan).unwrap();
}

// T41 T37: an approved `--drafter` path is checked again at launch, through
// symlinks, as `runtime/extra_args_policy.py` does for vLLM and SGLang.
#[test]
fn an_approved_drafter_path_is_checked_through_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let drafters = root.join("drafters");
    let outside = root.join("outside");
    std::fs::create_dir_all(&drafters).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let drafter = drafters.join("d");
    let resolve = || {
        let (mut deployment, mut host) = fixture();
        let security = &mut host["runtime_profiles"]["local"]["security"];
        security["approved_options"] = json!(["--drafter"]);
        security["approved_paths"] = json!([drafters.to_str().unwrap()]);
        deployment["engine_config"] = json!({
            "context_length": 8192,
            "accept_extra_args": true,
            "extra_args": ["--drafter", drafter.to_str().unwrap()],
        });
        resolve_effective(&deployment, &host).unwrap()
    };

    std::fs::create_dir(&drafter).unwrap();
    let plan = plan_from_effective(&resolve(), 8101, "/var/log/i.log".into(), None).unwrap();
    let argv = render_command(&plan).unwrap().argv;
    let at = argv.iter().position(|a| a == "--drafter").unwrap();
    assert_eq!(argv[at + 1], drafter.to_str().unwrap());
    assert_eq!(argv.iter().filter(|a| *a == "--drafter").count(), 1);

    std::fs::remove_dir(&drafter).unwrap();
    std::os::unix::fs::symlink(&outside, &drafter).unwrap();
    let error = plan_from_effective(&resolve(), 8101, "/var/log/i.log".into(), None).unwrap_err();
    assert!(
        matches!(error, TensorfoldPlanError::PathNotApproved(ref name) if name == "--drafter"),
        "{error}"
    );

    std::fs::remove_file(&drafter).unwrap();
    let error = plan_from_effective(&resolve(), 8101, "/var/log/i.log".into(), None).unwrap_err();
    assert!(
        matches!(error, TensorfoldPlanError::PathNotApproved(_)),
        "a missing drafter is not inside an approved path: {error}"
    );
}

//! ADR 0023 §3: the TensorFold command and its closed environment. CPU tests
//! only; they prove the rendering, not that TensorFold starts.
use capyctl_adapters::tensorfold::{
    engine_environment, plan_from_effective, render_command, PlanInputTensorfold,
    TensorfoldArgsError, TensorfoldPlanError,
};
use capyctl_adapters::traits::RenderedCommand;
use capyctl_config::effective::{derived_initialize_ms, resolve_effective};
use capyctl_domain::group::GroupMemberArgs;
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

// T41 T14 (ADR 0023 §3, §5): the drafter choice. Nothing said renders
// `--drafter none`, so TensorFold never picks one from a cache; a named drafter
// renders as given; `--no-drafts` renders alone, without `--drafter none`.
#[test]
fn the_drafter_choice_renders_once() {
    let drafter_none = |argv: &[String]| {
        argv.windows(2)
            .any(|pair| pair[0] == "--drafter" && pair[1] == "none")
    };
    let argv = render_command(&plan()).unwrap().argv;
    assert!(drafter_none(&argv), "{argv:?}");

    let mut input = plan();
    input.extra_args = vec!["--drafter".into(), "/srv/drafters/d".into()];
    let argv = render_command(&input).unwrap().argv;
    assert!(!drafter_none(&argv), "{argv:?}");
    assert!(argv.ends_with(&["--drafter".to_string(), "/srv/drafters/d".to_string()]));

    for off in ["--no-drafts", "--no_drafts", "--no-draft"] {
        let mut input = plan();
        input.extra_args = vec![off.into()];
        let argv = render_command(&input).unwrap().argv;
        assert!(!argv.iter().any(|a| a == "--drafter"), "{off}: {argv:?}");
        assert_eq!(argv.last().map(String::as_str), Some(off));
    }
    // A host-fixed `--no-drafts` turns them off too.
    let mut input = plan();
    input.engine_args = vec!["--no-drafts".into()];
    assert!(!render_command(&input)
        .unwrap()
        .argv
        .iter()
        .any(|a| a == "--drafter"));
}

// T41 T14: drafts off and a named drafter contradict each other, wherever
// each comes from.
#[test]
fn no_drafts_beside_a_drafter_is_refused() {
    for (fixed, extra) in [
        (vec![], vec!["--no-drafts", "--drafter", "/srv/drafters/d"]),
        (vec!["--drafter", "/srv/drafters/d"], vec!["--no-drafts"]),
        (vec!["--no-drafts"], vec!["--drafter=/srv/drafters/d"]),
    ] {
        let mut input = plan();
        input.engine_args = fixed.iter().map(|s| s.to_string()).collect();
        input.extra_args = extra.iter().map(|s| s.to_string()).collect();
        let error = render_command(&input).unwrap_err().to_string();
        assert!(
            error.contains("--no-drafts") && error.contains("--drafter"),
            "{error}"
        );
    }
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
    assert!(!env.contains_key("TENSORFOLD_CUDA_MEMORY_LIMIT_GB"));
}

// T37: the resolved engine env joins the closed allowlist, and CapyCTL's own
// values are never replaced by it.
#[test]
fn the_resolved_engine_env_is_admitted_but_never_wins() {
    let mut input = plan();
    input.build_env = BTreeMap::from([
        ("MBX_FUSED_DRAFT".to_string(), "1".to_string()),
        ("CAPYCTL_ENGINE_LOG".to_string(), "/elsewhere".to_string()),
    ]);
    let rendered = render_command(&input).unwrap().env;
    let toolchain = input.build_env.clone();
    let env = engine_environment(&rendered, &input, &|_| None, &toolchain);
    assert_eq!(env["MBX_FUSED_DRAFT"], "1");
    assert_eq!(env["CAPYCTL_ENGINE_LOG"], "/var/lib/capyctl/logs/i.log");
}

// ADR 0028 §2.1: Debug output names the resolved env, never its values.
#[test]
fn the_plan_debug_output_hides_env_values() {
    let mut input = plan();
    input.build_env = BTreeMap::from([("HF_TOKEN".to_string(), "hf_secret".to_string())]);
    let shown = format!("{input:?}");
    assert!(
        shown.contains("HF_TOKEN") && !shown.contains("hf_secret"),
        "{shown}"
    );
}

// T41 (found live 2026-10-03: TensorFold 0.6.3 grew to 74 GiB under a 58 GiB
// declaration, sizing its caches from the machine's free memory): the
// deployment's declared allocation caps TensorFold's CUDA budget through
// `TENSORFOLD_CUDA_MEMORY_LIMIT_GB`, in GiB, and the closed environment
// keeps it.
#[test]
fn the_declared_allocation_caps_the_cuda_budget() {
    let mut input = plan();
    input.memory_limit_bytes = Some(58 << 30);
    let rendered = render_command(&input).unwrap().env;
    assert_eq!(rendered["TENSORFOLD_CUDA_MEMORY_LIMIT_GB"], "58");
    let env = engine_environment(&rendered, &input, &|_| None, &BTreeMap::new());
    assert_eq!(env["TENSORFOLD_CUDA_MEMORY_LIMIT_GB"], "58");
    // Rounded down to a whole MiB, written exactly.
    input.memory_limit_bytes = Some((58 << 30) + (512 << 20) + 1000);
    let rendered = render_command(&input).unwrap().env;
    assert_eq!(rendered["TENSORFOLD_CUDA_MEMORY_LIMIT_GB"], "58.5");
    input.memory_limit_bytes = None;
    let rendered = render_command(&input).unwrap().env;
    assert!(!rendered.contains_key("TENSORFOLD_CUDA_MEMORY_LIMIT_GB"));
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
    // The ready allocation on the GPU's memory is the engine's cap.
    assert_eq!(plan.memory_limit_bytes, Some(8 << 30));
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

// ADR 0023 §4 (amended 2026-10-03, owner decision): TensorFold decodes as many
// requests together as CapyCTL keeps in flight for the deployment, as vLLM
// and SGLang do: `--parallel` is the deployment's `max_concurrent_requests`,
// else CapyCTL's TensorFold default (8). The same option in the extra or
// host-fixed arguments wins, and CapyCTL renders none.
#[test]
fn the_plan_passes_the_deployments_concurrency_as_parallel() {
    let parallel = |argv: &[String]| -> Vec<String> {
        argv.windows(2)
            .filter(|w| w[0].starts_with("--par"))
            .map(|w| format!("{} {}", w[0], w[1]))
            .chain(
                argv.iter()
                    .filter(|a| a.starts_with("--parallel="))
                    .cloned(),
            )
            .collect()
    };
    let (deployment, host) = fixture();
    let effective = resolve_effective(&deployment, &host).unwrap();
    let plan = plan_from_effective(&effective, 8101, "/var/log/i.log".into(), None).unwrap();
    assert_eq!(
        plan.parallel,
        Some(capyctl_domain::launch::TENSORFOLD_DEFAULT_PARALLEL)
    );
    let argv = render_command(&plan).unwrap().argv;
    assert_eq!(parallel(&argv), ["--parallel 8"]);
    // Typed: right after `--context`.
    let at = argv.iter().position(|a| a == "--parallel").unwrap();
    assert_eq!(argv[at - 2], "--context");

    let (mut deployment, host) = fixture();
    deployment["engine_config"]["max_concurrent_requests"] = 4.into();
    let effective = resolve_effective(&deployment, &host).unwrap();
    let plan = plan_from_effective(&effective, 8101, "/var/log/i.log".into(), None).unwrap();
    assert_eq!(plan.parallel, Some(4));
    assert_eq!(
        parallel(&render_command(&plan).unwrap().argv),
        ["--parallel 4"]
    );

    for (extra, rendered) in [
        (json!(["--parallel", "8"]), "--parallel 8"),
        (json!(["--parallel=8"]), "--parallel=8"),
        (json!(["--par", "8"]), "--par 8"),
    ] {
        let (mut deployment, host) = fixture();
        deployment["engine_config"]["accept_extra_args"] = true.into();
        deployment["engine_config"]["extra_args"] = extra;
        let effective = resolve_effective(&deployment, &host).unwrap();
        let plan = plan_from_effective(&effective, 8101, "/var/log/i.log".into(), None).unwrap();
        assert_eq!(plan.parallel, None, "{rendered}");
        assert_eq!(parallel(&render_command(&plan).unwrap().argv), [rendered]);
    }

    let (deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["args"] = json!(["--parallel", "2"]);
    let effective = resolve_effective(&deployment, &host).unwrap();
    let plan = plan_from_effective(&effective, 8101, "/var/log/i.log".into(), None).unwrap();
    assert_eq!(plan.parallel, None);
    assert_eq!(
        parallel(&render_command(&plan).unwrap().argv),
        ["--parallel 2"]
    );
}

// The render half of the same rule: a rendered `--parallel` and the same
// option among the pass-through arguments never both reach TensorFold.
#[test]
fn a_rendered_parallel_beside_a_passed_one_is_refused() {
    let mut input = plan();
    input.parallel = Some(4);
    for args in [
        vec!["--parallel".to_owned(), "8".to_owned()],
        vec!["--parallel=8".to_owned()],
        vec!["--par".to_owned(), "8".to_owned()],
    ] {
        input.extra_args = args.clone();
        assert!(
            matches!(
                render_command(&input),
                Err(TensorfoldArgsError::Duplicate(_))
            ),
            "{args:?}"
        );
        input.extra_args.clear();
        input.engine_args = args.clone();
        assert!(
            matches!(
                render_command(&input),
                Err(TensorfoldArgsError::Duplicate(_))
            ),
            "{args:?}"
        );
        input.engine_args.clear();
    }
}

// ADR 0028 §10: TensorFold two-rank groups. CPU tests of the rendered command
// only; the live rows MN1–MN9 are the qualification.

fn has_pair(argv: &[String], flag: &str, value: &str) -> bool {
    argv.windows(2).any(|w| w[0] == flag && w[1] == value)
}

fn member(rank: u32) -> GroupMemberArgs {
    GroupMemberArgs {
        tensor_parallel: 2,
        pipeline_parallel: 1,
        nnodes: 2,
        node_rank: rank,
        head_address: "192.0.2.10".parse().unwrap(),
        rendezvous_port: 25000,
        own_address: format!("192.0.2.{}", 10 + rank).parse().unwrap(),
        worker_port: None,
        own_interface: None,
    }
}

fn tf_group(rank: u32) -> PlanInputTensorfold {
    PlanInputTensorfold {
        engine_bin: "/opt/tf/bin/tensorfold".into(),
        engine_path_extra: Some("/opt/tf/bin".into()),
        model_path: "/models/m".into(),
        served_model_name: "m".into(),
        port: 8100,
        context_length: 32768,
        parallel: Some(4),
        kv_dtype: Some("fp8".into()),
        memory_limit_bytes: Some(58 << 30),
        engine_log: Some("/var/lib/capyctl/logs/i.log".into()),
        group: Some(member(rank)),
        ..PlanInputTensorfold::default()
    }
}

fn tf_group_with_drafter(rank: u32) -> PlanInputTensorfold {
    let mut input = tf_group(rank);
    input.engine_args = vec!["--drafter".into(), "/srv/drafters/d".into()];
    input
}

// T14: both ranks render the two-rank flags right after the model path; only
// rank 0 serves.
#[test]
fn two_rank_flags() {
    let head = render_command(&tf_group(0)).unwrap();
    let follower = render_command(&tf_group(1)).unwrap();
    for (cmd, rank) in [(&head, "0"), (&follower, "1")] {
        assert_eq!(
            cmd.argv[..11],
            [
                "/opt/tf/bin/tensorfold",
                "serve",
                "/models/m",
                "--tp",
                "2",
                "--rank",
                rank,
                "--master",
                "192.0.2.10",
                "--master-port",
                "25000",
            ]
        );
    }
    for (f, v) in [("--name", "m"), ("--host", "127.0.0.1"), ("--port", "8100")] {
        assert!(has_pair(&head.argv, f, v), "{f} {v}");
    }
    for absent in ["--host", "--port", "--name"] {
        assert!(!follower.argv.iter().any(|a| a == absent), "{absent}");
    }
    assert_eq!(
        follower.argv[11..],
        [
            "--no-update-check",
            "--backend",
            "cuda",
            "--snapshot-dir",
            "none",
            "--context",
            "32768",
            "--parallel",
            "4",
            "--kv-dtype",
            "fp8",
            "--drafter",
            "none",
        ]
    );
}

// T14: both ranks carry equal context, `--parallel`, KV dtype, drafter and
// pass-through arguments (TensorFold requires agreement), and the same memory cap
// (ADR 0025, per rank on its own host).
#[test]
fn ranks_agree_on_context_parallel_drafter_and_cap() {
    let tail = |c: &RenderedCommand| {
        c.argv
            .iter()
            .skip_while(|a| *a != "--context")
            .cloned()
            .collect::<Vec<_>>()
    };
    for build in [tf_group, tf_group_with_drafter] {
        let head = render_command(&build(0)).unwrap();
        let follower = render_command(&build(1)).unwrap();
        assert_eq!(tail(&head), tail(&follower));
        assert_eq!(head.env, follower.env);
    }
    let drafted = render_command(&tf_group_with_drafter(1)).unwrap();
    assert!(has_pair(&drafted.argv, "--drafter", "/srv/drafters/d"));
    assert_eq!(drafted.env["TENSORFOLD_CUDA_MEMORY_LIMIT_GB"], "58");
}

// T03: any shape other than TP 2, PP 1 on two members is refused; the user
// still cannot pass `--tp`, `--rank`, `--master` or `--master-port`.
#[test]
fn other_shapes_and_user_flags_are_refused() {
    let shapes: [fn(&mut GroupMemberArgs); 7] = [
        |g| g.pipeline_parallel = 2,
        |g| g.tensor_parallel = 4,
        |g| g.tensor_parallel = 1,
        |g| g.nnodes = 3,
        |g| g.node_rank = 2,
        |g| g.rendezvous_port = 0,
        |g| g.worker_port = Some(8101),
    ];
    for (i, change) in shapes.iter().enumerate() {
        for rank in [0, 1] {
            let mut input = tf_group(rank);
            change(input.group.as_mut().unwrap());
            assert!(
                matches!(render_command(&input), Err(TensorfoldArgsError::GroupShape)),
                "shape {i} rank {rank}"
            );
        }
    }
    let mut head = tf_group(0);
    head.group.as_mut().unwrap().own_address = "192.0.2.11".parse().unwrap();
    assert!(matches!(
        render_command(&head),
        Err(TensorfoldArgsError::GroupShape)
    ));
    for flag in ["--tp", "--rank", "--master", "--master-port"] {
        for mut input in [tf_group(0), tf_group(1), plan()] {
            input.extra_args = vec![flag.into(), "1".into()];
            assert!(
                matches!(
                    render_command(&input),
                    Err(TensorfoldArgsError::Reserved(_))
                ),
                "{flag}"
            );
            input.extra_args.clear();
            input.engine_args = vec![flag.into(), "1".into()];
            assert!(
                matches!(
                    render_command(&input),
                    Err(TensorfoldArgsError::Reserved(_))
                ),
                "{flag}"
            );
        }
    }
}

// T21 T37: no transport, rendezvous or TF_COMM variable is rendered or inherited,
// even with an interface known (R11 applies to the torch-gloo engines only); the
// follower's final spawned environment holds no key material and equals the head's.
#[test]
fn no_transport_variables_and_no_credentials() {
    let inherited = |name: &str| match name {
        "HOME" => Some("/home/svc".to_string()),
        "CUDA_VISIBLE_DEVICES" => Some("0".to_string()),
        "NCCL_SOCKET_IFNAME"
        | "NCCL_IB_HCA"
        | "GLOO_SOCKET_IFNAME"
        | "MASTER_ADDR"
        | "MASTER_PORT"
        | "TF_COMM_BACKEND"
        | "TF_NCCL_LIB"
        | "VLLM_API_KEY"
        | "CAPYCTL_VLLM_ADMIN_KEY"
        | "VLLM_HOST_IP" => Some("bad".to_string()),
        _ => None,
    };
    let owned = |k: &String| {
        ["NCCL_", "GLOO_", "MASTER_", "TF_COMM", "TF_NCCL"]
            .iter()
            .any(|p| k.starts_with(p))
    };
    let mut spawned = Vec::new();
    for rank in [0, 1] {
        let mut input = tf_group(rank);
        input.group.as_mut().unwrap().own_interface = Some("enp1s0f0".into());
        let cmd = render_command(&input).unwrap();
        assert!(!cmd.env.keys().any(owned), "{:?}", cmd.env);
        let env = engine_environment(&cmd.env, &input, &inherited, &BTreeMap::new());
        assert!(!env.keys().any(owned), "{env:?}");
        for absent in ["VLLM_API_KEY", "CAPYCTL_VLLM_ADMIN_KEY", "VLLM_HOST_IP"] {
            assert!(!env.contains_key(absent), "{absent}");
        }
        assert!(!env.values().any(|v| v == "bad"), "{env:?}");
        assert!(!cmd
            .argv
            .iter()
            .any(|a| a.contains("key") || a.contains("enp1s0f0")));
        spawned.push(env);
    }
    assert_eq!(spawned[0], spawned[1]);
}

// T39: a single-rank launch renders exactly today's command; no two-rank flag.
#[test]
fn single_rank_argv_is_unchanged() {
    let mut input = plan();
    input.parallel = Some(4);
    input.kv_dtype = Some("fp8".into());
    assert_eq!(input.group, None);
    assert_eq!(
        render_command(&input).unwrap().argv,
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
            "--parallel",
            "4",
            "--kv-dtype",
            "fp8",
            "--drafter",
            "none",
        ]
    );
}

//! ADR 0028 §10: vLLM multi-node rendering for group members. CPU tests of the
//! rendered command only; the live rows MN1–MN9 are the qualification.

use capyctl_adapters::group::{member_args, GroupMemberArgs};
use capyctl_adapters::vllm::args::{render_command, ArgsError, PlanInputVllm};
use capyctl_domain::group::{
    member_id, GroupEngine, GroupPlan, GroupTopology, MemberKey, MemberPlan, MemberRole,
};

fn base_input() -> PlanInputVllm {
    PlanInputVllm {
        engine_bin: "/opt/venv/bin/vllm".into(),
        model_path: "/models/m".into(),
        port: 8100,
        served_model_name: "m".into(),
        tensor_parallel_size: 2,
        pipeline_parallel_size: 1,
        runtime_dir: Some("/opt/capyctl/runtime".into()),
        ..PlanInputVllm::default()
    }
}

fn args(rank: u32) -> GroupMemberArgs {
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

fn group_input(rank: u32) -> PlanInputVllm {
    PlanInputVllm {
        group: Some(args(rank)),
        ..base_input()
    }
}

fn single_rank_reference_input() -> PlanInputVllm {
    PlanInputVllm {
        tensor_parallel_size: 1,
        ..base_input()
    }
}

fn has_pair(argv: &[String], flag: &str, value: &str) -> bool {
    argv.windows(2).any(|w| w[0] == flag && w[1] == value)
}

// T14: the head renders every multi-node flag and keeps its loopback API.
#[test]
fn head_renders_multinode_flags() {
    let cmd = render_command(&group_input(0)).unwrap();
    for (f, v) in [
        ("--tensor-parallel-size", "2"),
        ("--pipeline-parallel-size", "1"),
        ("--nnodes", "2"),
        ("--node-rank", "0"),
        ("--master-addr", "192.0.2.10"),
        ("--master-port", "25000"),
        ("--distributed-executor-backend", "mp"),
        ("--host", "127.0.0.1"),
        ("--port", "8100"),
    ] {
        assert!(has_pair(&cmd.argv, f, v), "{f} {v}");
    }
    assert!(!cmd.argv.iter().any(|a| a == "--headless"));
    assert_eq!(
        cmd.env.get("VLLM_HOST_IP").map(String::as_str),
        Some("192.0.2.10")
    );
    assert_eq!(
        cmd.env.get("CAPYCTL_GROUP_MODE").map(String::as_str),
        Some("1")
    );
    let expected: serde_json::Value =
        serde_json::from_str(&cmd.env["CAPYCTL_GROUP_EXPECTED"]).unwrap();
    assert_eq!(
        expected,
        serde_json::json!({
            "nnodes": 2, "node_rank": 0, "master_addr": "192.0.2.10",
            "master_port": 25000, "headless": false,
            "distributed_executor_backend": "mp",
            "tensor_parallel_size": 2, "pipeline_parallel_size": 1,
        })
    );
}

// T14, T21: a worker is headless with no API listener, key or middleware.
#[test]
fn worker_is_headless() {
    let mut input = group_input(1);
    input.sleep_flags = vec!["--enable-sleep-mode".into()];
    let cmd = render_command(&input).unwrap();
    assert!(cmd.argv.iter().any(|a| a == "--headless"));
    assert!(has_pair(&cmd.argv, "--node-rank", "1"));
    for absent in [
        "--host",
        "--port",
        "--api-key",
        "--middleware",
        "--served-model-name",
    ] {
        assert!(!cmd.argv.iter().any(|a| a == absent), "{absent}");
    }
    assert_eq!(
        cmd.env.get("VLLM_HOST_IP").map(String::as_str),
        Some("192.0.2.11")
    );
}

// T20: deep park needs sleep mode on every rank.
#[test]
fn sleep_mode_on_every_rank_when_deep() {
    for rank in [0, 1] {
        let mut input = group_input(rank);
        input.sleep_flags = vec!["--enable-sleep-mode".into()];
        assert!(render_command(&input)
            .unwrap()
            .argv
            .iter()
            .any(|a| a == "--enable-sleep-mode"));
    }
}

// T21, T37, R11 (ADR 0028 §10): no NCCL or MASTER variable is rendered; the only
// GLOO variable is the interface holding the member's own peer address.
#[test]
fn only_the_gloo_interface_is_rendered() {
    for rank in [0, 1] {
        let cmd = render_command(&group_input(rank)).unwrap();
        assert!(!cmd
            .env
            .keys()
            .any(|k| k.starts_with("NCCL_") || k.starts_with("GLOO_") || k.starts_with("MASTER_")));
        assert!(!cmd.env["CAPYCTL_GROUP_EXPECTED"].contains("gloo_socket_ifname"));

        let mut input = group_input(rank);
        input.group.as_mut().unwrap().own_interface = Some("eth9".into());
        let cmd = render_command(&input).unwrap();
        assert!(!cmd
            .env
            .keys()
            .any(|k| k.starts_with("NCCL_") || k.starts_with("MASTER_")));
        let gloo: Vec<_> = cmd.env.keys().filter(|k| k.starts_with("GLOO_")).collect();
        assert_eq!(gloo, ["GLOO_SOCKET_IFNAME"]);
        assert_eq!(cmd.env["GLOO_SOCKET_IFNAME"], "eth9");
        let expected: serde_json::Value =
            serde_json::from_str(&cmd.env["CAPYCTL_GROUP_EXPECTED"]).unwrap();
        assert_eq!(expected["gloo_socket_ifname"], "eth9");
    }
}

// T14: user args still cannot state a multi-node flag.
#[test]
fn user_multinode_flags_stay_reserved() {
    let mut input = group_input(0);
    input.extra_args = vec!["--nnodes".into(), "4".into()];
    assert!(matches!(
        render_command(&input),
        Err(ArgsError::ReservedConflict(_))
    ));
}

// T39: a single-rank launch renders byte-identically to before.
#[test]
fn single_rank_rendering_is_unchanged() {
    let mut input = group_input(0);
    input.group = None;
    input.tensor_parallel_size = 1;
    let cmd = render_command(&input).unwrap();
    assert_eq!(cmd, render_command(&single_rank_reference_input()).unwrap());
    assert!(!cmd
        .argv
        .iter()
        .any(|a| a == "--nnodes" || a == "--headless"));
    assert!(!cmd.env.keys().any(|k| k.starts_with("CAPYCTL_GROUP")));
}

fn member(rank: u32, host: &str, peer: &str) -> MemberPlan {
    MemberPlan {
        member: MemberKey {
            host_id: host.into(),
            member_id: member_id(rank),
        },
        rank,
        role: if rank == 0 {
            MemberRole::Head
        } else {
            MemberRole::Worker
        },
        profile_name: "p".into(),
        profile_fingerprint: "pf".into(),
        checkpoint_fingerprint: "cf".into(),
        model_path: "/models/m".into(),
        devices: vec!["0".into()],
        peer_address: peer.parse().unwrap(),
        service_port: (rank == 0).then_some(8100),
        worker_port: None,
    }
}

// T14: each host's arguments come from the plan; a host outside it has none.
#[test]
fn member_args_follow_the_plan() {
    let plan = GroupPlan::new(
        GroupEngine::Vllm,
        vec![
            member(0, "host-a", "192.0.2.10"),
            member(1, "host-b", "192.0.2.11"),
        ],
        GroupTopology {
            tensor_parallel: 2,
            pipeline_parallel: 1,
            local_ranks: 1,
        },
        25000,
        1,
    )
    .unwrap();
    assert_eq!(member_args(&plan, "host-b"), Some(args(1)));
    let head = member_args(&plan, "host-a").unwrap();
    assert!(head.is_head());
    assert_eq!(head, args(0));
    assert!(member_args(&plan, "host-c").is_none());
}

// T14, T39: a group member takes TP and PP from the plan; nothing else changes.
#[test]
fn with_group_takes_parallelism_from_the_plan() {
    let single = single_rank_reference_input();
    let mut group = args(1);
    group.tensor_parallel = 4;
    group.pipeline_parallel = 2;
    let plan = capyctl_adapters::vllm::with_group(single.clone(), group.clone());
    assert_eq!(plan.tensor_parallel_size, 4);
    assert_eq!(plan.pipeline_parallel_size, 2);
    assert_eq!(plan.group, Some(group));
    assert_eq!(plan.model_path, single.model_path);
}

//! ADR 0028 §10: SGLang multi-node rendering for group members. CPU tests of
//! the public descriptor and the rendered command only; the live rows MN1–MN9
//! are the qualification.

use capyctl_adapters::sglang::{ProtectedDescriptorFds, SglangLaunch};
use capyctl_adapters::RuntimeError;
use capyctl_domain::group::GroupMemberArgs;
use capyctl_domain::launch::{NativeDeviceSelection, NativeLaunch, NativeLaunchMetadata};
use serde_json::{json, Value};

static WRAPPER: std::sync::LazyLock<std::path::PathBuf> = std::sync::LazyLock::new(|| {
    std::path::Path::new("/usr/bin/true")
        .canonicalize()
        .unwrap()
});

fn wrapper() -> &'static std::path::Path {
    &WRAPPER
}

fn frozen_sglang_single() -> NativeLaunch {
    NativeLaunch::from_frozen_store(
        NativeLaunchMetadata {
            engine: "sglang".into(),
            recipe: "sglang_engine_config_v2".into(),
            checkpoint_revision: "sha256:any-model".into(),
            served_name: "route-1".into(),
            binding_id: "01K00000000000000000000001".into(),
            incarnation: "01K00000000000000000000099".into(),
            endpoint: "http://127.0.0.1:20001".into(),
            rendered_settings_digest: "a".repeat(64),
            placement_digest: None,
            device: NativeDeviceSelection {
                host_id: "host-a".into(),
                hardware_fingerprint: "hardware-v1".into(),
                device_id: "gpu0".into(),
                memory_domain: "uma".into(),
                physical_gpu_uuid: None,
                cuda_pci_index: None,
            },
        },
        "/private/checkpoints/qwen".into(),
        "/opt/sglang/bin/python3".into(),
        "private://inference-reference".into(),
        "private://admin-reference".into(),
        capyctl_testkit::sglang_launch_settings(),
    )
}

fn member(rank: u32, interface: Option<&str>) -> GroupMemberArgs {
    GroupMemberArgs {
        tensor_parallel: 2,
        pipeline_parallel: 1,
        nnodes: 2,
        node_rank: rank,
        head_address: "192.0.2.10".parse().unwrap(),
        rendezvous_port: 25000,
        own_address: format!("192.0.2.{}", 10 + rank).parse().unwrap(),
        worker_port: (rank > 0).then_some(8101),
        own_interface: interface.map(str::to_owned),
    }
}

fn frozen_sglang_group(rank: u32) -> NativeLaunch {
    frozen_sglang_single().with_group(member(rank, None))
}

/// Today's single-rank descriptor, frozen as a literal (T39).
fn single_reference_descriptor() -> Value {
    json!({
        "schema_version": 2,
        "kind": "sglang_launch",
        "engine": "sglang",
        "binding_id": "01K00000000000000000000001",
        "incarnation": "01K00000000000000000000099",
        "endpoint": "http://127.0.0.1:20001",
        "served_name": "route-1",
        "rendered_settings_digest": "a".repeat(64),
        "device": {
            "host_id": "host-a",
            "hardware_fingerprint": "hardware-v1",
            "device_id": "gpu0",
            "memory_domain": "uma"
        },
        "settings": {
            "chunked_prefill_size": null,
            "context_length": null,
            "cpu_weight_backup": false,
            "cuda_graphs": false,
            "dtype": null,
            "extra_args": [],
            "kv_cache_dtype": null,
            "language_model_only": false,
            "max_running_requests": null,
            "max_total_tokens": null,
            "memory": {
                "kv_cache_bytes": 4294967296i64,
                "margin_bytes": 8589934592i64,
                "request_bytes": 17179869184i64,
                "static_bytes": 8589934592i64
            },
            "memory_saver": true,
            "quantization": null,
            "tokenizer_workers": 1,
            "trust_remote_code": false,
            "weight_restore": "disk_reload"
        },
    })
}

// T22: the public descriptor carries the group; a worker's endpoint is its loopback port.
#[test]
fn sglang_group_descriptor() {
    let head = SglangLaunch::from_frozen(&frozen_sglang_group(0)).unwrap();
    let g = &head.public_metadata()["settings"]["group"];
    assert_eq!(g["tp_size"], 2);
    assert_eq!(g["pp_size"], 1);
    assert_eq!(g["nnodes"], 2);
    assert_eq!(g["node_rank"], 0);
    assert_eq!(g["dist_init_addr"], "192.0.2.10:25000");
    assert_eq!(g["host_ip"], "192.0.2.10");
    assert!(g.get("gloo_socket_ifname").is_none());
    // ADR 0012: the head keeps its loopback API endpoint.
    assert_eq!(head.public_metadata()["endpoint"], "http://127.0.0.1:20001");
    let worker = SglangLaunch::from_frozen(&frozen_sglang_group(1)).unwrap();
    assert_eq!(
        worker.public_metadata()["endpoint"],
        "http://127.0.0.1:8101"
    );
    assert_eq!(
        worker.public_metadata()["settings"]["group"]["host_ip"],
        "192.0.2.11"
    );
    assert_eq!(
        worker.public_metadata()["settings"]["group"]["node_rank"],
        1
    );
}

// T22, R11: the descriptor names the member's interface only when one was resolved.
#[test]
fn sglang_group_descriptor_names_the_interface() {
    let launch = frozen_sglang_single().with_group(member(1, Some("eth9")));
    let rendered = SglangLaunch::from_frozen(&launch).unwrap();
    assert_eq!(
        rendered.public_metadata()["settings"]["group"]["gloo_socket_ifname"],
        "eth9"
    );
}

// T22: an IPv6 head address is bracketed in the rendezvous address.
#[test]
fn sglang_group_descriptor_brackets_ipv6() {
    let mut args = member(0, None);
    args.head_address = "2001:db8::10".parse().unwrap();
    args.own_address = args.head_address;
    let rendered = SglangLaunch::from_frozen(&frozen_sglang_single().with_group(args)).unwrap();
    assert_eq!(
        rendered.public_metadata()["settings"]["group"]["dist_init_addr"],
        "[2001:db8::10]:25000"
    );
}

// T22: a group shape the entry would refuse is refused at render.
#[test]
fn sglang_group_shapes_are_checked() {
    let cases: Vec<fn(&mut GroupMemberArgs)> = vec![
        |a| a.nnodes = 1,
        |a| a.node_rank = 2,
        |a| a.tensor_parallel = 0,
        |a| a.pipeline_parallel = 0,
        |a| a.tensor_parallel = 3,
        |a| a.rendezvous_port = 0,
        |a| a.worker_port = None,
        |a| a.worker_port = Some(0),
        |a| a.own_interface = Some(String::new()),
        |a| a.own_interface = Some("eth0/1".into()),
        |a| a.own_interface = Some("a".repeat(16)),
    ];
    for mutate in cases {
        let mut args = member(1, None);
        mutate(&mut args);
        assert_eq!(
            SglangLaunch::from_frozen(&frozen_sglang_single().with_group(args.clone()))
                .unwrap_err(),
            RuntimeError::Unsupported,
            "{args:?}"
        );
    }
    // The head has no worker port and is its own rendezvous address.
    let mut head = member(0, None);
    head.worker_port = Some(8101);
    assert!(SglangLaunch::from_frozen(&frozen_sglang_single().with_group(head)).is_err());
    let mut head = member(0, None);
    head.own_address = "192.0.2.11".parse().unwrap();
    assert!(SglangLaunch::from_frozen(&frozen_sglang_single().with_group(head)).is_err());
}

// T39: a single-rank descriptor has no group and is byte-identical.
#[test]
fn sglang_single_rank_descriptor_unchanged() {
    let single = SglangLaunch::from_frozen(&frozen_sglang_single()).unwrap();
    assert!(single.public_metadata()["settings"].get("group").is_none());
    assert_eq!(single.public_metadata(), &single_reference_descriptor());
}

// T21, T37 (ADR 0012): a worker's command names no credential descriptor;
// the head and a single rank keep all three.
#[test]
fn only_the_worker_command_carries_no_credentials() {
    let worker = SglangLaunch::from_frozen(&frozen_sglang_group(1)).unwrap();
    let command = worker
        .render_for_launcher(ProtectedDescriptorFds::launch_only(3).unwrap(), wrapper())
        .unwrap();
    assert_eq!(&command.argv[5..], ["--launch-descriptor-fd", "3"]);
    assert!(command.argv.iter().all(|a| !a.contains("credential")));
    // A worker is never handed credential descriptors, and a credentialed
    // launch is never rendered without them.
    assert!(worker
        .render_for_launcher(
            ProtectedDescriptorFds::for_launcher(3, 4, 5).unwrap(),
            wrapper()
        )
        .is_err());
    for launch in [frozen_sglang_single(), frozen_sglang_group(0)] {
        let rendered = SglangLaunch::from_frozen(&launch).unwrap();
        assert!(rendered
            .render_for_launcher(ProtectedDescriptorFds::launch_only(3).unwrap(), wrapper())
            .is_err());
        let command = rendered
            .render_for_launcher(
                ProtectedDescriptorFds::for_launcher(3, 4, 5).unwrap(),
                wrapper(),
            )
            .unwrap();
        assert_eq!(
            &command.argv[5..],
            [
                "--launch-descriptor-fd",
                "3",
                "--inference-credential-fd",
                "4",
                "--admin-credential-fd",
                "5"
            ]
        );
    }
    assert!(ProtectedDescriptorFds::launch_only(2).is_err());
}

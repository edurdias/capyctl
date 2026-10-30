//! ADR 0035: one model on several identical discrete GPUs of one host, and
//! `topology: auto`. CPU tests only; none of this qualifies a multi-GPU
//! launch (live rows MG1-MG14 do).

use capyctl_config::effective::{
    choose_tensor_parallel, decode_effective_snapshot, fit_hint, resolve_effective_with_checkpoint,
    AutoCandidate, AutoChoice, AutoGpu, AutoTable, CheckpointFacts, CudaNamespace,
    EffectiveDeployment, PhaseCharges, ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES as OVERHEAD,
    ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES, LOCAL_TP_COMMUNICATION_PLACEHOLDER_BYTES as COMM,
    LOCAL_TP_HOST_OVERHEAD_PER_RANK_BYTES as PER_RANK,
};
use capyctl_config::instances::{device_choices, parse_instance_spec};
use capyctl_config::topology::{refuse_in_standalone, LocalTopology, Topology};
use capyctl_config::{parse_strict, ConfigError, ConfigKind};
use capyctl_domain::member_weights::{member_weights_bytes, CheckpointLayout};
use serde_json::{json, Value};

const GIB: i64 = 1 << 30;
/// A 40 GiB checkpoint whose decoder layers hold 36 GiB.
const WEIGHTS: i64 = 40 * GIB;
const LAYOUT: CheckpointLayout = CheckpointLayout {
    sharded_bytes: 36 * GIB,
    layer_count: 40,
    largest_layer_bytes: 900 << 20,
};
const A6000: &str = "NVIDIA RTX A6000";

fn fixture() -> (Value, Value) {
    let all: Value = serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    (all["deployment"].clone(), all["host"].clone())
}

/// A discrete host with `gpus` identical 48 GB cards (40 GiB managed each)
/// and 200 GiB of managed host RAM, publishing UUIDs and product names.
fn host(gpus: u32) -> Value {
    let (_, mut host) = fixture();
    let mut domains = json!({
        "system": {"memory": "distinct", "managed_limit": "200GiB", "free_reserve": "16GiB",
                   "parked_limit": "100GiB", "host_kv_limit": "4GiB"}
    });
    let mut devices = json!({});
    for n in 0..gpus {
        let id = format!("gpu{n}");
        domains[&id] = json!({"memory": "device", "device": id, "managed_limit": "40GiB",
            "free_reserve": "8GiB", "parked_limit": "4GiB"});
        devices[&id] = json!({"domain": id, "sharing": "shared", "product_name": A6000,
            "physical_gpu_uuid": format!("GPU-0000000{n}-0000-0000-0000-000000000000")});
    }
    host["resource_policy"]["domains"] = domains;
    host["resource_policy"]["devices"] = devices;
    let profile = &mut host["runtime_profiles"]["local"];
    profile["build_fingerprint"] = "0.32.0".into();
    profile["args"] = json!([]);
    host
}

/// The fixture deployment with derived phases and no devices, residency or
/// resources: the host and the topology decide.
fn deployment(topology: Option<Value>) -> Value {
    let (mut d, _) = fixture();
    let object = d.as_object_mut().unwrap();
    for field in ["resources", "residency", "devices"] {
        object.remove(field);
    }
    if let Some(topology) = topology {
        object.insert("topology".into(), topology);
    }
    d
}

fn tp(n: u32) -> Option<Value> {
    Some(json!({"tensor_parallel": n}))
}

fn measured() -> CheckpointFacts {
    CheckpointFacts {
        weights_bytes: Some(WEIGHTS),
        layout: Some(LAYOUT),
        ..CheckpointFacts::default()
    }
}

fn resolve(d: &Value, h: &Value) -> Result<EffectiveDeployment, ConfigError> {
    resolve_effective_with_checkpoint(d, h, measured())
}

fn refusal(d: &Value, h: &Value) -> String {
    resolve(d, h).unwrap_err().to_string()
}

fn phase_domains(e: &EffectiveDeployment) -> Vec<Vec<(String, i64)>> {
    let r = &e.resources;
    [&r.cold, &r.ready, &r.parking, &r.parked, &r.wake]
        .iter()
        .map(|p| {
            p.allocations
                .iter()
                .map(|a| (a.domain.clone(), a.bytes))
                .collect()
        })
        .collect()
}

fn round_trips(e: &EffectiveDeployment) {
    let text = serde_json::to_string(e).unwrap();
    assert_eq!(&decode_effective_snapshot(&text).unwrap(), e);
}

fn spec_local(extra: Value) -> Result<Option<LocalTopology>, ConfigError> {
    let mut d = json!({"schema_version": 1, "kind": "deployment", "name": "m"});
    d.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    parse_instance_spec(&d).map(|spec| spec.local)
}

// T46: `topology.tensor_parallel` without two or more `placement.hosts` is one
// instance on N GPUs of one host, beside `host`, a selector, a strategy,
// `max_per_host` and several instances; two or more hosts stay a group.
#[test]
fn a_topology_without_two_hosts_is_one_host() {
    let fixed = Some(LocalTopology::Fixed(Topology {
        tensor_parallel: 2,
        pipeline_parallel: 1,
    }));
    for extra in [
        json!({"topology": {"tensor_parallel": 2}}),
        json!({"topology": {"tensor_parallel": 2}, "host": "a"}),
        json!({"topology": {"tensor_parallel": 2}, "placement": {"hosts": ["a"]}}),
        json!({"topology": {"tensor_parallel": 2}, "instances": 3,
            "placement": {"selector": {"zone": "x"}, "strategy": "pack", "max_per_host": 2}}),
    ] {
        assert_eq!(spec_local(extra.clone()).unwrap(), fixed, "{extra}");
    }
    let group = parse_instance_spec(&json!({"schema_version": 1, "kind": "deployment",
        "name": "g", "topology": {"tensor_parallel": 2}, "placement": {"hosts": ["a", "b"]}}))
    .unwrap();
    assert!(group.group.is_some() && group.local.is_none());
    // A world size of one is neither, and keeps its command identity.
    assert_eq!(
        spec_local(json!({"topology": {"tensor_parallel": 1}})).unwrap(),
        None
    );
    assert_eq!(
        spec_local(json!({"topology": "auto"})).unwrap(),
        Some(LocalTopology::Auto)
    );
}

// T46: `topology: auto` is refused beside two or more hosts, beside
// `devices`, and beside a declared per-GPU charge; `devices` pins exactly N
// distinct GPUs.
#[test]
fn declaration_refusals_carry_their_codes() {
    for (extra, code) in [
        (
            json!({"topology": "auto", "placement": {"hosts": ["a", "b"]}}),
            "group_topology_invalid",
        ),
        (
            json!({"topology": "auto", "devices": [{"id": "gpu0"}]}),
            "topology_devices_mismatch",
        ),
        (
            json!({"topology": {"tensor_parallel": 2}, "devices": [{"id": "gpu0"}]}),
            "topology_devices_mismatch",
        ),
        (
            json!({"topology": {"tensor_parallel": 2},
                "devices": [{"id": "gpu0"}, {"id": "gpu0"}]}),
            "topology_devices_mismatch",
        ),
        (
            json!({"topology": {"tensor_parallel": 2},
                "devices": [{"id": "gpu0"}, {"id": "gpu1"}, {"id": "gpu2"}]}),
            "topology_devices_mismatch",
        ),
        (json!({"topology": "sideways"}), "group_topology_invalid"),
    ] {
        let error = spec_local(extra.clone()).unwrap_err().to_string();
        assert!(error.contains(code), "{extra}: {error}");
    }
    for (extra, path) in [
        (
            json!({"topology": "auto", "resources": {"gpu": "10GiB", "ram": "2GiB"}}),
            "resources",
        ),
        (
            json!({"topology": "auto", "engine_config": {"memory": {"request": "10GiB"}}}),
            "engine_config.memory.request",
        ),
        (
            json!({"topology": "auto", "engine_config": {"memory": {"startup": "10GiB"}}}),
            "engine_config.memory.startup",
        ),
    ] {
        let error = spec_local(extra.clone()).unwrap_err();
        assert_eq!(error.path, path, "{extra}");
        assert!(!error.detail.contains("topology_devices_mismatch"));
    }
    // A declared KV cache is per GPU and stays allowed.
    spec_local(json!({"topology": "auto", "engine_config": {"memory": {"kv_cache": "4GiB"}}}))
        .unwrap();
}

// T46: the strict schema takes `topology: auto` and no other word; standalone
// accepts a topology on one host and `auto`, and still refuses a group.
#[test]
fn strict_schema_and_standalone_accept_one_host() {
    let text = |topology: &str| format!("name: m\nengine: vllm\nmodel: m\ntopology: {topology}\n");
    parse_strict(ConfigKind::Deployment, &text("auto")).unwrap();
    parse_strict(ConfigKind::Deployment, &text("{tensor_parallel: 2}")).unwrap();
    assert!(parse_strict(ConfigKind::Deployment, &text("all")).is_err());
    assert!(parse_strict(ConfigKind::Deployment, &text("{gpus: 2}")).is_err());
    for accepted in [
        json!({"topology": {"tensor_parallel": 2}}),
        json!({"topology": "auto"}),
        json!({"topology": {"tensor_parallel": 2}, "placement": {"hosts": ["a"]}}),
    ] {
        refuse_in_standalone(&accepted).unwrap();
    }
    let group = json!({"topology": {"tensor_parallel": 2}, "placement": {"hosts": ["a", "b"]}});
    assert!(refuse_in_standalone(&group)
        .unwrap_err()
        .to_string()
        .contains("group_placement_required"));
    let auto = json!({"topology": "auto", "placement": {"hosts": ["a", "b"]}});
    assert!(refuse_in_standalone(&auto)
        .unwrap_err()
        .to_string()
        .contains("group_topology_invalid"));
}

// T46: a world size of one keeps every revision byte-identical.
#[test]
fn a_world_size_of_one_is_byte_identical() {
    let h = host(4);
    let small = CheckpointFacts {
        weights_bytes: Some(8 * GIB),
        ..CheckpointFacts::default()
    };
    let plain = resolve_effective_with_checkpoint(&deployment(None), &h, small).unwrap();
    let one = resolve_effective_with_checkpoint(&deployment(tp(1)), &h, small).unwrap();
    assert_eq!(
        serde_json::to_string(&plain).unwrap(),
        serde_json::to_string(&one).unwrap()
    );
    let encoded = serde_json::to_value(&plain).unwrap();
    assert!(encoded.get("topology").is_none());
    assert_eq!(plain.local_tensor_parallel(), 1);
    assert_eq!(plain.topology, None);
    // One GPU, the lowest, as before.
    assert_eq!(plain.selected_devices.len(), 1);
    assert_eq!(plain.selected_devices[0].id, "gpu0");
    assert_eq!(
        plain.cuda_namespaces().unwrap(),
        plain.cuda_namespace().unwrap().map(|n| vec![n])
    );
}

// T46: N GPUs are charged one rank each, with the weight share of ADR 0028's
// formula at T = N, the CUDA context, and the communication allowance; the
// host's RAM holds 4 GiB plus 3 GiB per rank after the first.
#[test]
fn each_gpu_is_charged_one_rank_and_the_host_the_ranks_overhead() {
    let h = host(4);
    for n in [2_u32, 4] {
        let mut d = deployment(tp(n));
        d["residency"] = "restart_only".into();
        let e = resolve(&d, &h).unwrap();
        assert_eq!(e.local_tensor_parallel(), n);
        let ids: Vec<&str> = e.selected_devices.iter().map(|c| c.id.as_str()).collect();
        let expected: Vec<String> = (0..n).map(|i| format!("gpu{i}")).collect();
        assert_eq!(ids, expected, "rank order is ascending index");
        let memory = e.engine_config.memory();
        let share = member_weights_bytes(WEIGHTS, Some(&LAYOUT), n, 1).unwrap();
        assert_eq!(memory.weights_bytes, Some(share));
        let member = memory.member.unwrap();
        assert_eq!((member.tensor_parallel, member.pipeline_parallel), (n, 1));
        assert_eq!(member.checkpoint_weights_bytes, Some(WEIGHTS));
        let per_gpu = memory.request_bytes + OVERHEAD + COMM;
        let cold = memory.startup_bytes.unwrap().max(memory.request_bytes) + OVERHEAD + COMM;
        let system = ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES + PER_RANK * i64::from(n - 1);
        let gpus = |bytes: i64| -> Vec<(String, i64)> {
            expected
                .iter()
                .map(|id| (id.clone(), bytes))
                .chain([("system".to_owned(), system)])
                .collect()
        };
        let phases = phase_domains(&e);
        assert_eq!(phases[0], gpus(cold), "cold at {n}");
        assert_eq!(phases[1], gpus(per_gpu), "ready at {n}");
        let restart: Vec<(String, i64)> = expected
            .iter()
            .map(|id| (id.clone(), 0))
            .chain([("system".to_owned(), 0)])
            .collect();
        assert_eq!(phases[3], restart);
        let record = e.topology.as_ref().unwrap();
        assert_eq!(record.tensor_parallel, n);
        assert_eq!(record.communication_bytes, Some(COMM));
        assert!(record.auto.is_none());
        let charges = e.per_gpu_charges();
        assert_eq!(charges.device.ready, per_gpu);
        assert_eq!(charges.system.ready, system);
        assert_eq!(e.ready_device_allocations().len(), n as usize);
        round_trips(&e);
    }
}

// T46: the engine child sees every GPU in rank order, by UUID, else by PCI
// index when any UUID is missing; one GPU keeps today's namespace.
#[test]
fn the_namespace_lists_every_gpu_in_rank_order() {
    let mut h = host(2);
    let e = resolve(&deployment(tp(2)), &h).unwrap();
    let list = e.cuda_namespaces().unwrap().unwrap();
    assert_eq!(
        CudaNamespace::environment_of(&list),
        vec![(
            "CUDA_VISIBLE_DEVICES",
            "GPU-00000000-0000-0000-0000-000000000000,GPU-00000001-0000-0000-0000-000000000000"
                .to_owned()
        )]
    );
    h["resource_policy"]["devices"]["gpu1"]
        .as_object_mut()
        .unwrap()
        .remove("physical_gpu_uuid");
    let e = resolve(&deployment(tp(2)), &h).unwrap();
    let list = e.cuda_namespaces().unwrap().unwrap();
    assert_eq!(
        CudaNamespace::environment_of(&list),
        vec![
            ("CUDA_DEVICE_ORDER", "PCI_BUS_ID".to_owned()),
            ("CUDA_VISIBLE_DEVICES", "0,1".to_owned())
        ]
    );
    // Each rank sizes from one card: the smallest observed total.
    let sized = e
        .with_device_total(|index| Some(if index == 0 { 48 * GIB } else { 47 * GIB }))
        .unwrap();
    assert_eq!(
        sized.engine_config.memory().device_total_bytes,
        Some(47 * GIB)
    );
}

// T46: `devices` pins the GPUs, with each pin's sharing; pinned GPUs of
// different products or memory are `multi_gpu_mixed_devices`.
#[test]
fn pinned_gpus_resolve_and_must_be_identical() {
    let mut h = host(4);
    let mut d = deployment(tp(2));
    d["devices"] = json!([{"id": "gpu3"}, {"id": "gpu1"}]);
    let e = resolve(&d, &h).unwrap();
    let ids: Vec<&str> = e.selected_devices.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["gpu3", "gpu1"], "a pin keeps its rank order");
    h["resource_policy"]["devices"]["gpu3"]["product_name"] = "NVIDIA RTX 3090".into();
    let error = refusal(&d, &h);
    assert!(error.contains("multi_gpu_mixed_devices"), "{error}");
    let mut h = host(4);
    h["resource_policy"]["domains"]["gpu1"]["managed_limit"] = "20GiB".into();
    assert!(refusal(&d, &h).contains("multi_gpu_mixed_devices"));
    // Within 64 MiB is the same card.
    let mut h = host(4);
    h["resource_policy"]["domains"]["gpu1"]["free_reserve"] = "8224MiB".into();
    resolve(&d, &h).unwrap();
}

// T46: what is refused, with its code: pipeline parallel on one host, a
// unified host, engines without a local multi-GPU mode, two devices without
// a topology, too few GPUs, and a parking tier the gate keeps closed.
#[test]
fn refusals_carry_their_codes() {
    let h = host(4);
    let error = refusal(
        &deployment(Some(json!({"tensor_parallel": 2, "pipeline_parallel": 2}))),
        &h,
    );
    assert!(error.contains("group_shape_unsupported:vllm"), "{error}");
    // A unified host has one GPU domain.
    let (_, unified) = fixture();
    let error = refusal(&deployment(tp(2)), &unified);
    assert!(error.contains("multi_gpu_unsupported"), "{error}");
    // TensorFold and custom engines have no local multi-GPU mode.
    for engine in ["tensorfold", "custom"] {
        let mut other = h.clone();
        other["runtime_profiles"]["local"]["engine"] = engine.into();
        let error = refusal(&deployment(tp(2)), &other);
        assert!(error.contains("multi_gpu_unsupported"), "{engine}: {error}");
    }
    // Two devices without a topology: the hint names the size.
    let mut two = deployment(None);
    two["devices"] = json!([{"id": "gpu0"}, {"id": "gpu1"}]);
    let error = refusal(&two, &h);
    assert!(error.contains("multi_gpu_unsupported"), "{error}");
    assert!(error.contains("topology: {tensor_parallel: 2}"), "{error}");
    // More GPUs than the host has, with what would fit named.
    let error = refusal(&deployment(tp(8)), &h);
    assert!(error.contains("insufficient_gpus"), "{error}");
    assert!(error.contains("what would fit"), "{error}");
    // LMCache stays off on several GPUs, so `required` is refused.
    let mut required = deployment(tp(2));
    required["lmcache"] = "required".into();
    let error = refusal(&required, &h);
    assert!(
        error.contains("lmcache_unavailable:multi_gpu_unqualified"),
        "{error}"
    );
    // A decisions deployment keeps its own refusal of any topology.
    let mut decisions = deployment(Some(json!("auto")));
    decisions["api"] = "decisions".into();
    let error = refusal(&decisions, &h);
    assert!(error.contains("topology"), "{error}");
    assert!(!error.contains("insufficient"), "{error}");
}

/// A host whose model store holds a checkpoint with `heads` attention heads.
fn with_heads(heads: u64) -> (tempfile::TempDir, Value, Value) {
    let store = tempfile::tempdir().unwrap();
    let model = store.path().join("toy");
    std::fs::create_dir(&model).unwrap();
    std::fs::write(
        model.join("config.json"),
        json!({"num_attention_heads": heads, "model_type": "qwen3"}).to_string(),
    )
    .unwrap();
    let mut h = host(4);
    h["model_store"]["path"] = store.path().display().to_string().into();
    let mut d = deployment(None);
    d["model"]["path"] = model.display().to_string().into();
    (store, d, h)
}

// T46: vLLM and SGLang need N to divide the attention heads; llama.cpp's
// layer split does not; an unread checkpoint is not refused.
#[test]
fn tensor_parallel_must_divide_the_attention_heads() {
    let (_store, mut d, h) = with_heads(6);
    d["topology"] = json!({"tensor_parallel": 4});
    let error = refusal(&d, &h);
    assert!(error.contains("tensor_parallel_indivisible"), "{error}");
    assert!(error.contains("6 attention heads"), "{error}");
    assert!(error.contains("1, 2, 3, 6"), "{error}");
    d["topology"] = json!({"tensor_parallel": 2});
    resolve(&d, &h).unwrap();
    let mut sglang = h.clone();
    let profile = &mut sglang["runtime_profiles"]["local"];
    profile["engine"] = "sglang".into();
    profile["security"]["admin_credential_ref"] = "secret://admin-key".into();
    d["topology"] = json!({"tensor_parallel": 4});
    assert!(refusal(&d, &sglang).contains("tensor_parallel_indivisible"));
    // No config.json here: nothing to refuse.
    resolve(&deployment(tp(4)), &host(4)).unwrap();
}

// T46: the short form charges `gpu` on each GPU and `ram` once; a declared
// `memory.request` is per GPU.
#[test]
fn declared_figures_are_per_gpu() {
    let h = host(2);
    let mut short = deployment(tp(2));
    short["resources"] = json!({"gpu": "30GiB", "ram": "8GiB"});
    let e = resolve(&short, &h).unwrap();
    let active = vec![
        ("gpu0".to_owned(), 30 * GIB),
        ("gpu1".to_owned(), 30 * GIB),
        ("system".to_owned(), 8 * GIB),
    ];
    let phases = phase_domains(&e);
    assert_eq!(phases[1], active);
    assert_eq!(phases[0], active);
    assert_eq!(e.topology.as_ref().unwrap().communication_bytes, None);
    let mut request = deployment(tp(2));
    request["engine_config"]["memory"] = json!({"request": "30GiB", "kv_cache": "4GiB"});
    let e = resolve(&request, &h).unwrap();
    assert_eq!(phase_domains(&e)[1][0].1, 30 * GIB + OVERHEAD + COMM);
    assert_eq!(phase_domains(&e)[1][1].1, 30 * GIB + OVERHEAD + COMM);
}

// T46: a refusal for device memory names the smallest size that fits.
#[test]
fn insufficient_device_memory_names_what_would_fit() {
    let h = host(4);
    let error = refusal(&deployment(None), &h);
    assert!(error.contains("insufficient_device_memory"), "{error}");
    let auto = resolve(&deployment(Some(json!("auto"))), &h).unwrap();
    let n = auto.local_tensor_parallel();
    assert!(n > 1);
    assert!(
        error.contains(&format!("`topology: {{tensor_parallel: {n}}}`")),
        "{error}"
    );
    // On a host with too few cards, no size fits: the charge is named.
    let error = refusal(&deployment(None), &host(1));
    assert!(error.contains("no tensor_parallel up to"), "{error}");
}

fn table_peaks(table: &AutoTable) -> Vec<(u32, i64)> {
    table
        .candidates
        .iter()
        .map(|c| (c.tensor_parallel, c.device.peak()))
        .collect()
}

// T46: `topology: auto` records the candidate table and resolves to the
// smallest N whose per-GPU charge every card's managed limit holds; the
// revision is the explicit N's, and round-trips.
#[test]
fn auto_records_its_table_and_takes_the_smallest_fitting_size() {
    let h = host(4);
    let e = resolve(&deployment(Some(json!("auto"))), &h).unwrap();
    let record = e.topology.as_ref().unwrap();
    let table = record.auto.as_ref().unwrap();
    assert!(table.weights_measured);
    let peaks = table_peaks(table);
    assert_eq!(
        peaks.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
    let chosen = peaks
        .iter()
        .find(|(_, peak)| *peak <= 40 * GIB)
        .map(|(n, _)| *n)
        .expect("a size fits four 40 GiB cards");
    assert_eq!(record.tensor_parallel, chosen);
    assert_eq!(e.local_tensor_parallel(), chosen);
    // The same launch as the explicit size.
    let explicit = resolve(&deployment(tp(chosen)), &h).unwrap();
    assert_eq!(e.recipe_fingerprint, explicit.recipe_fingerprint);
    assert_eq!(e.engine_config, explicit.engine_config);
    assert_eq!(e.resources, explicit.resources);
    round_trips(&e);
    // The rows are each size's derived phases.
    for (n, _) in &peaks {
        if *n == 1 || *n == chosen {
            continue;
        }
        if let Ok(other) = resolve(&deployment(tp(*n)), &h) {
            assert_eq!(
                table.candidates[(*n - 1) as usize].device.ready,
                other.per_gpu_charges().device.ready
            );
        }
    }
}

// T46 / MG13: the N=1 auto choice still needs measured layout facts to
// validate the larger candidates when its frozen revision is opened.
#[test]
fn auto_single_gpu_preserves_layout_for_snapshot_validation() {
    let facts = CheckpointFacts {
        weights_bytes: Some(8 * GIB),
        layout: Some(CheckpointLayout {
            sharded_bytes: 7 * GIB,
            layer_count: 32,
            largest_layer_bytes: 224 << 20,
        }),
        ..CheckpointFacts::default()
    };
    let mut d = deployment(Some(json!("auto")));
    d["residency"] = json!("host_backed");
    let e = resolve_effective_with_checkpoint(&d, &host(2), facts).unwrap();
    assert_eq!(e.local_tensor_parallel(), 1);
    round_trips(&e);
}

// T46: required LMCache still narrows auto to one GPU after parking qualifies.
#[test]
fn auto_narrows_to_one_gpu_for_required_lmcache() {
    let mut h = host(4);
    h["runtime_profiles"]["local"]["lmcache"] = json!({"version": "0.5.6",
        "digest": "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"});
    let small = CheckpointFacts {
        weights_bytes: Some(8 * GIB),
        ..CheckpointFacts::default()
    };
    let mut d = deployment(Some(json!("auto")));
    d["lmcache"] = "required".into();
    d["residency"] = "restart_only".into();
    let e = resolve_effective_with_checkpoint(&d, &h, small).unwrap();
    let table = e.topology.as_ref().unwrap().auto.as_ref().unwrap();
    assert_eq!(table_peaks(table).len(), 1);
    assert!(table
        .excluded
        .iter()
        .all(|x| x.reason == "lmcache_unavailable:multi_gpu_unqualified"));
    assert_eq!(e.local_tensor_parallel(), 1);
}

// T46: before the weights are measured no N is chosen; when no size fits,
// `auto` is refused naming the per-GPU charge at the largest size.
#[test]
fn auto_waits_for_the_weights_and_names_what_would_fit() {
    let h = host(2);
    let provisional = CheckpointFacts::provisional();
    let e = resolve_effective_with_checkpoint(&deployment(Some(json!("auto"))), &h, provisional)
        .unwrap();
    let table = e.topology.as_ref().unwrap().auto.as_ref().unwrap();
    assert!(!table.weights_measured);
    assert_eq!(e.local_tensor_parallel(), 1);
    let huge = CheckpointFacts {
        weights_bytes: Some(400 * GIB),
        ..CheckpointFacts::default()
    };
    let error = resolve_effective_with_checkpoint(&deployment(Some(json!("auto"))), &h, huge)
        .unwrap_err()
        .to_string();
    assert!(error.contains("insufficient_device_memory"), "{error}");
    assert!(
        error.contains("at tensor_parallel: 2 each GPU is charged"),
        "{error}"
    );
}

fn charges(bytes: i64) -> PhaseCharges {
    PhaseCharges {
        cold: bytes,
        ready: bytes,
        parking: bytes,
        parked: 0,
        wake: bytes,
    }
}

fn gpu(id: &str, managed: i64, product: &str) -> AutoGpu {
    AutoGpu {
        id: id.into(),
        managed_limit: managed,
        product: Some(product.into()),
        total: managed + 8 * GIB,
    }
}

fn table(rows: &[(u32, i64)]) -> AutoTable {
    AutoTable {
        weights_measured: true,
        layout: None,
        candidates: rows
            .iter()
            .map(|(n, bytes)| AutoCandidate {
                tensor_parallel: *n,
                device: charges(*bytes),
                system: charges(GIB),
            })
            .collect(),
        excluded: Vec::new(),
    }
}

// T46: the choice of N reads the cards' managed limits, never their free
// memory: the same table and cards give the same N, a measured peak at N
// replaces the placeholder, and only identical cards form a set.
#[test]
fn the_choice_reads_limits_and_identical_cards() {
    let rows = table(&[(1, 50 * GIB), (2, 30 * GIB), (3, 20 * GIB)]);
    let cards = [
        gpu("gpu0", 40 * GIB, A6000),
        gpu("gpu1", 40 * GIB, A6000),
        gpu("gpu2", 40 * GIB, A6000),
    ];
    let chosen = choose_tensor_parallel(&rows, &cards, &|_| None);
    assert_eq!(
        chosen,
        AutoChoice::Chosen {
            tensor_parallel: 2,
            sets: vec![
                vec!["gpu0".to_owned(), "gpu1".to_owned()],
                vec!["gpu0".to_owned(), "gpu2".to_owned()],
                vec!["gpu1".to_owned(), "gpu2".to_owned()],
            ],
        }
    );
    // A measured peak at 2 above the cards moves the choice to 3.
    let measured = choose_tensor_parallel(&rows, &cards, &|n| (n == 2).then_some(45 * GIB));
    assert!(matches!(
        measured,
        AutoChoice::Chosen {
            tensor_parallel: 3,
            ..
        }
    ));
    // A different card never joins a set.
    let mixed = [
        gpu("gpu0", 40 * GIB, A6000),
        gpu("gpu1", 40 * GIB, "NVIDIA RTX 3090"),
    ];
    assert_eq!(
        choose_tensor_parallel(&table(&[(1, 50 * GIB), (2, 30 * GIB)]), &mixed, &|_| None),
        AutoChoice::NoFit
    );
    let hint = fit_hint(&table(&[(1, 50 * GIB), (2, 45 * GIB)]), &cards[..2]);
    assert!(
        hint.contains("no tensor_parallel up to this host's 2 GPU(s) fits"),
        "{hint}"
    );
    assert!(hint.contains(&format!("{} bytes", 45 * GIB)), "{hint}");
    let unmeasured = AutoTable {
        weights_measured: false,
        ..rows
    };
    assert_eq!(
        choose_tensor_parallel(&unmeasured, &cards, &|_| None),
        AutoChoice::Unmeasured
    );
}

// T46 (ADR 0035 §2): unpinned, each choice is a set of N identical GPUs in
// rank order keyed by its ids; `auto` offers every size's sets as the
// explicit size states them; one GPU keeps its choices exactly.
#[test]
fn device_choices_are_sets_of_identical_gpus() {
    let mut h = host(4);
    let keys = |d: &Value, h: &Value| -> Vec<String> {
        device_choices(d, h)
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect()
    };
    assert_eq!(
        keys(&deployment(None), &h),
        ["gpu0", "gpu1", "gpu2", "gpu3"]
    );
    assert_eq!(
        keys(&deployment(tp(2)), &h),
        [
            "gpu0,gpu1",
            "gpu0,gpu2",
            "gpu0,gpu3",
            "gpu1,gpu2",
            "gpu1,gpu3",
            "gpu2,gpu3"
        ]
    );
    let choices = device_choices(&deployment(tp(2)), &h).unwrap();
    assert_eq!(
        choices[0].1["devices"],
        json!([{"id": "gpu0", "sharing": "shared"}, {"id": "gpu1", "sharing": "shared"}])
    );
    let auto = device_choices(&deployment(Some(json!("auto"))), &h).unwrap();
    assert_eq!(auto.len(), 4 + 6 + 4 + 1);
    assert!(auto[0].1.get("topology").is_none(), "N = 1 states none");
    assert_eq!(auto[4].1["topology"], json!({"tensor_parallel": 2}));
    assert_eq!(auto[14].0, "gpu0,gpu1,gpu2,gpu3");
    // A different card is in no set with the others.
    h["resource_policy"]["devices"]["gpu2"]["product_name"] = "NVIDIA RTX 3090".into();
    assert_eq!(
        keys(&deployment(tp(2)), &h),
        ["gpu0,gpu1", "gpu0,gpu3", "gpu1,gpu3"]
    );
    // A pin offers no choice.
    let mut pinned = deployment(tp(2));
    pinned["devices"] = json!([{"id": "gpu0"}, {"id": "gpu1"}]);
    assert!(device_choices(&pinned, &h).unwrap().is_empty());
}

/// #219 (ADR 0019 amendment A1, ADR 0035 §3): on two GPUs a request declared
/// per GPU before the checkpoint's weights are known has a startup peak equal
/// to the request, as on one GPU: each card's first start is charged its
/// Ready figure (the request, the CUDA context and the communication
/// allowance), with no first-start graph allowance on top. A first run
/// measures each card's peak (ADR 0019 amendment A2).
// T46 T14
#[test]
fn a_request_declared_per_gpu_starts_at_its_ready_charge_on_two_gpus() {
    let mut d = deployment(tp(2));
    d["residency"] = json!("restart_only");
    d["engine_config"]["memory"] = json!({"request": "30GiB", "kv_cache": "4GiB"});
    let e = resolve_effective_with_checkpoint(&d, &host(2), CheckpointFacts::default()).unwrap();
    assert_eq!(e.local_tensor_parallel(), 2);
    assert_eq!(e.engine_config.memory().startup_bytes, Some(30 * GIB));
    assert_eq!(e.engine_config.memory().startup_graphs_bytes, None);
    let per_gpu = 30 * GIB + OVERHEAD + COMM;
    let cards = |phase: &capyctl_config::effective::PhaseFootprint| {
        phase
            .allocations
            .iter()
            .filter(|a| a.domain.starts_with("gpu"))
            .map(|a| a.bytes)
            .collect::<Vec<_>>()
    };
    assert_eq!(cards(&e.resources.ready), [per_gpu, per_gpu]);
    assert_eq!(cards(&e.resources.cold), [per_gpu, per_gpu]);
    assert!(capyctl_config::effective::startup_measure(&e).is_some());
}

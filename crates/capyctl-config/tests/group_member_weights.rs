//! ADR 0028 §5 (amendment of 2026-10-07, owner decision): a group member whose
//! phases derive from `engine_config.memory` is charged its share of the
//! checkpoint's weights, not the whole checkpoint.

use capyctl_config::effective::{
    decode_effective_snapshot, derive_default_managed_ceiling, resolve_effective_with_checkpoint,
    resolve_snapshot_with_checkpoint, CheckpointFacts, EffectiveDeployment,
    ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES as OVERHEAD,
};
use capyctl_domain::member_weights::{member_weights_bytes, CheckpointLayout};
use serde_json::{json, Value};

const GIB: i64 = 1 << 30;

fn fixture() -> (Value, Value) {
    let all: Value = serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    let (mut deployment, mut host) = (all["deployment"].clone(), all["host"].clone());
    let object = deployment.as_object_mut().unwrap();
    object.remove("resources");
    object.remove("residency");
    host["resource_policy"]["domains"]["unified"]["managed_limit"] = "200GiB".into();
    (deployment, host)
}

fn group(tp: u32, pp: u32) -> (Value, Value) {
    let (mut deployment, host) = fixture();
    let hosts: Vec<String> = (0..tp * pp).map(|n| format!("host-{n}")).collect();
    deployment["topology"] = json!({"tensor_parallel": tp, "pipeline_parallel": pp});
    deployment["placement"] = json!({ "hosts": hosts });
    (deployment, host)
}

fn layout(sharded: i64, layers: u32, largest: i64) -> CheckpointLayout {
    CheckpointLayout {
        sharded_bytes: sharded,
        layer_count: layers,
        largest_layer_bytes: largest,
    }
}

fn measured(weights: i64, layout: Option<CheckpointLayout>) -> CheckpointFacts {
    CheckpointFacts {
        weights_bytes: Some(weights),
        layout,
        ..Default::default()
    }
}

fn ready(e: &EffectiveDeployment) -> i64 {
    e.resources.ready.allocations[0].bytes
}

fn cold(e: &EffectiveDeployment) -> i64 {
    e.resources.cold.allocations[0].bytes
}

/// The derived phases of one member sized for `weights` bytes held: the
/// request is those weights, the KV cache and the margin, plus the engine's
/// CUDA context and graphs.
fn assert_sized_for(e: &EffectiveDeployment, weights: i64) {
    let memory = e.engine_config.memory();
    assert_eq!(memory.weights_bytes, Some(weights));
    assert_eq!(memory.kv_cache_bytes, 4 * GIB);
    assert_eq!(
        memory.request_bytes,
        weights + 4 * GIB + memory.margin_bytes
    );
    assert_eq!(ready(e), memory.request_bytes + OVERHEAD);
}

fn round_trips(e: &EffectiveDeployment) {
    let text = serde_json::to_string(e).unwrap();
    assert_eq!(&decode_effective_snapshot(&text).unwrap(), e);
}

// T03, T39: a TP2 member holds half the sharded weights and every replicated
// tensor whole.
#[test]
fn a_tp2_member_is_charged_half_the_weights_and_the_replicated_tensors() {
    const WEIGHTS: i64 = 60 * GIB;
    let l = layout(56 * GIB, 40, 1400 << 20);
    let (deployment, host) = group(2, 1);
    let member =
        resolve_effective_with_checkpoint(&deployment, &host, measured(WEIGHTS, Some(l))).unwrap();
    let share = 28 * GIB + 4 * GIB;
    assert_sized_for(&member, share);
    let recorded = member.engine_config.memory().member.unwrap();
    assert_eq!(
        (recorded.tensor_parallel, recorded.pipeline_parallel),
        (2, 1)
    );
    assert_eq!(recorded.checkpoint_weights_bytes, Some(WEIGHTS));
    assert_eq!(recorded.layout, Some(l));
    assert_eq!(
        member.engine_config.memory().checkpoint_weights_bytes(),
        Some(WEIGHTS)
    );
    // The same deployment on one host is charged the whole checkpoint.
    let (single, host) = fixture();
    let whole =
        resolve_effective_with_checkpoint(&single, &host, measured(WEIGHTS, Some(l))).unwrap();
    assert_sized_for(&whole, WEIGHTS);
    assert!(ready(&member) < ready(&whole) - 25 * GIB);
    round_trips(&member);
}

// T03: a PP2 member holds the larger stage's run of layers, whole.
#[test]
fn a_pp2_member_is_charged_its_stage() {
    // 61 layers of at most 1 GiB: one stage holds 31 of them.
    const WEIGHTS: i64 = 61 * GIB + 3 * GIB;
    let l = layout(61 * GIB, 61, GIB);
    let (deployment, host) = group(1, 2);
    let member =
        resolve_effective_with_checkpoint(&deployment, &host, measured(WEIGHTS, Some(l))).unwrap();
    assert_sized_for(&member, 31 * GIB + 3 * GIB);
    round_trips(&member);
}

// T03: TP2 x PP2 over four hosts splits the stage again.
#[test]
fn a_tp2_pp2_member_is_charged_half_its_stage() {
    const WEIGHTS: i64 = 64 * GIB + 2 * GIB;
    let l = layout(64 * GIB, 64, GIB);
    let (deployment, host) = group(2, 2);
    let member =
        resolve_effective_with_checkpoint(&deployment, &host, measured(WEIGHTS, Some(l))).unwrap();
    assert_sized_for(&member, 16 * GIB + 2 * GIB);
    round_trips(&member);
}

// T03: without the checkpoint's headers a tenth of the weights is kept whole.
#[test]
fn without_a_layout_a_member_keeps_a_tenth_whole() {
    const WEIGHTS: i64 = 100 * GIB;
    let (deployment, host) = group(2, 1);
    let member =
        resolve_effective_with_checkpoint(&deployment, &host, measured(WEIGHTS, None)).unwrap();
    assert_sized_for(&member, 45 * GIB + 10 * GIB);
    assert_eq!(member.engine_config.memory().member.unwrap().layout, None);
    round_trips(&member);
}

// T03: a group accepted before its weights were measured is frozen
// provisional and re-resolved from its snapshot with the measurement; it
// keeps its topology, so the measured share replaces the placeholder.
#[test]
fn a_provisional_member_is_re_resolved_with_its_share() {
    let (deployment, host) = group(2, 1);
    let provisional =
        resolve_effective_with_checkpoint(&deployment, &host, measured(0, None)).unwrap();
    assert_eq!(provisional.engine_config.memory().weights_bytes, Some(0));
    let text = serde_json::to_string(&provisional).unwrap();
    let l = layout(56 * GIB, 40, 1400 << 20);
    let resolved = resolve_snapshot_with_checkpoint(&text, measured(60 * GIB, Some(l))).unwrap();
    assert_sized_for(&resolved, 32 * GIB);
    assert_eq!(
        resolved,
        resolve_effective_with_checkpoint(&deployment, &host, measured(60 * GIB, Some(l))).unwrap()
    );
    round_trips(&resolved);
}

// T03: a member whose request and KV cache are declared derives only the
// share-dependent parts; one with unknown weights records its topology.
#[test]
fn a_member_with_unknown_weights_records_its_topology() {
    let (mut deployment, host) = group(2, 1);
    deployment["engine_config"]["memory"] = json!({"request": "40GiB", "kv_cache": "4GiB"});
    let member =
        resolve_effective_with_checkpoint(&deployment, &host, CheckpointFacts::default()).unwrap();
    let memory = member.engine_config.memory();
    assert_eq!(memory.weights_bytes, None);
    let recorded = memory.member.unwrap();
    assert_eq!(recorded.checkpoint_weights_bytes, None);
    assert_eq!(recorded.tensor_parallel, 2);
    round_trips(&member);
}

// T39: declared per-member resources are charged exactly as written, with
// the identity they had before the share existed.
#[test]
fn declared_resources_are_unchanged() {
    let all: Value = serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    let (mut deployment, host) = (all["deployment"].clone(), all["host"].clone());
    deployment.as_object_mut().unwrap().remove("host");
    deployment["topology"] = json!({"tensor_parallel": 2});
    deployment["placement"] = json!({"hosts": ["host-a", "host-b"]});
    let l = layout(GIB, 4, GIB / 4);
    let member =
        resolve_effective_with_checkpoint(&deployment, &host, measured(2 * GIB, Some(l))).unwrap();
    let memory = member.engine_config.memory();
    assert_eq!(memory.weights_bytes, Some(2 * GIB));
    assert_eq!(memory.member, None);
    assert_eq!(ready(&member), 8 * GIB);
    assert!(!serde_json::to_string(&member).unwrap().contains("member"));
    // Re-pinned for ADR 0014 amendment A21 (2026-10-09): the fixture parks
    // on unified memory, so its effective configuration now carries the
    // defaulted `lazy` loader and its provenance entry.
    assert_eq!(
        member.recipe_fingerprint,
        "fec0b283800966df2d2d273b35f590b665e2d2395442e0dc768708a7cbf0094a"
    );
}

// T39: world size 1 keeps the whole checkpoint and the identity it had.
#[test]
fn a_single_host_deployment_is_unchanged() {
    let (deployment, host) = fixture();
    let l = layout(56 * GIB, 40, 1400 << 20);
    let single =
        resolve_effective_with_checkpoint(&deployment, &host, measured(60 * GIB, Some(l))).unwrap();
    assert_sized_for(&single, 60 * GIB);
    assert_eq!(single.engine_config.memory().member, None);
    assert!(!serde_json::to_string(&single).unwrap().contains("member"));
    let without =
        resolve_effective_with_checkpoint(&deployment, &host, measured(60 * GIB, None)).unwrap();
    assert_eq!(single, without);
    let mut tp1 = deployment.clone();
    tp1["topology"] = json!({"tensor_parallel": 1, "pipeline_parallel": 1});
    assert_eq!(
        resolve_effective_with_checkpoint(&tp1, &host, measured(60 * GIB, Some(l))).unwrap(),
        single
    );
    // Pinned on main after the unified-memory margin (ADR 0014 A18) merged: a
    // 60 GiB checkpoint is above its 26.7 GiB floor, so that change, not this
    // one, moved the identity. The assertions above prove the group share
    // leaves a single host untouched. Re-pinned again for amendment A21
    // (2026-10-09): the fixture parks on unified memory, so its effective
    // configuration now carries the defaulted `lazy` loader and its
    // provenance entry.
    assert_eq!(
        single.recipe_fingerprint,
        "84d40e87f397081bf2cfc4b126e21bc85ce2ec964d5ea7150fa9b15f9f5bf6ee"
    );
}

// T03 (owner decision 2026-10-07): a Flash-Next-sized checkpoint (126 GiB of
// weights, about 2 GiB of embeddings, head and norms) at TP 2 on two GB10
// hosts (121.7 GiB each, the automatic managed limit). Charged the whole
// checkpoint, a member could never be admitted; charged its share, each fits.
#[test]
fn a_flash_next_sized_tp2_group_fits_two_gb10_hosts() {
    const WEIGHTS: i64 = 126 * GIB;
    let capacity = 121 * GIB + 7 * GIB / 10;
    let managed = derive_default_managed_ceiling(Some(capacity))
        .unwrap()
        .unwrap()
        .managed_limit_bytes;
    let l = layout(124 * GIB, 48, 124 * GIB / 48);
    let (mut deployment, mut host) = group(2, 1);
    deployment["engine_config"]["memory"] = json!({"kv_cache": "4GiB", "startup": "90GiB"});
    host["resource_policy"]["domains"]["unified"]["managed_limit"] = format!("{managed}B").into();
    let member =
        resolve_effective_with_checkpoint(&deployment, &host, measured(WEIGHTS, Some(l))).unwrap();
    let share = member_weights_bytes(WEIGHTS, Some(&l), 2, 1).unwrap();
    assert_eq!(share, 64 * GIB);
    assert_sized_for(&member, share);
    assert!(ready(&member) <= managed, "{} > {managed}", ready(&member));
    assert!(cold(&member) <= managed, "{} > {managed}", cold(&member));
    assert_eq!(cold(&member), 90 * GIB + OVERHEAD);
    // The whole checkpoint alone is above the managed limit.
    assert!(WEIGHTS > managed);
    round_trips(&member);
}

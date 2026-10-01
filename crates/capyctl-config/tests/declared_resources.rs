//! SPEC §15.3, ADR 0023 §4: `resources` is checked offline with the server's
//! own decoding.
use capyctl_config::effective::validate_declared_resources;
use serde_json::{json, Value};

fn phase(bytes: &str, devices: Value) -> Value {
    json!({"allocations": [{"domain": "unified", "bytes": bytes, "host_kv_bytes": "0B"}], "devices": devices})
}

fn deployment(engine: &str, resources: Option<Value>) -> Value {
    let gpu = json!([{"id": "gpu0", "sharing": "exclusive"}]);
    let mut d = json!({"schema_version": 1, "kind": "deployment", "name": "m",
        "runtime_profile": engine, "devices": gpu});
    if let Some(r) = resources {
        d["resources"] = r;
    }
    d
}

fn recipe() -> Value {
    let gpu = json!([{"id": "gpu0", "sharing": "exclusive"}]);
    json!({"cold": phase("32GiB", gpu.clone()), "ready": phase("30GiB", gpu.clone()),
        "parking": phase("30GiB", gpu.clone()), "parked": phase("0B", json!([])),
        "wake": phase("32GiB", gpu)})
}

// T03 T41: the guide's TensorFold block passes offline.
#[test]
fn a_complete_resources_block_passes() {
    validate_declared_resources(&deployment("tensorfold", Some(recipe()))).unwrap();
}

// T03: the live finding, a block without `host_kv_bytes` and `devices`.
#[test]
fn a_block_missing_required_fields_is_refused_with_its_path() {
    let mut r = recipe();
    r["cold"] = json!({"allocations": [{"domain": "unified", "bytes": "32GiB"}]});
    let error = validate_declared_resources(&deployment("vllm", Some(r))).unwrap_err();
    assert!(error.path.starts_with("resources.cold"), "{error:?}");
}

// T03: phase claims must match the deployment's devices, as at resolution.
#[test]
fn a_phase_claim_that_is_not_the_deployments_is_refused() {
    let mut d = deployment("vllm", Some(recipe()));
    d["devices"] = json!([{"id": "gpu1", "sharing": "exclusive"}]);
    let error = validate_declared_resources(&d).unwrap_err();
    assert_eq!(error.path, "resources.devices");
}

// T03 T41, ADR 0023 §4: a TensorFold deployment states resources, offline too.
#[test]
fn a_tensorfold_deployment_without_resources_is_refused() {
    for d in [deployment("tensorfold", None), {
        let mut d = deployment("tf-local", None);
        d["engine_config"] = json!({"tensorfold": {"thinking": false}});
        d
    }] {
        let error = validate_declared_resources(&d).unwrap_err();
        assert_eq!(error.path, "resources", "{d}");
    }
    validate_declared_resources(&deployment("vllm", None)).unwrap();
}

// T03: `resources: null` is absent, as the server decodes it.
#[test]
fn a_null_resources_block_is_treated_as_absent() {
    validate_declared_resources(&deployment("vllm", Some(Value::Null))).unwrap();
    let error =
        validate_declared_resources(&deployment("tensorfold", Some(Value::Null))).unwrap_err();
    assert_eq!(error.path, "resources");
    assert_eq!(error.code, capyctl_config::ConfigErrorCode::MissingRequired);
}

// T03 T41, found live 2026-10-01: the guide's block names devices without
// `sharing`, which deploy fills in from the host. Offline it passes too.
#[test]
fn the_guides_block_without_sharing_passes() {
    let gpu = json!([{"id": "gpu0"}]);
    let r = json!({"cold": phase("32GiB", gpu.clone()), "ready": phase("30GiB", gpu.clone()),
        "parking": phase("30GiB", gpu.clone()), "parked": phase("0B", json!([])),
        "wake": phase("32GiB", gpu.clone())});
    let mut d = deployment("tensorfold", Some(r.clone()));
    d["devices"] = gpu;
    validate_declared_resources(&d).unwrap();
    // A claim that is not the deployment's device is still refused.
    let mut other = d.clone();
    other["resources"]["cold"]["devices"] = json!([{"id": "gpu1"}]);
    assert!(validate_declared_resources(&other).is_err());
}

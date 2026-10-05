use capyctl_config::engine_env::*;
use std::collections::BTreeMap;

fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}
fn approved(entries: &[&str]) -> ApprovedEnv {
    ApprovedEnv::parse(&entries.iter().map(|e| e.to_string()).collect::<Vec<_>>()).unwrap()
}

// T37: a deployment name must match an approval; globs match by prefix.
#[test]
fn deployment_names_need_an_approval() {
    let a = approved(&["SGLANG_ENABLE_*", "VLLM_MARLIN_USE_ATOMIC_ADD"]);
    assert!(
        resolve_engine_env(&map(&[]), &a, &map(&[("SGLANG_ENABLE_JIT_DEEPGEMM", "0")])).is_ok()
    );
    assert!(
        resolve_engine_env(&map(&[]), &a, &map(&[("VLLM_MARLIN_USE_ATOMIC_ADD", "1")])).is_ok()
    );
    let err = resolve_engine_env(
        &map(&[]),
        &a,
        &map(&[("TORCH_NCCL_HEARTBEAT_TIMEOUT_SEC", "180")]),
    )
    .unwrap_err();
    assert_eq!(
        err.code(),
        "engine_env_not_approved:TORCH_NCCL_HEARTBEAT_TIMEOUT_SEC"
    );
}

// T37: the profile's own env is host-authored and needs no approval.
#[test]
fn profile_env_needs_no_approval() {
    let r = resolve_engine_env(
        &map(&[("TORCH_NCCL_ASYNC_ERROR_HANDLING", "1")]),
        &approved(&[]),
        &map(&[]),
    )
    .unwrap();
    assert_eq!(
        r.vars["TORCH_NCCL_ASYNC_ERROR_HANDLING"].1,
        EnvSource::Profile
    );
}

// T21, T37, Review Focus 1: owned names are refused at both levels, even through wide globs.
#[test]
fn owned_names_are_never_settable() {
    let a = approved(&["N*", "G*", "M*", "V*", "S*", "T*", "H*"]);
    for name in [
        "NCCL_IB_HCA",
        "GLOO_SOCKET_IFNAME",
        "MASTER_ADDR",
        "VLLM_HOST_IP",
        "SGLANG_HOST_IP",
        "HOST_IP",
        "SGLANG_LOCAL_IP_NIC",
        "SGLANG_DISTRIBUTED_INIT_METHOD_OVERRIDE",
        "TF_COMM_BACKEND",
        "TF_NCCL_LIB",
        "CAPYCTL_ENGINE_KEY",
        "LD_PRELOAD",
        "PATH",
        "CUDA_VISIBLE_DEVICES",
    ] {
        let e = resolve_engine_env(&map(&[]), &a, &map(&[(name, "x")])).unwrap_err();
        assert_eq!(e.code(), format!("engine_env_reserved:{name}"));
        let e = resolve_engine_env(&map(&[(name, "x")]), &a, &map(&[])).unwrap_err();
        assert_eq!(e.code(), format!("engine_env_reserved:{name}"));
    }
}

// T03: approval entries are validated; a bare glob and owned-only entries are refused.
#[test]
fn approval_entries_are_validated() {
    for bad in [
        "*",
        "mbx_*",
        "MBX_**",
        "A*B",
        "NCCL_*",
        "VLLM_HOST_IP",
        "TF_COMM_BACKEND",
        "",
    ] {
        assert!(ApprovedEnv::parse(&[bad.to_string()]).is_err(), "{bad}");
    }
    assert!(ApprovedEnv::parse(&(0..65).map(|i| format!("V{i}")).collect::<Vec<_>>()).is_err());
}

// T14: the deployment overrides the profile for one name, with provenance.
#[test]
fn deployment_overrides_profile_with_provenance() {
    let r = resolve_engine_env(
        &map(&[("SGLANG_ENABLE_X", "1")]),
        &approved(&["SGLANG_ENABLE_*"]),
        &map(&[("SGLANG_ENABLE_X", "0")]),
    )
    .unwrap();
    assert_eq!(
        r.vars["SGLANG_ENABLE_X"],
        ("0".to_string(), EnvSource::Deployment)
    );
}

// T03: values are bounded and single-line; the safe names keep their rules.
#[test]
fn values_are_bounded_and_safe_names_keep_rules() {
    let a = approved(&["MBX_*"]);
    assert!(resolve_engine_env(&map(&[]), &a, &map(&[("MBX_X", "a\nb")])).is_err());
    assert!(resolve_engine_env(&map(&[]), &a, &map(&[("MBX_X", &"x".repeat(4097))])).is_err());
    assert!(resolve_engine_env(&map(&[]), &a, &map(&[("MAX_JOBS", "4")])).is_ok());
    assert!(resolve_engine_env(&map(&[]), &a, &map(&[("MAX_JOBS", "0")])).is_err());
}

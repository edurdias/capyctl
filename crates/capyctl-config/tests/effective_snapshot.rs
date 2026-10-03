use capyctl_config::effective::{decode_effective_snapshot, resolve_effective};
use serde_json::{json, Value};

#[test]
fn normalized_snapshot_revalidates_settings_and_fingerprint() {
    for fixture in [
        include_str!("fixtures/effective-vllm-golden.json"),
        include_str!("fixtures/effective-sglang-golden.json"),
    ] {
        let source: Value = serde_json::from_str(fixture).unwrap();
        let effective =
            resolve_effective(&source["input"]["deployment"], &source["input"]["host"]).unwrap();
        let snapshot = serde_json::to_value(&effective).unwrap();
        assert_eq!(
            decode_effective_snapshot(&snapshot.to_string()).unwrap(),
            effective
        );
        for (pointer, replacement) in [
            ("/recipe_fingerprint", json!("forged")),
            ("/profile/executable", json!("relative")),
            ("/resources/cold/allocations/0/bytes", json!(-1)),
            ("/host/hardware_fingerprint", json!("different")),
            ("/profile/revision", json!(99)),
        ] {
            let mut bad = snapshot.clone();
            *bad.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                decode_effective_snapshot(&bad.to_string()).is_err(),
                "{pointer}"
            );
        }
        let mut bad = snapshot.clone();
        bad["profile"]["unknown"] = json!(true);
        assert!(decode_effective_snapshot(&bad.to_string()).is_err());
        assert!(decode_effective_snapshot(&format!(
            "{{\"name\":\"duplicate\",{}",
            &snapshot.to_string()[1..]
        ))
        .is_err());
    }
    assert!(decode_effective_snapshot(&" ".repeat((1 << 20) + 1)).is_err());
}

/// ADR 0012 / T14: a snapshot of a profile whose deep-park value was defaulted
/// carries `default` provenance and revalidates; a snapshot that claims the
/// default for a value that is not the default, or names another provenance,
/// is refused.
// T14 T21
#[test]
fn a_defaulted_deep_park_snapshot_revalidates_and_cannot_be_forged() {
    for fixture in [
        include_str!("fixtures/effective-vllm-golden.json"),
        include_str!("fixtures/effective-sglang-golden.json"),
    ] {
        let source: Value = serde_json::from_str(fixture).unwrap();
        let mut host = source["input"]["host"].clone();
        for profile in host["runtime_profiles"]
            .as_object_mut()
            .unwrap()
            .values_mut()
        {
            profile["security"]
                .as_object_mut()
                .unwrap()
                .remove("deep_park");
        }
        let effective = resolve_effective(&source["input"]["deployment"], &host).unwrap();
        let snapshot = serde_json::to_value(&effective).unwrap();
        assert_eq!(
            snapshot["profile"]["security"]["deep_park_source"],
            "default"
        );
        assert_eq!(
            decode_effective_snapshot(&snapshot.to_string()).unwrap(),
            effective
        );
        for (field, forged) in [
            ("deep_park", json!("disabled")),
            ("deep_park_source", json!("host_policy")),
        ] {
            let mut bad = snapshot.clone();
            bad["profile"]["security"][field] = forged;
            assert!(
                decode_effective_snapshot(&bad.to_string()).is_err(),
                "{field}"
            );
        }
    }
}

/// ADR 0014 amendment A13: a revision frozen while CapyCTL turned SGLang's
/// CUDA graphs off beside the memory saver keeps them off: it revalidates
/// unchanged, with the old default and its provenance.
#[test]
fn a_revision_frozen_with_sglang_graphs_off_by_default_keeps_them_off() {
    let source: Value = serde_json::from_str(include_str!(
        "fixtures/effective-sglang-graphs-off-legacy.json"
    ))
    .unwrap();
    let frozen = &source["effective"];
    assert_eq!(frozen["engine_config"]["common"]["cuda_graphs"], false);
    assert_eq!(
        frozen["engine_config"]["provenance"]["cuda_graphs"],
        "capyctl default"
    );
    let decoded = decode_effective_snapshot(&frozen.to_string()).unwrap();
    assert_eq!(&serde_json::to_value(&decoded).unwrap(), frozen);
    // A new resolution of the same input leaves the graphs to SGLang.
    let fresh =
        resolve_effective(&source["input"]["deployment"], &source["input"]["host"]).unwrap();
    let fresh = serde_json::to_value(&fresh).unwrap();
    assert!(fresh["engine_config"]["common"]["cuda_graphs"].is_null());
    assert!(fresh["engine_config"]["provenance"]
        .get("cuda_graphs")
        .is_none());
}

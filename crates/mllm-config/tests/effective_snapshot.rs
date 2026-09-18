use mllm_config::effective::{decode_effective_snapshot, resolve_effective};
use serde_json::{Value, json};

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
        assert!(
            decode_effective_snapshot(&format!(
                "{{\"name\":\"duplicate\",{}",
                &snapshot.to_string()[1..]
            ))
            .is_err()
        );
    }
    assert!(decode_effective_snapshot(&" ".repeat((1 << 20) + 1)).is_err());
}

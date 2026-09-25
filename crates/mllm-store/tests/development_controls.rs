//! SPEC §9.1, T21, ADR 0012, owner decision P4: status marks every deployment
//! whose launch enables vLLM development mode, derived from the frozen
//! effective configuration and never declared. CPU tests of this surface are not
//! qualification of any native engine recipe.

use mllm_config::effective::{resolve_effective, EffectiveDeployment};
use mllm_store::development_controls::{
    for_effective, for_host_profile, ExposureState, MITIGATIONS, SURFACE,
};
use mllm_store::Store;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

fn golden() -> (Value, Value) {
    let source: Value = serde_json::from_str(include_str!(
        "../../mllm-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    (
        source["input"]["deployment"].clone(),
        source["input"]["host"].clone(),
    )
}

/// `deep_park`: `None` omits the switch so the ADR 0012 default applies. Sleep
/// mode is derived (ADR 0014 §4), so the installation carries no launch settings.
fn effective(deep_park: Option<&str>, residency: &str) -> EffectiveDeployment {
    let (mut deployment, mut host) = golden();
    let profile = &mut host["runtime_profiles"]["local"];
    profile.as_object_mut().unwrap().remove("launch_settings");
    match deep_park {
        Some(value) => profile["security"]["deep_park"] = json!(value),
        None => {
            profile["security"]
                .as_object_mut()
                .unwrap()
                .remove("deep_park");
        }
    }
    deployment["residency"] = json!(residency);
    // ADR 0014 §5: the golden checkpoint has no manifest, so the KV cache is declared.
    deployment["engine_config"] = json!({"memory": {"kv_cache": "4GiB"}});
    resolve_effective(&deployment, &host).unwrap()
}

fn store_with(effective_json: Option<&str>) -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.sqlite3");
    let store = Store::open(&path).unwrap();
    let writer = Connection::open(path).unwrap();
    writer
        .execute_batch(
            "INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('d','toy','managed','ready',1,0,1,1);",
        )
        .unwrap();
    if let Some(json) = effective_json {
        writer
            .execute(
                "INSERT INTO effective_revisions VALUES('d',1,?1,'digest')",
                params![json],
            )
            .unwrap();
    }
    (dir, store)
}

fn snapshot_controls(effective_json: Option<&str>) -> Value {
    let (_dir, store) = store_with(effective_json);
    let snapshot = store.snapshot().unwrap();
    serde_json::to_value(&snapshot).unwrap()["deployments"][0]["development_controls"].clone()
}

// T21
#[test]
fn a_parking_vllm_deployment_on_a_default_on_host_is_marked_with_its_mitigations() {
    let effective = effective(None, "deep");
    let controls = snapshot_controls(Some(&serde_json::to_string(&effective).unwrap()));
    assert_eq!(controls["state"], "exposed");
    assert_eq!(controls["engine"], "vllm");
    assert_eq!(controls["deep_park"], "enabled");
    assert_eq!(controls["enable_sleep_mode"], true);
    assert_eq!(controls["residency"], "deep");
    assert_eq!(controls["surface"], json!(SURFACE));
    assert_eq!(controls["mitigations"], json!(MITIGATIONS));
    assert_eq!(controls["production_safe"], false);
    assert_eq!(
        serde_json::to_value(for_effective(&effective)).unwrap(),
        controls,
        "the stored projection and the typed derivation must agree"
    );
}

// T14: provenance of a defaulted switch is shown, a declared one is not invented.
#[test]
fn deep_park_provenance_is_carried_into_the_status_mark() {
    let defaulted = effective(None, "deep");
    let controls = snapshot_controls(Some(&serde_json::to_string(&defaulted).unwrap()));
    assert_eq!(controls["deep_park_source"], "default");
    let declared = effective(Some("enabled"), "deep");
    let controls = snapshot_controls(Some(&serde_json::to_string(&declared).unwrap()));
    assert_eq!(controls["state"], "exposed");
    assert_eq!(controls["deep_park_source"], "host_policy");
}

// T21: vLLM + deep park on + parking residency, and nothing else, is marked.
// A host opt-out or a restart-only deployment launches with development mode off.
#[test]
fn the_mark_mirrors_whether_the_launch_enables_development_mode() {
    let cases = [
        (Some("disabled"), "restart_only", "not_exposed"),
        (None, "restart_only", "not_exposed"),
        (Some("enabled"), "restart_only", "not_exposed"),
        (None, "deep", "exposed"),
        (Some("enabled"), "deep", "exposed"),
    ];
    for (deep_park, residency, expected) in cases {
        let effective = effective(deep_park, residency);
        let controls = snapshot_controls(Some(&serde_json::to_string(&effective).unwrap()));
        assert_eq!(controls["state"], expected, "{deep_park:?} {residency}");
        assert_eq!(
            serde_json::to_value(for_effective(&effective)).unwrap(),
            controls
        );
        if expected == "not_exposed" {
            assert!(controls.get("surface").is_none());
            assert!(controls.get("mitigations").is_none());
        }
    }
}

// T21: an SGLang deployment exposes no vLLM development controls.
#[test]
fn an_sglang_launch_is_not_marked() {
    let effective = effective(None, "deep");
    let mut stored = serde_json::to_value(&effective).unwrap();
    stored["profile"]["engine"] = json!("sglang");
    stored["engine_config"] = json!({"engine": "sglang"});
    let controls = snapshot_controls(Some(&stored.to_string()));
    assert_eq!(controls["state"], "not_exposed");
    assert_eq!(controls["engine"], "sglang");
}

// T21: a revision frozen before ADR 0014 carries sleep mode on the profile's
// launch settings; it is still marked by what it launches with.
#[test]
fn a_revision_frozen_before_engine_config_is_still_marked() {
    let effective = effective(None, "deep");
    let mut stored = serde_json::to_value(&effective).unwrap();
    stored.as_object_mut().unwrap().remove("engine_config");
    stored["profile"]["launch_settings"] = json!({"engine": "vllm", "enable_sleep_mode": true});
    let controls = snapshot_controls(Some(&stored.to_string()));
    assert_eq!(controls["state"], "exposed");
    stored["profile"]["launch_settings"]["enable_sleep_mode"] = json!(false);
    let controls = snapshot_controls(Some(&stored.to_string()));
    assert_eq!(controls["state"], "not_exposed");
}

// T21: status never claims a deployment is safe when it cannot derive the
// answer. Missing or malformed configuration is `unknown`, and the raw stored
// text is never reflected.
#[test]
fn an_underivable_configuration_is_unknown_and_never_leaks() {
    for stored in [
        None,
        Some("SECRET-CONFIG-/private/checkpoint"),
        Some(
            r#"{"profile":{"engine":"vllm","security":{"deep_park":"SECRET"}},"engine_config":{"enable_sleep_mode":true},"residency":"deep"}"#,
        ),
        Some(
            r#"{"profile":{"engine":"vllm","security":{"deep_park":"enabled"}},"engine_config":{"enable_sleep_mode":"yes"},"residency":"deep"}"#,
        ),
        Some(r#"{"profile":{"engine":7}}"#),
    ] {
        let (_dir, store) = store_with(stored);
        let snapshot = store.snapshot().unwrap();
        let json = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(
            json["deployments"][0]["development_controls"]["state"], "unknown",
            "{stored:?}"
        );
        assert!(!serde_json::to_string(&snapshot).unwrap().contains("SECRET"));
    }
}

// T21: the host installation view marks every vLLM profile whose launches
// enable development mode, honoring the default and the opt-out.
#[test]
fn host_profiles_are_marked_from_the_published_document() {
    let (_, host) = golden();
    let mut profile = host["runtime_profiles"]["local"].clone();
    profile.as_object_mut().unwrap().remove("launch_settings");
    assert_eq!(for_host_profile(&profile).state, ExposureState::Exposed);

    let mut omitted = profile.clone();
    omitted["security"]
        .as_object_mut()
        .unwrap()
        .remove("deep_park");
    let marked = serde_json::to_value(for_host_profile(&omitted)).unwrap();
    assert_eq!(marked["state"], "exposed");
    assert_eq!(marked["deep_park_source"], "default");
    assert!(marked.get("residency").is_none());
    assert!(marked.get("enable_sleep_mode").is_none());
    assert_eq!(marked["applies_to"], "parking_deployments");

    let mut opted_out = profile.clone();
    opted_out["security"]["deep_park"] = json!("disabled");
    assert_eq!(
        for_host_profile(&opted_out).state,
        ExposureState::NotExposed
    );

    let mut malformed = profile;
    malformed["security"]["deep_park"] = json!("maybe");
    assert_eq!(for_host_profile(&malformed).state, ExposureState::Unknown);

    let sglang = json!({"engine":"sglang","security":{}});
    assert_eq!(for_host_profile(&sglang).state, ExposureState::NotExposed);
}

// T21: owner decision 2026-09-22: SGLang serves `/metrics` without its API key
// (SGLang exempts it), loopback only and read-only. Every SGLang deployment,
// instance and host installation is marked with that surface; it is derived
// from the engine family, never declared, and absent for vLLM.
#[test]
fn sglang_marks_its_unauthenticated_loopback_metrics() {
    use mllm_store::development_controls::UNAUTHENTICATED_LOCAL_SURFACES;
    let expected = json!({"surface": ["/metrics"], "listener": "loopback", "access": "read_only"});
    assert_eq!(
        serde_json::to_value(UNAUTHENTICATED_LOCAL_SURFACES).unwrap(),
        expected
    );

    let effective = effective(None, "deep");
    let mut stored = serde_json::to_value(&effective).unwrap();
    stored["profile"]["engine"] = json!("sglang");
    stored["engine_config"] = json!({"engine": "sglang"});
    let controls = snapshot_controls(Some(&stored.to_string()));
    assert_eq!(controls["state"], "not_exposed");
    assert_eq!(controls["unauthenticated_local_surfaces"], expected);

    let sglang = json!({"engine":"sglang","security":{}});
    let host = serde_json::to_value(for_host_profile(&sglang)).unwrap();
    assert_eq!(host["unauthenticated_local_surfaces"], expected);

    // vLLM keys every route, `/metrics` included: no such mark, exposed or not.
    let vllm = serde_json::to_value(for_effective(&effective)).unwrap();
    assert!(
        vllm.get("unauthenticated_local_surfaces").is_none(),
        "{vllm}"
    );
    let (_, golden_host) = golden();
    let profile =
        serde_json::to_value(for_host_profile(&golden_host["runtime_profiles"]["local"])).unwrap();
    assert!(profile.get("unauthenticated_local_surfaces").is_none());
    // Nothing is claimed when the engine cannot be derived.
    let unknown = snapshot_controls(Some("{}"));
    assert!(
        unknown.get("unauthenticated_local_surfaces").is_none(),
        "{unknown}"
    );
}

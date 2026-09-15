//! Real closed recipe acceptance under application-owned state and session.
use mllm_controller::OwnedCoordinatorState;
use mllm_domain::resources::MemoryObservation;
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
};

pub fn fixture() -> (
    tempfile::TempDir,
    Arc<Mutex<OwnedCoordinatorState>>,
    String,
    Vec<MemoryObservation>,
    Value,
) {
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let owner = Arc::new(Mutex::new(OwnedCoordinatorState::open(dir.path()).unwrap()));
    let golden: Value = serde_json::from_str(include_str!(
        "../../../mllm-config/tests/fixtures/effective-fake-golden.json"
    ))
    .unwrap();
    let mut host = golden["input"]["host"].clone();
    let mut manifest: Value = serde_json::from_str(include_str!(
        "../../../mllm-config/tests/fixtures/candidate-fake-qualification.json"
    ))
    .unwrap();
    manifest["limits"]["max_run_duration_ms"] = json!(600000);
    manifest["limits"]["max_cleanup_duration_ms"] = json!(60000);
    let reviewed = mllm_config::effective::candidate::validate_candidate_reviewed_snapshot_text(
        &manifest.to_string(),
    )
    .unwrap();
    host["runtime_profiles"]["local"]["build_fingerprint"] = json!("qualification-fake-v1");
    host["runtime_profiles"]["local"]["security"]["admin_credential_ref"] =
        json!("secret://admin-key");
    host["qualification_policy"] = json!({"revision":1,"allow_qualification_runs":true,"allow_experimental_controls":true,"allowed_manifest_digests":[reviewed.manifest_digest()],"max_run_duration":"600s","max_cleanup_duration":"60s","max_cases":128,"max_requests":4096,"max_request_body_bytes":"1MiB","max_input_tokens_per_request":131072,"max_output_tokens_per_request":16384});
    let policy = mllm_config::effective::resolve_effective(&golden["input"]["deployment"], &host)
        .unwrap()
        .host;
    let observations: Vec<_> = policy
        .domains
        .keys()
        .map(|domain| MemoryObservation {
            domain: domain.clone(),
            capacity_bytes: 1_i64 << 50,
            available_bytes: 1_i64 << 50,
            sampled_at_ms: 1000,
        })
        .collect();
    let run = {
        let state = owner.lock().unwrap();
        state
            .store()
            .import_resource_policy(state.session(), &policy, &observations, 1000)
            .unwrap();
        state
            .store()
            .import_qualification_policy(state.session(), &policy)
            .unwrap();
        let body = json!({"host_id":"lab","expected_host_revision":1,"recipe_digest":reviewed.manifest_digest(),"manifest":manifest,"deadline_ms":500000,"allow_owned_abort_cleanup":true});
        state
            .store()
            .create_candidate_run(
                state.session(),
                "owner",
                "create",
                &body.to_string(),
                &host,
                1000,
            )
            .unwrap()
            .run_id()
            .to_owned()
    };
    (dir, owner, run, observations, host)
}

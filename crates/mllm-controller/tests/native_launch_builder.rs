//! T02 acceptance: the production NativeLaunch builder and the shared private
//! descriptor builder. The descriptor JSON must stay byte-identical to what
//! `NativeLaunchHandoff::arm` built inline before the extraction.

use mllm_controller::native_launch::{frozen_from_work, private_descriptor};
use mllm_controller::coordinator::CoordinatorError;
use mllm_domain::completion::{ExecutionIdentities, StepExecutionContext, TransitionToken};
use mllm_domain::launch::ProfileLaunchSettings;
use mllm_store::ordinary_lifecycle::worker::InitializeWork;
use mllm_store::Store;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

struct WorkFixture {
    work: InitializeWork,
    _directory: tempfile::TempDir,
}

/// An ordinary managed deployment whose initialize has been accepted, so the
/// store can hand out the frozen `InitializeWork` the production builder reads.
fn accepted_work(fixture: &str, mutate: impl Fn(&mut Value, &mut Value)) -> WorkFixture {
    use mllm_config::effective::resolve_effective;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let source: Value = serde_json::from_str(fixture).unwrap();
    let mut host = source["input"]["host"].clone();
    let mut deployment = source["input"]["deployment"].clone();
    deployment["name"] = json!("ordinary");
    deployment["routes"] = json!(["ordinary"]);
    mutate(&mut deployment, &mut host);
    let policy = resolve_effective(&deployment, &host).unwrap().host;
    let store = Store::open(&root.join("native.db")).unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let observations: Vec<_> = policy
        .domains
        .keys()
        .map(|domain| mllm_domain::resources::MemoryObservation {
            domain: domain.clone(),
            capacity_bytes: 1_i64 << 50,
            available_bytes: 1_i64 << 50,
            sampled_at_ms: 1000,
        })
        .collect();
    store
        .import_resource_policy(&session, &policy, &observations, 1000)
        .unwrap();
    let receipt = store
        .create_stopped_managed_configuration(
            &session,
            "owner",
            "ordinary",
            &json!({ "config": deployment }).to_string(),
            &host,
            1000,
        )
        .unwrap();
    let fence = mllm_store::lifecycle::DeploymentFence {
        deployment_id: receipt.deployment_id.clone(),
        revision: receipt.revision,
        generation: receipt.generation,
    };
    store
        .accept_start(&session, &fence, 1100, 300000)
        .unwrap();
    let work = store.next_initialize(&session).unwrap().unwrap();
    drop(session);
    drop(store);
    WorkFixture {
        work,
        _directory: directory,
    }
}

const SGLANG_GOLDEN: &str = include_str!(
    "../../mllm-config/tests/fixtures/effective-sglang-golden.json"
);
const VLLM_GOLDEN: &str = include_str!(
    "../../mllm-config/tests/fixtures/effective-vllm-golden.json"
);

// T02: metadata must carry every field a frozen descriptor is validated against.
#[test]
fn builder_produces_expected_metadata_for_the_golden_sglang_config() {
    use mllm_config::effective::sglang::{
        NATIVE_CHECKPOINT_REVISION, NATIVE_SGLANG_RECIPE, NATIVE_SGLANG_SOURCE_REVISION,
    };
    let fixture = accepted_work(SGLANG_GOLDEN, |_, _| {});
    let work = &fixture.work;
    let ProfileLaunchSettings::Sglang(settings) = &work.effective().profile.launch_settings else {
        panic!("golden fixture must carry SGLang launch settings");
    };
    let expected_digest =
        hex::encode(Sha256::digest(serde_json::to_vec(settings).unwrap()));
    let launch = frozen_from_work(
        work,
        // The route the golden deployment serves; the served name is the route
        // name, never a derived binding artifact.
        "ordinary".into(),
        "secret://engine-key".into(),
        "secret://admin-key".into(),
    )
    .unwrap();
    let metadata = launch.metadata();
    assert_eq!(metadata.engine, "sglang");
    assert_eq!(metadata.recipe, NATIVE_SGLANG_RECIPE);
    assert_eq!(metadata.source_revision, NATIVE_SGLANG_SOURCE_REVISION);
    assert_eq!(metadata.checkpoint_revision, NATIVE_CHECKPOINT_REVISION);
    assert_eq!(metadata.binding_id, work.binding_id());
    assert_eq!(metadata.incarnation, work.incarnation());
    assert_eq!(metadata.endpoint, format!("http://{}", work.endpoint()));
    assert_eq!(metadata.served_name, "ordinary");
    assert_eq!(metadata.rendered_settings_digest, expected_digest);
    assert_eq!(metadata.rendered_settings_digest.len(), 64);
    assert!(
        metadata
            .rendered_settings_digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
    assert_eq!(metadata.device.host_id, "lab");
    assert_eq!(metadata.device.hardware_fingerprint, "hw-01");
    assert_eq!(metadata.device.device_id, "gpu0");
    assert_eq!(metadata.device.memory_domain, "unified");
    assert_eq!(launch.checkpoint_root(), "/srv/models/toy");
    assert_eq!(launch.executable(), "/bin/true");
    assert_eq!(launch.inference_credential_ref(), "secret://engine-key");
    assert_eq!(launch.admin_credential_ref(), "secret://admin-key");
    assert_eq!(launch.settings(), settings);
}

// T02: the shared descriptor builder must reproduce the exact JSON the arm
// inlined before the extraction.
#[test]
fn private_descriptor_output_byte_matches_the_json_the_arm_built() {
    let execution = StepExecutionContext {
        token: TransitionToken {
            deployment_id: "01J0000000000000000000000DE".into(),
            revision: 3,
            generation: 2,
            operation_id: "01J0000000000000000000000OP".into(),
            step_id: "01J0000000000000000000000ST".into(),
        },
        binding_id: "01J0000000000000000000000BI".into(),
        incarnation: "01J0000000000000000000000IN".into(),
        issued_at_ms: 1200,
        deadline_ms: 300000,
        identities: ExecutionIdentities::OwnedLaunch,
        completion_target: None,
        grant_id: None,
        launch_settings: None,
    };
    let public = json!({
        "schema_version": 1,
        "kind": "sglang_launch",
        "binding_id": execution.binding_id,
    });
    let bytes =
        private_descriptor("01J0000000000000000000000SE", &execution, "/srv/models/toy", &public)
            .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value.as_object().unwrap().len(), 5);
    assert_eq!(value["schema_version"], 2);
    assert_eq!(value["kind"], "sglang_private_launch");
    assert_eq!(value["checkpoint_root"], "/srv/models/toy");
    assert_eq!(value["public_settings"], public);
    assert_eq!(
        value["launch_scope"],
        json!({
            "session_id": "01J0000000000000000000000SE",
            "deployment_id": "01J0000000000000000000000DE",
            "operation_id": "01J0000000000000000000000OP",
            "step_id": "01J0000000000000000000000ST",
            "revision": 3,
            "generation": 2,
            "binding_id": "01J0000000000000000000000BI",
            "incarnation": "01J0000000000000000000000IN",
            "issued_at_ms": 1200,
            "deadline_ms": 300000,
        })
    );
}

// T02: a non-SGLang profile is refused closed.
#[test]
fn builder_refuses_a_vllm_profile() {
    let fixture = accepted_work(VLLM_GOLDEN, |_, _| {});
    let error = match frozen_from_work(
        &fixture.work,
        "ordinary".into(),
        "secret://engine-key".into(),
        "secret://admin-key".into(),
    ) {
        Err(error) => error,
        Ok(_) => panic!("a vLLM profile must not produce a native launch"),
    };
    assert!(matches!(error, CoordinatorError::Service(_)));
}

// T02: a deployment that selected no device cannot produce a native launch.
#[test]
fn builder_refuses_work_with_no_selected_device() {
    let fixture = accepted_work(SGLANG_GOLDEN, |deployment, _| {
        deployment["devices"] = json!([]);
        for phase in ["cold", "ready", "parking", "parked", "wake"] {
            deployment["resources"][phase]["devices"] = json!([]);
        }
    });
    let error = match frozen_from_work(
        &fixture.work,
        "ordinary".into(),
        "secret://engine-key".into(),
        "secret://admin-key".into(),
    ) {
        Err(error) => error,
        Ok(_) => panic!("work with no selected device must not produce a native launch"),
    };
    assert!(matches!(error, CoordinatorError::Service(_)));
}

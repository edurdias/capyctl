use super::*;
use crate::lifecycle::DeploymentFence;
use crate::managed_configuration::ManagedConfigurationReceipt;
use crate::Store;
use serde_json::{json, Value};

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const DIGEST: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

fn key() -> String {
    format!("sources/huggingface/Qwen--Qwen3-4B@{SHA}")
}

fn setup() -> (Store, CoordinatorSession, Value, Value) {
    let value: Value = serde_json::from_str(include_str!(
        "../../../mllm-config/tests/fixtures/f2-deployment.json"
    ))
    .unwrap();
    let (mut config, mut host) = (value["deployment"].clone(), value["host"].clone());
    let model = config["model"].as_object_mut().unwrap();
    model.remove("path");
    model.insert(
        "source".into(),
        json!({"huggingface": {"repo": "Qwen/Qwen3-4B", "revision": SHA}}),
    );
    host["model_sources"] = json!({"huggingface": "allowed", "max_bytes": "100GiB"});
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let effective = mllm_config::effective::resolve_effective(&config, &host).unwrap();
    store
        .import_resource_policy(
            &session,
            &effective.host,
            &[mllm_domain::resources::MemoryObservation {
                domain: "unified".into(),
                capacity_bytes: 64 << 30,
                available_bytes: 60 << 30,
                sampled_at_ms: 1,
            }],
            1,
        )
        .unwrap();
    (store, session, config, host)
}

fn deploy(
    store: &Store,
    session: &CoordinatorSession,
    key: &str,
    config: &Value,
    host: &Value,
) -> ManagedConfigurationReceipt {
    store
        .create_stopped_managed_configuration(
            session,
            "p",
            key,
            &json!({"config": config}).to_string(),
            host,
            1,
        )
        .unwrap()
}

fn fence(receipt: &ManagedConfigurationReceipt) -> DeploymentFence {
    DeploymentFence {
        deployment_id: receipt.deployment_id.clone(),
        revision: receipt.revision,
        generation: receipt.generation,
    }
}

fn report(state: SourceState, done: u64, total: u64, reason: Option<&str>) -> SourceReport {
    SourceReport {
        state,
        bytes_done: done,
        bytes_total: total,
        reason: reason.map(Into::into),
    }
}

// T14 (ADR 0008): a revision with a remote source starts pending; activation
// and the checkpoint digest wait for a verified copy; status shows progress.
#[test]
fn a_remote_source_gates_activation_and_the_digest_until_verified() {
    let (store, session, config, host) = setup();
    let receipt = deploy(&store, &session, "k", &config, &host);
    let id = receipt.deployment_id.clone();
    let records = store.model_sources(&id, 1).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].state, SourceState::Pending);
    assert_eq!(records[0].source_key, key());
    assert_eq!(store.pending_model_sources().unwrap().len(), 1);
    assert!(
        store.pending_checkpoint_digests().unwrap().is_empty(),
        "nothing to hash yet"
    );
    assert!(matches!(
        store.accept_start(&session, &fence(&receipt), 100, 100_100),
        Err(LifecycleError::ModelSourcePending)
    ));

    store
        .record_model_source(
            &session,
            &id,
            1,
            "lab",
            &key(),
            &report(SourceState::Downloading, 10, 100, None),
            2,
        )
        .unwrap();
    let status = serde_json::to_value(&store.snapshot().unwrap().deployments[0]).unwrap();
    assert_eq!(status["model_sources"][0]["state"], "downloading");
    assert_eq!(status["model_sources"][0]["bytes_done"], 10);
    assert_eq!(status["model_sources"][0]["bytes_total"], 100);

    store
        .record_model_source(
            &session,
            &id,
            1,
            "lab",
            &key(),
            &report(SourceState::Verified, 100, 100, None),
            3,
        )
        .unwrap();
    // A later answer never downgrades a verified copy.
    store
        .record_model_source(
            &session,
            &id,
            1,
            "lab",
            &key(),
            &report(SourceState::Pending, 0, 0, None),
            4,
        )
        .unwrap();
    assert_eq!(
        store.model_source(&id, 1, "lab").unwrap().unwrap().state,
        SourceState::Verified
    );
    assert!(store.pending_model_sources().unwrap().is_empty());
    assert_eq!(
        store.pending_checkpoint_digests().unwrap().len(),
        1,
        "now measurable"
    );
    store
        .accept_start(&session, &fence(&receipt), 100, 100_100)
        .unwrap();
    store
        .record_checkpoint_digest(&session, &id, 1, "lab", DIGEST, 7, 5)
        .unwrap();
    assert_eq!(store.referenced_model_sources().unwrap(), vec![key()]);
}

// T14 (ADR 0008): a terminal failure refuses activation and is not retried;
// a retryable one stays pending. Reasons are closed categories.
#[test]
fn a_terminal_failure_refuses_activation() {
    let (store, session, config, host) = setup();
    let receipt = deploy(&store, &session, "k", &config, &host);
    let id = receipt.deployment_id.clone();
    store
        .record_model_source(
            &session,
            &id,
            1,
            "lab",
            &key(),
            &report(SourceState::Failed, 0, 0, Some("network")),
            2,
        )
        .unwrap();
    assert_eq!(store.pending_model_sources().unwrap().len(), 1, "retryable");
    assert!(matches!(
        store.accept_start(&session, &fence(&receipt), 100, 100_100),
        Err(LifecycleError::ModelSourcePending)
    ));
    store
        .record_model_source(
            &session,
            &id,
            1,
            "lab",
            &key(),
            &report(SourceState::Failed, 0, 0, Some("hash_mismatch")),
            3,
        )
        .unwrap();
    assert!(
        store.pending_model_sources().unwrap().is_empty(),
        "terminal"
    );
    assert!(matches!(
        store.accept_start(&session, &fence(&receipt), 100, 100_100),
        Err(LifecycleError::ModelSourceFailed)
    ));
    let record = store.model_source(&id, 1, "lab").unwrap().unwrap();
    assert!(record.terminal);
    assert_eq!(record.reason.as_deref(), Some("hash_mismatch"));
    for bad in [
        report(SourceState::Failed, 0, 0, Some("token hf_abc leaked")),
        report(SourceState::Failed, 0, 0, None),
        report(SourceState::Verified, 1, 1, Some("network")),
    ] {
        assert!(store
            .record_model_source(&session, &id, 1, "lab", &key(), &bad, 4)
            .is_err());
    }
    // A host the revision did not resolve on, or another key, records nothing.
    assert!(store
        .record_model_source(
            &session,
            &id,
            1,
            "elsewhere",
            &key(),
            &report(SourceState::Pending, 0, 0, None),
            5
        )
        .is_err());
    assert!(store
        .record_model_source(
            &session,
            &id,
            1,
            "lab",
            "sources/http/x",
            &report(SourceState::Pending, 0, 0, None),
            5
        )
        .is_err());
}

// T14: a local source has no record and changes nothing.
#[test]
fn a_local_source_has_no_record() {
    let value: Value = serde_json::from_str(include_str!(
        "../../../mllm-config/tests/fixtures/f2-deployment.json"
    ))
    .unwrap();
    let store = Store::open_in_memory().unwrap();
    let session2 = store.begin_coordinator_session().unwrap();
    let effective =
        mllm_config::effective::resolve_effective(&value["deployment"], &value["host"]).unwrap();
    store
        .import_resource_policy(
            &session2,
            &effective.host,
            &[mllm_domain::resources::MemoryObservation {
                domain: "unified".into(),
                capacity_bytes: 64 << 30,
                available_bytes: 60 << 30,
                sampled_at_ms: 1,
            }],
            1,
        )
        .unwrap();
    let receipt = deploy(&store, &session2, "k", &value["deployment"], &value["host"]);
    assert!(store
        .model_sources(&receipt.deployment_id, 1)
        .unwrap()
        .is_empty());
    assert!(store.referenced_model_sources().unwrap().is_empty());
    store
        .accept_start(&session2, &fence(&receipt), 100, 100_100)
        .unwrap();
}

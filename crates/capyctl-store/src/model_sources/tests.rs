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
        "../../../capyctl-config/tests/fixtures/f2-deployment.json"
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
    let effective = capyctl_config::effective::resolve_effective(&config, &host).unwrap();
    store
        .import_resource_policy(
            &session,
            &effective.host,
            &[capyctl_domain::resources::MemoryObservation {
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
        store
            .model_source(&id, 1, "lab", &key())
            .unwrap()
            .unwrap()
            .state,
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

// T14 (ADR 0008): a new deployment naming a source already verified on its
// host (for a deployment that still exists) starts verified, so activation is
// admitted at once; a copy still downloading is not reused (found live
// 2026-09-25: `deploy --activate` was refused `model_source_pending`).
#[test]
fn a_copy_verified_on_the_host_is_reused_by_a_new_deployment() {
    let (store, session, config, host) = setup();
    let first = deploy(&store, &session, "k1", &config, &host);
    let mut second_config = config.clone();
    second_config["name"] = json!("second");
    if second_config.get("routes").is_some() {
        second_config["routes"] = json!(["second"]);
    }
    let second = deploy(&store, &session, "k2", &second_config, &host);
    assert_eq!(
        store
            .model_source(&second.deployment_id, 1, "lab", &key())
            .unwrap()
            .unwrap()
            .state,
        SourceState::Pending,
        "a copy still pending is not reused"
    );
    store
        .record_model_source(
            &session,
            &first.deployment_id,
            1,
            "lab",
            &key(),
            &report(SourceState::Verified, 100, 100, None),
            3,
        )
        .unwrap();
    let mut third_config = config.clone();
    third_config["name"] = json!("third");
    if third_config.get("routes").is_some() {
        third_config["routes"] = json!(["third"]);
    }
    let third = deploy(&store, &session, "k3", &third_config, &host);
    let record = store
        .model_source(&third.deployment_id, 1, "lab", &key())
        .unwrap()
        .unwrap();
    assert_eq!(record.state, SourceState::Verified);
    assert_eq!((record.bytes_done, record.bytes_total), (100, 100));
    store
        .accept_start(&session, &fence(&third), 100, 100_100)
        .unwrap();
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
    let record = store.model_source(&id, 1, "lab", &key()).unwrap().unwrap();
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
        "../../../capyctl-config/tests/fixtures/f2-deployment.json"
    ))
    .unwrap();
    let store = Store::open_in_memory().unwrap();
    let session2 = store.begin_coordinator_session().unwrap();
    let effective =
        capyctl_config::effective::resolve_effective(&value["deployment"], &value["host"]).unwrap();
    store
        .import_resource_policy(
            &session2,
            &effective.host,
            &[capyctl_domain::resources::MemoryObservation {
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

fn draft_key(fill: char) -> String {
    format!("sources/http/{}", fill.to_string().repeat(64))
}

/// The setup deployment with an http drafter beside its weights, and the
/// operator's vLLM speculation switch the drafter needs.
fn with_drafter(config: &Value, host: &Value, name: &str, fill: char) -> (Value, Value) {
    let (mut config, mut host) = (config.clone(), host.clone());
    config["name"] = json!(name);
    config["routes"] = json!([name]);
    config["model"]["draft"] = json!({"http": {
        "url": "https://drafts.example.test/d", "sha256": fill.to_string().repeat(64)}});
    config["engine_config"]["accept_extra_args"] = json!(true);
    config["engine_config"]["extra_args"] = json!([
        "--speculative-config",
        json!({"method": "draft_model", "num_speculative_tokens": 4}).to_string()
    ]);
    host["runtime_profiles"]["local"]["security"]["approved_options"] =
        json!(["--speculative-config"]);
    (config, host)
}

// T14 (ADR 0008 amendment 2026-10-08): a remote drafter is a second source
// of the revision on its host, recorded under its own key. Activation and the
// checkpoint digest wait until both copies are verified on one host; a
// terminal failure of the drafter refuses activation like the weights'; both
// copies count as referenced.
#[test]
fn a_remote_drafter_is_a_second_source_that_gates_activation() {
    let (store, session, config, host) = setup();
    let (config, host) = with_drafter(&config, &host, "drafted", 'a');
    let receipt = deploy(&store, &session, "k", &config, &host);
    let id = receipt.deployment_id.clone();
    let records = store.model_sources(&id, 1).unwrap();
    let keys: Vec<_> = records.iter().map(|r| r.source_key.clone()).collect();
    assert_eq!(keys, [draft_key('a'), key()]);
    assert!(records.iter().all(|r| r.state == SourceState::Pending));
    assert_eq!(store.pending_model_sources().unwrap().len(), 2);
    let verified = report(SourceState::Verified, 100, 100, None);
    store
        .record_model_source(&session, &id, 1, "lab", &key(), &verified, 2)
        .unwrap();
    assert!(matches!(
        store.accept_start(&session, &fence(&receipt), 100, 100_100),
        Err(LifecycleError::ModelSourcePending)
    ));
    assert!(
        store.pending_checkpoint_digests().unwrap().is_empty(),
        "the drafter's weights are sized with the checkpoint's"
    );
    store
        .record_model_source(
            &session,
            &id,
            1,
            "lab",
            &draft_key('a'),
            &report(SourceState::Downloading, 5, 10, None),
            3,
        )
        .unwrap();
    let status = serde_json::to_value(&store.snapshot().unwrap().deployments[0]).unwrap();
    assert_eq!(
        status["model_sources"].as_array().unwrap().len(),
        2,
        "{status}"
    );
    store
        .record_model_source(&session, &id, 1, "lab", &draft_key('a'), &verified, 4)
        .unwrap();
    assert_eq!(
        store
            .model_source(&id, 1, "lab", &draft_key('a'))
            .unwrap()
            .unwrap()
            .state,
        SourceState::Verified
    );
    assert!(store.pending_model_sources().unwrap().is_empty());
    assert_eq!(store.pending_checkpoint_digests().unwrap().len(), 1);
    store
        .accept_start(&session, &fence(&receipt), 100, 100_100)
        .unwrap();
    assert_eq!(
        store.referenced_model_sources().unwrap(),
        vec![draft_key('a'), key()]
    );
    // A key the revision does not name records nothing.
    assert!(store
        .record_model_source(&session, &id, 1, "lab", &draft_key('c'), &verified, 5)
        .is_err());

    // A drafter whose bytes do not match its pin refuses activation even
    // with the weights verified (here reused from the first deployment).
    let (config, host) = with_drafter(&config, &host, "mismatched", 'b');
    let receipt = deploy(&store, &session, "k2", &config, &host);
    let id = receipt.deployment_id.clone();
    assert_eq!(
        store
            .model_source(&id, 1, "lab", &key())
            .unwrap()
            .unwrap()
            .state,
        SourceState::Verified
    );
    store
        .record_model_source(
            &session,
            &id,
            1,
            "lab",
            &draft_key('b'),
            &report(SourceState::Failed, 0, 0, Some("hash_mismatch")),
            6,
        )
        .unwrap();
    assert!(matches!(
        store.accept_start(&session, &fence(&receipt), 100, 100_100),
        Err(LifecycleError::ModelSourceFailed)
    ));
}

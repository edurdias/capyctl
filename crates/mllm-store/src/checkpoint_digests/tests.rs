use super::*;
use crate::lifecycle::DeploymentFence;
use crate::managed_configuration::ManagedConfigurationReceipt;
use crate::Store;
use serde_json::{json, Value};

const DIGEST: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
const OTHER: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

fn setup() -> (Store, CoordinatorSession, Value, Value) {
    let value: Value = serde_json::from_str(include_str!(
        "../../../mllm-config/tests/fixtures/f2-deployment.json"
    ))
    .unwrap();
    let (config, host) = (value["deployment"].clone(), value["host"].clone());
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

fn frozen_memory(store: &Store, receipt: &ManagedConfigurationReceipt) -> Value {
    let raw: String = store
        .conn
        .query_row(
            "SELECT effective_json FROM effective_revisions WHERE deployment_id=?1 AND revision=?2",
            params![receipt.deployment_id, receipt.revision],
            |r| r.get(0),
        )
        .unwrap();
    serde_json::from_str::<Value>(&raw).unwrap()["engine_config"]["memory"].clone()
}

// T14: every accepted revision starts pending and status says so; a declared
// memory request activates on first placement without waiting.
#[test]
fn an_accepted_revision_is_pending_until_a_host_measures_it() {
    let (store, session, config, host) = setup();
    let receipt = deploy(&store, &session, "k", &config, &host);
    let record = store
        .checkpoint_digest(&receipt.deployment_id, receipt.revision)
        .unwrap()
        .unwrap();
    assert_eq!(record.state, DigestState::Pending);
    assert!(!record.provisional);
    // `sha256:model` is a label, not a digest: it expects nothing.
    assert_eq!(record.expected, None);
    let status = store.snapshot().unwrap();
    let deployment = &status.deployments[0];
    assert_eq!(
        serde_json::to_value(deployment).unwrap()["checkpoint_digest"]["state"],
        "pending"
    );
    let pending = store.pending_checkpoint_digests().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].host_id, "lab");
    store
        .accept_start(&session, &fence(&receipt), 100, 100_100)
        .unwrap();
    assert_eq!(
        store
            .record_checkpoint_digest(&session, &receipt.deployment_id, 1, "lab", DIGEST, 7, 2)
            .unwrap(),
        RecordOutcome::Recorded {
            digest: DIGEST.into(),
            weights_bytes: 7
        }
    );
    assert_eq!(
        store
            .recorded_checkpoint(&receipt.deployment_id, 1)
            .unwrap()
            .as_deref(),
        Some(DIGEST)
    );
    assert!(store.pending_checkpoint_digests().unwrap().is_empty());
    // A declared request is not rewritten: both sides keep resolving it
    // without weights, as the launch plan says.
    assert!(frozen_memory(&store, &receipt)
        .get("weights_bytes")
        .is_none_or(Value::is_null));
}

// T34: a declared digest is the expectation; once recorded, another host's
// different measurement is a mismatch that changes nothing recorded.
#[test]
fn a_declared_or_recorded_digest_refuses_any_other_measurement() {
    let (store, session, mut config, host) = setup();
    config["model"]["content_fingerprint"] = json!(DIGEST);
    let receipt = deploy(&store, &session, "declared", &config, &host);
    let id = &receipt.deployment_id;
    assert_eq!(
        store
            .checkpoint_digest(id, 1)
            .unwrap()
            .unwrap()
            .expected
            .as_deref(),
        Some(DIGEST)
    );
    assert_eq!(
        store
            .record_checkpoint_digest(&session, id, 1, "lab", OTHER, 7, 2)
            .unwrap(),
        RecordOutcome::Mismatch
    );
    assert!(matches!(
        store.accept_start(&session, &fence(&receipt), 100, 100_100),
        Err(LifecycleError::CheckpointMismatch)
    ));
    assert_eq!(store.recorded_checkpoint(id, 1).unwrap(), None);

    let mut second = config.clone();
    second["name"] = json!("second");
    second["routes"] = json!(["second"]);
    let receipt = deploy(&store, &session, "second", &second, &host);
    let id = &receipt.deployment_id;
    let recorded = RecordOutcome::Recorded {
        digest: DIGEST.into(),
        weights_bytes: 7,
    };
    assert_eq!(
        store
            .record_checkpoint_digest(&session, id, 1, "lab", DIGEST, 7, 2)
            .unwrap(),
        recorded
    );
    assert_eq!(
        store
            .record_checkpoint_digest(&session, id, 1, "lab", DIGEST, 7, 3)
            .unwrap(),
        recorded
    );
    // ADR 0014 §7: only a host the revision resolved on may report for it. A
    // host outside that set is refused and changes nothing.
    assert!(matches!(
        store.record_checkpoint_digest(&session, id, 1, "other", OTHER, 7, 4),
        Err(CheckpointDigestError::UnresolvedHost)
    ));
    assert_eq!(
        store.recorded_checkpoint(id, 1).unwrap().as_deref(),
        Some(DIGEST)
    );
    store
        .accept_start(&session, &fence(&receipt), 100, 100_100)
        .unwrap();
}

// T14 (P2): a memory request derived from the weights is accepted
// provisional, waits for the digest, and is then resolved exactly with the
// measured weights; the frozen revision and its receipt stay valid.
#[test]
fn a_derived_memory_request_waits_for_the_digest_then_resolves_exactly() {
    let (store, session, mut config, host) = setup();
    config.as_object_mut().unwrap().remove("resources");
    config["engine_config"] = json!({"memory": {"kv_cache": "4GiB"}});
    let receipt = deploy(&store, &session, "derived", &config, &host);
    let id = &receipt.deployment_id;
    let record = store.checkpoint_digest(id, 1).unwrap().unwrap();
    assert!(record.provisional);
    assert!(matches!(
        store.accept_start(&session, &fence(&receipt), 100, 100_100),
        Err(LifecycleError::CheckpointDigestPending)
    ));
    let weights: i64 = 2 << 30;
    store
        .record_checkpoint_digest(&session, id, 1, "lab", DIGEST, weights, 2)
        .unwrap();
    let memory = frozen_memory(&store, &receipt);
    assert_eq!(memory["weights_bytes"], json!(weights));
    assert_eq!(
        memory["request_bytes"],
        json!(weights + (4 << 30) + mllm_config::effective::SGLANG_OVERHEAD_MARGIN_BYTES)
    );
    assert!(!store.checkpoint_digest(id, 1).unwrap().unwrap().provisional);
    // The retried deploy is the same command and the same receipt.
    assert_eq!(deploy(&store, &session, "derived", &config, &host), receipt);
    store
        .accept_start(&session, &fence(&receipt), 100, 100_100)
        .unwrap();
}

// T14 (P2): measured weights that leave no KV cache in a declared request make the
// revision unusable; it never starts.
#[test]
fn weights_that_do_not_fit_a_declared_request_make_the_revision_unusable() {
    let (store, session, mut config, host) = setup();
    config.as_object_mut().unwrap().remove("resources");
    config["engine_config"] = json!({"memory": {"request": "10GiB"}});
    let receipt = deploy(&store, &session, "tight", &config, &host);
    let id = &receipt.deployment_id;
    assert_eq!(
        store
            .record_checkpoint_digest(&session, id, 1, "lab", DIGEST, 4 << 30, 2)
            .unwrap(),
        RecordOutcome::Unusable
    );
    let record = store.checkpoint_digest(id, 1).unwrap().unwrap();
    assert_eq!(record.state, DigestState::Unusable);
    assert!(record.diagnostic.is_some());
    assert!(matches!(
        store.accept_start(&session, &fence(&receipt), 100, 100_100),
        Err(LifecycleError::CheckpointMismatch)
    ));
}

// T26 (discrete GPU design §11): found live on the 16 GB discrete-GPU laptop
// host. A minimal deployment whose request, derived from the measured
// weights, is larger than the card was accepted provisionally, turned
// unusable once measured, and every start answered `checkpoint_mismatch`
// (exit 2) instead of `insufficient_device_memory` (exit 4). The refusal the
// re-resolution met is kept and a start is refused with it.
#[test]
fn a_derived_request_larger_than_the_card_refuses_the_start_with_its_code() {
    let value: Value = serde_json::from_str(include_str!(
        "../../../mllm-config/tests/fixtures/f2-deployment.json"
    ))
    .unwrap();
    let (mut config, mut host) = (value["deployment"].clone(), value["host"].clone());
    host["resource_policy"]["domains"] = json!({
        "system": {"memory": "distinct", "managed_limit": "24GiB", "free_reserve": "8GiB",
                   "parked_limit": "16GiB", "host_kv_limit": "4GiB"},
        "gpu0": {"memory": "device", "device": "gpu0", "managed_limit": "14848MiB",
                 "free_reserve": "1536MiB", "parked_limit": "2GiB"}
    });
    host["resource_policy"]["devices"] = json!({"gpu0": {"domain": "gpu0", "sharing": "shared"}});
    config.as_object_mut().unwrap().remove("resources");
    config["residency"] = json!("deep");
    config["engine_config"] = json!({"memory": {"kv_cache": "12GiB"}});
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    // The host policy, from a resolvable deployment on the same host.
    let mut sized = config.clone();
    sized["engine_config"] = json!({"memory": {"request": "10GiB", "kv_cache": "2GiB"}});
    let effective = mllm_config::effective::resolve_effective(&sized, &host).unwrap();
    let observation = |domain: &str, gib: i64| mllm_domain::resources::MemoryObservation {
        domain: domain.into(),
        capacity_bytes: gib << 30,
        available_bytes: gib << 30,
        sampled_at_ms: 1,
    };
    store
        .import_resource_policy(
            &session,
            &effective.host,
            &[observation("gpu0", 16), observation("system", 64)],
            1,
        )
        .unwrap();
    let receipt = deploy(&store, &session, "big", &config, &host);
    let id = &receipt.deployment_id;
    assert_eq!(
        store
            .record_checkpoint_digest(&session, id, 1, "lab", DIGEST, 8 << 30, 2)
            .unwrap(),
        RecordOutcome::Unusable
    );
    let diagnostic = store
        .checkpoint_digest(id, 1)
        .unwrap()
        .unwrap()
        .diagnostic
        .unwrap();
    assert!(
        diagnostic.starts_with("insufficient_device_memory:"),
        "{diagnostic}"
    );
    match store.accept_start(&session, &fence(&receipt), 100, 100_100) {
        Err(LifecycleError::CheckpointUnusable(reason)) => {
            assert!(
                reason.starts_with("insufficient_device_memory:"),
                "{reason}"
            )
        }
        other => panic!("expected the device refusal, got {other:?}"),
    }
}

// T33, owner decision 2026-09-22: a revision accepted before WE3 has no
// digest row; it starts, and its first placement records the digest.
#[test]
fn a_revision_accepted_before_we3_records_on_first_placement() {
    let (store, session, config, host) = setup();
    let receipt = deploy(&store, &session, "legacy", &config, &host);
    let id = &receipt.deployment_id;
    store
        .conn
        .execute("DELETE FROM checkpoint_digests", [])
        .unwrap();
    assert!(store.checkpoint_digest(id, 1).unwrap().is_none());
    assert!(store.pending_checkpoint_digests().unwrap().is_empty());
    assert!(store.snapshot().unwrap().deployments[0]
        .checkpoint_digest
        .is_none());
    store
        .accept_start(&session, &fence(&receipt), 100, 100_100)
        .unwrap();
    store
        .record_checkpoint_digest(&session, id, 1, "lab", DIGEST, 7, 2)
        .unwrap();
    assert_eq!(
        store.recorded_checkpoint(id, 1).unwrap().as_deref(),
        Some(DIGEST)
    );
}

// T34 T37: refusal diagnostics are closed categories; writes are session-fenced.
#[test]
fn refusals_keep_only_closed_categories_and_writes_are_fenced() {
    let (store, session, config, host) = setup();
    let receipt = deploy(&store, &session, "k", &config, &host);
    let id = &receipt.deployment_id;
    store
        .note_checkpoint_digest_refusal(&session, id, 1, "unsafe_file", 2)
        .unwrap();
    assert_eq!(
        store
            .checkpoint_digest(id, 1)
            .unwrap()
            .unwrap()
            .diagnostic
            .as_deref(),
        Some("unsafe_file")
    );
    assert!(matches!(
        store.note_checkpoint_digest_refusal(&session, id, 1, "/srv/models/toy", 2),
        Err(CheckpointDigestError::Invalid)
    ));
    for (digest, weights) in [("sha256:model", 1), (DIGEST, -1)] {
        assert!(matches!(
            store.record_checkpoint_digest(&session, id, 1, "lab", digest, weights, 2),
            Err(CheckpointDigestError::Invalid)
        ));
    }
    let newer = store.begin_coordinator_session().unwrap();
    assert!(matches!(
        store.record_checkpoint_digest(&session, id, 1, "lab", DIGEST, 7, 2),
        Err(CheckpointDigestError::StaleSession)
    ));
    store
        .record_checkpoint_digest(&newer, id, 1, "lab", DIGEST, 7, 2)
        .unwrap();
}

// ADR 0014 §7, SPEC §8: a digest is a host's measurement of the checkpoint the
// revision was resolved with there. A report from a host the revision did not
// resolve on (a stale, misrouted or foreign report) records nothing, not even
// on a pending revision, and cannot turn it into a mismatch.
// T14 T23
#[test]
fn a_digest_from_a_host_the_revision_did_not_resolve_on_is_refused() {
    let (store, session, config, host) = setup();
    let receipt = deploy(&store, &session, "k", &config, &host);
    let id = &receipt.deployment_id;
    for digest in [DIGEST, OTHER] {
        assert!(matches!(
            store.record_checkpoint_digest(&session, id, 1, "stranger", digest, 7, 2),
            Err(CheckpointDigestError::UnresolvedHost)
        ));
    }
    let record = store.checkpoint_digest(id, 1).unwrap().unwrap();
    assert_eq!(record.state, DigestState::Pending);
    assert_eq!(record.host_id, "lab");
    // The revision's own host still records it.
    assert_eq!(
        store
            .record_checkpoint_digest(&session, id, 1, "lab", DIGEST, 7, 3)
            .unwrap(),
        RecordOutcome::Recorded {
            digest: DIGEST.into(),
            weights_bytes: 7
        }
    );
}

//! Schema v19 against real pre-E1 state.
//!
//! The fixture is built by today's helpers and then written back into the exact
//! pre-E1 shape a store held before WE1: the effective revision carries the
//! profile's `launch_settings` (values from the pre-E1 vLLM golden, `git show
//! HEAD:crates/mllm-config/tests/fixtures/effective-vllm-golden.json`) and that
//! golden's recipe fingerprint, the binding is identified by that fingerprint,
//! the retained source has no `engine_config`, and every digest over them is
//! re-sealed. WE1 cannot read it; v19 must carry it forward so the Ready launch
//! can be adopted, stopped and cleaned up with nothing released on the way.

use super::super::tests::{armed_ordinary, identity};
use super::super::*;
use super::substitute;
use crate::snapshot::DeploymentSnapshot;
use mllm_domain::completion::{CleanupEvidence, ProcessIdentity};
use serde_json::{json, Value};

/// The pre-E1 vLLM golden's recipe fingerprint.
const LEGACY_FINGERPRINT: &str = "751975f79429dadf0676247ee9a5c46c656bc07b374011841123128ad58871ce";

fn pre_e1_launch_settings() -> Value {
    let golden: Value = serde_json::from_str(include_str!(
        "../../../../mllm-config/tests/fixtures/pre-e1-effective-vllm.json"
    ))
    .unwrap();
    golden["profile"]["launch_settings"].clone()
}

fn group() -> Vec<ProcessIdentity> {
    vec![identity("api", 71), identity("worker-0", 72)]
}

fn ready(
    remote: bool,
) -> (
    crate::Store,
    CoordinatorSession,
    DeploymentFence,
    StepExecutionContext,
) {
    let (store, session, fence, execution) = armed_ordinary();
    if remote {
        store
            .conn
            .execute_batch(&format!(
                "INSERT OR IGNORE INTO enrolled_hosts(host_id,host_name,key_digest) VALUES('lab','lab','digest');
                 INSERT INTO remote_binding_ingress VALUES('{}','lab','http://100.64.0.1:9443');",
                execution.binding_id
            ))
            .unwrap();
    }
    let step = execution.token.step_id.clone();
    let now = execution.issued_at_ms + 5;
    store
        .record_owned_launch(
            &session,
            &step,
            &OwnedLaunchReceipt {
                binding_id: execution.binding_id.clone(),
                incarnation: execution.incarnation.clone(),
                identities: group(),
                observed_at_ms: now,
                receipt: "owned launch observed".into(),
            },
            now,
        )
        .unwrap();
    let ttl = store.observation_ttl_for_step(&step).unwrap();
    store
        .complete_step(
            &session,
            &step,
            &CompletionEvidence {
                token: execution.token.clone(),
                identities: group(),
                observed_at_ms: now,
                control_receipt: Some("model list names the route".into()),
                milestones: vec![
                    mllm_domain::completion::Milestone::AllocationsRestored,
                    mllm_domain::completion::Milestone::WeightsUsable,
                    mllm_domain::completion::Milestone::CacheValid,
                    mllm_domain::completion::Milestone::ModelUsable,
                ],
            },
            now,
            ttl,
        )
        .unwrap();
    (store, session, fence, execution)
}

fn text(store: &crate::Store, sql: &str, id: &str) -> String {
    store.conn.query_row(sql, [id], |r| r.get(0)).unwrap()
}

/// Write the fixture's state back into the pre-E1 shape and rewind the schema
/// to v18, so reopening applies v19 to it exactly as an upgrade would.
fn downgrade_to_pre_e1(
    store: &crate::Store,
    deployment: &str,
    adjust: impl FnOnce(&mut Value),
) -> (String, Value) {
    let current = text(
        store,
        "SELECT effective_json FROM effective_revisions WHERE deployment_id=?1",
        deployment,
    );
    let fingerprint = text(
        store,
        "SELECT fingerprint FROM effective_revisions WHERE deployment_id=?1",
        deployment,
    );
    let mut legacy: Value = serde_json::from_str(&current).unwrap();
    legacy.as_object_mut().unwrap().remove("engine_config");
    let profile = legacy["profile"].as_object_mut().unwrap();
    profile.insert("launch_settings".into(), pre_e1_launch_settings());
    for field in ["extra_args", "approved_options", "approved_paths"] {
        profile["security"].as_object_mut().unwrap().remove(field);
    }
    legacy["recipe_fingerprint"] = json!(LEGACY_FINGERPRINT);
    adjust(&mut legacy);
    let legacy_text = legacy.to_string();
    let source: Value = serde_json::from_str(&text(
        store,
        "SELECT config_json FROM managed_configuration_sources WHERE deployment_id=?1",
        deployment,
    ))
    .unwrap();
    let mut legacy_source = source.clone();
    legacy_source
        .as_object_mut()
        .unwrap()
        .remove("engine_config");
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate).unwrap();
    substitute(&tx, &current, &legacy_text).unwrap();
    tx.execute(
        "UPDATE effective_revisions SET effective_json=?2,fingerprint=?3 WHERE deployment_id=?1",
        params![deployment, legacy_text, LEGACY_FINGERPRINT],
    )
    .unwrap();
    for (table, column) in [
        ("runtime_bindings", "binding_json"),
        ("lifecycle_steps", "step_json"),
    ] {
        tx.execute(
            &format!("UPDATE {table} SET {column}=replace({column},?1,?2)"),
            params![fingerprint, LEGACY_FINGERPRINT],
        )
        .unwrap();
    }
    tx.execute(
        "UPDATE managed_configuration_sources SET config_json=?2 WHERE deployment_id=?1",
        params![deployment, legacy_source.to_string()],
    )
    .unwrap();
    crate::managed_configuration::reseal_migrated_receipt(
        &tx,
        deployment,
        1,
        &legacy_text,
        Some(&legacy_source),
        None,
    )
    .unwrap();
    super::super::receipt::reseal_start_receipts(&tx, deployment).unwrap();
    tx.execute_batch(
        "DELETE FROM schema_migrations WHERE version>=19;
         DROP TABLE engine_config_migrations; DROP TABLE host_publication_migrations;
         DROP TABLE checkpoint_digests;",
    )
    .unwrap();
    tx.commit().unwrap();
    (legacy_text, legacy_source)
}

fn upgrade(store: &crate::Store) {
    crate::migrations::apply(&store.conn).unwrap();
}

fn owns(store: &crate::Store, deployment: &str) -> bool {
    store
        .resource_snapshot()
        .unwrap()
        .owners
        .contains_key(deployment)
}

fn deployment_status(store: &crate::Store, deployment: &str) -> DeploymentSnapshot {
    store
        .snapshot()
        .unwrap()
        .deployments
        .into_iter()
        .find(|d| d.id == deployment)
        .unwrap()
}

fn stop_and_clean_up(
    store: &crate::Store,
    session: &CoordinatorSession,
    fence: &DeploymentFence,
    execution: &StepExecutionContext,
) {
    let step = execution.token.step_id.clone();
    let ttl = store.observation_ttl_for_step(&step).unwrap();
    let now = execution.issued_at_ms + 10_000;
    let receipt = store
        .accept_ordinary_stop_command(
            session,
            "owner",
            &fence.deployment_id,
            fence.revision,
            "stop-after-upgrade",
            now,
            now + 50_000,
        )
        .unwrap();
    let (_, context) = store
        .arm_ordinary_cleanup_with_context(session, &receipt.step_id, now + 1)
        .unwrap();
    assert_eq!(context.unwrap().identities, group());
    store
        .complete_cleanup(
            session,
            &receipt.step_id,
            &CleanupEvidence {
                binding_id: execution.binding_id.clone(),
                incarnation: execution.incarnation.clone(),
                identities: group(),
                observed_at_ms: now + 2,
                receipt: "every recorded process gone".into(),
            },
            now + 3,
            ttl,
        )
        .unwrap();
    assert!(!owns(store, &fence.deployment_id));
}

/// W11 after an upgrade: a Ready embedded launch recorded before E1 is carried
/// forward, keeps its binding identity, is adopted by the restarted standalone
/// role, re-proven, and stopped with cleanup on gone evidence. Before v19 the
/// same state cannot be read at all.
// T33 T14 T32
#[test]
fn a_pre_e1_ready_embedded_launch_is_migrated_adopted_and_stopped() {
    let (store, old, fence, execution) = ready(false);
    let step = execution.token.step_id.clone();
    let binding_before = text(
        &store,
        "SELECT binding_json FROM runtime_bindings WHERE id=?1",
        &execution.binding_id,
    );
    let (legacy_text, legacy_source) = downgrade_to_pre_e1(&store, &fence.deployment_id, |_| {});
    let binding_legacy = text(
        &store,
        "SELECT binding_json FROM runtime_bindings WHERE id=?1",
        &execution.binding_id,
    );
    assert_ne!(binding_before, binding_legacy);
    assert!(binding_legacy.contains(&format!("declared:{LEGACY_FINGERPRINT}")));
    // WE1 alone reads the pre-E1 launch as corrupt: it can be neither
    // resumed nor adopted.
    assert!(store.initialize_execution(&old, &step).is_err());
    let probe = store.begin_coordinator_session().unwrap();
    assert!(store.retired_local_launches(&probe).unwrap().is_empty());
    assert!(owns(&store, &fence.deployment_id));

    upgrade(&store);
    let (outcome, legacy, fingerprint, stored_legacy): (String, String, String, String) = store
        .conn
        .query_row(
            "SELECT outcome,legacy_fingerprint,fingerprint,legacy_effective_json FROM engine_config_migrations WHERE deployment_id=?1",
            [&fence.deployment_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(outcome, "migrated");
    assert_eq!(legacy, LEGACY_FINGERPRINT);
    assert_ne!(fingerprint, LEGACY_FINGERPRINT);
    assert_eq!(stored_legacy, legacy_text);
    let migrated: Value = serde_json::from_str(&text(
        &store,
        "SELECT effective_json FROM effective_revisions WHERE deployment_id=?1",
        &fence.deployment_id,
    ))
    .unwrap();
    assert!(migrated["profile"].get("launch_settings").is_none());
    assert_eq!(
        migrated["engine_config"]["memory"]["kv_cache_bytes"],
        4_i64 << 30
    );
    assert_eq!(migrated["engine_config"]["block_size_tokens"], 16);
    let source: Value = serde_json::from_str(&text(
        &store,
        "SELECT config_json FROM managed_configuration_sources WHERE deployment_id=?1",
        &fence.deployment_id,
    ))
    .unwrap();
    assert_eq!(
        source["engine_config"],
        json!({"kv_cache_dtype": "auto", "vllm": {"block_size_tokens": 16}, "memory": {"kv_cache": "4294967296B"}})
    );
    // Ownership is untouched: same binding bytes, same identity, same reservation.
    assert_eq!(
        text(
            &store,
            "SELECT binding_json FROM runtime_bindings WHERE id=?1",
            &execution.binding_id
        ),
        binding_legacy
    );
    assert!(owns(&store, &fence.deployment_id));
    assert!(deployment_status(&store, &fence.deployment_id)
        .operator_action
        .is_none());

    // The original command, retried after the upgrade, is the same command.
    let replayed = store
        .create_stopped_managed_configuration(
            &probe,
            "principal",
            "key",
            &json!({"config": legacy_source}).to_string(),
            &json!({}),
            20,
        )
        .unwrap();
    assert_eq!(replayed.deployment_id, fence.deployment_id);

    // W11: the restarted role adopts the launch and can reverify and stop it.
    let session = store.begin_coordinator_session().unwrap();
    let retired = store.retired_local_launches(&session).unwrap();
    assert_eq!(retired.len(), 1);
    assert_eq!(retired[0].work.step_id(), step);
    store.adopt_retired_local_launch(&session, &step).unwrap();
    let listed = store.local_ready_launches(&session).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].identities, group());
    let context = store.initialize_execution(&session, &step).unwrap();
    assert_eq!(context.binding_id, execution.binding_id);
    stop_and_clean_up(&store, &session, &fence, &execution);
}

/// W0 after an upgrade: a Ready remote launch recorded before E1 is adopted by
/// the restarted controller and stopped with cleanup.
// T33 T34 T32
#[test]
fn a_pre_e1_ready_remote_launch_is_migrated_adopted_and_stopped() {
    let (store, _old, fence, execution) = ready(true);
    let step = execution.token.step_id.clone();
    downgrade_to_pre_e1(&store, &fence.deployment_id, |_| {});
    let probe = store.begin_coordinator_session().unwrap();
    assert!(store.retired_remote_launches(&probe).unwrap().is_empty());
    upgrade(&store);
    let session = store.begin_coordinator_session().unwrap();
    let retired = store.retired_remote_launches(&session).unwrap();
    assert_eq!(retired.len(), 1);
    assert!(retired[0].completed);
    store.adopt_retired_remote_launch(&session, &step).unwrap();
    assert!(owns(&store, &fence.deployment_id));
    stop_and_clean_up(&store, &session, &fence, &execution);
}

/// A pre-E1 setting with no E1 mapping is refused, not approximated. The
/// revision keeps its bytes, the launch keeps its binding and reservation, and
/// status names the operator action; nothing is adopted or released.
// T33 T32
#[test]
fn an_unmappable_pre_e1_launch_is_refused_and_keeps_everything_it_owns() {
    let (store, _old, fence, execution) = ready(false);
    let (legacy_text, _) = downgrade_to_pre_e1(&store, &fence.deployment_id, |legacy| {
        legacy["profile"]["launch_settings"]["tensor_parallel_size"] = json!(2);
    });
    let step_before = text(
        &store,
        "SELECT step_json FROM lifecycle_steps WHERE id=?1",
        &execution.token.step_id,
    );
    upgrade(&store);
    let (outcome, diagnostic): (String, String) = store
        .conn
        .query_row(
            "SELECT outcome,diagnostic FROM engine_config_migrations WHERE deployment_id=?1",
            [&fence.deployment_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(outcome, "refused");
    assert!(
        diagnostic.contains("tensor_parallel_size is 2"),
        "{diagnostic}"
    );
    assert_eq!(
        text(
            &store,
            "SELECT effective_json FROM effective_revisions WHERE deployment_id=?1",
            &fence.deployment_id
        ),
        legacy_text
    );
    assert_eq!(
        text(
            &store,
            "SELECT step_json FROM lifecycle_steps WHERE id=?1",
            &execution.token.step_id
        ),
        step_before
    );
    let status = deployment_status(&store, &fence.deployment_id);
    let action = status
        .operator_action
        .expect("status names the operator action");
    assert!(
        action.contains("tensor_parallel_size") && action.contains("engine_config"),
        "{action}"
    );
    let journaled: String = store
        .conn
        .query_row(
            "SELECT evidence FROM journal_entries WHERE state='engine_config_migration_refused'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(journaled.contains("retained"), "{journaled}");
    let session = store.begin_coordinator_session().unwrap();
    assert!(store.retired_local_launches(&session).unwrap().is_empty());
    assert!(owns(&store, &fence.deployment_id));
    let live: String = text(
        &store,
        "SELECT state FROM runtime_bindings WHERE id=?1",
        &execution.binding_id,
    );
    assert_eq!(live, "live");
}

/// A stored host publication loses only its retired settings, and its
/// fingerprint is recomputed so the publication reads again.
// T33
#[test]
fn a_pre_e1_host_publication_is_stripped_and_refingerprinted() {
    let store = crate::Store::open_in_memory().unwrap();
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../mllm-config/tests/fixtures/f2-deployment.json"
    ))
    .unwrap();
    let mut document: Value =
        serde_json::from_str(&mllm_config::remote_roles::HostConfig::template(
            std::path::Path::new("/home/operator/host"),
        ))
        .unwrap();
    let mut profile = fixture["host"]["runtime_profiles"]["local"].clone();
    document["runtime_profiles"]["local"] = profile.clone();
    let edited = mllm_config::remote_roles::HostConfig::parse(&document.to_string()).unwrap();
    profile["launch_settings"] = json!({"engine": "vllm", "tensor_parallel_size": 1});
    document["runtime_profiles"]["local"] = profile;
    let legacy_fingerprint = mllm_config::remote_resources::policy_fingerprint(&document);
    store
        .conn
        .execute_batch("INSERT INTO enrolled_hosts(host_id,host_name,key_digest) VALUES('spark','spark','digest')")
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO approved_host_publications VALUES('spark',?1,'boot',?2,1)",
            params![document.to_string(), legacy_fingerprint],
        )
        .unwrap();
    assert!(store.host_publication("spark").is_err());
    store
        .conn
        .execute_batch(
            "DELETE FROM schema_migrations WHERE version>=19;
             DROP TABLE engine_config_migrations; DROP TABLE host_publication_migrations;
             DROP TABLE checkpoint_digests;",
        )
        .unwrap();
    upgrade(&store);
    let publication = store.host_publication("spark").unwrap().unwrap();
    // Exactly the fingerprint the host publishes once its YAML is edited.
    assert_eq!(
        publication.fingerprint,
        mllm_config::remote_resources::policy_fingerprint(&edited.document)
    );
    let (legacy, kept): (String, String) = store
        .conn
        .query_row(
            "SELECT legacy_fingerprint,legacy_config_json FROM host_publication_migrations WHERE host_id='spark'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(legacy, legacy_fingerprint);
    assert!(kept.contains("launch_settings"));
}

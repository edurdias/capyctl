use super::*;
use crate::Store;

/// Rows are inserted directly so these tests exercise the read contract itself —
/// ordering, incarnation matching and step state — rather than a lifecycle path
/// that could mask a wrong query by never producing the shapes it must reject.
fn seed(store: &Store) {
    store
        .conn
        .execute_batch(
            "INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version) \
               VALUES('dep-1','d','model','ready',1,0,1,1);
             INSERT INTO runtime_bindings(id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state) \
               VALUES('bind-1','dep-1',1,'inc-1','managed','{}','[]','live');
             INSERT INTO operations(id,deployment_id,kind,state) VALUES('op-1','dep-1','park','running');
             INSERT INTO lifecycle_runs(operation_id,deployment_id,revision,generation,session_id,action,state,deadline_ms,plan_json) \
               VALUES('op-1','dep-1',1,1,'sess-1','park','running',9999,'{}');",
        )
        .unwrap();
}

fn step(store: &Store, id: &str, ordinal: i64, state: &str) {
    store
        .conn
        .execute(
            "INSERT INTO lifecycle_steps(id,operation_id,ordinal,deployment_id,binding_id,session_id,state,step_json) \
             VALUES(?1,'op-1',?2,'dep-1','bind-1','sess-1',?3,'{}')",
            (id, ordinal, state),
        )
        .unwrap();
}

fn evidence(store: &Store, step_id: &str, epoch: i64, facts: &str) {
    store
        .conn
        .execute(
            "INSERT INTO lifecycle_evidence(step_id,evidence_json,committed_epoch) VALUES(?1,?2,?3)",
            (
                step_id,
                format!(r#"{{"version":1,"milestones":[{facts}]}}"#),
                epoch,
            ),
        )
        .unwrap();
}

#[test]
fn milestones_are_returned_in_commit_order_not_ordinal_order() {
    let store = Store::open_in_memory().unwrap();
    seed(&store);
    step(&store, "s-1", 0, "completed");
    step(&store, "s-2", 1, "completed");
    // The later ordinal committed first; commit order is what actually happened.
    evidence(&store, "s-2", 10, r#""allocations_restored""#);
    evidence(&store, "s-1", 20, r#""weights_usable""#);
    assert_eq!(
        store.committed_milestones("bind-1", "inc-1").unwrap(),
        vec![Milestone::AllocationsRestored, Milestone::WeightsUsable]
    );
}

#[test]
fn only_completed_steps_contribute_milestones() {
    for state in ["planned", "armed", "uncertain", "cancelled"] {
        let store = Store::open_in_memory().unwrap();
        seed(&store);
        step(&store, "s-1", 0, state);
        evidence(&store, "s-1", 1, r#""memory_released""#);
        assert!(
            store.committed_milestones("bind-1", "inc-1").unwrap().is_empty(),
            "a {state} step has decided nothing"
        );
    }
}

#[test]
fn a_stale_incarnation_is_not_answered_with_current_history() {
    let store = Store::open_in_memory().unwrap();
    seed(&store);
    step(&store, "s-1", 0, "completed");
    evidence(&store, "s-1", 1, r#""allocations_restored""#);
    assert!(matches!(
        store.committed_milestones("bind-1", "inc-0"),
        Err(LifecycleError::NotFound)
    ));
    assert!(matches!(
        store.committed_milestones("bind-0", "inc-1"),
        Err(LifecycleError::NotFound)
    ));
}

#[test]
fn every_milestone_variant_round_trips_from_stored_evidence() {
    let store = Store::open_in_memory().unwrap();
    seed(&store);
    step(&store, "s-1", 0, "completed");
    evidence(
        &store,
        "s-1",
        1,
        r#""quiesced","memory_released","allocations_restored","weights_usable","cache_valid","model_usable""#,
    );
    assert_eq!(
        store.committed_milestones("bind-1", "inc-1").unwrap(),
        vec![
            Milestone::Quiesced,
            Milestone::MemoryReleased,
            Milestone::AllocationsRestored,
            Milestone::WeightsUsable,
            Milestone::CacheValid,
            Milestone::ModelUsable,
        ]
    );
}

#[test]
fn unreadable_evidence_is_corrupt_rather_than_empty() {
    let store = Store::open_in_memory().unwrap();
    seed(&store);
    step(&store, "s-1", 0, "completed");
    store
        .conn
        .execute(
            "INSERT INTO lifecycle_evidence(step_id,evidence_json,committed_epoch) VALUES('s-1','{\"milestones\":[\"teleported\"]}',1)",
            (),
        )
        .unwrap();
    assert!(matches!(
        store.committed_milestones("bind-1", "inc-1"),
        Err(LifecycleError::CorruptStoredData)
    ));
}

#[test]
fn empty_identifiers_are_rejected() {
    let store = Store::open_in_memory().unwrap();
    seed(&store);
    assert!(matches!(
        store.committed_milestones("", "inc-1"),
        Err(LifecycleError::Invalid)
    ));
    assert!(matches!(
        store.committed_milestones("bind-1", ""),
        Err(LifecycleError::Invalid)
    ));
    assert!(matches!(
        store.outstanding_requests(""),
        Err(LifecycleError::Invalid)
    ));
}

#[test]
fn uncertain_leases_count_as_outstanding_work() {
    let store = Store::open_in_memory().unwrap();
    seed(&store);
    assert_eq!(store.outstanding_requests("dep-1").unwrap(), 0);
    for (id, disposition) in [("r-1", "inflight"), ("r-2", "uncertain")] {
        store
            .conn
            .execute(
                "INSERT INTO request_leases(id,deployment_id,revision,generation,session_id,disposition) \
                 VALUES(?1,'dep-1',1,1,'sess-1',?2)",
                (id, disposition),
            )
            .unwrap();
    }
    assert_eq!(
        store.outstanding_requests("dep-1").unwrap(),
        2,
        "uncertain work is not proven terminated"
    );
}

/// Commands fence on the effective revision, which is not the row's schema version.
/// A caller that confuses them gets a revision conflict rather than an obvious
/// error, so the two must be readable apart.
#[test]
fn the_effective_revision_is_distinct_from_the_schema_version() {
    let store = Store::open_in_memory().unwrap();
    seed(&store);
    store
        .conn
        .execute("UPDATE deployments SET revision=7, schema_version=1", ())
        .unwrap();
    assert_eq!(store.current_revision("dep-1").unwrap(), Some(7));
    let row = store.get_deployment("dep-1").unwrap().unwrap();
    assert_eq!(row.schema_version, 1, "the two are different numbers");
    assert_eq!(store.current_revision("missing").unwrap(), None);
}

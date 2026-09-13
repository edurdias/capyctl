use super::*;
use mllm_domain::completion::{CompletionEvidence, Milestone, OwnedLaunchReceipt, ProcessIdentity};

fn receipt(context: &mllm_domain::completion::StepExecutionContext) -> OwnedLaunchReceipt {
    OwnedLaunchReceipt {
        binding_id: context.binding_id.clone(),
        incarnation: context.incarnation.clone(),
        identities: vec![
            ProcessIdentity {
                role: "api".into(),
                pid: 41,
                boot_id: "fake-owned-boot".into(),
                start_ticks: 12,
            },
            ProcessIdentity {
                role: "worker-0".into(),
                pid: 42,
                boot_id: "fake-owned-boot".into(),
                start_ticks: 13,
            },
        ],
        observed_at_ms: 1250,
        receipt: "complete fake owned membership".into(),
    }
}
fn evidence(
    context: &mllm_domain::completion::StepExecutionContext,
    receipt: &OwnedLaunchReceipt,
) -> CompletionEvidence {
    CompletionEvidence {
        token: context.token.clone(),
        identities: receipt.identities.clone(),
        observed_at_ms: 1300,
        control_receipt: Some("fake model usable".into()),
        milestones: vec![
            Milestone::AllocationsRestored,
            Milestone::WeightsUsable,
            Milestone::CacheValid,
            Milestone::ModelUsable,
        ],
    }
}
#[test]
fn ready_retains_cold_and_closed_gates_and_replays_after_restart() {
    let (store, s, c) = created("fake");
    let r = store
        .accept_candidate_initialize(&s, "owner", c.run_id(), "init", BODY, 1100)
        .unwrap();
    arm(&store, &s, r.step_id()).unwrap();
    let context = store
        .candidate_initialize_execution(&s, r.step_id())
        .unwrap();
    let receipt = receipt(&context);
    let e = evidence(&context, &receipt);
    let before = store.resource_snapshot().unwrap();
    let ttl = store
        .resource_policy("lab")
        .unwrap()
        .unwrap()
        .controls
        .observation_ttl_ms;
    assert!(store.complete_step(&s, r.step_id(), &e, 1300, ttl).is_err());
    store
        .record_owned_launch(&s, r.step_id(), &receipt, 1250)
        .unwrap();
    store.complete_step(&s, r.step_id(), &e, 1300, ttl).unwrap();
    let after = store.resource_snapshot().unwrap();
    assert_eq!(after.owners, before.owners);
    assert_eq!(after.epoch, before.epoch + 1);
    let state:(String,String,i64,i64)=store.conn.query_row("SELECT desired_state,observed_state,admission_enabled,dispatch_enabled FROM deployments WHERE id=?1",[c.deployment_id()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
    assert_eq!(state, ("stopped".into(), "ready".into(), 0, 0));
    assert_eq!(
        store
            .candidate_run_snapshot("owner", c.run_id())
            .unwrap()
            .unwrap()
            .state(),
        crate::candidate_creation::CandidateRunState::Running
    );
    let s2 = store.begin_coordinator_session().unwrap();
    store
        .complete_step(&s2, r.step_id(), &e, 900000, 1)
        .unwrap();
    store
        .record_owned_launch(&s2, r.step_id(), &receipt, 900000)
        .unwrap();
    assert_eq!(store.resource_snapshot().unwrap().epoch, after.epoch);
    assert!(store.complete_step(&s, r.step_id(), &e, 1300, ttl).is_err());
}

#[path = "../../candidate_creation/cleanup/tests.rs"]
mod cleanup_tests;

#[test]
fn association_rejects_incomplete_changed_and_unfresh_collectors_atomically() {
    let (store, s, c) = created("fake");
    let r = store
        .accept_candidate_initialize(&s, "owner", c.run_id(), "init", BODY, 1100)
        .unwrap();
    arm(&store, &s, r.step_id()).unwrap();
    let context = store
        .candidate_initialize_execution(&s, r.step_id())
        .unwrap();
    let good = receipt(&context);
    let ttl = store
        .resource_policy("lab")
        .unwrap()
        .unwrap()
        .controls
        .observation_ttl_ms;
    for case in 0..15 {
        let mut bad = good.clone();
        let mut now = 1250;
        match case {
            0 => bad.identities.clear(),
            1 => {
                bad.identities.pop();
            }
            2 => bad.identities[1].role = "api".into(),
            3 => bad.identities[1].pid = 41,
            4 => bad.identities[1].boot_id = "other-boot".into(),
            5 => bad.identities[1].start_ticks = 0,
            6 => bad.identities[0].pid = 0,
            7 => bad.identities[1].role = "worker-1".into(),
            8 => {
                let mut extra = bad.identities[1].clone();
                extra.role = "worker-2".into();
                extra.pid = 43;
                bad.identities.push(extra);
            }
            9 => bad.receipt = " \n\t".into(),
            10 => bad.observed_at_ms = 1199,
            11 => bad.observed_at_ms = 1251,
            12 => {
                bad.observed_at_ms = 400001;
                now = 400001;
            }
            13 => now = 1251 + ttl,
            14 => bad.incarnation = ulid::Ulid::new().to_string(),
            _ => unreachable!(),
        }
        let before = durable(&store);
        assert!(
            store
                .record_owned_launch(&s, r.step_id(), &bad, now)
                .is_err(),
            "case {case}"
        );
        assert_eq!(durable(&store), before, "case {case}");
    }
    store
        .record_owned_launch(&s, r.step_id(), &good, 1250)
        .unwrap();
    let before = durable(&store);
    let mut reordered = good.clone();
    reordered.identities.reverse();
    store
        .record_owned_launch(&s, r.step_id(), &reordered, 900000)
        .unwrap();
    assert_eq!(durable(&store), before);
    let mut changed = good.clone();
    changed.identities[1].start_ticks += 1;
    assert!(store
        .record_owned_launch(&s, r.step_id(), &changed, 1250)
        .is_err());
    assert_eq!(durable(&store), before);
    store
        .record_api_identity(
            &s,
            &crate::lifecycle::DeploymentFence {
                deployment_id: c.deployment_id().into(),
                revision: 1,
                generation: 1,
            },
            c.binding_id(),
            &good.identities[0],
        )
        .unwrap();
    assert_eq!(durable(&store), before);
}

#[test]
fn completion_rejects_each_token_identity_milestone_and_ttl_mutation_without_release() {
    let (store, s, c) = created("fake");
    let r = store
        .accept_candidate_initialize(&s, "owner", c.run_id(), "init", BODY, 1100)
        .unwrap();
    arm(&store, &s, r.step_id()).unwrap();
    let context = store
        .candidate_initialize_execution(&s, r.step_id())
        .unwrap();
    let receipt = receipt(&context);
    store
        .record_owned_launch(&s, r.step_id(), &receipt, 1250)
        .unwrap();
    let good = evidence(&context, &receipt);
    let ttl = store
        .resource_policy("lab")
        .unwrap()
        .unwrap()
        .controls
        .observation_ttl_ms;
    for case in 0..16 {
        let mut bad = good.clone();
        let mut supplied_ttl = ttl;
        let mut now = 1300;
        match case {
            0 => bad.token.deployment_id = "wrong".into(),
            1 => bad.token.revision += 1,
            2 => bad.token.generation += 1,
            3 => bad.token.operation_id = "wrong".into(),
            4 => bad.token.step_id = "wrong".into(),
            5 => bad.token.qualification_id = "wrong".into(),
            6 => bad.identities[1].start_ticks += 1,
            7 => bad.milestones.reverse(),
            8 => bad.control_receipt = None,
            9 => bad.control_receipt = Some(" \n".into()),
            10 => bad.observed_at_ms = 1199,
            11 => bad.observed_at_ms = 1301,
            12 => now = 400001,
            13 => supplied_ttl += 1,
            14 => supplied_ttl -= 1,
            15 => now = 1301 + ttl,
            _ => unreachable!(),
        }
        let before = durable(&store);
        assert!(
            store
                .complete_step(&s, r.step_id(), &bad, now, supplied_ttl)
                .is_err(),
            "case {case}"
        );
        assert_eq!(durable(&store), before, "case {case}");
    }
    store
        .complete_step(&s, r.step_id(), &good, 1300, ttl)
        .unwrap();
    let before = durable(&store);
    let mut reordered = good.clone();
    reordered.identities.reverse();
    store
        .complete_step(&s, r.step_id(), &reordered, 999999, 1)
        .unwrap();
    assert_eq!(durable(&store), before);
    let mut changed = good.clone();
    changed.control_receipt = Some("different valid receipt".into());
    assert!(matches!(
        store.complete_step(&s, r.step_id(), &changed, 1300, ttl),
        Err(LifecycleError::Conflict)
    ));
    assert_eq!(durable(&store), before);
}

#[test]
fn association_and_completion_event_failure_roll_back_all_tables() {
    let (store, s, c) = created("fake");
    let r = store
        .accept_candidate_initialize(&s, "owner", c.run_id(), "init", BODY, 1100)
        .unwrap();
    arm(&store, &s, r.step_id()).unwrap();
    let context = store
        .candidate_initialize_execution(&s, r.step_id())
        .unwrap();
    let receipt = receipt(&context);
    let e = evidence(&context, &receipt);
    let ttl = store
        .resource_policy("lab")
        .unwrap()
        .unwrap()
        .controls
        .observation_ttl_ms;
    store.conn.execute_batch("CREATE TEMP TRIGGER fail_event BEFORE INSERT ON management_events BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    let before = durable(&store);
    assert!(store
        .record_owned_launch(&s, r.step_id(), &receipt, 1250)
        .is_err());
    assert_eq!(durable(&store), before);
    store
        .conn
        .execute_batch("DROP TRIGGER fail_event;")
        .unwrap();
    store
        .record_owned_launch(&s, r.step_id(), &receipt, 1250)
        .unwrap();
    store.conn.execute_batch("CREATE TEMP TRIGGER fail_event BEFORE INSERT ON management_events BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    let before = durable(&store);
    assert!(store.complete_step(&s, r.step_id(), &e, 1300, ttl).is_err());
    assert_eq!(durable(&store), before);
}

#[test]
fn strict_history_rejects_duplicate_fields_oversize_and_missing_evidence() {
    let (store, s, c) = created("fake");
    let r = store
        .accept_candidate_initialize(&s, "owner", c.run_id(), "init", BODY, 1100)
        .unwrap();
    arm(&store, &s, r.step_id()).unwrap();
    let context = store
        .candidate_initialize_execution(&s, r.step_id())
        .unwrap();
    let receipt = receipt(&context);
    store
        .record_owned_launch(&s, r.step_id(), &receipt, 1250)
        .unwrap();
    let e = evidence(&context, &receipt);
    let ttl = store
        .resource_policy("lab")
        .unwrap()
        .unwrap()
        .controls
        .observation_ttl_ms;
    store.complete_step(&s, r.step_id(), &e, 1300, ttl).unwrap();
    let original: String = store
        .conn
        .query_row(
            "SELECT evidence_json FROM lifecycle_evidence WHERE step_id=?1",
            [r.step_id()],
            |r| r.get(0),
        )
        .unwrap();
    for bad in [
        original.replacen('{', "{\"version\":1,", 1),
        " ".repeat(1048577),
        "{}".into(),
    ] {
        store
            .conn
            .execute(
                "UPDATE lifecycle_evidence SET evidence_json=?1 WHERE step_id=?2",
                params![bad, r.step_id()],
            )
            .unwrap();
        let before = durable(&store);
        assert!(matches!(
            store.complete_step(&s, r.step_id(), &e, 1300, ttl),
            Err(LifecycleError::CorruptStoredData)
        ));
        assert_eq!(durable(&store), before);
    }
    store
        .conn
        .execute(
            "DELETE FROM lifecycle_evidence WHERE step_id=?1",
            [r.step_id()],
        )
        .unwrap();
    assert!(matches!(
        store.complete_step(&s, r.step_id(), &e, 1300, ttl),
        Err(LifecycleError::CorruptStoredData)
    ));
}

#[test]
fn new_completion_uses_current_policy_and_rejects_corrupt_claims_and_restart() {
    let (store, s, c) = created("fake");
    let r = store
        .accept_candidate_initialize(&s, "owner", c.run_id(), "init", BODY, 1100)
        .unwrap();
    arm(&store, &s, r.step_id()).unwrap();
    let context = store
        .candidate_initialize_execution(&s, r.step_id())
        .unwrap();
    let receipt = receipt(&context);
    store
        .record_owned_launch(&s, r.step_id(), &receipt, 1250)
        .unwrap();
    let e = evidence(&context, &receipt);
    let policy = store.resource_policy("lab").unwrap().unwrap();
    let old = policy.controls.observation_ttl_ms;
    let mut next = policy.controls.clone();
    next.observation_ttl_ms = old + 1;
    let observations: Vec<_> = next
        .domains
        .keys()
        .map(|domain| mllm_domain::resources::MemoryObservation {
            domain: domain.clone(),
            capacity_bytes: 1_i64 << 50,
            available_bytes: 1_i64 << 50,
            sampled_at_ms: 1300,
        })
        .collect();
    store
        .update_resource_policy(&s, "owner", "lab", 1, "new-ttl", &next, &observations, 1300)
        .unwrap();
    let before = durable(&store);
    assert!(store.complete_step(&s, r.step_id(), &e, 1300, old).is_err());
    assert_eq!(durable(&store), before);
    store
        .conn
        .execute(
            "UPDATE lifecycle_claims SET generation=2 WHERE operation_id=?1",
            [r.operation_id()],
        )
        .unwrap();
    let before = durable(&store);
    assert!(store
        .complete_step(&s, r.step_id(), &e, 1300, old + 1)
        .is_err());
    assert_eq!(durable(&store), before);
    store
        .conn
        .execute(
            "UPDATE lifecycle_claims SET generation=1 WHERE operation_id=?1",
            [r.operation_id()],
        )
        .unwrap();
    let newer = store.begin_coordinator_session().unwrap();
    let before = durable(&store);
    assert!(store
        .complete_step(&newer, r.step_id(), &e, 1300, old + 1)
        .is_err());
    assert_eq!(durable(&store), before);
}

#[test]
fn stored_association_rejects_duplicate_fields_and_oversized_decode() {
    let (store, s, c) = created("fake");
    let r = store
        .accept_candidate_initialize(&s, "owner", c.run_id(), "init", BODY, 1100)
        .unwrap();
    arm(&store, &s, r.step_id()).unwrap();
    let context = store
        .candidate_initialize_execution(&s, r.step_id())
        .unwrap();
    let receipt = receipt(&context);
    let mut oversized = receipt.clone();
    oversized.receipt = "x".repeat(1048577);
    let before = durable(&store);
    assert!(store
        .record_owned_launch(&s, r.step_id(), &oversized, 1250)
        .is_err());
    assert_eq!(durable(&store), before);
    store
        .record_owned_launch(&s, r.step_id(), &receipt, 1250)
        .unwrap();
    let original: String = store
        .conn
        .query_row(
            "SELECT association_json FROM owned_launch_associations WHERE step_id=?1",
            [r.step_id()],
            |r| r.get(0),
        )
        .unwrap();
    store
        .conn
        .execute_batch("PRAGMA ignore_check_constraints=ON;")
        .unwrap();
    for raw in [
        original.replacen('{', "{\"version\":1,", 1),
        " ".repeat(1048577),
    ] {
        store
            .conn
            .execute(
                "UPDATE owned_launch_associations SET association_json=?1 WHERE step_id=?2",
                params![raw, r.step_id()],
            )
            .unwrap();
        assert!(matches!(
            store.record_owned_launch(&s, r.step_id(), &receipt, 1250),
            Err(LifecycleError::CorruptStoredData)
        ));
    }
}

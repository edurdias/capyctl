use super::*;
use mllm_domain::completion::CleanupEvidence;

#[test]
fn unarmed_inspection_successor_retains_prior_termination_history() {
    let (store, s, c) = created("fake");
    let init = store
        .accept_candidate_initialize(&s, "owner", c.run_id(), "init", BODY, 1100)
        .unwrap();
    arm(&store, &s, init.step_id()).unwrap();
    let context = store
        .candidate_initialize_execution(&s, init.step_id())
        .unwrap();
    let owned = receipt(&context);
    store
        .record_owned_launch(&s, init.step_id(), &owned, 1250)
        .unwrap();
    let body = r#"{"expected_revision":1,"action":"cleanup","deadline_ms":12000}"#;
    let first = store
        .accept_candidate_cleanup(&s, "owner", c.run_id(), "cleanup-a", body, 2000)
        .unwrap();
    store
        .arm_candidate_cleanup(&s, first.step_id(), 2100)
        .unwrap();
    let second_session = store.begin_coordinator_session().unwrap();
    let second = store
        .accept_candidate_cleanup(
            &second_session,
            "owner",
            c.run_id(),
            "cleanup-b",
            body,
            2300,
        )
        .unwrap();
    let third_session = store.begin_coordinator_session().unwrap();
    let third = store
        .accept_candidate_cleanup(&third_session, "owner", c.run_id(), "cleanup-c", body, 2400)
        .unwrap();
    store
        .arm_candidate_cleanup(&third_session, third.step_id(), 2500)
        .unwrap();
    let execution = store
        .candidate_cleanup_execution(&third_session, third.step_id())
        .unwrap();
    assert_eq!(
        execution.mode,
        crate::candidate_creation::cleanup::CleanupMode::InspectOwnedGone
    );
    // The immutable reader must also reject a forged downgrade after acceptance.
    let original: String = store
        .conn
        .query_row(
            "SELECT step_json FROM lifecycle_steps WHERE id=?1",
            [third.step_id()],
            |r| r.get(0),
        )
        .unwrap();
    let mut downgraded: Value = serde_json::from_str(&original).unwrap();
    downgraded["planned"]["mode"] = json!("terminate_owned");
    store
        .conn
        .execute(
            "UPDATE lifecycle_steps SET step_json=?1 WHERE id=?2",
            params![downgraded.to_string(), third.step_id()],
        )
        .unwrap();
    assert!(matches!(
        store.candidate_cleanup_execution(&third_session, third.step_id()),
        Err(LifecycleError::CorruptStoredData)
    ));
    store
        .conn
        .execute(
            "UPDATE lifecycle_steps SET step_json=?1 WHERE id=?2",
            params![original, third.step_id()],
        )
        .unwrap();
    let evidence = CleanupEvidence {
        binding_id: owned.binding_id,
        incarnation: owned.incarnation,
        identities: owned.identities,
        observed_at_ms: 2600,
        receipt: "all owned members gone".into(),
    };
    let ttl = store
        .resource_policy("lab")
        .unwrap()
        .unwrap()
        .controls
        .observation_ttl_ms;
    store
        .complete_cleanup(&third_session, third.step_id(), &evidence, 2600, ttl)
        .unwrap();
    for step in [init.step_id(), first.step_id(), second.step_id()] {
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT state FROM lifecycle_steps WHERE id=?1",
                    [step],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "cancelled"
        );
    }
    assert!(store.resource_snapshot().unwrap().owners.is_empty());
}

#[test]
fn cleanup_reader_rejects_corrupt_predecessor_claim_generation() {
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
    let cleanup = store
        .accept_candidate_cleanup(
            &s,
            "owner",
            c.run_id(),
            "cleanup",
            r#"{"expected_revision":1,"action":"cleanup","deadline_ms":12000}"#,
            2000,
        )
        .unwrap();
    let raw: String = store
        .conn
        .query_row(
            "SELECT plan_json FROM lifecycle_runs WHERE operation_id=?1",
            [cleanup.operation_id()],
            |r| r.get(0),
        )
        .unwrap();
    let mut plan: Value = serde_json::from_str(&raw).unwrap();
    plan["handoffs"][0]["claims"][0]["generation"] = json!(0);
    store
        .conn
        .execute(
            "UPDATE lifecycle_runs SET plan_json=?1 WHERE operation_id=?2",
            params![plan.to_string(), cleanup.operation_id()],
        )
        .unwrap();
    assert!(matches!(
        store.arm_candidate_cleanup(&s, cleanup.step_id(), 2100),
        Err(LifecycleError::CorruptStoredData)
    ));
}
#[test]
fn real_cleanup_releases_once_and_preserves_ready_replay() {
    let (store, s, c) = created("fake");
    let r = store
        .accept_candidate_initialize(&s, "owner", c.run_id(), "init", BODY, 1100)
        .unwrap();
    arm(&store, &s, r.step_id()).unwrap();
    let context = store
        .candidate_initialize_execution(&s, r.step_id())
        .unwrap();
    let receipt = receipt(&context);
    let ready = evidence(&context, &receipt);
    let ttl = store
        .resource_policy("lab")
        .unwrap()
        .unwrap()
        .controls
        .observation_ttl_ms;
    store
        .record_owned_launch(&s, r.step_id(), &receipt, 1250)
        .unwrap();
    store
        .complete_step(&s, r.step_id(), &ready, 1300, ttl)
        .unwrap();
    let before = store.resource_snapshot().unwrap().epoch;
    let body = r#"{"expected_revision":1,"action":"cleanup","deadline_ms":12000}"#;
    let cleanup = store
        .accept_candidate_cleanup(&s, "owner", c.run_id(), "cleanup", body, 2000)
        .unwrap();
    assert_eq!(cleanup.generation(), 2);
    assert!(matches!(
        store
            .arm_candidate_cleanup(&s, cleanup.step_id(), 2100)
            .unwrap(),
        ArmResult::New { .. }
    ));
    let execution = store
        .candidate_cleanup_execution(&s, cleanup.step_id())
        .unwrap();
    assert_eq!(execution.identities, receipt.identities);
    let e = CleanupEvidence {
        binding_id: receipt.binding_id.clone(),
        incarnation: receipt.incarnation.clone(),
        identities: receipt.identities.clone(),
        observed_at_ms: 2200,
        receipt: "complete owned fake disappearance".into(),
    };
    store
        .complete_cleanup(&s, cleanup.step_id(), &e, 2200, ttl)
        .unwrap();
    let after = store.resource_snapshot().unwrap();
    assert!(after.owners.is_empty());
    assert_eq!(after.epoch, before + 1);
    assert_eq!(
        store
            .conn
            .query_row("SELECT COUNT(*) FROM endpoint_leases", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let s2 = store.begin_coordinator_session().unwrap();
    store
        .complete_cleanup(&s2, cleanup.step_id(), &e, 999999, 1)
        .unwrap();
    store
        .complete_step(&s2, r.step_id(), &ready, 999999, 1)
        .unwrap();
    store
        .record_owned_launch(&s2, r.step_id(), &receipt, 999999)
        .unwrap();
    assert_eq!(
        store
            .accept_candidate_cleanup(&s2, "owner", c.run_id(), "cleanup", body, 999999)
            .unwrap(),
        cleanup
    );
    assert_eq!(store.resource_snapshot().unwrap().epoch, after.epoch);
    assert_eq!(
        store
            .accept_candidate_initialize(&s2, "owner", c.run_id(), "init", BODY, 999999)
            .unwrap()
            .operation_id(),
        r.operation_id()
    );
}

#[test]
fn cleanup_recovery_uses_inspection_after_armed_uncertainty_and_resolves_predecessors() {
    for already_armed in [false, true] {
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
        let body = r#"{"expected_revision":1,"action":"cleanup","deadline_ms":12000}"#;
        let first = store
            .accept_candidate_cleanup(&s, "owner", c.run_id(), "first", body, 2000)
            .unwrap();
        if already_armed {
            store
                .arm_candidate_cleanup(&s, first.step_id(), 2100)
                .unwrap();
        }
        assert!(store
            .accept_candidate_cleanup(&s, "owner", c.run_id(), "different", body, 2200)
            .is_err());
        let s2 = store.begin_coordinator_session().unwrap();
        let next = store
            .accept_candidate_cleanup(&s2, "owner", c.run_id(), "next", body, 2300)
            .unwrap();
        store
            .arm_candidate_cleanup(&s2, next.step_id(), 2400)
            .unwrap();
        let execution = store
            .candidate_cleanup_execution(&s2, next.step_id())
            .unwrap();
        assert_eq!(
            execution.mode,
            if already_armed {
                crate::candidate_creation::cleanup::CleanupMode::InspectOwnedGone
            } else {
                crate::candidate_creation::cleanup::CleanupMode::TerminateOwned
            }
        );
        assert!(store
            .candidate_cleanup_execution(&s2, first.step_id())
            .is_err());
        let e = CleanupEvidence {
            binding_id: receipt.binding_id.clone(),
            incarnation: receipt.incarnation.clone(),
            identities: receipt.identities.clone(),
            observed_at_ms: 2500,
            receipt: "fresh complete inspection".into(),
        };
        let ttl = store
            .resource_policy("lab")
            .unwrap()
            .unwrap()
            .controls
            .observation_ttl_ms;
        store
            .complete_cleanup(&s2, next.step_id(), &e, 2500, ttl)
            .unwrap();
        assert!(store.resource_snapshot().unwrap().owners.is_empty());
        let states: Vec<String> = store
            .conn
            .prepare("SELECT state FROM lifecycle_steps WHERE id IN (?1,?2)")
            .unwrap()
            .query_map(params![first.step_id(), r.step_id()], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(states, vec!["cancelled", "cancelled"]);
        store
            .record_owned_launch(&s2, r.step_id(), &receipt, 999999)
            .unwrap();
    }
}

#[test]
fn cleanup_permission_survives_revocation_and_original_run_expiry() {
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
    let (_, _, mut policy) = fixture("fake");
    policy.qualification_policy = None;
    store.import_qualification_policy(&s, &policy).unwrap();
    let body = r#"{"expected_revision":1,"action":"cleanup","deadline_ms":560001}"#;
    let cleanup = store
        .accept_candidate_cleanup(&s, "owner", c.run_id(), "cleanup", body, 500001)
        .unwrap();
    store
        .arm_candidate_cleanup(&s, cleanup.step_id(), 500002)
        .unwrap();
    let e = CleanupEvidence {
        binding_id: receipt.binding_id,
        incarnation: receipt.incarnation,
        identities: receipt.identities,
        observed_at_ms: 500003,
        receipt: "gone".into(),
    };
    let ttl = store
        .resource_policy("lab")
        .unwrap()
        .unwrap()
        .controls
        .observation_ttl_ms;
    store
        .complete_cleanup(&s, cleanup.step_id(), &e, 500003, ttl)
        .unwrap();
}

#[test]
fn cleanup_rejects_wrong_membership_and_rolls_back_event_failure() {
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
    let body = r#"{"expected_revision":1,"action":"cleanup","deadline_ms":12000}"#;
    store.conn.execute_batch("CREATE TEMP TRIGGER fail_event BEFORE INSERT ON management_events BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    let before = durable(&store);
    assert!(store
        .accept_candidate_cleanup(&s, "owner", c.run_id(), "cleanup", body, 2000)
        .is_err());
    assert_eq!(durable(&store), before);
    store
        .conn
        .execute_batch("DROP TRIGGER fail_event;")
        .unwrap();
    let cleanup = store
        .accept_candidate_cleanup(&s, "owner", c.run_id(), "cleanup", body, 2000)
        .unwrap();
    store.conn.execute_batch("CREATE TEMP TRIGGER fail_event BEFORE INSERT ON management_events BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    let before = durable(&store);
    assert!(store
        .arm_candidate_cleanup(&s, cleanup.step_id(), 2100)
        .is_err());
    assert_eq!(durable(&store), before);
    store
        .conn
        .execute_batch("DROP TRIGGER fail_event;")
        .unwrap();
    store
        .arm_candidate_cleanup(&s, cleanup.step_id(), 2100)
        .unwrap();
    let e = CleanupEvidence {
        binding_id: receipt.binding_id,
        incarnation: receipt.incarnation,
        identities: receipt.identities,
        observed_at_ms: 2200,
        receipt: "gone".into(),
    };
    let ttl = store
        .resource_policy("lab")
        .unwrap()
        .unwrap()
        .controls
        .observation_ttl_ms;
    for case in 0..6 {
        let mut bad = e.clone();
        let mut now = 2200;
        match case {
            0 => {
                bad.identities.pop();
            }
            1 => bad.identities[1].start_ticks += 1,
            2 => bad.receipt = " ".into(),
            3 => bad.observed_at_ms = 2099,
            4 => now = 2201 + ttl,
            5 => bad.incarnation = ulid::Ulid::new().to_string(),
            _ => unreachable!(),
        }
        let before = durable(&store);
        assert!(store
            .complete_cleanup(&s, cleanup.step_id(), &bad, now, ttl)
            .is_err());
        assert_eq!(durable(&store), before);
    }
    store.conn.execute_batch("CREATE TEMP TRIGGER fail_event BEFORE INSERT ON management_events BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    let before = durable(&store);
    assert!(store
        .complete_cleanup(&s, cleanup.step_id(), &e, 2200, ttl)
        .is_err());
    assert_eq!(durable(&store), before);
}

#[test]
fn cleanup_rejects_overlong_overflow_duplicate_commands_and_missing_membership() {
    let (store, s, c) = created("fake");
    let r = store
        .accept_candidate_initialize(&s, "owner", c.run_id(), "init", BODY, 1100)
        .unwrap();
    arm(&store, &s, r.step_id()).unwrap();
    let body = r#"{"expected_revision":1,"action":"cleanup","deadline_ms":12000}"#;
    let before = durable(&store);
    assert!(store
        .accept_candidate_cleanup(&s, "owner", c.run_id(), "cleanup", body, 2000)
        .is_err());
    assert_eq!(durable(&store), before);
    let context = store
        .candidate_initialize_execution(&s, r.step_id())
        .unwrap();
    let receipt = receipt(&context);
    store
        .record_owned_launch(&s, r.step_id(), &receipt, 1250)
        .unwrap();
    for (bad,now) in [(r#"{"expected_revision":1,"action":"cleanup","deadline_ms":62001}"#.to_string(),2000),(r#"{"expected_revision":1,"expected_revision":1,"action":"cleanup","deadline_ms":12000}"#.to_string(),2000),(format!("{{\"expected_revision\":1,\"action\":\"cleanup\",\"deadline_ms\":{}}}",i64::MAX),i64::MAX-1),(body.into(),12000)] {let before=durable(&store);assert!(store.accept_candidate_cleanup(&s,"owner",c.run_id(),"cleanup",&bad,now).is_err());assert_eq!(durable(&store),before);}
    let accepted = store
        .accept_candidate_cleanup(&s, "owner", c.run_id(), "cleanup", body, 2000)
        .unwrap();
    let before = durable(&store);
    assert_eq!(
        store
            .accept_candidate_cleanup(&s, "owner", c.run_id(), "cleanup", body, 999999)
            .unwrap(),
        accepted
    );
    assert_eq!(durable(&store), before);
    assert!(store
        .accept_candidate_cleanup(
            &s,
            "owner",
            c.run_id(),
            "cleanup",
            &body.replace("12000", "12001"),
            2100
        )
        .is_err());
}

#[test]
fn verified_cleanup_settles_old_leases_only_for_single_historical_binding() {
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
    store
        .conn
        .execute(
            "INSERT INTO request_leases VALUES('old-work',?1,1,1,?2,'inflight')",
            params![c.deployment_id(), s.id()],
        )
        .unwrap();
    let s2 = store.begin_coordinator_session().unwrap();
    let cleanup = store
        .accept_candidate_cleanup(
            &s2,
            "owner",
            c.run_id(),
            "cleanup",
            r#"{"expected_revision":1,"action":"cleanup","deadline_ms":12000}"#,
            2000,
        )
        .unwrap();
    store
        .arm_candidate_cleanup(&s2, cleanup.step_id(), 2100)
        .unwrap();
    store.conn.execute("INSERT INTO runtime_bindings SELECT 'unexpected-old-binding',deployment_id,revision,'unexpected-old-incarnation',ownership,binding_json,'[]','released' FROM runtime_bindings WHERE id=?1",[c.binding_id()]).unwrap();
    let e = CleanupEvidence {
        binding_id: receipt.binding_id,
        incarnation: receipt.incarnation,
        identities: receipt.identities,
        observed_at_ms: 2200,
        receipt: "gone".into(),
    };
    let ttl = store
        .resource_policy("lab")
        .unwrap()
        .unwrap()
        .controls
        .observation_ttl_ms;
    let before = durable(&store);
    assert!(matches!(
        store.complete_cleanup(&s2, cleanup.step_id(), &e, 2200, ttl),
        Err(LifecycleError::Conflict)
    ));
    assert_eq!(durable(&store), before);
    store
        .conn
        .execute(
            "DELETE FROM runtime_bindings WHERE id='unexpected-old-binding'",
            [],
        )
        .unwrap();
    store
        .complete_cleanup(&s2, cleanup.step_id(), &e, 2200, ttl)
        .unwrap();
    assert_eq!(
        store
            .conn
            .query_row("SELECT COUNT(*) FROM request_leases", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn forged_cleanup_flag_and_missing_cleanup_evidence_never_excuse_missing_owner() {
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
    let ready = evidence(&context, &receipt);
    let ttl = store
        .resource_policy("lab")
        .unwrap()
        .unwrap()
        .controls
        .observation_ttl_ms;
    store
        .complete_step(&s, r.step_id(), &ready, 1300, ttl)
        .unwrap();
    store
        .conn
        .execute(
            "DELETE FROM resource_owners WHERE owner_id=?1",
            [c.deployment_id()],
        )
        .unwrap();
    store
        .conn
        .execute(
            "DELETE FROM endpoint_leases WHERE binding_id=?1",
            [c.binding_id()],
        )
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE runtime_bindings SET state='released' WHERE id=?1",
            [c.binding_id()],
        )
        .unwrap();
    store.conn.execute("UPDATE qualification_runs SET cleanup_state='verified_gone',cleanup_step_id=?1 WHERE id=?2",params![r.step_id(),c.run_id()]).unwrap();
    assert!(store
        .record_owned_launch(&s, r.step_id(), &receipt, 999999)
        .is_err());
    assert!(store
        .complete_step(&s, r.step_id(), &ready, 999999, 1)
        .is_err());
    assert!(store.candidate_run_snapshot("owner", c.run_id()).is_err());
}

#[test]
fn cleanup_cannot_be_completed_on_initialize_or_arm_a_released_binding() {
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
    let e = CleanupEvidence {
        binding_id: receipt.binding_id,
        incarnation: receipt.incarnation,
        identities: receipt.identities,
        observed_at_ms: 2200,
        receipt: "gone".into(),
    };
    let ttl = store
        .resource_policy("lab")
        .unwrap()
        .unwrap()
        .controls
        .observation_ttl_ms;
    assert!(matches!(
        store.complete_cleanup(&s, r.step_id(), &e, 2200, ttl),
        Err(LifecycleError::Unsupported)
    ));
    let cleanup = store
        .accept_candidate_cleanup(
            &s,
            "owner",
            c.run_id(),
            "cleanup",
            r#"{"expected_revision":1,"action":"cleanup","deadline_ms":12000}"#,
            2000,
        )
        .unwrap();
    store
        .conn
        .execute(
            "DELETE FROM endpoint_leases WHERE binding_id=?1",
            [c.binding_id()],
        )
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE runtime_bindings SET state='released' WHERE id=?1",
            [c.binding_id()],
        )
        .unwrap();
    assert!(store
        .arm_candidate_cleanup(&s, cleanup.step_id(), 2100)
        .is_err());
}

#[test]
fn missing_frozen_cleanup_permission_denies_new_cleanup() {
    let store = crate::Store::open_in_memory().unwrap();
    let (manifest, host, policy) = fixture("fake");
    let s = setup(&store, &policy);
    let mut body: Value = serde_json::from_str(&command(&manifest)).unwrap();
    body["allow_owned_abort_cleanup"] = json!(false);
    let c = store
        .create_candidate_run(&s, "owner", "create", &body.to_string(), &host, 1000)
        .unwrap();
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
    let before = durable(&store);
    assert!(store
        .accept_candidate_cleanup(
            &s,
            "owner",
            c.run_id(),
            "cleanup",
            r#"{"expected_revision":1,"action":"cleanup","deadline_ms":12000}"#,
            2000
        )
        .is_err());
    assert_eq!(durable(&store), before);
}

#[test]
fn concurrent_ready_and_cleanup_retries_commit_one_epoch_each() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("candidate.sqlite");
    let store = crate::Store::open(&path).unwrap();
    let (manifest, host, policy) = fixture("fake");
    let s = setup(&store, &policy);
    let c = store
        .create_candidate_run(&s, "owner", "create", &command(&manifest), &host, 1000)
        .unwrap();
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
    let ready = evidence(&context, &receipt);
    let ttl = store
        .resource_policy("lab")
        .unwrap()
        .unwrap()
        .controls
        .observation_ttl_ms;
    let before = store.resource_snapshot().unwrap().epoch;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let mut handles = vec![];
    for _ in 0..2 {
        let other = crate::Store::open(&path).unwrap();
        let s = s.clone();
        let step = r.step_id().to_owned();
        let evidence = ready.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            other.complete_step(&s, &step, &evidence, 1300, ttl)
        }));
    }
    barrier.wait();
    for h in handles {
        h.join().unwrap().unwrap();
    }
    assert_eq!(store.resource_snapshot().unwrap().epoch, before + 1);
    let cleanup = store
        .accept_candidate_cleanup(
            &s,
            "owner",
            c.run_id(),
            "cleanup",
            r#"{"expected_revision":1,"action":"cleanup","deadline_ms":12000}"#,
            2000,
        )
        .unwrap();
    store
        .arm_candidate_cleanup(&s, cleanup.step_id(), 2100)
        .unwrap();
    let gone = CleanupEvidence {
        binding_id: receipt.binding_id,
        incarnation: receipt.incarnation,
        identities: receipt.identities,
        observed_at_ms: 2200,
        receipt: "gone".into(),
    };
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let mut handles = vec![];
    for _ in 0..2 {
        let other = crate::Store::open(&path).unwrap();
        let s = s.clone();
        let step = cleanup.step_id().to_owned();
        let evidence = gone.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            other.complete_cleanup(&s, &step, &evidence, 2200, ttl)
        }));
    }
    barrier.wait();
    for h in handles {
        h.join().unwrap().unwrap();
    }
    assert_eq!(store.resource_snapshot().unwrap().epoch, before + 2);
    assert_eq!(store.conn.query_row("SELECT COUNT(*) FROM management_events WHERE kind IN ('candidate_ready_completed','candidate_cleanup_completed')",[],|r|r.get::<_,i64>(0)).unwrap(),2);
}

#[test]
fn cleanup_history_requires_evidence_and_rejects_duplicate_step_fields() {
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
    let cleanup = store
        .accept_candidate_cleanup(
            &s,
            "owner",
            c.run_id(),
            "cleanup",
            r#"{"expected_revision":1,"action":"cleanup","deadline_ms":12000}"#,
            2000,
        )
        .unwrap();
    store
        .arm_candidate_cleanup(&s, cleanup.step_id(), 2100)
        .unwrap();
    let gone = CleanupEvidence {
        binding_id: receipt.binding_id.clone(),
        incarnation: receipt.incarnation.clone(),
        identities: receipt.identities.clone(),
        observed_at_ms: 2200,
        receipt: "gone".into(),
    };
    let ttl = store
        .resource_policy("lab")
        .unwrap()
        .unwrap()
        .controls
        .observation_ttl_ms;
    store
        .complete_cleanup(&s, cleanup.step_id(), &gone, 2200, ttl)
        .unwrap();
    let raw: String = store
        .conn
        .query_row(
            "SELECT step_json FROM lifecycle_steps WHERE id=?1",
            [cleanup.step_id()],
            |r| r.get(0),
        )
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE lifecycle_steps SET step_json=?1 WHERE id=?2",
            params![raw.replacen('{', "{\"version\":2,", 1), cleanup.step_id()],
        )
        .unwrap();
    assert!(store
        .record_owned_launch(&s, r.step_id(), &receipt, 999999)
        .is_err());
    store
        .conn
        .execute(
            "UPDATE lifecycle_steps SET step_json=?1 WHERE id=?2",
            params![raw, cleanup.step_id()],
        )
        .unwrap();
    store
        .conn
        .execute(
            "DELETE FROM lifecycle_evidence WHERE step_id=?1",
            [cleanup.step_id()],
        )
        .unwrap();
    assert!(store
        .record_owned_launch(&s, r.step_id(), &receipt, 999999)
        .is_err());
    assert!(store
        .complete_cleanup(&s, cleanup.step_id(), &gone, 999999, 1)
        .is_err());
    assert!(store.candidate_run_snapshot("owner", c.run_id()).is_err());
}

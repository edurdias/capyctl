use super::*;
use mllm_store::candidate_creation::progression::CandidateSecurityDispatch;

#[tokio::test]
async fn security_owned_control_clock_samples_after_terminal_and_failure_retains_arm() {
    for failure in [false, true] {
        let f = fixture();
        let fake = FakeEngine::for_qualification();
        let collector = f.baseline(&fake).await;
        let CandidateSecurityDispatch::NewControl(d) = f
            .store
            .advance_candidate_security(&f.session, &collector, f.admission())
            .unwrap()
        else {
            panic!()
        };
        let step = d.context().token.step_id.clone();
        let before = fake.qualification_activity().unwrap();
        let observed =
            mllm_controller::qualification::collect_security_control_with_clock(&fake, *d, &|| {
                assert_eq!(
                    fake.qualification_activity().unwrap().1,
                    before.1 + 1,
                    "clock must follow actual terminal control check"
                );
                if failure {
                    Err(mllm_store::lifecycle::LifecycleError::Invalid)
                } else {
                    Ok(1375)
                }
            })
            .await;
        if failure {
            assert!(observed.is_err());
            assert_eq!(
                f.sql
                    .query_row(
                        "SELECT state FROM lifecycle_steps WHERE id=?1",
                        [&step],
                        |r| r.get::<_, String>(0)
                    )
                    .unwrap(),
                "armed"
            );
            assert!(matches!(
                f.store
                    .advance_candidate_security(&f.session, &collector, f.admission())
                    .unwrap(),
                CandidateSecurityDispatch::AlreadyRecorded {
                    complete: false,
                    ..
                }
            ));
        } else {
            let observed = observed.unwrap();
            assert_eq!(observed.effect.observed_at_ms, 1375);
            f.store
                .record_candidate_security_control(&f.session, &collector, &observed, 1375)
                .unwrap();
        }
        assert_eq!(f.scalar("SELECT count(*) FROM resource_owners"), 1);
        assert_eq!(f.scalar("SELECT count(*) FROM qualifications"), 0);
    }
}

#[tokio::test]
async fn security_owned_send_fences_exact_scope_each_child_and_cannot_recover_new() {
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    let collector = f.baseline(&fake).await;
    let work = f
        .store
        .next_candidate_security(&f.session, 1200)
        .unwrap()
        .unwrap();
    for child in 0..3 {
        let d = f
            .store
            .advance_candidate_security(&f.session, &collector, f.admission())
            .unwrap();
        f.store
            .revalidate_candidate_security_send(&f.session, &work, &d, f.admission())
            .unwrap();
        for (change,restore) in [
            ("UPDATE deployments SET current_generation=current_generation+1", "UPDATE deployments SET current_generation=current_generation-1"),
            ("UPDATE deployments SET dispatch_enabled=1", "UPDATE deployments SET dispatch_enabled=0"),
            ("UPDATE host_resource_policies SET revision=revision+1,policy_json=json_set(policy_json,'$.revision',revision+1)", "UPDATE host_resource_policies SET revision=revision-1,policy_json=json_set(policy_json,'$.revision',revision-1)"),
            ("UPDATE host_qualification_policies SET revision=revision+1,policy_json=json_set(policy_json,'$.revision',revision+1)", "UPDATE host_qualification_policies SET revision=revision-1,policy_json=json_set(policy_json,'$.revision',revision-1)"),
        ] {
            f.sql.execute(change,[]).unwrap();
            assert!(f.store.revalidate_candidate_security_send(&f.session,&work,&d,f.admission()).is_err(),"{child}: {change}");
            f.sql.execute(restore,[]).unwrap();
        }
        for field in [
            "principal",
            "run",
            "binding",
            "incarnation",
            "host",
            "revision",
            "deadline",
            "policy",
        ] {
            let mut wrong = work.clone();
            match field {
                "principal" => wrong.principal = "other".into(),
                "run" => wrong.run_id = "other".into(),
                "binding" => wrong.binding_id = "other".into(),
                "incarnation" => wrong.incarnation = "other".into(),
                "host" => wrong.host_id = "other".into(),
                "revision" => wrong.revision += 1,
                "deadline" => wrong.deadline_ms += 1,
                _ => wrong.policy.revision += 1,
            }
            assert!(
                f.store
                    .revalidate_candidate_security_send(&f.session, &wrong, &d, f.admission())
                    .is_err(),
                "{child} {field}"
            );
        }
        for now in [1199, 500000, 500001] {
            assert!(
                f.store
                    .revalidate_candidate_security_send(
                        &f.session,
                        &work,
                        &d,
                        AdmissionContext::new(&f.observations, &f.limits, now, f.ttl, f.max_parked)
                    )
                    .is_err(),
                "{child} {now}"
            );
        }
        let historical = f
            .store
            .advance_candidate_security(&f.session, &collector, f.admission())
            .unwrap();
        assert!(matches!(
            historical,
            CandidateSecurityDispatch::AlreadyRecorded {
                complete: false,
                ..
            }
        ));
        assert!(f
            .store
            .revalidate_candidate_security_send(&f.session, &work, &historical, f.admission())
            .is_err());
        match d {
            CandidateSecurityDispatch::NewControl(d) => {
                let o = mllm_controller::qualification::collect_security_control_with_clock(
                    &fake,
                    *d,
                    &|| Ok(1300),
                )
                .await
                .unwrap();
                f.store
                    .record_candidate_security_control(&f.session, &collector, &o, 1300)
                    .unwrap();
            }
            CandidateSecurityDispatch::NewRequest(d) => {
                let o =
                    mllm_controller::qualification::collect_probe_with_clock(&fake, *d, &|| {
                        Ok(1300)
                    })
                    .await
                    .unwrap();
                f.store
                    .record_candidate_result(&f.session, &collector, &o, 1300)
                    .unwrap();
            }
            _ => panic!(),
        }
    }
    assert!(f
        .store
        .next_candidate_security(&f.session, 1300)
        .unwrap()
        .is_none());
    assert_eq!(fake.qualification_activity().unwrap(), (7, 2, 5));
}

#[tokio::test]
async fn security_owned_discovery_rejects_corrupt_previously_armed_action() {
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    let collector = f.baseline(&fake).await;
    let CandidateSecurityDispatch::NewControl(d) = f
        .store
        .advance_candidate_security(&f.session, &collector, f.admission())
        .unwrap()
    else {
        panic!()
    };
    f.sql
        .execute(
            "UPDATE lifecycle_runs SET plan_json='{}' WHERE operation_id=?1",
            [&d.context().token.operation_id],
        )
        .unwrap();
    assert!(
        f.store.next_candidate_security(&f.session, 1200).is_err(),
        "discovery must validate existing Security authority before returning work"
    );
}

#[tokio::test]
async fn security_owned_discovery_bounds_sqlite_text_and_excludes_old_sessions() {
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    f.baseline(&fake).await;
    let original: String = f
        .sql
        .query_row(
            "SELECT plan_json FROM lifecycle_runs WHERE operation_id=?1",
            [f.init.operation_id()],
            |r| r.get(0),
        )
        .unwrap();
    for corrupt in [
        rusqlite::types::Value::Blob(vec![b'x'; 32]),
        rusqlite::types::Value::Text("x".repeat(1048577)),
        rusqlite::types::Value::Text("{}".into()),
    ] {
        f.sql
            .execute(
                "UPDATE lifecycle_runs SET plan_json=?1 WHERE operation_id=?2",
                rusqlite::params![corrupt, f.init.operation_id()],
            )
            .unwrap();
        assert!(f.store.next_candidate_security(&f.session, 1200).is_err());
    }
    f.sql
        .execute(
            "UPDATE lifecycle_runs SET plan_json=?1 WHERE operation_id=?2",
            rusqlite::params![original, f.init.operation_id()],
        )
        .unwrap();
    let next = f.store.begin_coordinator_session().unwrap();
    assert!(f.store.next_candidate_security(&f.session, 1200).is_err());
    assert!(f
        .store
        .next_candidate_security(&next, 1200)
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn security_owned_discovery_denies_current_generation_and_policy_changes() {
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    f.baseline(&fake).await;
    for (change,restore) in [
        ("UPDATE deployments SET current_generation=current_generation+1", "UPDATE deployments SET current_generation=current_generation-1"),
        ("UPDATE host_qualification_policies SET revision=revision+1,policy_json=json_set(policy_json,'$.revision',revision+1)", "UPDATE host_qualification_policies SET revision=revision-1,policy_json=json_set(policy_json,'$.revision',revision-1)"),
    ] {
        f.sql.execute(change,[]).unwrap();
        assert!(f.store.next_candidate_security(&f.session,1200).is_err(),"discovery must reject {change}");
        f.sql.execute(restore,[]).unwrap();
    }
}

#[tokio::test]
async fn security_owned_discovery_does_not_hide_oversized_completed_anchor() {
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    f.secured(&fake).await;
    f.sql.execute("UPDATE lifecycle_steps SET step_json=json_set(step_json,'$.padding',?1) WHERE ordinal=0 AND json_extract(step_json,'$.plan.action')='security'",["x".repeat(1048577)]).unwrap();
    assert!(
        f.store.next_candidate_security(&f.session, 1300).is_err(),
        "completed anchor must be bounded before it can exclude work from discovery"
    );
}

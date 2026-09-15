use super::*;

#[tokio::test]
async fn warm_owned_each_child_rechecks_exact_fences_and_retains_full_grant() {
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    let collector = f.secured(&fake).await;
    let before = f.store.resource_snapshot().unwrap().owners;
    for action in ["park", "restore"] {
        let body = json!({"expected_revision":1,"action":action,"deadline_ms":400000}).to_string();
        let accepted = f
            .store
            .accept_candidate_action(&f.session, "owner", f.created.run_id(), action, &body, 1200)
            .unwrap();
        let work = f
            .store
            .next_candidate_warm(&f.session, 1200)
            .unwrap()
            .unwrap();
        assert_eq!(work.operation_id, accepted.operation_id());
        let original: String = f
            .sql
            .query_row(
                "SELECT plan_json FROM lifecycle_runs WHERE operation_id=?1",
                [accepted.operation_id()],
                |r| r.get(0),
            )
            .unwrap();
        for corrupted in [
            rusqlite::types::Value::Blob(b"{}".to_vec()),
            rusqlite::types::Value::Text(" ".repeat((1 << 20) + 1)),
        ] {
            f.sql
                .execute(
                    "UPDATE lifecycle_runs SET plan_json=?1 WHERE operation_id=?2",
                    rusqlite::params![corrupted, accepted.operation_id()],
                )
                .unwrap();
            assert!(matches!(
                f.store.next_candidate_warm(&f.session, 1200),
                Err(mllm_store::lifecycle::LifecycleError::CorruptStoredData)
            ));
            f.sql
                .execute(
                    "UPDATE lifecycle_runs SET plan_json=?1 WHERE operation_id=?2",
                    rusqlite::params![original, accepted.operation_id()],
                )
                .unwrap();
        }
        for (child, kind) in &work.effects {
            use mllm_store::candidate_creation::progression::PersistedEffectKind as K;
            if *kind == K::Probe {
                break;
            }
            assert!(matches!(
                f.store
                    .arm_candidate_effect(&f.session, child, f.admission())
                    .unwrap(),
                ArmResult::New { .. }
            ));
            let (_, context) = f
                .store
                .candidate_effect_execution(&f.session, child)
                .unwrap();
            assert!(matches!(
                f.store
                    .arm_candidate_effect(&f.session, child, f.admission())
                    .unwrap(),
                ArmResult::AlreadyRecorded
            ));
            f.store
                .revalidate_candidate_warm_send(&f.session, child, &context, f.admission())
                .unwrap();
            for (change,restore) in [
                ("UPDATE deployments SET current_generation=current_generation+1","UPDATE deployments SET current_generation=current_generation-1"),
                ("UPDATE deployments SET dispatch_enabled=1","UPDATE deployments SET dispatch_enabled=0"),
                ("UPDATE host_resource_policies SET revision=revision+1,policy_json=json_set(policy_json,'$.revision',revision+1)","UPDATE host_resource_policies SET revision=revision-1,policy_json=json_set(policy_json,'$.revision',revision-1)"),
                ("UPDATE host_qualification_policies SET revision=revision+1,policy_json=json_set(policy_json,'$.revision',revision+1)","UPDATE host_qualification_policies SET revision=revision-1,policy_json=json_set(policy_json,'$.revision',revision-1)"),
            ] {
                f.sql.execute(change,[]).unwrap();
                assert!(f.store.revalidate_candidate_warm_send(&f.session,child,&context,f.admission()).is_err(),"{kind:?}: {change}");
                f.sql.execute(restore,[]).unwrap();
            }
            f.sql
                .execute("UPDATE runtime_bindings SET incarnation='other'", [])
                .unwrap();
            assert!(f
                .store
                .revalidate_candidate_warm_send(&f.session, child, &context, f.admission())
                .is_err());
            f.sql
                .execute(
                    "UPDATE runtime_bindings SET incarnation=?1",
                    [&context.incarnation],
                )
                .unwrap();
            let mut changed = context.clone();
            changed.issued_at_ms += 1;
            assert!(f
                .store
                .revalidate_candidate_warm_send(&f.session, child, &changed, f.admission())
                .is_err());
            let stale = AdmissionContext::new(&f.observations, &f.limits, 400000, 1000, 1);
            assert!(f
                .store
                .revalidate_candidate_warm_send(&f.session, child, &context, stale)
                .is_err());
            let mut pressured = f.observations.clone();
            for observation in &mut pressured {
                observation.available_bytes = 0;
            }
            let admission = f.admission();
            assert!(f
                .store
                .revalidate_candidate_warm_send(
                    &f.session,
                    child,
                    &context,
                    AdmissionContext::new(
                        &pressured,
                        admission.limits,
                        admission.now_ms,
                        admission.ttl_ms,
                        admission.max_parked
                    )
                )
                .is_err());
            f.sql
                .execute(
                    "INSERT INTO request_leases VALUES('unproven',?1,1,1,?2,'uncertain')",
                    rusqlite::params![f.created.deployment_id(), f.session.id()],
                )
                .unwrap();
            assert!(f
                .store
                .revalidate_candidate_warm_send(&f.session, child, &context, f.admission())
                .is_err());
            f.sql
                .execute("DELETE FROM request_leases WHERE id='unproven'", [])
                .unwrap();
            let effect = match kind {
                K::Drain => RuntimeAction::Drain,
                K::Park => RuntimeAction::Park,
                K::Restore => RuntimeAction::Restore,
                K::ReloadWeights => RuntimeAction::ReloadWeights,
                K::InvalidateCache => RuntimeAction::InvalidateCache,
                _ => unreachable!(),
            };
            let observation = fake
                .execute_persisted(&RuntimeCommand {
                    action: effect,
                    context,
                })
                .await
                .unwrap();
            f.store
                .record_candidate_effect(&f.session, &collector, child, &observation, 1300)
                .unwrap();
            assert_eq!(f.store.resource_snapshot().unwrap().owners, before);
        }
        if action == "park" {
            let context = f
                .store
                .candidate_parked_status_execution(&f.session, accepted.step_id(), 1300)
                .unwrap();
            let activity = fake.qualification_activity().unwrap();
            let failed = mllm_controller::qualification::collect_parked_status_with_clock(
                &fake,
                &context,
                &|| Err(mllm_store::lifecycle::LifecycleError::Invalid),
            );
            assert!(failed.is_err());
            assert_eq!(
                f.scalar("SELECT count(*) FROM qualification_parked_status"),
                0
            );
            assert!(
                mllm_controller::qualification::collect_parked_status_with_clock(
                    &fake,
                    &context,
                    &|| Ok(1299)
                )
                .is_err(),
                "service status clock must not precede issued observation context"
            );
            f.sql.execute("UPDATE host_qualification_policies SET revision=revision+1,policy_json=json_set(policy_json,'$.revision',revision+1)",[]).unwrap();
            assert!(
                f.store
                    .candidate_parked_status_execution(&f.session, accepted.step_id(), 1300)
                    .is_err(),
                "parked status must recheck original qualification policy revision"
            );
            f.sql.execute("UPDATE host_qualification_policies SET revision=revision-1,policy_json=json_set(policy_json,'$.revision',revision-1)",[]).unwrap();
            f.sql
                .execute(
                    "INSERT INTO request_leases VALUES('unproven',?1,1,1,?2,'uncertain')",
                    rusqlite::params![f.created.deployment_id(), f.session.id()],
                )
                .unwrap();
            assert!(
                f.store
                    .candidate_parked_status_execution(&f.session, accepted.step_id(), 1300)
                    .is_err(),
                "parked status must reject unproven leases"
            );
            f.sql
                .execute("DELETE FROM request_leases WHERE id='unproven'", [])
                .unwrap();
            let observed = mllm_controller::qualification::collect_parked_status_with_clock(
                &fake,
                &context,
                &|| Ok(1375),
            )
            .unwrap();
            assert_eq!(observed.observed_at_ms, 1375);
            assert_eq!(fake.qualification_activity().unwrap(), activity);
            f.store
                .record_candidate_parked_status(&f.session, &collector, &observed, 1375)
                .unwrap();
        } else {
            let CandidateDispatchResult::New(probe) = f
                .store
                .arm_candidate_probe(&f.session, accepted.step_id(), f.admission())
                .unwrap()
            else {
                panic!()
            };
            f.store
                .revalidate_candidate_probe_send(&f.session, &probe, 1200)
                .unwrap();
            let observation = mllm_controller::qualification::collect_probe(&fake, *probe)
                .await
                .unwrap();
            f.store
                .record_candidate_result(&f.session, &collector, &observation, 1300)
                .unwrap();
        }
        assert!(f
            .store
            .next_candidate_warm(&f.session, 1400)
            .unwrap()
            .is_none());
        assert_eq!(
            f.store
                .candidate_action_command_receipt(
                    &f.session,
                    "owner",
                    f.created.run_id(),
                    action,
                    &body
                )
                .unwrap()
                .unwrap()
                .operation_id(),
            accepted.operation_id()
        );
    }
}

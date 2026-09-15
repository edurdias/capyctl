use super::*;

pub(super) async fn qualified() -> (Fixture, mllm_store::qualification::QualificationReceipt) {
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    f.completed_suite(&fake).await;
    let catalog = f
        .store
        .finish_candidate_run(
            &f.session,
            "owner",
            f.created.run_id(),
            "finish",
            r#"{"expected_revision":1,"action":"finish"}"#,
            1400,
        )
        .unwrap();
    let before_cleanup = managed_edit(&f, &catalog, "before-cleanup", |_, _| {});
    let before = f.counts();
    assert!(
        f.store
            .accept_qualified_start(&f.session, &before_cleanup, 1800, 10000)
            .is_err()
    );
    assert_eq!(f.counts(), before);
    let cleanup = f
        .store
        .accept_candidate_cleanup(
            &f.session,
            "owner",
            f.created.run_id(),
            "cleanup",
            r#"{"expected_revision":1,"action":"cleanup","deadline_ms":60000}"#,
            1500,
        )
        .unwrap();
    f.store
        .arm_candidate_cleanup(&f.session, cleanup.step_id(), 1550)
        .unwrap();
    let context = f
        .store
        .candidate_cleanup_execution(&f.session, cleanup.step_id())
        .unwrap();
    let gone = mllm_controller::qualification::collect_cleanup(&fake, &context, 1600).unwrap();
    f.store
        .complete_cleanup(&f.session, cleanup.step_id(), &gone, 1650, f.ttl)
        .unwrap();
    (f, catalog)
}

pub(super) fn managed(
    f: &Fixture,
    catalog: &mllm_store::qualification::QualificationReceipt,
) -> mllm_store::lifecycle::DeploymentFence {
    managed_edit(f, catalog, "ordinary", |_, _| {})
}
pub(super) fn managed_edit(
    f: &Fixture,
    catalog: &mllm_store::qualification::QualificationReceipt,
    name: &str,
    edit: impl FnOnce(&mut Value, &mut Value),
) -> mllm_store::lifecycle::DeploymentFence {
    let source: Value = serde_json::from_str(include_str!(
        "../../../mllm-config/tests/fixtures/effective-fake-golden.json"
    ))
    .unwrap();
    let mut host = source["input"]["host"].clone();
    host["runtime_profiles"]["local"]["build_fingerprint"] = json!("qualification-fake-v1");
    host["runtime_profiles"]["local"]["security"]["admin_credential_ref"] =
        json!("secret://another-admin");
    host["runtime_profiles"]["local"]["qualification_id"] =
        json!(format!("qualified:{}", catalog.qualification_id()));
    let mut deployment = source["input"]["deployment"].clone();
    deployment["name"] = json!(name);
    deployment["routes"] = json!([name]);
    edit(&mut deployment, &mut host);
    let receipt = f
        .store
        .create_stopped_managed_configuration(
            &f.session,
            "owner",
            name,
            &json!({"config":deployment}).to_string(),
            &host,
            1700,
        )
        .unwrap();
    mllm_store::lifecycle::DeploymentFence {
        deployment_id: receipt.deployment_id,
        revision: receipt.revision,
        generation: receipt.generation,
    }
}

#[tokio::test]
async fn ordinary_rejects_revision_history_tampering_and_catalog_mismatches() {
    let (f, catalog) = qualified().await;
    assert!(
        f.store
            .read_qualification(&f.session, catalog.qualification_id())
            .unwrap()
            .is_some()
    );
    let (operation,original):(String,String)=f.sql.query_row("SELECT r.request_operation_id,r.evidence_json FROM qualification_request_results r JOIN qualification_request_attempts a ON a.request_operation_id=r.request_operation_id WHERE json_extract(a.receipt_json,'$.kind')='candidate_marker_request' LIMIT 1",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    let mut corrupt: Value = serde_json::from_str(&original).unwrap();
    corrupt["output_matches"] = json!(false);
    f.sql.execute("UPDATE qualification_request_results SET evidence_json=?2 WHERE request_operation_id=?1",rusqlite::params![operation,corrupt.to_string()]).unwrap();
    assert!(
        f.store
            .read_qualification(&f.session, catalog.qualification_id())
            .is_err()
    );
    f.sql.execute("UPDATE qualification_request_results SET evidence_json=?2 WHERE request_operation_id=?1",rusqlite::params![operation,original]).unwrap();
    let fence = managed(&f, &catalog);
    let raw: String = f
        .sql
        .query_row(
            "SELECT effective_json FROM effective_revisions WHERE deployment_id=?1",
            [&fence.deployment_id],
            |r| r.get(0),
        )
        .unwrap();
    let mut altered: Value = serde_json::from_str(&raw).unwrap();
    altered["routes"] = json!(["silently-replaced-route"]);
    f.sql
        .execute(
            "UPDATE effective_revisions SET effective_json=?2 WHERE deployment_id=?1",
            rusqlite::params![fence.deployment_id, altered.to_string()],
        )
        .unwrap();
    let before = f.counts();
    assert!(
        f.store
            .accept_qualified_start(&f.session, &fence, 1800, 10000)
            .is_err()
    );
    assert_eq!(f.counts(), before);
    f.sql
        .execute(
            "UPDATE effective_revisions SET effective_json=?2 WHERE deployment_id=?1",
            rusqlite::params![fence.deployment_id, raw],
        )
        .unwrap();
    f.sql
        .execute(
            "UPDATE deployment_routes SET route='corrupt-route' WHERE deployment_id=?1",
            [&fence.deployment_id],
        )
        .unwrap();
    let before = f.counts();
    assert!(
        f.store
            .accept_qualified_start(&f.session, &fence, 1800, 10000)
            .is_err()
    );
    assert_eq!(f.counts(), before);
    f.sql
        .execute(
            "UPDATE deployment_routes SET route='ordinary' WHERE deployment_id=?1",
            [&fence.deployment_id],
        )
        .unwrap();
    for variant in 0..5 {
        let mismatch = managed_edit(
            &f,
            &catalog,
            &format!("mismatch-{variant}"),
            |deployment, host| match variant {
                0 => host["hardware_fingerprint"] = json!("other-hardware"),
                1 => {
                    host["runtime_profiles"]["local"]["revision"] = {
                        deployment["runtime_profile_revision"] = json!(2);
                        json!(2)
                    }
                }
                2 => deployment["recipe"] = json!("different"),
                3 => host["runtime_profiles"]["local"]["build_fingerprint"] = json!("other-build"),
                _ => {
                    host["runtime_profiles"]["local"]["qualification_id"] =
                        json!(catalog.qualification_id())
                }
            },
        );
        let before = f.counts();
        assert!(
            f.store
                .accept_qualified_start(&f.session, &mismatch, 1800, 10000)
                .is_err()
        );
        assert_eq!(f.counts(), before);
    }
}

#[tokio::test]
async fn ordinary_policy_change_stop_and_restart_retain_peak_and_never_resend() {
    let (f, catalog) = qualified().await;
    let fence = managed(&f, &catalog);
    let accepted = f
        .store
        .accept_qualified_start(&f.session, &fence, 1800, 10000)
        .unwrap();
    let mut context = f.admission();
    context.now_ms = 1900;
    let mut controls = f.store.resource_policy("lab").unwrap().unwrap().controls;
    let previous = controls.domains["unified"].managed_limit;
    controls.domains.get_mut("unified").unwrap().managed_limit = 9_i64 << 30;
    f.store
        .update_resource_policy(
            &f.session,
            "owner",
            "lab",
            1,
            "lower",
            &controls,
            &f.observations,
            1800,
        )
        .unwrap();
    let before = f.counts();
    assert!(
        f.store
            .arm_step(&f.session, &accepted.step_id, context)
            .is_err()
    );
    assert_eq!(f.counts(), before);
    assert!(f.store.resource_snapshot().unwrap().owners.is_empty());
    controls.domains.get_mut("unified").unwrap().managed_limit = previous;
    f.store
        .update_resource_policy(
            &f.session,
            "owner",
            "lab",
            2,
            "restore",
            &controls,
            &f.observations,
            1850,
        )
        .unwrap();
    let mut stale = context;
    stale.now_ms = 10001;
    assert!(
        f.store
            .arm_step(&f.session, &accepted.step_id, stale)
            .is_err()
    );
    assert!(matches!(
        f.store
            .arm_step(&f.session, &accepted.step_id, context)
            .unwrap(),
        ArmResult::New { .. }
    ));
    let execution = f
        .store
        .qualified_initialize_execution(&f.session, &accepted.step_id)
        .unwrap();
    let observation = FakeEngine::for_qualification()
        .execute_persisted(&RuntimeCommand {
            action: RuntimeAction::Initialize,
            context: execution,
        })
        .await
        .unwrap();
    f.store
        .record_owned_launch(
            &f.session,
            &accepted.step_id,
            &OwnedLaunchReceipt {
                binding_id: observation.binding_id,
                incarnation: observation.incarnation,
                identities: observation.identities.clone(),
                observed_at_ms: observation.observed_at_ms,
                receipt: observation.receipt.clone(),
            },
            1950,
        )
        .unwrap();
    f.store.fence_stop(&f.session, &fence, 10000).unwrap();
    let before = f.counts();
    let evidence = CompletionEvidence {
        token: observation.token,
        identities: observation.identities,
        observed_at_ms: observation.observed_at_ms,
        control_receipt: Some(observation.receipt),
        milestones: observation.facts,
    };
    assert!(
        f.store
            .complete_step(&f.session, &accepted.step_id, &evidence, 2000, f.ttl)
            .is_err()
    );
    assert_eq!(f.counts(), before);
    let reopened = Store::open(&f._dir.path().join("qualification.db")).unwrap();
    let session = reopened.begin_coordinator_session().unwrap();
    assert!(
        reopened
            .arm_step(&session, &accepted.step_id, context)
            .is_err()
    );
    assert!(
        reopened
            .arm_step(&f.session, &accepted.step_id, context)
            .is_err()
    );
    assert_eq!(
        reopened.resource_snapshot().unwrap().owners[&fence.deployment_id].phase,
        ResourcePhase::Cold
    );
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM lifecycle_steps WHERE state='uncertain'"),
        1
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM endpoint_leases"), 1);
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM deployments WHERE name='ordinary' AND dispatch_enabled=1"),
        0
    );
}

#[tokio::test]
async fn ordinary_initialize_actual_catalog_to_ready() {
    let (f, catalog) = qualified().await;
    let fence = managed(&f, &catalog);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let barrier = barrier.clone();
            let path = f._dir.path().join("qualification.db");
            let session = f.session.clone();
            let target = fence.clone();
            std::thread::spawn(move || {
                let store = Store::open(&path).unwrap();
                barrier.wait();
                store.accept_qualified_start(&session, &target, 1800, 10000)
            })
        })
        .collect();
    let results: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    let accepted = results
        .iter()
        .find_map(|r| r.as_ref().ok())
        .unwrap()
        .clone();
    let joined = f
        .store
        .accept_qualified_start(&f.session, &fence, 1801, 20000)
        .unwrap();
    assert!(joined.joined);
    assert_eq!(accepted.operation_id, joined.operation_id);
    assert_eq!(accepted.step_id, joined.step_id);
    assert_eq!(accepted.binding_id, joined.binding_id);
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM deployments WHERE name='ordinary' AND dispatch_enabled=1"),
        0
    );
    let mut admission = f.admission();
    admission.now_ms = 1900;
    let mut stale = admission;
    stale.now_ms = 4000;
    assert!(
        f.store
            .arm_step(&f.session, &accepted.step_id, stale)
            .is_err()
    );
    let before = f.counts();
    let epoch_before = f.store.resource_snapshot().unwrap().epoch;
    f.sql.execute_batch("CREATE TRIGGER ordinary_arm_failure BEFORE UPDATE OF state ON lifecycle_steps WHEN NEW.state='armed' AND json_extract(NEW.step_json,'$.kind')='qualified_initialize' BEGIN SELECT RAISE(ABORT,'ordinary arm failure'); END;").unwrap();
    assert!(
        f.store
            .arm_step(&f.session, &accepted.step_id, admission)
            .is_err()
    );
    assert_eq!(f.counts(), before);
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch_before);
    assert!(f.store.resource_snapshot().unwrap().owners.is_empty());
    f.sql
        .execute_batch("DROP TRIGGER ordinary_arm_failure;")
        .unwrap();
    assert!(matches!(
        f.store
            .arm_step(&f.session, &accepted.step_id, admission)
            .unwrap(),
        ArmResult::New { .. }
    ));
    let context = f
        .store
        .qualified_initialize_execution(&f.session, &accepted.step_id)
        .unwrap();
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        2,
        "concurrent accepts must both succeed"
    );
    assert_eq!(
        results
            .iter()
            .filter(|r| r.as_ref().is_ok_and(|r| r.joined))
            .count(),
        1
    );
    assert!(
        results
            .iter()
            .all(|r| r.as_ref().is_ok_and(|r| r.step_id == accepted.step_id))
    );
    assert_eq!(context.deadline_ms, 10000);
    assert_eq!(
        f.store.resource_snapshot().unwrap().owners[&fence.deployment_id].phase,
        ResourcePhase::Cold
    );
    assert_eq!(
        f.store
            .arm_step(&f.session, &accepted.step_id, admission)
            .unwrap(),
        ArmResult::AlreadyRecorded
    );
    let observation = FakeEngine::for_qualification()
        .execute_persisted(&RuntimeCommand {
            action: RuntimeAction::Initialize,
            context,
        })
        .await
        .unwrap();
    let evidence = CompletionEvidence {
        token: observation.token,
        identities: observation.identities.clone(),
        observed_at_ms: observation.observed_at_ms,
        control_receipt: Some(observation.receipt.clone()),
        milestones: observation.facts,
    };
    assert!(
        f.store
            .complete_step(&f.session, &accepted.step_id, &evidence, 1950, f.ttl)
            .is_err()
    );
    f.store
        .record_owned_launch(
            &f.session,
            &accepted.step_id,
            &OwnedLaunchReceipt {
                binding_id: observation.binding_id,
                incarnation: observation.incarnation,
                identities: observation.identities,
                observed_at_ms: observation.observed_at_ms,
                receipt: observation.receipt,
            },
            1950,
        )
        .unwrap();
    let before = f.counts();
    let peak_epoch = f.store.resource_snapshot().unwrap().epoch;
    let mut mutations = Vec::new();
    let mut wrong = evidence.clone();
    wrong.token.generation += 1;
    mutations.push(wrong);
    let mut wrong = evidence.clone();
    wrong.token.revision += 1;
    mutations.push(wrong);
    let mut wrong = evidence.clone();
    wrong.token.qualification_id = "qualified:01ARZ3NDEKTSV4RRFFQ69G5FAV".into();
    mutations.push(wrong);
    let mut wrong = evidence.clone();
    wrong.identities[1].start_ticks += 1;
    mutations.push(wrong);
    let mut wrong = evidence.clone();
    wrong.identities.pop();
    mutations.push(wrong);
    let mut wrong = evidence.clone();
    wrong.milestones.swap(0, 1);
    mutations.push(wrong);
    let mut wrong = evidence.clone();
    wrong.milestones.pop();
    mutations.push(wrong);
    let mut wrong = evidence.clone();
    wrong.observed_at_ms = 10001;
    mutations.push(wrong);
    for wrong in mutations {
        assert!(
            f.store
                .complete_step(&f.session, &accepted.step_id, &wrong, 1950, f.ttl)
                .is_err()
        );
        assert_eq!(f.counts(), before);
        assert_eq!(f.store.resource_snapshot().unwrap().epoch, peak_epoch);
    }
    f.sql.execute("INSERT INTO request_leases(id,deployment_id,revision,generation,session_id,disposition) VALUES('ordinary-unknown',?1,1,1,?2,'uncertain')",rusqlite::params![fence.deployment_id,f.session.id()]).unwrap();
    assert!(
        f.store
            .complete_step(&f.session, &accepted.step_id, &evidence, 1950, f.ttl)
            .is_err()
    );
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, peak_epoch);
    f.sql
        .execute("DELETE FROM request_leases WHERE id='ordinary-unknown'", [])
        .unwrap();
    f.sql.execute_batch("CREATE TRIGGER ordinary_completion_failure BEFORE INSERT ON lifecycle_evidence WHEN NEW.step_id IN (SELECT id FROM lifecycle_steps WHERE json_extract(step_json,'$.kind')='qualified_initialize') BEGIN SELECT RAISE(ABORT,'ordinary completion failure'); END;").unwrap();
    assert!(
        f.store
            .complete_step(&f.session, &accepted.step_id, &evidence, 1950, f.ttl)
            .is_err()
    );
    assert_eq!(f.counts(), before);
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, peak_epoch);
    f.sql
        .execute_batch("DROP TRIGGER ordinary_completion_failure;")
        .unwrap();
    f.store
        .complete_step(&f.session, &accepted.step_id, &evidence, 1950, f.ttl)
        .unwrap();
    let epoch = f.store.resource_snapshot().unwrap().epoch;
    f.store
        .complete_step(&f.session, &accepted.step_id, &evidence, 2000, f.ttl)
        .unwrap();
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
    assert_eq!(
        f.store.resource_snapshot().unwrap().owners[&fence.deployment_id].phase,
        ResourcePhase::Ready
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM deployments WHERE name='ordinary' AND dispatch_enabled=1 AND observed_state='ready'"), 1);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM endpoint_leases"), 1);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM lifecycle_claims"), 0);
    let mut wrong = evidence.clone();
    wrong.observed_at_ms += 1;
    assert!(
        f.store
            .complete_step(&f.session, &accepted.step_id, &wrong, 2000, f.ttl)
            .is_err()
    );
}

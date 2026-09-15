use super::*;
use mllm_store::lifecycle::LifecycleError;

const BODY: &str = r#"{"expected_revision":1,"action":"finish","deadline_ms":400000}"#;

fn finish(
    f: &Fixture,
    key: &str,
    body: &str,
    now: i64,
) -> Result<mllm_store::qualification::QualificationReceipt, LifecycleError> {
    f.store
        .finish_candidate_run_command(&f.session, "owner", f.created.run_id(), key, body, now)
}

#[tokio::test]
async fn finish_service_requires_sources_and_exact_deadline_before_atomic_completion() {
    let f = fixture();
    let before = f.counts();
    assert!(finish(&f, "finish", BODY, 1400).is_err());
    assert_eq!(f.counts(), before);
    let fake = FakeEngine::for_qualification();
    f.completed_suite(&fake).await;
    let counts = f.counts();
    let resources = f.store.resource_snapshot().unwrap();
    let retained = || {
        [
        "SELECT json_group_array(json_array(id,deployment_id,operation_id,request_json,committed_epoch)) FROM resource_grants",
        "SELECT json_group_array(json_array(id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state)) FROM runtime_bindings",
        "SELECT json_group_array(json_array(host,port,binding_id)) FROM endpoint_leases",
        "SELECT json_group_array(association_json) FROM owned_launch_associations",
    ].map(|query| f.sql.query_row(query, [], |r| r.get::<_, String>(0)).unwrap())
    };
    let retained_before = retained();
    for (body, now) in [
        (BODY.to_owned(), 1199),
        (BODY.to_owned(), 1200),
        (BODY.to_owned(), 400000),
        (BODY.to_owned(), 500000),
        (BODY.replace("400000", "500001"), 1400),
        (BODY.replace("400000", "1400"), 1400),
        (BODY.replace("400000", "0"), 1400),
        (BODY.replace("400000", "null"), 1400),
        (BODY.replace("\"finish\"", "\"abort\""), 1400),
        (BODY.replace("revision\":1", "revision\":2"), 1400),
        (BODY.replacen('{', "{\"deadline_ms\":400000,", 1), 1400),
        (
            r#"{"expected_revision":1,"action":"finish"}"#.to_owned(),
            1400,
        ),
    ] {
        assert!(finish(&f, "finish", &body, now).is_err(), "{body} at {now}");
        assert_eq!(f.counts(), counts);
        assert_eq!(f.store.resource_snapshot().unwrap(), resources);
    }
    // Every SQL insertion point rolls the entire completion back.
    for table in [
        "operations",
        "qualifications",
        "command_receipts",
        "management_events",
    ] {
        f.sql.execute_batch(&format!("CREATE TRIGGER reject_finish BEFORE INSERT ON {table} BEGIN SELECT RAISE(ABORT,'finish rollback'); END")).unwrap();
        assert!(matches!(
            finish(&f, "finish", BODY, 1400),
            Err(LifecycleError::Sql(_))
        ));
        assert_eq!(f.counts(), counts, "{table}");
        assert_eq!(f.store.resource_snapshot().unwrap(), resources);
        assert_eq!(
            f.scalar("SELECT count(*) FROM qualification_runs WHERE state='running'"),
            1
        );
        f.sql.execute_batch("DROP TRIGGER reject_finish").unwrap();
    }
    for ticks in [
        vec![1400, 400000],
        vec![1400, 1399],
        vec![1400, 1500, 400000],
        vec![1400, 1500, 1499],
    ] {
        let mut clock = ticks.into_iter();
        assert!(
            f.store
                .finish_candidate_run_command_with_clock(
                    &f.session,
                    "owner",
                    f.created.run_id(),
                    "clock",
                    BODY,
                    || Ok(clock.next().expect("unexpected clock read"))
                )
                .is_err()
        );
        assert_eq!(f.counts(), counts);
        assert_eq!(f.store.resource_snapshot().unwrap(), resources);
        assert_eq!(
            f.scalar("SELECT count(*) FROM qualification_runs WHERE state='running'"),
            1
        );
    }
    let receipt = finish(&f, "finish", BODY, 1400).unwrap();
    assert_eq!(retained(), retained_before);
    let raw: String = f
        .sql
        .query_row("SELECT record_json FROM qualifications", [], |r| r.get(0))
        .unwrap();
    let record: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(record["version"], 4);
    assert_eq!(record["command_deadline_ms"], 400000);
    assert_eq!(
        f.sql
            .query_row(
                "SELECT response_json FROM command_receipts WHERE idempotency_key='finish'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        raw
    );
    assert_eq!(
        f.scalar(
            "SELECT count(*) FROM operations WHERE kind='candidate_finish_v4' AND state='succeeded'"
        ),
        1
    );
    assert_eq!(
        f.store.resource_snapshot().unwrap().owners,
        resources.owners
    );
    assert_eq!(fake.qualification_activity().unwrap(), (12, 7, 10));
    assert_eq!(finish(&f, "finish", BODY, 999999).unwrap(), receipt);
    assert_eq!(
        f.store
            .finish_candidate_run_command_with_clock(
                &f.session,
                "owner",
                f.created.run_id(),
                "finish",
                BODY,
                || panic!("historical retry must not sample an admission clock")
            )
            .unwrap(),
        receipt
    );
    // Changing both catalog and receipt raw JSON cannot bypass the command hash,
    // version/deadline contract, or original operation identity.
    let mut corrupt = Vec::new();
    for (field, value) in [
        ("command_deadline_ms", json!(399999)),
        ("command_deadline_ms", Value::Null),
        ("version", json!(3)),
        ("version", json!(5)),
    ] {
        let mut bad = record.clone();
        bad[field] = value;
        corrupt.push(bad.to_string());
    }
    let mut missing = record.clone();
    missing
        .as_object_mut()
        .unwrap()
        .remove("command_deadline_ms");
    corrupt.push(missing.to_string());
    corrupt.push(raw.replacen('{', "{\"command_deadline_ms\":400000,", 1));
    corrupt.push(" ".repeat(1048577));
    for bad in corrupt {
        f.sql
            .execute("UPDATE qualifications SET record_json=?1", [&bad])
            .unwrap();
        f.sql
            .execute(
                "UPDATE command_receipts SET response_json=?1 WHERE idempotency_key='finish'",
                [&bad],
            )
            .unwrap();
        assert!(
            f.store
                .read_qualification(&f.session, receipt.qualification_id())
                .is_err()
        );
        assert!(
            f.store
                .candidate_finish_command_receipt(
                    &f.session,
                    "owner",
                    f.created.run_id(),
                    "finish",
                    BODY
                )
                .is_err()
        );
    }
    f.sql
        .execute("UPDATE qualifications SET record_json=?1", [&raw])
        .unwrap();
    f.sql
        .execute(
            "UPDATE command_receipts SET response_json=?1 WHERE idempotency_key='finish'",
            [&raw],
        )
        .unwrap();
    let counts = f.counts();
    for (key, body) in [
        ("other", BODY.to_owned()),
        ("init", BODY.to_owned()),
        ("finish", BODY.replace("400000", "399999")),
    ] {
        assert!(matches!(
            finish(&f, key, &body, 1400),
            Err(LifecycleError::Conflict)
        ));
        assert_eq!(f.counts(), counts);
    }
    assert!(
        f.store
            .finish_candidate_run(
                &f.session,
                "owner",
                f.created.run_id(),
                "finish",
                r#"{"expected_revision":1,"action":"finish"}"#,
                1400
            )
            .is_err()
    );
    f.sql.execute_batch("UPDATE host_qualification_policies SET policy_json=json_set(policy_json,'$.state.policy.allow_qualification_runs',json('false'))").unwrap();
    assert_eq!(finish(&f, "finish", BODY, 999999).unwrap(), receipt);
    let next = f.store.begin_coordinator_session().unwrap();
    assert!(finish(&f, "finish", BODY, 1400).is_err());
    assert_eq!(
        f.store
            .candidate_finish_command_receipt(&next, "owner", f.created.run_id(), "finish", BODY)
            .unwrap()
            .unwrap(),
        receipt
    );
    assert_eq!(
        f.store
            .finish_candidate_run_command(
                &next,
                "owner",
                f.created.run_id(),
                "finish",
                BODY,
                999999
            )
            .unwrap(),
        receipt
    );
}

#[tokio::test]
async fn finish_service_rejects_corrupt_missing_failed_and_outstanding_sources() {
    let f = fixture();
    f.completed_suite(&FakeEngine::for_qualification()).await;
    for (table, column, selector) in [
        ("qualification_parked_status", "evidence_json", "1=1"),
        (
            "qualification_request_results",
            "evidence_json",
            "rowid=(SELECT min(rowid) FROM qualification_request_results)",
        ),
        (
            "qualification_evidence_refs",
            "metadata_json",
            "rowid=(SELECT min(rowid) FROM qualification_evidence_refs)",
        ),
        (
            "lifecycle_evidence",
            "evidence_json",
            "rowid=(SELECT min(rowid) FROM lifecycle_evidence)",
        ),
    ] {
        let raw: String = f
            .sql
            .query_row(
                &format!("SELECT {column} FROM {table} WHERE {selector}"),
                [],
                |r| r.get(0),
            )
            .unwrap();
        let counts = f.counts();
        f.sql
            .execute(
                &format!("UPDATE {table} SET {column}=?1 WHERE {selector}"),
                [raw.replacen('{', "{\"version\":3,", 1)],
            )
            .unwrap();
        assert!(finish(&f, "finish", BODY, 1400).is_err(), "{table}");
        assert_eq!(f.counts(), counts);
        f.sql
            .execute(
                &format!("UPDATE {table} SET {column}=?1 WHERE {selector}"),
                [raw],
            )
            .unwrap();
    }
    for (change, restore) in [
        (
            "UPDATE qualification_runs SET state='failed'",
            "UPDATE qualification_runs SET state='running'",
        ),
        (
            "UPDATE lifecycle_steps SET state='armed' WHERE id=(SELECT step_id FROM qualification_case_actions WHERE case_id='restore-1')",
            "UPDATE lifecycle_steps SET state='completed' WHERE id=(SELECT step_id FROM qualification_case_actions WHERE case_id='restore-1')",
        ),
        (
            "UPDATE deployments SET current_generation=current_generation+1",
            "UPDATE deployments SET current_generation=current_generation-1",
        ),
        (
            "UPDATE deployments SET revision=revision+1",
            "UPDATE deployments SET revision=revision-1",
        ),
        (
            "UPDATE deployments SET admission_enabled=1",
            "UPDATE deployments SET admission_enabled=0",
        ),
        (
            "UPDATE deployments SET dispatch_enabled=1",
            "UPDATE deployments SET dispatch_enabled=0",
        ),
        (
            "UPDATE host_resource_policies SET revision=revision+1,policy_json=json_set(policy_json,'$.revision',revision+1)",
            "UPDATE host_resource_policies SET revision=revision-1,policy_json=json_set(policy_json,'$.revision',revision-1)",
        ),
        (
            "UPDATE host_qualification_policies SET revision=revision+1,policy_json=json_set(policy_json,'$.revision',revision+1)",
            "UPDATE host_qualification_policies SET revision=revision-1,policy_json=json_set(policy_json,'$.revision',revision-1)",
        ),
        (
            "UPDATE host_qualification_policies SET policy_json=json_set(policy_json,'$.state.policy.allow_qualification_runs',json('false'))",
            "UPDATE host_qualification_policies SET policy_json=json_set(policy_json,'$.state.policy.allow_qualification_runs',json('true'))",
        ),
    ] {
        // A nonmatching selector would be a vacuous regression test.
        assert!(f.sql.execute(change, []).unwrap() > 0, "{change}");
        let counts = f.counts();
        assert!(finish(&f, "finish", BODY, 1400).is_err(), "{change}");
        assert_eq!(f.counts(), counts);
        f.sql.execute_batch(restore).unwrap();
    }
    let lease = ulid::Ulid::new().to_string();
    f.sql
        .execute(
            "INSERT INTO request_leases VALUES(?1,?2,1,1,?3,'uncertain')",
            rusqlite::params![lease, f.created.deployment_id(), f.session.id()],
        )
        .unwrap();
    let counts = f.counts();
    assert!(finish(&f, "finish", BODY, 1400).is_err());
    assert_eq!(f.counts(), counts);
    f.sql
        .execute("DELETE FROM request_leases WHERE id=?1", [lease])
        .unwrap();
    let reference: (String, String, String, String, String) = f.sql.query_row(
        "SELECT id,run_id,case_id,evidence_digest,metadata_json FROM qualification_evidence_refs LIMIT 1", [],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
    f.sql
        .execute(
            "DELETE FROM qualification_evidence_refs WHERE id=?1",
            [&reference.0],
        )
        .unwrap();
    let counts = f.counts();
    assert!(finish(&f, "finish", BODY, 1400).is_err());
    assert_eq!(f.counts(), counts);
    f.sql
        .execute(
            "INSERT INTO qualification_evidence_refs VALUES(?1,?2,?3,?4,?5)",
            rusqlite::params![
                reference.0,
                reference.1,
                reference.2,
                reference.3,
                reference.4
            ],
        )
        .unwrap();
    finish(&f, "finish", BODY, 1400).unwrap();
}

#[tokio::test]
async fn finish_service_catalog_resolves_only_after_verified_cleanup_and_preserves_v3_reads() {
    let mut f = fixture();
    let fake = FakeEngine::for_qualification();
    f.completed_suite(&fake).await;
    let source_session = f.session.clone();
    f.session = f.store.begin_coordinator_session().unwrap();
    assert!(
        f.store
            .finish_candidate_run_command(
                &source_session,
                "owner",
                f.created.run_id(),
                "finish",
                BODY,
                1400
            )
            .is_err()
    );
    let receipt = finish(&f, "finish", BODY, 1400).unwrap();
    let raw: String = f
        .sql
        .query_row("SELECT record_json FROM qualifications", [], |r| r.get(0))
        .unwrap();
    let record: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(record["session_id"], f.session.id());
    assert_eq!(record["source"]["session_id"], source_session.id());
    assert_eq!(fake.qualification_activity().unwrap(), (12, 7, 10));
    let (fence, binding) = f.fresh_binding(&receipt);
    assert!(
        f.store
            .resolve_ordinary_qualification(
                &f.session,
                receipt.qualification_id(),
                &fence,
                &binding
            )
            .is_err()
    );
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
    assert_eq!(
        f.store
            .resolve_ordinary_qualification(
                &f.session,
                receipt.qualification_id(),
                &fence,
                &binding
            )
            .unwrap(),
        receipt
    );
    let managed = managed(&f, &receipt);
    f.store
        .accept_qualified_start(&f.session, &managed, 1800, 10000)
        .unwrap();
    let legacy = fixture();
    legacy
        .completed_suite(&FakeEngine::for_qualification())
        .await;
    let body = r#"{"expected_revision":1,"action":"finish"}"#;
    let old = legacy
        .store
        .finish_candidate_run(
            &legacy.session,
            "owner",
            legacy.created.run_id(),
            "legacy",
            body,
            1400,
        )
        .unwrap();
    let raw: String = legacy
        .sql
        .query_row("SELECT record_json FROM qualifications", [], |r| r.get(0))
        .unwrap();
    let record: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(record["version"], 3);
    assert!(record.get("command_deadline_ms").is_none());
    assert_eq!(
        legacy
            .store
            .finish_candidate_run(
                &legacy.session,
                "owner",
                legacy.created.run_id(),
                "other-legacy",
                body,
                999999
            )
            .unwrap(),
        old
    );
    assert!(finish(&legacy, "service", BODY, 1400).is_err());
    assert_eq!(
        legacy
            .store
            .read_qualification(&legacy.session, old.qualification_id())
            .unwrap()
            .unwrap(),
        old
    );
    assert_eq!(
        legacy
            .sql
            .query_row("SELECT record_json FROM qualifications", [], |r| r
                .get::<_, String>(0))
            .unwrap(),
        raw
    );
    let bad = raw.replacen('{', "{\"command_deadline_ms\":null,", 1);
    legacy
        .sql
        .execute("UPDATE qualifications SET record_json=?1", [bad])
        .unwrap();
    assert!(
        legacy
            .store
            .read_qualification(&legacy.session, old.qualification_id())
            .is_err()
    );
    legacy
        .sql
        .execute("UPDATE qualifications SET record_json=?1", [raw])
        .unwrap();
}

use super::*;

#[tokio::test]
async fn unarmed_stop_rejects_nontext_history_as_internal_corruption() {
    let (store, session, sql, _dir) = fixture().await;
    let source = fixture_support::owned_source().await;
    let id = &source.fence.deployment_id;
    store
        .accept_qualified_start_command(&session, "owner", id, 1, "start", 1800, 10000)
        .unwrap();
    let stop = store
        .accept_ordinary_stop_command(&session, "owner", id, 1, "stop", 1900, 10000)
        .unwrap();
    for corruption in [
        "UPDATE lifecycle_steps SET step_json=CAST(step_json AS BLOB) WHERE operation_id=?1",
        "UPDATE lifecycle_steps SET step_json=step_json || printf('%1048577s',' ') WHERE operation_id=?1",
        "UPDATE operations SET kind=CAST(kind AS BLOB) WHERE id=?1",
        "UPDATE operations SET kind=printf('%1048577s','x') WHERE id=?1",
    ] {
        let copy=tempfile::tempdir().unwrap();let path=copy.path().join("corrupt.sqlite3");sql.execute("VACUUM INTO ?1",[path.to_str().unwrap()]).unwrap();
        let candidate=Store::open(&path).unwrap();let corrupt=rusqlite::Connection::open(path).unwrap();corrupt.execute(corruption,[&stop.operation_id]).unwrap();
        let before=state(&corrupt);
        assert!(matches!(candidate.accept_ordinary_stop_command(&session,"owner",id,1,"stop",1900,10000),Err(LifecycleError::CorruptStoredData)),"{corruption}");
        assert_eq!(state(&corrupt),before);
    }
}

#[tokio::test]
async fn unarmed_stop_serializes_with_arm_and_expiry_on_independent_connections() {
    for competing_arm in [true, false] {
        let (store, session, sql, dir) = fixture().await;
        let source = fixture_support::owned_source().await;
        let id = source.fence.deployment_id.clone();
        let start = store
            .accept_qualified_start_command(&session, "owner", &id, 1, "start", 1800, 1901)
            .unwrap();
        let (limits, ttl, max_parked) = super::expired_unarmed::limits(&store, &sql, &id);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let other = Store::open(&dir.path().join("start.sqlite3")).unwrap();
        let other_session = session.clone();
        let other_step = start.step_id().to_owned();
        let other_barrier = barrier.clone();
        let observations = source.observations.clone();
        let competing = std::thread::spawn(move || {
            other_barrier.wait();
            if competing_arm {
                other
                    .arm_qualified_initialize_with_context(
                        &other_session,
                        &other_step,
                        AdmissionContext::new(&observations, &limits, 1900, ttl, max_parked),
                    )
                    .map(|_| ())
            } else {
                other
                    .expire_unarmed_qualified_initialize(&other_session, &other_step, 1901)
                    .map(|_| ())
            }
        });
        let stopper = Store::open(&dir.path().join("start.sqlite3")).unwrap();
        let stop_session = session.clone();
        let stop_id = id.clone();
        let stop = std::thread::spawn(move || {
            barrier.wait();
            stopper.accept_ordinary_stop_command(
                &stop_session,
                "owner",
                &stop_id,
                1,
                "stop",
                1900,
                10000,
            )
        });
        let competing = competing.join().unwrap();
        let stop = stop.join().unwrap();
        assert_ne!(competing.is_ok(), stop.is_ok());
        if let Ok(stop) = stop {
            assert!(store
                .complete_unarmed_stop(&session, &stop.step_id)
                .unwrap());
            assert!(store.resource_snapshot().unwrap().owners.is_empty());
            assert!(store.runtime_binding(&id).unwrap().is_none());
        } else if competing_arm {
            let before = state(&sql);
            assert!(store
                .accept_ordinary_stop_command(&session, "owner", &id, 1, "retry", 1900, 10000)
                .is_err());
            assert_eq!(state(&sql), before);
            assert_eq!(
                store.runtime_binding(&id).unwrap().unwrap().state,
                "uncertain"
            );
            assert!(store.resource_snapshot().unwrap().owners.contains_key(&id));
        } else {
            assert_eq!(
                sql.query_row(
                    "SELECT error_code FROM operations WHERE id=?1",
                    [start.operation_id()],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
                "deadline_expired_unarmed"
            );
        }
    }
}

#[tokio::test]
async fn unarmed_stop_terminal_history_rejects_contradictions() {
    let (store, session, sql, _dir) = fixture().await;
    let source = fixture_support::owned_source().await;
    let id = &source.fence.deployment_id;
    let start = store
        .accept_qualified_start_command(&session, "owner", id, 1, "start", 1800, 10000)
        .unwrap();
    let stop = store
        .accept_ordinary_stop_command(&session, "owner", id, 1, "stop", 1900, 10000)
        .unwrap();
    store
        .complete_unarmed_stop(&session, &stop.step_id)
        .unwrap();
    for corruption in [
        "UPDATE operations SET error_code='deadline_expired_unarmed' WHERE id='$SOURCE'",
        "UPDATE lifecycle_steps SET state='planned' WHERE operation_id='$SOURCE'",
        "UPDATE lifecycle_runs SET state='queued' WHERE operation_id='$SOURCE'",
        "UPDATE operations SET state='pending' WHERE id='$STOP'",
        "UPDATE lifecycle_steps SET state='planned' WHERE operation_id='$STOP'",
        "UPDATE lifecycle_runs SET plan_json=json_set(plan_json,'$.handoffs[0].steps[0].state','armed') WHERE operation_id='$STOP'",
        "UPDATE runtime_bindings SET state='reserved' WHERE id=(SELECT binding_id FROM lifecycle_steps WHERE operation_id='$SOURCE')",
        "INSERT INTO lifecycle_claims SELECT deployment_id,operation_id,revision,generation FROM lifecycle_runs WHERE operation_id='$STOP'",
        "INSERT INTO lifecycle_claims SELECT deployment_id,operation_id,revision,generation FROM lifecycle_runs WHERE operation_id='$SOURCE'",
        "INSERT INTO resource_owners SELECT deployment_id,'{}' FROM lifecycle_runs WHERE operation_id='$SOURCE'",
        "INSERT INTO request_leases SELECT 'old-lease',deployment_id,revision,generation,session_id,'uncertain' FROM lifecycle_runs WHERE operation_id='$SOURCE'",
        "INSERT INTO lifecycle_evidence SELECT id,'{}',0 FROM lifecycle_steps WHERE operation_id='$STOP'",
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.kind','unknown') WHERE idempotency_key='stop'",
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.kind','ordinary_cleanup') WHERE idempotency_key='stop'",
        "UPDATE command_receipts SET response_json=json_set(response_json,'$.extra',1) WHERE idempotency_key='stop'",
        "UPDATE operations SET kind='unknown' WHERE id='$STOP'",
        "INSERT INTO lifecycle_steps SELECT 'extra-step',operation_id,1,deployment_id,binding_id,session_id,'planned',step_json,NULL FROM lifecycle_steps WHERE operation_id='$STOP'",
    ] {
        let copy=tempfile::tempdir().unwrap();let path=copy.path().join("corrupt.sqlite3");sql.execute("VACUUM INTO ?1",[path.to_str().unwrap()]).unwrap();
        let candidate=Store::open(&path).unwrap();let corrupt=rusqlite::Connection::open(path).unwrap();corrupt.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        corrupt.execute_batch(&corruption.replace("$SOURCE",start.operation_id()).replace("$STOP",&stop.operation_id)).unwrap();
        let before=state(&corrupt);
        assert!(candidate.complete_unarmed_stop(&session,&stop.step_id).is_err(),"{corruption}");
        assert!(matches!(candidate.accept_ordinary_stop_command(&session,"owner",id,1,"stop",90000,10000),Err(LifecycleError::CorruptStoredData)),"{corruption}");
        assert_eq!(state(&corrupt),before,"{corruption}");
    }
}

fn state(sql: &rusqlite::Connection) -> Vec<String> {
    [
        "deployments",
        "generation_history",
        "operations",
        "lifecycle_runs",
        "lifecycle_steps",
        "runtime_bindings",
        "endpoint_leases",
        "lifecycle_claims",
        "command_receipts",
        "management_events",
        "resource_ledger_meta",
        "resource_grants",
        "resource_owners",
        "request_leases",
        "owned_launch_associations",
        "lifecycle_evidence",
    ]
    .into_iter()
    .flat_map(|table| {
        let mut statement = sql
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .unwrap();
        let count = statement.column_count();
        statement
            .query_map([], |r| {
                Ok((0..count)
                    .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                    .collect::<Vec<_>>()
                    .join("|"))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    })
    .collect()
}

#[tokio::test]
async fn unarmed_stop_contradictions_fail_closed_at_acceptance_and_completion() {
    let (store, session, sql, _dir) = fixture().await;
    let source = fixture_support::owned_source().await;
    let id = &source.fence.deployment_id;
    let start = store
        .accept_qualified_start_command(&session, "owner", id, 1, "start", 1800, 10000)
        .unwrap();
    for accepted in [false, true] {
        let stop = accepted.then(|| {
            store
                .accept_ordinary_stop_command(&session, "owner", id, 1, "stop", 1900, 10000)
                .unwrap()
        });
        for corruption in [
            "UPDATE deployments SET revision=revision+1 WHERE id=(SELECT deployment_id FROM lifecycle_runs WHERE operation_id='$SOURCE')",
            "UPDATE deployments SET current_generation=current_generation+1 WHERE id=(SELECT deployment_id FROM lifecycle_runs WHERE operation_id='$SOURCE')",
            "UPDATE lifecycle_claims SET generation=generation+1",
            "DELETE FROM lifecycle_claims",
            "UPDATE lifecycle_claims SET operation_id='wrong-claim'",
            "UPDATE lifecycle_steps SET step_json=json_set(step_json,'$.execution',json('{}')) WHERE operation_id='$SOURCE'",
            "UPDATE lifecycle_steps SET state='armed' WHERE operation_id='$SOURCE'",
            "UPDATE lifecycle_steps SET session_id='other-session' WHERE operation_id='$SOURCE'",
            "UPDATE runtime_bindings SET identities_json='[{}]' WHERE state='reserved'",
            "UPDATE runtime_bindings SET incarnation='contradiction' WHERE state='reserved'",
            "UPDATE runtime_bindings SET ownership='attached' WHERE state='reserved'",
            "UPDATE endpoint_leases SET port=port+1",
            "DELETE FROM endpoint_leases",
            "UPDATE deployments SET dispatch_enabled=1 WHERE id=(SELECT deployment_id FROM lifecycle_runs WHERE operation_id='$SOURCE')",
            "UPDATE deployments SET observed_state='ready' WHERE id=(SELECT deployment_id FROM lifecycle_runs WHERE operation_id='$SOURCE')",
            "UPDATE deployments SET suspended=1 WHERE id=(SELECT deployment_id FROM lifecycle_runs WHERE operation_id='$SOURCE')",
            "INSERT INTO request_leases SELECT 'retained-lease',deployment_id,revision,generation,session_id,'uncertain' FROM lifecycle_runs WHERE operation_id='$SOURCE'",
            "INSERT INTO resource_grants SELECT 'retained-grant',deployment_id,operation_id,'{}',999999 FROM lifecycle_runs WHERE operation_id='$SOURCE'",
            "INSERT INTO resource_owners SELECT deployment_id,'{}' FROM lifecycle_runs WHERE operation_id='$SOURCE'",
            "INSERT INTO owned_launch_associations SELECT id,binding_id,'contradiction','{}' FROM lifecycle_steps WHERE operation_id='$SOURCE'",
            "INSERT INTO lifecycle_evidence SELECT id,'{}',0 FROM lifecycle_steps WHERE operation_id='$SOURCE'",
            "UPDATE lifecycle_steps SET grant_id='retained-grant' WHERE operation_id='$SOURCE'",
            "INSERT INTO lifecycle_steps SELECT 'extra-step',operation_id,1,deployment_id,binding_id,session_id,'planned',step_json,NULL FROM lifecycle_steps WHERE operation_id='$SOURCE'",
            "INSERT INTO lifecycle_steps SELECT 'extra-step',operation_id,1,deployment_id,binding_id,session_id,'cancelled',step_json,NULL FROM lifecycle_steps WHERE operation_id='$SOURCE'",
            "INSERT INTO lifecycle_steps SELECT 'extra-step',(SELECT operation_id FROM lifecycle_runs WHERE operation_id!='$SOURCE' LIMIT 1),99,deployment_id,binding_id,session_id,'planned',step_json,NULL FROM lifecycle_steps WHERE operation_id='$SOURCE'",
            "INSERT INTO lifecycle_steps SELECT 'extra-step',(SELECT operation_id FROM lifecycle_runs WHERE operation_id!='$SOURCE' LIMIT 1),99,deployment_id,binding_id,session_id,'cancelled',step_json,NULL FROM lifecycle_steps WHERE operation_id='$SOURCE'",
        ] {
            let copy=tempfile::tempdir().unwrap();let path=copy.path().join("corrupt.sqlite3");
            sql.execute("VACUUM INTO ?1",[path.to_str().unwrap()]).unwrap();
            let candidate=Store::open(&path).unwrap();let corrupt=rusqlite::Connection::open(path).unwrap();
            corrupt.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
            corrupt.execute_batch(&corruption.replace("$SOURCE",start.operation_id())).unwrap();
            let before=state(&corrupt);
            let denied=if let Some(stop)=&stop {candidate.complete_unarmed_stop(&session,&stop.step_id).is_err()}else {candidate.accept_ordinary_stop_command(&session,"owner",id,1,"stop",1900,10000).is_err()};
            assert!(denied,"accepted={accepted}: {corruption}");
            assert_eq!(state(&corrupt),before,"accepted={accepted}: {corruption}");
        }
    }
    let stop = store
        .ordinary_stop_command_receipt(&session, "owner", id, 1, "stop", 10000)
        .unwrap()
        .unwrap();
    for corruption in [
        "UPDATE lifecycle_steps SET state='armed' WHERE operation_id=?1",
        "UPDATE lifecycle_steps SET state='uncertain' WHERE operation_id=?1",
        "UPDATE lifecycle_steps SET state='cancelled' WHERE operation_id=?1",
        "UPDATE lifecycle_steps SET grant_id='retained-grant' WHERE operation_id=?1",
        "INSERT INTO lifecycle_evidence SELECT id,'{}',0 FROM lifecycle_steps WHERE operation_id=?1",
        "INSERT INTO resource_grants SELECT 'retained-grant',deployment_id,operation_id,'{}',999999 FROM lifecycle_runs WHERE operation_id=?1",
        "UPDATE lifecycle_steps SET step_json=json_set(step_json,'$.source.generation',999) WHERE operation_id=?1",
        "UPDATE lifecycle_steps SET step_json=json_set(step_json,'$.receipt.incarnation','wrong') WHERE operation_id=?1",
        "INSERT INTO lifecycle_steps SELECT 'extra-stop',operation_id,1,deployment_id,binding_id,session_id,'planned',step_json,NULL FROM lifecycle_steps WHERE operation_id=?1",
    ] {
        let copy=tempfile::tempdir().unwrap();let path=copy.path().join("corrupt.sqlite3");sql.execute("VACUUM INTO ?1",[path.to_str().unwrap()]).unwrap();
        let candidate=Store::open(&path).unwrap();let corrupt=rusqlite::Connection::open(path).unwrap();corrupt.execute_batch("PRAGMA foreign_keys=OFF").unwrap();corrupt.execute(corruption,[&stop.operation_id]).unwrap();
        let before=state(&corrupt);
        assert!(candidate.complete_unarmed_stop(&session,&stop.step_id).is_err(),"{corruption}");
        assert_eq!(state(&corrupt),before);
    }
    let current = store.begin_coordinator_session().unwrap();
    let before = state(&sql);
    assert!(store
        .complete_unarmed_stop(&session, &stop.step_id)
        .is_err());
    assert!(store
        .complete_unarmed_stop(&current, &stop.step_id)
        .is_err());
    assert!(store
        .accept_ordinary_stop_command(&current, "owner", id, 1, "fresh", 1900, 10000)
        .is_err());
    assert_eq!(state(&sql), before);
    assert_eq!(
        store
            .ordinary_stop_command_receipt(&current, "owner", id, 1, "stop", 10000)
            .unwrap()
            .unwrap(),
        stop
    );
}

#[tokio::test]
async fn unarmed_stop_rolls_back_receipt_and_events_and_replays_after_replacement() {
    let (store, session, sql, _dir) = fixture().await;
    let source = fixture_support::owned_source().await;
    let id = &source.fence.deployment_id;
    let start = store
        .accept_qualified_start_command(&session, "owner", id, 1, "start", 1800, 1901)
        .unwrap();
    for (table, condition) in [
        ("command_receipts", "NEW.idempotency_key='stop'"),
        (
            "management_events",
            "NEW.kind='ordinary_unarmed_stop_accepted'",
        ),
    ] {
        sql.execute_batch(&format!("CREATE TRIGGER failure BEFORE INSERT ON {table} WHEN {condition} BEGIN SELECT RAISE(ABORT,'rollback'); END;")).unwrap();
        let before = state(&sql);
        assert!(matches!(
            store.accept_ordinary_stop_command(&session, "owner", id, 1, "stop", 1900, 10000),
            Err(LifecycleError::Sql(_))
        ));
        assert_eq!(state(&sql), before);
        sql.execute_batch("DROP TRIGGER failure").unwrap();
    }
    let stop = store
        .accept_ordinary_stop_command(&session, "owner", id, 1, "stop", 1900, 10000)
        .unwrap();
    let ledger = store.resource_snapshot().unwrap();
    let before = state(&sql);
    assert!(store
        .expire_unarmed_qualified_initialize(&session, start.step_id(), 1901)
        .is_err());
    assert_eq!(state(&sql), before);
    for (key, action) in [("start", "stop"), ("stop", "start")] {
        let error = if action == "stop" {
            store
                .accept_ordinary_stop_command(&session, "owner", id, 1, key, 1900, 1901)
                .unwrap_err()
        } else {
            store
                .accept_qualified_start_command(&session, "owner", id, 1, key, 1900, 10000)
                .unwrap_err()
        };
        assert!(matches!(error, LifecycleError::IdempotencyConflict));
    }
    assert!(store
        .accept_ordinary_stop_command(&session, "owner", id, 1, "different", 1900, 10000)
        .is_err());
    sql.execute_batch("CREATE TRIGGER failure BEFORE INSERT ON management_events WHEN NEW.kind='ordinary_unarmed_stop_completed' BEGIN SELECT RAISE(ABORT,'rollback'); END;").unwrap();
    let before = state(&sql);
    assert!(matches!(
        store.complete_unarmed_stop(&session, &stop.step_id),
        Err(LifecycleError::Sql(_))
    ));
    assert_eq!(state(&sql), before);
    sql.execute_batch("DROP TRIGGER failure").unwrap();
    assert!(store
        .complete_unarmed_stop(&session, &stop.step_id)
        .unwrap());
    assert_eq!(store.resource_snapshot().unwrap(), ledger);
    let before = state(&sql);
    assert!(!store
        .complete_unarmed_stop(&session, &stop.step_id)
        .unwrap());
    assert_eq!(
        store
            .accept_ordinary_stop_command(&session, "owner", id, 1, "stop", 90000, 10000)
            .unwrap(),
        stop
    );
    assert_eq!(state(&sql), before);
    let golden: Value = serde_json::from_str(include_str!(
        "../../../mllm-config/tests/fixtures/effective-fake-golden.json"
    ))
    .unwrap();
    let raw: String = sql
        .query_row(
            "SELECT effective_json FROM effective_revisions WHERE deployment_id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    let effective: Value = serde_json::from_str(&raw).unwrap();
    let mut config = golden["input"]["deployment"].clone();
    let mut host = golden["input"]["host"].clone();
    config["name"] = json!("ordinary");
    config["routes"] = json!(["ordinary-stop-replaced"]);
    host["runtime_profiles"]["local"]["build_fingerprint"] = json!("qualification-fake-v1");
    host["runtime_profiles"]["local"]["security"]["admin_credential_ref"] =
        json!("secret://another-admin");
    host["runtime_profiles"]["local"]["qualification_id"] =
        effective["profile"]["qualification_id"].clone();
    store
        .replace_stopped_managed_configuration(
            &session,
            "owner",
            "replace",
            id,
            &json!({"expected_revision":1,"config":config}).to_string(),
            &host,
            2000,
        )
        .unwrap();
    let replacement = store
        .accept_qualified_start_command(&session, "owner", id, 2, "replacement", 2000, 10000)
        .unwrap();
    let (limits, ttl, max_parked) = super::expired_unarmed::limits(&store, &sql, id);
    store
        .arm_qualified_initialize_with_context(
            &session,
            replacement.step_id(),
            AdmissionContext::new(&source.observations, &limits, 2000, ttl, max_parked),
        )
        .unwrap();
    let retained = store.resource_snapshot().unwrap();
    assert!(retained.owners.contains_key(id));
    assert_eq!(
        store
            .accept_ordinary_stop_command(&session, "owner", id, 1, "stop", 90000, 10000)
            .unwrap(),
        stop
    );
    assert!(!store
        .complete_unarmed_stop(&session, &stop.step_id)
        .unwrap());
    assert_eq!(
        store.runtime_binding(id).unwrap().unwrap().id,
        replacement.binding_id()
    );
    assert_eq!(store.resource_snapshot().unwrap(), retained);
    assert_eq!(
        store
            .qualified_initialize_status(&session, start.step_id(), 2000)
            .unwrap(),
        mllm_store::ordinary_lifecycle::worker::QualifiedInitializeStatus::Superseded
    );
    let new_session = store.begin_coordinator_session().unwrap();
    assert_eq!(
        store
            .accept_ordinary_stop_command(&new_session, "owner", id, 1, "stop", 90000, 10000)
            .unwrap(),
        stop
    );
    assert!(!store
        .complete_unarmed_stop(&new_session, &stop.step_id)
        .unwrap());
    assert!(store
        .complete_unarmed_stop(&session, &stop.step_id)
        .is_err());
}

// The handoff must record that the predecessor never armed. A generic cleanup
// validator accepting an armed historical step must not permit this release.
#[tokio::test]
async fn unarmed_stop_rejects_armed_predecessor_history_before_release() {
    let (store, session, sql, _dir) = fixture().await;
    let source = fixture_support::owned_source().await;
    store
        .accept_qualified_start_command(
            &session,
            "owner",
            &source.fence.deployment_id,
            1,
            "start",
            1800,
            10000,
        )
        .unwrap();
    let stop = store
        .accept_ordinary_stop_command(
            &session,
            "owner",
            &source.fence.deployment_id,
            1,
            "stop",
            1900,
            10000,
        )
        .unwrap();
    sql.execute("UPDATE lifecycle_runs SET plan_json=json_set(plan_json,'$.handoffs[0].steps[0].state','armed') WHERE operation_id=?1",[&stop.operation_id]).unwrap();
    let before = counts(&sql);
    assert!(
        store
            .complete_unarmed_stop(&session, &stop.step_id)
            .is_err(),
        "armed predecessor history must deny no-effect release"
    );
    assert_eq!(counts(&sql), before);
    assert_eq!(
        store
            .runtime_binding(&source.fence.deployment_id)
            .unwrap()
            .unwrap()
            .state,
        "reserved"
    );
}

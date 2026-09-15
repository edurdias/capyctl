use super::*;

#[tokio::test]
async fn worker_selects_oldest_current_generation_without_adopting_superseded_work() {
    let (f, catalog) = ordinary_initialize::qualified().await;
    let first = ordinary_initialize::managed_edit(&f, &catalog, "first", |_, _| {});
    let second = ordinary_initialize::managed_edit(&f, &catalog, "second", |_, _| {});
    let a = f
        .store
        .accept_qualified_start(&f.session, &first, 1800, 10000)
        .unwrap();
    let b = f
        .store
        .accept_qualified_start(&f.session, &second, 1801, 10001)
        .unwrap();
    // Control only acceptance ordering; all authorization/evidence came through writers.
    f.sql
        .execute(
            "UPDATE operations SET accepted_at='2026-09-15T00:00:00Z' WHERE id=?1",
            [&a.operation_id],
        )
        .unwrap();
    f.sql
        .execute(
            "UPDATE operations SET accepted_at='2026-09-15T00:00:01Z' WHERE id=?1",
            [&b.operation_id],
        )
        .unwrap();
    assert_eq!(
        f.store
            .next_qualified_initialize(&f.session)
            .unwrap()
            .unwrap()
            .operation_id(),
        a.operation_id
    );
    f.store.fence_stop(&f.session, &first, 10000).unwrap();
    assert_eq!(
        f.store
            .next_qualified_initialize(&f.session)
            .unwrap()
            .unwrap()
            .operation_id(),
        b.operation_id
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM endpoint_leases"), 2);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM lifecycle_claims"), 2);
    assert!(f.store.resource_snapshot().unwrap().owners.is_empty());
    // A new coordinator may inspect/reconcile the reservations, never silently adopt them.
    let next = f.store.begin_coordinator_session().unwrap();
    assert!(f.store.next_qualified_initialize(&next).unwrap().is_none());
    assert_eq!(f.scalar("SELECT COUNT(*) FROM endpoint_leases"), 2);
}

#[tokio::test]
async fn worker_selection_and_uncertainty_preserve_exact_durable_intent() {
    let (f, catalog) = ordinary_initialize::qualified().await;
    assert!(
        f.store
            .next_qualified_initialize(&f.session)
            .unwrap()
            .is_none()
    );
    let fence = ordinary_initialize::managed(&f, &catalog);
    let accepted = f
        .store
        .accept_qualified_start(&f.session, &fence, 1800, 10000)
        .unwrap();
    let before = f.counts();
    let work = f
        .store
        .next_qualified_initialize(&f.session)
        .unwrap()
        .unwrap();
    assert_eq!(work.operation_id(), accepted.operation_id);
    assert_eq!(work.step_id(), accepted.step_id);
    assert_eq!(work.binding_id(), accepted.binding_id);
    assert_eq!(work.fence(), &fence);
    assert_eq!(work.deadline_ms(), 10000);
    assert_eq!(work.effective().name, "ordinary");
    assert_eq!(work.policy().revision, 1);
    assert_eq!(f.counts(), before);
    let stored_plan: String = f
        .sql
        .query_row(
            "SELECT step_json FROM lifecycle_steps WHERE id=?1",
            [&accepted.step_id],
            |row| row.get(0),
        )
        .unwrap();
    f.sql
        .execute(
            "UPDATE lifecycle_steps SET step_json='{}' WHERE id=?1",
            [&accepted.step_id],
        )
        .unwrap();
    assert!(f.store.next_qualified_initialize(&f.session).is_err());
    f.sql
        .execute(
            "UPDATE lifecycle_steps SET step_json=?2 WHERE id=?1",
            rusqlite::params![accepted.step_id, stored_plan],
        )
        .unwrap();
    assert!(
        f.store
            .mark_qualified_initialize_uncertain(&f.session, &accepted.step_id, 1900)
            .is_err()
    );
    let mut admission = f.admission();
    admission.now_ms = 1900;
    assert!(matches!(
        f.store
            .arm_step(&f.session, &accepted.step_id, admission)
            .unwrap(),
        ArmResult::New { .. }
    ));
    assert!(
        f.store
            .next_qualified_initialize(&f.session)
            .unwrap()
            .is_none()
    );
    let charged = f.store.resource_snapshot().unwrap();
    f.sql.execute_batch("CREATE TRIGGER uncertain_event_failure BEFORE INSERT ON management_events WHEN NEW.kind='qualified_initialize_uncertain' BEGIN SELECT RAISE(ABORT,'uncertain event failure'); END;").unwrap();
    assert!(
        f.store
            .mark_qualified_initialize_uncertain(&f.session, &accepted.step_id, 10001)
            .is_err()
    );
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM lifecycle_steps WHERE state='armed'"),
        1
    );
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM lifecycle_runs WHERE state='running'"),
        1
    );
    assert_eq!(f.store.resource_snapshot().unwrap(), charged);
    f.sql
        .execute_batch("DROP TRIGGER uncertain_event_failure;")
        .unwrap();
    assert!(
        f.store
            .mark_qualified_initialize_uncertain(&f.session, &accepted.step_id, 10001)
            .unwrap()
    );
    assert!(
        !f.store
            .mark_qualified_initialize_uncertain(&f.session, &accepted.step_id, 10002)
            .unwrap()
    );
    assert_eq!(f.store.resource_snapshot().unwrap(), charged);
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM lifecycle_steps WHERE state='uncertain'"),
        1
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM lifecycle_claims"), 1);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM endpoint_leases"), 1);
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM management_events WHERE kind='qualified_initialize_uncertain'"
        ),
        1
    );
    assert_eq!(
        f.store
            .arm_step(&f.session, &accepted.step_id, admission)
            .unwrap(),
        ArmResult::AlreadyRecorded
    );
    let session = f.store.begin_coordinator_session().unwrap();
    assert!(f.store.next_qualified_initialize(&f.session).is_err());
    assert!(
        f.store
            .mark_qualified_initialize_uncertain(&session, &accepted.step_id, 10003)
            .is_err()
    );
    assert_eq!(f.store.resource_snapshot().unwrap(), charged);
}

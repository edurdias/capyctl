//! Real candidate writers and opt-in Fake execution; no catalog fixtures.
use mllm_adapters::{fake::FakeEngine, traits::EngineAdapter};
use mllm_controller::{RuntimeAction, RuntimeCommand};
use mllm_domain::completion::{CompletionEvidence, Milestone, OwnedLaunchReceipt};
use mllm_domain::resources::{MemoryObservation, ResourcePhase};
use mllm_scheduler::residency::AdmissionContext;
use mllm_store::candidate_creation::progression::CandidateDispatchResult;
use mllm_store::{candidate_creation::initialize::ArmResult, Store};
use serde_json::{json, Value};

#[path = "qualification_support/ordinary_initialize.rs"]
mod ordinary_initialize;
#[path = "qualification_support/ordinary_cleanup.rs"]
mod ordinary_cleanup;
#[path = "qualification_support/start_receipts.rs"]
mod start_receipts;

#[path = "qualification_support/worker_store.rs"]
mod worker_store;

#[path = "qualification_support/owned_worker.rs"]
mod owned_worker;

#[path = "qualification_support/candidate_worker.rs"]
mod candidate_worker;

#[path = "qualification_support/security_owned.rs"]
mod security_owned;

#[path = "qualification_support/fixture.rs"]
mod fixture_support;
use fixture_support::*;

// Semantic equality does not authorize extra wire bytes beyond the run bound.
#[tokio::test]
async fn public_marker_dispatch_enforces_actual_frozen_body_size() {
    let f = fixture_custom(9663676416, 10737418240, |v| {
        v["limits"]["max_request_body_bytes"] = json!(256);
        v["limits"]["max_input_tokens_per_request"] = json!(29);
        v["limits"]["max_output_tokens_per_request"] = json!(16);
    });
    let fake = FakeEngine::for_qualification();
    f.ready(&fake).await;
    let body = marker_body(&f, "MLLM_ALPHA_71", false);
    assert!(body.len() < 256);
    let oversized = format!("{}{}", body, " ".repeat(257 - body.len()));
    let before = f.counts();
    assert!(matches!(
        f.store.grant_candidate_inference(
            &f.session,
            "owner",
            f.created.run_id(),
            "marker",
            &oversized,
            f.admission()
        ),
        Err(mllm_store::lifecycle::LifecycleError::Invalid)
    ));
    assert_eq!(f.counts(), before);
    assert_eq!(f.scalar("SELECT requests_used FROM qualification_runs"), 1);
    assert!(matches!(
        f.store
            .grant_candidate_inference(
                &f.session,
                "owner",
                f.created.run_id(),
                "marker",
                &body,
                f.admission()
            )
            .unwrap(),
        CandidateDispatchResult::New(_)
    ));
    assert!(f
        .store
        .grant_candidate_inference(
            &f.session,
            "owner",
            f.created.run_id(),
            "marker",
            &oversized,
            f.admission()
        )
        .is_err());
}

// Corruption must return an error, never abort the coordinator. The disposable
// child confines the pre-fix stack overflow and keeps the regression observable.
#[tokio::test]
async fn corrupt_warm_predecessor_self_and_cross_links_are_bounded() {
    const CHILD: &str = "MLLM_TEST_CORRUPT_WARM_LINK";
    let Ok(kind) = std::env::var(CHILD) else {
        for kind in ["self", "cross", "marker"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "corrupt_warm_predecessor_self_and_cross_links_are_bounded",
                    "--nocapture",
                ])
                .env(CHILD, kind)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{kind}: child {:?}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return;
    };
    let f = if kind == "marker" {
        fixture_custom(9663676416, 10737418240, |v| {
            let mut extra = v["cases"].as_array().unwrap()[5..].to_vec();
            for case in &mut extra {
                case["cycle"] = json!(2);
                case["id"] = json!(case["id"].as_str().unwrap().replace("-1", "-2"));
            }
            v["cases"].as_array_mut().unwrap().extend(extra);
            v["limits"]["max_requests"] = json!(17);
        })
    } else {
        fixture()
    };
    let fake = FakeEngine::for_qualification();
    // All link targets originate in actual writers and real Fake execution.
    if kind == "marker" {
        f.completed_suite(&fake).await;
    } else {
        let collector = f.secured(&fake).await;
        let park = f
            .store
            .accept_candidate_action(
                &f.session,
                "owner",
                f.created.run_id(),
                "park",
                r#"{"expected_revision":1,"action":"park","deadline_ms":400000}"#,
                1200,
            )
            .unwrap();
        if kind == "cross" {
            for (child, action) in park
                .effect_ids()
                .iter()
                .zip([RuntimeAction::Drain, RuntimeAction::Park])
            {
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
                let o = fake
                    .execute_persisted(&RuntimeCommand { action, context })
                    .await
                    .unwrap();
                f.store
                    .record_candidate_effect(&f.session, &collector, child, &o, 1300)
                    .unwrap();
            }
            let context = f
                .store
                .candidate_parked_status_execution(&f.session, park.step_id(), 1200)
                .unwrap();
            let o = mllm_controller::qualification::collect_parked_status(&fake, &context).unwrap();
            f.store
                .record_candidate_parked_status(&f.session, &collector, &o, 1300)
                .unwrap();
            f.store
                .accept_candidate_action(
                    &f.session,
                    "owner",
                    f.created.run_id(),
                    "restore",
                    r#"{"expected_revision":1,"action":"restore","deadline_ms":400000}"#,
                    1200,
                )
                .unwrap();
        }
    }
    if kind == "marker" {
        let body = r#"{"expected_revision":1,"action":"park","deadline_ms":400000}"#;
        let park = f
            .store
            .accept_candidate_action(
                &f.session,
                "owner",
                f.created.run_id(),
                "park-2",
                body,
                1400,
            )
            .unwrap();
        f.sql.execute("UPDATE qualification_ready_probes SET parent_step_id=?1 WHERE case_id='ready_probe-1'",[park.step_id()]).unwrap();
        assert!(matches!(
            f.store.accept_candidate_action(
                &f.session,
                "owner",
                f.created.run_id(),
                "park-2",
                body,
                999999
            ),
            Err(mllm_store::lifecycle::LifecycleError::CorruptStoredData)
        ));
        return;
    }
    let target = if kind == "self" {
        "park-1"
    } else {
        "restore-1"
    };
    let (operation, anchor): (String, String) = f
        .sql
        .query_row(
            "SELECT operation_id,step_id FROM qualification_case_actions WHERE case_id=?1",
            [target],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    f.sql
        .execute(
            "DELETE FROM qualification_case_actions WHERE case_id=?1",
            [target],
        )
        .unwrap();
    f.sql.execute("UPDATE qualification_case_actions SET operation_id=?1,step_id=?2 WHERE case_id='security-0'",rusqlite::params![operation,anchor]).unwrap();
    let result = f.store.accept_candidate_action(
        &f.session,
        "owner",
        f.created.run_id(),
        "park",
        r#"{"expected_revision":1,"action":"park","deadline_ms":400000}"#,
        999999,
    );
    assert!(
        matches!(
            result,
            Err(mllm_store::lifecycle::LifecycleError::CorruptStoredData)
        ),
        "{result:?}"
    );
}

// Catches rejecting session-induced uncertainty as corrupt evidence, including
// a parent restarted between checks while its next child remains planned.
#[tokio::test]
async fn security_cleanup_accepted_before_restart_keeps_current_uncertain_lease() {
    for subcheck in [1, 2] {
        security_cleanup_handoff_restart(subcheck, false).await;
    }
}

#[tokio::test]
async fn security_cleanup_executed_before_restart_recovers_by_inspection_only() {
    for subcheck in [1, 2] {
        security_cleanup_handoff_restart(subcheck, true).await;
    }
}

// Catches choosing the current lease disposition from the handoff's historical
// armed envelope instead of the independently persisted session rollover.
async fn security_cleanup_handoff_restart(subcheck: usize, execute_cleanup: bool) {
    use mllm_store::candidate_creation::{
        cleanup::CleanupMode, progression::CandidateSecurityDispatch,
    };
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    let collector = f.baseline(&fake).await;
    for current in 0..=subcheck {
        match f
            .store
            .advance_candidate_security(&f.session, &collector, f.admission())
            .unwrap()
        {
            CandidateSecurityDispatch::NewControl(d) => {
                let o = mllm_controller::qualification::collect_security_control(&fake, *d)
                    .await
                    .unwrap();
                f.store
                    .record_candidate_security_control(&f.session, &collector, &o, 1300)
                    .unwrap();
            }
            CandidateSecurityDispatch::NewRequest(d) => {
                let o = mllm_controller::qualification::collect_probe(&fake, *d)
                    .await
                    .unwrap();
                if current < subcheck {
                    f.store
                        .record_candidate_result(&f.session, &collector, &o, 1300)
                        .unwrap();
                }
                // The selected real request executes, but its reply is lost.
            }
            _ => panic!("missing original Security subcheck {current}"),
        }
    }
    let expected_spend = if subcheck == 1 { 6 } else { 7 };
    assert_eq!(
        f.scalar("SELECT requests_used FROM qualification_runs"),
        expected_spend
    );
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM request_leases WHERE disposition='inflight'"),
        1
    );
    let envelopes:Vec<(String,String)>=f.sql.prepare("SELECT id,step_json FROM lifecycle_steps WHERE operation_id IN (SELECT operation_id FROM lifecycle_runs WHERE action='prepare') ORDER BY ordinal").unwrap().query_map([],|r|Ok((r.get(0)?,r.get(1)?))).unwrap().collect::<Result<_,_>>().unwrap();
    let body = r#"{"expected_revision":1,"action":"cleanup","deadline_ms":60000}"#;
    let first = f
        .store
        .accept_candidate_cleanup(
            &f.session,
            "owner",
            f.created.run_id(),
            "cleanup",
            body,
            1400,
        )
        .unwrap();
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM request_leases WHERE disposition='inflight'"),
        1
    );
    let first_plan: String = f
        .sql
        .query_row(
            "SELECT plan_json FROM lifecycle_runs WHERE operation_id=?1",
            [first.operation_id()],
            |r| r.get(0),
        )
        .unwrap();
    let sends = fake.qualification_activity().unwrap();
    if execute_cleanup {
        assert!(matches!(
            f.store
                .arm_candidate_cleanup(&f.session, first.step_id(), 1450)
                .unwrap(),
            ArmResult::New { .. }
        ));
        let context = f
            .store
            .candidate_cleanup_execution(&f.session, first.step_id())
            .unwrap();
        assert_eq!(context.mode, CleanupMode::TerminateOwned);
        let _lost = mllm_controller::qualification::collect_cleanup(&fake, &context, 1500).unwrap();
    }
    // Crash after cleanup acceptance (or physical termination), before completion.
    let next = f.store.begin_coordinator_session().unwrap();
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM request_leases WHERE disposition='uncertain'"),
        1
    );
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM request_leases WHERE disposition='inflight'"),
        0
    );
    assert_eq!(
        f.store
            .accept_candidate_cleanup(&next, "owner", f.created.run_id(), "cleanup", body, 1600)
            .unwrap(),
        first
    );
    assert!(matches!(
        f.store
            .advance_candidate_security(&next, &collector, f.admission())
            .unwrap(),
        CandidateSecurityDispatch::AlreadyRecorded {
            complete: false,
            ..
        }
    ));
    let successor = f
        .store
        .accept_candidate_cleanup(&next, "owner", f.created.run_id(), "recovery", body, 1600)
        .unwrap();
    assert_eq!(successor.deadline_ms(), 60000);
    assert!(matches!(
        f.store
            .arm_candidate_cleanup(&next, successor.step_id(), 1650)
            .unwrap(),
        ArmResult::New { .. }
    ));
    let context = f
        .store
        .candidate_cleanup_execution(&next, successor.step_id())
        .unwrap();
    assert_eq!(
        context.mode,
        if execute_cleanup {
            CleanupMode::InspectOwnedGone
        } else {
            CleanupMode::TerminateOwned
        }
    );
    let gone = mllm_controller::qualification::collect_cleanup(&fake, &context, 1700).unwrap();
    assert_eq!(
        fake.qualification_activity().unwrap(),
        (sends.0, sends.1 + 1, sends.2)
    );
    f.store
        .complete_cleanup(&next, successor.step_id(), &gone, 1750, f.ttl)
        .unwrap();
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM qualification_runs WHERE cleanup_state='verified_gone'"),
        1
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM request_leases"), 0);
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM operations WHERE state='running'"),
        0
    );
    assert_eq!(
        f.scalar("SELECT requests_used FROM qualification_runs"),
        expected_spend
    );
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM qualification_request_attempts"),
        expected_spend
    );
    assert!(f.store.resource_snapshot().unwrap().owners.is_empty());
    let last = f.store.begin_coordinator_session().unwrap();
    let counts = f.counts();
    let epoch = f.store.resource_snapshot().unwrap().epoch;
    for (key, want) in [("cleanup", &first), ("recovery", &successor)] {
        assert_eq!(
            &f.store
                .accept_candidate_cleanup(&last, "owner", f.created.run_id(), key, body, 999999)
                .unwrap(),
            want
        );
    }
    f.store
        .complete_cleanup(&last, successor.step_id(), &gone, 999999, f.ttl)
        .unwrap();
    assert!(matches!(
        f.store
            .advance_candidate_security(&last, &collector, f.admission())
            .unwrap(),
        CandidateSecurityDispatch::AlreadyRecorded {
            complete: false,
            ..
        }
    ));
    assert_eq!(f.counts(), counts);
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
    assert_eq!(
        fake.qualification_activity().unwrap(),
        (sends.0, sends.1 + 1, sends.2)
    );
    for (id, want) in envelopes {
        let actual: String = f
            .sql
            .query_row(
                "SELECT step_json FROM lifecycle_steps WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(actual, want);
    }
    let actual: String = f
        .sql
        .query_row(
            "SELECT plan_json FROM lifecycle_runs WHERE operation_id=?1",
            [first.operation_id()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(actual, first_plan);
}

#[tokio::test]
async fn security_crash_after_each_arm_and_between_checks_remains_cleanup_recoverable() {
    use mllm_store::candidate_creation::progression::CandidateSecurityDispatch;
    for (stop, expected_spend, uncertain_leases, restart) in [
        (0, 5, 0, true),
        (1, 5, 0, true),
        (2, 6, 1, true),
        (3, 6, 0, true),
        (4, 7, 1, true),
        (0, 5, 0, false),
    ] {
        let f = fixture();
        let fake = FakeEngine::for_qualification();
        let collector = f.baseline(&fake).await;
        for check in 0..=stop / 2 {
            let record = check * 2 < stop;
            match f
                .store
                .advance_candidate_security(&f.session, &collector, f.admission())
                .unwrap()
            {
                CandidateSecurityDispatch::NewControl(d) => {
                    let o = mllm_controller::qualification::collect_security_control(&fake, *d)
                        .await
                        .unwrap();
                    if record {
                        f.store
                            .record_candidate_security_control(&f.session, &collector, &o, 1300)
                            .unwrap();
                    }
                }
                CandidateSecurityDispatch::NewRequest(d) => {
                    let o = mllm_controller::qualification::collect_probe(&fake, *d)
                        .await
                        .unwrap();
                    if record {
                        f.store
                            .record_candidate_result(&f.session, &collector, &o, 1300)
                            .unwrap();
                    }
                }
                _ => panic!("missing original check {check}"),
            }
        }
        let envelopes:Vec<(String,String)>=f.sql.prepare("SELECT id,step_json FROM lifecycle_steps WHERE operation_id IN (SELECT operation_id FROM lifecycle_runs WHERE action='prepare') ORDER BY ordinal").unwrap().query_map([],|r|Ok((r.get(0)?,r.get(1)?))).unwrap().collect::<Result<_,_>>().unwrap();
        let sends = fake.qualification_activity().unwrap();
        let next = if restart {
            f.store.begin_coordinator_session().unwrap()
        } else {
            f.session.clone()
        };
        assert_eq!(
            f.scalar(
                "SELECT COUNT(*) FROM lifecycle_runs WHERE action='prepare' AND state='uncertain'"
            ),
            i64::from(restart)
        );
        assert_eq!(
            f.scalar("SELECT COUNT(*) FROM request_leases WHERE disposition='uncertain'"),
            uncertain_leases
        );
        for (id, raw) in envelopes {
            let current: String = f
                .sql
                .query_row(
                    "SELECT step_json FROM lifecycle_steps WHERE id=?1",
                    [id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(current, raw);
        }
        assert!(
            matches!(
                f.store
                    .advance_candidate_security(&next, &collector, f.admission())
                    .unwrap(),
                CandidateSecurityDispatch::AlreadyRecorded {
                    complete: false,
                    ..
                }
            ),
            "stop {stop}"
        );
        let cleanup = f
            .store
            .accept_candidate_cleanup(
                &next,
                "owner",
                f.created.run_id(),
                "cleanup",
                r#"{"expected_revision":1,"action":"cleanup","deadline_ms":60000}"#,
                1400,
            )
            .unwrap();
        assert!(matches!(
            f.store
                .arm_candidate_cleanup(&next, cleanup.step_id(), 1450)
                .unwrap(),
            ArmResult::New { .. }
        ));
        let context = f
            .store
            .candidate_cleanup_execution(&next, cleanup.step_id())
            .unwrap();
        let gone = mllm_controller::qualification::collect_cleanup(&fake, &context, 1500).unwrap();
        f.store
            .complete_cleanup(&next, cleanup.step_id(), &gone, 1550, f.ttl)
            .unwrap();
        let last = f.store.begin_coordinator_session().unwrap();
        assert!(matches!(
            f.store
                .advance_candidate_security(&last, &collector, f.admission())
                .unwrap(),
            CandidateSecurityDispatch::AlreadyRecorded {
                complete: false,
                ..
            }
        ));
        assert_eq!(
            f.scalar("SELECT requests_used FROM qualification_runs"),
            expected_spend
        );
        assert_eq!(
            f.scalar("SELECT COUNT(*) FROM qualification_request_attempts"),
            expected_spend
        );
        assert_eq!(f.scalar("SELECT COUNT(*) FROM request_leases"), 0);
        assert_eq!(
            f.scalar("SELECT COUNT(*) FROM operations WHERE state='running'"),
            0
        );
        assert!(f.store.resource_snapshot().unwrap().owners.is_empty());
        assert_eq!(
            fake.qualification_activity().unwrap(),
            (sends.0, sends.1 + 1, sends.2)
        );
        assert!(f
            .store
            .finish_candidate_run(
                &last,
                "owner",
                f.created.run_id(),
                "finish",
                r#"{"expected_revision":1,"action":"finish"}"#,
                1600
            )
            .is_err());
    }
}

#[tokio::test]
async fn uncertain_security_and_marker_cleanup_keep_failed_samples_replayable() {
    use mllm_store::candidate_creation::progression::CandidateSecurityDispatch;
    for security in [false, true] {
        let f = fixture();
        let fake = FakeEngine::for_qualification();
        let collector = if security {
            f.baseline(&fake).await
        } else {
            f.ready(&fake).await
        };
        let fake = fake.with_qualification_fault(if security {
            mllm_adapters::fake::QualificationFault::UnauthorizedInferenceExec
        } else {
            mllm_adapters::fake::QualificationFault::LostProbeReply
        });
        let dispatch = if security {
            let CandidateSecurityDispatch::NewControl(control) = f
                .store
                .advance_candidate_security(&f.session, &collector, f.admission())
                .unwrap()
            else {
                panic!()
            };
            let observation =
                mllm_controller::qualification::collect_security_control(&fake, *control)
                    .await
                    .unwrap();
            f.store
                .record_candidate_security_control(&f.session, &collector, &observation, 1300)
                .unwrap();
            let CandidateSecurityDispatch::NewRequest(dispatch) = f
                .store
                .advance_candidate_security(&f.session, &collector, f.admission())
                .unwrap()
            else {
                panic!()
            };
            dispatch
        } else {
            let CandidateDispatchResult::New(dispatch) = f
                .store
                .grant_candidate_inference(
                    &f.session,
                    "owner",
                    f.created.run_id(),
                    "bad-marker",
                    &marker_body(&f, "MLLM_ALPHA_71", false),
                    f.admission(),
                )
                .unwrap()
            else {
                panic!()
            };
            dispatch
        };
        let observation = mllm_controller::qualification::collect_probe(&fake, *dispatch)
            .await
            .unwrap();
        f.store
            .record_candidate_result(&f.session, &collector, &observation, 1300)
            .unwrap();
        let cleanup = f
            .store
            .accept_candidate_cleanup(
                &f.session,
                "owner",
                f.created.run_id(),
                "cleanup",
                r#"{"expected_revision":1,"action":"cleanup","deadline_ms":60000}"#,
                1400,
            )
            .unwrap();
        f.store
            .arm_candidate_cleanup(&f.session, cleanup.step_id(), 1450)
            .unwrap();
        let context = f
            .store
            .candidate_cleanup_execution(&f.session, cleanup.step_id())
            .unwrap();
        let gone = mllm_controller::qualification::collect_cleanup(&fake, &context, 1500).unwrap();
        f.store
            .complete_cleanup(&f.session, cleanup.step_id(), &gone, 1550, f.ttl)
            .unwrap();
        let next = f.store.begin_coordinator_session().unwrap();
        f.store
            .record_candidate_result(&next, &collector, &observation, 999999)
            .unwrap();
        assert_eq!(
            f.scalar("SELECT requests_used FROM qualification_runs"),
            if security { 6 } else { 2 }
        );
        assert_eq!(f.scalar("SELECT COUNT(*) FROM request_leases"), 0);
        assert!(f.store.resource_snapshot().unwrap().owners.is_empty());
        assert!(f
            .store
            .finish_candidate_run(
                &next,
                "owner",
                f.created.run_id(),
                "finish",
                r#"{"expected_revision":1,"action":"finish"}"#,
                1600
            )
            .is_err());
    }
}

// Catches a cleanup reader that only accepts the legacy single-step Initialize.
#[tokio::test]
async fn v3_ready_candidate_accepts_real_bounded_cleanup() {
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    f.ready(&fake).await;
    let cleanup = f
        .store
        .accept_candidate_cleanup(
            &f.session,
            "owner",
            f.created.run_id(),
            "cleanup",
            r#"{"expected_revision":1,"action":"cleanup","deadline_ms":60000}"#,
            1400,
        )
        .unwrap();
    assert_eq!(cleanup.binding_id(), f.created.binding_id());
    assert_eq!(cleanup.generation(), 2);
    assert!(matches!(
        f.store
            .arm_candidate_cleanup(&f.session, cleanup.step_id(), 1450)
            .unwrap(),
        ArmResult::New { .. }
    ));
    assert_eq!(f.scalar("SELECT requests_used FROM qualification_runs"), 1);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM endpoint_leases"), 1);
    assert_eq!(f.store.resource_snapshot().unwrap().owners.len(), 1);
    let context = f
        .store
        .candidate_cleanup_execution(&f.session, cleanup.step_id())
        .unwrap();
    let _lost = mllm_controller::qualification::collect_cleanup(&fake, &context, 1500).unwrap();
    let sends = fake.qualification_activity().unwrap();
    let next = f.store.begin_coordinator_session().unwrap();
    let successor = f
        .store
        .accept_candidate_cleanup(
            &next,
            "owner",
            f.created.run_id(),
            "recovery",
            r#"{"expected_revision":1,"action":"cleanup","deadline_ms":60000}"#,
            1600,
        )
        .unwrap();
    assert!(matches!(
        f.store
            .arm_candidate_cleanup(&next, successor.step_id(), 1650)
            .unwrap(),
        ArmResult::New { .. }
    ));
    let inspect = f
        .store
        .candidate_cleanup_execution(&next, successor.step_id())
        .unwrap();
    assert_eq!(
        inspect.mode,
        mllm_store::candidate_creation::cleanup::CleanupMode::InspectOwnedGone
    );
    let gone = mllm_controller::qualification::collect_cleanup(&fake, &inspect, 1700).unwrap();
    assert_eq!(fake.qualification_activity().unwrap(), sends);
    let counts = f.counts();
    let epoch = f.store.resource_snapshot().unwrap().epoch;
    f.sql.execute_batch("CREATE TRIGGER fail_cleanup_event BEFORE INSERT ON management_events WHEN NEW.kind='candidate_cleanup_completed' BEGIN SELECT RAISE(ABORT,'cleanup rollback'); END").unwrap();
    assert!(f
        .store
        .complete_cleanup(&next, successor.step_id(), &gone, 1750, f.ttl)
        .is_err());
    assert_eq!(f.counts(), counts);
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
    f.sql
        .execute_batch("DROP TRIGGER fail_cleanup_event")
        .unwrap();
    f.store
        .complete_cleanup(&next, successor.step_id(), &gone, 1750, f.ttl)
        .unwrap();
    assert_eq!(f.scalar("SELECT requests_used FROM qualification_runs"), 1);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM endpoint_leases"), 0);
    assert!(f.store.resource_snapshot().unwrap().owners.is_empty());
    let epoch = f.store.resource_snapshot().unwrap().epoch;
    let mut reordered = gone.clone();
    reordered.identities.reverse();
    f.store
        .complete_cleanup(&next, successor.step_id(), &reordered, 999999, f.ttl)
        .unwrap();
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
}

// Catches catalog promotion from partial coverage and mutable/replacing finish writes.
#[tokio::test]
async fn catalog_finish_requires_entire_suite_and_retains_accounting() {
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    let finish = r#"{"expected_revision":1,"action":"finish"}"#;
    assert!(f
        .store
        .finish_candidate_run(
            &f.session,
            "owner",
            f.created.run_id(),
            "finish",
            finish,
            1400
        )
        .is_err());
    let (collector, requests, statuses) = f.completed_suite(&fake).await;
    let before = f.store.resource_snapshot().unwrap();
    assert!(f
        .store
        .finish_candidate_run(
            &f.session,
            "owner",
            f.created.run_id(),
            "premature-time",
            finish,
            1199
        )
        .is_err());
    let before_counts = f.counts();
    f.sql.execute_batch("CREATE TRIGGER reject_finish_event BEFORE INSERT ON management_events WHEN NEW.kind='candidate_qualification_finished' BEGIN SELECT RAISE(ABORT,'finish rollback'); END").unwrap();
    assert!(f
        .store
        .finish_candidate_run(
            &f.session,
            "owner",
            f.created.run_id(),
            "finish",
            finish,
            1400
        )
        .is_err());
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, before.epoch);
    assert_eq!(f.counts(), before_counts);
    f.sql
        .execute_batch("DROP TRIGGER reject_finish_event")
        .unwrap();
    // A deferred status flag without its actual source never qualifies the suite.
    let status_raw: String = f
        .sql
        .query_row(
            "SELECT evidence_json FROM qualification_parked_status",
            [],
            |r| r.get(0),
        )
        .unwrap();
    f.sql
        .execute(
            "UPDATE qualification_parked_status SET evidence_json=?1",
            [status_raw.replacen('{', "{\"version\":3,", 1)],
        )
        .unwrap();
    assert!(f
        .store
        .finish_candidate_run(
            &f.session,
            "owner",
            f.created.run_id(),
            "finish",
            finish,
            1400
        )
        .is_err());
    f.sql
        .execute(
            "UPDATE qualification_parked_status SET evidence_json=?1",
            [status_raw],
        )
        .unwrap();
    let receipt = f
        .store
        .finish_candidate_run(
            &f.session,
            "owner",
            f.created.run_id(),
            "finish",
            finish,
            1400,
        )
        .unwrap();
    assert_eq!(receipt.source_run_id(), f.created.run_id());
    assert_eq!(receipt.recipe_fingerprint(), f.created.recipe_fingerprint());
    assert_eq!(f.scalar("SELECT COUNT(*) FROM qualifications"), 1);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM qualification_runs WHERE state='passed' AND cleanup_state='retained'"), 1);
    assert_eq!(f.scalar("SELECT requests_used FROM qualification_runs"), 12);
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM qualification_evidence_refs"),
        17
    );
    assert_eq!(f.store.resource_snapshot().unwrap().owners, before.owners);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM endpoint_leases"), 1);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM deployments WHERE desired_state='stopped' AND admission_enabled=0 AND dispatch_enabled=0"), 1);
    let epoch = f.store.resource_snapshot().unwrap().epoch;
    assert_eq!(
        f.store
            .finish_candidate_run(
                &f.session,
                "owner",
                f.created.run_id(),
                "finish",
                finish,
                999999
            )
            .unwrap(),
        receipt
    );
    assert_eq!(
        f.store
            .finish_candidate_run(
                &f.session,
                "owner",
                f.created.run_id(),
                "other-finish",
                finish,
                999999
            )
            .unwrap(),
        receipt
    );
    assert_eq!(
        f.store
            .read_qualification(&f.session, receipt.qualification_id())
            .unwrap()
            .unwrap(),
        receipt
    );
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
    assert_eq!(fake.qualification_activity().unwrap(), (12, 7, 10));
    let (fence, binding) = f.fresh_binding(&receipt);
    assert!(f
        .store
        .resolve_ordinary_qualification(&f.session, receipt.qualification_id(), &fence, &binding)
        .is_err());
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
    assert!(matches!(
        f.store
            .arm_candidate_cleanup(&f.session, cleanup.step_id(), 1550)
            .unwrap(),
        ArmResult::New { .. }
    ));
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
    assert_eq!(f.scalar("SELECT requests_used FROM qualification_runs"), 12);
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM qualification_evidence_refs"),
        17
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM qualification_runs WHERE state='passed' AND cleanup_state='verified_gone'"),1);
    assert_eq!(fake.qualification_activity().unwrap(), (12, 8, 10));
    let epoch = f.store.resource_snapshot().unwrap().epoch;
    let session = f.store.begin_coordinator_session().unwrap();
    assert_eq!(
        f.store
            .finish_candidate_run(
                &session,
                "owner",
                f.created.run_id(),
                "finish",
                finish,
                999999
            )
            .unwrap(),
        receipt
    );
    f.store
        .complete_cleanup(&session, cleanup.step_id(), &gone, 999999, f.ttl)
        .unwrap();
    assert_eq!(
        f.store
            .resolve_ordinary_qualification(&session, receipt.qualification_id(), &fence, &binding)
            .unwrap(),
        receipt
    );
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
    for observation in requests {
        f.store
            .record_candidate_result(&session, &collector, &observation, 999999)
            .unwrap();
    }
    for observation in statuses {
        f.store
            .record_candidate_parked_status(&session, &collector, &observation, 999999)
            .unwrap();
    }
    assert!(matches!(
        f.store
            .grant_candidate_inference(
                &session,
                "owner",
                f.created.run_id(),
                "warm-3",
                &marker_body(&f, "MLLM_BETA_29", true),
                f.admission()
            )
            .unwrap(),
        CandidateDispatchResult::AlreadyRecorded { .. }
    ));
    assert!(f
        .store
        .read_qualification(&f.session, receipt.qualification_id())
        .is_err());
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
    // Mutations are faults in the real persisted output, restored from its exact
    // original bytes between checks. They are never qualifying setup fixtures.
    let catalog_raw: String = f
        .sql
        .query_row("SELECT record_json FROM qualifications", [], |r| r.get(0))
        .unwrap();
    for bad in [
        catalog_raw.replacen('{', "{\"version\":3,", 1),
        catalog_raw.replacen('{', "{\"unexpected\":true,", 1),
        " ".repeat(1048577),
    ] {
        f.sql
            .execute("UPDATE qualifications SET record_json=?1", [bad])
            .unwrap();
        assert!(f
            .store
            .read_qualification(&session, receipt.qualification_id())
            .is_err());
    }
    f.sql
        .execute("UPDATE qualifications SET record_json=?1", [catalog_raw])
        .unwrap();
    let binding_raw: String = f
        .sql
        .query_row(
            "SELECT binding_json FROM runtime_bindings WHERE id=?1",
            [&binding],
            |r| r.get(0),
        )
        .unwrap();
    for (path, value) in [
        ("/recipe/resolved_profile/engine", json!("sglang")),
        (
            "/recipe/resolved_profile/build_fingerprint",
            json!("changed-build"),
        ),
        ("/host/hardware_fingerprint", json!("different-hardware")),
        (
            "/host/environment_fingerprint",
            json!("different-environment"),
        ),
        ("/recipe/recipe", json!("changed-recipe")),
    ] {
        let mut outer: Value = serde_json::from_str(&binding_raw).unwrap();
        let mut payload: Value = serde_json::from_str(outer["payload"].as_str().unwrap()).unwrap();
        *payload.pointer_mut(path).unwrap() = value;
        outer["payload"] = json!(payload.to_string());
        f.sql
            .execute(
                "UPDATE runtime_bindings SET binding_json=?1 WHERE id=?2",
                rusqlite::params![outer.to_string(), binding],
            )
            .unwrap();
        assert!(f
            .store
            .resolve_ordinary_qualification(&session, receipt.qualification_id(), &fence, &binding)
            .is_err());
    }
    f.sql
        .execute(
            "UPDATE runtime_bindings SET binding_json=?1 WHERE id=?2",
            rusqlite::params![binding_raw, binding],
        )
        .unwrap();
    let cleanup_raw: String = f
        .sql
        .query_row(
            "SELECT evidence_json FROM lifecycle_evidence WHERE step_id=?1",
            [cleanup.step_id()],
            |r| r.get(0),
        )
        .unwrap();
    f.sql
        .execute(
            "UPDATE lifecycle_evidence SET evidence_json='{}' WHERE step_id=?1",
            [cleanup.step_id()],
        )
        .unwrap();
    assert!(f
        .store
        .read_qualification(&session, receipt.qualification_id())
        .is_err());
    f.sql
        .execute(
            "UPDATE lifecycle_evidence SET evidence_json=?1 WHERE step_id=?2",
            rusqlite::params![cleanup_raw, cleanup.step_id()],
        )
        .unwrap();
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
}

// Catches requiring the expired collector session to finalize fully committed
// evidence, despite a current caller and unchanged deployment fences.
#[tokio::test]
async fn catalog_can_finish_completed_sources_after_restart() {
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    f.completed_suite(&fake).await;
    let reopened = Store::open(&f._dir.path().join("qualification.db")).unwrap();
    let next = reopened.begin_coordinator_session().unwrap();
    let finish = r#"{"expected_revision":1,"action":"finish"}"#;
    let receipt = reopened
        .finish_candidate_run(&next, "owner", f.created.run_id(), "finish", finish, 1400)
        .unwrap();
    assert_eq!(receipt.source_run_id(), f.created.run_id());
    assert_eq!(fake.qualification_activity().unwrap(), (12, 7, 10));
    assert_eq!(f.scalar("SELECT requests_used FROM qualification_runs"), 12);
    assert!(f
        .store
        .finish_candidate_run(
            &f.session,
            "owner",
            f.created.run_id(),
            "finish",
            finish,
            1400
        )
        .is_err());
    f.sql.execute("DELETE FROM qualification_evidence_refs WHERE id=(SELECT id FROM qualification_evidence_refs LIMIT 1)",[]).unwrap();
    assert!(matches!(
        reopened.read_qualification(&next, receipt.qualification_id()),
        Err(mllm_store::lifecycle::LifecycleError::CorruptStoredData)
    ));
}

// A physically executed warm child with a lost reply can only be resolved by
// actual owned disappearance. Its cancelled history must never count as success.
#[tokio::test]
async fn warm_lost_reply_cleanup_preserves_cancelled_history() {
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    f.secured(&fake).await;
    let body = r#"{"expected_revision":1,"action":"park","deadline_ms":400000}"#;
    let park = f
        .store
        .accept_candidate_action(&f.session, "owner", f.created.run_id(), "park", body, 1200)
        .unwrap();
    let child = &park.effect_ids()[0];
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
    let _lost = fake
        .execute_persisted(&RuntimeCommand {
            action: RuntimeAction::Drain,
            context,
        })
        .await
        .unwrap();
    let cleanup = f
        .store
        .accept_candidate_cleanup(
            &f.session,
            "owner",
            f.created.run_id(),
            "cleanup",
            r#"{"expected_revision":1,"action":"cleanup","deadline_ms":60000}"#,
            1400,
        )
        .unwrap();
    assert!(matches!(
        f.store
            .arm_candidate_cleanup(&f.session, cleanup.step_id(), 1450)
            .unwrap(),
        ArmResult::New { .. }
    ));
    let context = f
        .store
        .candidate_cleanup_execution(&f.session, cleanup.step_id())
        .unwrap();
    let gone = mllm_controller::qualification::collect_cleanup(&fake, &context, 1500).unwrap();
    f.store
        .complete_cleanup(&f.session, cleanup.step_id(), &gone, 1550, f.ttl)
        .unwrap();
    assert_eq!(f.scalar("SELECT requests_used FROM qualification_runs"), 7);
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM lifecycle_steps WHERE state='cancelled'"),
        3
    );
    assert!(f.store.resource_snapshot().unwrap().owners.is_empty());
    let next = f.store.begin_coordinator_session().unwrap();
    assert!(matches!(
        f.store
            .arm_candidate_effect(&next, child, f.admission())
            .unwrap(),
        ArmResult::AlreadyRecorded
    ));
    assert!(f
        .store
        .finish_candidate_run(
            &next,
            "owner",
            f.created.run_id(),
            "finish",
            r#"{"expected_revision":1,"action":"finish"}"#,
            1600
        )
        .is_err());
    assert_eq!(f.scalar("SELECT COUNT(*) FROM qualifications"), 0);
    f.store
        .complete_cleanup(&next, cleanup.step_id(), &gone, 999999, f.ttl)
        .unwrap();
}

#[tokio::test]
async fn uncertain_wake_probe_cleanup_settles_work_without_refunding_or_promoting() {
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    let collector = f.secured(&fake).await;
    let fake =
        fake.with_qualification_fault(mllm_adapters::fake::QualificationFault::LostProbeReply);
    let mut wake = None;
    for (action, effects) in [
        ("park", vec![RuntimeAction::Drain, RuntimeAction::Park]),
        (
            "restore",
            vec![
                RuntimeAction::Restore,
                RuntimeAction::ReloadWeights,
                RuntimeAction::InvalidateCache,
            ],
        ),
    ] {
        let body = json!({"expected_revision":1,"action":action,"deadline_ms":400000}).to_string();
        let parent = f
            .store
            .accept_candidate_action(&f.session, "owner", f.created.run_id(), action, &body, 1200)
            .unwrap();
        for (i, action) in effects.into_iter().enumerate() {
            let child = &parent.effect_ids()[i];
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
            let observation = fake
                .execute_persisted(&RuntimeCommand { action, context })
                .await
                .unwrap();
            f.store
                .record_candidate_effect(&f.session, &collector, child, &observation, 1300)
                .unwrap();
        }
        if action == "park" {
            let context = f
                .store
                .candidate_parked_status_execution(&f.session, parent.step_id(), 1200)
                .unwrap();
            let observation =
                mllm_controller::qualification::collect_parked_status(&fake, &context).unwrap();
            f.store
                .record_candidate_parked_status(&f.session, &collector, &observation, 1300)
                .unwrap();
        } else {
            wake = Some(parent);
        }
    }
    let wake = wake.unwrap();
    let CandidateDispatchResult::New(dispatch) = f
        .store
        .arm_candidate_probe(&f.session, wake.step_id(), f.admission())
        .unwrap()
    else {
        panic!()
    };
    let observation = mllm_controller::qualification::collect_probe(&fake, *dispatch)
        .await
        .unwrap();
    f.store
        .record_candidate_result(&f.session, &collector, &observation, 1300)
        .unwrap();
    assert_eq!(f.scalar("SELECT COUNT(*) FROM request_leases"), 1);
    let cleanup = f
        .store
        .accept_candidate_cleanup(
            &f.session,
            "owner",
            f.created.run_id(),
            "cleanup",
            r#"{"expected_revision":1,"action":"cleanup","deadline_ms":60000}"#,
            1400,
        )
        .unwrap();
    f.store
        .arm_candidate_cleanup(&f.session, cleanup.step_id(), 1450)
        .unwrap();
    let context = f
        .store
        .candidate_cleanup_execution(&f.session, cleanup.step_id())
        .unwrap();
    let gone = mllm_controller::qualification::collect_cleanup(&fake, &context, 1500).unwrap();
    f.store
        .complete_cleanup(&f.session, cleanup.step_id(), &gone, 1550, f.ttl)
        .unwrap();
    assert_eq!(f.scalar("SELECT COUNT(*) FROM request_leases"), 0);
    assert_eq!(f.scalar("SELECT requests_used FROM qualification_runs"), 8);
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM operations WHERE kind='candidate_probe_v3' AND state='running'"
        ),
        0
    );
    let epoch = f.store.resource_snapshot().unwrap().epoch;
    let next = f.store.begin_coordinator_session().unwrap();
    f.store
        .record_candidate_result(&next, &collector, &observation, 999999)
        .unwrap();
    assert!(matches!(
        f.store
            .arm_candidate_probe(&next, wake.step_id(), f.admission())
            .unwrap(),
        CandidateDispatchResult::AlreadyRecorded { .. }
    ));
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
    assert!(f
        .store
        .finish_candidate_run(
            &next,
            "owner",
            f.created.run_id(),
            "finish",
            r#"{"expected_revision":1,"action":"finish"}"#,
            1600
        )
        .is_err());
}

// Catches missing warm progression, hidden compound wake effects and duplicate spends.
#[tokio::test]
async fn parking_peak_is_reserved_before_send_and_late_arm_failure_rolls_back() {
    let f = fixture_peaks(12884901888, 15032385536);
    let fake = FakeEngine::for_qualification();
    let collector = f.secured(&fake).await;
    let body = r#"{"expected_revision":1,"action":"park","deadline_ms":400000}"#;
    let receipt = f
        .store
        .accept_candidate_action(&f.session, "owner", f.created.run_id(), "park", body, 1200)
        .unwrap();
    let epoch = f.store.resource_snapshot().unwrap().epoch;
    let mut pressure = f.observations.clone();
    for o in &mut pressure {
        o.available_bytes = 0;
    }
    assert!(f
        .store
        .arm_candidate_effect(
            &f.session,
            &receipt.effect_ids()[0],
            AdmissionContext::new(&pressure, &f.limits, 1200, f.ttl, f.max_parked)
        )
        .is_err());
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
    f.sql.execute_batch("CREATE TRIGGER fail_warm_arm BEFORE UPDATE OF step_json ON lifecycle_steps WHEN OLD.ordinal=1 BEGIN SELECT RAISE(ABORT,'warm rollback'); END").unwrap();
    assert!(f
        .store
        .arm_candidate_effect(&f.session, &receipt.effect_ids()[0], f.admission())
        .is_err());
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM resource_grants"), 2);
    f.sql.execute_batch("DROP TRIGGER fail_warm_arm").unwrap();
    assert!(f
        .store
        .arm_candidate_effect(&f.session, &receipt.effect_ids()[1], f.admission())
        .is_err());
    assert!(matches!(
        f.store
            .arm_candidate_effect(&f.session, &receipt.effect_ids()[0], f.admission())
            .unwrap(),
        ArmResult::New { .. }
    ));
    assert_eq!(
        f.store.resource_snapshot().unwrap().owners[f.created.deployment_id()].allocations[0].bytes,
        12884901888
    );
    assert_eq!(fake.qualification_activity().unwrap(), (7, 2, 5));
    let (_, context) = f
        .store
        .candidate_effect_execution(&f.session, &receipt.effect_ids()[0])
        .unwrap();
    let mut wrong = context.clone();
    if let mllm_domain::completion::ExecutionIdentities::Retained(ref mut ids) = wrong.identities {
        ids[0].pid += 1;
    }
    assert!(fake
        .execute_persisted(&RuntimeCommand {
            action: RuntimeAction::Drain,
            context: wrong
        })
        .await
        .is_err());
    let o = fake
        .execute_persisted(&RuntimeCommand {
            action: RuntimeAction::Drain,
            context,
        })
        .await
        .unwrap();
    f.store
        .record_candidate_effect(&f.session, &collector, &receipt.effect_ids()[0], &o, 1300)
        .unwrap();
    assert!(matches!(
        f.store
            .arm_candidate_effect(&f.session, &receipt.effect_ids()[1], f.admission())
            .unwrap(),
        ArmResult::New { .. }
    ));
    let (_, context) = f
        .store
        .candidate_effect_execution(&f.session, &receipt.effect_ids()[1])
        .unwrap();
    let o = fake
        .execute_persisted(&RuntimeCommand {
            action: RuntimeAction::Park,
            context,
        })
        .await
        .unwrap();
    f.store
        .record_candidate_effect(&f.session, &collector, &receipt.effect_ids()[1], &o, 1300)
        .unwrap();
    let c = f
        .store
        .candidate_parked_status_execution(&f.session, receipt.step_id(), 1200)
        .unwrap();
    let o = mllm_controller::qualification::collect_parked_status(&fake, &c).unwrap();
    f.store
        .record_candidate_parked_status(&f.session, &collector, &o, 1300)
        .unwrap();
    let wake = f
        .store
        .accept_candidate_action(
            &f.session,
            "owner",
            f.created.run_id(),
            "wake",
            r#"{"expected_revision":1,"action":"restore","deadline_ms":400000}"#,
            1200,
        )
        .unwrap();
    let epoch = f.store.resource_snapshot().unwrap().epoch;
    assert!(f
        .store
        .arm_candidate_effect(
            &f.session,
            &wake.effect_ids()[0],
            AdmissionContext::new(&pressure, &f.limits, 1200, f.ttl, f.max_parked)
        )
        .is_err());
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
    for (i, action) in [
        RuntimeAction::Restore,
        RuntimeAction::ReloadWeights,
        RuntimeAction::InvalidateCache,
    ]
    .into_iter()
    .enumerate()
    {
        assert!(matches!(
            f.store
                .arm_candidate_effect(&f.session, &wake.effect_ids()[i], f.admission())
                .unwrap(),
            ArmResult::New { .. }
        ));
        assert_eq!(
            f.store.resource_snapshot().unwrap().owners[f.created.deployment_id()].allocations[0]
                .bytes,
            15032385536
        );
        let (_, context) = f
            .store
            .candidate_effect_execution(&f.session, &wake.effect_ids()[i])
            .unwrap();
        let o = fake
            .execute_persisted(&RuntimeCommand { action, context })
            .await
            .unwrap();
        f.store
            .record_candidate_effect(&f.session, &collector, &wake.effect_ids()[i], &o, 1300)
            .unwrap();
        assert_eq!(
            f.scalar("SELECT COUNT(*) FROM deployments WHERE observed_state='ready'"),
            0
        );
    }
    assert!(f
        .store
        .arm_candidate_probe(
            &f.session,
            wake.step_id(),
            AdmissionContext::new(&pressure, &f.limits, 1200, f.ttl, f.max_parked)
        )
        .is_err());
    assert_eq!(f.scalar("SELECT requests_used FROM qualification_runs"), 7);
    let CandidateDispatchResult::New(d) = f
        .store
        .arm_candidate_probe(&f.session, wake.step_id(), f.admission())
        .unwrap()
    else {
        panic!()
    };
    let o = mllm_controller::qualification::collect_probe(&fake, *d)
        .await
        .unwrap();
    f.store
        .record_candidate_result(&f.session, &collector, &o, 1300)
        .unwrap();
    let next = f.store.begin_coordinator_session().unwrap();
    assert!(f
        .store
        .arm_candidate_effect(&f.session, &receipt.effect_ids()[0], f.admission())
        .is_err());
    assert!(matches!(
        f.store
            .arm_candidate_effect(&next, &receipt.effect_ids()[0], f.admission())
            .unwrap(),
        ArmResult::AlreadyRecorded
    ));
    assert!(matches!(
        f.store
            .arm_candidate_effect(&next, &receipt.effect_ids()[1], f.admission())
            .unwrap(),
        ArmResult::AlreadyRecorded
    ));
    assert_eq!(
        f.store.resource_snapshot().unwrap().owners[f.created.deployment_id()].allocations[0].bytes,
        15032385536
    );
}

#[tokio::test]
async fn warm_cycle_preserves_membership_and_accounts_all_twelve_requests() {
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    let collector = f.secured(&fake).await;
    let before = f.store.resource_snapshot().unwrap().owners;
    let mut parked_observations = Vec::new();
    for (action, effects) in [
        ("park", vec![RuntimeAction::Drain, RuntimeAction::Park]),
        (
            "restore",
            vec![
                RuntimeAction::Restore,
                RuntimeAction::ReloadWeights,
                RuntimeAction::InvalidateCache,
            ],
        ),
    ] {
        let body = json!({"expected_revision":1,"action":action,"deadline_ms":400000}).to_string();
        let receipt = f
            .store
            .accept_candidate_action(&f.session, "owner", f.created.run_id(), action, &body, 1200)
            .unwrap();
        assert_eq!(
            receipt.effect_ids().len(),
            if action == "park" { 2 } else { 4 }
        );
        for (i, effect) in effects.into_iter().enumerate() {
            let child = &receipt.effect_ids()[i];
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
            let o = fake
                .execute_persisted(&RuntimeCommand {
                    action: effect,
                    context,
                })
                .await
                .unwrap();
            f.store
                .record_candidate_effect(&f.session, &collector, child, &o, 1300)
                .unwrap();
            assert!(matches!(
                f.store
                    .arm_candidate_effect(&f.session, child, f.admission())
                    .unwrap(),
                ArmResult::AlreadyRecorded
            ));
        }
        if action == "park" {
            let activity = fake.qualification_activity().unwrap();
            let context = f
                .store
                .candidate_parked_status_execution(&f.session, receipt.step_id(), 1200)
                .unwrap();
            let status =
                mllm_controller::qualification::collect_parked_status(&fake, &context).unwrap();
            assert_eq!(fake.qualification_activity().unwrap(), activity);
            assert!(f
                .store
                .accept_candidate_action(
                    &f.session,
                    "owner",
                    f.created.run_id(),
                    "early-restore",
                    r#"{"expected_revision":1,"action":"restore","deadline_ms":400000}"#,
                    1200
                )
                .is_err());
            let mut bad = status.clone();
            bad.allocations = true;
            assert!(f
                .store
                .record_candidate_parked_status(&f.session, &collector, &bad, 1300)
                .is_err());
            let mut bad = status.clone();
            bad.identities.push(bad.identities[0].clone());
            assert!(f
                .store
                .record_candidate_parked_status(&f.session, &collector, &bad, 1300)
                .is_err());
            let mut bad = status.clone();
            bad.activity_after.1 += 1;
            assert!(f
                .store
                .record_candidate_parked_status(&f.session, &collector, &bad, 1300)
                .is_err());
            let epoch = f.store.resource_snapshot().unwrap().epoch;
            f.sql.execute_batch("CREATE TRIGGER fail_park_complete BEFORE DELETE ON lifecycle_claims BEGIN SELECT RAISE(ABORT,'park rollback'); END").unwrap();
            assert!(f
                .store
                .record_candidate_parked_status(&f.session, &collector, &status, 1300)
                .is_err());
            assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
            assert_eq!(
                f.scalar("SELECT COUNT(*) FROM qualification_parked_status"),
                0
            );
            assert_eq!(f.scalar("SELECT COUNT(*) FROM lifecycle_claims"), 1);
            f.sql
                .execute_batch("DROP TRIGGER fail_park_complete")
                .unwrap();
            f.store
                .record_candidate_parked_status(&f.session, &collector, &status, 1300)
                .unwrap();
            parked_observations.push(status.clone());
            let epoch = f.store.resource_snapshot().unwrap().epoch;
            f.store
                .record_candidate_parked_status(&f.session, &collector, &status, 999999)
                .unwrap();
            assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
        } else {
            assert_eq!(
                f.scalar("SELECT COUNT(*) FROM deployments WHERE observed_state='ready'"),
                0
            );
            let CandidateDispatchResult::New(d) = f
                .store
                .arm_candidate_probe(&f.session, receipt.step_id(), f.admission())
                .unwrap()
            else {
                panic!()
            };
            let o = mllm_controller::qualification::collect_probe(&fake, *d)
                .await
                .unwrap();
            f.store
                .record_candidate_result(&f.session, &collector, &o, 1300)
                .unwrap();
        }
    }
    for (i, (marker, stream)) in [
        ("MLLM_ALPHA_71", false),
        ("MLLM_BETA_29", false),
        ("MLLM_ALPHA_71", true),
        ("MLLM_BETA_29", true),
    ]
    .into_iter()
    .enumerate()
    {
        let CandidateDispatchResult::New(d) = f
            .store
            .grant_candidate_inference(
                &f.session,
                "owner",
                f.created.run_id(),
                &format!("warm-{i}"),
                &marker_body(&f, marker, stream),
                f.admission(),
            )
            .unwrap()
        else {
            panic!()
        };
        let o = mllm_controller::qualification::collect_probe(&fake, *d)
            .await
            .unwrap();
        f.store
            .record_candidate_result(&f.session, &collector, &o, 1300)
            .unwrap();
    }
    assert_eq!(f.scalar("SELECT requests_used FROM qualification_runs"), 12);
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM qualification_request_attempts"),
        12
    );
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM qualification_evidence_refs"),
        17
    );
    assert_eq!(
        f.scalar("SELECT COUNT(DISTINCT case_id) FROM qualification_evidence_refs"),
        10
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM resource_grants"), 4);
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM lifecycle_steps WHERE ordinal>0 AND grant_id IS NOT NULL"),
        0
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM runtime_bindings"), 1);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM endpoint_leases"), 1);
    let incarnation: String = f
        .sql
        .query_row("SELECT incarnation FROM runtime_bindings", [], |r| r.get(0))
        .unwrap();
    assert_eq!(incarnation, f.created.incarnation());
    assert_eq!(f.scalar("SELECT COUNT(*) FROM request_leases"), 0);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM lifecycle_claims"), 0);
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM owned_launch_associations"),
        1
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM qualification_evidence_refs WHERE metadata_json LIKE '%parked_status_observation%'"),1);
    assert_eq!(fake.qualification_activity().unwrap(), (12, 7, 10));
    assert_eq!(f.store.resource_snapshot().unwrap().owners, before);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM deployments WHERE desired_state='stopped' AND observed_state='ready' AND admission_enabled=0 AND dispatch_enabled=0"),1);
    let epoch = f.store.resource_snapshot().unwrap().epoch;
    let session = f.store.begin_coordinator_session().unwrap();
    for o in parked_observations {
        f.store
            .record_candidate_parked_status(&session, &collector, &o, 999999)
            .unwrap();
    }
    for action in ["park", "restore"] {
        let body = json!({"expected_revision":1,"action":action,"deadline_ms":400000}).to_string();
        f.store
            .accept_candidate_action(&session, "owner", f.created.run_id(), action, &body, 999999)
            .unwrap();
    }
    assert!(matches!(
        f.store
            .grant_candidate_inference(
                &session,
                "owner",
                f.created.run_id(),
                "warm-3",
                &marker_body(&f, "MLLM_BETA_29", true),
                f.admission()
            )
            .unwrap(),
        CandidateDispatchResult::AlreadyRecorded { .. }
    ));
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
    assert_eq!(fake.qualification_activity().unwrap(), (12, 7, 10));
    f.sql.execute("UPDATE lifecycle_steps SET ordinal=99 WHERE id=(SELECT s.id FROM lifecycle_steps s JOIN lifecycle_runs r ON r.operation_id=s.operation_id WHERE r.action='activate' AND s.ordinal=3 LIMIT 1)",[]).unwrap();
    assert!(f
        .store
        .accept_candidate_action(
            &session,
            "owner",
            f.created.run_id(),
            "restore",
            r#"{"expected_revision":1,"action":"restore","deadline_ms":400000}"#,
            999999
        )
        .is_err());
}

#[tokio::test]
async fn every_warm_child_lost_reply_is_never_retried_after_restart() {
    for lost in [
        RuntimeAction::Drain,
        RuntimeAction::Park,
        RuntimeAction::Restore,
        RuntimeAction::ReloadWeights,
        RuntimeAction::InvalidateCache,
    ] {
        let f = fixture();
        let fake = FakeEngine::for_qualification();
        let collector = f.secured(&fake).await;
        'actions: for (action, effects) in [
            ("park", vec![RuntimeAction::Drain, RuntimeAction::Park]),
            (
                "restore",
                vec![
                    RuntimeAction::Restore,
                    RuntimeAction::ReloadWeights,
                    RuntimeAction::InvalidateCache,
                ],
            ),
        ] {
            let body =
                json!({"expected_revision":1,"action":action,"deadline_ms":400000}).to_string();
            let p = f
                .store
                .accept_candidate_action(
                    &f.session,
                    "owner",
                    f.created.run_id(),
                    action,
                    &body,
                    1200,
                )
                .unwrap();
            for (i, effect) in effects.into_iter().enumerate() {
                let child = &p.effect_ids()[i];
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
                let o = fake
                    .execute_persisted(&RuntimeCommand {
                        action: effect,
                        context,
                    })
                    .await
                    .unwrap();
                if effect == lost {
                    // The actual engine completed its effect; transport drops the reply.
                    drop(o);
                    let activity = fake.qualification_activity().unwrap();
                    let retained = f.store.resource_snapshot().unwrap();
                    assert!(matches!(
                        f.store
                            .arm_candidate_effect(&f.session, child, f.admission())
                            .unwrap(),
                        ArmResult::AlreadyRecorded
                    ));
                    assert!(f
                        .store
                        .arm_candidate_probe(&f.session, p.step_id(), f.admission())
                        .is_err());
                    if let Some(next) = p.effect_ids().get(i + 1) {
                        assert!(f
                            .store
                            .arm_candidate_effect(&f.session, next, f.admission())
                            .is_err());
                    }
                    let session = f.store.begin_coordinator_session().unwrap();
                    assert!(matches!(
                        f.store
                            .arm_candidate_effect(&session, child, f.admission())
                            .unwrap(),
                        ArmResult::AlreadyRecorded
                    ));
                    assert!(f.store.candidate_effect_execution(&session, child).is_err());
                    assert_eq!(f.store.resource_snapshot().unwrap(), retained);
                    assert_eq!(fake.qualification_activity().unwrap(), activity);
                    assert_eq!(f.scalar("SELECT requests_used FROM qualification_runs"), 7);
                    assert_eq!(f.scalar("SELECT COUNT(*) FROM lifecycle_claims"), 1);
                    assert_eq!(
                        f.scalar("SELECT COUNT(*) FROM runtime_bindings WHERE state='uncertain'"),
                        1
                    );
                    break 'actions;
                }
                let mut wrong = o.clone();
                wrong.token.step_id = p.step_id().into();
                assert!(f
                    .store
                    .record_candidate_effect(&f.session, &collector, child, &wrong, 1300)
                    .is_err());
                f.store
                    .record_candidate_effect(&f.session, &collector, child, &o, 1300)
                    .unwrap();
            }
            let c = f
                .store
                .candidate_parked_status_execution(&f.session, p.step_id(), 1200)
                .unwrap();
            let o = mllm_controller::qualification::collect_parked_status(&fake, &c).unwrap();
            f.store
                .record_candidate_parked_status(&f.session, &collector, &o, 1300)
                .unwrap();
        }
    }
}

#[tokio::test]
async fn unknown_request_lease_blocks_park_and_stays_reserved() {
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    f.secured(&fake).await;
    let p = f
        .store
        .accept_candidate_action(
            &f.session,
            "owner",
            f.created.run_id(),
            "park",
            r#"{"expected_revision":1,"action":"park","deadline_ms":400000}"#,
            1200,
        )
        .unwrap();
    f.sql
        .execute(
            "INSERT INTO request_leases VALUES(?1,?2,1,1,?3,'uncertain')",
            rusqlite::params![
                ulid::Ulid::new().to_string(),
                f.created.deployment_id(),
                f.session.id()
            ],
        )
        .unwrap();
    let retained = f.store.resource_snapshot().unwrap();
    assert!(f
        .store
        .arm_candidate_effect(&f.session, &p.effect_ids()[0], f.admission())
        .is_err());
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM request_leases WHERE disposition='uncertain'"),
        1
    );
    assert_eq!(f.store.resource_snapshot().unwrap(), retained);
    assert_eq!(fake.qualification_activity().unwrap(), (7, 2, 5));
}

#[tokio::test]
async fn marker_templates_reject_overrides_and_duplicate_fields_before_spend() {
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    f.ready(&fake).await;
    let body = marker_body(&f, "MLLM_ALPHA_71", false);
    let mut invalid = vec![
        marker_body(&f, "MLLM_BETA_29", false),
        marker_body(&f, "MLLM_ALPHA_71", true),
        body.replacen("\"model\":", "\"stream\":false,\"model\":", 1),
    ];
    for (field, value) in [
        ("temperature", json!(1)),
        ("max_tokens", json!(17)),
        ("model", json!("other")),
        ("extra", json!(true)),
    ] {
        let mut v: Value = serde_json::from_str(&body).unwrap();
        v[field] = value;
        invalid.push(v.to_string());
    }
    for text in invalid {
        assert!(f
            .store
            .grant_candidate_inference(
                &f.session,
                "owner",
                f.created.run_id(),
                "invalid",
                &text,
                f.admission()
            )
            .is_err());
    }
    assert_eq!(f.scalar("SELECT requests_used FROM qualification_runs"), 1);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM request_leases"), 0);
}

#[tokio::test]
async fn marker_transactions_roll_back_spend_and_late_coverage_failure() {
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    let collector = f.ready(&fake).await;
    let body = marker_body(&f, "MLLM_ALPHA_71", false);
    f.sql.execute_batch("CREATE TRIGGER reject_marker_spend BEFORE UPDATE OF requests_used ON qualification_runs BEGIN SELECT RAISE(ABORT,'test spending rollback'); END").unwrap();
    assert!(f
        .store
        .grant_candidate_inference(
            &f.session,
            "owner",
            f.created.run_id(),
            "marker",
            &body,
            f.admission()
        )
        .is_err());
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM qualification_request_attempts"),
        1
    );
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM operations WHERE kind='candidate_marker_v3'"),
        0
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM request_leases"), 0);
    f.sql
        .execute_batch("DROP TRIGGER reject_marker_spend")
        .unwrap();
    let CandidateDispatchResult::New(dispatch) = f
        .store
        .grant_candidate_inference(
            &f.session,
            "owner",
            f.created.run_id(),
            "marker",
            &body,
            f.admission(),
        )
        .unwrap()
    else {
        panic!()
    };
    let result = mllm_controller::qualification::collect_probe(&fake, *dispatch)
        .await
        .unwrap();
    let epoch = f.store.resource_snapshot().unwrap().epoch;
    f.sql.execute_batch("CREATE TRIGGER reject_marker_coverage BEFORE INSERT ON qualification_evidence_refs BEGIN SELECT RAISE(ABORT,'test result rollback'); END").unwrap();
    assert!(f
        .store
        .record_candidate_result(&f.session, &collector, &result, 1300)
        .is_err());
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM qualification_request_results"),
        0
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM request_leases"), 1);
    assert_eq!(f.scalar("SELECT requests_used FROM qualification_runs"), 2);
    f.sql
        .execute_batch("DROP TRIGGER reject_marker_coverage")
        .unwrap();
    f.store
        .record_candidate_result(&f.session, &collector, &result, 1300)
        .unwrap();
}

#[tokio::test]
async fn failed_marker_never_replaced_and_stream_corruption_never_advances() {
    use mllm_adapters::fake::QualificationFault;
    for (fault, stream, uncertain) in [
        (QualificationFault::WrongProbeOutput, false, false),
        (QualificationFault::LostProbeReply, false, true),
        (QualificationFault::MissingProbeFinish, true, false),
        (QualificationFault::StreamMissingDone, true, true),
        (QualificationFault::StreamAfterFinish, true, false),
        (QualificationFault::CrossMarkerOutput, false, false),
        (QualificationFault::StreamDuplicateField, true, true),
        (QualificationFault::StreamOverflow, true, true),
    ] {
        let f = fixture();
        let fake = FakeEngine::for_qualification();
        let collector = f.ready(&fake).await;
        if stream {
            for (i, marker) in ["MLLM_ALPHA_71", "MLLM_BETA_29"].into_iter().enumerate() {
                let CandidateDispatchResult::New(d) = f
                    .store
                    .grant_candidate_inference(
                        &f.session,
                        "owner",
                        f.created.run_id(),
                        &format!("setup-{i}"),
                        &marker_body(&f, marker, false),
                        f.admission(),
                    )
                    .unwrap()
                else {
                    panic!()
                };
                let o = mllm_controller::qualification::collect_probe(&fake, *d)
                    .await
                    .unwrap();
                f.store
                    .record_candidate_result(&f.session, &collector, &o, 1300)
                    .unwrap();
            }
        }
        let fake = fake.with_qualification_fault(fault);
        let body = marker_body(&f, "MLLM_ALPHA_71", stream);
        let CandidateDispatchResult::New(d) = f
            .store
            .grant_candidate_inference(
                &f.session,
                "owner",
                f.created.run_id(),
                "fault",
                &body,
                f.admission(),
            )
            .unwrap()
        else {
            panic!()
        };
        let o = mllm_controller::qualification::collect_probe(&fake, *d)
            .await
            .unwrap();
        f.store
            .record_candidate_result(&f.session, &collector, &o, 1300)
            .unwrap();
        assert_eq!(
            f.scalar("SELECT COUNT(*) FROM qualification_evidence_refs"),
            if stream { 4 } else { 2 },
            "{fault:?}"
        );
        assert_eq!(
            f.scalar("SELECT COUNT(*) FROM request_leases"),
            i64::from(uncertain)
        );
        assert!(f
            .store
            .grant_candidate_inference(
                &f.session,
                "owner",
                f.created.run_id(),
                "replacement",
                &body,
                f.admission()
            )
            .is_err());
        let epoch = f.store.resource_snapshot().unwrap().epoch;
        let next = f.store.begin_coordinator_session().unwrap();
        assert!(matches!(
            f.store
                .grant_candidate_inference(
                    &next,
                    "owner",
                    f.created.run_id(),
                    "fault",
                    &body,
                    f.admission()
                )
                .unwrap(),
            CandidateDispatchResult::AlreadyRecorded { .. }
        ));
        f.store
            .record_candidate_result(&next, &collector, &o, 999999)
            .unwrap();
        assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
        assert_eq!(
            f.scalar("SELECT requests_used FROM qualification_runs"),
            if stream { 4 } else { 2 }
        );
    }
}

// Catches missing coordinator selection, duplicate spending, and marker coverage
// inferred from a disappeared lease rather than actual collected output.
#[tokio::test]
async fn baseline_markers_use_frozen_order_and_receipt_first_replay() {
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    let collector = f.ready(&fake).await;
    for (index, (marker, stream)) in [
        ("MLLM_ALPHA_71", false),
        ("MLLM_BETA_29", false),
        ("MLLM_ALPHA_71", true),
        ("MLLM_BETA_29", true),
    ]
    .into_iter()
    .enumerate()
    {
        let body = marker_body(&f, marker, stream);
        let key = format!("marker-{index}");
        let CandidateDispatchResult::New(dispatch) = f
            .store
            .grant_candidate_inference(
                &f.session,
                "owner",
                f.created.run_id(),
                &key,
                &body,
                f.admission(),
            )
            .unwrap()
        else {
            panic!("first attempt must have a ticket")
        };
        let operation = dispatch.request_operation_id().to_owned();
        assert_eq!(
            serde_json::from_str::<Value>(dispatch.request()).unwrap(),
            serde_json::from_str::<Value>(&body).unwrap()
        );
        let result = mllm_controller::qualification::collect_probe(&fake, *dispatch)
            .await
            .unwrap();
        f.store
            .record_candidate_result(&f.session, &collector, &result, 1300)
            .unwrap();
        assert_eq!(
            f.scalar("SELECT COUNT(*) FROM qualification_evidence_refs"),
            3 + index as i64
        );
        let epoch = f.store.resource_snapshot().unwrap().epoch;
        assert!(
            matches!(f.store.grant_candidate_inference(&f.session, "owner", f.created.run_id(), &key, &body, f.admission()).unwrap(), CandidateDispatchResult::AlreadyRecorded { request_operation_id } if request_operation_id == operation)
        );
        f.store
            .record_candidate_result(&f.session, &collector, &result, 999999)
            .unwrap();
        assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
    }
    assert_eq!(f.scalar("SELECT requests_used FROM qualification_runs"), 5);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM request_leases"), 0);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM lifecycle_claims"), 0);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM resource_owners"), 1);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM deployments WHERE desired_state='stopped' AND admission_enabled=0 AND dispatch_enabled=0"), 1);
}

#[tokio::test]
async fn security_requires_all_fixed_rejections_and_spends_only_two_requests() {
    use mllm_store::candidate_creation::progression::CandidateSecurityDispatch;
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    let collector = f.baseline(&fake).await;
    assert!(matches!(
        f.store.accept_candidate_action(
            &f.session,
            "owner",
            f.created.run_id(),
            "wire-security",
            r#"{"expected_revision":1,"action":"security","deadline_ms":400000}"#,
            1200
        ),
        Err(mllm_store::lifecycle::LifecycleError::Invalid)
    ));
    let CandidateSecurityDispatch::NewControl(control) = f
        .store
        .advance_candidate_security(&f.session, &collector, f.admission())
        .unwrap()
    else {
        panic!()
    };
    let operation = control.context().token.operation_id.clone();
    assert_eq!(f.scalar("SELECT COUNT(*) FROM lifecycle_claims"), 1);
    assert_eq!(f.scalar("SELECT requests_used FROM qualification_runs"), 5);
    let observation = mllm_controller::qualification::collect_security_control(&fake, *control)
        .await
        .unwrap();
    f.store
        .record_candidate_security_control(&f.session, &collector, &observation, 1300)
        .unwrap();
    for endpoint in [
        mllm_domain::qualification::CandidateSecurityEndpoint::Inference,
        mllm_domain::qualification::CandidateSecurityEndpoint::HealthGeneration,
    ] {
        let CandidateSecurityDispatch::NewRequest(dispatch) = f
            .store
            .advance_candidate_security(&f.session, &collector, f.admission())
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(dispatch.security_endpoint(), Some(endpoint));
        assert_eq!(dispatch.context().token.operation_id, operation);
        assert_ne!(dispatch.request_operation_id(), operation);
        assert!(matches!(
            f.store
                .advance_candidate_security(&f.session, &collector, f.admission())
                .unwrap(),
            CandidateSecurityDispatch::AlreadyRecorded {
                complete: false,
                ..
            }
        ));
        let result = mllm_controller::qualification::collect_probe(&fake, *dispatch)
            .await
            .unwrap();
        f.store
            .record_candidate_result(&f.session, &collector, &result, 1300)
            .unwrap();
        let epoch = f.store.resource_snapshot().unwrap().epoch;
        f.store
            .record_candidate_result(&f.session, &collector, &result, 999999)
            .unwrap();
        assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
    }
    assert!(matches!(
        f.store
            .advance_candidate_security(&f.session, &collector, f.admission())
            .unwrap(),
        CandidateSecurityDispatch::AlreadyRecorded { complete: true, .. }
    ));
    assert_eq!(f.scalar("SELECT requests_used FROM qualification_runs"), 7);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM request_leases"), 0);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM lifecycle_claims"), 0);
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM lifecycle_runs WHERE action='prepare' AND state='succeeded'"
        ),
        1
    );
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM qualification_evidence_refs"),
        9
    );
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM lifecycle_steps WHERE grant_id IS NOT NULL"),
        2
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM deployments WHERE desired_state='stopped' AND observed_state='ready' AND admission_enabled=0 AND dispatch_enabled=0"),1);
    let epoch = f.store.resource_snapshot().unwrap().epoch;
    let newer = f.store.begin_coordinator_session().unwrap();
    assert!(f
        .store
        .advance_candidate_security(&f.session, &collector, f.admission())
        .is_err());
    assert!(matches!(
        f.store
            .advance_candidate_security(
                &newer,
                &collector,
                AdmissionContext::new(&f.observations, &f.limits, 999999, f.ttl, f.max_parked)
            )
            .unwrap(),
        CandidateSecurityDispatch::AlreadyRecorded { complete: true, .. }
    ));
    let mut canonical = observation.clone();
    canonical.effect.identities.reverse();
    f.store
        .record_candidate_security_control(&newer, &collector, &canonical, 999999)
        .unwrap();
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
    assert_eq!(fake.qualification_activity().unwrap(), (7, 2, 5));
}

#[tokio::test]
async fn security_history_rejects_a_missing_inflight_lease() {
    use mllm_store::candidate_creation::progression::CandidateSecurityDispatch;
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
    let o = mllm_controller::qualification::collect_security_control(&fake, *d)
        .await
        .unwrap();
    f.store
        .record_candidate_security_control(&f.session, &collector, &o, 1300)
        .unwrap();
    let CandidateSecurityDispatch::NewRequest(_) = f
        .store
        .advance_candidate_security(&f.session, &collector, f.admission())
        .unwrap()
    else {
        panic!()
    };
    f.sql.execute("DELETE FROM request_leases", []).unwrap();
    assert!(f
        .store
        .advance_candidate_security(&f.session, &collector, f.admission())
        .is_err());
}

#[tokio::test]
async fn security_new_result_cannot_advance_a_terminal_run() {
    use mllm_store::candidate_creation::progression::CandidateSecurityDispatch;
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
    let o = mllm_controller::qualification::collect_security_control(&fake, *d)
        .await
        .unwrap();
    f.sql
        .execute("UPDATE qualification_runs SET state='aborted'", [])
        .unwrap();
    assert!(f
        .store
        .record_candidate_security_control(&f.session, &collector, &o, 1300)
        .is_err());
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM qualification_evidence_refs"),
        6
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM lifecycle_claims"), 1);
}

#[tokio::test]
async fn security_unexpected_work_and_lost_replies_retain_uncertainty_and_never_retry() {
    use mllm_adapters::fake::QualificationFault;
    use mllm_store::candidate_creation::progression::CandidateSecurityDispatch;
    for (fault, index) in [
        (QualificationFault::UnauthorizedAdminExec, 0),
        (QualificationFault::UnauthorizedInferenceExec, 1),
        (QualificationFault::UnauthorizedHealthExec, 2),
        (QualificationFault::LostSecurityInferenceReply, 1),
    ] {
        let f = fixture();
        let fake = FakeEngine::for_qualification();
        let collector = f.baseline(&fake).await;
        let fake = fake.with_qualification_fault(fault);
        for current in 0..=index {
            match f
                .store
                .advance_candidate_security(&f.session, &collector, f.admission())
                .unwrap()
            {
                CandidateSecurityDispatch::NewControl(d) => {
                    let o = mllm_controller::qualification::collect_security_control(&fake, *d)
                        .await
                        .unwrap();
                    f.store
                        .record_candidate_security_control(&f.session, &collector, &o, 1300)
                        .unwrap();
                }
                CandidateSecurityDispatch::NewRequest(d) => {
                    let o = mllm_controller::qualification::collect_probe(&fake, *d)
                        .await
                        .unwrap();
                    f.store
                        .record_candidate_result(&f.session, &collector, &o, 1300)
                        .unwrap();
                }
                _ => panic!("missing new effect {current}"),
            }
        }
        assert_eq!(
            f.scalar("SELECT COUNT(*) FROM qualification_evidence_refs"),
            6 + index,
            "{fault:?}"
        );
        assert_eq!(
            f.scalar(
                "SELECT COUNT(*) FROM lifecycle_runs WHERE action='prepare' AND state='uncertain'"
            ),
            1
        );
        assert_eq!(f.scalar("SELECT COUNT(*) FROM lifecycle_claims"), 1);
        assert_eq!(
            f.scalar("SELECT COUNT(*) FROM qualification_runs WHERE state='uncertain'"),
            1
        );
        assert_eq!(
            f.scalar("SELECT COUNT(*) FROM request_leases WHERE disposition='uncertain'"),
            i64::from(index > 0)
        );
        assert_eq!(f.scalar("SELECT COUNT(*) FROM resource_owners"), 1);
        let used = f.scalar("SELECT requests_used FROM qualification_runs");
        let next = f.store.begin_coordinator_session().unwrap();
        assert!(matches!(
            f.store
                .advance_candidate_security(&next, &collector, f.admission())
                .unwrap(),
            CandidateSecurityDispatch::AlreadyRecorded {
                complete: false,
                ..
            }
        ));
        assert_eq!(
            f.scalar("SELECT requests_used FROM qualification_runs"),
            used
        );
    }
}

#[tokio::test]
async fn security_acceptance_spending_and_completion_are_atomic() {
    use mllm_store::candidate_creation::progression::CandidateSecurityDispatch;
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    let collector = f.baseline(&fake).await;
    let epoch = f.store.resource_snapshot().unwrap().epoch;
    f.sql.execute_batch("CREATE TRIGGER reject_security_accept BEFORE INSERT ON command_receipts WHEN NEW.command_scope LIKE 'INTERNAL qualification Security%' BEGIN SELECT RAISE(ABORT,'security acceptance rollback'); END").unwrap();
    assert!(f
        .store
        .advance_candidate_security(&f.session, &collector, f.admission())
        .is_err());
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM resource_grants"), 1);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM lifecycle_claims"), 0);
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM lifecycle_runs WHERE action='prepare'"),
        0
    );
    f.sql
        .execute_batch("DROP TRIGGER reject_security_accept")
        .unwrap();
    let CandidateSecurityDispatch::NewControl(d) = f
        .store
        .advance_candidate_security(&f.session, &collector, f.admission())
        .unwrap()
    else {
        panic!()
    };
    let observation = mllm_controller::qualification::collect_security_control(&fake, *d)
        .await
        .unwrap();
    f.store
        .record_candidate_security_control(&f.session, &collector, &observation, 1300)
        .unwrap();
    f.sql.execute_batch("CREATE TRIGGER reject_security_spend BEFORE UPDATE OF requests_used ON qualification_runs BEGIN SELECT RAISE(ABORT,'security spending rollback'); END").unwrap();
    assert!(f
        .store
        .advance_candidate_security(&f.session, &collector, f.admission())
        .is_err());
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM operations WHERE kind='candidate_security_v3'"),
        0
    );
    assert_eq!(f.scalar("SELECT requests_used FROM qualification_runs"), 5);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM request_leases"), 0);
    f.sql
        .execute_batch("DROP TRIGGER reject_security_spend")
        .unwrap();
    for index in 0..2 {
        let CandidateSecurityDispatch::NewRequest(d) = f
            .store
            .advance_candidate_security(&f.session, &collector, f.admission())
            .unwrap()
        else {
            panic!()
        };
        let o = mllm_controller::qualification::collect_probe(&fake, *d)
            .await
            .unwrap();
        if index == 1 {
            let epoch = f.store.resource_snapshot().unwrap().epoch;
            f.sql.execute_batch("CREATE TRIGGER reject_security_complete BEFORE DELETE ON lifecycle_claims BEGIN SELECT RAISE(ABORT,'security completion rollback'); END").unwrap();
            assert!(f
                .store
                .record_candidate_result(&f.session, &collector, &o, 1300)
                .is_err());
            assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
            assert_eq!(f.scalar("SELECT COUNT(*) FROM request_leases"), 1);
            assert_eq!(
                f.scalar("SELECT COUNT(*) FROM qualification_evidence_refs"),
                8
            );
            assert_eq!(f.scalar("SELECT COUNT(*) FROM lifecycle_claims"), 1);
            f.sql
                .execute_batch("DROP TRIGGER reject_security_complete")
                .unwrap();
        }
        f.store
            .record_candidate_result(&f.session, &collector, &o, 1300)
            .unwrap();
    }
}

#[tokio::test]
async fn replay_rejects_corrupt_prior_spending_even_when_replaying_another_item() {
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    f.baseline(&fake).await;
    // Corrupt an earlier item's duplicated columns, leaving the replayed last item intact.
    f.sql.execute("UPDATE qualification_request_attempts SET item_ordinal=17 WHERE request_operation_id=(SELECT id FROM operations WHERE kind='candidate_marker_v3' ORDER BY rowid LIMIT 1)",[]).unwrap();
    assert!(f
        .store
        .grant_candidate_inference(
            &f.session,
            "owner",
            f.created.run_id(),
            "baseline-3",
            &marker_body(&f, "MLLM_BETA_29", true),
            f.admission()
        )
        .is_err());
}

#[tokio::test]
async fn probe_rechecks_physical_pressure_before_spending() {
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    f.initialized(&fake).await;
    let mut pressure = f.observations.clone();
    for o in &mut pressure {
        o.available_bytes = 0;
    }
    let admission = AdmissionContext::new(&pressure, &f.limits, 1200, f.ttl, f.max_parked);
    assert!(f
        .store
        .arm_candidate_probe(&f.session, f.init.step_id(), admission)
        .is_err());
    assert_eq!(f.scalar("SELECT requests_used FROM qualification_runs"), 0);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM request_leases"), 0);
    assert!(matches!(
        f.store
            .arm_candidate_probe(&f.session, f.init.step_id(), f.admission())
            .unwrap(),
        CandidateDispatchResult::New(_)
    ));
}

#[tokio::test]
async fn unknown_extra_work_blocks_joint_ready_completion() {
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    let collector = f.initialized(&fake).await;
    let CandidateDispatchResult::New(probe) = f
        .store
        .arm_candidate_probe(&f.session, f.init.step_id(), f.admission())
        .unwrap()
    else {
        panic!()
    };
    let result = mllm_controller::qualification::collect_probe(&fake, *probe)
        .await
        .unwrap();
    f.sql.execute_batch("INSERT INTO request_leases SELECT id || '-unknown',deployment_id,revision,generation,session_id,'uncertain' FROM request_leases").unwrap();
    assert!(f
        .store
        .record_candidate_result(&f.session, &collector, &result, 1300)
        .is_err());
    assert_eq!(f.scalar("SELECT COUNT(*) FROM request_leases"), 2);
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM qualification_evidence_refs"),
        0
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM lifecycle_claims"), 1);
}

#[tokio::test]
async fn collector_rejects_a_fake_runtime_without_the_retained_owned_membership() {
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    f.initialized(&fake).await;
    let CandidateDispatchResult::New(probe) = f
        .store
        .arm_candidate_probe(&f.session, f.init.step_id(), f.admission())
        .unwrap()
    else {
        panic!()
    };
    assert!(mllm_controller::qualification::collect_probe(
        &FakeEngine::for_qualification(),
        *probe
    )
    .await
    .is_err());
    assert_eq!(f.scalar("SELECT COUNT(*) FROM request_leases"), 1);
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM qualification_evidence_refs"),
        0
    );
}

#[tokio::test]
async fn child_crash_and_wrong_result_tokens_cannot_complete_or_resend() {
    let f = fixture();
    f.store
        .arm_candidate_effect(&f.session, &f.init.effect_ids()[0], f.admission())
        .unwrap();
    let next = f.store.begin_coordinator_session().unwrap();
    assert!(f
        .store
        .candidate_effect_execution(&next, &f.init.effect_ids()[0])
        .is_err());
    assert!(!matches!(
        f.store
            .arm_candidate_effect(&next, &f.init.effect_ids()[0], f.admission()),
        Ok(ArmResult::New { .. })
    ));
    assert!(f
        .store
        .arm_candidate_probe(&next, f.init.step_id(), f.admission())
        .is_err());
    assert_eq!(f.scalar("SELECT COUNT(*) FROM lifecycle_claims"), 1);
    assert_eq!(f.scalar("SELECT requests_used FROM qualification_runs"), 0);
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    let collector = f.initialized(&fake).await;
    let CandidateDispatchResult::New(probe) = f
        .store
        .arm_candidate_probe(&f.session, f.init.step_id(), f.admission())
        .unwrap()
    else {
        panic!()
    };
    let result = mllm_controller::qualification::collect_probe(&fake, *probe)
        .await
        .unwrap();
    for mutation in 0..5 {
        let mut wrong = result.clone();
        match mutation {
            0 => wrong.token.step_id = f.init.step_id().into(),
            1 => wrong.token.operation_id = result.request_operation_id.clone(),
            2 => wrong.identities.push(wrong.identities[0].clone()),
            3 => wrong.lease_id = f.init.operation_id().into(),
            _ => {
                wrong.response =
                    mllm_domain::qualification::CandidateResponseObservation::Streaming {
                        chunks: vec![],
                        completed: false,
                    }
            }
        }
        assert!(f
            .store
            .record_candidate_result(&f.session, &collector, &wrong, 1300)
            .is_err());
    }
    assert_eq!(f.scalar("SELECT COUNT(*) FROM request_leases"), 1);
    f.store
        .record_candidate_result(&f.session, &collector, &result, 1300)
        .unwrap();
    let mut reordered = result.clone();
    reordered.identities.reverse();
    f.store
        .record_candidate_result(&f.session, &collector, &reordered, 999999)
        .unwrap();
    reordered.receipt.push_str(" altered");
    assert!(f
        .store
        .record_candidate_result(&f.session, &collector, &reordered, 999999)
        .is_err());
}

#[tokio::test]
async fn failed_or_uncertain_probe_retains_original_sample_and_never_refunds_or_resends() {
    use mllm_adapters::fake::QualificationFault;
    for (fault, leases) in [
        (QualificationFault::WrongProbeOutput, 0),
        (QualificationFault::LostProbeReply, 1),
        (QualificationFault::FailedProbe, 0),
        (QualificationFault::MissingProbeFinish, 0),
    ] {
        let f = fixture();
        let fake = FakeEngine::for_qualification().with_qualification_fault(fault);
        let collector = f.initialized(&fake).await;
        let CandidateDispatchResult::New(probe) = f
            .store
            .arm_candidate_probe(&f.session, f.init.step_id(), f.admission())
            .unwrap()
        else {
            panic!()
        };
        let request = probe.request_operation_id().to_string();
        let result = mllm_controller::qualification::collect_probe(&fake, *probe)
            .await
            .unwrap();
        f.store
            .record_candidate_result(&f.session, &collector, &result, 1300)
            .unwrap();
        assert_eq!(
            f.scalar("SELECT COUNT(*) FROM qualification_evidence_refs"),
            0,
            "{fault:?}"
        );
        assert_eq!(
            f.scalar("SELECT COUNT(*) FROM request_leases"),
            leases,
            "{fault:?}"
        );
        assert_eq!(f.scalar("SELECT COUNT(*) FROM lifecycle_claims"), 1);
        assert_eq!(
            f.scalar("SELECT COUNT(*) FROM lifecycle_steps WHERE state='uncertain'"),
            2
        );
        assert_eq!(f.scalar("SELECT requests_used FROM qualification_runs"), 1);
        let epoch = f.store.resource_snapshot().unwrap().epoch;
        let next = f.store.begin_coordinator_session().unwrap();
        f.store
            .record_candidate_result(&next, &collector, &result, 999999)
            .unwrap();
        assert!(
            matches!(f.store.arm_candidate_probe(&next,f.init.step_id(),f.admission()).unwrap(),CandidateDispatchResult::AlreadyRecorded{request_operation_id} if request_operation_id==request)
        );
        assert_eq!(f.store.resource_snapshot().unwrap().epoch, epoch);
        assert_eq!(f.scalar("SELECT requests_used FROM qualification_runs"), 1);
    }
}

#[tokio::test]
async fn probe_arm_and_joint_completion_roll_back_all_related_writes() {
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    let collector = f.initialized(&fake).await;
    let before = f.store.resource_snapshot().unwrap();
    f.sql.execute_batch("CREATE TRIGGER reject_spend BEFORE UPDATE OF requests_used ON qualification_runs BEGIN SELECT RAISE(ABORT,'injected spend failure'); END;").unwrap();
    assert!(f
        .store
        .arm_candidate_probe(&f.session, f.init.step_id(), f.admission())
        .is_err());
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM qualification_request_attempts"),
        0
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM request_leases"), 0);
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM operations WHERE kind='candidate_probe_v3'"),
        0
    );
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM lifecycle_steps WHERE ordinal=2 AND state='planned'"),
        1
    );
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, before.epoch);
    f.sql.execute_batch("DROP TRIGGER reject_spend").unwrap();
    let CandidateDispatchResult::New(probe) = f
        .store
        .arm_candidate_probe(&f.session, f.init.step_id(), f.admission())
        .unwrap()
    else {
        panic!()
    };
    let result = mllm_controller::qualification::collect_probe(&fake, *probe)
        .await
        .unwrap();
    let events = f.scalar("SELECT COUNT(*) FROM management_events");
    f.sql.execute_batch("CREATE TRIGGER reject_coverage BEFORE INSERT ON qualification_evidence_refs BEGIN SELECT RAISE(ABORT,'injected coverage failure'); END;").unwrap();
    assert!(f
        .store
        .record_candidate_result(&f.session, &collector, &result, 1300)
        .is_err());
    assert_eq!(f.scalar("SELECT COUNT(*) FROM lifecycle_evidence"), 1);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM request_leases"), 1);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM lifecycle_claims"), 1);
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM lifecycle_steps WHERE state='armed'"),
        2
    );
    assert_eq!(
        f.scalar(
            "SELECT COUNT(*) FROM operations WHERE kind='candidate_probe_v3' AND state='running'"
        ),
        1
    );
    assert_eq!(f.scalar("SELECT requests_used FROM qualification_runs"), 1);
    assert_eq!(f.store.resource_snapshot().unwrap().epoch, before.epoch);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM management_events"), events);
    f.sql.execute_batch("DROP TRIGGER reject_coverage").unwrap();
    f.store
        .record_candidate_result(&f.session, &collector, &result, 1300)
        .unwrap();
    f.sql
        .execute_batch("UPDATE operations SET state='running' WHERE kind='candidate_probe_v3'")
        .unwrap();
    assert!(
        f.store
            .record_candidate_result(&f.session, &collector, &result, 999999)
            .is_err(),
        "request operation must agree with successful evidence"
    );
    f.sql
        .execute_batch("UPDATE operations SET state='succeeded' WHERE kind='candidate_probe_v3'")
        .unwrap();
    assert_eq!(f.scalar("SELECT COUNT(DISTINCT committed_epoch) FROM lifecycle_evidence WHERE step_id IN (SELECT id FROM lifecycle_steps WHERE ordinal IN (0,2))"),1);
}

#[tokio::test]
async fn missing_probe_mapping_cannot_hide_a_spent_attempt_or_issue_another_ticket() {
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    f.initialized(&fake).await;
    let CandidateDispatchResult::New(_) = f
        .store
        .arm_candidate_probe(&f.session, f.init.step_id(), f.admission())
        .unwrap()
    else {
        panic!()
    };
    f.sql
        .execute_batch("UPDATE qualification_request_attempts SET item_ordinal=1")
        .unwrap();
    assert!(f
        .store
        .arm_candidate_probe(&f.session, f.init.step_id(), f.admission())
        .is_err());
    assert_eq!(f.scalar("SELECT requests_used FROM qualification_runs"), 1);
}

#[tokio::test]
async fn completed_probe_replay_requires_original_coverage_and_operation_state() {
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    let collector = f.initialized(&fake).await;
    let CandidateDispatchResult::New(probe) = f
        .store
        .arm_candidate_probe(&f.session, f.init.step_id(), f.admission())
        .unwrap()
    else {
        panic!()
    };
    let result = mllm_controller::qualification::collect_probe(&fake, *probe)
        .await
        .unwrap();
    f.store
        .record_candidate_result(&f.session, &collector, &result, 1300)
        .unwrap();
    f.sql
        .execute_batch("DELETE FROM qualification_evidence_refs")
        .unwrap();
    assert!(
        f.store
            .record_candidate_result(&f.session, &collector, &result, 999999)
            .is_err(),
        "missing coverage must invalidate historical success"
    );
}


#[tokio::test]
async fn initialized_child_associates_on_anchor_without_ready_or_probe_bypass() {
    let f = fixture();
    let (store, session, created, init) = (&f.store, f.session.clone(), &f.created, &f.init);
    let admission = || f.admission();
    assert!(store
        .arm_candidate_effect(&session, init.step_id(), admission())
        .is_err());
    assert!(matches!(
        store
            .arm_candidate_effect(&session, &init.effect_ids()[0], admission())
            .unwrap(),
        ArmResult::New { .. }
    ));
    assert!(store
        .candidate_effect_execution(&session, init.step_id())
        .is_err());
    let (_, context) = store
        .candidate_effect_execution(&session, &init.effect_ids()[0])
        .unwrap();
    let fake = FakeEngine::for_qualification();
    let observed = fake
        .execute_persisted(&RuntimeCommand {
            action: RuntimeAction::Initialize,
            context: context.clone(),
        })
        .await
        .unwrap();
    let launch = OwnedLaunchReceipt {
        binding_id: observed.binding_id.clone(),
        incarnation: observed.incarnation.clone(),
        identities: observed.identities.clone(),
        observed_at_ms: observed.observed_at_ms,
        receipt: observed.receipt.clone(),
    };
    assert!(store
        .record_owned_launch(&session, &init.effect_ids()[0], &launch, 1250)
        .is_err());
    store
        .record_owned_launch(&session, init.step_id(), &launch, 1250)
        .unwrap();
    let collector = store
        .candidate_collector(&session, "owner", created.run_id())
        .unwrap();
    store
        .record_candidate_effect(&session, &collector, &init.effect_ids()[0], &observed, 1250)
        .unwrap();
    let mut token = context.token.clone();
    token.step_id = init.step_id().into();
    let fabricated = CompletionEvidence {
        token,
        identities: observed.identities.clone(),
        observed_at_ms: 1300,
        control_receipt: Some("caller supplied ready".into()),
        milestones: vec![
            Milestone::AllocationsRestored,
            Milestone::WeightsUsable,
            Milestone::CacheValid,
            Milestone::ModelUsable,
        ],
    };
    assert!(store
        .complete_step(&session, init.step_id(), &fabricated, 1300, f.ttl)
        .is_err());
    assert_eq!(
        store.resource_snapshot().unwrap().owners[created.deployment_id()].phase,
        ResourcePhase::Cold
    );
    assert!(matches!(
        store
            .arm_candidate_effect(&session, &init.effect_ids()[0], admission())
            .unwrap(),
        ArmResult::AlreadyRecorded
    ));
    store
        .record_owned_launch(&session, init.step_id(), &launch, 1250)
        .unwrap();
    store
        .record_candidate_effect(
            &session,
            &collector,
            &init.effect_ids()[0],
            &observed,
            999999,
        )
        .unwrap();
    let probe = match store
        .arm_candidate_probe(&session, init.step_id(), admission())
        .unwrap()
    {
        CandidateDispatchResult::New(probe) => probe,
        _ => panic!("first probe must issue a single ticket"),
    };
    assert_ne!(probe.request_operation_id(), init.operation_id());
    assert_eq!(probe.context().token.operation_id, init.operation_id());
    assert_eq!(probe.context().token.step_id, init.effect_ids()[1]);
    assert_eq!(
        store
            .candidate_run_snapshot("owner", created.run_id())
            .unwrap()
            .unwrap()
            .requests_used(),
        1
    );
    assert!(
        matches!(store.arm_candidate_probe(&session, init.step_id(), admission()).unwrap(), CandidateDispatchResult::AlreadyRecorded { request_operation_id } if request_operation_id == probe.request_operation_id())
    );
    let result = mllm_controller::qualification::collect_probe(&fake, *probe)
        .await
        .unwrap();
    store
        .record_candidate_result(&session, &collector, &result, 1300)
        .unwrap();
    let epoch = store.resource_snapshot().unwrap().epoch;
    store
        .record_candidate_result(&session, &collector, &result, 999999)
        .unwrap();
    assert_eq!(store.resource_snapshot().unwrap().epoch, epoch);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM request_leases"), 0);
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM lifecycle_steps WHERE state='completed'"),
        3
    );
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM qualification_evidence_refs"),
        2
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM lifecycle_claims"), 0);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM resource_owners"), 1);
    assert_eq!(f.scalar("SELECT COUNT(*) FROM deployments WHERE desired_state='stopped' AND observed_state='ready' AND admission_enabled=0 AND dispatch_enabled=0"),1);
    let newer = store.begin_coordinator_session().unwrap();
    assert!(store
        .record_candidate_result(&session, &collector, &result, 999999)
        .is_err());
    store
        .record_candidate_result(&newer, &collector, &result, 999999)
        .unwrap();
    assert!(matches!(
        store
            .arm_candidate_probe(&newer, init.step_id(), admission())
            .unwrap(),
        CandidateDispatchResult::AlreadyRecorded { .. }
    ));
}

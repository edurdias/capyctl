//! ADR 0032 §7: restart when the deployment's limit is full of cancelled
//! requests held until the next stop, against the durable lifecycle. CPU
//! only; nothing here qualifies an engine.
use super::*;
use crate::dispatch::{DispatchRequest, DispatchTicket, LeaseWrite};
use crate::ordinary_lifecycle::held_restart::{HeldInstanceRestart, HeldRestartDecision};
use capyctl_domain::launch::MAX_REQUESTS_PER_DEPLOYMENT;

const LIMIT: usize = MAX_REQUESTS_PER_DEPLOYMENT as usize;

/// A Ready built-in instance of a fresh deployment.
fn ready_lab() -> (Lab, DeploymentFence) {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    lab.ready(&a, 1_000, 10);
    (lab, a)
}

fn grant(lab: &Lab, fence: &DeploymentFence) -> DispatchTicket {
    lab.store
        .grant_dispatch(
            &lab.session,
            DispatchRequest {
                deployment_id: &fence.deployment_id,
                revision: fence.revision,
                generation: lab.instance(&fence.deployment_id).2,
                max_per_deployment: LIMIT,
                max_total: 4 * LIMIT,
            },
        )
        .unwrap()
}

fn write(lab: &Lab, write: LeaseWrite) {
    let outcome = lab
        .store
        .apply_request_lease_batch(&lab.session, &[write])
        .unwrap();
    assert!(outcome.iter().all(Result::is_ok), "{outcome:?}");
}

/// `count` requests cancelled before the engine answered them: each settles
/// only at the instance's verified exit.
fn hold(lab: &Lab, fence: &DeploymentFence, count: usize) {
    for _ in 0..count {
        write(
            lab,
            LeaseWrite::Cancel {
                ticket: grant(lab, fence),
                answered: false,
            },
        );
    }
}

fn restart(lab: &Lab, fence: &DeploymentFence, now: i64) -> HeldRestartDecision {
    lab.store
        .restart_for_held_requests(&lab.session, &fence.deployment_id, LIMIT, now)
        .unwrap()
}

fn events(lab: &Lab, kind: &str) -> Vec<Value> {
    lab.store
        .events_after(None, 1_000)
        .unwrap()
        .events
        .into_iter()
        .filter(|e| e.kind == kind)
        .map(|e| serde_json::from_str(&e.payload_json).unwrap())
        .collect()
}

fn leases(lab: &Lab, fence: &DeploymentFence) -> i64 {
    lab.scalar(
        "SELECT COUNT(*) FROM request_leases WHERE deployment_id=?1",
        &fence.deployment_id,
    )
}

fn stops(lab: &Lab, fence: &DeploymentFence) -> i64 {
    lab.scalar(
        "SELECT COUNT(*) FROM lifecycle_runs WHERE deployment_id=?1 AND action='stop'",
        &fence.deployment_id,
    )
}

/// Nothing was stopped or journaled for the deployment.
fn assert_untouched(lab: &Lab, fence: &DeploymentFence) {
    assert_eq!(stops(lab, fence), 0);
    assert!(events(lab, "instance_restarted_for_held_requests").is_empty());
    let pending: Option<i64> = lab.scalar(
        "SELECT pending_start_until_ms FROM deployment_instances WHERE deployment_id=?1",
        &fence.deployment_id,
    );
    assert_eq!(pending, None);
}

// T17 (ADR 0032 §7): one request below the limit restarts nothing; the
// trigger is the count reaching the limit.
#[test]
fn held_requests_below_the_limit_restart_nothing() {
    let (lab, a) = ready_lab();
    hold(&lab, &a, LIMIT - 1);
    assert_eq!(restart(&lab, &a, 1_100), HeldRestartDecision::NotHeld);
    assert_untouched(&lab, &a);
}

// T17 (ADR 0032 §7): held requests alone filling the limit restart the
// instance holding them: its verified stop is accepted and its start made
// pending, journaled once with the deployment, instance and count. A second
// caller finds it stopping and is answered Restarting without a second stop.
#[test]
fn held_requests_filling_the_limit_restart_the_instance_once() {
    let (lab, a) = ready_lab();
    hold(&lab, &a, LIMIT);
    let generation = lab.instance(&a.deployment_id).2;
    let HeldRestartDecision::Restarting { accepted } = restart(&lab, &a, 1_100) else {
        panic!("expected a restart");
    };
    assert!(
        matches!(
            accepted.as_slice(),
            [HeldInstanceRestart { instance_index: 0, generation: g, count, .. }]
                if *g == generation && *count == LIMIT
        ),
        "{accepted:?}"
    );
    assert_eq!(stops(&lab, &a), 1);
    let restarted = events(&lab, "instance_restarted_for_held_requests");
    assert_eq!(restarted.len(), 1);
    assert_eq!(
        restarted[0],
        json!({"version": "1", "deployment_id": a.deployment_id,
            "operation_id": accepted[0].operation_id, "instance_index": 0,
            "generation": generation, "count": LIMIT})
    );
    let pending: Option<i64> = lab.scalar(
        "SELECT pending_start_until_ms FROM deployment_instances WHERE deployment_id=?1",
        &a.deployment_id,
    );
    assert!(pending.is_some_and(|until| until > 1_100));
    assert_eq!(
        restart(&lab, &a, 1_150),
        HeldRestartDecision::Restarting { accepted: vec![] }
    );
    assert_eq!(stops(&lab, &a), 1);
    assert_eq!(
        events(&lab, "instance_restarted_for_held_requests").len(),
        1
    );
    // Still charged until the stop's verified exit.
    assert_eq!(leases(&lab, &a), LIMIT as i64);
}

// T17 T38 (ADR 0032 §7): an uncertain lease is not a request held until the
// next stop, so one keeps the restart from being triggered.
#[test]
fn an_uncertain_lease_keeps_the_restart_from_being_triggered() {
    let (lab, a) = ready_lab();
    hold(&lab, &a, LIMIT - 1);
    write(&lab, LeaseWrite::Uncertain(grant(&lab, &a)));
    assert_eq!(leases(&lab, &a), LIMIT as i64);
    assert_eq!(restart(&lab, &a, 1_100), HeldRestartDecision::NotHeld);
    assert_untouched(&lab, &a);
}

// T17 (ADR 0032 §7): a live request, or a cancellation the engine answered
// (its idle reading settles it), keeps the restart from being triggered: a
// restart never cuts live work.
#[test]
fn a_live_or_answered_lease_keeps_the_restart_from_being_triggered() {
    let (lab, a) = ready_lab();
    hold(&lab, &a, LIMIT - 1);
    let live = grant(&lab, &a);
    assert_eq!(restart(&lab, &a, 1_100), HeldRestartDecision::NotHeld);
    write(
        &lab,
        LeaseWrite::Cancel {
            ticket: live,
            answered: true,
        },
    );
    assert_eq!(restart(&lab, &a, 1_100), HeldRestartDecision::NotHeld);
    assert_untouched(&lab, &a);
}

// T17 (ADR 0032 §7): an instance already stopping (an operator's stop, say)
// is not asked again: the answer is Restarting, with no second stop.
#[test]
fn an_instance_already_stopping_is_not_stopped_again() {
    let (lab, a) = ready_lab();
    hold(&lab, &a, LIMIT);
    lab.store
        .accept_instance_stop_command(
            &lab.session,
            "operator",
            &a.deployment_id,
            0,
            a.revision,
            "stop",
            1_050,
            60_000,
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        restart(&lab, &a, 1_100),
        HeldRestartDecision::Restarting { accepted: vec![] }
    );
    assert_eq!(stops(&lab, &a), 1);
    assert!(events(&lab, "instance_restarted_for_held_requests").is_empty());
}

// T17 (ADR 0032 §7, SPEC §10): the restart's verified exit settles every held
// request with the stop's own receipt, and the pending start is placed again;
// an unverified exit keeps them charged.
#[test]
fn the_restart_s_verified_exit_settles_the_held_requests_and_starts_again() {
    let (lab, a) = ready_lab();
    hold(&lab, &a, LIMIT);
    let HeldRestartDecision::Restarting { accepted } = restart(&lab, &a, 1_100) else {
        panic!("expected a restart");
    };
    let step: String = lab.scalar(
        "SELECT id FROM lifecycle_steps WHERE operation_id=?1",
        &accepted[0].operation_id,
    );
    let (_, context) = lab
        .store
        .arm_ordinary_cleanup_with_context(&lab.session, &step, 1_200)
        .unwrap();
    let context = context.unwrap();
    let evidence = |identities| capyctl_domain::completion::CleanupEvidence {
        binding_id: context.binding_id.clone(),
        incarnation: context.incarnation.clone(),
        identities,
        observed_at_ms: 1_300,
        receipt: "gone".into(),
    };
    assert!(lab
        .store
        .complete_cleanup(&lab.session, &step, &evidence(group(99)), 1_300, 2_000)
        .is_err());
    assert_eq!(leases(&lab, &a), LIMIT as i64, "unverified: still charged");
    lab.store
        .complete_cleanup(
            &lab.session,
            &step,
            &evidence(context.identities.clone()),
            1_300,
            2_000,
        )
        .unwrap();
    assert_eq!(leases(&lab, &a), 0);
    let acknowledged = events(&lab, "request_cancellation_acknowledged");
    assert_eq!(acknowledged.len(), 1, "{acknowledged:?}");
    assert_eq!(acknowledged[0]["count"], json!(LIMIT));
    let receipt = acknowledged[0]["receipt"].as_str().unwrap();
    assert!(
        receipt.starts_with("cancelled before the engine answered; stop operation")
            && receipt.contains(&accepted[0].operation_id),
        "{receipt}"
    );
    let started = lab
        .store
        .reconcile_instances(&lab.session, 1_400, None, true)
        .unwrap();
    assert!(
        started.iter().any(|done| matches!(
            done,
            crate::ordinary_lifecycle::reconcile::Reconciled::Started { instance: 0, .. }
        )),
        "{started:?}"
    );
}

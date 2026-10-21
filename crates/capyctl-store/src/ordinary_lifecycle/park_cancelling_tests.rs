//! ADR 0031 §6, SPEC §10: cancelling requests on a custom engine, with and
//! without an idle signal, and (amended 2026-10-21) on a built-in engine,
//! answered by the engine before their cancellation or not, against the
//! durable lifecycle. CPU only.
use super::*;
use crate::dispatch::{DispatchRequest, DispatchTicket, LeaseWrite, LeaseWriteOutcome};
use crate::ordinary_lifecycle::retired_leases::QuiescenceEvidence;
use crate::request_lease_cancellations::CancellingBinding;
use capyctl_config::custom::{CustomManifest, CustomProfile};
use capyctl_config::effective::InstallationDrift;
use capyctl_config::registration::{custom_profile_document, CustomProfileSpec};

/// A chat custom engine's manifest, with an idle signal when `idle`.
fn manifest(idle: bool) -> CustomManifest {
    let mut value = json!({
        "kind": "custom_engine",
        "schema_version": 1,
        "api": "chat",
        "version": "2.2.0",
        "args": ["--host", "{host}", "--port", "{port}", "--model", "{model_dir}"],
        "env": {"BASAL_API_KEY": "{api_key}"},
    });
    if idle {
        value["idle"] =
            json!({"url": "/metrics", "gauges": ["requests_running", "requests_waiting"]});
    }
    CustomManifest::from_value(value).unwrap()
}

/// The F2 fixture on a custom engine: its `local` profile is the one
/// `engine add --custom` writes, and the deployment states its resources.
fn custom_fixture(idle: bool) -> (Value, Value) {
    let (mut config, mut host) = fixture();
    host["runtime_profiles"]["local"] = custom_profile_document(&CustomProfileSpec {
        executable: "/opt/basal/bin/basal-serve".into(),
        custom: CustomProfile::new(manifest(idle)),
        installation_drift: InstallationDrift::Warn,
        env: Default::default(),
        approved_env: Vec::new(),
    });
    config["runtime_profile_revision"] = 1.into();
    config["api"] = "chat".into();
    config["engine_config"] = json!({});
    config.as_object_mut().unwrap().remove("residency");
    (config, host)
}

fn custom_lab(idle: bool) -> Lab {
    let (config, host) = custom_fixture(idle);
    let effective = resolve_effective(&config, &host).unwrap();
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    store
        .import_resource_policy(&session, &effective.host, &observed(1), 1)
        .unwrap();
    Lab {
        store,
        session,
        host,
    }
}

impl Lab {
    fn deploy_custom(&self, name: &str, idle: bool) -> DeploymentFence {
        let (custom, _) = custom_fixture(idle);
        self.deploy(name, |config| {
            for key in ["runtime_profile_revision", "api", "engine_config"] {
                config[key] = custom[key].clone();
            }
            config.as_object_mut().unwrap().remove("residency");
        })
    }

    fn grant(
        &self,
        fence: &DeploymentFence,
        max: usize,
    ) -> Result<DispatchTicket, crate::dispatch::DispatchError> {
        let generation = self.instance(&fence.deployment_id).2;
        self.store.grant_dispatch(
            &self.session,
            DispatchRequest {
                deployment_id: &fence.deployment_id,
                revision: fence.revision,
                generation,
                max_per_deployment: max,
                max_total: 16,
            },
        )
    }

    /// Cancel `ticket`; `answered` when the engine answered it first.
    fn cancel(&self, ticket: &DispatchTicket, answered: bool) {
        let outcome = self
            .store
            .apply_request_lease_batch(
                &self.session,
                &[LeaseWrite::Cancel {
                    ticket: ticket.clone(),
                    answered,
                }],
            )
            .unwrap();
        assert!(matches!(
            outcome[..],
            [Ok(LeaseWriteOutcome::Settled(true))]
        ));
    }

    fn binding(&self, fence: &DeploymentFence) -> String {
        self.scalar(
            "SELECT id FROM runtime_bindings WHERE deployment_id=?1 AND state!='released'",
            &fence.deployment_id,
        )
    }

    fn leases(&self, fence: &DeploymentFence) -> i64 {
        self.scalar(
            "SELECT COUNT(*) FROM request_leases WHERE deployment_id=?1",
            &fence.deployment_id,
        )
    }

    fn acknowledgements(&self) -> Vec<crate::events::ManagementEvent> {
        self.store
            .events_after(None, 1_000)
            .unwrap()
            .events
            .into_iter()
            .filter(|e| e.kind == "request_cancellation_acknowledged")
            .collect()
    }

    fn requests(&self, fence: &DeploymentFence) -> crate::snapshot::InstanceRequests {
        let snapshot = self.store.snapshot().unwrap();
        let d = snapshot
            .deployments
            .iter()
            .find(|d| d.id == fence.deployment_id)
            .unwrap();
        d.instances[0].requests.clone()
    }
}

/// A Ready custom instance with one cancelling lease the engine answered.
fn cancelling(idle: bool) -> (Lab, DeploymentFence) {
    let lab = custom_lab(idle);
    let a = lab.deploy_custom("a", idle);
    lab.ready(&a, 1_000, 10);
    let ticket = lab.grant(&a, 4).unwrap();
    lab.cancel(&ticket, true);
    (lab, a)
}

// T44 T17 (ADR 0031 §6): without an idle signal a cancelling lease is never
// settled while the engine runs, and drains do not wait for it, but it stays
// charged: admission counts it and status shows when it settles.
#[test]
fn a_cancelling_lease_without_an_idle_signal_stays_charged_and_drains_skip_it() {
    let (lab, a) = cancelling(false);
    let binding = lab.binding(&a);
    // Never asked, and nothing a caller observed settles it.
    assert!(lab
        .store
        .cancelling_bindings(&lab.session)
        .unwrap()
        .is_empty());
    assert_eq!(
        lab.store
            .settle_cancelled_leases(&lab.session, &binding, i64::MAX, "generic read")
            .unwrap(),
        0
    );
    assert_eq!(lab.leases(&a), 1);
    assert!(lab.acknowledgements().is_empty());
    // Admission still counts it.
    assert!(lab.grant(&a, 1).is_err());
    // Stop and switch drains do not wait for it.
    let generation = lab.instance(&a.deployment_id).2;
    assert_eq!(
        lab.store.binding_outstanding_leases(&binding).unwrap(),
        Some(0)
    );
    assert_eq!(
        lab.store
            .switch_outstanding_leases(&a.deployment_id, 0, generation)
            .unwrap(),
        0
    );
    // A request still being answered is waited for, as for every engine.
    let answering = lab.grant(&a, 4).unwrap();
    assert_eq!(
        lab.store.binding_outstanding_leases(&binding).unwrap(),
        Some(1)
    );
    assert_eq!(
        lab.store
            .switch_outstanding_leases(&a.deployment_id, 0, generation)
            .unwrap(),
        1
    );
    lab.cancel(&answering, false);
    assert_eq!(
        lab.store.binding_outstanding_leases(&binding).unwrap(),
        Some(0)
    );
    let requests = lab.requests(&a);
    assert_eq!(requests.cancelling, "2");
    assert_eq!(
        requests.cancelling_settles,
        crate::snapshot::CancellingSettles::Stop
    );
    let json = serde_json::to_value(&requests).unwrap();
    assert_eq!(
        json,
        json!({"cancelling": "2", "cancelling_settles": "stop",
            "settle_when_idle": "0", "settle_at_stop": "2"})
    );
}

// T44 T17 (ADR 0031 §6): with an idle signal a custom engine keeps the
// built-in rules: it is asked, drains wait, and its reading settles it.
#[test]
fn a_cancelling_lease_with_an_idle_signal_settles_on_the_reading() {
    let (lab, a) = cancelling(true);
    let binding = lab.binding(&a);
    let generation = lab.instance(&a.deployment_id).2;
    let asked = lab.store.cancelling_bindings(&lab.session).unwrap();
    assert!(matches!(
        asked.as_slice(),
        [CancellingBinding { binding_id, leases: 1, .. }] if *binding_id == binding
    ));
    assert_eq!(
        lab.store.binding_outstanding_leases(&binding).unwrap(),
        Some(1)
    );
    assert_eq!(
        lab.store
            .switch_outstanding_leases(&a.deployment_id, 0, generation)
            .unwrap(),
        1
    );
    assert_eq!(
        lab.requests(&a).cancelling_settles,
        crate::snapshot::CancellingSettles::IdleSignal
    );
    assert_eq!(
        lab.store
            .settle_cancelled_leases(&lab.session, &binding, i64::MAX, "idle")
            .unwrap(),
        1
    );
    assert_eq!(lab.leases(&a), 0);
    assert_eq!(lab.requests(&a).cancelling, "0");
}

// T44 (ADR 0031 §6): the idle policy stops an instance whose only lease is a
// cancellation no idle signal can settle; it does not wait for it forever.
#[test]
fn the_idle_policy_stops_an_instance_holding_only_an_unsettled_cancellation() {
    let policy = IdlePolicy {
        ready_idle_ms: Some(5_000),
        parked_idle_ms: None,
    };
    let quiet = |_: &str, _: i64| None;
    let (lab, a) = cancelling(false);
    let actions = lab
        .store
        .apply_idle_policy(&lab.session, 7_000, policy, &quiet, 0)
        .unwrap();
    assert!(
        matches!(
            actions.as_slice(),
            [IdleAction::Stopped { deployment_id, reason: "ready_idle_restart_only", .. }]
                if *deployment_id == a.deployment_id
        ),
        "{actions:?}"
    );
    // With an idle signal the cancellation still holds the instance busy.
    let (lab, _) = cancelling(true);
    assert!(lab
        .store
        .apply_idle_policy(&lab.session, 7_000, policy, &quiet, 0)
        .unwrap()
        .is_empty());
}

/// Arm the stop `step` and complete it with gone evidence at `at`, or with
/// evidence naming another group when `verified` is false.
fn finish_stop(lab: &Lab, step: &str, at: i64, verified: bool) -> Result<(), LifecycleError> {
    let (_, context) = lab
        .store
        .arm_ordinary_cleanup_with_context(&lab.session, step, at)
        .unwrap();
    let context = context.unwrap();
    lab.store.complete_cleanup(
        &lab.session,
        step,
        &capyctl_domain::completion::CleanupEvidence {
            binding_id: context.binding_id,
            incarnation: context.incarnation,
            identities: if verified {
                context.identities
            } else {
                group(99)
            },
            observed_at_ms: at + 100,
            receipt: "gone".into(),
        },
        at + 100,
        2_000,
    )
}

// T44 T17 (ADR 0031 §6, SPEC §10): a switch releases the instance without
// waiting for the cancellation, by a stop; that stop's verified exit settles
// it with exactly one acknowledgement naming the stop, in the transaction
// that releases the reservation.
#[test]
fn a_switch_release_stops_and_its_verified_exit_settles_the_cancellation() {
    let (lab, a) = cancelling(false);
    let generation = lab.instance(&a.deployment_id).2;
    assert!(lab
        .store
        .close_for_switch(&lab.session, &a.deployment_id, 0, generation)
        .unwrap());
    let release = lab
        .store
        .accept_switch_release(
            &lab.session,
            "switch",
            &a.deployment_id,
            0,
            generation,
            "switch-release",
            1_100,
            true,
        )
        .unwrap();
    assert!(!release.parked);
    let step: String = lab.scalar(
        "SELECT id FROM lifecycle_steps WHERE operation_id=?1",
        &release.operation_id,
    );
    finish_stop(&lab, &step, 1_200, true).unwrap();
    assert_eq!(lab.leases(&a), 0);
    assert!(!lab
        .store
        .resource_snapshot()
        .unwrap()
        .owners
        .contains_key(&a.deployment_id));
    let acknowledged = lab.acknowledgements();
    assert_eq!(acknowledged.len(), 1, "{acknowledged:?}");
    assert_eq!(
        acknowledged[0].deployment_id.as_deref(),
        Some(a.deployment_id.as_str())
    );
    let payload: Value = serde_json::from_str(&acknowledged[0].payload_json).unwrap();
    assert_eq!(payload["count"], json!(1), "{payload}");
    let receipt = payload["receipt"].as_str().unwrap();
    assert!(receipt.contains(&release.operation_id), "{receipt}");
    assert!(receipt.contains(&step), "{receipt}");
}

// T44 T17 (ADR 0031 §6, SPEC §10): a stop that cannot prove the exit keeps the
// cancellation charged and the reservation held; nothing is acknowledged.
#[test]
fn an_unverified_stop_keeps_the_cancellation_and_the_reservation() {
    let (lab, a) = cancelling(false);
    let stop = lab
        .store
        .accept_instance_stop_command(
            &lab.session,
            "operator",
            &a.deployment_id,
            0,
            a.revision,
            "stop",
            1_100,
            60_000,
        )
        .unwrap()
        .unwrap();
    assert!(finish_stop(&lab, &stop.step_id, 1_200, false).is_err());
    assert_eq!(lab.leases(&a), 1);
    assert!(lab
        .store
        .resource_snapshot()
        .unwrap()
        .owners
        .contains_key(&a.deployment_id));
    assert!(lab.acknowledgements().is_empty());
    assert_eq!(lab.requests(&a).cancelling, "1");
}

// T44 T38 (ADR 0031 §6): after a role restart a generic quiescence reading
// never settles a lease on an engine that reports no idle signal.
#[test]
fn retired_leases_without_an_idle_signal_are_not_abandoned_on_a_reading() {
    let (lab, a) = cancelling(false);
    let step: String = lab.scalar(
        "SELECT s.id FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id
          WHERE o.deployment_id=?1 AND o.kind='initialize'",
        &a.deployment_id,
    );
    let error = lab
        .store
        .abandon_retired_request_leases(
            &lab.session,
            &step,
            &QuiescenceEvidence {
                binding_id: lab.binding(&a),
                incarnation: lab.scalar(
                    "SELECT incarnation FROM runtime_bindings WHERE deployment_id=?1",
                    &a.deployment_id,
                ),
                identities: group(10),
                readiness_observed_at_ms: 1_000,
                quiescent_at_ms: 1_000,
                receipt: "metrics read 0".into(),
            },
            1_000,
        )
        .unwrap_err();
    assert!(matches!(error, LifecycleError::Rejected(_)), "{error:?}");
    assert_eq!(lab.leases(&a), 1);
}

/// A Ready deep-parking built-in instance with one cancelling lease, answered
/// by the engine before its cancellation when `answered`.
fn built_in_cancelling(answered: bool) -> (Lab, DeploymentFence) {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    lab.ready(&a, 1_000, 10);
    let ticket = lab.grant(&a, 4).unwrap();
    lab.cancel(&ticket, answered);
    (lab, a)
}

// T17 T38 (SPEC §10, amended 2026-10-21): a request cut before the engine
// answered it is never settled by an idle reading, however late, and status
// counts it apart from one the engine answered, which still settles on the
// reading. Only the instance's verified stop settles it, journaled once with
// a receipt naming that stop and why.
#[test]
fn an_unanswered_cancellation_stays_charged_until_the_verified_stop() {
    let (lab, a) = built_in_cancelling(false);
    let binding = lab.binding(&a);
    assert!(lab
        .store
        .cancelling_bindings(&lab.session)
        .unwrap()
        .is_empty());
    assert_eq!(
        lab.store
            .settle_cancelled_leases(&lab.session, &binding, i64::MAX, "metrics read 0")
            .unwrap(),
        0
    );
    assert_eq!(lab.leases(&a), 1);
    // An answered one beside it settles on the reading; the other stays.
    let answered = lab.grant(&a, 4).unwrap();
    lab.cancel(&answered, true);
    let requests = lab.requests(&a);
    assert_eq!(
        serde_json::to_value(&requests).unwrap(),
        json!({"cancelling": "2", "cancelling_settles": "idle_signal",
            "settle_when_idle": "1", "settle_at_stop": "1"})
    );
    assert!(matches!(
        lab.store
            .cancelling_bindings(&lab.session)
            .unwrap()
            .as_slice(),
        [CancellingBinding { leases: 1, .. }]
    ));
    assert_eq!(
        lab.store
            .settle_cancelled_leases(&lab.session, &binding, i64::MAX, "metrics read 0")
            .unwrap(),
        1
    );
    assert_eq!(lab.leases(&a), 1);
    assert_eq!(lab.acknowledgements().len(), 1);
    // The stop does not wait for it, and its verified exit settles it.
    assert_eq!(
        lab.store.binding_outstanding_leases(&binding).unwrap(),
        Some(0)
    );
    let stop = lab
        .store
        .accept_instance_stop_command(
            &lab.session,
            "operator",
            &a.deployment_id,
            0,
            a.revision,
            "stop",
            1_100,
            60_000,
        )
        .unwrap()
        .unwrap();
    finish_stop(&lab, &stop.step_id, 1_200, true).unwrap();
    assert_eq!(lab.leases(&a), 0);
    let acknowledged = lab.acknowledgements();
    assert_eq!(acknowledged.len(), 2, "{acknowledged:?}");
    let payload: Value = serde_json::from_str(&acknowledged[1].payload_json).unwrap();
    assert_eq!(payload["count"], json!(1), "{payload}");
    let receipt = payload["receipt"].as_str().unwrap();
    assert!(
        receipt.starts_with("cancelled before the engine answered; stop operation"),
        "{receipt}"
    );
    assert!(receipt.contains(&stop.step_id), "{receipt}");
}

// T17 T38 (SPEC §10, amended 2026-10-21): an unverified stop keeps an
// unanswered cancellation charged; nothing is acknowledged.
#[test]
fn an_unverified_stop_keeps_an_unanswered_cancellation() {
    let (lab, a) = built_in_cancelling(false);
    let stop = lab
        .store
        .accept_instance_stop_command(
            &lab.session,
            "operator",
            &a.deployment_id,
            0,
            a.revision,
            "stop",
            1_100,
            60_000,
        )
        .unwrap()
        .unwrap();
    assert!(finish_stop(&lab, &stop.step_id, 1_200, false).is_err());
    assert_eq!(lab.leases(&a), 1);
    assert!(lab.acknowledgements().is_empty());
    assert_eq!(lab.requests(&a).settle_at_stop, "1");
}

// T17 (SPEC §10, amended 2026-10-21, §9.2): the idle policy stops, rather
// than parks, an instance whose only lease is a cancellation the engine
// never answered, and a switch releases it by a stop: a park would wait for
// it forever. One the engine answered still holds the instance busy.
#[test]
fn an_unanswered_cancellation_turns_a_park_into_a_stop() {
    let policy = IdlePolicy {
        ready_idle_ms: Some(5_000),
        parked_idle_ms: None,
    };
    let quiet = |_: &str, _: i64| None;
    let (lab, a) = built_in_cancelling(false);
    let actions = lab
        .store
        .apply_idle_policy(&lab.session, 7_000, policy, &quiet, 0)
        .unwrap();
    assert!(
        matches!(
            actions.as_slice(),
            [IdleAction::Stopped { deployment_id, reason: "ready_idle_unanswered_cancellation", .. }]
                if *deployment_id == a.deployment_id
        ),
        "{actions:?}"
    );
    let (lab, _) = built_in_cancelling(true);
    assert!(lab
        .store
        .apply_idle_policy(&lab.session, 7_000, policy, &quiet, 0)
        .unwrap()
        .is_empty());
    let (lab, a) = built_in_cancelling(false);
    let generation = lab.instance(&a.deployment_id).2;
    assert!(lab
        .store
        .close_for_switch(&lab.session, &a.deployment_id, 0, generation)
        .unwrap());
    let release = lab
        .store
        .accept_switch_release(
            &lab.session,
            "switch",
            &a.deployment_id,
            0,
            generation,
            "switch-release",
            1_100,
            true,
        )
        .unwrap();
    assert!(!release.parked);
}

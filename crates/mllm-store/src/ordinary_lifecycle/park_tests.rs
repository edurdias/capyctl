//! W5 store tests: park, restore, idle policy, parked-set bounds and
//! preinitialize against the durable lifecycle. CPU only; nothing here
//! qualifies an engine.
use super::*;
use crate::Store;
use mllm_config::effective::resolve_effective;
use mllm_domain::completion::Milestone;
use mllm_domain::resources::MemoryObservation;
use serde_json::{json, Value};

fn fixture() -> (Value, Value) {
    let value: Value = serde_json::from_str(include_str!(
        "../../../mllm-config/tests/fixtures/f2-deployment.json"
    ))
    .unwrap();
    (value["deployment"].clone(), value["host"].clone())
}

fn identity(role: &str, pid: u32) -> ProcessIdentity {
    ProcessIdentity {
        role: role.into(),
        pid,
        boot_id: "boot-1".into(),
        start_ticks: 1,
    }
}

fn group(base: u32) -> Vec<ProcessIdentity> {
    vec![identity("api", base), identity("worker-0", base + 1)]
}

struct Lab {
    store: Store,
    session: CoordinatorSession,
    host: Value,
}

impl Lab {
    fn new(edit_host: impl FnOnce(&mut Value)) -> Self {
        let (mut config, mut host) = fixture();
        edit_host(&mut host);
        if host["runtime_profiles"]["local"]["security"]["deep_park"] == "disabled" {
            // ADR 0012: an opted-out host takes only restart-only deployments.
            config["residency"] = json!("restart_only");
        }
        let effective = resolve_effective(&config, &host).unwrap();
        let store = Store::open_in_memory().unwrap();
        let session = store.begin_coordinator_session().unwrap();
        store
            .import_resource_policy(&session, &effective.host, &observed(1), 1)
            .unwrap();
        Self {
            store,
            session,
            host,
        }
    }

    fn deploy(&self, name: &str, edit: impl FnOnce(&mut Value)) -> DeploymentFence {
        let (mut config, _) = fixture();
        config["name"] = json!(name);
        config["routes"] = json!([name]);
        edit(&mut config);
        let body = json!({ "config": config }).to_string();
        let receipt = self
            .store
            .create_stopped_managed_configuration(
                &self.session,
                "principal",
                name,
                &body,
                &self.host,
                10,
            )
            .unwrap();
        DeploymentFence {
            deployment_id: receipt.deployment_id,
            revision: receipt.revision,
            generation: receipt.generation,
        }
    }

    fn context<'a>(
        &self,
        observations: &'a [MemoryObservation],
        limits: &'a [MemoryLimit],
        now: i64,
    ) -> AdmissionContext<'a> {
        let policy = self.store.resource_policy("lab").unwrap().unwrap();
        AdmissionContext::new(
            observations,
            limits,
            now,
            policy.controls.observation_ttl_ms,
            policy.controls.max_parked as usize,
        )
    }

    fn limits(&self) -> Vec<MemoryLimit> {
        let policy = self.store.resource_policy("lab").unwrap().unwrap();
        policy
            .controls
            .domains
            .iter()
            .map(|(domain, d)| MemoryLimit {
                domain: domain.clone(),
                managed_bytes: d.managed_limit,
                free_reserve_bytes: d.free_reserve,
                host_kv_bytes: d.host_kv_limit,
                parked_bytes: d.parked_limit,
            })
            .collect()
    }

    /// Start `fence` and bring it Ready at `now` with the group from `base`.
    fn ready(&self, fence: &DeploymentFence, now: i64, base: u32) {
        let accepted = self
            .store
            .accept_start(&self.session, fence, now, now + 100_000)
            .unwrap();
        let observations = observed(now);
        let limits = self.limits();
        self.store
            .arm_step(
                &self.session,
                &accepted.step_id,
                self.context(&observations, &limits, now),
            )
            .unwrap();
        let execution = self
            .store
            .initialize_execution(&self.session, &accepted.step_id)
            .unwrap();
        self.store
            .record_owned_launch(
                &self.session,
                &accepted.step_id,
                &OwnedLaunchReceipt {
                    binding_id: execution.binding_id.clone(),
                    incarnation: execution.incarnation.clone(),
                    identities: group(base),
                    observed_at_ms: now,
                    receipt: "fake ready".into(),
                },
                now,
            )
            .unwrap();
        self.store
            .complete_step(
                &self.session,
                &accepted.step_id,
                &CompletionEvidence {
                    token: execution.token,
                    identities: group(base),
                    observed_at_ms: now,
                    control_receipt: Some("fake ready".into()),
                    milestones: ResidencyKind::Restore.facts().to_vec(),
                },
                now,
                2_000,
            )
            .unwrap();
    }

    fn arm(&self, step: &str, now: i64) -> ResidencyArm {
        let observations = observed(now);
        let limits = self.limits();
        self.store
            .arm_residency(
                &self.session,
                step,
                self.context(&observations, &limits, now),
            )
            .unwrap()
    }

    fn complete(&self, context: &StepExecutionContext, kind: ResidencyKind, base: u32, now: i64) {
        self.store
            .complete_residency(
                &self.session,
                &context.token.step_id,
                &EffectObservation {
                    token: context.token.clone(),
                    binding_id: context.binding_id.clone(),
                    incarnation: context.incarnation.clone(),
                    identities: group(base),
                    observed_at_ms: now,
                    receipt: "fake host".into(),
                    facts: kind.facts().to_vec(),
                },
                now,
            )
            .unwrap();
    }

    fn park(&self, fence: &DeploymentFence, key: &str, now: i64) -> ResidencyReceipt {
        self.store
            .accept_park_command(
                &self.session,
                "operator",
                &fence.deployment_id,
                fence.revision,
                key,
                now,
                now + 60_000,
            )
            .unwrap()
    }

    fn wake(
        &self,
        fence: &DeploymentFence,
        scope: WakeScope,
        key: &str,
        now: i64,
    ) -> Option<ResidencyReceipt> {
        self.store
            .accept_restore_command(
                &self.session,
                "router",
                &fence.deployment_id,
                scope,
                fence.revision,
                key,
                now,
                now + 60_000,
            )
            .unwrap()
    }

    fn scalar<T: rusqlite::types::FromSql>(&self, sql: &str, id: &str) -> T {
        self.store.conn.query_row(sql, [id], |r| r.get(0)).unwrap()
    }

    fn instance(&self, id: &str) -> (String, bool, i64) {
        self.store
            .conn
            .query_row(
                "SELECT observed_state,dispatch_enabled=1,generation FROM deployment_instances WHERE deployment_id=?1 AND instance_index=0",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    }

    fn phase(&self, id: &str) -> ResourcePhase {
        self.store.resource_snapshot().unwrap().owners[id].phase
    }

    fn reported(&self, id: &str) -> String {
        let snapshot = self.store.snapshot().unwrap();
        let d = snapshot.deployments.iter().find(|d| d.id == id).unwrap();
        d.observed_state.clone()
    }
}

fn observed(now: i64) -> Vec<MemoryObservation> {
    vec![MemoryObservation {
        domain: "unified".into(),
        capacity_bytes: 64 << 30,
        available_bytes: 60 << 30,
        sampled_at_ms: now,
    }]
}

fn new_context(arm: ResidencyArm) -> StepExecutionContext {
    match arm {
        ResidencyArm::New(context) => *context,
        other => panic!("expected a fresh arm, got {other:?}"),
    }
}

// T16 (A -> park -> wake -> A): the same launch, generation and binding, with
// the reservation moved through the budgeted phases on evidence only.
#[test]
fn park_then_restore_keeps_the_launch_and_moves_the_reservation_on_evidence() {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    lab.ready(&a, 1_000, 10);
    let (_, open, generation) = lab.instance(&a.deployment_id);
    assert!(open);
    let park = lab.park(&a, "park-1", 1_100);
    assert!(!park.joined);
    // SPEC §10: dispatch closes at acceptance, before the drain.
    assert_eq!(
        lab.instance(&a.deployment_id),
        ("ready".into(), false, generation)
    );
    assert_eq!(lab.reported(&a.deployment_id), "parking");
    // An exact retry replays the receipt.
    assert_eq!(lab.park(&a, "park-1", 1_100), park);
    let context = new_context(lab.arm(&park.step_id, 1_200));
    assert_eq!(context.token.generation, generation);
    assert!(matches!(&context.identities, ExecutionIdentities::Retained(ids) if *ids == group(10)));
    assert!(context.completion_target.is_none() && context.launch_settings.is_none());
    assert_eq!(lab.phase(&a.deployment_id), ResourcePhase::Parking);
    lab.complete(&context, ResidencyKind::Park, 10, 1_300);
    assert_eq!(
        lab.instance(&a.deployment_id),
        ("parked".into(), false, generation)
    );
    assert_eq!(lab.phase(&a.deployment_id), ResourcePhase::Parked);
    assert_eq!(lab.reported(&a.deployment_id), "parked");

    // Owner decision Q5 / ADR 0013 §4: the wake restores in place.
    let wake = lab.wake(&a, WakeScope::OnDemand, "wake-1", 2_000).unwrap();
    assert_eq!(wake.generation, generation);
    assert_eq!(lab.reported(&a.deployment_id), "waking");
    // T15: a second on-demand request joins the same restore.
    let joined = lab.wake(&a, WakeScope::OnDemand, "wake-2", 2_001).unwrap();
    assert_eq!(joined.operation_id, wake.operation_id);
    assert!(joined.joined);
    let context = new_context(lab.arm(&wake.step_id, 2_100));
    assert_eq!(lab.phase(&a.deployment_id), ResourcePhase::Wake);
    lab.complete(&context, ResidencyKind::Restore, 10, 2_200);
    assert_eq!(
        lab.instance(&a.deployment_id),
        ("ready".into(), true, generation)
    );
    assert_eq!(lab.phase(&a.deployment_id), ResourcePhase::Ready);
    // Nothing parked any more: an on-demand wake finds nothing to restore.
    assert!(lab.wake(&a, WakeScope::OnDemand, "wake-3", 2_300).is_none());
}

// SPEC §6.1: a restore commits only with a usable model and the recorded
// group; a park only with released memory.
#[test]
fn evidence_must_name_the_retained_group_and_exact_facts() {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    lab.ready(&a, 1_000, 10);
    let park = lab.park(&a, "p", 1_100);
    let context = new_context(lab.arm(&park.step_id, 1_200));
    let observation = |ids: Vec<ProcessIdentity>, facts: Vec<Milestone>| EffectObservation {
        token: context.token.clone(),
        binding_id: context.binding_id.clone(),
        incarnation: context.incarnation.clone(),
        identities: ids,
        observed_at_ms: 1_250,
        receipt: "host".into(),
        facts,
    };
    for (ids, facts) in [
        (group(99), vec![Milestone::MemoryReleased]),
        (group(10), vec![Milestone::ModelUsable]),
        (group(10), vec![]),
    ] {
        assert!(lab
            .store
            .complete_residency(&lab.session, &park.step_id, &observation(ids, facts), 1_260)
            .is_err());
    }
    // Stale evidence (older than the observation ttl) is refused too.
    assert!(lab
        .store
        .complete_residency(
            &lab.session,
            &park.step_id,
            &observation(group(10), vec![Milestone::MemoryReleased]),
            9_999
        )
        .is_err());
    assert_eq!(lab.phase(&a.deployment_id), ResourcePhase::Parking);
}

// T20 (W4 hand-off): a park refused before any effect settles at once; the
// embedded launch serves again and its reservation returns to Ready.
#[test]
fn a_refused_park_reopens_an_embedded_launch_and_returns_its_reservation() {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    lab.ready(&a, 1_000, 10);
    let park = lab.park(&a, "p", 1_100);
    let _ = new_context(lab.arm(&park.step_id, 1_200));
    lab.store
        .refuse_residency(&lab.session, &park.step_id, "host refused: unchanged")
        .unwrap();
    assert_eq!(lab.instance(&a.deployment_id).0, "ready");
    assert!(lab.instance(&a.deployment_id).1, "dispatch reopens");
    assert_eq!(lab.phase(&a.deployment_id), ResourcePhase::Ready);
    let code: Option<String> = lab.scalar(
        "SELECT error_code FROM operations WHERE id=?1",
        &park.operation_id,
    );
    assert_eq!(code.as_deref(), Some("park_refused"));
    // Nothing is retained for it: a new park can be accepted.
    lab.park(&a, "p2", 1_300);
}

// T20, AGENTS.md: an uncertain park keeps the peak reservation, the claim and
// the closed gate; it is never re-armed; a stop takes it over and settles it
// only on gone evidence.
#[test]
fn an_uncertain_park_keeps_accounting_until_a_stop_proves_the_group_gone() {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    lab.ready(&a, 1_000, 10);
    let park = lab.park(&a, "p", 1_100);
    let _ = new_context(lab.arm(&park.step_id, 1_200));
    assert!(lab
        .store
        .mark_residency_uncertain(&lab.session, &park.step_id, "sleep outcome unknown")
        .unwrap());
    assert_eq!(lab.phase(&a.deployment_id), ResourcePhase::Parking);
    assert!(!lab.instance(&a.deployment_id).1);
    assert_eq!(lab.reported(&a.deployment_id), "uncertain");
    // Never re-armed, and not offered as work.
    let observations = observed(1_300);
    let limits = lab.limits();
    assert!(lab
        .store
        .arm_residency(
            &lab.session,
            &park.step_id,
            lab.context(&observations, &limits, 1_300)
        )
        .is_err());
    assert!(lab
        .store
        .next_residency_work(&lab.session, 1_300)
        .unwrap()
        .is_empty());
    // A stop takes the uncertain park over.
    let stop = lab
        .store
        .accept_instance_stop_command(
            &lab.session,
            "operator",
            &a.deployment_id,
            0,
            a.revision,
            "stop",
            1_400,
            60_000,
        )
        .unwrap()
        .unwrap();
    let (_, context) = lab
        .store
        .arm_ordinary_cleanup_with_context(&lab.session, &stop.step_id, 1_500)
        .unwrap();
    let context = context.unwrap();
    // Still retained while the stop runs.
    assert_eq!(lab.phase(&a.deployment_id), ResourcePhase::Parking);
    lab.store
        .complete_cleanup(
            &lab.session,
            &stop.step_id,
            &mllm_domain::completion::CleanupEvidence {
                binding_id: context.binding_id,
                incarnation: context.incarnation,
                identities: context.identities,
                observed_at_ms: 1_600,
                receipt: "gone".into(),
            },
            1_600,
            2_000,
        )
        .unwrap();
    assert!(!lab
        .store
        .resource_snapshot()
        .unwrap()
        .owners
        .contains_key(&a.deployment_id));
    let code: Option<String> = lab.scalar(
        "SELECT error_code FROM operations WHERE id=?1",
        &park.operation_id,
    );
    assert_eq!(code.as_deref(), Some("resolved_by_owned_cleanup"));
}

// T21, SPEC §6.2/§6.3: restart_only never parks and never receives a sleep;
// preinitialize must fail rather than claim it prewarmed.
#[test]
fn restart_only_and_opted_out_hosts_refuse_park_and_preinitialize() {
    for deep_park in ["enabled", "disabled"] {
        let lab = Lab::new(|host| {
            host["runtime_profiles"]["local"]["security"]["deep_park"] = json!(deep_park);
        });
        let a = lab.deploy("a", |config| {
            config["residency"] = json!("restart_only");
        });
        lab.ready(&a, 1_000, 10);
        assert!(matches!(
            lab.store.accept_park_command(
                &lab.session,
                "operator",
                &a.deployment_id,
                a.revision,
                "p",
                1_100,
                61_100
            ),
            Err(LifecycleError::Unsupported)
        ));
        assert!(matches!(
            lab.store.accept_preinitialize_command(
                &lab.session,
                "operator",
                &a.deployment_id,
                a.revision,
                "pre",
                1_100,
                61_100
            ),
            Err(LifecycleError::Unsupported)
        ));
        let (observed, open, _) = lab.instance(&a.deployment_id);
        assert_eq!((observed.as_str(), open), ("ready", true));
    }
}

// SPEC §10: requests admitted before the park drain first; the park arms only
// once no lease remains, and closes (no effect) if its deadline passes first.
#[test]
fn a_park_waits_for_leases_and_closes_unarmed_at_its_deadline() {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    lab.ready(&a, 1_000, 10);
    let generation = lab.instance(&a.deployment_id).2;
    lab.store
        .conn
        .execute(
            "INSERT INTO request_leases(id,deployment_id,revision,generation,session_id,disposition,instance_index) VALUES('lease',?1,?2,?3,?4,'inflight',0)",
            params![a.deployment_id, a.revision, generation, lab.session.id()],
        )
        .unwrap();
    let park = lab.park(&a, "p", 1_100);
    assert_eq!(lab.arm(&park.step_id, 1_200), ResidencyArm::Draining);
    assert_eq!(lab.phase(&a.deployment_id), ResourcePhase::Ready);
    // The deadline passes before the drain: closed, nothing sent, dispatch reopened.
    assert!(lab
        .store
        .next_residency_work(&lab.session, 61_200)
        .unwrap()
        .is_empty());
    let (state, code): (String, Option<String>) = lab
        .store
        .conn
        .query_row(
            "SELECT state,error_code FROM operations WHERE id=?1",
            [&park.operation_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (state.as_str(), code.as_deref()),
        ("failed", Some("park_deadline"))
    );
    assert!(lab.instance(&a.deployment_id).1);
}

// SPEC §6.5, T26 T27: max_parked is enforced by stopping the least recently
// parked instance first, never a Ready one.
#[test]
fn a_full_parked_set_reclaims_the_least_recently_parked_instance() {
    let lab = Lab::new(|host| {
        host["resource_policy"]["max_parked"] = json!(1);
    });
    let a = lab.deploy("a", |_| {});
    let b = lab.deploy("b", |_| {});
    lab.ready(&a, 1_000, 10);
    lab.ready(&b, 1_000, 20);
    let park_a = lab.park(&a, "pa", 1_100);
    let context = new_context(lab.arm(&park_a.step_id, 1_200));
    lab.complete(&context, ResidencyKind::Park, 10, 1_300);
    let park_b = lab.park(&b, "pb", 1_400);
    let stops = match lab.arm(&park_b.step_id, 1_500) {
        ResidencyArm::Reclaiming(stops) => stops,
        other => panic!("expected reclamation, got {other:?}"),
    };
    assert_eq!(stops.len(), 1);
    let stopped: String = lab.scalar(
        "SELECT deployment_id FROM operations WHERE id=?1",
        &stops[0],
    );
    assert_eq!(stopped, a.deployment_id);
    // An ordinary stop: the deployment stays eligible for on-demand activation.
    let admin: bool = lab.scalar(
        "SELECT admin_stopped=1 FROM deployments WHERE id=?1",
        &a.deployment_id,
    );
    assert!(!admin);
    // The park of B still waits, unarmed, until A's cleanup completes.
    assert_eq!(lab.phase(&b.deployment_id), ResourcePhase::Ready);
    let step: String = lab.scalar(
        "SELECT state FROM lifecycle_steps WHERE id=?1",
        &park_b.step_id,
    );
    assert_eq!(step, "planned");
    // While A's stop runs, B waits; nothing else is reclaimed or refused.
    assert!(matches!(
        lab.arm(&park_b.step_id, 1_600),
        ResidencyArm::Blocked(_)
    ));
    // A's verified cleanup releases it; B then parks.
    let (_, cleanup) = lab
        .store
        .arm_ordinary_cleanup_with_context(
            &lab.session,
            &lab.scalar::<String>(
                "SELECT id FROM lifecycle_steps WHERE operation_id=?1",
                &stops[0],
            ),
            1_700,
        )
        .unwrap();
    let cleanup = cleanup.unwrap();
    lab.store
        .complete_cleanup(
            &lab.session,
            &cleanup.step_id,
            &mllm_domain::completion::CleanupEvidence {
                binding_id: cleanup.binding_id.clone(),
                incarnation: cleanup.incarnation.clone(),
                identities: cleanup.identities.clone(),
                observed_at_ms: 1_750,
                receipt: "gone".into(),
            },
            1_750,
            2_000,
        )
        .unwrap();
    let context = new_context(lab.arm(&park_b.step_id, 1_800));
    lab.complete(&context, ResidencyKind::Park, 20, 1_900);
    assert_eq!(lab.phase(&b.deployment_id), ResourcePhase::Parked);
}

// SPEC §6.5, T23: a cold start that does not fit reclaims the least recently
// parked instance on its host first; a start that fits reclaims nothing.
#[test]
fn a_cold_start_reclaims_parked_capacity_before_it_arms() {
    let lab = Lab::new(|host| {
        // 32 GiB managed: A parked (2) + B Ready (8) + C cold (10) fit; a
        // fourth cold start (10) + C Ready does not without A.
        host["resource_policy"]["domains"]["unified"]["managed_limit"] = json!("24GiB");
    });
    let a = lab.deploy("a", |_| {});
    let b = lab.deploy("b", |_| {});
    let c = lab.deploy("c", |_| {});
    lab.ready(&a, 1_000, 10);
    lab.ready(&b, 1_000, 20);
    let park = lab.park(&a, "p", 1_100);
    let context = new_context(lab.arm(&park.step_id, 1_200));
    lab.complete(&context, ResidencyKind::Park, 10, 1_300);
    // A parked (2) + B Ready (8) = 10 GiB; C's cold 10 GiB fits in 24.
    let start = lab
        .store
        .accept_start(&lab.session, &c, 1_400, 101_400)
        .unwrap();
    let observations = observed(1_500);
    let limits = lab.limits();
    assert_eq!(
        lab.store
            .reclaim_for_start(
                &lab.session,
                &start.step_id,
                lab.context(&observations, &limits, 1_500)
            )
            .unwrap(),
        None
    );
    // With B at its 10 GiB wake peak instead of Ready, C (10) needs A's 2.
    lab.store
        .conn
        .execute(
            "UPDATE resource_owners SET footprint_json=?2 WHERE owner_id=?1",
            params![
                b.deployment_id,
                resource_ledger::encode(&PhaseFootprint {
                    phase: ResourcePhase::Ready,
                    allocations: vec![mllm_domain::resources::Allocation {
                        domain: "unified".into(),
                        bytes: 13 << 30,
                        host_kv_bytes: 1 << 30,
                    }],
                    devices: vec![mllm_domain::resources::DeviceClaim {
                        device: "gpu0".into(),
                        sharing: Sharing::Shared,
                    }],
                })
                .unwrap()
            ],
        )
        .unwrap();
    let reason = lab
        .store
        .reclaim_for_start(
            &lab.session,
            &start.step_id,
            lab.context(&observations, &limits, 1_500),
        )
        .unwrap();
    assert!(reason.is_some_and(|r| r.contains("reclaiming 1")));
    let stopping: bool = lab.scalar(
        "SELECT EXISTS(SELECT 1 FROM lifecycle_runs WHERE deployment_id=?1 AND action='stop' AND state='queued')",
        &a.deployment_id,
    );
    assert!(stopping);
}

// SPEC §6.5: with max_parked 0 a park can never fit and is refused clearly.
#[test]
fn a_park_that_can_never_fit_the_parked_set_is_refused() {
    let lab = Lab::new(|host| {
        host["resource_policy"]["max_parked"] = json!(0);
    });
    let a = lab.deploy("a", |_| {});
    lab.ready(&a, 1_000, 10);
    let park = lab.park(&a, "p", 1_100);
    assert_eq!(
        lab.arm(&park.step_id, 1_200),
        ResidencyArm::Refused("parked_capacity")
    );
    assert!(lab.instance(&a.deployment_id).1, "the launch serves again");
    assert_eq!(lab.phase(&a.deployment_id), ResourcePhase::Ready);
}

// SPEC §6.5: ready-idle parks (restart-only stops), parked-idle stops; both
// leave automatic activation enabled.
#[test]
fn idle_timers_park_then_stop() {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    let r = lab.deploy("r", |config| config["residency"] = json!("restart_only"));
    lab.ready(&a, 1_000, 10);
    lab.ready(&r, 1_000, 20);
    let policy = IdlePolicy {
        ready_idle_ms: Some(5_000),
        parked_idle_ms: Some(10_000),
    };
    let quiet = |_: &str, _: i64| None;
    // Not idle long enough yet.
    assert!(lab
        .store
        .apply_idle_policy(&lab.session, 4_000, policy, &quiet, 0)
        .unwrap()
        .is_empty());
    // Router activity resets the ready timer.
    let busy = |_: &str, _: i64| Some(5_500);
    assert!(lab
        .store
        .apply_idle_policy(&lab.session, 7_000, policy, &busy, 0)
        .unwrap()
        .is_empty());
    let actions = lab
        .store
        .apply_idle_policy(&lab.session, 7_000, policy, &quiet, 0)
        .unwrap();
    assert_eq!(actions.len(), 2);
    let park = actions.iter().find_map(|a| match a {
        IdleAction::Parked { operation_id, .. } => Some(operation_id.clone()),
        _ => None,
    });
    let stop = actions.iter().find_map(|a| match a {
        IdleAction::Stopped {
            deployment_id,
            reason,
            ..
        } => Some((deployment_id.clone(), *reason)),
        _ => None,
    });
    assert_eq!(
        stop,
        Some((r.deployment_id.clone(), "ready_idle_restart_only"))
    );
    let step: String = lab.scalar(
        "SELECT id FROM lifecycle_steps WHERE operation_id=?1",
        &park.unwrap(),
    );
    let context = new_context(lab.arm(&step, 7_100));
    lab.complete(&context, ResidencyKind::Park, 10, 7_200);
    // Parked-idle: measured from the park's evidence.
    assert!(lab
        .store
        .apply_idle_policy(&lab.session, 12_000, policy, &quiet, 0)
        .unwrap()
        .is_empty());
    let actions = lab
        .store
        .apply_idle_policy(&lab.session, 17_300, policy, &quiet, 0)
        .unwrap();
    assert!(matches!(
        actions.as_slice(),
        [IdleAction::Stopped {
            reason: "parked_idle",
            ..
        }]
    ));
    let admin: bool = lab.scalar(
        "SELECT admin_stopped=1 FROM deployments WHERE id=?1",
        &a.deployment_id,
    );
    assert!(!admin);
}

// SPEC §6.5: preinitialize runs start, verify, park per instance in turn and
// succeeds only when every instance is parked.
#[test]
fn preinitialize_starts_verifies_and_parks_in_sequence() {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    let receipt = lab
        .store
        .accept_preinitialize_command(
            &lab.session,
            "operator",
            &a.deployment_id,
            a.revision,
            "pre",
            1_000,
            200_000,
        )
        .unwrap();
    // A second request joins it.
    let joined = lab
        .store
        .accept_preinitialize_command(
            &lab.session,
            "operator",
            &a.deployment_id,
            a.revision,
            "pre-2",
            1_000,
            200_000,
        )
        .unwrap();
    assert_eq!(joined.operation_id, receipt.operation_id);
    assert!(joined.joined);
    let progress = lab
        .store
        .advance_preinitialize(&lab.session, 1_100, None)
        .unwrap();
    assert!(matches!(
        progress.as_slice(),
        [PreinitializeProgress::Started { instance: 0, .. }]
    ));
    // While the start is in flight nothing else happens.
    assert!(lab
        .store
        .advance_preinitialize(&lab.session, 1_150, None)
        .unwrap()
        .is_empty());
    // Drive the start Ready as the worker would.
    let step: String = lab.scalar(
        "SELECT s.id FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id WHERE o.deployment_id=?1 AND o.kind='initialize' AND s.state='planned'",
        &a.deployment_id,
    );
    let observations = observed(1_200);
    let limits = lab.limits();
    lab.store
        .arm_step(
            &lab.session,
            &step,
            lab.context(&observations, &limits, 1_200),
        )
        .unwrap();
    let execution = lab.store.initialize_execution(&lab.session, &step).unwrap();
    lab.store
        .record_owned_launch(
            &lab.session,
            &step,
            &OwnedLaunchReceipt {
                binding_id: execution.binding_id.clone(),
                incarnation: execution.incarnation.clone(),
                identities: group(10),
                observed_at_ms: 1_300,
                receipt: "ready".into(),
            },
            1_300,
        )
        .unwrap();
    lab.store
        .complete_step(
            &lab.session,
            &step,
            &CompletionEvidence {
                token: execution.token,
                identities: group(10),
                observed_at_ms: 1_300,
                control_receipt: Some("ready".into()),
                milestones: ResidencyKind::Restore.facts().to_vec(),
            },
            1_300,
            2_000,
        )
        .unwrap();
    let progress = lab
        .store
        .advance_preinitialize(&lab.session, 1_400, None)
        .unwrap();
    assert!(matches!(
        progress.as_slice(),
        [PreinitializeProgress::Parking { instance: 0, .. }]
    ));
    let work = lab.store.next_residency_work(&lab.session, 1_450).unwrap();
    assert_eq!(work.len(), 1);
    let context = new_context(lab.arm(&work[0].step_id, 1_500));
    lab.complete(&context, ResidencyKind::Park, 10, 1_600);
    let progress = lab
        .store
        .advance_preinitialize(&lab.session, 1_700, None)
        .unwrap();
    assert!(matches!(
        progress.as_slice(),
        [PreinitializeProgress::Finished {
            outcome: "succeeded",
            ..
        }]
    ));
    let state: String = lab.scalar(
        "SELECT state FROM operations WHERE id=?1",
        &receipt.operation_id,
    );
    assert_eq!(state, "succeeded");
    assert_eq!(lab.instance(&a.deployment_id).0, "parked");
}

// Owner decision Q5, ADR 0013 §4: an on-demand wake prefers the parked
// instance to a cold start, and an explicit start wakes every parked one.
#[test]
fn wake_scopes_choose_parked_instances() {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    // Nothing parked: the caller starts cold instead.
    assert!(lab.wake(&a, WakeScope::All, "w0", 900).is_none());
    lab.ready(&a, 1_000, 10);
    let park = lab.park(&a, "p", 1_100);
    let context = new_context(lab.arm(&park.step_id, 1_200));
    lab.complete(&context, ResidencyKind::Park, 10, 1_300);
    assert!(lab.wake(&a, WakeScope::Instance(3), "w1", 1_400).is_none());
    let woken = lab.wake(&a, WakeScope::All, "w2", 1_400).unwrap();
    assert_eq!(woken.instance, 0);
    // A park cannot be accepted while the restore is open.
    assert!(lab
        .store
        .accept_park_command(
            &lab.session,
            "operator",
            &a.deployment_id,
            a.revision,
            "p2",
            1_500,
            61_500
        )
        .is_err());
}

// T33 (SPEC §13.2): a restarted controller adopts a parked launch and a launch
// whose park was in flight. The parked one stays parked and wakes under the
// new session; the in-flight one is uncertain with its accounting and is
// never re-armed; a planned one is closed without effect.
#[test]
fn a_restart_adopts_parked_launches_and_their_residency_work() {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    let b = lab.deploy("b", |_| {});
    lab.ready(&a, 1_000, 10);
    lab.ready(&b, 1_000, 20);
    let park = lab.park(&a, "pa", 1_100);
    let context = new_context(lab.arm(&park.step_id, 1_200));
    lab.complete(&context, ResidencyKind::Park, 10, 1_300);
    let park_b = lab.park(&b, "pb", 1_400);
    let _ = new_context(lab.arm(&park_b.step_id, 1_500));
    // The controller dies with B's park armed.
    let session = lab.store.begin_coordinator_session().unwrap();
    let retired = lab.store.retired_local_launches(&session).unwrap();
    assert_eq!(retired.len(), 2, "both launches are adoptable");
    for launch in &retired {
        lab.store
            .adopt_retired_local_launch(&session, launch.work.step_id())
            .unwrap();
    }
    let lab = Lab { session, ..lab };
    assert_eq!(lab.instance(&a.deployment_id).0, "parked");
    // B's park is uncertain (armed at the restart), adopted, never re-armed.
    let state: String = lab.scalar(
        "SELECT state FROM lifecycle_steps WHERE id=?1",
        &park_b.step_id,
    );
    assert_eq!(state, "uncertain");
    assert_eq!(lab.phase(&b.deployment_id), ResourcePhase::Parking);
    assert!(lab
        .store
        .next_residency_work(&lab.session, 1_600)
        .unwrap()
        .is_empty());
    // A wakes under the new session.
    let wake = lab.wake(&a, WakeScope::OnDemand, "w", 1_700).unwrap();
    let context = new_context(lab.arm(&wake.step_id, 1_800));
    lab.complete(&context, ResidencyKind::Restore, 10, 1_900);
    assert_eq!(lab.instance(&a.deployment_id).0, "ready");
}

// T16 T23: found live 2026-09-23 (matrix M27, host-a). A park allocates
// nothing: its parking phase is the Ready footprint (derived configurations)
// and its parked phase is smaller. The launch's own charge is already in use,
// so the host's published free memory never covered it again, and every park
// of a large Ready model was held "insufficient resources" until its deadline.
// A transition that increases no allocation arms on the parked-set rules alone.
#[test]
fn a_park_that_increases_nothing_arms_when_free_memory_is_low() {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |config| {
        config["resources"]["parking"]["allocations"][0]["bytes"] = json!("8GiB");
    });
    lab.ready(&a, 1_000, 10);
    let park = lab.park(&a, "park-low", 1_100);
    // 8 GiB held and in use; only 10 GiB free, below the 16 GiB free reserve.
    let low = vec![MemoryObservation {
        domain: "unified".into(),
        capacity_bytes: 64 << 30,
        available_bytes: 10 << 30,
        sampled_at_ms: 1_200,
    }];
    let limits = lab.limits();
    let armed = lab
        .store
        .arm_residency(
            &lab.session,
            &park.step_id,
            lab.context(&low, &limits, 1_200),
        )
        .unwrap();
    let context = new_context(armed);
    lab.complete(&context, ResidencyKind::Park, 10, 1_300);
    assert_eq!(lab.phase(&a.deployment_id), ResourcePhase::Parked);
}

// T16 T26: found live on the 16 GB discrete-GPU laptop host. A host_backed
// park grows the system domain by the weights copy and keeps its GPU charge
// in the parking phase, so the M27 rule (nothing increases) did not apply and
// the whole parking footprint was charged against free memory again: the
// card's 12 GiB, already in use by the engine being parked, against 2.5 GiB
// free. A park is charged only what it adds beyond what the owner holds.
#[test]
fn a_park_is_charged_only_what_it_adds_beyond_its_own_charge() {
    // One rule on every host shape (final review I5, owner rule 2026-09-26):
    // a unified domain is judged exactly as a discrete host's.
    let lab = Lab::new(|host| {
        host["resource_policy"]["domains"]["unified"]["memory"] = json!("distinct");
    });
    // Ready holds 8 GiB; parking needs 9 GiB (the fixture), 1 GiB more.
    let a = lab.deploy("a", |_| {});
    lab.ready(&a, 1_000, 10);
    let park = lab.park(&a, "park-grows", 1_100);
    let limits = lab.limits();
    let arm = |available: i64, now: i64| {
        let observation = vec![MemoryObservation {
            domain: "unified".into(),
            capacity_bytes: 64 << 30,
            available_bytes: available,
            sampled_at_ms: now,
        }];
        lab.store
            .arm_residency(
                &lab.session,
                &park.step_id,
                lab.context(&observation, &limits, now),
            )
            .unwrap()
    };
    // 16.5 GiB free less the 1 GiB it adds is below the 16 GiB reserve.
    assert!(matches!(
        arm((33 << 30) / 2, 1_200),
        ResidencyArm::Blocked(_)
    ));
    // 17 GiB free: the added 1 GiB fits; its own 8 GiB is not charged again.
    let context = new_context(arm(17 << 30, 1_300));
    lab.complete(&context, ResidencyKind::Park, 10, 1_400);
    assert_eq!(lab.phase(&a.deployment_id), ResourcePhase::Parked);
    // A unified domain follows the same rule: the same park arms there
    // (before the one-rule change it waited, charged its own 8 GiB again).
    let unified = Lab::new(|_| {});
    let u = unified.deploy("u", |_| {});
    unified.ready(&u, 1_000, 10);
    let park = unified.park(&u, "park-unified", 1_100);
    let limits = unified.limits();
    let observation = vec![MemoryObservation {
        domain: "unified".into(),
        capacity_bytes: 64 << 30,
        available_bytes: 17 << 30,
        sampled_at_ms: 1_300,
    }];
    assert!(matches!(
        unified
            .store
            .arm_residency(
                &unified.session,
                &park.step_id,
                unified.context(&observation, &limits, 1_300),
            )
            .unwrap(),
        ResidencyArm::New(_)
    ));
}

// T16 T26: found live on the 16 GB discrete-GPU laptop host. A switch
// planned a host_backed park against the ledger, but host RAM (most of it held
// by other programs) could not take the copy: the park waited at arm until
// the switch's deadline and the waiting request failed after 10 minutes. A
// switch park that host memory cannot take now is refused `parked_capacity`,
// so the switch stops the victim instead (discrete GPU design §5: a copy that
// does not fit host RAM is stopped rather than parked). An operator's park
// still waits for memory.
#[test]
fn a_switch_park_that_host_memory_cannot_take_is_refused() {
    let lab = Lab::new(|host| {
        host["resource_policy"]["domains"]["unified"]["memory"] = json!("distinct");
    });
    let limits = lab.limits();
    let low = |now: i64| {
        vec![MemoryObservation {
            domain: "unified".into(),
            capacity_bytes: 64 << 30,
            available_bytes: (33 << 30) / 2,
            sampled_at_ms: now,
        }]
    };
    let park_as = |principal: &str, name: &str| {
        let fence = lab.deploy(name, |_| {});
        lab.ready(&fence, 1_000, if name == "s" { 10 } else { 20 });
        let park = lab
            .store
            .accept_park_command(
                &lab.session,
                principal,
                &fence.deployment_id,
                fence.revision,
                &format!("park-{name}"),
                1_100,
                61_100,
            )
            .unwrap();
        let observation = low(1_200);
        lab.store
            .arm_residency(
                &lab.session,
                &park.step_id,
                lab.context(&observation, &limits, 1_200),
            )
            .unwrap()
    };
    assert!(matches!(
        park_as("switch", "s"),
        ResidencyArm::Refused("parked_capacity")
    ));
    assert!(matches!(park_as("operator", "o"), ResidencyArm::Blocked(_)));
    // A unified host follows the same rule (final review I5): its switch
    // park is refused too, so the switch stops the victim instead of waiting.
    let unified = Lab::new(|_| {});
    let fence = unified.deploy("u", |_| {});
    unified.ready(&fence, 1_000, 10);
    let park = unified
        .store
        .accept_park_command(
            &unified.session,
            "switch",
            &fence.deployment_id,
            fence.revision,
            "park-u",
            1_100,
            61_100,
        )
        .unwrap();
    let observation = low(1_200);
    let limits = unified.limits();
    assert!(matches!(
        unified
            .store
            .arm_residency(
                &unified.session,
                &park.step_id,
                unified.context(&observation, &limits, 1_200),
            )
            .unwrap(),
        ResidencyArm::Refused("parked_capacity")
    ));
}

// T26 T27, ADR 0007: found live 2026-09-23 (matrix M33, host-a), a wake
// beside a Ready engine was refused `insufficient resources` although both
// fit the managed limit: the host's availability already excluded what the
// Ready engine held and admission charged its reservation again. The memory
// the host sampled for that engine's own processes (matched by pid, boot id
// and start ticks) is credited as a resident floor; a sample that names other
// processes credits nothing.
#[test]
fn a_wake_beside_a_ready_engine_is_credited_its_resident_memory() {
    let lab = Lab::new(|_| {});
    let v = lab.deploy("v", |_| {});
    let s = lab.deploy("s", |config| {
        for phase in ["cold", "ready", "parking", "wake"] {
            config["resources"][phase]["allocations"][0]["bytes"] = json!("16GiB");
        }
    });
    lab.ready(&v, 1_000, 10);
    let park = lab.park(&v, "park-v", 1_100);
    let context = new_context(lab.arm(&park.step_id, 1_200));
    lab.complete(&context, ResidencyKind::Park, 10, 1_300);
    lab.ready(&s, 1_400, 20);
    let wake = lab.wake(&v, WakeScope::OnDemand, "wake-v", 2_000).unwrap();
    // S holds 16 GiB, V 2 GiB parked, the host 8 GiB of its own: 38 GiB
    // free. Charging S's 16 GiB again leaves 38 - 10 - 16 = 12 GiB, below
    // the 16 GiB reserve.
    let observation = vec![MemoryObservation {
        domain: "unified".into(),
        capacity_bytes: 64 << 30,
        available_bytes: 38 << 30,
        sampled_at_ms: 2_100,
    }];
    let limits = lab.limits();
    let arm = |residents: &[mllm_domain::resources::ProcessResident]| {
        lab.store
            .arm_residency_with_residents(
                &lab.session,
                &wake.step_id,
                lab.context(&observation, &limits, 2_100),
                residents,
            )
            .unwrap()
    };
    let resident = |pid: u32, start_ticks: u64, gib: i64| mllm_domain::resources::ProcessResident {
        pid,
        boot_id: "boot-1".into(),
        start_ticks,
        bytes: gib << 30,
        device_bytes: 0,
        host_bytes: 0,
    };
    // Without a sample, or with one naming other processes, nothing is
    // credited and the wake waits, as before.
    assert!(matches!(arm(&[]), ResidencyArm::Blocked(_)));
    assert!(matches!(
        arm(&[resident(20, 2, 10), resident(21, 2, 5)]),
        ResidencyArm::Blocked(_)
    ));
    // S's own processes hold 15 GiB: 38 - 10 - (16 - 15) = 27 GiB stays free.
    let context = new_context(arm(&[resident(20, 1, 10), resident(21, 1, 5)]));
    assert_eq!(lab.phase(&v.deployment_id), ResourcePhase::Wake);
    lab.complete(&context, ResidencyKind::Restore, 10, 2_200);
    assert_eq!(lab.phase(&v.deployment_id), ResourcePhase::Ready);
}

// ADR 0007: a floor never exceeds the owner's reservation or the memory the
// host has in use, and a parked or transitioning owner is never credited.
#[test]
fn resident_floors_are_bounded_lower_bounds() {
    let lab = Lab::new(|_| {});
    let s = lab.deploy("s", |_| {});
    lab.ready(&s, 1_000, 20);
    let ledger = lab.store.resource_snapshot().unwrap();
    let observation = |available: i64| {
        vec![MemoryObservation {
            domain: "unified".into(),
            capacity_bytes: 64 << 30,
            available_bytes: available << 30,
            sampled_at_ms: 1_100,
        }]
    };
    let resident = |pid: u32, gib: i64| mllm_domain::resources::ProcessResident {
        pid,
        boot_id: "boot-1".into(),
        start_ticks: 1,
        bytes: gib << 30,
        device_bytes: 0,
        host_bytes: 0,
    };
    let unified = || {
        std::collections::BTreeMap::from([(
            "unified".to_string(),
            mllm_config::effective::DomainMemory::Unified,
        )])
    };
    let floors = |available: i64, residents: &[mllm_domain::resources::ProcessResident]| {
        crate::resident_floors::resident_floors(
            &lab.store.conn,
            &ledger,
            "someone-else",
            &observation(available),
            residents,
            &unified(),
        )
        .unwrap()
    };
    // Capped at the 8 GiB Ready reservation.
    let credited = floors(40, &[resident(20, 30)]);
    assert_eq!(credited.len(), 1);
    assert_eq!(credited[0].bytes, 8 << 30);
    assert_eq!(credited[0].sampled_at_ms, 1_100);
    // More than the host has in use: none at all.
    assert!(floors(60, &[resident(20, 6)]).is_empty());
    // The candidate itself is never credited.
    let own = crate::resident_floors::resident_floors(
        &lab.store.conn,
        &ledger,
        &s.deployment_id,
        &observation(40),
        &[resident(20, 6)],
        &unified(),
    )
    .unwrap();
    assert!(own.is_empty());
    // Parking: a run in flight, so no credit.
    lab.park(&s, "park-s", 1_200);
    assert!(floors(40, &[resident(20, 6)]).is_empty());
}

// T26 / ADR 0007 (M33 regression on discrete hosts): a Ready engine with two
// allocations is credited on both domains, not skipped: its GPU bytes against
// the device domain and its anonymous pages against the system domain.
#[test]
fn a_two_domain_owner_is_credited_per_domain() {
    use mllm_config::effective::DomainMemory;
    use mllm_domain::resources::{Allocation, ProcessResident};
    use std::collections::BTreeMap;
    let lab = Lab::new(|_| {});
    let s = lab.deploy("s", |_| {});
    lab.ready(&s, 1_000, 20);
    let mut ledger = lab.store.resource_snapshot().unwrap();
    let owner = crate::instances::instance_owner_id(&s.deployment_id, 0);
    ledger.owners.get_mut(&owner).unwrap().allocations = vec![
        Allocation {
            domain: "gpu0".into(),
            bytes: 10 << 30,
            host_kv_bytes: 0,
        },
        Allocation {
            domain: "system".into(),
            bytes: 4 << 30,
            host_kv_bytes: 0,
        },
    ];
    let observation = |domain: &str| MemoryObservation {
        domain: domain.into(),
        capacity_bytes: 64 << 30,
        available_bytes: 32 << 30,
        sampled_at_ms: 1_100,
    };
    let observations = vec![observation("gpu0"), observation("system")];
    let residents = vec![ProcessResident {
        pid: 20,
        boot_id: "boot-1".into(),
        start_ticks: 1,
        bytes: (9 << 30) + (3 << 30),
        device_bytes: 9 << 30,
        host_bytes: 3 << 30,
    }];
    let kinds = BTreeMap::from([
        ("gpu0".to_string(), DomainMemory::Device),
        ("system".to_string(), DomainMemory::Distinct),
    ]);
    let floors = |observations: &[MemoryObservation],
                  residents: &[ProcessResident],
                  kinds: &BTreeMap<String, DomainMemory>| {
        crate::resident_floors::resident_floors(
            &lab.store.conn,
            &ledger,
            "candidate",
            observations,
            residents,
            kinds,
        )
        .unwrap()
        .into_iter()
        .map(|f| {
            assert_eq!(f.owner, owner);
            (f.domain, f.bytes)
        })
        .collect::<BTreeMap<_, _>>()
    };
    let by_domain = floors(&observations, &residents, &kinds);
    assert_eq!(by_domain["gpu0"], 9 << 30);
    assert_eq!(by_domain["system"], 3 << 30);
    // Each floor is capped at its own allocation.
    let mut large = residents.clone();
    large[0].device_bytes = 12 << 30;
    large[0].host_bytes = 6 << 30;
    large[0].bytes = 18 << 30;
    let capped = floors(&observations, &large, &kinds);
    assert_eq!(capped["gpu0"], 10 << 30);
    assert_eq!(capped["system"], 4 << 30);
    // A domain whose kind the policy does not name is credited nothing (fail
    // closed); the other domain keeps its floor.
    let partial = BTreeMap::from([("gpu0".to_string(), DomainMemory::Device)]);
    let only_gpu = floors(&observations, &residents, &partial);
    assert_eq!(only_gpu.keys().collect::<Vec<_>>(), ["gpu0"]);
    // So is a domain without an observation.
    let only_gpu = floors(&observations[..1], &residents, &kinds);
    assert_eq!(only_gpu.keys().collect::<Vec<_>>(), ["gpu0"]);
}

// T16 T26: found live on the 16 GB discrete-GPU laptop host. A parked vLLM
// keeps a residue on the card (its CUDA context) and, when host_backed, its
// weights copy in host RAM; both are in use and out of the host's free
// memory, and charging the parked reservation again made every start beside
// a parked model stop it instead. A parked owner whose park completed is
// credited what its own processes hold, capped at its parked reservation. One
// rule on every host shape (final review I5, owner rule 2026-09-26): on a
// unified domain it is credited the figure of the one pool.
#[test]
fn a_parked_owner_is_credited_on_every_domain_kind() {
    use mllm_config::effective::DomainMemory;
    use mllm_domain::resources::{Allocation, ProcessResident};
    use std::collections::BTreeMap;
    let lab = Lab::new(|_| {});
    let s = lab.deploy("s", |_| {});
    lab.ready(&s, 1_000, 20);
    let park = lab.park(&s, "park-s", 1_100);
    let context = new_context(lab.arm(&park.step_id, 1_200));
    lab.complete(&context, ResidencyKind::Park, 20, 1_300);
    assert_eq!(lab.instance(&s.deployment_id).0, "parked");
    let mut ledger = lab.store.resource_snapshot().unwrap();
    let owner = crate::instances::instance_owner_id(&s.deployment_id, 0);
    ledger.owners.get_mut(&owner).unwrap().allocations = vec![
        Allocation {
            domain: "gpu0".into(),
            bytes: 1 << 30,
            host_kv_bytes: 0,
        },
        Allocation {
            domain: "system".into(),
            bytes: 12 << 30,
            host_kv_bytes: 0,
        },
    ];
    let observation = |domain: &str| MemoryObservation {
        domain: domain.into(),
        capacity_bytes: 64 << 30,
        available_bytes: 32 << 30,
        sampled_at_ms: 1_400,
    };
    let observations = vec![observation("gpu0"), observation("system")];
    let residents = vec![ProcessResident {
        pid: 21,
        boot_id: "boot-1".into(),
        start_ticks: 1,
        bytes: (1600 << 20) + (10 << 30),
        device_bytes: 1600 << 20,
        host_bytes: 10 << 30,
    }];
    let discrete = BTreeMap::from([
        ("gpu0".to_string(), DomainMemory::Device),
        ("system".to_string(), DomainMemory::Distinct),
    ]);
    let floors = |kinds: &BTreeMap<String, DomainMemory>| {
        crate::resident_floors::resident_floors(
            &lab.store.conn,
            &ledger,
            "candidate",
            &observations,
            &residents,
            kinds,
        )
        .unwrap()
        .into_iter()
        .map(|f| (f.domain, f.bytes))
        .collect::<BTreeMap<_, _>>()
    };
    let credited = floors(&discrete);
    // Capped at the 1 GiB parked reservation on the card; the sampled 10 GiB
    // of host pages (the pinned copy included) on the system domain.
    assert_eq!(credited["gpu0"], 1 << 30);
    assert_eq!(credited["system"], 10 << 30);
    // The same owner on unified domains is credited by the same rule, from
    // the pool's figure (device plus host pages), capped at each allocation.
    let unified = BTreeMap::from([
        ("gpu0".to_string(), DomainMemory::Unified),
        ("system".to_string(), DomainMemory::Unified),
    ]);
    let pooled = floors(&unified);
    assert_eq!(pooled["gpu0"], 1 << 30);
    assert_eq!(pooled["system"], (1600 << 20) + (10 << 30));
    // A wake in flight: no credit.
    lab.wake(&s, WakeScope::OnDemand, "wake-s", 1_500).unwrap();
    assert!(floors(&discrete).is_empty());
}

impl Lab {
    /// Record `reason` for instance 0's current incarnation, as the host-loss
    /// or engine-exit path would while a park or restore is in flight.
    fn close_for(&self, fence: &DeploymentFence, reason: crate::switch_state::ClosureReason) {
        let generation = self.instance(&fence.deployment_id).2;
        crate::switch_state::record_closure(
            &self.store.conn,
            &fence.deployment_id,
            0,
            generation,
            reason,
        )
        .unwrap();
    }
}

// ADR 0015 amendment (closure reasons), SPEC §13.2: a park refused before any
// effect reopens the gate it closed, but not one an engine exit closed while
// the park was in flight.
// T20 T34
#[test]
fn a_refused_park_leaves_a_gate_another_reason_closed() {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    lab.ready(&a, 1_000, 10);
    let park = lab.park(&a, "p", 1_100);
    let _ = new_context(lab.arm(&park.step_id, 1_200));
    lab.close_for(&a, crate::switch_state::ClosureReason::EngineExit);
    lab.store
        .refuse_residency(&lab.session, &park.step_id, "host refused: unchanged")
        .unwrap();
    assert!(
        !lab.instance(&a.deployment_id).1,
        "the engine-exit closure keeps the gate closed"
    );
}

// SPEC §13.2: a park that closes at its deadline before arming reopens only
// when no closure reason remains; a host-session loss recorded meanwhile holds.
// T20 T32
#[test]
fn a_cancelled_park_leaves_a_gate_host_loss_closed() {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    lab.ready(&a, 1_000, 10);
    let generation = lab.instance(&a.deployment_id).2;
    lab.store
        .conn
        .execute(
            "INSERT INTO request_leases(id,deployment_id,revision,generation,session_id,disposition,instance_index) VALUES('lease',?1,?2,?3,?4,'inflight',0)",
            params![a.deployment_id, a.revision, generation, lab.session.id()],
        )
        .unwrap();
    let park = lab.park(&a, "p", 1_100);
    assert_eq!(lab.arm(&park.step_id, 1_200), ResidencyArm::Draining);
    lab.close_for(&a, crate::switch_state::ClosureReason::HostSession);
    assert!(lab
        .store
        .next_residency_work(&lab.session, 61_200)
        .unwrap()
        .is_empty());
    assert!(
        !lab.instance(&a.deployment_id).1,
        "the host-session closure keeps the gate closed"
    );
}

// SPEC §6.1, §13.2: a completed restore makes the model usable, but dispatch
// reopens only when no closure reason remains for the incarnation.
// T16 T32
#[test]
fn a_completed_restore_leaves_a_gate_another_reason_closed() {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    lab.ready(&a, 1_000, 10);
    let park = lab.park(&a, "p", 1_100);
    let context = new_context(lab.arm(&park.step_id, 1_200));
    lab.complete(&context, ResidencyKind::Park, 10, 1_300);
    let wake = lab.wake(&a, WakeScope::OnDemand, "w", 2_000).unwrap();
    let context = new_context(lab.arm(&wake.step_id, 2_100));
    lab.close_for(&a, crate::switch_state::ClosureReason::HostSession);
    lab.complete(&context, ResidencyKind::Restore, 10, 2_200);
    let (state, open, _) = lab.instance(&a.deployment_id);
    assert_eq!((state.as_str(), open), ("ready", false));
}

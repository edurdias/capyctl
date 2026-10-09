//! ADR 0014 amendments A13 and A19: the parked charge measured per revision,
//! and the bound on one launch's parked-charge growth. CPU only; nothing here
//! qualifies an engine.
use super::super::park::{
    IdleAction, IdlePolicy, ResidencyArm, ResidencyKind, ResidencyReceipt, WakeScope,
};
use super::*;
use crate::Store;
use capyctl_config::effective::{resolve_effective, PARKED_RESIDUAL_PLACEHOLDER_BYTES};
use capyctl_domain::completion::{
    CleanupEvidence, CompletionEvidence, EffectObservation, OwnedLaunchReceipt, ProcessIdentity,
    StepExecutionContext,
};
use capyctl_domain::resources::{MemoryLimit, MemoryObservation, ProcessResident};
use capyctl_scheduler::residency::AdmissionContext;
use serde_json::{json, Value};

const GIB: i64 = 1 << 30;

fn fixture() -> (Value, Value) {
    let value: Value = serde_json::from_str(include_str!(
        "../../../capyctl-config/tests/fixtures/f2-deployment.json"
    ))
    .unwrap();
    (value["deployment"].clone(), value["host"].clone())
}

fn group(base: u32) -> Vec<ProcessIdentity> {
    ["api", "worker-0"]
        .iter()
        .zip(base..)
        .map(|(role, pid)| ProcessIdentity {
            role: (*role).into(),
            pid,
            boot_id: "boot-1".into(),
            start_ticks: 1,
        })
        .collect()
}

/// What the host sampled for the group from `base`: `gib` (in quarters of a
/// GiB) per process.
fn residents(base: u32, quarters: [i64; 2]) -> Vec<ProcessResident> {
    quarters
        .iter()
        .zip(base..)
        .map(|(q, pid)| ProcessResident {
            pid,
            boot_id: "boot-1".into(),
            start_ticks: 1,
            bytes: q * GIB / 4,
            device_bytes: q * GIB / 4,
            host_bytes: 0,
        })
        .collect()
}

fn observed(now: i64) -> Vec<MemoryObservation> {
    vec![MemoryObservation {
        domain: "unified".into(),
        capacity_bytes: 64 * GIB,
        available_bytes: 50 * GIB,
        sampled_at_ms: now,
    }]
}

/// A deployment whose phases CapyCTL derives from an 8 GiB request.
fn derived(config: &mut Value) {
    config.as_object_mut().unwrap().remove("resources");
    config["engine_config"]["memory"] = json!({"request": "8GiB", "kv_cache": "4GiB"});
}

struct Lab {
    store: Store,
    session: CoordinatorSession,
    host: Value,
}

impl Lab {
    fn new() -> Self {
        let (mut config, host) = fixture();
        derived(&mut config);
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
                reserve_absorbs_unmanaged: d.memory
                    == capyctl_config::effective::DomainMemory::Device,
                host_kv_bytes: d.host_kv_limit,
                parked_bytes: d.parked_limit,
            })
            .collect()
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

    fn ready(&self, fence: &DeploymentFence, now: i64, base: u32) {
        let accepted = self
            .store
            .accept_start(&self.session, fence, now, now + 100_000)
            .unwrap();
        let (observations, limits) = (observed(now), self.limits());
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

    fn arm(&self, step: &str, now: i64) -> StepExecutionContext {
        let (observations, limits) = (observed(now), self.limits());
        match self
            .store
            .arm_residency(
                &self.session,
                step,
                self.context(&observations, &limits, now),
            )
            .unwrap()
        {
            ResidencyArm::New(context) => *context,
            other => panic!("expected a fresh arm, got {other:?}"),
        }
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
                    kernel_builds: Vec::new(),
                },
                now,
            )
            .unwrap();
    }

    /// Park `fence` (instance 0, group from `base`) and return the park step.
    fn parked(&self, fence: &DeploymentFence, key: &str, base: u32, now: i64) -> String {
        let receipt: ResidencyReceipt = self
            .store
            .accept_park_command(
                &self.session,
                "operator",
                &fence.deployment_id,
                fence.revision,
                key,
                now,
                now + 60_000,
            )
            .unwrap();
        let context = self.arm(&receipt.step_id, now + 10);
        self.complete(&context, ResidencyKind::Park, base, now + 20);
        receipt.step_id
    }

    fn woken(&self, fence: &DeploymentFence, key: &str, base: u32, now: i64) {
        let receipt = self
            .store
            .accept_restore_command(
                &self.session,
                "router",
                &fence.deployment_id,
                WakeScope::OnDemand,
                fence.revision,
                key,
                now,
                now + 60_000,
            )
            .unwrap()
            .unwrap();
        let context = self.arm(&receipt.step_id, now + 10);
        self.complete(&context, ResidencyKind::Restore, base, now + 20);
    }

    fn record(&self, step: &str, sampled_at: i64, residents: &[ProcessResident]) -> bool {
        self.store
            .record_parked_residue(
                &self.session,
                step,
                &observed(sampled_at),
                residents,
                1_320,
                sampled_at,
            )
            .unwrap()
    }

    /// The bytes the ledger charges `id` on the unified pool, and its phase.
    fn charge(&self, id: &str) -> (i64, ResourcePhase) {
        let owner = &self.store.resource_snapshot().unwrap().owners[id];
        (owner.allocations[0].bytes, owner.phase)
    }

    fn status(&self, id: &str) -> Value {
        let snapshot = self.store.snapshot().unwrap();
        let d = snapshot.deployments.iter().find(|d| d.id == id).unwrap();
        serde_json::to_value(&d.parked).unwrap()
    }
}

// The parked engine's measured residue replaces the placeholder: the parked
// owner is charged it at once, a later park of the revision is charged it
// from the start, and status shows it.
#[test]
fn a_measured_parked_residue_replaces_the_placeholder_charge() {
    let lab = Lab::new();
    let a = lab.deploy("a", derived);
    lab.ready(&a, 1_000, 10);
    let step = lab.parked(&a, "park-1", 10, 1_300);
    assert_eq!(
        lab.charge(&a.deployment_id),
        (PARKED_RESIDUAL_PLACEHOLDER_BYTES, ResourcePhase::Parked)
    );
    assert_eq!(lab.status(&a.deployment_id)["provenance"], "placeholder");
    // 2.25 GiB + 1.25 GiB held by the parked group, above the 2 GiB placeholder.
    assert!(lab.record(&step, 1_400, &residents(10, [9, 5])));
    let measured = 14 * GIB / 4;
    assert_eq!(
        lab.charge(&a.deployment_id),
        (measured, ResourcePhase::Parked)
    );
    let status = lab.status(&a.deployment_id);
    assert_eq!(status["provenance"], "measured", "{status}");
    assert_eq!(status["bytes"], measured);
    assert_eq!(status["measured"][0]["bytes"], measured);
    assert_eq!(status["measured"][0]["domain"], "unified");

    // The wake and the next park move through the measured charge.
    lab.woken(&a, "wake-1", 10, 2_000);
    assert_eq!(lab.charge(&a.deployment_id).1, ResourcePhase::Ready);
    lab.parked(&a, "park-2", 10, 3_000);
    assert_eq!(
        lab.charge(&a.deployment_id),
        (measured, ResourcePhase::Parked)
    );
}

// The largest residue measured for the revision is kept, and a residue below
// the placeholder never lowers the charge (the host re-checks co-residence
// with the revision's own parked phase, so the controller never charges less).
#[test]
fn the_parked_charge_keeps_the_largest_residue_and_never_drops_below_the_placeholder() {
    let lab = Lab::new();
    let a = lab.deploy("a", derived);
    lab.ready(&a, 1_000, 10);
    let step = lab.parked(&a, "park-1", 10, 1_300);
    assert!(lab.record(&step, 1_400, &residents(10, [2, 1])));
    assert_eq!(
        lab.charge(&a.deployment_id).0,
        PARKED_RESIDUAL_PLACEHOLDER_BYTES
    );
    assert!(lab.record(&step, 1_500, &residents(10, [10, 2])));
    assert!(lab.record(&step, 1_600, &residents(10, [9, 1])));
    assert_eq!(lab.charge(&a.deployment_id).0, 3 * GIB);
    assert_eq!(
        lab.status(&a.deployment_id)["measured"][0]["bytes"],
        3 * GIB
    );
}

// Nothing is recorded from a sample taken before the park completed, from a
// sample that names none of the parked processes, or for a deployment that
// declares its own resources.
#[test]
fn a_stale_sample_an_unmatched_group_or_declared_resources_record_nothing() {
    let lab = Lab::new();
    let a = lab.deploy("a", derived);
    lab.ready(&a, 1_000, 10);
    let step = lab.parked(&a, "park-1", 10, 1_300);
    assert!(!lab.record(&step, 1_310, &residents(10, [40, 4])));
    assert!(!lab.record(&step, 1_400, &residents(90, [9, 5])));
    assert_eq!(
        lab.charge(&a.deployment_id).0,
        PARKED_RESIDUAL_PLACEHOLDER_BYTES
    );

    let b = lab.deploy("b", |_| {});
    lab.ready(&b, 2_000, 20);
    let step = lab.parked(&b, "park-b", 20, 2_300);
    assert!(!lab.record(&step, 2_400, &residents(20, [9, 5])));
    assert!(lab.status(&b.deployment_id).is_null());
}

// --- ADR 0014 amendment A19: the parked growth bound ---------------------------

impl Lab {
    /// A lab whose host states `edit` in its published policy.
    fn with_host(edit: impl FnOnce(&mut Value)) -> Self {
        let (mut config, mut host) = fixture();
        edit(&mut host);
        derived(&mut config);
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

    /// One park of instance 0 measured at `quarters` (quarters of a GiB per
    /// process), then a wake: a park and wake cycle of the same launch.
    fn cycle(&self, fence: &DeploymentFence, n: u32, quarters: [i64; 2], now: i64) {
        let step = self.parked(fence, &format!("park-{n}"), 10, now);
        assert!(self.record(&step, now + 100, &residents(10, quarters)));
        self.woken(fence, &format!("wake-{n}"), 10, now + 500);
    }

    fn operation_kind(&self, id: &str) -> String {
        self.store
            .conn
            .query_row("SELECT kind FROM operations WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
    }

    fn stored_policy(&self) -> String {
        self.store
            .conn
            .query_row(
                "SELECT policy_json FROM host_resource_policies WHERE host_id='lab'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn park_command(&self, fence: &DeploymentFence, key: &str, now: i64) -> ResidencyReceipt {
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

    /// Found live 2026-10-09: the stop the bound turned a park into runs as
    /// any stop does. The cleanup worker discovers and arms it; the instance
    /// keeps its Ready charge until the stop's gone evidence, which releases
    /// it and leaves the instance stopped.
    fn stop_runs(&self, fence: &DeploymentFence, operation: &str, now: i64) {
        let next = self
            .store
            .next_ordinary_cleanup(&self.session)
            .unwrap()
            .expect("the cleanup worker discovers the stop");
        assert_eq!(next.operation_id, operation);
        let (_, context) = self
            .store
            .arm_ordinary_cleanup_with_context(&self.session, &next.step_id, now)
            .unwrap();
        let context = context.expect("a fresh arm");
        assert_eq!(self.charge(&fence.deployment_id).1, ResourcePhase::Ready);
        let ttl = self
            .store
            .resource_policy("lab")
            .unwrap()
            .unwrap()
            .controls
            .observation_ttl_ms;
        self.store
            .complete_cleanup(
                &self.session,
                &next.step_id,
                &CleanupEvidence {
                    binding_id: context.binding_id,
                    incarnation: context.incarnation,
                    identities: context.identities,
                    observed_at_ms: now + 10,
                    receipt: "gone".into(),
                },
                now + 10,
                ttl,
            )
            .unwrap();
        assert!(!self
            .store
            .resource_snapshot()
            .unwrap()
            .owners
            .contains_key(&fence.deployment_id));
        let (state, observed): (String, String) = self
            .store
            .conn
            .query_row(
                "SELECT o.state,i.observed_state FROM operations o JOIN deployment_instances i
                   ON i.deployment_id=o.deployment_id AND i.instance_index=0 WHERE o.id=?1",
                [operation],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (state.as_str(), observed.as_str()),
            ("succeeded", "stopped")
        );
    }
}

/// The catalog's shape (GB10, vLLM 0.30.0): one launch parked and woken
/// twice, its parked charge 3.5 GiB at the first park and 7.5 GiB at the
/// second, growth (4 GiB) past the default bound (the first charge).
fn outgrown_launch(lab: &Lab) -> DeploymentFence {
    let a = lab.deploy("a", derived);
    lab.ready(&a, 1_000, 10);
    lab.cycle(&a, 1, [9, 5], 1_300);
    lab.cycle(&a, 2, [20, 10], 3_000);
    a
}

// T16 (owner decision 2026-10-07, ADR 0014 amendment A19): once a launch's
// measured parked charge grew past its host's `parked_growth_limit` (default:
// the first charge again), `park deployment` stops it instead. The stop
// releases nothing yet (the instance keeps its Ready charge until the stop's
// own cleanup evidence), and status says why.
#[test]
fn a_park_after_the_parked_charge_outgrew_its_first_park_is_a_stop() {
    let lab = Lab::new();
    let a = outgrown_launch(&lab);
    let status = lab.status(&a.deployment_id);
    let growth = &status["growth"][0];
    assert_eq!(growth["first_bytes"], 14 * GIB / 4, "{status}");
    assert_eq!(growth["last_bytes"], 30 * GIB / 4, "{status}");
    assert_eq!(growth["parks"], 2);
    assert_eq!(growth["limit_bytes"], 14 * GIB / 4);
    assert_eq!(growth["state"], "past_limit");

    let receipt = lab.park_command(&a, "park-3", 5_000);
    assert_eq!(
        lab.operation_kind(&receipt.operation_id),
        "ordinary_cleanup"
    );
    assert_eq!(
        lab.store.residency_step_state(&receipt.step_id).unwrap(),
        None,
        "no park step was accepted"
    );
    assert_eq!(lab.charge(&a.deployment_id).1, ResourcePhase::Ready);
    let status = lab.status(&a.deployment_id);
    assert_eq!(status["growth"][0]["state"], "stopped", "{status}");
    assert_eq!(
        status["growth"][0]["stop_operation_id"],
        json!(receipt.operation_id)
    );
    let evidence: String = lab
        .store
        .conn
        .query_row(
            "SELECT evidence FROM journal_entries WHERE operation_id=?1 AND state='park_growth_stop'",
            [&receipt.operation_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(evidence.contains("parked_growth"), "{evidence}");
    assert!(evidence.contains("grew from 3.5 GiB"), "{evidence}");
    // An exact retry of the command replays the same stop.
    assert_eq!(lab.park_command(&a, "park-3", 5_000), receipt);
    // Found live 2026-10-09: the park's receipt names the stop too, and the
    // stop still runs and releases only on its gone evidence; a retry after
    // it settled still replays it.
    lab.stop_runs(&a, &receipt.operation_id, 5_100);
    assert_eq!(lab.park_command(&a, "park-3", 5_000), receipt);
}

// T16 (ADR 0014 amendment A19): the park's own receipt is the only other
// receipt the stop it became may have. The same receipt under another
// principal is still corruption, never authority, so the stop is not driven.
#[test]
fn a_growth_stop_refuses_a_park_receipt_it_did_not_answer() {
    let lab = Lab::new();
    let a = outgrown_launch(&lab);
    lab.park_command(&a, "park-3", 5_000);
    // Corruption injection, never acceptance authority.
    lab.store
        .conn
        .execute(
            "INSERT INTO command_receipts SELECT 'other',command_scope,idempotency_key,request_hash,operation_id,response_json
               FROM command_receipts WHERE command_scope LIKE '%#park' AND idempotency_key='park-3'",
            [],
        )
        .unwrap();
    assert!(matches!(
        lab.store.next_ordinary_cleanup(&lab.session),
        Err(LifecycleError::CorruptStoredData)
    ));
}

// T16 (ADR 0014 amendment A19): growth within the bound parks as before.
#[test]
fn a_park_within_the_growth_limit_parks_as_before() {
    let lab = Lab::new();
    let a = lab.deploy("a", derived);
    lab.ready(&a, 1_000, 10);
    lab.cycle(&a, 1, [9, 5], 1_300);
    // 4 GiB: 0.5 GiB of growth past 3.5 GiB.
    lab.cycle(&a, 2, [10, 6], 3_000);
    assert_eq!(
        lab.status(&a.deployment_id)["growth"][0]["state"],
        "within_limit"
    );
    let receipt = lab.park_command(&a, "park-3", 5_000);
    assert_eq!(lab.operation_kind(&receipt.operation_id), "park");
    let context = lab.arm(&receipt.step_id, 5_010);
    lab.complete(&context, ResidencyKind::Park, 10, 5_020);
    assert_eq!(lab.charge(&a.deployment_id).1, ResourcePhase::Parked);
}

// T16 (ADR 0014 amendment A19): the idle policy's park of an outgrown launch
// is a stop with the reason `ready_idle_parked_growth`; within the bound it
// parks.
#[test]
fn the_idle_policy_stops_an_outgrown_launch_instead_of_parking_it() {
    let policy = IdlePolicy {
        ready_idle_ms: Some(1_000),
        parked_idle_ms: None,
    };
    let quiet = |_: &str, _: i64| None;
    let lab = Lab::new();
    let a = outgrown_launch(&lab);
    let actions = lab
        .store
        .apply_idle_policy(&lab.session, 10_000, policy, &quiet, 0)
        .unwrap();
    let [IdleAction::Stopped {
        reason,
        operation_id,
        ..
    }] = actions.as_slice()
    else {
        panic!("{actions:?}");
    };
    assert_eq!(*reason, "ready_idle_parked_growth");
    assert_eq!(lab.operation_kind(operation_id), "ordinary_cleanup");
    assert_eq!(
        lab.status(&a.deployment_id)["growth"][0]["state"],
        "stopped"
    );
    lab.stop_runs(&a, operation_id, 10_100);

    let lab = Lab::new();
    let b = lab.deploy("b", derived);
    lab.ready(&b, 1_000, 10);
    lab.cycle(&b, 1, [9, 5], 1_300);
    lab.cycle(&b, 2, [10, 6], 3_000);
    let actions = lab
        .store
        .apply_idle_policy(&lab.session, 10_000, policy, &quiet, 0)
        .unwrap();
    assert!(
        matches!(actions.as_slice(), [IdleAction::Parked { .. }]),
        "{actions:?}"
    );
}

// T16 T03 (ADR 0014 amendment A19): the host's `parked_growth_limit` sets the
// bound: `off` never stops, a percentage or a size of growth is measured from
// the first charge. A policy that states none stores no such field, so its
// stored identity is the one it had before the setting existed.
#[test]
fn the_hosts_parked_growth_limit_sets_the_bound() {
    assert!(!Lab::new().stored_policy().contains("parked_growth_limit"));
    let auto = Lab::with_host(|host| {
        host["resource_policy"]["parked_growth_limit"] = json!("auto");
    });
    assert_eq!(auto.stored_policy(), Lab::new().stored_policy());
    // The outgrown launch grew 4 GiB from 3.5 GiB (114 %).
    for (limit, stops) in [
        ("off", false),
        ("200%", false),
        ("100%", true),
        ("5GiB", false),
        ("3GiB", true),
    ] {
        let lab = Lab::with_host(|host| {
            host["resource_policy"]["parked_growth_limit"] = json!(limit);
        });
        assert!(
            lab.stored_policy().contains("parked_growth_limit"),
            "{limit}"
        );
        let a = outgrown_launch(&lab);
        let receipt = lab.park_command(&a, "park-3", 5_000);
        let kind = lab.operation_kind(&receipt.operation_id);
        assert_eq!(kind == "ordinary_cleanup", stops, "{limit}: {kind}");
        let growth = &lab.status(&a.deployment_id)["growth"][0];
        assert_eq!(growth["limit_bytes"].is_null(), limit == "off", "{limit}");
    }
}

// T16 T32 (ADR 0014 amendment A19): a switch releases an outgrown victim by
// a stop even when the plan would park it, and records why.
#[test]
fn a_switch_stops_an_outgrown_victim_instead_of_parking_it() {
    let lab = Lab::new();
    let a = outgrown_launch(&lab);
    let generation: i64 = lab
        .store
        .conn
        .query_row(
            "SELECT generation FROM deployment_instances WHERE deployment_id=?1 AND instance_index=0",
            [&a.deployment_id],
            |r| r.get(0),
        )
        .unwrap();
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
            5_000,
            true,
        )
        .unwrap();
    assert!(!release.parked);
    assert_eq!(
        lab.operation_kind(&release.operation_id),
        "ordinary_cleanup"
    );
    let evidence: String = lab
        .store
        .conn
        .query_row(
            "SELECT evidence FROM journal_entries WHERE operation_id=?1 AND state='switch_stop'",
            [&release.operation_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(evidence.contains("parked_growth"), "{evidence}");
    assert_eq!(
        lab.status(&a.deployment_id)["growth"][0]["state"],
        "stopped"
    );
    lab.stop_runs(&a, &release.operation_id, 5_100);
}

//! ADR 0014 amendment A13: the parked charge measured per revision. CPU
//! only; nothing here qualifies an engine.
use super::super::park::{ResidencyArm, ResidencyKind, ResidencyReceipt, WakeScope};
use super::*;
use crate::Store;
use capyctl_config::effective::{resolve_effective, PARKED_RESIDUAL_PLACEHOLDER_BYTES};
use capyctl_domain::completion::{
    CompletionEvidence, EffectObservation, OwnedLaunchReceipt, ProcessIdentity,
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

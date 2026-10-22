//! ADR 0033 store tests: the per-launch LMCache plan, its ports and charge,
//! the host's report, the engine-start fallback, restart-only parking, the
//! server's exit and a role start's retirement. CPU only; nothing here
//! qualifies an engine or LMCache.
use super::*;
use crate::ordinary_lifecycle::engine_exit::{EngineExit, ExitSource};
use crate::ordinary_lifecycle::park::{ResidencyArm, ResidencyKind, WakeScope};
use crate::Store;
use capyctl_config::effective::resolve_effective;
use capyctl_domain::completion::{CleanupEvidence, EffectObservation, Milestone};
use capyctl_domain::resources::{MemoryLimit, MemoryObservation};
use serde_json::{json, Value};

const GIB: i64 = 1 << 30;
const MIB: i64 = 1 << 20;
const DIGEST: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";

fn fixture() -> (Value, Value) {
    let value: Value = serde_json::from_str(include_str!(
        "../../../capyctl-config/tests/fixtures/f2-deployment.json"
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

fn engine(base: u32) -> Vec<ProcessIdentity> {
    vec![identity("api", base), identity("worker-0", base + 1)]
}

fn server(base: u32) -> Vec<ProcessIdentity> {
    vec![identity("lmcache", base + 50)]
}

fn observed(now: i64) -> Vec<MemoryObservation> {
    vec![MemoryObservation {
        domain: "unified".into(),
        capacity_bytes: 64 << 30,
        available_bytes: 60 << 30,
        sampled_at_ms: now,
    }]
}

/// A host whose vLLM profile is 0.32.0 with LMCache 0.5.6 registered, so a
/// `restart_only` deployment plans LMCache on.
struct Lab {
    store: Store,
    session: CoordinatorSession,
    host: Value,
}

impl Lab {
    fn new(edit_host: impl FnOnce(&mut Value)) -> Self {
        let (config, mut host) = fixture();
        let profile = &mut host["runtime_profiles"]["local"];
        profile["build_fingerprint"] = json!("0.32.0");
        profile["lmcache"] = json!({"version": "0.5.6", "digest": DIGEST});
        host["resource_policy"]["domains"]["unified"]["host_kv_limit"] = json!("8GiB");
        edit_host(&mut host);
        let effective = resolve_effective(&on(config), &host).unwrap();
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
        let (config, _) = fixture();
        let mut config = on(config);
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
                reserve_absorbs_unmanaged: false,
                host_kv_bytes: d.host_kv_limit,
                parked_bytes: d.parked_limit,
            })
            .collect()
    }

    fn arm_start(&self, step: &str, now: i64) -> Result<StepExecutionContext, LifecycleError> {
        let observations = observed(now);
        let limits = self.limits();
        let policy = self.store.resource_policy("lab").unwrap().unwrap();
        self.store.arm_step(
            &self.session,
            step,
            AdmissionContext::new(
                &observations,
                &limits,
                now,
                policy.controls.observation_ttl_ms,
                policy.controls.max_parked as usize,
            ),
        )?;
        self.store.initialize_execution(&self.session, step)
    }

    fn start(&self, fence: &DeploymentFence, now: i64) -> (Start, StepExecutionContext) {
        let accepted = self
            .store
            .accept_start(&self.session, fence, now, now + 100_000)
            .unwrap();
        let context = self.arm_start(&accepted.step_id, now).unwrap();
        (accepted, context)
    }

    /// Bring a started launch Ready with the engine from `base` and, when
    /// LMCache ran, its server, after its host reported `outcome`.
    fn ready(
        &self,
        context: &StepExecutionContext,
        base: u32,
        outcome: Option<LmcacheOutcome>,
        now: i64,
    ) -> Vec<ProcessIdentity> {
        let mut identities = engine(base);
        if outcome == Some(LmcacheOutcome::On) {
            self.store
                .record_launch_identity(
                    &self.session,
                    &fence_of(context),
                    &context.binding_id,
                    &server(base)[0],
                )
                .unwrap();
            identities.extend(server(base));
        }
        if let Some(outcome) = &outcome {
            self.store
                .record_launch_lmcache(&self.session, &context.binding_id, outcome, now)
                .unwrap();
        }
        self.store
            .record_owned_launch(
                &self.session,
                &context.token.step_id,
                &OwnedLaunchReceipt {
                    binding_id: context.binding_id.clone(),
                    incarnation: context.incarnation.clone(),
                    identities: identities.clone(),
                    observed_at_ms: now,
                    receipt: "fake ready".into(),
                },
                now,
            )
            .unwrap();
        self.store
            .complete_step(
                &self.session,
                &context.token.step_id,
                &CompletionEvidence {
                    token: context.token.clone(),
                    identities: identities.clone(),
                    observed_at_ms: now,
                    control_receipt: Some("fake ready".into()),
                    milestones: ResidencyKind::Restore.facts().to_vec(),
                },
                now,
                2_000,
            )
            .unwrap();
        identities
    }

    fn arm_residency(&self, step: &str, now: i64) -> StepExecutionContext {
        let observations = observed(now);
        let limits = self.limits();
        let policy = self.store.resource_policy("lab").unwrap().unwrap();
        match self
            .store
            .arm_residency(
                &self.session,
                step,
                AdmissionContext::new(
                    &observations,
                    &limits,
                    now,
                    policy.controls.observation_ttl_ms,
                    policy.controls.max_parked as usize,
                ),
            )
            .unwrap()
        {
            ResidencyArm::New(context) => *context,
            other => panic!("expected a fresh arm, got {other:?}"),
        }
    }

    fn complete_residency(
        &self,
        context: &StepExecutionContext,
        identities: Vec<ProcessIdentity>,
        facts: Vec<Milestone>,
        now: i64,
    ) -> Result<(), LifecycleError> {
        self.store.complete_residency(
            &self.session,
            &context.token.step_id,
            &EffectObservation {
                token: context.token.clone(),
                binding_id: context.binding_id.clone(),
                incarnation: context.incarnation.clone(),
                identities,
                observed_at_ms: now,
                receipt: "fake host".into(),
                facts,
                kernel_builds: Vec::new(),
            },
            now,
        )
    }

    fn footprint(&self, fence: &DeploymentFence) -> PhaseFootprint {
        let owner = crate::instances::instance_owner_id(&fence.deployment_id, 0);
        self.store.resource_snapshot().unwrap().owners[&owner].clone()
    }

    fn row<T: rusqlite::types::FromSql>(&self, sql: &str, id: &str) -> T {
        self.store.conn.query_row(sql, [id], |r| r.get(0)).unwrap()
    }

    fn events(&self, kind: &str, deployment: &str) -> Vec<Value> {
        self.store
            .conn
            .prepare("SELECT payload_json FROM management_events WHERE kind=?1 AND deployment_id=?2 ORDER BY sequence")
            .unwrap()
            .query_map([kind, deployment], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|raw| serde_json::from_str(&raw.unwrap()).unwrap())
            .collect()
    }

    fn release(&self, context: &StepExecutionContext, identities: Vec<ProcessIdentity>, now: i64) {
        let ttl = self
            .store
            .observation_ttl_for_step(&context.token.step_id)
            .unwrap();
        self.store
            .release_failed_launch(
                &self.session,
                &context.token.step_id,
                &gone(context, identities, now),
                now,
                ttl,
            )
            .unwrap();
    }
}

/// The fixture deployment as a `restart_only` one with a 1 GiB pool.
fn on(mut config: Value) -> Value {
    config["residency"] = json!("restart_only");
    config["lmcache_pool"] = json!("1GiB");
    config
}

fn fence_of(context: &StepExecutionContext) -> DeploymentFence {
    DeploymentFence {
        deployment_id: context.token.deployment_id.clone(),
        revision: context.token.revision,
        generation: context.token.generation,
    }
}

fn gone(
    context: &StepExecutionContext,
    identities: Vec<ProcessIdentity>,
    now: i64,
) -> CleanupEvidence {
    CleanupEvidence {
        binding_id: context.binding_id.clone(),
        incarnation: context.incarnation.clone(),
        identities,
        observed_at_ms: now,
        receipt: "every recorded process observed gone".into(),
    }
}

fn unified(footprint: &PhaseFootprint) -> (i64, i64) {
    let a = footprint
        .allocations
        .iter()
        .find(|a| a.domain == "unified")
        .unwrap();
    (a.bytes, a.host_kv_bytes)
}

/// The LMCache charge on unified memory (ADR 0033 §6, LC1): the pool, the
/// overhead allowance and the device allowance, all in the one pool.
const CHARGE: i64 = GIB + 1280 * MIB + 512 * MIB;

// T45: the server's two ports come from the host's endpoint range, apart from
// every engine port and every other server's, and stay held until the launch
// is released; a new revision's launch (a new binding) gets its own server.
#[test]
fn ports_are_leased_disjoint_and_held_until_release() {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    let b = lab.deploy("b", |_| {});
    let (_, ca) = lab.start(&a, 100);
    let (_, cb) = lab.start(&b, 100);
    let grant_a = lab
        .store
        .launch_lmcache_grant(&ca.binding_id)
        .unwrap()
        .unwrap();
    let grant_b = lab
        .store
        .launch_lmcache_grant(&cb.binding_id)
        .unwrap()
        .unwrap();
    let engine_ports: Vec<u16> = lab
        .store
        .conn
        .prepare("SELECT port FROM endpoint_leases")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let mut every = engine_ports.clone();
    every.extend([
        grant_a.rpc_port,
        grant_a.http_port,
        grant_b.rpc_port,
        grant_b.http_port,
    ]);
    let distinct: std::collections::BTreeSet<_> = every.iter().collect();
    assert_eq!(distinct.len(), every.len(), "{every:?}");
    assert!(every.iter().all(|p| (8100..=8199).contains(p)));
    assert_eq!(grant_a.pool_bytes, GIB);
    // Held while the binding is retained.
    let tx = lab.store.conn.unchecked_transaction().unwrap();
    assert!(port_held(&tx, "lab", grant_a.rpc_port).unwrap());
    drop(tx);
    // Released with the launch, so a later start may lease them again.
    lab.release(&ca, Vec::new(), 200);
    let tx = lab.store.conn.unchecked_transaction().unwrap();
    assert!(!port_held(&tx, "lab", grant_a.rpc_port).unwrap());
    assert!(!port_held(&tx, "lab", grant_a.http_port).unwrap());
    assert!(port_held(&tx, "lab", grant_b.http_port).unwrap());
}

// T45 T43: a reason the store knows journals `lmcache_off` once per launch;
// the deployment's own `lmcache: off` journals nothing.
#[test]
fn store_known_off_reasons_are_recorded_once_and_disabled_is_not() {
    let lab = Lab::new(|_| {});
    let disabled = lab.deploy("off", |c| {
        c["lmcache"] = json!("off");
        c.as_object_mut().unwrap().remove("lmcache_pool");
    });
    let parked = lab.deploy("deep", |c| c["residency"] = json!("deep"));
    let (_, cd) = lab.start(&disabled, 100);
    let (_, cp) = lab.start(&parked, 100);
    let plan = lab.store.launch_lmcache(&cd.binding_id).unwrap().unwrap();
    assert_eq!(
        plan.planned,
        PlannedLmcache::Off(LmcacheOff::new(LmcacheOffReason::Disabled))
    );
    assert!(lab
        .events("lmcache_off", &disabled.deployment_id)
        .is_empty());
    let plan = lab.store.launch_lmcache(&cp.binding_id).unwrap().unwrap();
    assert!(
        matches!(&plan.planned, PlannedLmcache::Off(off) if off.reason == LmcacheOffReason::ParkUnqualified)
    );
    let events = lab.events("lmcache_off", &parked.deployment_id);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["reason"], "park_unqualified");
    assert_eq!(events[0]["binding_id"], json!(cp.binding_id));
    // The host reporting the same reason again records nothing more.
    lab.store
        .record_launch_lmcache(
            &lab.session,
            &cp.binding_id,
            &LmcacheOutcome::Off {
                reason: LmcacheOffReason::ParkUnqualified,
                detail: String::new(),
            },
            150,
        )
        .unwrap();
    assert_eq!(lab.events("lmcache_off", &parked.deployment_id).len(), 1);
    // No ports were leased for either.
    assert!(lab
        .store
        .launch_lmcache_grant(&cd.binding_id)
        .unwrap()
        .is_none());
}

// T45 T14: the cold and Ready charges are the engine's plus the pool and the
// allowances (unified: all in the one pool, the pool against host_kv_limit);
// a host's Off report drops the charge at Ready; a measured overhead replaces
// the allowances for the revision's next launch.
#[test]
fn the_charge_is_pool_plus_allowance_then_measured() {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    let (_, context) = lab.start(&a, 100);
    assert_eq!(unified(&lab.footprint(&a)), (10 * GIB + CHARGE, 2 * GIB));
    lab.ready(&context, 10, Some(LmcacheOutcome::On), 200);
    assert_eq!(unified(&lab.footprint(&a)), (8 * GIB + CHARGE, 2 * GIB));
    assert!(lab
        .store
        .record_lmcache_overhead(&lab.session, &context.binding_id, 600 * MIB, 300 * MIB, 300)
        .unwrap());
    // Max-merged: a smaller figure does not lower it.
    assert!(!lab
        .store
        .record_lmcache_overhead(&lab.session, &context.binding_id, 500 * MIB, 200 * MIB, 301)
        .unwrap());
    // A second instance of the same revision charges the measured figures.
    let b = lab.deploy("b", |_| {});
    let (_, cb) = lab.start(&b, 400);
    let grant = lab
        .store
        .launch_lmcache_grant(&cb.binding_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        (grant.overhead_bytes, grant.device_bytes),
        (1280 * MIB, 512 * MIB)
    );
    // The revision's next launch on the host charges the measured figures
    // in place of the allowances.
    let tx = lab.store.conn.unchecked_transaction().unwrap();
    let (_, e) = effective(&tx, &a).unwrap();
    let plan = plan_on(&tx, &a.deployment_id, a.revision, "lab", &e).unwrap();
    let (charge, measured) = plan_charge(&tx, &plan, &a.deployment_id, a.revision, "lab", &e)
        .unwrap()
        .unwrap();
    assert!(measured);
    assert_eq!(
        (
            charge.pool_bytes,
            charge.overhead_bytes,
            charge.device_bytes
        ),
        (GIB, 600 * MIB, 300 * MIB)
    );
    assert_eq!(charge.held_kv_bytes, 4 * GIB);
    drop(tx);

    lab.release(&cb, Vec::new(), 500);
    // Reported off at Ready: the steady charge is the engine's alone.
    let c = lab.deploy("c", |_| {});
    let (_, cc) = lab.start(&c, 600);
    lab.ready(
        &cc,
        30,
        Some(LmcacheOutcome::Off {
            reason: LmcacheOffReason::ServerStartFailed,
            detail: "no /status within 30 s".into(),
        }),
        700,
    );
    assert_eq!(unified(&lab.footprint(&c)), (8 * GIB, GIB));
    assert_eq!(lab.events("lmcache_off", &c.deployment_id).len(), 1);
}

// T45 T14: the pool counts against host_kv_limit: a 4 GiB pool beside the
// engine's 1 GiB of host KV does not fit a 4 GiB limit, and the start is
// refused at arm.
#[test]
fn the_pool_counts_against_host_kv_limit() {
    let lab = Lab::new(|h| {
        h["resource_policy"]["domains"]["unified"]["host_kv_limit"] = json!("4GiB");
    });
    let a = lab.deploy("a", |c| {
        c.as_object_mut().unwrap().remove("lmcache_pool");
    });
    let accepted = lab
        .store
        .accept_start(&lab.session, &a, 100, 100_100)
        .unwrap();
    assert!(lab.arm_start(&accepted.step_id, 100).is_err());
}

// T45 T10 T16: a restart_only launch with LMCache on parks by stopping its
// engine (explicit park and idle park), parked charge = server charge + the
// engine KV it keeps mapped; an on-demand wake starts a new engine on the same
// server, whose identities the binding then records.
#[test]
fn restart_park_parks_on_the_server_and_wakes_a_new_engine() {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    let (_, context) = lab.start(&a, 100);
    let recorded = lab.ready(&context, 10, Some(LmcacheOutcome::On), 200);
    assert_eq!(
        lab.store.park_mode(&a.deployment_id, 0).unwrap(),
        ParkMode::EngineStop
    );
    let park = lab
        .store
        .accept_park_command(
            &lab.session,
            "operator",
            &a.deployment_id,
            a.revision,
            "park-1",
            1_000,
            61_000,
        )
        .unwrap();
    let parking = lab.arm_residency(&park.step_id, 1_100);
    assert!(
        matches!(&parking.identities, ExecutionIdentities::Retained(ids) if *ids == canonical_members(&recorded).unwrap())
    );
    // The engine still named: refused.
    assert!(lab
        .complete_residency(
            &parking,
            recorded.clone(),
            vec![Milestone::Quiesced, Milestone::MemoryReleased],
            1_200
        )
        .is_err());
    lab.complete_residency(
        &parking,
        server(10),
        vec![Milestone::Quiesced, Milestone::MemoryReleased],
        1_200,
    )
    .unwrap();
    // LC1: the server keeps the stopped engine's 4 GiB KV mapped.
    assert_eq!(unified(&lab.footprint(&a)), (CHARGE + 4 * GIB, GIB));
    assert_eq!(
        lab.store
            .runtime_binding_identities(&context.binding_id)
            .unwrap(),
        server(10)
    );
    // T10: an on-demand wake restores it.
    let wake = lab
        .store
        .accept_restore_command(
            &lab.session,
            "router",
            &a.deployment_id,
            WakeScope::OnDemand,
            a.revision,
            "wake-1",
            2_000,
            62_000,
        )
        .unwrap()
        .unwrap();
    let waking = lab.arm_residency(&wake.step_id, 2_100);
    assert!(matches!(&waking.identities, ExecutionIdentities::Retained(ids) if *ids == server(10)));
    // The wake's engine is recorded before it runs.
    lab.store
        .record_wake_identity(
            &lab.session,
            &a,
            &context.binding_id,
            &wake.step_id,
            &identity("api", 70),
        )
        .unwrap();
    let mut woken = vec![identity("api", 70), identity("worker-0", 71)];
    woken.extend(server(10));
    lab.complete_residency(
        &waking,
        woken.clone(),
        ResidencyKind::Restore.facts().to_vec(),
        2_200,
    )
    .unwrap();
    assert_eq!(
        lab.store
            .runtime_binding_identities(&context.binding_id)
            .unwrap(),
        canonical_members(&woken).unwrap()
    );
    assert_eq!(unified(&lab.footprint(&a)), (8 * GIB + CHARGE, 2 * GIB));

    // T10: the idle policy parks it the same way and keeps it eligible.
    let actions = lab
        .store
        .apply_idle_policy(
            &lab.session,
            1_000_000,
            crate::ordinary_lifecycle::park::IdlePolicy {
                ready_idle_ms: Some(1_000),
                ..Default::default()
            },
            &|_, _| None,
            0,
        )
        .unwrap();
    assert!(
        matches!(
            actions.as_slice(),
            [crate::ordinary_lifecycle::park::IdleAction::Parked { .. }]
        ),
        "{actions:?}"
    );
}

// T45: a restart_only launch whose host reported LMCache off does not park.
#[test]
fn restart_only_without_a_server_refuses_the_park() {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    let (_, context) = lab.start(&a, 100);
    lab.ready(
        &context,
        10,
        Some(LmcacheOutcome::Off {
            reason: LmcacheOffReason::CheckFailed,
            detail: "the package digest differs".into(),
        }),
        200,
    );
    assert_eq!(
        lab.store.park_mode(&a.deployment_id, 0).unwrap(),
        ParkMode::None
    );
    assert!(matches!(
        lab.store.accept_park_command(
            &lab.session,
            "operator",
            &a.deployment_id,
            a.revision,
            "park-1",
            1_000,
            61_000
        ),
        Err(LifecycleError::Unsupported)
    ));
}

// T45 T30: the engine failing with the connector is released and started
// again once without LMCache, the fallback sticks for the revision on the
// profile; under `required` it fails `lmcache_unavailable:engine_start_failed`
// and nothing starts again.
#[test]
fn the_engine_start_fallback_relaunches_once_and_sticks() {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    let (_, context) = lab.start(&a, 100);
    lab.store
        .record_launch_identity(&lab.session, &a, &context.binding_id, &server(10)[0])
        .unwrap();
    let ttl = lab
        .store
        .observation_ttl_for_step(&context.token.step_id)
        .unwrap();
    let outcome = lab
        .store
        .release_failed_launch_lmcache(
            &lab.session,
            &context.token.step_id,
            &gone(&context, server(10), 200),
            200,
            ttl,
            "RuntimeError: connector handshake failed",
        )
        .unwrap();
    let LmcacheFallback::Relaunched(start) = outcome else {
        panic!("expected a relaunch, got {outcome:?}");
    };
    let plan = lab
        .store
        .launch_lmcache(&start.binding_id)
        .unwrap()
        .unwrap();
    assert!(
        matches!(&plan.planned, PlannedLmcache::Off(off) if off.reason == LmcacheOffReason::EngineStartFailed)
    );
    let events = lab.events("lmcache_off", &a.deployment_id);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["reason"], "engine_start_failed");
    assert!(events[0]["detail"]
        .as_str()
        .unwrap()
        .contains("connector handshake failed"));
    let code: String = lab.row(
        "SELECT error_code FROM operations WHERE id=?1",
        &context.token.operation_id,
    );
    assert_eq!(code, "launch_failed");
    // Sticky: the relaunch fails too, and the next start of the revision
    // still runs without LMCache.
    let relaunched = lab.arm_start(&start.step_id, 300).unwrap();
    lab.release(&relaunched, Vec::new(), 400);
    // ADR 0011 decision 4: the failed instance closes its admission, as the
    // coordinator's conclusion does.
    lab.store
        .close_instance_admission_at(&a.deployment_id, 0, a.generation)
        .unwrap();
    let again = lab
        .store
        .accept_start(&lab.session, &a, 500, 100_500)
        .unwrap();
    let plan = lab
        .store
        .launch_lmcache(&again.binding_id)
        .unwrap()
        .unwrap();
    assert!(
        matches!(&plan.planned, PlannedLmcache::Off(off) if off.reason == LmcacheOffReason::EngineStartFailed)
    );

    // Under `required`: no relaunch, the closed code, nothing running.
    let r = lab.deploy("r", |c| c["lmcache"] = json!("required"));
    let (_, cr) = lab.start(&r, 100);
    let ttl = lab
        .store
        .observation_ttl_for_step(&cr.token.step_id)
        .unwrap();
    let outcome = lab
        .store
        .release_failed_launch_lmcache(
            &lab.session,
            &cr.token.step_id,
            &gone(&cr, Vec::new(), 200),
            200,
            ttl,
            "engine log tail",
        )
        .unwrap();
    assert_eq!(
        outcome,
        LmcacheFallback::Failed("lmcache_unavailable:engine_start_failed".into())
    );
    let code: String = lab.row(
        "SELECT error_code FROM operations WHERE id=?1",
        &cr.token.operation_id,
    );
    assert_eq!(code, "lmcache_unavailable:engine_start_failed");
    let open: i64 = lab.row(
        "SELECT COUNT(*) FROM runtime_bindings WHERE deployment_id=?1 AND state!='released'",
        &r.deployment_id,
    );
    assert_eq!(open, 0);
}

// T45 T24: the server exiting while Ready closes the instance's dispatch and
// journals `lmcache_server_exited` once; while parked too.
#[test]
fn a_server_exit_fails_the_instance_once_ready_or_parked() {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    let (_, context) = lab.start(&a, 100);
    lab.ready(&context, 10, Some(LmcacheOutcome::On), 200);
    let exit = EngineExit {
        deployment_id: a.deployment_id.clone(),
        generation: a.generation,
        step_id: context.token.step_id.clone(),
        process: server(10)[0].clone(),
        status: "signal 9".into(),
        observed_at_ms: 300,
    };
    let exited = lab
        .store
        .record_engine_exit(&lab.session, ExitSource::Embedded, &exit)
        .unwrap()
        .unwrap();
    assert!(exited.lmcache_server && !exited.parked && exited.first);
    let dispatch: bool = lab.row(
        "SELECT dispatch_enabled=1 FROM deployment_instances WHERE deployment_id=?1",
        &a.deployment_id,
    );
    assert!(!dispatch);
    lab.store
        .record_engine_exit(&lab.session, ExitSource::Embedded, &exit)
        .unwrap();
    assert_eq!(
        lab.events("lmcache_server_exited", &a.deployment_id).len(),
        1
    );

    // Parked: the server is all the launch holds.
    let b = lab.deploy("b", |_| {});
    let (_, cb) = lab.start(&b, 100);
    lab.ready(&cb, 20, Some(LmcacheOutcome::On), 200);
    let park = lab
        .store
        .accept_instance_park(
            &lab.session,
            "idle",
            &b.deployment_id,
            0,
            "idle-1",
            1_000,
            61_000,
        )
        .unwrap();
    let parking = lab.arm_residency(&park.step_id, 1_100);
    lab.complete_residency(&parking, server(20), vec![Milestone::MemoryReleased], 1_200)
        .unwrap();
    let exited = lab
        .store
        .record_engine_exit(
            &lab.session,
            ExitSource::Embedded,
            &EngineExit {
                deployment_id: b.deployment_id.clone(),
                generation: b.generation,
                step_id: cb.token.step_id.clone(),
                process: server(20)[0].clone(),
                status: "exit code 1".into(),
                observed_at_ms: 1_300,
            },
        )
        .unwrap()
        .unwrap();
    assert!(exited.lmcache_server && exited.parked);
    assert_eq!(
        lab.events("lmcache_server_exited", &b.deployment_id).len(),
        1
    );
    let local = lab.store.local_lmcache_launches(&lab.session).unwrap();
    assert!(local
        .iter()
        .all(|l| l.binding_id != cb.binding_id || l.parked));
}

// T45 T10: a role starting with `lmcache: off` (or `retention: zero`) stops
// each instance running a server: a Ready one owes a start without LMCache,
// a parked one is released like an idle stop; `lmcache_off` once each.
#[test]
fn role_start_retirement_stops_servers_and_owes_ready_ones_a_relaunch() {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    let (_, ca) = lab.start(&a, 100);
    lab.ready(&ca, 10, Some(LmcacheOutcome::On), 200);
    let b = lab.deploy("b", |_| {});
    let (_, cb) = lab.start(&b, 100);
    lab.ready(&cb, 20, Some(LmcacheOutcome::On), 200);
    let park = lab
        .store
        .accept_instance_park(
            &lab.session,
            "idle",
            &b.deployment_id,
            0,
            "idle-1",
            1_000,
            61_000,
        )
        .unwrap();
    let parking = lab.arm_residency(&park.step_id, 1_100);
    lab.complete_residency(&parking, server(20), vec![Milestone::MemoryReleased], 1_200)
        .unwrap();
    let stops = lab
        .store
        .retire_lmcache_on_host(&lab.session, "lab", LmcacheOffReason::HostOff, 2_000)
        .unwrap();
    assert_eq!(stops.len(), 2);
    // Idempotent: instances already stopping are not stopped again.
    assert!(lab
        .store
        .retire_lmcache_on_host(&lab.session, "lab", LmcacheOffReason::HostOff, 2_001)
        .unwrap()
        .is_empty());
    let owed: i64 = lab.row(
        "SELECT COUNT(*) FROM lmcache_relaunches WHERE deployment_id=?1",
        &a.deployment_id,
    );
    assert_eq!(owed, 1);
    let owed: i64 = lab.row(
        "SELECT COUNT(*) FROM lmcache_relaunches WHERE deployment_id=?1",
        &b.deployment_id,
    );
    assert_eq!(owed, 0);
    for d in [&a, &b] {
        let events = lab.events("lmcache_off", &d.deployment_id);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["reason"], "host_off");
    }
    // Nothing is relaunched before the stop completed.
    assert!(lab
        .store
        .relaunch_retired_lmcache(&lab.session, 2_100, None)
        .unwrap()
        .is_empty());
}

// T45: the server's group stands beside the engine's in a canonical set, and
// alone for a restart-parked launch; members need their leader.
#[test]
fn canonical_members_set_the_server_group_aside() {
    let mut both = engine(10);
    both.extend([identity("lmcache", 60), identity("lmcache-1", 61)]);
    assert!(canonical_members(&both).is_ok());
    assert!(canonical_members(&server(10)).is_ok());
    assert!(canonical_members(&[identity("api", 10), identity("lmcache-1", 61)]).is_err());
    assert!(canonical_members(&[identity("lmcache-1", 61)]).is_err());
}

// T45: the server's leader is recorded before it runs, then the engine's;
// a different server identity for the launch is a conflict.
#[test]
fn launch_identities_record_both_leaders_once() {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    let (_, context) = lab.start(&a, 100);
    let lmcache = server(10)[0].clone();
    lab.store
        .record_launch_identity(&lab.session, &a, &context.binding_id, &lmcache)
        .unwrap();
    lab.store
        .record_launch_identity(&lab.session, &a, &context.binding_id, &identity("api", 10))
        .unwrap();
    lab.store
        .record_launch_identity(&lab.session, &a, &context.binding_id, &lmcache)
        .unwrap();
    assert!(matches!(
        lab.store.record_launch_identity(
            &lab.session,
            &a,
            &context.binding_id,
            &identity("lmcache", 99)
        ),
        Err(LifecycleError::Conflict)
    ));
    assert_eq!(
        lab.store
            .runtime_binding_identities(&context.binding_id)
            .unwrap()
            .len(),
        2
    );
}

// T45 T14: ADR 0033 §§6, 12 (lifecycle design §7). What a host measured of
// its launch's server (resident memory beyond the pool is the overhead, and
// its GPU memory) is recorded for the revision on that host, max-merged per
// figure. The revision's next launch there charges it in place of the
// allowances; the same revision on another host still charges the
// allowances. A sample from another host, of another generation or naming
// no launch, changes nothing.
#[test]
fn a_host_measurement_replaces_the_allowances_of_the_next_launch_there() {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    let (_, context) = lab.start(&a, 100);
    let recorded = lab.ready(&context, 10, Some(LmcacheOutcome::On), 200);
    let step = context.token.step_id.clone();
    // The server group's resident memory is the 1 GiB pool plus `beyond`
    // MiB; its GPU memory is `device` MiB.
    let measure = |host: &str, step: &str, generation: i64, beyond: i64, device: i64| {
        lab.store
            .record_host_lmcache_measurement(
                &lab.session,
                host,
                &a.deployment_id,
                generation,
                step,
                GIB + beyond * MIB,
                device * MIB,
                300,
            )
            .unwrap()
    };
    let rows = || -> Vec<(String, i64, i64)> {
        lab.store
            .conn
            .prepare(
                "SELECT host_id,overhead_bytes,device_bytes FROM lmcache_measurements
                  WHERE deployment_id=?1 ORDER BY host_id",
            )
            .unwrap()
            .query_map([&a.deployment_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    let generation = a.generation;
    assert!(!measure("other", &step, generation, 600, 300));
    assert!(!measure("lab", &step, generation + 1, 600, 300));
    assert!(!measure("lab", "no-such-launch", generation, 600, 300));
    // A server within its pool using no GPU memory measured no overhead.
    assert!(!measure("lab", &step, generation, 0, 0));
    assert!(rows().is_empty());

    assert!(measure("lab", &step, generation, 600, 300));
    // A smaller sample does not lower it; a larger figure grows on its own.
    assert!(!measure("lab", &step, generation, 500, 200));
    assert!(measure("lab", &step, generation, 700, 100));
    assert_eq!(rows(), [("lab".to_owned(), 700 * MIB, 300 * MIB)]);
    // The running launch keeps the charge it was granted.
    assert_eq!(unified(&lab.footprint(&a)), (8 * GIB + CHARGE, 2 * GIB));

    // Stopped, then started again: the next launch of the revision on this
    // host is granted the measured figures.
    let stop = lab
        .store
        .accept_ordinary_stop_command(
            &lab.session,
            "operator",
            &a.deployment_id,
            a.revision,
            "stop-1",
            1_000,
            61_000,
        )
        .unwrap();
    let (_, cleanup) = lab
        .store
        .arm_ordinary_cleanup_with_context(&lab.session, &stop.step_id, 1_100)
        .unwrap();
    let cleanup = cleanup.unwrap();
    let ttl = lab
        .store
        .resource_policy("lab")
        .unwrap()
        .unwrap()
        .controls
        .observation_ttl_ms;
    lab.store
        .complete_cleanup(
            &lab.session,
            &stop.step_id,
            &CleanupEvidence {
                binding_id: cleanup.binding_id,
                incarnation: cleanup.incarnation,
                identities: recorded,
                observed_at_ms: 1_200,
                receipt: "every recorded process observed gone".into(),
            },
            1_200,
            ttl,
        )
        .unwrap();
    let restarted = DeploymentFence {
        generation: lab.row(
            "SELECT current_generation FROM deployments WHERE id=?1",
            &a.deployment_id,
        ),
        ..a.clone()
    };
    let (_, next) = lab.start(&restarted, 2_000);
    let grant = lab
        .store
        .launch_lmcache_grant(&next.binding_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        (grant.pool_bytes, grant.overhead_bytes, grant.device_bytes),
        (GIB, 700 * MIB, 300 * MIB)
    );
    assert_eq!(
        unified(&lab.footprint(&a)),
        (10 * GIB + GIB + 700 * MIB + 300 * MIB, 2 * GIB)
    );
    // The same revision on another host is charged the allowances.
    let tx = lab.store.conn.unchecked_transaction().unwrap();
    let (_, e) = effective(&tx, &a).unwrap();
    let plan = plan_on(&tx, &a.deployment_id, a.revision, "other", &e).unwrap();
    let (charge, measured) = plan_charge(&tx, &plan, &a.deployment_id, a.revision, "other", &e)
        .unwrap()
        .unwrap();
    assert!(!measured);
    assert_eq!(
        (charge.overhead_bytes, charge.device_bytes),
        (1280 * MIB, 512 * MIB)
    );
}

// T45 T14 (ADR 0033 §§6, 12, A1; found live 2026-10-22, GB10): a host's
// report whose server GPU memory nothing read (device zero) records the
// overhead alone, and the revision's next launch there keeps the device
// allowance; once a report reads the GPU memory, that figure replaces it.
#[test]
fn a_host_measurement_without_a_gpu_reading_keeps_the_device_allowance() {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    let (_, context) = lab.start(&a, 100);
    lab.ready(&context, 10, Some(LmcacheOutcome::On), 200);
    let step = context.token.step_id.clone();
    let measure = |resident: i64, device: i64| {
        lab.store
            .record_host_lmcache_measurement(
                &lab.session,
                "lab",
                &a.deployment_id,
                a.generation,
                &step,
                resident,
                device,
                300,
            )
            .unwrap()
    };
    let next = || {
        let tx = lab.store.conn.unchecked_transaction().unwrap();
        let (_, e) = effective(&tx, &a).unwrap();
        let plan = plan_on(&tx, &a.deployment_id, a.revision, "lab", &e).unwrap();
        let (charge, measured) = plan_charge(&tx, &plan, &a.deployment_id, a.revision, "lab", &e)
            .unwrap()
            .unwrap();
        (measured, charge.overhead_bytes, charge.device_bytes)
    };
    // The server's `VmRSS` is the 1 GiB pool plus 807 MiB.
    assert!(measure(GIB + 807 * MIB, 0));
    assert_eq!(next(), (true, 807 * MIB, 512 * MIB));
    assert!(measure(GIB + 807 * MIB, 406 * MIB));
    assert_eq!(next(), (true, 807 * MIB, 406 * MIB));
    // A later report without a GPU reading lowers nothing.
    assert!(!measure(GIB + 700 * MIB, 0));
    assert_eq!(next(), (true, 807 * MIB, 406 * MIB));
}

/// Instance 0 of `fence` in the status snapshot, as the management API
/// serializes it.
fn instance_status(lab: &Lab, fence: &DeploymentFence) -> Value {
    let snapshot = serde_json::to_value(lab.store.snapshot().unwrap()).unwrap();
    let deployment = snapshot["deployments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == json!(fence.deployment_id))
        .unwrap()
        .clone();
    deployment["instances"][0].clone()
}

/// The `lmcache` object status shows for a launch with LMCache on that was
/// charged the allowances.
fn on_with_allowances(server: &str, charge: i64) -> Value {
    json!({
        "state": "on",
        "server": server,
        "pool_bytes": GIB,
        "overhead_bytes": 1280 * MIB,
        "device_bytes": 512 * MIB,
        "held_kv_bytes": 4 * GIB,
        "charge_bytes": charge,
        "measured": false,
    })
}

// T45 T10 T14: ADR 0033 §§6, 7, 9. Status shows the charge the launch holds
// for its server, and it is the charge the store reserves: the pool and the
// allowances while starting and Ready; while restart-parked, the engine KV
// cache the server keeps mapped as well. A stop of the parked instance
// releases all of it, held KV included, and the stopped instance shows no
// LMCache.
#[test]
fn status_shows_the_server_charge_the_store_holds_through_park_and_stop() {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    let (_, context) = lab.start(&a, 100);
    assert_eq!(
        instance_status(&lab, &a)["lmcache"],
        on_with_allowances("starting", CHARGE)
    );
    lab.ready(&context, 10, Some(LmcacheOutcome::On), 200);
    assert_eq!(
        instance_status(&lab, &a)["lmcache"],
        on_with_allowances("running", CHARGE)
    );
    // The Ready footprint is the engine's 8 GiB plus exactly that charge.
    assert_eq!(unified(&lab.footprint(&a)).0, 8 * GIB + CHARGE);

    let park = lab
        .store
        .accept_park_command(
            &lab.session,
            "operator",
            &a.deployment_id,
            a.revision,
            "park-1",
            1_000,
            61_000,
        )
        .unwrap();
    let parking = lab.arm_residency(&park.step_id, 1_100);
    lab.complete_residency(
        &parking,
        server(10),
        vec![Milestone::Quiesced, Milestone::MemoryReleased],
        1_200,
    )
    .unwrap();
    let parked = instance_status(&lab, &a);
    assert_eq!(parked["observed_state"], "parked");
    assert_eq!(
        parked["lmcache"],
        on_with_allowances("running", CHARGE + 4 * GIB)
    );
    // LC1: the parked footprint is the server's charge and the held KV.
    assert_eq!(unified(&lab.footprint(&a)).0, CHARGE + 4 * GIB);

    let stop = lab
        .store
        .accept_instance_stop_command(
            &lab.session,
            "operator",
            &a.deployment_id,
            0,
            a.revision,
            "stop-1",
            2_000,
            62_000,
        )
        .unwrap()
        .unwrap();
    finish_stop(&lab, &stop.step_id, server(10), 2_100);
    let owner = crate::instances::instance_owner_id(&a.deployment_id, 0);
    assert!(!lab
        .store
        .resource_snapshot()
        .unwrap()
        .owners
        .contains_key(&owner));
    // The stop retired the incarnation: the instance shows no LMCache until
    // its next start plans one.
    let stopped = instance_status(&lab, &a);
    assert_eq!(stopped["observed_state"], "stopped", "{stopped}");
    assert!(stopped.get("lmcache").is_none(), "{stopped}");
}

// T45 T14: ADR 0033 §§6, 9. A running launch keeps the allowances it was
// granted after its server is measured; the revision's next launch on the
// host shows the measured overhead and GPU memory, marked `measured`, and
// that is what the store charges for it.
#[test]
fn status_marks_the_measured_overhead_of_the_next_launch() {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    let (_, context) = lab.start(&a, 100);
    let recorded = lab.ready(&context, 10, Some(LmcacheOutcome::On), 200);
    assert!(lab
        .store
        .record_lmcache_overhead(&lab.session, &context.binding_id, 600 * MIB, 300 * MIB, 300)
        .unwrap());
    assert_eq!(
        instance_status(&lab, &a)["lmcache"],
        on_with_allowances("running", CHARGE)
    );

    let stop = lab
        .store
        .accept_ordinary_stop_command(
            &lab.session,
            "operator",
            &a.deployment_id,
            a.revision,
            "stop-1",
            1_000,
            61_000,
        )
        .unwrap();
    finish_stop(&lab, &stop.step_id, recorded, 1_100);
    let restarted = DeploymentFence {
        generation: lab.row(
            "SELECT current_generation FROM deployments WHERE id=?1",
            &a.deployment_id,
        ),
        ..a.clone()
    };
    lab.start(&restarted, 2_000);
    let measured = GIB + 600 * MIB + 300 * MIB;
    assert_eq!(
        instance_status(&lab, &a)["lmcache"],
        json!({
            "state": "on",
            "server": "starting",
            "pool_bytes": GIB,
            "overhead_bytes": 600 * MIB,
            "device_bytes": 300 * MIB,
            "held_kv_bytes": 4 * GIB,
            "charge_bytes": measured,
            "measured": true,
        })
    );
    assert_eq!(unified(&lab.footprint(&a)).0, 10 * GIB + measured);
}

/// Arm the stop `step` and complete it on the host's evidence that
/// `identities` are gone.
fn finish_stop(lab: &Lab, step: &str, identities: Vec<ProcessIdentity>, now: i64) {
    let (_, cleanup) = lab
        .store
        .arm_ordinary_cleanup_with_context(&lab.session, step, now)
        .unwrap();
    let cleanup = cleanup.unwrap();
    let ttl = lab
        .store
        .resource_policy("lab")
        .unwrap()
        .unwrap()
        .controls
        .observation_ttl_ms;
    lab.store
        .complete_cleanup(
            &lab.session,
            step,
            &CleanupEvidence {
                binding_id: cleanup.binding_id,
                incarnation: cleanup.incarnation,
                identities,
                observed_at_ms: now + 100,
                receipt: "every recorded process observed gone".into(),
            },
            now + 100,
            ttl,
        )
        .unwrap();
}

// T45 T24 T30: ADR 0033 §§8, 9, 14. Status says why LMCache is off: the
// closed reason with the host's one-line detail, or the reason's sentence
// for a multi-line diagnostic, for the engine's log tail and for the
// deployment's own `off`; an off instance shows no figures. A server that
// exited shows `exited` while its launch is still held; a launch released
// before Ready shows it `stopped` with no charge. An instance never started
// shows no LMCache at all. One host per case: each start holds its charge.
#[test]
fn status_says_why_lmcache_is_off_and_when_its_server_exited() {
    let lmcache =
        |lab: &Lab, fence: &DeploymentFence| instance_status(lab, fence)["lmcache"].clone();
    let reported_off = |reason: LmcacheOffReason, detail: &str| {
        let lab = Lab::new(|_| {});
        let a = lab.deploy("a", |_| {});
        let (_, context) = lab.start(&a, 100);
        lab.ready(
            &context,
            10,
            Some(LmcacheOutcome::Off {
                reason,
                detail: detail.into(),
            }),
            200,
        );
        lmcache(&lab, &a)
    };
    assert_eq!(
        reported_off(
            LmcacheOffReason::ServerStartFailed,
            "no /status within 30 s"
        ),
        json!({"state": "off", "reason": "server_start_failed", "message": "no /status within 30 s"})
    );
    assert_eq!(
        reported_off(
            LmcacheOffReason::CheckFailed,
            "missing: lmcache/v1/multiprocess\nmissing: salt threading"
        ),
        json!({
            "state": "off",
            "reason": "check_failed",
            "message": "the launch-time check of the LMCache installation failed",
        })
    );

    let lab = Lab::new(|_| {});
    let disabled = lab.deploy("disabled", |c| {
        c["lmcache"] = json!("off");
        c.as_object_mut().unwrap().remove("lmcache_pool");
    });
    lab.start(&disabled, 100);
    assert_eq!(
        lmcache(&lab, &disabled),
        json!({"state": "off", "reason": "disabled", "message": "turned off by the deployment"})
    );

    // ADR 0033 §8: the relaunch without LMCache shows the reason's sentence,
    // never the engine's log tail.
    let lab = Lab::new(|_| {});
    let fallback = lab.deploy("fallback", |_| {});
    let (_, context) = lab.start(&fallback, 100);
    lab.store
        .record_launch_identity(&lab.session, &fallback, &context.binding_id, &server(30)[0])
        .unwrap();
    let ttl = lab
        .store
        .observation_ttl_for_step(&context.token.step_id)
        .unwrap();
    let outcome = lab
        .store
        .release_failed_launch_lmcache(
            &lab.session,
            &context.token.step_id,
            &gone(&context, server(30), 200),
            200,
            ttl,
            "RuntimeError: connector handshake failed",
        )
        .unwrap();
    assert!(
        matches!(outcome, LmcacheFallback::Relaunched(_)),
        "{outcome:?}"
    );
    assert_eq!(
        lmcache(&lab, &fallback),
        json!({
            "state": "off",
            "reason": "engine_start_failed",
            "message": "the engine did not start with LMCache",
        })
    );

    let lab = Lab::new(|_| {});
    let exited = lab.deploy("exited", |_| {});
    let (_, context) = lab.start(&exited, 100);
    lab.ready(&context, 40, Some(LmcacheOutcome::On), 200);
    lab.store
        .record_engine_exit(
            &lab.session,
            ExitSource::Embedded,
            &EngineExit {
                deployment_id: exited.deployment_id.clone(),
                generation: exited.generation,
                step_id: context.token.step_id.clone(),
                process: server(40)[0].clone(),
                status: "signal 9".into(),
                observed_at_ms: 300,
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        lmcache(&lab, &exited),
        on_with_allowances("exited", CHARGE),
        "the launch is still held until its stop proves it gone"
    );

    // A launch released before Ready keeps its incarnation: its server is
    // stopped and the launch holds no charge.
    let lab = Lab::new(|_| {});
    let released = lab.deploy("released", |_| {});
    let (_, context) = lab.start(&released, 100);
    lab.release(&context, Vec::new(), 200);
    let shown = lmcache(&lab, &released);
    assert_eq!(
        shown,
        json!({
            "state": "on",
            "server": "stopped",
            "pool_bytes": GIB,
            "overhead_bytes": 1280 * MIB,
            "device_bytes": 512 * MIB,
            "held_kv_bytes": 4 * GIB,
            "measured": false,
        })
    );
    let owner = crate::instances::instance_owner_id(&released.deployment_id, 0);
    assert!(!lab
        .store
        .resource_snapshot()
        .unwrap()
        .owners
        .contains_key(&owner));

    let never = lab.deploy("never", |_| {});
    let snapshot = serde_json::to_value(lab.store.snapshot().unwrap()).unwrap();
    let never = snapshot["deployments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == json!(never.deployment_id))
        .unwrap()
        .clone();
    assert!(
        never["instances"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i.get("lmcache").is_none()),
        "{never}"
    );
}

/// The binding's recorded identities, sorted.
fn recorded(lab: &Lab, binding: &str) -> Vec<ProcessIdentity> {
    let mut held = identities(
        &decode::<Vec<IdentityDto>>(&lab.row::<String>(
            "SELECT identities_json FROM runtime_bindings WHERE id=?1",
            binding,
        ))
        .unwrap(),
    );
    held.sort_by_key(|identity| identity.pid);
    held
}

// T45 T37 T14 T24 (ADR 0033 §§3, 9; found live on SGLang): a server that
// failed its listener check under `auto` was stopped (verified gone by the
// host) before the engine was spawned. Still recorded, it makes the engine's
// Ready a `lifecycle conflict`; once it left the binding's record (journaled
// `lmcache_server_departed`), the engine alone reaches Ready and status
// shows LMCache off with the reason. A departure is refused for a set that
// is not the server's, for a server the binding does not record, under a
// stale fence and once the launch is no longer armed; a repeat is a replay.
#[test]
fn a_server_stopped_before_its_engine_leaves_the_record_and_the_engine_reaches_ready() {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    let (_, context) = lab.start(&a, 100);
    let fence = fence_of(&context);
    let binding = context.binding_id.clone();
    let group = vec![identity("lmcache", 60), identity("lmcache-0", 61)];
    lab.store
        .record_launch_identity(&lab.session, &fence, &binding, &group[0])
        .unwrap();
    // The bug: the engine alone against a record that names the server.
    let receipt = OwnedLaunchReceipt {
        binding_id: binding.clone(),
        incarnation: context.incarnation.clone(),
        identities: engine(10),
        observed_at_ms: 150,
        receipt: "fake ready".into(),
    };
    assert!(matches!(
        lab.store
            .record_owned_launch(&lab.session, &context.token.step_id, &receipt, 150),
        Err(LifecycleError::Conflict)
    ));
    let depart = |fence: &DeploymentFence, server: &[ProcessIdentity]| {
        lab.store
            .depart_lmcache_server(&lab.session, fence, &binding, server)
    };
    assert!(matches!(
        depart(&fence, &engine(10)),
        Err(LifecycleError::Invalid)
    ));
    assert!(matches!(
        depart(&fence, &[identity("lmcache", 99)]),
        Err(LifecycleError::Conflict)
    ));
    let stale = DeploymentFence {
        generation: fence.generation + 1,
        ..fence.clone()
    };
    assert!(depart(&stale, &group).is_err());
    assert_eq!(recorded(&lab, &binding), group[..1]);

    depart(&fence, &group).unwrap();
    depart(&fence, &group).unwrap();
    assert!(recorded(&lab, &binding).is_empty());
    let departed: i64 = lab.row(
        "SELECT COUNT(*) FROM journal_entries WHERE operation_id=?1 AND state='lmcache_server_departed'",
        &context.token.operation_id,
    );
    assert_eq!(departed, 1);

    lab.store
        .record_launch_identity(&lab.session, &fence, &binding, &engine(10)[0])
        .unwrap();
    lab.ready(
        &context,
        10,
        Some(LmcacheOutcome::Off {
            reason: LmcacheOffReason::ListenerExposed,
            detail: "the LMCache server listens on 0.0.0.0:25002; the server was stopped".into(),
        }),
        200,
    );
    assert_eq!(recorded(&lab, &binding), engine(10));
    let status = instance_status(&lab, &a);
    assert_eq!(status["observed_state"], "ready", "{status}");
    assert_eq!(status["lmcache"]["state"], "off", "{status}");
    assert_eq!(status["lmcache"]["reason"], "listener_exposed", "{status}");
    assert_eq!(lab.events("lmcache_off", &a.deployment_id).len(), 1);
    assert!(matches!(
        depart(&fence, &group),
        Err(LifecycleError::Conflict)
    ));
}

// T45 T24 T10 (ADR 0033 §3; found live on SGLang): the server is its group's
// leader. A short-lived child recorded at Ready (`lmcache-0`) that exits is
// not the server's exit: the instance keeps serving, and a restart-park
// whose evidence no longer names the child parks with the child still
// recorded (only the launch's verified stop proves it gone). The leader's
// exit still fails the instance.
#[test]
fn a_server_member_exit_is_not_the_servers() {
    let lab = Lab::new(|_| {});
    let a = lab.deploy("a", |_| {});
    let (_, context) = lab.start(&a, 100);
    let fence = fence_of(&context);
    let group = vec![identity("lmcache", 60), identity("lmcache-0", 61)];
    lab.store
        .record_launch_identity(&lab.session, &fence, &context.binding_id, &group[0])
        .unwrap();
    lab.store
        .record_launch_lmcache(&lab.session, &context.binding_id, &LmcacheOutcome::On, 150)
        .unwrap();
    let mut launched = engine(10);
    launched.extend(group.clone());
    lab.store
        .record_owned_launch(
            &lab.session,
            &context.token.step_id,
            &OwnedLaunchReceipt {
                binding_id: context.binding_id.clone(),
                incarnation: context.incarnation.clone(),
                identities: launched.clone(),
                observed_at_ms: 150,
                receipt: "fake ready".into(),
            },
            150,
        )
        .unwrap();
    lab.store
        .complete_step(
            &lab.session,
            &context.token.step_id,
            &CompletionEvidence {
                token: context.token.clone(),
                identities: launched.clone(),
                observed_at_ms: 150,
                control_receipt: Some("fake ready".into()),
                milestones: ResidencyKind::Restore.facts().to_vec(),
            },
            150,
            2_000,
        )
        .unwrap();
    let exit = |process: &ProcessIdentity, at: i64| EngineExit {
        deployment_id: a.deployment_id.clone(),
        generation: a.generation,
        step_id: context.token.step_id.clone(),
        process: process.clone(),
        status: "exit code 0".into(),
        observed_at_ms: at,
    };
    assert_eq!(
        lab.store
            .record_engine_exit(&lab.session, ExitSource::Embedded, &exit(&group[1], 300))
            .unwrap(),
        None
    );
    let dispatch = |lab: &Lab| -> bool {
        lab.row(
            "SELECT dispatch_enabled=1 FROM deployment_instances WHERE deployment_id=?1",
            &a.deployment_id,
        )
    };
    assert!(dispatch(&lab), "the instance still serves");
    assert!(lab
        .events("lmcache_server_exited", &a.deployment_id)
        .is_empty());

    let park = lab
        .store
        .accept_instance_park(
            &lab.session,
            "idle",
            &a.deployment_id,
            0,
            "idle-1",
            1_000,
            61_000,
        )
        .unwrap();
    let parking = lab.arm_residency(&park.step_id, 1_100);
    lab.complete_residency(
        &parking,
        group[..1].to_vec(),
        vec![Milestone::MemoryReleased],
        1_200,
    )
    .unwrap();
    assert_eq!(recorded(&lab, &context.binding_id), group);

    let exited = lab
        .store
        .record_engine_exit(&lab.session, ExitSource::Embedded, &exit(&group[0], 1_300))
        .unwrap()
        .unwrap();
    assert!(exited.lmcache_server && exited.parked);
    assert_eq!(
        lab.events("lmcache_server_exited", &a.deployment_id).len(),
        1
    );
}

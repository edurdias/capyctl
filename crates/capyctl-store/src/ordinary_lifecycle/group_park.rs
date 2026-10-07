//! ADR 0028 §12: park and wake of a multi-node group instance, durably.
//!
//! A group's park or wake is the same operation, run and persisted step a
//! single-rank launch's is (`park.rs`): acceptance closes the head's
//! dispatch, the step arms once, its effect is sent exactly once (the head's
//! agent invokes the collective) and its evidence completes it. What differs
//! is the accounting: a group is charged as one owner per member, each on its
//! own host and judged against that host's own admission context, so
//!
//! | step    | arm                                                   | completion (each member's own report)   |
//! |---------|-------------------------------------------------------|-----------------------------------------|
//! | park    | every member stays at Ready; each host must hold its   | each member → its Parked budget         |
//! |         | member's parked budget (its `max_parked`, its parked  |                                         |
//! |         | limit), else `parked_capacity`                         |                                         |
//! | restore | each member Parked → Wake peak on its own host, all or | each member → Ready                     |
//! |         | none                                                  |                                         |
//!
//! A member's charge moves only on its own host's report: the park completes
//! only when every member reported at or below its parked budget, the wake
//! only when every member reported resident again. A member that did not
//! report, or still holds its memory, leaves the step uncertain with every
//! charge as it was (AGENTS.md: uncertainty retains accounting); the group is
//! then stopped and each member released on its own evidence (ADR 0028 §11).
//! `max_parked` counts the group once on each host, because each host holds
//! exactly one member owner of it.
use super::*;
use crate::resource_ledger::reserve_member_increase_in_transaction;
use std::collections::BTreeMap;

/// One member of a group instance as its park or wake judges it.
pub(super) struct Member {
    pub(super) rank: u32,
    pub(super) host: String,
    pub(super) owner: String,
    pub(super) state: String,
    pub(super) launch_handle: Option<String>,
    pub(super) identities: Option<String>,
    pub(super) effective: EffectiveDeployment,
}

impl Member {
    pub(super) fn ready(&self) -> PhaseFootprint {
        phase(&self.effective.resources.ready, ResourcePhase::Ready)
    }
    pub(super) fn parked(&self) -> PhaseFootprint {
        phase(&self.effective.resources.parked, ResourcePhase::Parked)
    }
    /// SPEC §7.3: the wake peak, from the parked budget into the wake phase.
    pub(super) fn wake(&self) -> PhaseFootprint {
        peak(
            &self.parked(),
            &phase(&self.effective.resources.wake, ResourcePhase::Wake),
            ResourcePhase::Wake,
        )
    }
}

/// The bytes a footprint charges across its domains.
fn total(footprint: &PhaseFootprint) -> i64 {
    footprint
        .allocations
        .iter()
        .map(|a| a.bytes)
        .fold(0_i64, i64::saturating_add)
}

/// One `group_members` row: rank, host, owner, state, launch handle and
/// recorded identities.
type MemberRowRaw = (u32, String, String, String, Option<String>, Option<String>);

/// The members of the instance's group at `generation` in rank order, each
/// with the revision as its own host resolved it. Empty for a single-host
/// launch, which nothing here changes (T39).
pub(super) fn members(
    tx: &Transaction<'_>,
    deployment: &str,
    revision: i64,
    instance: u32,
    generation: i64,
) -> Result<Vec<Member>, LifecycleError> {
    let rows: Vec<MemberRowRaw> = tx
        .prepare(
            "SELECT rank,host_id,owner_id,state,launch_handle,identities_json FROM group_members
              WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 ORDER BY rank",
        )?
        .query_map(params![deployment, instance, generation], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    rows.into_iter()
        .map(
            |(rank, host, owner, state, launch_handle, identities)| -> Result<Member, LifecycleError> {
                let (raw, _) = frozen_on_host(tx, deployment, revision, Some(&host))?;
                let effective = decode_effective_snapshot(&raw)
                    .map_err(|_| LifecycleError::CorruptStoredData)?;
                Ok(Member {
                    rank,
                    host,
                    owner,
                    state,
                    launch_handle,
                    identities,
                    effective,
                })
            },
        )
        .collect()
}

/// Whether the residency plan's instance runs as a group at its generation.
pub(super) fn is_group(tx: &Transaction<'_>, p: &ResidencyPlan) -> Result<bool, LifecycleError> {
    is_group_at(tx, &p.deployment_id, p.instance_index, p.generation)
}

/// Whether the instance ran as a group at `generation`.
pub(super) fn is_group_at(
    tx: &Transaction<'_>,
    deployment: &str,
    instance: u32,
    generation: i64,
) -> Result<bool, LifecycleError> {
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM group_plans WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3)",
        params![deployment, instance, generation],
        |r| r.get(0),
    )?)
}

/// ADR 0028 §12 (R34): a `restart_only` group, on any engine, parks by a
/// group stop: each READY instance is stopped ordinarily (it stays eligible
/// for on-demand activation, so it wakes by a group relaunch), and its
/// cleanup stops every member and releases each only on its own host's
/// evidence (ADR 0028 §11). The receipt names the first instance's stop.
#[allow(clippy::too_many_arguments)]
pub(super) fn stop_restart_only(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    principal: &str,
    deployment: &str,
    key: &str,
    now: i64,
    deadline: i64,
) -> Result<ResidencyReceipt, LifecycleError> {
    let targets: Vec<u32> = tx
        .prepare(
            "SELECT instance_index FROM deployment_instances
              WHERE deployment_id=?1 AND state='active' AND observed_state='ready'
              ORDER BY instance_index",
        )?
        .query_map([deployment], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let mut first: Option<ResidencyReceipt> = None;
    for instance in targets {
        let (source, _, _) = launch(tx, deployment, instance)?;
        let stop = crate::Store::accept_instance_stop_in_transaction(
            tx,
            s,
            principal,
            &source.fence(),
            key,
            now,
            deadline,
            &StopCommand {
                scope: Some(instance_scope(deployment, instance)),
                revision: Some(source.revision),
            },
        )?;
        journal(
            tx,
            &stop.operation_id,
            "group_park_stop",
            &format!(
                "deployment {deployment}: instance {instance} is a restart_only group; it parks by a group stop (each member released on its own host's evidence) and wakes by a group relaunch"
            ),
        )?;
        first.get_or_insert(ResidencyReceipt {
            operation_id: stop.operation_id,
            step_id: stop.step_id,
            deployment_id: deployment.into(),
            instance,
            kind: ResidencyKind::Park,
            revision: stop.revision,
            generation: stop.generation,
            accepted_at_ms: stop.accepted_at_ms,
            deadline_ms: stop.deadline_ms,
            joined: false,
        });
    }
    // SPEC §6.3: a deployment with no READY instance has nothing to park.
    first.ok_or(LifecycleError::Conflict)
}

/// ADR 0028 §12: a group parks only when every member's own host resolved a
/// deployment that parks (`deep`, its host's deep park enabled).
pub(super) fn group_parks(members: &[Member]) -> bool {
    !members.is_empty() && members.iter().all(|m| parks(&m.effective))
}

/// ADR 0028 §12 (R12): one member of an armed group park or wake, as the
/// controller collects its own host's report for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArmedMember {
    pub rank: u32,
    pub host_id: String,
    pub owner_id: String,
    /// The bytes the member's parked budget charges: a park settles for it
    /// only at or below this, a wake only above it.
    pub parked_bytes: i64,
    /// The member's Launch handle on its host.
    pub launch_handle: Option<String>,
    /// The member's recorded process identities.
    pub identities: Vec<ProcessIdentity>,
    /// The effective residency the member's own host resolved (R34: park
    /// and wake follow it, never the engine).
    pub residency: Residency,
}

/// ADR 0028 §12 (R12): what one member's own host reported of it after the
/// head's collective: the bytes its own processes hold (vLLM, per-member
/// `process_residency`) or its saver still maps (SGLang, saver-map facts
/// from that host's observation directory), observed at `observed_at_ms`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberResidency {
    pub rank: u32,
    pub host_id: String,
    pub resident_bytes: i64,
    pub observed_at_ms: i64,
}

impl MemberResidency {
    /// SPEC §6.1 PARKED: at or below the member's parked budget.
    pub fn released(&self, member: &ArmedMember) -> bool {
        self.rank == member.rank
            && self.host_id == member.host_id
            && self.resident_bytes >= 0
            && self.resident_bytes <= member.parked_bytes
    }
    /// SPEC §9.1: memory came back above the parked budget.
    pub fn resident(&self, member: &ArmedMember) -> bool {
        self.rank == member.rank
            && self.host_id == member.host_id
            && self.resident_bytes > member.parked_bytes
    }
}

fn armed(m: &Member) -> Result<ArmedMember, LifecycleError> {
    Ok(ArmedMember {
        rank: m.rank,
        host_id: m.host.clone(),
        owner_id: m.owner.clone(),
        parked_bytes: total(&m.parked()),
        residency: m.effective.residency,
        launch_handle: m.launch_handle.clone(),
        identities: match &m.identities {
            Some(raw) => crate::groups::decode_identities(raw)
                .map_err(|_| LifecycleError::CorruptStoredData)?,
            None => Vec::new(),
        },
    })
}

/// ADR 0028 §12: arm one planned group park or wake. `contexts` holds one
/// admission context per member host, from that host's own observations,
/// limits and `max_parked`.
pub(super) fn arm(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    id: &str,
    now: i64,
    contexts: &BTreeMap<String, AdmissionContext<'_>>,
) -> Result<ResidencyArm, LifecycleError> {
    let (mut p, state) = read(tx, id)?;
    if state != "planned" {
        return Err(LifecycleError::Conflict);
    }
    let (_, _, identities) = current_residency(tx, s, &p)?;
    let members = members(
        tx,
        &p.deployment_id,
        p.revision,
        p.instance_index,
        p.generation,
    )?;
    // ADR 0028 §11: a group with a member not proven launched is not one a
    // collective may act on. A wake is judged by every member host's own
    // fresh observations; a park, which only releases memory, by the ledger.
    if members.len() < 2
        || members.iter().any(|m| m.state != "launched")
        || (p.kind == ResidencyKind::Restore
            && (contexts.len() != members.len()
                || members.iter().any(|m| !contexts.contains_key(&m.host))))
    {
        return Err(LifecycleError::Conflict);
    }
    if now < p.accepted_at_ms || now >= p.deadline_ms {
        return Err(LifecycleError::Conflict);
    }
    let (observed, _, _, admitting) =
        instance_state(tx, &p.deployment_id, p.instance_index)?.ok_or(LifecycleError::NotFound)?;
    let expected = match p.kind {
        ResidencyKind::Park => "ready",
        ResidencyKind::Restore => "parked",
    };
    if observed != expected || !admitting {
        return Err(LifecycleError::Conflict);
    }
    if p.kind == ResidencyKind::Park {
        // SPEC §10 step 4: in-flight work drains before the park is sent.
        let leased: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM request_leases WHERE deployment_id=?1 AND instance_index=?2)",
            params![p.deployment_id, p.instance_index],
            |r| r.get(0),
        )?;
        if leased {
            return Ok(ResidencyArm::Draining);
        }
    }
    let ledger = resource_ledger::read_snapshot(tx).map_err(resource)?;
    // Every member holds the footprint the transition starts from, on its
    // own host, judged against that host's own policy.
    for m in &members {
        let held = match p.kind {
            ResidencyKind::Park => m.ready(),
            ResidencyKind::Restore => m.parked(),
        };
        if !ledger
            .owners
            .get(&m.owner)
            .is_some_and(|current| same(current, &held))
        {
            return Err(LifecycleError::Conflict);
        }
        if let Some(context) = contexts.get(&m.host) {
            let policy = policy(tx, &m.effective)?;
            let mut supplied = context.limits.to_vec();
            supplied.sort_by(|a, b| a.domain.cmp(&b.domain));
            if supplied != limits(&policy)
                || context.ttl_ms != policy.controls.observation_ttl_ms
                || context.max_parked != policy.controls.max_parked as usize
                || !context.resident_floors.is_empty()
                || context.now_ms != now
            {
                return Err(LifecycleError::Conflict);
            }
        }
    }
    // SPEC §6.5, §7.3, ADR 0028 §12: judge every host before anything is
    // charged; one host that cannot take its member refuses the whole step.
    let mut shortfalls = Vec::new();
    for m in &members {
        let policy = policy(tx, &m.effective)?;
        let limits = limits(&policy);
        let scoped = resource_ledger::scoped_to_domain_hosts(
            tx,
            &ledger,
            limits.iter().map(|l| l.domain.as_str()),
        )
        .map_err(resource)?;
        let refused = match p.kind {
            // The parked set each host holds after the park: its parked limit
            // and its `max_parked`, which counts this group once there.
            ResidencyKind::Park => capyctl_scheduler::placement::fits(
                &scoped,
                &m.owner,
                &m.parked(),
                &limits,
                policy.controls.max_parked as usize,
            )
            .err()
            .map(|refusal| refusal.code().to_owned()),
            ResidencyKind::Restore => capyctl_scheduler::residency::admit_phase(
                &scoped,
                &m.owner,
                &m.wake(),
                contexts[&m.host],
            )
            .err()
            .map(|error| error.to_string()),
        };
        if let Some(why) = refused {
            shortfalls.push(format!("host {} (rank {}): {why}", m.host, m.rank));
        }
    }
    if !shortfalls.is_empty() {
        let why = shortfalls.join("; ");
        return Ok(match p.kind {
            // A park whose parked set no host may hold is refused; the group
            // stays Ready and serves again.
            ResidencyKind::Park => {
                fail(tx, s, &p, "parked_capacity", &why)?;
                ResidencyArm::Refused("parked_capacity")
            }
            // A wake waits for capacity or its deadline; it never evicts
            // here (that is switching's, across every named host).
            ResidencyKind::Restore => ResidencyArm::Blocked(why),
        });
    }
    let execution = ResidencyExecution {
        issued_at_ms: now,
        // The step names no ledger grant: each member's own grant is its
        // charge (`grant_id` on the step stays NULL).
        grant_id: ulid::Ulid::new().to_string(),
        expected_epoch: ledger.epoch,
    };
    match p.kind {
        // A park only ever releases memory: every member keeps its Ready
        // charge until its own report moves it. The epoch advances once, as
        // an ordinary arm's grant advances it, so every fence reads the same.
        ResidencyKind::Park => {
            resource_ledger::advance_completion_epoch(tx)?;
        }
        // SPEC §7.3: the wake peak is charged on every host before anything
        // grows; a member that no longer fits rolls the whole step back.
        ResidencyKind::Restore => {
            for m in &members {
                let epoch = resource_ledger::read_snapshot(tx).map_err(resource)?.epoch;
                reserve_member_increase_in_transaction(
                    tx,
                    &GrantRequest {
                        id: ulid::Ulid::new().to_string(),
                        owner_id: m.owner.clone(),
                        deployment_id: p.deployment_id.clone(),
                        operation_id: p.operation_id.clone(),
                        revision: p.revision,
                        generation: p.generation,
                        expected_epoch: epoch,
                        next: m.wake(),
                    },
                    m.rank,
                    contexts[&m.host],
                )
                .map_err(|error| LifecycleError::Rejected(error.to_string()))?;
            }
        }
    }
    one(tx.execute(
        "UPDATE operations SET state='running' WHERE id=?1 AND state='pending'",
        [&p.operation_id],
    )?)?;
    one(tx.execute(
        "UPDATE lifecycle_runs SET state='running' WHERE operation_id=?1 AND session_id=?2 AND state='queued'",
        params![p.operation_id, s.id()],
    )?)?;
    p.execution = Some(execution);
    one(tx.execute(
        "UPDATE lifecycle_steps SET state='armed',step_json=?2 WHERE id=?1 AND session_id=?3 AND state='planned' AND grant_id IS NULL",
        params![id, encode(&p)?, s.id()],
    )?)?;
    record(tx, s, &p, Stage::Armed, None)?;
    Ok(ResidencyArm::New(Box::new(StepExecutionContext {
        token: p.token(),
        binding_id: p.binding_id.clone(),
        incarnation: p.incarnation.clone(),
        issued_at_ms: now,
        deadline_ms: p.deadline_ms,
        identities: ExecutionIdentities::Retained(identities),
        completion_target: None,
        grant_id: None,
        launch_settings: None,
    })))
}

/// ADR 0028 §12 (R12): commit a group park's or wake's evidence. Every
/// member must be reported by its own host, observed fresh within the step:
/// at or below its parked budget for a park, above it for a wake. Each member
/// owner then moves to its own host's Parked (park) or Ready (wake)
/// footprint; anything short of every member leaves everything unchanged.
pub(super) fn complete(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    id: &str,
    reports: &[MemberResidency],
    receipt: &str,
    now: i64,
) -> Result<(), LifecycleError> {
    let (p, state) = read(tx, id)?;
    if state != "armed" {
        return Err(LifecycleError::Conflict);
    }
    let (_, e, identities) = current_residency(tx, s, &p)?;
    let execution = p
        .execution
        .clone()
        .ok_or(LifecycleError::CorruptStoredData)?;
    let ttl = policy(tx, &e)?.controls.observation_ttl_ms;
    let members = members(
        tx,
        &p.deployment_id,
        p.revision,
        p.instance_index,
        p.generation,
    )?;
    if members.len() < 2 || members.iter().any(|m| m.state != "launched") {
        return Err(LifecycleError::Conflict);
    }
    let ledger = resource_ledger::read_snapshot(tx).map_err(resource)?;
    if ledger.epoch <= execution.expected_epoch {
        return Err(LifecycleError::Conflict);
    }
    let mut last_observed = execution.issued_at_ms;
    for m in &members {
        let report = reports.iter().find(|r| r.rank == m.rank).ok_or_else(|| {
            LifecycleError::Rejected(format!("rank {} has no report of its own", m.rank))
        })?;
        let member = armed(m)?;
        let proven = match p.kind {
            ResidencyKind::Park => report.released(&member),
            ResidencyKind::Restore => report.resident(&member),
        };
        if !proven {
            return Err(LifecycleError::Rejected(format!(
                "rank {} on {} did not report the {} on its own host",
                m.rank,
                m.host,
                p.kind.verb()
            )));
        }
        fresh(
            execution.issued_at_ms,
            p.deadline_ms,
            report.observed_at_ms,
            now,
            ttl,
        )?;
        last_observed = last_observed.max(report.observed_at_ms);
        let held = match p.kind {
            ResidencyKind::Park => m.ready(),
            ResidencyKind::Restore => m.wake(),
        };
        if !ledger
            .owners
            .get(&m.owner)
            .is_some_and(|current| same(current, &held))
        {
            return Err(LifecycleError::Conflict);
        }
    }
    if p.kind == ResidencyKind::Park {
        let leased: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM request_leases WHERE deployment_id=?1 AND instance_index=?2)",
            params![p.deployment_id, p.instance_index],
            |r| r.get(0),
        )?;
        if leased {
            return Err(LifecycleError::Conflict);
        }
    }
    let evidence = encode(&completion_value(&CompletionEvidence {
        token: p.token(),
        identities,
        observed_at_ms: last_observed,
        control_receipt: Some(receipt.to_owned()),
        milestones: p.kind.facts().to_vec(),
    })?)?;
    let epoch = resource_ledger::advance_completion_epoch(tx)?;
    for m in &members {
        let next = match p.kind {
            ResidencyKind::Park => m.parked(),
            ResidencyKind::Restore => m.ready(),
        };
        one(tx.execute(
            "UPDATE resource_owners SET footprint_json=?2 WHERE owner_id=?1",
            params![m.owner, resource_ledger::encode(&next).map_err(resource)?],
        )?)?;
    }
    match p.kind {
        ResidencyKind::Park => one(tx.execute(
            "UPDATE deployment_instances SET observed_state='parked',dispatch_enabled=0 WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 AND observed_state='ready'",
            params![p.deployment_id, p.instance_index, p.generation],
        )?)?,
        ResidencyKind::Restore => one(tx.execute(
            &format!(
                "UPDATE deployment_instances AS i SET observed_state='ready',
                        dispatch_enabled=CASE WHEN i.desired_state='ready' AND i.admission_enabled=1 AND NOT EXISTS(SELECT 1 FROM deployments d WHERE d.id=?1 AND d.suspended=1) AND {} THEN 1 ELSE 0 END
                  WHERE i.deployment_id=?1 AND i.instance_index=?2 AND i.generation=?3 AND i.observed_state='parked'",
                crate::switch_state::no_closure_clause("i")
            ),
            params![p.deployment_id, p.instance_index, p.generation],
        )?)?,
    }
    one(tx.execute(
        "UPDATE lifecycle_steps SET state='completed' WHERE id=?1 AND state='armed'",
        [id],
    )?)?;
    one(tx.execute(
        "UPDATE lifecycle_runs SET state='succeeded' WHERE operation_id=?1 AND state='running'",
        [&p.operation_id],
    )?)?;
    one(tx.execute(
        "UPDATE operations SET state='succeeded' WHERE id=?1 AND state='running'",
        [&p.operation_id],
    )?)?;
    tx.execute(
        "INSERT INTO lifecycle_evidence(step_id,evidence_json,committed_epoch) VALUES(?1,?2,?3)",
        params![id, evidence, epoch],
    )?;
    one(tx.execute(
        "DELETE FROM lifecycle_claims WHERE operation_id=?1",
        [&p.operation_id],
    )?)?;
    journal(
        tx,
        &p.operation_id,
        match p.kind {
            ResidencyKind::Park => "parked",
            ResidencyKind::Restore => "restored",
        },
        &format!(
            "deployment {}: group instance {} {}; every member reported on its own host ({})",
            p.deployment_id,
            p.instance_index,
            match p.kind {
                ResidencyKind::Park => "parked by the head's one collective",
                ResidencyKind::Restore =>
                    "woken by the head's one collective, probed and canary-checked",
            },
            redact_reason(receipt)
        ),
    )?;
    record(tx, s, &p, Stage::Completed, Some(epoch))
}

/// SPEC §13 (W4), ADR 0028 §12: the head refused the armed group step before
/// any effect. A park charged nothing beyond Ready, so every member keeps
/// it; a wake returns every member from its wake peak to its parked budget.
pub(super) fn refuse_members(
    tx: &Transaction<'_>,
    p: &ResidencyPlan,
) -> Result<(), LifecycleError> {
    let members = members(
        tx,
        &p.deployment_id,
        p.revision,
        p.instance_index,
        p.generation,
    )?;
    let ledger = resource_ledger::read_snapshot(tx).map_err(resource)?;
    for m in &members {
        let (held, back) = match p.kind {
            ResidencyKind::Park => (m.ready(), m.ready()),
            ResidencyKind::Restore => (m.wake(), m.parked()),
        };
        if !ledger
            .owners
            .get(&m.owner)
            .is_some_and(|current| same(current, &held))
        {
            return Err(LifecycleError::Conflict);
        }
        one(tx.execute(
            "UPDATE resource_owners SET footprint_json=?2 WHERE owner_id=?1",
            params![m.owner, resource_ledger::encode(&back).map_err(resource)?],
        )?)?;
    }
    Ok(())
}

impl crate::Store {
    /// ADR 0028 §12: the members of the group a planned or armed park or wake
    /// acts on, in rank order, each with its own host and parked budget.
    /// Empty when the step's instance is not a group.
    pub fn group_residency_members(
        &self,
        s: &CoordinatorSession,
        id: &str,
    ) -> Result<Vec<ArmedMember>, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, s)?;
        let (p, _) = read(&tx, id)?;
        let out = if is_group(&tx, &p)? {
            members(
                &tx,
                &p.deployment_id,
                p.revision,
                p.instance_index,
                p.generation,
            )?
            .iter()
            .map(armed)
            .collect::<Result<_, _>>()?
        } else {
            Vec::new()
        };
        tx.commit()?;
        Ok(out)
    }

    /// ADR 0028 §12: arm one planned group park or wake at `now`. A wake is
    /// judged against every member host's own fresh observations
    /// (`contexts`, one per member host); a park, which only releases
    /// memory, by each host's ledger, limits and `max_parked` alone
    /// (`contexts` may be empty). Only `ResidencyArm::New` permits the one
    /// send to the head.
    pub fn arm_group_residency(
        &self,
        s: &CoordinatorSession,
        id: &str,
        now: i64,
        contexts: &BTreeMap<String, AdmissionContext<'_>>,
    ) -> Result<ResidencyArm, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let outcome = arm(&tx, s, id, now, contexts)?;
        tx.commit()?;
        Ok(outcome)
    }

    /// ADR 0028 §12 (R12): commit an armed group park's or wake's evidence,
    /// each member on its own host's report.
    pub fn complete_group_residency(
        &self,
        s: &CoordinatorSession,
        id: &str,
        reports: &[MemberResidency],
        receipt: &str,
        now: i64,
    ) -> Result<(), LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        complete(&tx, s, id, reports, receipt, now)?;
        tx.commit()?;
        Ok(())
    }
}

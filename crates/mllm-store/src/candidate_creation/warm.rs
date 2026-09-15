//! Warm effects retain the candidate owner; completion consumes committed sources.
use super::*;
use mllm_domain::completion::{ExecutionIdentities, Milestone, StepExecutionContext};
use mllm_domain::resources::{PhaseFootprint, ResourcePhase};
use mllm_scheduler::residency::AdmissionContext;

// Validation is synchronous and transaction-local. Track active warm ancestors,
// not successful results: repeated sequential reads still validate every source.
thread_local! {
    static ACTIVE_PREDECESSORS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}
struct PredecessorVisit;
impl PredecessorVisit {
    fn enter(operation: &str) -> Result<Self, LifecycleError> {
        ACTIVE_PREDECESSORS.with(|active| {
            let mut active = active.borrow_mut();
            if active.len() >= 128 || active.iter().any(|id| id == operation) {
                return Err(LifecycleError::CorruptStoredData);
            }
            active.push(operation.into());
            Ok(Self)
        })
    }
}
impl Drop for PredecessorVisit {
    fn drop(&mut self) {
        ACTIVE_PREDECESSORS.with(|active| {
            active.borrow_mut().pop();
        });
    }
}

pub(super) fn facts(effect: PersistedEffectKind) -> Result<Vec<Fact>, LifecycleError> {
    Ok(match effect {
        PersistedEffectKind::Initialize => vec![
            Fact::AllocationsRestored,
            Fact::WeightsUsable,
            Fact::CacheValid,
        ],
        PersistedEffectKind::Drain => vec![Fact::Quiesced],
        PersistedEffectKind::Park => vec![Fact::MemoryReleased],
        PersistedEffectKind::Restore => vec![Fact::AllocationsRestored],
        PersistedEffectKind::ReloadWeights => vec![Fact::WeightsUsable],
        PersistedEffectKind::InvalidateCache => vec![Fact::CacheValid],
        PersistedEffectKind::Probe => return Err(LifecycleError::Unsupported),
    })
}
pub(super) fn milestone(f: &Fact) -> Milestone {
    match f {
        Fact::Quiesced => Milestone::Quiesced,
        Fact::MemoryReleased => Milestone::MemoryReleased,
        Fact::AllocationsRestored => Milestone::AllocationsRestored,
        Fact::WeightsUsable => Milestone::WeightsUsable,
        Fact::CacheValid => Milestone::CacheValid,
        Fact::ModelUsable => Milestone::ModelUsable,
    }
}
fn kinds(action: Action) -> Result<Vec<PersistedEffectKind>, LifecycleError> {
    use PersistedEffectKind::*;
    Ok(match action {
        Action::Initialize => vec![Initialize, Probe],
        Action::Park => vec![Drain, Park],
        Action::Restore => vec![Restore, ReloadWeights, InvalidateCache, Probe],
        Action::Security => return Err(LifecycleError::Invalid),
    })
}
pub(super) fn validate_specs(p: &CandidateActionPlanV3) -> Result<(), LifecycleError> {
    let expected = kinds(p.action)?;
    let mut ids = std::collections::BTreeSet::from([p.scope.parent_step_id.clone()]);
    let mut prior_facts = Vec::new();
    if p.effects.len() != expected.len() {
        return Err(LifecycleError::CorruptStoredData);
    }
    for (i, (spec, kind)) in p.effects.iter().zip(expected).enumerate() {
        let probe = kind == PersistedEffectKind::Probe;
        if spec.effect != kind
            || spec.ordinal != i as u32 + 1
            || spec.predecessor != i.checked_sub(1).map(|j| p.effects[j].step_id.clone())
            || spec.required_facts != prior_facts
            || spec.request_case
                != if probe {
                    p.ready_probe_case.clone()
                } else {
                    None
                }
            || spec.request_item != if probe { Some(0) } else { None }
            || spec.request_subcheck != if probe { Some(String::new()) } else { None }
            || spec.deadline_ms != p.deadline_ms
            || !super::super::ulid(&spec.step_id)
            || !ids.insert(spec.step_id.clone())
        {
            return Err(LifecycleError::CorruptStoredData);
        }
        if !probe {
            prior_facts.extend(facts(kind)?);
        }
    }
    Ok(())
}
pub(super) fn cold(
    tx: &Transaction<'_>,
    run: &str,
) -> Result<CandidateActionPlanV3, LifecycleError> {
    let (anchor,operation,case):(String,String,String)=tx.query_row("SELECT a.step_id,a.operation_id,a.case_id FROM qualification_case_actions a JOIN lifecycle_runs r ON r.operation_id=a.operation_id WHERE a.run_id=?1 AND r.action='activate' ORDER BY a.rowid LIMIT 1",[run],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    let p = plan_for_step(tx, &anchor)?;
    if p.action != Action::Initialize
        || p.case_kind != CandidateCaseKind::ColdInitialize
        || p.cycle != 0
        || p.case_id != case
        || p.scope.run_id != run
        || p.scope.parent_step_id != anchor
        || p.scope.operation_id != operation
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok(p)
}
pub(super) fn owned(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
) -> Result<crate::lifecycle::completion::OwnedLaunchAssociationV1, LifecycleError> {
    owned_read(tx,p,&ReadValidation::new(tx))
}

pub(super) fn owned_read(tx:&Transaction<'_>,p:&CandidateActionPlanV3,read:&ReadValidation<'_, '_>)->Result<crate::lifecycle::completion::OwnedLaunchAssociationV1,LifecycleError> {
    let source = if p.action == Action::Initialize {
        p.clone()
    } else {
        cold(tx, &p.scope.run_id)?
    };
    let v = anchor_context_read(tx, &source.scope.parent_step_id,false,read)?;
    crate::lifecycle::completion::association(tx, &v)?.ok_or(LifecycleError::Conflict)
}
pub(super) fn predecessors(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
    spec: &EffectSpec,
    observed: i64,
) -> Result<(), LifecycleError> {
    let mut collected = Vec::new();
    let mut previous_time = p.accepted_at_ms;
    let mut previous_epoch = 0;
    for child in p.effects.iter().take(spec.ordinal as usize - 1) {
        let (raw,epoch):(String,u64)=tx.query_row("SELECT e.evidence_json,e.committed_epoch FROM lifecycle_evidence e JOIN lifecycle_steps s ON s.id=e.step_id WHERE s.id=?1 AND s.state='completed'",[&child.step_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
        let e: EffectEvidenceV3 = decode(&raw)?;
        if e.scope != p.scope
            || e.effect != *child
            || e.facts != facts(child.effect)?
            || e.observed_at_ms < previous_time
            || e.observed_at_ms > observed
            || epoch <= previous_epoch
        {
            return Err(LifecycleError::Conflict);
        }
        previous_time = e.observed_at_ms;
        previous_epoch = epoch;
        collected.extend(e.facts);
    }
    if collected != spec.required_facts {
        return Err(LifecycleError::Conflict);
    }
    Ok(())
}
pub(super) fn execution(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
    spec: &EffectSpec,
    e: &ArmedEffectV3,
) -> Result<StepExecutionContext, LifecycleError> {
    let members = crate::lifecycle::completion::members(&owned(tx, p)?.identities)?;
    Ok(StepExecutionContext {
        token: child_token(p, &spec.step_id),
        binding_id: p.scope.binding_id.clone(),
        incarnation: p.scope.incarnation.clone(),
        issued_at_ms: e.issued_at_ms,
        deadline_ms: p.deadline_ms,
        identities: ExecutionIdentities::Retained(members),
        completion_target: None,
        grant_id: Some(e.grant_id.clone()),
        launch_settings: None,
    })
}

// Keep the owner's original conservative phase while joining every required peak.
// Original grant payloads stay immutable; later increases cannot invalidate history.
fn join(base: &mut PhaseFootprint, peak: &PhaseFootprint) -> Result<(), LifecycleError> {
    for next in &peak.allocations {
        let old = base
            .allocations
            .iter_mut()
            .find(|a| a.domain == next.domain)
            .ok_or(LifecycleError::CorruptStoredData)?;
        old.bytes = old.bytes.max(next.bytes);
        old.host_kv_bytes = old.host_kv_bytes.max(next.host_kv_bytes);
    }
    for next in &peak.devices {
        if let Some(old) = base.devices.iter_mut().find(|d| d.device == next.device) {
            if next.sharing == mllm_domain::resources::Sharing::Exclusive {
                old.sharing = next.sharing;
            }
        } else {
            base.devices.push(next.clone());
        }
    }
    base.devices.sort_by(|a, b| a.device.cmp(&b.device));
    Ok(())
}
pub(super) fn reservation_at(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
    epoch: u64,
) -> Result<PhaseFootprint, LifecycleError> {
    let snapshot = super::super::read_snapshot(tx, &p.scope.principal, &p.scope.run_id)
        .map_err(creation_error)?
        .ok_or(LifecycleError::CorruptStoredData)?;
    let resources = snapshot.reviewed_manifest().effective_recipe().resources();
    let mut retained = super::super::initialize::footprint(resources.cold(), ResourcePhase::Cold)?;
    let mut stmt=tx.prepare("SELECT r.plan_json FROM resource_grants g JOIN lifecycle_runs r ON r.operation_id=g.operation_id JOIN qualification_case_actions a ON a.operation_id=r.operation_id WHERE a.run_id=?1 AND g.committed_epoch<=?2 ORDER BY g.committed_epoch")?;
    let plans = stmt
        .query_map(params![p.scope.run_id, epoch], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for raw in plans {
        let plan: CandidateActionPlanV3 = decode(&raw)?;
        if plan.scope.run_id != p.scope.run_id || plan.scope.descriptor != p.scope.descriptor {
            return Err(LifecycleError::CorruptStoredData);
        }
        match plan.action {
            Action::Park => join(
                &mut retained,
                &super::super::initialize::footprint(resources.parking(), ResourcePhase::Parking)?,
            )?,
            Action::Restore => join(
                &mut retained,
                &super::super::initialize::footprint(resources.wake(), ResourcePhase::Wake)?,
            )?,
            _ => {}
        }
    }
    Ok(retained)
}
fn next_reservation(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
) -> Result<PhaseFootprint, LifecycleError> {
    let ledger = crate::resource_ledger::read_snapshot(tx).map_err(resource_error)?;
    let mut retained = ledger
        .owners
        .get(&p.scope.deployment_id)
        .ok_or(LifecycleError::Conflict)?
        .clone();
    let snapshot = super::super::read_snapshot(tx, &p.scope.principal, &p.scope.run_id)
        .map_err(creation_error)?
        .ok_or(LifecycleError::Conflict)?;
    let r = snapshot.reviewed_manifest().effective_recipe().resources();
    let peak = match p.action {
        Action::Park => super::super::initialize::footprint(r.parking(), ResourcePhase::Parking)?,
        Action::Restore => super::super::initialize::footprint(r.wake(), ResourcePhase::Wake)?,
        _ => return Err(LifecycleError::Conflict),
    };
    join(&mut retained, &peak)?;
    Ok(retained)
}
pub(super) fn prior(tx: &Transaction<'_>, p: &CandidateActionPlanV3) -> Result<(), LifecycleError> {
    prior_read(tx,p,&ReadValidation::new(tx))
}
pub(super) fn prior_read(tx:&Transaction<'_>,p:&CandidateActionPlanV3,read:&ReadValidation<'_, '_>)->Result<(),LifecycleError> {
    let _visit = PredecessorVisit::enter(&p.scope.operation_id)?;
    let snapshot = super::super::read_snapshot(tx, &p.scope.principal, &p.scope.run_id)
        .map_err(creation_error)?
        .ok_or(LifecycleError::Conflict)?;
    let cases = snapshot.reviewed_manifest().cases();
    let index = cases
        .iter()
        .position(|c| c.id() == p.case_id)
        .ok_or(LifecycleError::Conflict)?;
    let previous = cases
        .get(index.checked_sub(1).ok_or(LifecycleError::Conflict)?)
        .ok_or(LifecycleError::Conflict)?;
    if p.action == Action::Park && previous.kind() == CandidateCaseKind::MarkerStreaming {
        let source_index = index
            .checked_sub(4)
            .ok_or(LifecycleError::CorruptStoredData)?;
        let ready_case = &cases[index - 3];
        if cases[source_index].kind() != CandidateCaseKind::Restore
            || ready_case.kind() != CandidateCaseKind::ReadyProbe
            || ready_case.cycle() + 1 != p.cycle
        {
            return Err(LifecycleError::CorruptStoredData);
        }
        let anchor: String = tx.query_row(
            "SELECT parent_step_id FROM qualification_ready_probes WHERE run_id=?1 AND case_id=?2",
            params![p.scope.run_id, cases[index - 3].id()],
            |r| r.get(0),
        )?;
        let ready = exact_predecessor(tx, p, &cases[source_index], &anchor)?;
        if ready.ready_probe_case.as_deref() != Some(ready_case.id()) {
            return Err(LifecycleError::CorruptStoredData);
        }
        return inference::markers::baseline_read(tx, &ready,read);
    }
    let anchor: String = tx.query_row(
        "SELECT step_id FROM qualification_case_actions WHERE run_id=?1 AND case_id=?2",
        params![p.scope.run_id, previous.id()],
        |r| r.get(0),
    )?;
    if !matches!(
        (p.action, previous.kind()),
        (Action::Restore, CandidateCaseKind::Park) | (Action::Park, CandidateCaseKind::Security)
    ) {
        return Err(LifecycleError::CorruptStoredData);
    }
    let previous = exact_predecessor(tx, p, previous, &anchor)?;
    validate_plan_read(tx, &previous, false,read)?;
    let state: String = tx.query_row(
        "SELECT state FROM lifecycle_steps WHERE id=?1",
        [anchor],
        |r| r.get(0),
    )?;
    if state != "completed"
        || (p.action == Action::Restore && previous.action != Action::Park)
        || (p.action == Action::Park && previous.action != Action::Security)
    {
        return Err(LifecycleError::Conflict);
    }
    Ok(())
}

// This edge is checked before recursion. The caller selects an exact strictly
// earlier manifest index, so a linked FK alone cannot redefine ancestry.
fn exact_predecessor(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
    case: &mllm_config::effective::candidate::CandidateCase,
    anchor: &str,
) -> Result<CandidateActionPlanV3, LifecycleError> {
    let previous = plan_for_step(tx, anchor)?;
    let action = match case.kind() {
        CandidateCaseKind::Security => Action::Security,
        CandidateCaseKind::Park => Action::Park,
        CandidateCaseKind::Restore => Action::Restore,
        _ => return Err(LifecycleError::CorruptStoredData),
    };
    let mut expected = p.scope.clone();
    expected.operation_id = previous.scope.operation_id.clone();
    expected.parent_step_id = anchor.into();
    let linked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM qualification_case_actions a JOIN lifecycle_steps s ON s.id=a.step_id WHERE a.run_id=?1 AND a.case_id=?2 AND a.operation_id=?3 AND a.step_id=?4 AND s.operation_id=a.operation_id AND s.ordinal=0)",params![p.scope.run_id,case.id(),previous.scope.operation_id,anchor],|r|r.get(0))?;
    if previous.scope != expected
        || previous.scope.operation_id == p.scope.operation_id
        || previous.case_id != case.id()
        || previous.case_kind != case.kind()
        || previous.cycle != case.cycle()
        || previous.action != action
        || previous.accepted_at_ms > p.accepted_at_ms
        || !linked
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok(previous)
}
#[allow(clippy::too_many_arguments)]
pub(super) fn accept(
    tx: &Transaction<'_>,
    session: &CoordinatorSession,
    snapshot: &super::super::CandidateRunSnapshot,
    command: &Command,
    principal: &str,
    scope: &str,
    key: &str,
    hash: &str,
    now: i64,
) -> Result<CandidateActionPlanV3, LifecycleError> {
    let cold = cold(tx, snapshot.receipt().run_id())?;
    validate_plan(tx, &cold)?;
    let selected=snapshot.reviewed_manifest().cases().iter().find(|c| {
        matches!(c.kind(),CandidateCaseKind::Park|CandidateCaseKind::Restore) && !tx.query_row("SELECT EXISTS(SELECT 1 FROM qualification_case_actions WHERE run_id=?1 AND case_id=?2)",params![cold.scope.run_id,c.id()],|r|r.get::<_,bool>(0)).unwrap_or(true)
    }).ok_or(LifecycleError::Conflict)?;
    let action = match command.action {
        WireAction::Park => Action::Park,
        WireAction::Restore => Action::Restore,
        _ => return Err(LifecycleError::Conflict),
    };
    if selected.kind()
        != if action == Action::Park {
            CandidateCaseKind::Park
        } else {
            CandidateCaseKind::Restore
        }
        || now < cold.accepted_at_ms
        || command.deadline_ms <= now
        || command.deadline_ms > snapshot.receipt().deadline_ms()
    {
        return Err(LifecycleError::Conflict);
    }
    let mut s = cold.scope.clone();
    s.operation_id = ulid::Ulid::new().to_string();
    s.parent_step_id = ulid::Ulid::new().to_string();
    if s.session_id != session.id() {
        return Err(LifecycleError::Stale);
    }
    let ready = if action == Action::Restore {
        Some(
            snapshot
                .reviewed_manifest()
                .cases()
                .iter()
                .find(|c| {
                    c.kind() == CandidateCaseKind::ReadyProbe && c.cycle() == selected.cycle()
                })
                .ok_or(LifecycleError::Conflict)?
                .id()
                .to_owned(),
        )
    } else {
        None
    };
    let mut effects = Vec::new();
    let mut required = Vec::new();
    for kind in kinds(action)? {
        let probe = kind == PersistedEffectKind::Probe;
        effects.push(EffectSpec {
            step_id: ulid::Ulid::new().to_string(),
            ordinal: effects.len() as u32 + 1,
            effect: kind,
            predecessor: effects.last().map(|s: &EffectSpec| s.step_id.clone()),
            required_facts: required.clone(),
            request_case: if probe { ready.clone() } else { None },
            request_item: if probe { Some(0) } else { None },
            request_subcheck: if probe { Some(String::new()) } else { None },
            deadline_ms: command.deadline_ms,
        });
        if !probe {
            required.extend(facts(kind)?);
        }
    }
    let resources = snapshot.reviewed_manifest().effective_recipe().resources();
    let p = CandidateActionPlanV3 {
        version: 3,
        scope: s,
        action,
        case_id: selected.id().into(),
        case_kind: selected.kind(),
        cycle: selected.cycle(),
        accepted_at_ms: now,
        deadline_ms: command.deadline_ms,
        completion_target: if action == Action::Park {
            resources.parked().clone()
        } else {
            resources.ready().clone()
        },
        effects,
        ready_probe_case: ready,
    };
    prior(tx, &p)?;
    available(tx, &p)?;
    super::super::initialize::policy(tx, snapshot)?;
    let s = &p.scope;
    tx.execute("INSERT INTO operations(id,deployment_id,kind,state) VALUES(?1,?2,'candidate_action_v3','pending')",params![s.operation_id,s.deployment_id])?;
    tx.execute(
        "INSERT INTO lifecycle_runs VALUES(?1,?2,?3,?4,?5,?6,'queued',?7,?8)",
        params![
            s.operation_id,
            s.deployment_id,
            s.revision,
            s.generation,
            s.session_id,
            if action == Action::Park {
                "park"
            } else {
                "activate"
            },
            p.deadline_ms,
            encode(&p)?
        ],
    )?;
    tx.execute(
        "INSERT INTO lifecycle_claims VALUES(?1,?2,?3,?4)",
        params![s.deployment_id, s.operation_id, s.revision, s.generation],
    )?;
    tx.execute("INSERT INTO lifecycle_steps(id,operation_id,ordinal,deployment_id,binding_id,session_id,state,step_json) VALUES(?1,?2,0,?3,?4,?5,'planned',?6)",params![s.parent_step_id,s.operation_id,s.deployment_id,s.binding_id,s.session_id,encode(&PlannedActionV3 {version:3,kind:ActionTag::Planned,plan:p.clone()})?])?;
    for e in &p.effects {
        tx.execute("INSERT INTO lifecycle_steps(id,operation_id,ordinal,deployment_id,binding_id,session_id,state,step_json) VALUES(?1,?2,?3,?4,?5,?6,'planned',?7)",params![e.step_id,s.operation_id,e.ordinal,s.deployment_id,s.binding_id,s.session_id,encode(&PlannedEffectV3 {version:3,kind:EffectTag::Planned,parent:s.clone(),effect:e.clone()})?])?;
    }
    tx.execute(
        "INSERT INTO qualification_case_actions VALUES(?1,?2,?3,?4)",
        params![s.run_id, p.case_id, s.operation_id, s.parent_step_id],
    )?;
    if let Some(case) = &p.ready_probe_case {
        tx.execute(
            "INSERT INTO qualification_ready_probes VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                s.run_id,
                case,
                s.operation_id,
                s.parent_step_id,
                p.effects.last().unwrap().step_id,
                encode(&ProbeLinkV3 {
                    version: 3,
                    cycle: p.cycle,
                    descriptor: s.descriptor.clone()
                })?
            ],
        )?;
    }
    tx.execute(
        "INSERT INTO command_receipts VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            principal,
            scope,
            key,
            hash,
            s.operation_id,
            encode(&ReceiptV3 {
                version: 3,
                principal: principal.into(),
                scope: scope.into(),
                key: key.into(),
                request_hash: hash.into(),
                plan: p.clone()
            })?
        ],
    )?;
    Ok(p)
}
fn available(tx: &Transaction<'_>, p: &CandidateActionPlanV3) -> Result<(), LifecycleError> {
    let s = &p.scope;
    let yes:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM deployments d JOIN runtime_bindings b ON b.id=?1 JOIN qualification_runs q ON q.id=?2 WHERE d.id=?3 AND b.deployment_id=d.id AND d.revision=?4 AND d.current_generation=?5 AND d.desired_state='stopped' AND d.admission_enabled=0 AND d.dispatch_enabled=0 AND d.observed_state=?6 AND b.state='live' AND q.state='running' AND NOT EXISTS(SELECT 1 FROM request_leases WHERE deployment_id=d.id) AND NOT EXISTS(SELECT 1 FROM lifecycle_claims WHERE deployment_id=d.id))",params![s.binding_id,s.run_id,s.deployment_id,s.revision,s.generation,if p.action==Action::Park {"ready"} else {"parked"}],|r|r.get(0))?;
    if yes {
        Ok(())
    } else {
        Err(LifecycleError::Conflict)
    }
}
pub(super) fn arm(
    tx: &Transaction<'_>,
    session: &CoordinatorSession,
    p: &CandidateActionPlanV3,
    child: &str,
    context: AdmissionContext<'_>,
) -> Result<super::super::initialize::ArmResult, LifecycleError> {
    let spec = p
        .effects
        .iter()
        .find(|e| e.step_id == child)
        .ok_or(LifecycleError::Conflict)?;
    if spec.effect == PersistedEffectKind::Probe {
        return Err(LifecycleError::Unsupported);
    }
    let state: String = tx.query_row(
        "SELECT state FROM lifecycle_steps WHERE id=?1",
        [child],
        |r| r.get(0),
    )?;
    if matches!(state.as_str(), "armed" | "completed" | "uncertain") {
        return Ok(super::super::initialize::ArmResult::AlreadyRecorded);
    }
    current(tx, session, p)?;
    let snapshot = super::super::read_snapshot(tx, &p.scope.principal, &p.scope.run_id)
        .map_err(creation_error)?
        .ok_or(LifecycleError::Conflict)?;
    if snapshot.state() != super::super::CandidateRunState::Running
        || state != "planned"
        || context.now_ms < p.accepted_at_ms
        || context.now_ms >= p.deadline_ms
    {
        return Err(LifecycleError::Conflict);
    }
    let resource = super::super::initialize::policy(tx, &snapshot)?;
    let policy = read_candidate_policy(tx, &p.scope.host)
        .map_err(super::super::map_qualification)
        .map_err(creation_error)?
        .ok_or(LifecycleError::Conflict)?;
    let limits: Vec<_> = resource
        .controls
        .domains
        .iter()
        .map(|(d, p)| mllm_domain::resources::MemoryLimit {
            domain: d.clone(),
            managed_bytes: p.managed_limit,
            free_reserve_bytes: p.free_reserve,
            host_kv_bytes: p.host_kv_limit,
            parked_bytes: p.parked_limit,
        })
        .collect();
    let mut supplied = context.limits.to_vec();
    supplied.sort_by(|a, b| a.domain.cmp(&b.domain));
    if supplied != limits
        || context.ttl_ms != resource.controls.observation_ttl_ms
        || context.max_parked != resource.controls.max_parked as usize
        || resource.revision != p.scope.resource_policy_revision
        || policy.revision != p.scope.qualification_policy_revision
    {
        return Err(LifecycleError::Conflict);
    }
    let unknown: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM request_leases WHERE deployment_id=?1)",
        [&p.scope.deployment_id],
        |r| r.get(0),
    )?;
    if unknown {
        return Err(LifecycleError::Conflict);
    }
    predecessors(tx, p, spec, context.now_ms)?;
    let ledger = crate::resource_ledger::read_snapshot(tx).map_err(resource_error)?;
    let grant = if spec.ordinal == 1 {
        let id = ulid::Ulid::new().to_string();
        crate::resource_ledger::reserve_retained_candidate_in_transaction(
            tx,
            &crate::resource_ledger::GrantRequest {
                id: id.clone(),
                deployment_id: p.scope.deployment_id.clone(),
                operation_id: p.scope.operation_id.clone(),
                revision: p.scope.revision,
                generation: p.scope.generation,
                expected_epoch: ledger.epoch,
                next: next_reservation(tx, p)?,
            },
            context,
        )
        .map_err(resource_error)?;
        let anchor = ArmedActionV3 {
            version: 3,
            kind: ArmedActionTag::Armed,
            plan: p.clone(),
            grant_id: id.clone(),
            issued_at_ms: context.now_ms,
            expected_epoch: ledger.epoch,
        };
        one(tx.execute("UPDATE lifecycle_steps SET state='armed',step_json=?1,grant_id=?2 WHERE id=?3 AND state='planned'",params![encode(&anchor)?,id,p.scope.parent_step_id])?)?;
        one(tx.execute(
            "UPDATE lifecycle_runs SET state='running' WHERE operation_id=?1 AND state='queued'",
            [&p.scope.operation_id],
        )?)?;
        one(tx.execute(
            "UPDATE operations SET state='running' WHERE id=?1 AND state='pending'",
            [&p.scope.operation_id],
        )?)?;
        one(tx.execute(
            "UPDATE runtime_bindings SET state='uncertain' WHERE id=?1 AND state='live'",
            [&p.scope.binding_id],
        )?)?;
        one(tx.execute(
            "UPDATE deployments SET observed_state='uncertain' WHERE id=?1",
            [&p.scope.deployment_id],
        )?)?;
        id
    } else {
        current_anchor(tx, session, &p.scope.parent_step_id)?;
        let retained = ledger
            .owners
            .get(&p.scope.deployment_id)
            .ok_or(LifecycleError::Conflict)?;
        mllm_scheduler::residency::admit_phase(&ledger, &p.scope.deployment_id, retained, context)
            .map_err(|e| LifecycleError::Rejected(e.to_string()))?;
        let raw: String = tx.query_row(
            "SELECT step_json FROM lifecycle_steps WHERE id=?1",
            [&p.scope.parent_step_id],
            |r| r.get(0),
        )?;
        decode::<ArmedActionV3>(&raw)?.grant_id
    };
    let e = ArmedEffectV3 {
        version: 3,
        kind: ArmedEffectTag::Armed,
        parent: p.scope.clone(),
        effect: spec.clone(),
        grant_id: grant,
        issued_at_ms: context.now_ms,
    };
    one(tx.execute(
        "UPDATE lifecycle_steps SET state='armed',step_json=?1 WHERE id=?2 AND state='planned'",
        params![encode(&e)?, child],
    )?)?;
    validate_plan(tx, p)?;
    Ok(super::super::initialize::ArmResult::New {
        step_id: child.into(),
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum ParkTag {
    #[serde(rename = "candidate_park_aggregate")]
    Park,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ParkEvidence {
    version: u8,
    kind: ParkTag,
    scope: Scope,
    source_steps: Vec<String>,
    source_digests: Vec<String>,
    status_digest: String,
    origin: String,
    observed_at_ms: i64,
}
fn park_evidence(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
) -> Result<ParkEvidence, LifecycleError> {
    let mut sources = Vec::new();
    let mut at = 0;
    let owned = owned(tx, p)?;
    for spec in &p.effects {
        let raw: String = tx.query_row(
            "SELECT evidence_json FROM lifecycle_evidence WHERE step_id=?1",
            [&spec.step_id],
            |r| r.get(0),
        )?;
        let e: EffectEvidenceV3 = decode(&raw)?;
        if e.scope != p.scope
            || e.effect != *spec
            || e.identities != owned.identities
            || e.facts != facts(spec.effect)?
            || e.observed_at_ms < at
        {
            return Err(LifecycleError::CorruptStoredData);
        }
        at = e.observed_at_ms;
        sources.push(inference::digest(&e)?);
    }
    let status = read_status(tx, p)?.ok_or(LifecycleError::Conflict)?.0;
    if status.observed_at_ms < at {
        return Err(LifecycleError::Conflict);
    }
    Ok(ParkEvidence {
        version: 3,
        kind: ParkTag::Park,
        scope: p.scope.clone(),
        source_steps: p.effects.iter().map(|e| e.step_id.clone()).collect(),
        source_digests: sources,
        status_digest: inference::digest(&status)?,
        origin: crate::qualification::recipe_v1::PROGRAM_REVISION.into(),
        observed_at_ms: status.observed_at_ms,
    })
}
fn complete_park(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
    epoch: u64,
) -> Result<(), LifecycleError> {
    let aggregate = park_evidence(tx, p)?;
    tx.execute(
        "INSERT INTO lifecycle_evidence VALUES(?1,?2,?3)",
        params![p.scope.parent_step_id, encode(&aggregate)?, epoch],
    )?;
    one(tx.execute(
        "UPDATE lifecycle_steps SET state='completed' WHERE id=?1 AND state='armed'",
        [&p.scope.parent_step_id],
    )?)?;
    one(tx.execute(
        "UPDATE lifecycle_runs SET state='succeeded' WHERE operation_id=?1 AND state='running'",
        [&p.scope.operation_id],
    )?)?;
    one(tx.execute(
        "UPDATE operations SET state='succeeded' WHERE id=?1 AND state='running'",
        [&p.scope.operation_id],
    )?)?;
    one(tx.execute(
        "UPDATE runtime_bindings SET state='live' WHERE id=?1 AND state='uncertain'",
        [&p.scope.binding_id],
    )?)?;
    one(tx.execute(
        "UPDATE deployments SET observed_state='parked' WHERE id=?1",
        [&p.scope.deployment_id],
    )?)?;
    one(tx.execute(
        "DELETE FROM lifecycle_claims WHERE operation_id=?1",
        [&p.scope.operation_id],
    )?)?;
    inference::coverage(
        tx,
        p,
        &p.case_id,
        inference::SourceKind::LifecycleStep,
        &p.scope.parent_step_id,
        &inference::digest(&aggregate)?,
    )?;
    inference::coverage(
        tx,
        p,
        &p.case_id,
        inference::SourceKind::ParkedStatusObservation,
        &p.scope.parent_step_id,
        &aggregate.status_digest,
    )?;
    Ok(())
}
pub(super) fn validate_park(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
    states: &[&str],
    run: &str,
) -> Result<(), LifecycleError> {
    inference::spending(tx, p)?;
    if states[0] == "completed" {
        if states != ["completed", "completed", "completed"] || run != "succeeded" {
            return Err(LifecycleError::CorruptStoredData);
        }
        let raw: String = tx.query_row(
            "SELECT evidence_json FROM lifecycle_evidence WHERE step_id=?1",
            [&p.scope.parent_step_id],
            |r| r.get(0),
        )?;
        let aggregate = park_evidence(tx, p)?;
        if decode::<ParkEvidence>(&raw)? != aggregate {
            return Err(LifecycleError::CorruptStoredData);
        }
        let expected = [
            (
                inference::SourceKind::LifecycleStep,
                inference::digest(&aggregate)?,
            ),
            (
                inference::SourceKind::ParkedStatusObservation,
                aggregate.status_digest.clone(),
            ),
        ];
        inference::validate_case_coverage(tx, p, &p.case_id, &expected, &p.scope.parent_step_id)?;
        let epoch: u64 = tx.query_row(
            "SELECT committed_epoch FROM lifecycle_evidence WHERE step_id=?1",
            [&p.scope.parent_step_id],
            |r| r.get(0),
        )?;
        if read_status(tx, p)?
            .ok_or(LifecycleError::CorruptStoredData)?
            .1
            != epoch
        {
            return Err(LifecycleError::CorruptStoredData);
        }
    } else if run == "succeeded"
        || read_status(tx, p)?.is_some()
        || (states[0] == "planned"
            && (states != ["planned", "planned", "planned"] || run != "queued"))
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum StatusTag {
    #[serde(rename = "candidate_parked_status_observed")]
    Status,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusEvidence {
    version: u8,
    kind: StatusTag,
    scope: Scope,
    origin: String,
    identities: Vec<crate::lifecycle::IdentityDto>,
    observed_at_ms: i64,
    receipt: String,
    allocations: bool,
    weights: bool,
    cache: bool,
    quiesced: bool,
    unknown_work: bool,
    activity_before: (u64, u64, u64),
    activity_after: (u64, u64, u64),
}
fn read_status(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
) -> Result<Option<(StatusEvidence, u64)>, LifecycleError> {
    let raw:Option<(String,u64)>=tx.query_row("SELECT evidence_json,committed_epoch FROM qualification_parked_status WHERE parent_step_id=?1",[&p.scope.parent_step_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    raw.map(|(raw, epoch)| {
        let e: StatusEvidence = decode(&raw)?;
        if e.version != 3
            || e.scope != p.scope
            || e.origin != crate::qualification::recipe_v1::PROGRAM_REVISION
            || e.allocations
            || e.weights
            || e.cache
            || !e.quiesced
            || e.unknown_work
            || e.activity_before != e.activity_after
            || e.observed_at_ms < p.accepted_at_ms
            || e.observed_at_ms > p.deadline_ms
            || epoch
                > crate::resource_ledger::read_snapshot(tx)
                    .map_err(resource_error)?
                    .epoch
            || e.identities != owned(tx, p)?.identities
        {
            return Err(LifecycleError::CorruptStoredData);
        }
        crate::lifecycle::completion::members(&e.identities)?;
        crate::lifecycle::completion::nonempty_receipt(&e.receipt)?;
        Ok((e, epoch))
    })
    .transpose()
}
impl crate::Store {
    /// Local status context only. Contains no engine send permission or request ticket.
    pub fn candidate_parked_status_execution(
        &self,
        session: &CoordinatorSession,
        parent: &str,
        now: i64,
    ) -> Result<StepExecutionContext, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, session)?;
        let p = plan_for_step(&tx, parent)?;
        validate_plan(&tx, &p)?;
        current_anchor(&tx, session, parent)?;
        if p.action != Action::Park || now < p.accepted_at_ms || now >= p.deadline_ms {
            return Err(LifecycleError::Conflict);
        }
        let done: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM lifecycle_steps WHERE id=?1 AND state='completed')",
            [&p.effects[1].step_id],
            |r| r.get(0),
        )?;
        if !done {
            return Err(LifecycleError::Conflict);
        }
        let raw: String = tx.query_row(
            "SELECT step_json FROM lifecycle_steps WHERE id=?1",
            [&p.effects[1].step_id],
            |r| r.get(0),
        )?;
        let e: ArmedEffectV3 = decode(&raw)?;
        let mut c = execution(&tx, &p, &p.effects[1], &e)?;
        c.token = child_token(&p, parent);
        c.issued_at_ms = now;
        c.grant_id = None;
        Ok(c)
    }
    pub fn record_candidate_parked_status(
        &self,
        session: &CoordinatorSession,
        collector: &CandidateCollector,
        o: &mllm_domain::qualification::CandidateParkedStatusObservation,
        now: i64,
    ) -> Result<(), LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session)?;
        let p = plan_for_step(&tx, &o.token.step_id)?;
        validate_plan(&tx, &p)?;
        collector.validate(&p)?;
        if p.action != Action::Park
            || o.token != child_token(&p, &p.scope.parent_step_id)
            || o.binding_id != p.scope.binding_id
            || o.incarnation != p.scope.incarnation
        {
            return Err(LifecycleError::Conflict);
        }
        let identities = crate::lifecycle::completion::identity_dtos(
            &crate::lifecycle::completion::canonical_members(&o.identities)?,
        );
        if identities != owned(&tx, &p)?.identities
            || o.allocations
            || o.weights
            || o.cache
            || !o.quiesced
            || o.unknown_work
            || o.activity_before != o.activity_after
        {
            return Err(LifecycleError::Conflict);
        }
        crate::lifecycle::completion::nonempty_receipt(&o.receipt)?;
        let e = StatusEvidence {
            version: 3,
            kind: StatusTag::Status,
            scope: p.scope.clone(),
            origin: crate::qualification::recipe_v1::PROGRAM_REVISION.into(),
            identities,
            observed_at_ms: o.observed_at_ms,
            receipt: o.receipt.clone(),
            allocations: o.allocations,
            weights: o.weights,
            cache: o.cache,
            quiesced: o.quiesced,
            unknown_work: o.unknown_work,
            activity_before: o.activity_before,
            activity_after: o.activity_after,
        };
        if let Some((old, _)) = read_status(&tx, &p)? {
            return if old == e {
                Ok(())
            } else {
                Err(LifecycleError::Conflict)
            };
        }
        current_anchor(&tx, session, &p.scope.parent_step_id)?;
        let last = &p.effects[1];
        let (raw,state):(String,String)=tx.query_row("SELECT e.evidence_json,s.state FROM lifecycle_evidence e JOIN lifecycle_steps s ON s.id=e.step_id WHERE s.id=?1",[&last.step_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
        let park: EffectEvidenceV3 = decode(&raw)?;
        if state != "completed" || e.observed_at_ms < park.observed_at_ms {
            return Err(LifecycleError::Conflict);
        }
        crate::lifecycle::completion::fresh(
            park.observed_at_ms,
            p.deadline_ms,
            e.observed_at_ms,
            now,
            crate::lifecycle::completion::policy_ttl(&tx, &p.scope.host)?,
        )?;
        let work: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM request_leases WHERE deployment_id=?1)",
            [&p.scope.deployment_id],
            |r| r.get(0),
        )?;
        if work {
            return Err(LifecycleError::Conflict);
        }
        let anchor = validated_anchor(&tx, &p.scope.parent_step_id)?;
        let members = crate::lifecycle::completion::members(&e.identities)?;
        let mut milestones = Vec::new();
        for spec in &p.effects {
            let raw: String = tx.query_row(
                "SELECT evidence_json FROM lifecycle_evidence WHERE step_id=?1",
                [&spec.step_id],
                |r| r.get(0),
            )?;
            let source: EffectEvidenceV3 = decode(&raw)?;
            if source.scope != p.scope
                || source.effect != *spec
                || source.facts != facts(spec.effect)?
                || source.identities != e.identities
            {
                return Err(LifecycleError::CorruptStoredData);
            }
            milestones.extend(source.facts.iter().map(milestone));
        }
        mllm_domain::completion::verify_completion(
            &mllm_domain::completion::CompletionExpectation {
                token: anchor.context.token.clone(),
                identities: members.clone(),
                target: anchor
                    .context
                    .completion_target
                    .ok_or(LifecycleError::CorruptStoredData)?,
                issued_at_ms: anchor.context.issued_at_ms,
                deadline_ms: p.deadline_ms,
            },
            &mllm_domain::completion::CompletionEvidence {
                token: anchor.context.token,
                identities: members,
                observed_at_ms: e.observed_at_ms,
                control_receipt: Some(inference::digest(&e)?),
                milestones,
            },
            now,
            crate::lifecycle::completion::policy_ttl(&tx, &p.scope.host)?,
        )
        .map_err(|e| LifecycleError::Rejected(e.to_string()))?;
        let epoch = crate::resource_ledger::advance_completion_epoch(&tx)?;
        tx.execute(
            "INSERT INTO qualification_parked_status VALUES(?1,?2,?3)",
            params![p.scope.parent_step_id, encode(&e)?, epoch],
        )?;
        complete_park(&tx, &p, epoch)?;
        crate::lifecycle::completion::event(
            &tx,
            session,
            &p.scope.operation_id,
            &p.scope.deployment_id,
            &p.scope.parent_step_id,
            crate::events::CandidateLifecycleTransition::ParkCompleted,
            Some(epoch),
        )?;
        validate_plan(&tx, &p)?;
        tx.commit()?;
        Ok(())
    }
}

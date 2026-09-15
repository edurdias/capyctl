use super::*;
use mllm_domain::qualification::{
    CandidateRequestObservation, CandidateResponseObservation, CandidateSecurityControlObservation,
    CandidateSecurityEndpoint, CandidateTerminal,
};
#[derive(Debug)]
pub enum CandidateSecurityDispatch {
    NewControl(Box<CandidateSecurityControlDispatch>),
    NewRequest(Box<CandidateProbeDispatch>),
    AlreadyRecorded {
        operation_id: String,
        complete: bool,
    },
}
#[derive(Debug)]
pub struct CandidateSecurityControlDispatch {
    context: StepExecutionContext,
}
impl CandidateSecurityControlDispatch {
    pub fn context(&self) -> &StepExecutionContext {
        &self.context
    }
}
impl crate::Store {
    /// Internal coordinator progression. There is deliberately no Security wire action.
    pub fn advance_candidate_security(
        &self,
        session: &CoordinatorSession,
        collector: &CandidateCollector,
        context: AdmissionContext<'_>,
    ) -> Result<CandidateSecurityDispatch, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session)?;
        let cold = source(&tx, &collector.run)?;
        validate_plan(&tx, &cold)?;
        collector.validate(&cold)?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT step_id FROM qualification_case_actions WHERE run_id=?1 AND case_id=?2",
                params![collector.run, security_case(&tx, &cold)?],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(anchor) = existing {
            let p = plan_for_step(&tx, &anchor)?;
            validate_plan(&tx, &p)?;
            let parent_state: String = tx.query_row(
                "SELECT state FROM lifecycle_steps WHERE id=?1",
                [&p.scope.parent_step_id],
                |r| r.get(0),
            )?;
            if matches!(parent_state.as_str(), "uncertain" | "cancelled") {
                return Ok(CandidateSecurityDispatch::AlreadyRecorded {
                    operation_id: p.scope.operation_id,
                    complete: false,
                });
            }
            for (index, effect) in p.effects.iter().enumerate() {
                let state: String = tx.query_row(
                    "SELECT state FROM lifecycle_steps WHERE id=?1",
                    [&effect.step_id],
                    |r| r.get(0),
                )?;
                if state == "completed" {
                    continue;
                }
                if state != "planned" {
                    return Ok(CandidateSecurityDispatch::AlreadyRecorded {
                        operation_id: p.scope.operation_id,
                        complete: false,
                    });
                }
                current_security(&tx, session, &p, context)?;
                if index == 0 {
                    return Err(LifecycleError::CorruptStoredData);
                }
                let dispatch = arm_request(&tx, &p, index, context.now_ms)?;
                validate(&tx, &p)?;
                tx.commit()?;
                return Ok(CandidateSecurityDispatch::NewRequest(Box::new(dispatch)));
            }
            return Ok(CandidateSecurityDispatch::AlreadyRecorded {
                operation_id: p.scope.operation_id,
                complete: true,
            });
        }
        markers::baseline(&tx, &cold)?;
        markers::current_request(&tx, session, &cold, context)?;
        let v = validated_anchor(&tx, &cold.scope.parent_step_id)?;
        let mut scope = cold.scope.clone();
        scope.operation_id = ulid::Ulid::new().to_string();
        scope.parent_step_id = ulid::Ulid::new().to_string();
        let case = security_case(&tx, &cold)?;
        let mut effects = Vec::new();
        for index in 0..3 {
            effects.push(EffectSpec {
                step_id: ulid::Ulid::new().to_string(),
                ordinal: index + 1,
                effect: if index == 0 {
                    PersistedEffectKind::Park
                } else {
                    PersistedEffectKind::Probe
                },
                predecessor: effects.last().map(|e: &EffectSpec| e.step_id.clone()),
                required_facts: vec![],
                request_case: Some(case.clone()),
                request_item: if index == 0 { None } else { Some(0) },
                request_subcheck: Some(subcheck(index as usize).into()),
                deadline_ms: v.snapshot.receipt().deadline_ms(),
            });
        }
        let p = CandidateActionPlanV3 {
            version: 3,
            scope,
            action: Action::Security,
            case_id: case,
            case_kind: CandidateCaseKind::Security,
            cycle: 0,
            accepted_at_ms: context.now_ms,
            deadline_ms: v.snapshot.receipt().deadline_ms(),
            completion_target: cold.completion_target.clone(),
            effects,
            ready_probe_case: None,
        };
        let s = &p.scope;
        tx.execute("INSERT INTO operations(id,deployment_id,kind,state) VALUES(?1,?2,'candidate_action_v3','pending')",params![s.operation_id,s.deployment_id])?;
        tx.execute(
            "INSERT INTO lifecycle_runs VALUES(?1,?2,?3,?4,?5,'prepare','queued',?6,?7)",
            params![
                s.operation_id,
                s.deployment_id,
                s.revision,
                s.generation,
                s.session_id,
                p.deadline_ms,
                encode(&p)?
            ],
        )?;
        tx.execute(
            "INSERT INTO lifecycle_claims VALUES(?1,?2,?3,?4)",
            params![s.deployment_id, s.operation_id, s.revision, s.generation],
        )?;
        let ledger = crate::resource_ledger::read_snapshot(&tx).map_err(resource_error)?;
        let grant = ulid::Ulid::new().to_string();
        crate::resource_ledger::reserve_retained_candidate_in_transaction(
            &tx,
            &crate::resource_ledger::GrantRequest {
                id: grant.clone(),
                deployment_id: s.deployment_id.clone(),
                operation_id: s.operation_id.clone(),
                revision: s.revision,
                generation: s.generation,
                expected_epoch: ledger.epoch,
                next: ledger
                    .owners
                    .get(&s.deployment_id)
                    .ok_or(LifecycleError::Conflict)?
                    .clone(),
            },
            context,
        )
        .map_err(resource_error)?;
        let anchor = ArmedActionV3 {
            version: 3,
            kind: ArmedActionTag::Armed,
            plan: p.clone(),
            grant_id: grant.clone(),
            issued_at_ms: context.now_ms,
            expected_epoch: ledger.epoch,
        };
        tx.execute("INSERT INTO lifecycle_steps(id,operation_id,ordinal,deployment_id,binding_id,session_id,state,step_json,grant_id) VALUES(?1,?2,0,?3,?4,?5,'armed',?6,?7)",params![s.parent_step_id,s.operation_id,s.deployment_id,s.binding_id,s.session_id,encode(&anchor)?,grant])?;
        for (index, effect) in p.effects.iter().enumerate() {
            let json = if index == 0 {
                encode(&ArmedEffectV3 {
                    version: 3,
                    kind: ArmedEffectTag::Armed,
                    parent: s.clone(),
                    effect: effect.clone(),
                    grant_id: grant.clone(),
                    issued_at_ms: context.now_ms,
                })?
            } else {
                encode(&PlannedEffectV3 {
                    version: 3,
                    kind: EffectTag::Planned,
                    parent: s.clone(),
                    effect: effect.clone(),
                })?
            };
            tx.execute("INSERT INTO lifecycle_steps(id,operation_id,ordinal,deployment_id,binding_id,session_id,state,step_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![effect.step_id,s.operation_id,effect.ordinal,s.deployment_id,s.binding_id,s.session_id,if index==0{"armed"}else{"planned"},json])?;
        }
        tx.execute(
            "UPDATE operations SET state='running' WHERE id=?1",
            [&s.operation_id],
        )?;
        tx.execute(
            "UPDATE lifecycle_runs SET state='running' WHERE operation_id=?1",
            [&s.operation_id],
        )?;
        tx.execute(
            "INSERT INTO qualification_case_actions VALUES(?1,?2,?3,?4)",
            params![s.run_id, p.case_id, s.operation_id, s.parent_step_id],
        )?;
        let receipt = ReceiptV3 {
            version: 3,
            principal: s.principal.clone(),
            scope: accept_scope(&s.run_id),
            key: p.case_id.clone(),
            request_hash: digest(&(3_u8, &s.run_id, &p.case_id, "security"))?,
            plan: p.clone(),
        };
        tx.execute(
            "INSERT INTO command_receipts VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                receipt.principal,
                receipt.scope,
                receipt.key,
                receipt.request_hash,
                s.operation_id,
                encode(&receipt)?
            ],
        )?;
        validate_plan(&tx, &p)?;
        let execution = execution(&tx, &p, 0, context.now_ms)?;
        tx.commit()?;
        Ok(CandidateSecurityDispatch::NewControl(Box::new(
            CandidateSecurityControlDispatch { context: execution },
        )))
    }
    pub fn record_candidate_security_control(
        &self,
        session: &CoordinatorSession,
        collector: &CandidateCollector,
        observation: &CandidateSecurityControlObservation,
        now: i64,
    ) -> Result<(), LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session)?;
        let p = plan_for_step(&tx, &observation.effect.token.step_id)?;
        validate_plan(&tx, &p)?;
        collector.validate(&p)?;
        let o = &observation.effect;
        if o.token != child_token(&p, &p.effects[0].step_id)
            || o.binding_id != p.scope.binding_id
            || o.incarnation != p.scope.incarnation
            || !o.facts.is_empty()
        {
            return Err(LifecycleError::Conflict);
        }
        let e = observed(
            &tx,
            &p,
            0,
            None,
            &o.identities,
            o.observed_at_ms,
            &o.receipt,
            &observation.terminal,
            &observation.response,
        )?;
        record(&tx, session, &p, 0, &e, now)?;
        tx.commit()?;
        Ok(())
    }
}

fn subcheck(index: usize) -> &'static str {
    crate::qualification::recipe_v1::security_check(index)
        .expect("validated fixed Security index")
        .id
}
fn endpoint(index: usize) -> CandidateSecurityEndpoint {
    crate::qualification::recipe_v1::security_check(index)
        .expect("validated fixed Security index")
        .endpoint
}
fn endpoint_name(e: CandidateSecurityEndpoint) -> &'static str {
    match e {
        CandidateSecurityEndpoint::AdminControl => "admin_control",
        CandidateSecurityEndpoint::Inference => "inference",
        CandidateSecurityEndpoint::HealthGeneration => "health_generation",
    }
}
fn accept_scope(run: &str) -> String {
    format!("INTERNAL qualification Security {run}")
}
fn source(tx: &Transaction<'_>, run: &str) -> Result<CandidateActionPlanV3, LifecycleError> {
    let p = warm::cold(tx, run)?;
    validate_plan_inner(tx, &p, false)?;
    if p.action != Action::Initialize {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok(p)
}
fn security_case(
    tx: &Transaction<'_>,
    cold: &CandidateActionPlanV3,
) -> Result<String, LifecycleError> {
    let v = immutable_anchor(tx, &cold.scope.parent_step_id)?;
    let case = v
        .snapshot
        .reviewed_manifest()
        .cases()
        .get(4)
        .ok_or(LifecycleError::CorruptStoredData)?;
    if case.kind() != CandidateCaseKind::Security || case.cycle() != 0 {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok(case.id().into())
}
fn anchor(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
) -> Result<ArmedActionV3, LifecycleError> {
    let raw: String = tx.query_row(
        "SELECT step_json FROM lifecycle_steps WHERE id=?1",
        [&p.scope.parent_step_id],
        |r| r.get(0),
    )?;
    decode(&raw)
}
fn execution(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
    index: usize,
    issued: i64,
) -> Result<StepExecutionContext, LifecycleError> {
    let cold = source(tx, &p.scope.run_id)?;
    let v = validated_anchor(tx, &cold.scope.parent_step_id)?;
    let owned =
        crate::lifecycle::completion::association(tx, &v)?.ok_or(LifecycleError::Conflict)?;
    let mut context = v.context;
    context.token = child_token(p, &p.effects[index].step_id);
    context.issued_at_ms = issued;
    context.deadline_ms = p.deadline_ms;
    context.grant_id = Some(anchor(tx, p)?.grant_id);
    context.identities = mllm_domain::completion::ExecutionIdentities::Retained(
        crate::lifecycle::completion::members(&owned.identities)?,
    );
    context.completion_target = None;
    context.launch_settings = None;
    Ok(context)
}
fn current_security(
    tx: &Transaction<'_>,
    session: &CoordinatorSession,
    p: &CandidateActionPlanV3,
    context: AdmissionContext<'_>,
) -> Result<(), LifecycleError> {
    current(tx, session, p)?;
    let cold = source(tx, &p.scope.run_id)?;
    let v = validated_anchor(tx, &cold.scope.parent_step_id)?;
    let resource = super::super::super::initialize::policy(tx, &v.snapshot)?;
    let policy = read_candidate_policy(tx, &p.scope.host)
        .map_err(super::super::super::map_qualification)
        .map_err(creation_error)?
        .ok_or(LifecycleError::Conflict)?;
    let limits: Vec<_> = resource
        .controls
        .domains
        .iter()
        .map(|(domain, p)| mllm_domain::resources::MemoryLimit {
            domain: domain.clone(),
            managed_bytes: p.managed_limit,
            free_reserve_bytes: p.free_reserve,
            host_kv_bytes: p.host_kv_limit,
            parked_bytes: p.parked_limit,
        })
        .collect();
    let mut supplied = context.limits.to_vec();
    supplied.sort_by(|a, b| a.domain.cmp(&b.domain));
    if v.snapshot.state() != super::super::super::CandidateRunState::Running
        || supplied != limits
        || context.ttl_ms != resource.controls.observation_ttl_ms
        || context.max_parked != resource.controls.max_parked as usize
        || resource.revision != p.scope.resource_policy_revision
        || policy.revision != p.scope.qualification_policy_revision
        || context.now_ms < p.accepted_at_ms
        || context.now_ms >= p.deadline_ms
    {
        return Err(LifecycleError::Conflict);
    }
    let busy: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM request_leases WHERE deployment_id=?1)",
        [&p.scope.deployment_id],
        |r| r.get(0),
    )?;
    if busy {
        return Err(LifecycleError::Conflict);
    }
    let ledger = crate::resource_ledger::read_snapshot(tx).map_err(resource_error)?;
    mllm_scheduler::residency::admit_phase(
        &ledger,
        &p.scope.deployment_id,
        ledger
            .owners
            .get(&p.scope.deployment_id)
            .ok_or(LifecycleError::Conflict)?,
        context,
    )
    .map_err(|e| LifecycleError::Rejected(e.to_string()))?;
    Ok(())
}

fn arm_request(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
    index: usize,
    now: i64,
) -> Result<CandidateProbeDispatch, LifecycleError> {
    let cold = source(tx, &p.scope.run_id)?;
    let v = validated_anchor(tx, &cold.scope.parent_step_id)?;
    if v.snapshot.requests_used() >= v.snapshot.reviewed_manifest().limits().max_requests() {
        return Err(LifecycleError::Conflict);
    }
    let frozen = probe_request(p)?;
    let a = RequestAttemptV3 {
        version: 3,
        kind: AttemptTag::Security,
        scope: p.scope.clone(),
        case_id: p.case_id.clone(),
        item_ordinal: 0,
        subcheck_id: subcheck(index).into(),
        child_step_id: p.effects[index].step_id.clone(),
        request_operation_id: ulid::Ulid::new().to_string(),
        lease_id: ulid::Ulid::new().to_string(),
        command_scope: accept_scope(&p.scope.run_id),
        idempotency_key: format!("{}:{}", p.scope.operation_id, subcheck(index)),
        request_hash: digest(&(endpoint_name(endpoint(index)), &frozen))?,
        program_revision: crate::qualification::recipe_v1::PROGRAM_REVISION.into(),
        case_kind: CandidateCaseKind::Security,
        cycle: 0,
        issued_at_ms: now,
        deadline_ms: p.deadline_ms,
    };
    let s = &p.scope;
    let armed = ArmedEffectV3 {
        version: 3,
        kind: ArmedEffectTag::Armed,
        parent: s.clone(),
        effect: p.effects[index].clone(),
        grant_id: anchor(tx, p)?.grant_id,
        issued_at_ms: now,
    };
    one(tx.execute(
        "UPDATE lifecycle_steps SET state='armed',step_json=?1 WHERE id=?2 AND state='planned'",
        params![encode(&armed)?, a.child_step_id],
    )?)?;
    tx.execute("INSERT INTO operations(id,deployment_id,kind,state) VALUES(?1,?2,'candidate_security_v3','running')",params![a.request_operation_id,s.deployment_id])?;
    let raw = encode(&a)?;
    tx.execute(
        "INSERT INTO command_receipts VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            s.principal,
            a.command_scope,
            a.idempotency_key,
            a.request_hash,
            a.request_operation_id,
            raw
        ],
    )?;
    tx.execute(
        "INSERT INTO qualification_request_attempts VALUES(?1,?2,0,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        params![
            s.run_id,
            p.case_id,
            a.subcheck_id,
            a.request_operation_id,
            a.lease_id,
            s.principal,
            a.command_scope,
            a.idempotency_key,
            s.operation_id,
            a.child_step_id,
            raw
        ],
    )?;
    tx.execute(
        "INSERT INTO request_leases VALUES(?1,?2,?3,?4,?5,'inflight')",
        params![
            a.lease_id,
            s.deployment_id,
            s.revision,
            s.generation,
            s.session_id
        ],
    )?;
    one(tx.execute("UPDATE qualification_runs SET requests_used=requests_used+1 WHERE id=?1 AND requests_used=?2",params![s.run_id,v.snapshot.requests_used()])?)?;
    let context = execution(tx, p, index, now)?;
    Ok(CandidateProbeDispatch {
        ticket: DispatchTicket::candidate(
            a.lease_id,
            s.deployment_id.clone(),
            s.revision,
            s.generation,
            s.session_id.clone(),
        ),
        request_operation_id: a.request_operation_id,
        request: frozen,
        context,
        security_endpoint: Some(endpoint(index)),
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum SecurityTag {
    #[serde(rename = "candidate_security_observed")]
    Observed,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SecurityEvidence {
    version: u8,
    kind: SecurityTag,
    scope: Scope,
    effect: EffectSpec,
    attempt: Option<RequestAttemptV3>,
    origin: String,
    identities: Vec<crate::lifecycle::IdentityDto>,
    observed_at_ms: i64,
    receipt: String,
    terminal: Terminal,
    endpoint: String,
    status: u16,
    no_work: bool,
    separate_credentials: bool,
}
impl SecurityEvidence {
    fn passes(&self) -> bool {
        self.terminal == Terminal::Rejected
            && self
                .effect
                .ordinal
                .checked_sub(1)
                .and_then(|i| crate::qualification::recipe_v1::security_check(i as usize))
                .is_some_and(|check| {
                    self.status == check.status && self.endpoint == endpoint_name(check.endpoint)
                })
            && self.no_work
            && self.separate_credentials
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum SecurityAggregateTag {
    #[serde(rename = "candidate_security_aggregate")]
    Aggregate,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SecurityAggregate {
    version: u8,
    kind: SecurityAggregateTag,
    scope: Scope,
    origin: String,
    source_steps: Vec<String>,
    source_digests: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
fn observed(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
    index: usize,
    attempt: Option<RequestAttemptV3>,
    members: &[mllm_domain::completion::ProcessIdentity],
    at: i64,
    receipt: &str,
    terminal: &CandidateTerminal,
    response: &CandidateResponseObservation,
) -> Result<SecurityEvidence, LifecycleError> {
    let cold = source(tx, &p.scope.run_id)?;
    let v = validated_anchor(tx, &cold.scope.parent_step_id)?;
    let owned =
        crate::lifecycle::completion::association(tx, &v)?.ok_or(LifecycleError::Conflict)?;
    let identities = crate::lifecycle::completion::identity_dtos(
        &crate::lifecycle::completion::canonical_members(members)?,
    );
    if identities != owned.identities {
        return Err(LifecycleError::Conflict);
    }
    crate::lifecycle::completion::nonempty_receipt(receipt)?;
    let (actual, status, no_work, separate_credentials) = match response {
        CandidateResponseObservation::SecurityRejection {
            endpoint,
            status,
            no_work,
            separate_credentials,
        } => (
            endpoint_name(*endpoint),
            *status,
            *no_work,
            *separate_credentials,
        ),
        _ => return Err(LifecycleError::Invalid),
    };
    if actual != endpoint_name(endpoint(index)) {
        return Err(LifecycleError::Conflict);
    }
    Ok(SecurityEvidence {
        version: 3,
        kind: SecurityTag::Observed,
        scope: p.scope.clone(),
        effect: p.effects[index].clone(),
        attempt,
        origin: crate::qualification::recipe_v1::PROGRAM_REVISION.into(),
        identities,
        observed_at_ms: at,
        receipt: receipt.into(),
        terminal: match terminal {
            CandidateTerminal::RejectedWithoutWork => Terminal::Rejected,
            CandidateTerminal::Completed => Terminal::Completed,
            CandidateTerminal::FailedTerminal => Terminal::Failed,
            CandidateTerminal::Uncertain => Terminal::Uncertain,
        },
        endpoint: actual.into(),
        status,
        no_work,
        separate_credentials,
    })
}

pub(super) fn is_request(tx: &Transaction<'_>, operation: &str) -> Result<bool, LifecycleError> {
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1 AND kind='candidate_security_v3')",
        [operation],
        |r| r.get(0),
    )?)
}
pub(super) fn record_request(
    tx: &Transaction<'_>,
    session: &CoordinatorSession,
    collector: &CandidateCollector,
    o: &CandidateRequestObservation,
    now: i64,
) -> Result<(), LifecycleError> {
    let p = plan_for_step(tx, &o.token.step_id)?;
    validate_plan(tx, &p)?;
    collector.validate(&p)?;
    let index = p
        .effects
        .iter()
        .position(|s| s.step_id == o.token.step_id)
        .ok_or(LifecycleError::Conflict)?;
    if index == 0 {
        return Err(LifecycleError::Conflict);
    }
    let a = attempt_for(tx, &p, index)?.ok_or(LifecycleError::Conflict)?;
    if a.request_operation_id != o.request_operation_id
        || a.lease_id != o.lease_id
        || o.token != child_token(&p, &a.child_step_id)
        || o.binding_id != p.scope.binding_id
        || o.incarnation != p.scope.incarnation
    {
        return Err(LifecycleError::Conflict);
    }
    let e = observed(
        tx,
        &p,
        index,
        Some(a),
        &o.identities,
        o.observed_at_ms,
        &o.receipt,
        &o.terminal,
        &o.response,
    )?;
    record(tx, session, &p, index, &e, now)
}

fn read_evidence(
    tx: &Transaction<'_>,
    step: &str,
) -> Result<Option<(SecurityEvidence, u64)>, LifecycleError> {
    let row: Option<(String, u64)> = tx
        .query_row(
            "SELECT evidence_json,committed_epoch FROM lifecycle_evidence WHERE step_id=?1",
            [step],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    row.map(|(raw, epoch)| Ok((decode(&raw)?, epoch)))
        .transpose()
}
fn record(
    tx: &Transaction<'_>,
    session: &CoordinatorSession,
    p: &CandidateActionPlanV3,
    index: usize,
    e: &SecurityEvidence,
    now: i64,
) -> Result<(), LifecycleError> {
    if let Some((old, _)) = read_evidence(tx, &p.effects[index].step_id)? {
        return if old == *e {
            Ok(())
        } else {
            Err(LifecycleError::Conflict)
        };
    }
    current(tx, session, p)?;
    let cold = source(tx, &p.scope.run_id)?;
    let v = validated_anchor(tx, &cold.scope.parent_step_id)?;
    if v.snapshot.state() != super::super::super::CandidateRunState::Running {
        return Err(LifecycleError::Conflict);
    }
    super::super::super::initialize::policy(tx, &v.snapshot)?;
    let raw: String = tx.query_row(
        "SELECT step_json FROM lifecycle_steps WHERE id=?1",
        [&p.effects[index].step_id],
        |r| r.get(0),
    )?;
    let armed: ArmedEffectV3 = decode(&raw)?;
    crate::lifecycle::completion::fresh(
        armed.issued_at_ms,
        p.deadline_ms,
        e.observed_at_ms,
        now,
        crate::lifecycle::completion::policy_ttl(tx, &p.scope.host)?,
    )?;
    let epoch = crate::resource_ledger::advance_completion_epoch(tx)?;
    tx.execute(
        "INSERT INTO lifecycle_evidence VALUES(?1,?2,?3)",
        params![p.effects[index].step_id, encode(e)?, epoch],
    )?;
    if let Some(a) = &e.attempt {
        tx.execute(
            "INSERT INTO qualification_request_results VALUES(?1,?2,?3)",
            params![a.request_operation_id, encode(e)?, epoch],
        )?;
        // Only an actual rejected/no-work terminal proves settlement for a negative check.
        if e.terminal == Terminal::Rejected && e.no_work {
            one(tx.execute(
                "DELETE FROM request_leases WHERE id=?1 AND disposition='inflight'",
                [&a.lease_id],
            )?)?;
            one(tx.execute(
                "UPDATE operations SET state=?1 WHERE id=?2 AND state='running'",
                params![
                    if e.passes() { "succeeded" } else { "failed" },
                    a.request_operation_id
                ],
            )?)?;
        } else {
            one(tx.execute("UPDATE request_leases SET disposition='uncertain' WHERE id=?1 AND disposition='inflight'",[&a.lease_id])?)?;
        }
    }
    if e.passes() {
        one(tx.execute(
            "UPDATE lifecycle_steps SET state='completed' WHERE id=?1 AND state='armed'",
            [&p.effects[index].step_id],
        )?)?;
        coverage(
            tx,
            p,
            &p.case_id,
            if index == 0 {
                SourceKind::LifecycleStep
            } else {
                SourceKind::RequestAttempt
            },
            e.attempt
                .as_ref()
                .map_or(p.effects[index].step_id.as_str(), |a| {
                    a.request_operation_id.as_str()
                }),
            &digest(e)?,
        )?;
        if index == 2 {
            let aggregate = aggregate(tx, p)?;
            tx.execute(
                "INSERT INTO lifecycle_evidence VALUES(?1,?2,?3)",
                params![p.scope.parent_step_id, encode(&aggregate)?, epoch],
            )?;
            one(tx.execute(
                "UPDATE lifecycle_steps SET state='completed' WHERE id=?1 AND state='armed'",
                [&p.scope.parent_step_id],
            )?)?;
            one(tx.execute("UPDATE lifecycle_runs SET state='succeeded' WHERE operation_id=?1 AND state='running'",[&p.scope.operation_id])?)?;
            one(tx.execute(
                "UPDATE operations SET state='succeeded' WHERE id=?1 AND state='running'",
                [&p.scope.operation_id],
            )?)?;
            one(tx.execute(
                "DELETE FROM lifecycle_claims WHERE operation_id=?1",
                [&p.scope.operation_id],
            )?)?;
        }
    } else {
        tx.execute(
            "UPDATE lifecycle_steps SET state='uncertain' WHERE id IN (?1,?2)",
            params![p.scope.parent_step_id, p.effects[index].step_id],
        )?;
        tx.execute(
            "UPDATE lifecycle_runs SET state='uncertain' WHERE operation_id=?1",
            [&p.scope.operation_id],
        )?;
        tx.execute(
            "UPDATE runtime_bindings SET state='uncertain' WHERE id=?1",
            [&p.scope.binding_id],
        )?;
        one(tx.execute(
            "UPDATE qualification_runs SET state=?1 WHERE id=?2 AND state='running'",
            params![
                if e.terminal == Terminal::Rejected && e.no_work {
                    "failed"
                } else {
                    "uncertain"
                },
                p.scope.run_id
            ],
        )?)?;
    }
    validate(tx, p)?;
    Ok(())
}
fn aggregate(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
) -> Result<SecurityAggregate, LifecycleError> {
    let mut digests = Vec::new();
    for effect in &p.effects {
        let (e, _) =
            read_evidence(tx, &effect.step_id)?.ok_or(LifecycleError::CorruptStoredData)?;
        if !e.passes() {
            return Err(LifecycleError::Conflict);
        }
        digests.push(digest(&e)?);
    }
    Ok(SecurityAggregate {
        version: 3,
        kind: SecurityAggregateTag::Aggregate,
        scope: p.scope.clone(),
        origin: crate::qualification::recipe_v1::PROGRAM_REVISION.into(),
        source_steps: p.effects.iter().map(|e| e.step_id.clone()).collect(),
        source_digests: digests,
    })
}

pub(super) fn attempt_for(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
    index: usize,
) -> Result<Option<RequestAttemptV3>, LifecycleError> {
    let raw: Option<String> = tx
        .query_row(
            "SELECT receipt_json FROM qualification_request_attempts WHERE child_step_id=?1",
            [&p.effects[index].step_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(raw) = raw else { return Ok(None) };
    let a: RequestAttemptV3 = decode(&raw)?;
    if index == 0
        || a.version != 3
        || a.kind != AttemptTag::Security
        || a.scope != p.scope
        || a.case_id != p.case_id
        || a.item_ordinal != 0
        || a.subcheck_id != subcheck(index)
        || a.child_step_id != p.effects[index].step_id
        || a.command_scope != accept_scope(&p.scope.run_id)
        || a.idempotency_key != format!("{}:{}", p.scope.operation_id, subcheck(index))
        || a.request_hash != digest(&(endpoint_name(endpoint(index)), probe_request(p)?))?
        || a.program_revision != crate::qualification::recipe_v1::PROGRAM_REVISION
        || a.case_kind != CandidateCaseKind::Security
        || a.cycle != 0
        || a.issued_at_ms < p.accepted_at_ms
        || a.issued_at_ms >= p.deadline_ms
        || a.deadline_ms != p.deadline_ms
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM qualification_request_attempts a JOIN command_receipts c ON c.principal_id=a.principal_id AND c.command_scope=a.command_scope AND c.idempotency_key=a.idempotency_key JOIN operations o ON o.id=a.request_operation_id WHERE a.run_id=?1 AND a.case_id=?2 AND a.item_ordinal=0 AND a.subcheck_id=?3 AND a.request_operation_id=?4 AND a.lease_id=?5 AND a.principal_id=?6 AND a.command_scope=?7 AND a.idempotency_key=?8 AND a.parent_operation_id=?9 AND a.child_step_id=?10 AND c.operation_id=?4 AND c.request_hash=?11 AND c.response_json=a.receipt_json AND o.kind='candidate_security_v3' AND o.deployment_id=?12 AND o.idempotency_key IS NULL)",params![p.scope.run_id,p.case_id,a.subcheck_id,a.request_operation_id,a.lease_id,p.scope.principal,a.command_scope,a.idempotency_key,p.scope.operation_id,a.child_step_id,a.request_hash,p.scope.deployment_id],|r|r.get(0))?;
    if !valid {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok(Some(a))
}

pub(in super::super) fn validate(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
) -> Result<(), LifecycleError> {
    validate_read(tx,p,&ReadValidation::new(tx))
}
pub(in super::super) fn validate_read(tx:&Transaction<'_>,p:&CandidateActionPlanV3,read:&ReadValidation<'_, '_>)->Result<(),LifecycleError> {
    let bad = || LifecycleError::CorruptStoredData;
    let cold = source(tx, &p.scope.run_id)?;
    markers::baseline_read(tx, &cold,read)?;
    let v = anchor_context_read(tx, &cold.scope.parent_step_id,false,read)?;
    let mut expected = cold.scope.clone();
    expected.operation_id = p.scope.operation_id.clone();
    expected.parent_step_id = p.scope.parent_step_id.clone();
    if p.version != 3
        || p.action != Action::Security
        || p.scope != expected
        || p.case_id != security_case(tx, &cold)?
        || p.case_kind != CandidateCaseKind::Security
        || p.cycle != 0
        || p.effects.len() != 3
        || p.ready_probe_case.is_some()
        || p.completion_target != cold.completion_target
        || p.accepted_at_ms < cold.accepted_at_ms
        || p.accepted_at_ms >= p.deadline_ms
        || p.deadline_ms != v.snapshot.receipt().deadline_ms()
    {
        return Err(bad());
    }
    let s = &p.scope;
    let a = anchor(tx, p)?;
    if a.version != 3
        || a.plan != *p
        || a.issued_at_ms != p.accepted_at_ms
        || !super::super::super::ulid(&a.grant_id)
    {
        return Err(bad());
    }
    let (grant_raw,epoch):(String,u64)=tx.query_row("SELECT request_json,committed_epoch FROM resource_grants WHERE id=?1 AND deployment_id=?2 AND operation_id=?3",params![a.grant_id,s.deployment_id,s.operation_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
    let grant: (String, String, i64, i64, u64, String) = decode(&grant_raw)?;
    let ledger = crate::resource_ledger::read_snapshot(tx).map_err(resource_error)?;
    if grant.0 != s.deployment_id
        || grant.1 != s.operation_id
        || grant.2 != s.revision
        || grant.3 != s.generation
        || grant.4 != a.expected_epoch
        || epoch != a.expected_epoch + 1
        || crate::resource_ledger::decode(&grant.5).map_err(resource_error)?
            != warm::reservation_at(tx, p, epoch)?
    {
        return Err(bad());
    }
    let mut stmt=tx.prepare("SELECT id,ordinal,deployment_id,binding_id,session_id,state,step_json,grant_id FROM lifecycle_steps WHERE operation_id=?1 ORDER BY ordinal")?;
    let mut rows = stmt
        .query_map([&s.operation_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, u32>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, Option<String>>(7)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if rows.len() != 4 {
        return Err(bad());
    }
    let handoff = crate::lifecycle::candidate_handoff_states(tx, &s.run_id, &s.operation_id)?;
    let actual_run: String = tx.query_row(
        "SELECT state FROM lifecycle_runs WHERE operation_id=?1",
        [&s.operation_id],
        |r| r.get(0),
    )?;
    let cleanup_resolved = handoff.is_some() && actual_run == "failed";
    let session_changed: bool = tx.query_row(
        "SELECT session_id<>?1 FROM coordinator_session WHERE singleton=1",
        [&s.session_id],
        |r| r.get(0),
    )?;
    if let Some(history) = &handoff {
        if !matches!(actual_run.as_str(), "uncertain" | "failed") || history.len() != rows.len() {
            return Err(bad());
        }
        for (row, (id, original)) in rows.iter_mut().zip(history) {
            let expected = match original.as_str() {
                "completed" => "completed",
                "planned" => "cancelled",
                "armed" | "uncertain" => {
                    if cleanup_resolved {
                        "cancelled"
                    } else {
                        "uncertain"
                    }
                }
                _ => return Err(bad()),
            };
            if row.0 != *id || row.5 != expected {
                return Err(bad());
            }
            // Validate the original evidence against the exact frozen handoff;
            // the persisted cancelled child never becomes a successful source.
            row.5 = original.clone();
        }
    }
    // A later session cannot rewrite the original state frozen by an earlier
    // cleanup handoff. Distinguish that history from actual rollover uncertainty.
    let rollover = session_changed && (handoff.is_none() || rows[0].5 == "uncertain");
    let mut all = true;
    let mut failed = false;
    let mut previous_epoch = epoch;
    let mut coverage_expected = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if row.1 != index as u32
            || row.2 != s.deployment_id
            || row.3 != s.binding_id
            || row.4 != s.session_id
        {
            return Err(bad());
        }
        if index == 0 {
            if row.0 != s.parent_step_id
                || row.7.as_deref() != Some(a.grant_id.as_str())
                || !matches!(row.5.as_str(), "armed" | "completed" | "uncertain")
            {
                return Err(bad());
            }
            continue;
        }
        let i = index - 1;
        let effect = &p.effects[i];
        if row.0 != effect.step_id
            || row.7.is_some()
            || !super::super::super::ulid(&effect.step_id)
            || effect.ordinal != index as u32
            || effect.effect
                != if i == 0 {
                    PersistedEffectKind::Park
                } else {
                    PersistedEffectKind::Probe
                }
            || effect.predecessor != i.checked_sub(1).map(|j| p.effects[j].step_id.clone())
            || !effect.required_facts.is_empty()
            || effect.request_case.as_deref() != Some(p.case_id.as_str())
            || effect.request_item != if i == 0 { None } else { Some(0) }
            || effect.request_subcheck.as_deref() != Some(subcheck(i))
            || effect.deadline_ms != p.deadline_ms
        {
            return Err(bad());
        }
        let attempt = attempt_for(tx, p, i)?;
        if row.5 == "planned" {
            let child: PlannedEffectV3 = decode(&row.6)?;
            if child.version != 3
                || child.parent != *s
                || child.effect != *effect
                || i == 0
                || attempt.is_some()
                || read_evidence(tx, &effect.step_id)?.is_some()
            {
                return Err(bad());
            }
            all = false;
            continue;
        }
        let child: ArmedEffectV3 = decode(&row.6)?;
        if !all
            || child.version != 3
            || child.parent != *s
            || child.effect != *effect
            || child.grant_id != a.grant_id
            || child.issued_at_ms < p.accepted_at_ms
            || child.issued_at_ms >= p.deadline_ms
            || (i == 0 && child.issued_at_ms != a.issued_at_ms)
            || (i > 0
                && attempt
                    .as_ref()
                    .is_none_or(|a| a.issued_at_ms != child.issued_at_ms))
        {
            return Err(bad());
        }
        let evidence = read_evidence(tx, &effect.step_id)?;
        if let Some((e, committed)) = evidence {
            let owned = crate::lifecycle::completion::association(tx, &v)?.ok_or_else(bad)?;
            if e.version != 3
                || e.scope != *s
                || e.effect != *effect
                || e.attempt != attempt
                || e.origin != crate::qualification::recipe_v1::PROGRAM_REVISION
                || e.identities != owned.identities
                || e.observed_at_ms < child.issued_at_ms
                || e.observed_at_ms > p.deadline_ms
                || e.endpoint != endpoint_name(endpoint(i))
                || committed <= previous_epoch
                || committed > ledger.epoch
                || row.5 != if e.passes() { "completed" } else { "uncertain" }
            {
                return Err(bad());
            }
            crate::lifecycle::completion::nonempty_receipt(&e.receipt)?;
            previous_epoch = committed;
            if let Some(attempt) = &attempt {
                let (raw,request_epoch):(String,u64)=tx.query_row("SELECT evidence_json,committed_epoch FROM qualification_request_results WHERE request_operation_id=?1",[&attempt.request_operation_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
                if decode::<SecurityEvidence>(&raw)? != e || request_epoch != committed {
                    return Err(bad());
                }
                let lease: Option<String> = tx
                    .query_row(
                        "SELECT disposition FROM request_leases WHERE id=?1",
                        [&attempt.lease_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                let state: String = tx.query_row(
                    "SELECT state FROM operations WHERE id=?1",
                    [&attempt.request_operation_id],
                    |r| r.get(0),
                )?;
                let settled = e.terminal == Terminal::Rejected && e.no_work;
                if ((settled || cleanup_resolved) && lease.is_some())
                    || (!settled && !cleanup_resolved && lease.as_deref() != Some("uncertain"))
                    || state
                        != if e.passes() {
                            "succeeded"
                        } else if settled || cleanup_resolved {
                            "failed"
                        } else {
                            "running"
                        }
                {
                    return Err(bad());
                }
                if !settled && cleanup_resolved {
                    let resolved: bool = tx.query_row("SELECT error_code IS 'resolved_by_owned_cleanup' FROM operations WHERE id=?1", [&attempt.request_operation_id], |r| r.get(0))?;
                    if !resolved {
                        return Err(bad());
                    }
                }
            }
            if e.passes() {
                coverage_expected.push(CoverageV3 {
                    version: 3,
                    scope: s.clone(),
                    case_id: p.case_id.clone(),
                    source_kind: if i == 0 {
                        SourceKind::LifecycleStep
                    } else {
                        SourceKind::RequestAttempt
                    },
                    source_id: e
                        .attempt
                        .as_ref()
                        .map_or(effect.step_id.clone(), |a| a.request_operation_id.clone()),
                    source_digest: digest(&e)?,
                    origin: e.origin.clone(),
                });
            } else {
                all = false;
                failed = true;
            }
        } else {
            if row.5 != "armed" && !(row.5 == "uncertain" && session_changed) {
                return Err(bad());
            }
            if let Some(attempt) = &attempt {
                // Rollover changes the live lease even when an earlier cleanup
                // handoff froze an armed child. That envelope remains historical.
                let inflight:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM request_leases l JOIN operations o ON o.id=?1 WHERE l.id=?2 AND l.deployment_id=?3 AND l.revision=?4 AND l.generation=?5 AND l.session_id=?6 AND l.disposition=?7 AND o.state='running')",params![attempt.request_operation_id,attempt.lease_id,s.deployment_id,s.revision,s.generation,s.session_id,if session_changed {"uncertain"} else {"inflight"}],|r|r.get(0))?;
                let resolved: bool = cleanup_resolved && tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1 AND state='failed' AND error_code='resolved_by_owned_cleanup') AND NOT EXISTS(SELECT 1 FROM request_leases WHERE id=?2)", params![attempt.request_operation_id,attempt.lease_id], |r| r.get(0))?;
                if !inflight && !resolved {
                    return Err(bad());
                }
            }
            all = false;
        }
    }
    let mut stmt=tx.prepare("SELECT evidence_digest,metadata_json FROM qualification_evidence_refs WHERE run_id=?1 AND case_id=?2")?;
    let refs = stmt
        .query_map(params![s.run_id, p.case_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if refs.len() != coverage_expected.len() {
        return Err(bad());
    }
    for (hash, raw) in refs {
        let value: CoverageV3 = decode(&raw)?;
        if hash != digest(&value)? || !coverage_expected.contains(&value) {
            return Err(bad());
        }
    }
    let (run_state,operation_state):(String,String)=tx.query_row("SELECT r.state,o.state FROM lifecycle_runs r JOIN operations o ON o.id=r.operation_id WHERE r.operation_id=?1 AND r.deployment_id=?2 AND r.revision=?3 AND r.generation=?4 AND r.session_id=?5 AND r.action='prepare' AND r.deadline_ms=?6 AND r.plan_json=?7 AND o.kind='candidate_action_v3' AND o.deployment_id=?2 AND o.idempotency_key IS NULL",params![s.operation_id,s.deployment_id,s.revision,s.generation,s.session_id,p.deadline_ms,encode(p)?],|r|Ok((r.get(0)?,r.get(1)?)))?;
    if run_state
        != if cleanup_resolved {
            "failed"
        } else if handoff.is_some() {
            "uncertain"
        } else if all {
            "succeeded"
        } else if failed || rollover {
            "uncertain"
        } else {
            "running"
        }
        || operation_state
            != if cleanup_resolved {
                "failed"
            } else if all {
                "succeeded"
            } else {
                "running"
            }
        || rows[0].5
            != if all {
                "completed"
            } else if failed || rollover {
                "uncertain"
            } else {
                "armed"
            }
    {
        return Err(bad());
    }
    if cleanup_resolved {
        let resolved: bool = tx.query_row(
            "SELECT error_code IS 'resolved_by_owned_cleanup' FROM operations WHERE id=?1",
            [&s.operation_id],
            |r| r.get(0),
        )?;
        if !resolved {
            return Err(bad());
        }
    }
    let associated:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM qualification_case_actions WHERE run_id=?1 AND case_id=?2 AND operation_id=?3 AND step_id=?4)",params![s.run_id,p.case_id,s.operation_id,s.parent_step_id],|r|r.get(0))?;
    if !associated {
        return Err(bad());
    }
    let original = ReceiptV3 {
        version: 3,
        principal: s.principal.clone(),
        scope: accept_scope(&s.run_id),
        key: p.case_id.clone(),
        request_hash: digest(&(3_u8, &s.run_id, &p.case_id, "security"))?,
        plan: p.clone(),
    };
    let raw:String=tx.query_row("SELECT response_json FROM command_receipts WHERE principal_id=?1 AND command_scope=?2 AND idempotency_key=?3 AND request_hash=?4 AND operation_id=?5",params![s.principal,original.scope,original.key,original.request_hash,s.operation_id],|r|r.get(0))?;
    if decode::<ReceiptV3>(&raw)? != original {
        return Err(bad());
    }
    let parent_evidence: Option<(String, u64)> = tx
        .query_row(
            "SELECT evidence_json,committed_epoch FROM lifecycle_evidence WHERE step_id=?1",
            [&s.parent_step_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if all {
        let (raw, epoch) = parent_evidence.ok_or_else(bad)?;
        if decode::<SecurityAggregate>(&raw)? != aggregate(tx, p)? || epoch != previous_epoch {
            return Err(bad());
        }
    } else if parent_evidence.is_some() {
        return Err(bad());
    }
    spending(tx, p)?;
    Ok(())
}

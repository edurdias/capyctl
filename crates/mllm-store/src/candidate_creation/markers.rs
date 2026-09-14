//! Coordinator-selected corpus requests. The retained Initialize source owns identity;
//! marker requests own dispatch leases, never a lifecycle claim.
use super::*;
use mllm_domain::qualification::{
    CandidateRequestObservation, CandidateResponseObservation, CandidateTerminal,
};

#[derive(Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Request {
    model: String,
    messages: Vec<Message>,
    temperature: u32,
    max_tokens: u32,
    stream: bool,
}
#[derive(Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Message {
    role: String,
    content: String,
}
fn request(text: &str) -> Result<Request, LifecycleError> {
    if text.len() > 1048576 {
        return Err(LifecycleError::Invalid);
    }
    serde_json::from_str(text).map_err(|_| LifecycleError::Invalid)
}
fn command_scope(run: &str) -> String {
    format!("POST /management/v1/qualification-runs/{run}/inference")
}

pub(in super::super) fn baseline(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
) -> Result<(), LifecycleError> {
    let v = immutable_anchor(tx, &p.scope.parent_step_id)?;
    if v.state != "completed" {
        return Err(LifecycleError::Conflict);
    }
    for case in v
        .snapshot
        .reviewed_manifest()
        .cases()
        .iter()
        .skip_while(|c| Some(c.id()) != p.ready_probe_case.as_deref())
        .skip(1)
        .take(2)
    {
        for item in 0..case.count() {
            let op:Option<String>=tx.query_row("SELECT request_operation_id FROM qualification_request_attempts WHERE run_id=?1 AND case_id=?2 AND item_ordinal=?3 AND subcheck_id=''",params![p.scope.run_id,case.id(),item],|r|r.get(0)).optional()?;
            let (_, a) = load_attempt(tx, &op.ok_or(LifecycleError::Conflict)?)?;
            if !result(tx, p, &a)?.is_some_and(|e| e.passes()) {
                return Err(LifecycleError::Conflict);
            }
        }
    }
    Ok(())
}

impl crate::Store {
    pub fn grant_candidate_inference(
        &self,
        session: &CoordinatorSession,
        principal: &str,
        run: &str,
        key: &str,
        body: &str,
        context: AdmissionContext<'_>,
    ) -> Result<CandidateDispatchResult, LifecycleError> {
        if !super::super::super::valid_id(principal)
            || !super::super::super::valid_id(key)
            || !super::super::super::ulid(run)
        {
            return Err(LifecycleError::Invalid);
        }
        let supplied = request(body)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session)?;
        // Resolve the original command before selecting the next case or ordinal.
        let prior: Option<String> = tx.query_row("SELECT operation_id FROM command_receipts WHERE principal_id=?1 AND command_scope=?2 AND idempotency_key=?3", params![principal,command_scope(run),key], |r|r.get(0)).optional()?;
        if let Some(operation) = prior {
            let (p, a) = load_attempt(&tx, &operation)?;
            validate_plan(&tx, &p)?;
            result(&tx, &p, &a)?;
            let expected = request(&template(&tx, &p, &a)?)?;
            if supplied != expected
                || a.scope.principal != principal
                || a.scope.run_id != run
                || a.idempotency_key != key
            {
                return Err(LifecycleError::Conflict);
            }
            return Ok(CandidateDispatchResult::AlreadyRecorded {
                request_operation_id: operation,
            });
        }
        let anchor: Option<String> = tx.query_row("SELECT q.parent_step_id FROM qualification_ready_probes q JOIN lifecycle_steps s ON s.id=q.parent_step_id WHERE q.run_id=?1 AND s.state='completed' ORDER BY q.rowid DESC LIMIT 1",[run],|r|r.get(0)).optional()?;
        let p = plan_for_step(&tx, &anchor.ok_or(LifecycleError::Conflict)?)?;
        let v = validated_anchor(&tx, &p.scope.parent_step_id)?;
        if principal != p.scope.principal {
            return Err(LifecycleError::Conflict);
        }
        current_request(&tx, session, &p, context)?;
        let program = QualificationProgram::resolve(v.snapshot.reviewed_manifest())?;
        let mut selected = None;
        for case in v
            .snapshot
            .reviewed_manifest()
            .cases()
            .iter()
            .skip_while(|c| Some(c.id()) != p.ready_probe_case.as_deref())
            .skip(1)
        {
            if !matches!(
                case.kind(),
                CandidateCaseKind::MarkerNonstreaming | CandidateCaseKind::MarkerStreaming
            ) {
                break;
            }
            for item in 0..case.count() {
                let op: Option<String> = tx.query_row("SELECT request_operation_id FROM qualification_request_attempts WHERE run_id=?1 AND case_id=?2 AND item_ordinal=?3 AND subcheck_id=''",params![run,case.id(),item],|r|r.get(0)).optional()?;
                if let Some(op) = op {
                    let (_, a) = load_attempt(&tx, &op)?;
                    if !result(&tx, &p, &a)?.is_some_and(|e| e.passes()) {
                        return Err(LifecycleError::Conflict);
                    }
                } else {
                    selected = Some((case, item));
                    break;
                }
            }
            if selected.is_some() {
                break;
            }
        }
        let (case, item) = selected.ok_or(LifecycleError::Conflict)?;
        let frozen = program.marker_request(
            &p.scope.deployment_id,
            item,
            case.kind() == CandidateCaseKind::MarkerStreaming,
        )?;
        if supplied != request(&frozen)?
            || v.snapshot.requests_used() >= v.snapshot.reviewed_manifest().limits().max_requests()
            || item >= case.request_budget()
        {
            return Err(LifecycleError::Conflict);
        }
        let a = RequestAttemptV3 {
            version: 3,
            kind: AttemptTag::Marker,
            scope: p.scope.clone(),
            case_id: case.id().into(),
            item_ordinal: item,
            subcheck_id: String::new(),
            child_step_id: String::new(),
            request_operation_id: ulid::Ulid::new().to_string(),
            lease_id: ulid::Ulid::new().to_string(),
            command_scope: command_scope(run),
            idempotency_key: key.into(),
            request_hash: digest(&supplied)?,
            program_revision: crate::qualification::recipe_v1::PROGRAM_REVISION.into(),
            case_kind: case.kind(),
            cycle: case.cycle(),
            issued_at_ms: context.now_ms,
            deadline_ms: v.snapshot.receipt().deadline_ms(),
        };
        insert_attempt(&tx, &a, v.snapshot.requests_used())?;
        load_attempt(&tx, &a.request_operation_id)?;
        spending(&tx, &p)?;
        let owned = warm::owned(&tx, &p)?;
        let mut execution = v.context;
        execution.token = child_token(&p, &p.scope.parent_step_id);
        execution.issued_at_ms = a.issued_at_ms;
        execution.deadline_ms = a.deadline_ms;
        execution.identities = mllm_domain::completion::ExecutionIdentities::Retained(
            crate::lifecycle::completion::members(&owned.identities)?,
        );
        execution.completion_target = None;
        execution.launch_settings = None;
        tx.commit()?;
        Ok(CandidateDispatchResult::New(Box::new(
            CandidateProbeDispatch {
                ticket: DispatchTicket::candidate(
                    a.lease_id,
                    p.scope.deployment_id,
                    p.scope.revision,
                    p.scope.generation,
                    p.scope.session_id,
                ),
                request_operation_id: a.request_operation_id,
                request: frozen,
                context: execution,
                security_endpoint: None,
            },
        )))
    }
}

fn insert_attempt(
    tx: &Transaction<'_>,
    a: &RequestAttemptV3,
    used: u32,
) -> Result<(), LifecycleError> {
    let s = &a.scope;
    tx.execute("INSERT INTO operations(id,deployment_id,kind,state) VALUES(?1,?2,'candidate_marker_v3','running')",params![a.request_operation_id,s.deployment_id])?;
    let raw = encode(a)?;
    tx.execute("INSERT INTO command_receipts(principal_id,command_scope,idempotency_key,request_hash,operation_id,response_json) VALUES(?1,?2,?3,?4,?5,?6)",params![s.principal,a.command_scope,a.idempotency_key,a.request_hash,a.request_operation_id,raw])?;
    tx.execute("INSERT INTO qualification_request_attempts VALUES(?1,?2,?3,'',?4,?5,?6,?7,?8,NULL,NULL,?9)",params![s.run_id,a.case_id,a.item_ordinal,a.request_operation_id,a.lease_id,s.principal,a.command_scope,a.idempotency_key,raw])?;
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
    one(tx.execute("UPDATE qualification_runs SET requests_used=requests_used+1 WHERE id=?1 AND requests_used=?2",params![s.run_id,used])?)?;
    Ok(())
}

pub(super) fn current_request(
    tx: &Transaction<'_>,
    session: &CoordinatorSession,
    p: &CandidateActionPlanV3,
    context: AdmissionContext<'_>,
) -> Result<(), LifecycleError> {
    let v = validated_anchor(tx, &p.scope.parent_step_id)?;
    let s = &p.scope;
    if s.session_id != session.id() {
        return Err(LifecycleError::Stale);
    }
    let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM deployments d JOIN runtime_bindings b ON b.deployment_id=d.id JOIN qualification_runs q ON q.id=?1 WHERE d.id=?2 AND d.revision=?3 AND d.current_generation=?4 AND d.desired_state='stopped' AND d.admission_enabled=0 AND d.dispatch_enabled=0 AND d.observed_state='ready' AND b.id=?5 AND b.state='live' AND q.state='running' AND NOT EXISTS(SELECT 1 FROM lifecycle_claims WHERE deployment_id=d.id) AND NOT EXISTS(SELECT 1 FROM request_leases WHERE deployment_id=d.id))",params![s.run_id,s.deployment_id,s.revision,s.generation,s.binding_id],|r|r.get(0))?;
    if !valid
        || v.state != "completed"
        || context.now_ms < p.accepted_at_ms
        || context.now_ms >= v.snapshot.receipt().deadline_ms()
    {
        return Err(LifecycleError::Conflict);
    }
    let resource = super::super::super::initialize::policy(tx, &v.snapshot)?;
    let qualification = read_candidate_policy(tx, &s.host)
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
    if supplied != limits
        || context.ttl_ms != resource.controls.observation_ttl_ms
        || context.max_parked != resource.controls.max_parked as usize
        || resource.revision != s.resource_policy_revision
        || qualification.revision != s.qualification_policy_revision
    {
        return Err(LifecycleError::Conflict);
    }
    let ledger = crate::resource_ledger::read_snapshot(tx).map_err(resource_error)?;
    let retained = ledger
        .owners
        .get(&s.deployment_id)
        .ok_or(LifecycleError::Conflict)?;
    mllm_scheduler::residency::admit_phase(&ledger, &s.deployment_id, retained, context)
        .map_err(|e| LifecycleError::Rejected(e.to_string()))?;
    Ok(())
}

pub(super) fn is_marker(tx: &Transaction<'_>, operation: &str) -> Result<bool, LifecycleError> {
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1 AND kind='candidate_marker_v3')",
        [operation],
        |r| r.get(0),
    )?)
}
fn template(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
    a: &RequestAttemptV3,
) -> Result<String, LifecycleError> {
    let v = super::super::super::read_snapshot(tx, &p.scope.principal, &p.scope.run_id)
        .map_err(creation_error)?
        .ok_or(LifecycleError::CorruptStoredData)?;
    QualificationProgram::resolve(v.reviewed_manifest())?.marker_request(
        &p.scope.deployment_id,
        a.item_ordinal,
        a.case_kind == CandidateCaseKind::MarkerStreaming,
    )
}
pub(super) fn load_attempt(
    tx: &Transaction<'_>,
    operation: &str,
) -> Result<(CandidateActionPlanV3, RequestAttemptV3), LifecycleError> {
    let raw: String = tx.query_row(
        "SELECT receipt_json FROM qualification_request_attempts WHERE request_operation_id=?1",
        [operation],
        |r| r.get(0),
    )?;
    let a: RequestAttemptV3 = decode(&raw)?;
    let p = plan_for_step(tx, &a.scope.parent_step_id)?;
    let snapshot = super::super::super::read_snapshot(tx, &p.scope.principal, &p.scope.run_id)
        .map_err(creation_error)?
        .ok_or(LifecycleError::CorruptStoredData)?;
    let case = snapshot
        .reviewed_manifest()
        .cases()
        .iter()
        .find(|c| c.id() == a.case_id)
        .ok_or(LifecycleError::CorruptStoredData)?;
    if a.version != 3
        || a.kind != AttemptTag::Marker
        || a.scope != p.scope
        || a.request_operation_id != operation
        || a.command_scope != command_scope(&p.scope.run_id)
        || !a.child_step_id.is_empty()
        || !a.subcheck_id.is_empty()
        || a.program_revision != crate::qualification::recipe_v1::PROGRAM_REVISION
        || !matches!(
            a.case_kind,
            CandidateCaseKind::MarkerNonstreaming | CandidateCaseKind::MarkerStreaming
        )
        || case.kind() != a.case_kind
        || case.cycle() != a.cycle
        || a.cycle != p.cycle
        || !matches!(p.action, Action::Initialize | Action::Restore)
        || a.item_ordinal >= case.count()
        || a.issued_at_ms < p.accepted_at_ms
        || a.issued_at_ms >= a.deadline_ms
        || a.deadline_ms != snapshot.receipt().deadline_ms()
        || a.request_hash != digest(&request(&template(tx, &p, &a)?)?)?
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM qualification_request_attempts a JOIN command_receipts c ON c.principal_id=a.principal_id AND c.command_scope=a.command_scope AND c.idempotency_key=a.idempotency_key JOIN operations o ON o.id=a.request_operation_id WHERE a.run_id=?1 AND a.case_id=?2 AND a.item_ordinal=?3 AND a.subcheck_id='' AND a.request_operation_id=?4 AND a.lease_id=?5 AND a.principal_id=?6 AND a.command_scope=?7 AND a.idempotency_key=?8 AND a.parent_operation_id IS NULL AND a.child_step_id IS NULL AND c.operation_id=?4 AND c.request_hash=?9 AND c.response_json=a.receipt_json AND o.kind='candidate_marker_v3' AND o.deployment_id=?10 AND o.idempotency_key IS NULL)",params![a.scope.run_id,a.case_id,a.item_ordinal,a.request_operation_id,a.lease_id,a.scope.principal,a.command_scope,a.idempotency_key,a.request_hash,a.scope.deployment_id],|r|r.get(0))?;
    if !valid {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok((p, a))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum MarkerTag {
    #[serde(rename = "candidate_marker_observed")]
    Marker,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MarkerEvidence {
    version: u8,
    kind: MarkerTag,
    attempt: RequestAttemptV3,
    identities: Vec<crate::lifecycle::IdentityDto>,
    origin: String,
    observed_at_ms: i64,
    receipt: String,
    terminal: Terminal,
    model_matches: bool,
    output_matches: bool,
    finish_matches: bool,
    response_digest: String,
}
impl MarkerEvidence {
    fn passes(&self) -> bool {
        self.terminal == Terminal::Completed
            && self.model_matches
            && self.output_matches
            && self.finish_matches
    }
}

fn result(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
    a: &RequestAttemptV3,
) -> Result<Option<MarkerEvidence>, LifecycleError> {
    let row:Option<(String,u64)>=tx.query_row("SELECT evidence_json,committed_epoch FROM qualification_request_results WHERE request_operation_id=?1",[&a.request_operation_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let Some((raw, epoch)) = row else {
        return Ok(None);
    };
    let e: MarkerEvidence = decode(&raw)?;
    let lease: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM request_leases WHERE id=?1)",
        [&a.lease_id],
        |r| r.get(0),
    )?;
    let state: String = tx.query_row(
        "SELECT state FROM operations WHERE id=?1",
        [&a.request_operation_id],
        |r| r.get(0),
    )?;
    let owned = warm::owned(tx, p)?;
    let cleanup_resolved = e.terminal == Terminal::Uncertain && !lease && state == "failed";
    if cleanup_resolved {
        let cold = warm::cold(tx, &p.scope.run_id)?;
        let v = immutable_initialize_anchor(tx, &cold.scope.parent_step_id)?;
        super::super::super::cleanup::validate_gone_history(tx, &v)?;
        let exact: bool = tx.query_row(
            "SELECT error_code='resolved_by_owned_cleanup' FROM operations WHERE id=?1",
            [&a.request_operation_id],
            |r| r.get(0),
        )?;
        if !exact {
            return Err(LifecycleError::CorruptStoredData);
        }
    }
    if e.version != 3
        || e.attempt != *a
        || e.identities != owned.identities
        || e.origin != crate::qualification::recipe_v1::PROGRAM_REVISION
        || e.observed_at_ms < a.issued_at_ms
        || e.observed_at_ms > a.deadline_ms
        || e.response_digest.len() != 64
        || epoch
            > crate::resource_ledger::read_snapshot(tx)
                .map_err(resource_error)?
                .epoch
        || lease != (e.terminal == Terminal::Uncertain && !cleanup_resolved)
        || state
            != if e.passes() {
                "succeeded"
            } else if e.terminal == Terminal::Uncertain && !cleanup_resolved {
                "running"
            } else {
                "failed"
            }
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    crate::lifecycle::completion::nonempty_receipt(&e.receipt)?;
    let expected = CoverageV3 {
        version: 3,
        scope: p.scope.clone(),
        case_id: a.case_id.clone(),
        source_kind: SourceKind::RequestAttempt,
        source_id: a.request_operation_id.clone(),
        source_digest: digest(&e)?,
        origin: e.origin.clone(),
    };
    let mut stmt=tx.prepare("SELECT evidence_digest,metadata_json FROM qualification_evidence_refs WHERE run_id=?1 AND case_id=?2")?;
    let rows = stmt
        .query_map(params![p.scope.run_id, a.case_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut found = 0;
    for (hash, raw) in rows {
        let c: CoverageV3 = decode(&raw)?;
        if hash != digest(&c)? {
            return Err(LifecycleError::CorruptStoredData);
        }
        if c.source_id == a.request_operation_id {
            if c != expected {
                return Err(LifecycleError::CorruptStoredData);
            }
            found += 1;
        }
    }
    if found != usize::from(e.passes()) {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok(Some(e))
}

pub(super) fn record(
    tx: &Transaction<'_>,
    session: &CoordinatorSession,
    collector: &CandidateCollector,
    o: &CandidateRequestObservation,
    now: i64,
) -> Result<(), LifecycleError> {
    let (p, a) = load_attempt(tx, &o.request_operation_id)?;
    validate_plan(tx, &p)?;
    collector.validate(&p)?;
    if o.token != child_token(&p, &p.scope.parent_step_id)
        || o.lease_id != a.lease_id
        || o.binding_id != p.scope.binding_id
        || o.incarnation != p.scope.incarnation
    {
        return Err(LifecycleError::Conflict);
    }
    let v = validated_anchor(tx, &p.scope.parent_step_id)?;
    let owned = warm::owned(tx, &p)?;
    let identities = crate::lifecycle::completion::identity_dtos(
        &crate::lifecycle::completion::canonical_members(&o.identities)?,
    );
    if identities != owned.identities {
        return Err(LifecycleError::Conflict);
    }
    crate::lifecycle::completion::nonempty_receipt(&o.receipt)?;
    let (model_matches, output_matches, finish_matches, response_digest) =
        response_facts(&p, &a, &o.response)?;
    let e = MarkerEvidence {
        version: 3,
        kind: MarkerTag::Marker,
        attempt: a.clone(),
        identities,
        origin: crate::qualification::recipe_v1::PROGRAM_REVISION.into(),
        observed_at_ms: o.observed_at_ms,
        receipt: o.receipt.clone(),
        terminal: match o.terminal {
            CandidateTerminal::Completed => Terminal::Completed,
            CandidateTerminal::FailedTerminal | CandidateTerminal::RejectedWithoutWork => {
                Terminal::Failed
            }
            CandidateTerminal::Uncertain => Terminal::Uncertain,
        },
        model_matches,
        output_matches,
        finish_matches,
        response_digest,
    };
    if let Some(old) = result(tx, &p, &a)? {
        return if old == e {
            Ok(())
        } else {
            Err(LifecycleError::Conflict)
        };
    }
    if p.scope.session_id != session.id()
        || v.snapshot.state() != super::super::super::CandidateRunState::Running
        || v.state != "completed"
    {
        return Err(LifecycleError::Stale);
    }
    super::super::super::initialize::policy(tx, &v.snapshot)?;
    crate::lifecycle::completion::fresh(
        a.issued_at_ms,
        a.deadline_ms,
        e.observed_at_ms,
        now,
        crate::lifecycle::completion::policy_ttl(tx, &p.scope.host)?,
    )?;
    let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM request_leases l JOIN deployments d ON d.id=l.deployment_id WHERE l.id=?1 AND l.deployment_id=?2 AND l.revision=?3 AND l.generation=?4 AND l.session_id=?5 AND l.disposition='inflight' AND d.revision=l.revision AND d.current_generation=l.generation AND d.admission_enabled=0 AND d.dispatch_enabled=0 AND d.desired_state='stopped')",params![a.lease_id,p.scope.deployment_id,p.scope.revision,p.scope.generation,p.scope.session_id],|r|r.get(0))?;
    if !valid {
        return Err(LifecycleError::Conflict);
    }
    let epoch = crate::resource_ledger::advance_completion_epoch(tx)?;
    tx.execute(
        "INSERT INTO qualification_request_results VALUES(?1,?2,?3)",
        params![a.request_operation_id, encode(&e)?, epoch],
    )?;
    if e.terminal == Terminal::Uncertain {
        one(tx.execute(
            "UPDATE request_leases SET disposition='uncertain' WHERE id=?1",
            [&a.lease_id],
        )?)?;
    } else {
        one(tx.execute("DELETE FROM request_leases WHERE id=?1", [&a.lease_id])?)?;
        one(tx.execute(
            "UPDATE operations SET state=?1 WHERE id=?2 AND state='running'",
            params![
                if e.passes() { "succeeded" } else { "failed" },
                a.request_operation_id
            ],
        )?)?;
    }
    if e.passes() {
        coverage(
            tx,
            &p,
            &a.case_id,
            SourceKind::RequestAttempt,
            &a.request_operation_id,
            &digest(&e)?,
        )?;
    }
    result(tx, &p, &a)?;
    Ok(())
}

fn response_facts(
    p: &CandidateActionPlanV3,
    a: &RequestAttemptV3,
    response: &CandidateResponseObservation,
) -> Result<(bool, bool, bool, String), LifecycleError> {
    crate::qualification::recipe_v1::marker_response_facts(
        &p.scope.deployment_id,
        a.item_ordinal,
        a.case_kind == CandidateCaseKind::MarkerStreaming,
        response,
    )
}

//! Accounted dependent requests share the parent lifecycle claim.
use super::*;
use crate::dispatch::DispatchTicket;
use mllm_domain::completion::StepExecutionContext;
use mllm_scheduler::residency::AdmissionContext;
#[path = "markers.rs"]
pub(super) mod markers;
#[path = "security.rs"]
pub(super) mod security;
pub use security::{CandidateSecurityControlDispatch, CandidateSecurityDispatch};

#[derive(Debug)]
pub enum CandidateDispatchResult {
    New(Box<CandidateProbeDispatch>),
    AlreadyRecorded { request_operation_id: String },
}
#[derive(Debug)]
pub struct CandidateProbeDispatch {
    ticket: DispatchTicket,
    request_operation_id: String,
    request: String,
    context: StepExecutionContext,
    security_endpoint: Option<mllm_domain::qualification::CandidateSecurityEndpoint>,
}
impl CandidateProbeDispatch {
    pub fn security_endpoint(
        &self,
    ) -> Option<mllm_domain::qualification::CandidateSecurityEndpoint> {
        self.security_endpoint
    }
    pub fn ticket(&self) -> &DispatchTicket {
        &self.ticket
    }
    pub fn request_operation_id(&self) -> &str {
        &self.request_operation_id
    }
    pub fn request(&self) -> &str {
        &self.request
    }
    pub fn context(&self) -> &StepExecutionContext {
        &self.context
    }
}
impl crate::Store {
    pub fn record_candidate_result(
        &self,
        session: &CoordinatorSession,
        collector: &CandidateCollector,
        observation: &mllm_domain::qualification::CandidateRequestObservation,
        now: i64,
    ) -> Result<(), LifecycleError> {
        use crate::lifecycle::completion::{
            canonical_members, fresh, identity_dtos, nonempty_receipt, policy_ttl,
        };
        use mllm_domain::qualification::{CandidateResponseObservation, CandidateTerminal};
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session)?;
        if security::is_request(&tx, &observation.request_operation_id)? {
            security::record_request(&tx, session, collector, observation, now)?;
            tx.commit()?;
            return Ok(());
        }
        if markers::is_marker(&tx, &observation.request_operation_id)? {
            markers::record(&tx, session, collector, observation, now)?;
            tx.commit()?;
            return Ok(());
        }
        let p = plan_for_step(&tx, &observation.token.step_id)?;
        validate_plan(&tx, &p)?;
        collector.validate(&p)?;
        let a = attempt(&tx, &p)?.ok_or(LifecycleError::Conflict)?;
        spending(&tx, &p)?;
        if observation.request_operation_id != a.request_operation_id
            || observation.lease_id != a.lease_id
            || observation.token != child_token(&p, &a.child_step_id)
            || observation.binding_id != p.scope.binding_id
            || observation.incarnation != p.scope.incarnation
        {
            return Err(LifecycleError::Conflict);
        }
        nonempty_receipt(&observation.receipt)?;
        let v = validated_anchor(&tx, &p.scope.parent_step_id)?;
        let owned = warm::owned(&tx, &p)?;
        let identities = identity_dtos(&canonical_members(&observation.identities)?);
        if identities != owned.identities {
            return Err(LifecycleError::Conflict);
        }
        let (model, content, finish) = match &observation.response {
            CandidateResponseObservation::Nonstreaming {
                model,
                content,
                finish_reason,
            } => (model.as_str(), content.as_str(), finish_reason.as_str()),
            CandidateResponseObservation::NoResponse => ("", "", ""),
            _ => return Err(LifecycleError::Invalid),
        };
        if model.len() + content.len() + finish.len() > 1048576 {
            return Err(LifecycleError::Invalid);
        }
        let terminal = match observation.terminal {
            CandidateTerminal::Completed => Terminal::Completed,
            CandidateTerminal::FailedTerminal | CandidateTerminal::RejectedWithoutWork => {
                Terminal::Failed
            }
            CandidateTerminal::Uncertain => Terminal::Uncertain,
        };
        let value = ProbeEvidenceV3 {
            version: 3,
            kind: ProbeEvidenceTag::Probe,
            attempt: a.clone(),
            identities,
            origin: crate::qualification::recipe_v1::PROGRAM_REVISION.into(),
            observed_at_ms: observation.observed_at_ms,
            receipt: observation.receipt.clone(),
            terminal,
            model_matches: model == format!("candidate-{}", p.scope.deployment_id),
            output_matches: content == "MLLM_READY_13",
            finish_matches: finish == "stop",
            response_digest: format!("{:x}", Sha256::digest(encode(&(model, content, finish))?)),
        };
        if let Some((old, _)) = probe_evidence(&tx, &p)? {
            return if old == value {
                Ok(())
            } else {
                Err(LifecycleError::Conflict)
            };
        }
        current_anchor(&tx, session, &p.scope.parent_step_id)?;
        fresh(
            a.issued_at_ms,
            a.deadline_ms,
            value.observed_at_ms,
            now,
            policy_ttl(&tx, &p.scope.host)?,
        )?;
        let lease:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM request_leases WHERE id=?1 AND deployment_id=?2 AND revision=?3 AND generation=?4 AND session_id=?5 AND disposition='inflight')",params![a.lease_id,p.scope.deployment_id,p.scope.revision,p.scope.generation,p.scope.session_id],|r|r.get(0))?;
        if !lease {
            return Err(LifecycleError::Conflict);
        }
        if value.passes() {
            let other_work: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM request_leases WHERE deployment_id=?1 AND id!=?2)",
                params![p.scope.deployment_id, a.lease_id],
                |r| r.get(0),
            )?;
            if other_work {
                return Err(LifecycleError::Conflict);
            }
        }
        let epoch = crate::resource_ledger::advance_completion_epoch(&tx)?;
        tx.execute(
            "INSERT INTO lifecycle_evidence VALUES(?1,?2,?3)",
            params![a.child_step_id, encode(&value)?, epoch],
        )?;
        if value.terminal != Terminal::Uncertain {
            one(tx.execute("DELETE FROM request_leases WHERE id=?1", [&a.lease_id])?)?;
            one(tx.execute(
                "UPDATE operations SET state=?1 WHERE id=?2 AND state='running'",
                params![
                    if value.passes() {
                        "succeeded"
                    } else {
                        "failed"
                    },
                    a.request_operation_id
                ],
            )?)?;
        } else {
            one(tx.execute(
                "UPDATE request_leases SET disposition='uncertain' WHERE id=?1",
                [&a.lease_id],
            )?)?;
        }
        if value.passes() {
            let source_digests = ready_sources(&tx, &p, &value, epoch)?;
            let aggregate = AggregateEvidenceV3 {
                version: 3,
                kind: AggregateTag::Ready,
                scope: p.scope.clone(),
                origin: value.origin.clone(),
                source_steps: p.effects.iter().map(|s| s.step_id.clone()).collect(),
                source_digests,
                observed_at_ms: value.observed_at_ms,
                facts: vec![
                    Fact::AllocationsRestored,
                    Fact::WeightsUsable,
                    Fact::CacheValid,
                    Fact::ModelUsable,
                ],
            };
            verify_aggregate(
                &v,
                &observation.identities,
                &aggregate,
                now,
                policy_ttl(&tx, &p.scope.host)?,
            )?;
            one(tx.execute(
                "UPDATE lifecycle_steps SET state='completed' WHERE id=?1 AND state='armed'",
                [&a.child_step_id],
            )?)?;
            one(tx.execute(
                "UPDATE lifecycle_steps SET state='completed' WHERE id=?1 AND state='armed'",
                [&p.scope.parent_step_id],
            )?)?;
            one(tx.execute("UPDATE lifecycle_runs SET state='succeeded' WHERE operation_id=?1 AND state='running'",[&p.scope.operation_id])?)?;
            one(tx.execute(
                "UPDATE operations SET state='succeeded' WHERE id=?1 AND state='running'",
                [&p.scope.operation_id],
            )?)?;
            tx.execute(
                "INSERT INTO lifecycle_evidence VALUES(?1,?2,?3)",
                params![p.scope.parent_step_id, encode(&aggregate)?, epoch],
            )?;
            one(tx.execute(
                "UPDATE runtime_bindings SET state='live' WHERE id=?1 AND state='uncertain'",
                [&p.scope.binding_id],
            )?)?;
            one(tx.execute(
                "UPDATE deployments SET observed_state='ready' WHERE id=?1",
                [&p.scope.deployment_id],
            )?)?;
            one(tx.execute(
                "DELETE FROM lifecycle_claims WHERE operation_id=?1",
                [&p.scope.operation_id],
            )?)?;
            coverage(
                &tx,
                &p,
                &p.case_id,
                SourceKind::LifecycleStep,
                &p.scope.parent_step_id,
                &digest(&aggregate)?,
            )?;
            coverage(
                &tx,
                &p,
                &a.case_id,
                SourceKind::RequestAttempt,
                &a.request_operation_id,
                &digest(&value)?,
            )?;
            crate::lifecycle::completion::event(
                &tx,
                session,
                &p.scope.operation_id,
                &p.scope.deployment_id,
                &p.scope.parent_step_id,
                crate::events::CandidateLifecycleTransition::ReadyCompleted,
                Some(epoch),
            )?;
        } else {
            tx.execute(
                "UPDATE lifecycle_steps SET state='uncertain' WHERE id IN (?1,?2)",
                params![p.scope.parent_step_id, a.child_step_id],
            )?;
            tx.execute(
                "UPDATE lifecycle_runs SET state='uncertain' WHERE operation_id=?1",
                [&p.scope.operation_id],
            )?;
        }
        validate_plan(&tx, &p)?;
        tx.commit()?;
        Ok(())
    }
    pub fn arm_candidate_probe(
        &self,
        session: &CoordinatorSession,
        parent: &str,
        context: AdmissionContext<'_>,
    ) -> Result<CandidateDispatchResult, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session)?;
        let p = plan_for_step(&tx, parent)?;
        let v = validated_anchor(&tx, parent)?;
        spending(&tx, &p)?;
        if let Some(prior) = attempt(&tx, &p)? {
            return Ok(CandidateDispatchResult::AlreadyRecorded {
                request_operation_id: prior.request_operation_id,
            });
        }
        current_anchor(&tx, session, parent)?;
        let resource = super::super::initialize::policy(&tx, &v.snapshot)?;
        let qualification = read_candidate_policy(&tx, &p.scope.host)
            .map_err(super::super::map_qualification)
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
            || resource.revision != p.scope.resource_policy_revision
            || qualification.revision != p.scope.qualification_policy_revision
            || context.now_ms < v.context.issued_at_ms
            || context.now_ms >= p.deadline_ms
        {
            return Err(LifecycleError::Conflict);
        }
        let ledger = crate::resource_ledger::read_snapshot(&tx).map_err(resource_error)?;
        let retained = ledger
            .owners
            .get(&p.scope.deployment_id)
            .ok_or(LifecycleError::Conflict)?;
        mllm_scheduler::residency::admit_phase(&ledger, &p.scope.deployment_id, retained, context)
            .map_err(|e| LifecycleError::Rejected(e.to_string()))?;
        if !matches!(p.action, Action::Initialize | Action::Restore) {
            return Err(LifecycleError::Conflict);
        }
        let probe = p.effects.last().ok_or(LifecycleError::CorruptStoredData)?;
        warm::predecessors(&tx, &p, probe, context.now_ms)?;
        if v.snapshot.requests_used() >= v.snapshot.reviewed_manifest().limits().max_requests() {
            return Err(LifecycleError::Conflict);
        }
        let association = warm::owned(&tx, &p)?;
        let request = probe_request(&p)?;
        let operation = ulid::Ulid::new().to_string();
        let lease = ulid::Ulid::new().to_string();
        let a = RequestAttemptV3 {
            version: 3,
            kind: AttemptTag::Probe,
            scope: p.scope.clone(),
            case_id: p
                .ready_probe_case
                .clone()
                .ok_or(LifecycleError::CorruptStoredData)?,
            item_ordinal: 0,
            subcheck_id: String::new(),
            child_step_id: probe.step_id.clone(),
            request_operation_id: operation.clone(),
            lease_id: lease.clone(),
            command_scope: "INTERNAL qualification ReadyProbe".into(),
            idempotency_key: format!(
                "{}:{}",
                p.scope.operation_id,
                p.ready_probe_case.as_deref().unwrap()
            ),
            request_hash: format!("{:x}", Sha256::digest(request.as_bytes())),
            program_revision: crate::qualification::recipe_v1::PROGRAM_REVISION.into(),
            case_kind: CandidateCaseKind::ReadyProbe,
            cycle: p.cycle,
            issued_at_ms: context.now_ms,
            deadline_ms: p.deadline_ms,
        };
        let child = ArmedEffectV3 {
            version: 3,
            kind: ArmedEffectTag::Armed,
            parent: p.scope.clone(),
            effect: probe.clone(),
            grant_id: v
                .context
                .grant_id
                .clone()
                .ok_or(LifecycleError::CorruptStoredData)?,
            issued_at_ms: context.now_ms,
        };
        one(tx.execute(
            "UPDATE lifecycle_steps SET state='armed',step_json=?1 WHERE id=?2 AND state='planned'",
            params![encode(&child)?, a.child_step_id],
        )?)?;
        tx.execute("INSERT INTO operations(id,deployment_id,kind,state) VALUES(?1,?2,'candidate_probe_v3','running')",params![operation,p.scope.deployment_id])?;
        let json = encode(&a)?;
        tx.execute("INSERT INTO command_receipts(principal_id,command_scope,idempotency_key,request_hash,operation_id,response_json) VALUES(?1,?2,?3,?4,?5,?6)",params![p.scope.principal,a.command_scope,a.idempotency_key,a.request_hash,operation,json])?;
        tx.execute("INSERT INTO qualification_request_attempts VALUES(?1,?2,0,'',?3,?4,?5,?6,?7,?8,?9,?10)",params![p.scope.run_id,a.case_id,operation,lease,p.scope.principal,a.command_scope,a.idempotency_key,p.scope.operation_id,a.child_step_id,json])?;
        tx.execute(
            "INSERT INTO request_leases VALUES(?1,?2,?3,?4,?5,'inflight')",
            params![
                lease,
                p.scope.deployment_id,
                p.scope.revision,
                p.scope.generation,
                p.scope.session_id
            ],
        )?;
        one(tx.execute("UPDATE qualification_runs SET requests_used=requests_used+1 WHERE id=?1 AND requests_used=?2",params![p.scope.run_id,v.snapshot.requests_used()])?)?;
        validate_plan(&tx, &p)?;
        attempt(&tx, &p)?.ok_or(LifecycleError::CorruptStoredData)?;
        spending(&tx, &p)?;
        tx.commit()?;
        let mut execution = v.context;
        execution.token = child_token(&p, &a.child_step_id);
        execution.issued_at_ms = context.now_ms;
        execution.identities = mllm_domain::completion::ExecutionIdentities::Retained(
            crate::lifecycle::completion::members(&association.identities)?,
        );
        execution.completion_target = None;
        execution.launch_settings = None;
        Ok(CandidateDispatchResult::New(Box::new(
            CandidateProbeDispatch {
                ticket: DispatchTicket::candidate(
                    lease,
                    p.scope.deployment_id,
                    p.scope.revision,
                    p.scope.generation,
                    p.scope.session_id,
                ),
                request_operation_id: operation,
                request,
                context: execution,
                security_endpoint: None,
            },
        )))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Terminal {
    Completed,
    Rejected,
    Failed,
    Uncertain,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum ProbeEvidenceTag {
    #[serde(rename = "candidate_probe_observed")]
    Probe,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeEvidenceV3 {
    version: u8,
    kind: ProbeEvidenceTag,
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
impl ProbeEvidenceV3 {
    fn passes(&self) -> bool {
        self.terminal == Terminal::Completed
            && self.model_matches
            && self.output_matches
            && self.finish_matches
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum AggregateTag {
    #[serde(rename = "candidate_ready_aggregate")]
    Ready,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AggregateEvidenceV3 {
    version: u8,
    kind: AggregateTag,
    scope: Scope,
    origin: String,
    source_steps: Vec<String>,
    source_digests: Vec<String>,
    observed_at_ms: i64,
    facts: Vec<Fact>,
}
pub(super) fn digest(v: &impl Serialize) -> Result<String, LifecycleError> {
    Ok(format!("{:x}", Sha256::digest(encode(v)?)))
}
fn probe_evidence(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
) -> Result<Option<(ProbeEvidenceV3, u64)>, LifecycleError> {
    let row: Option<(String, u64)> = tx
        .query_row(
            "SELECT evidence_json,committed_epoch FROM lifecycle_evidence WHERE step_id=?1",
            [&p.effects
                .last()
                .ok_or(LifecycleError::CorruptStoredData)?
                .step_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    row.map(|(s, e)| Ok((decode(&s)?, e))).transpose()
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SourceKind {
    LifecycleStep,
    RequestAttempt,
    ParkedStatusObservation,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CoverageV3 {
    pub(super) version: u8,
    pub(super) scope: Scope,
    pub(super) case_id: String,
    pub(super) source_kind: SourceKind,
    pub(super) source_id: String,
    pub(super) source_digest: String,
    pub(super) origin: String,
}
pub(super) fn coverage(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
    case: &str,
    kind: SourceKind,
    id: &str,
    source_digest: &str,
) -> Result<(), LifecycleError> {
    let n: u32 = tx.query_row(
        "SELECT COUNT(*) FROM qualification_evidence_refs WHERE run_id=?1",
        [&p.scope.run_id],
        |r| r.get(0),
    )?;
    if n >= 4096 {
        return Err(LifecycleError::Conflict);
    }
    let value = CoverageV3 {
        version: 3,
        scope: p.scope.clone(),
        case_id: case.into(),
        source_kind: kind,
        source_id: id.into(),
        source_digest: source_digest.into(),
        origin: crate::qualification::recipe_v1::PROGRAM_REVISION.into(),
    };
    tx.execute(
        "INSERT INTO qualification_evidence_refs VALUES(?1,?2,?3,?4,?5)",
        params![
            ulid::Ulid::new().to_string(),
            p.scope.run_id,
            case,
            digest(&value)?,
            encode(&value)?
        ],
    )?;
    Ok(())
}
fn ready_sources(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
    probe: &ProbeEvidenceV3,
    epoch: u64,
) -> Result<Vec<String>, LifecycleError> {
    let mut digests = Vec::new();
    let mut facts = Vec::new();
    let mut previous = 0;
    for spec in p.effects.iter().take(p.effects.len() - 1) {
        let (raw, e): (String, u64) = tx.query_row(
            "SELECT evidence_json,committed_epoch FROM lifecycle_evidence WHERE step_id=?1",
            [&spec.step_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let source: EffectEvidenceV3 = decode(&raw)?;
        if source.scope != p.scope
            || source.effect != *spec
            || source.facts != warm::facts(spec.effect)?
            || source.identities != probe.identities
            || source.observed_at_ms > probe.observed_at_ms
            || e <= previous
            || e >= epoch
        {
            return Err(LifecycleError::CorruptStoredData);
        }
        previous = e;
        facts.extend(source.facts.iter().copied());
        digests.push(digest(&source)?);
    }
    if facts
        != [
            Fact::AllocationsRestored,
            Fact::WeightsUsable,
            Fact::CacheValid,
        ]
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    digests.push(digest(probe)?);
    Ok(digests)
}
fn verify_aggregate(
    v: &super::super::initialize::ValidatedInitialize,
    identities: &[mllm_domain::completion::ProcessIdentity],
    a: &AggregateEvidenceV3,
    now: i64,
    ttl: i64,
) -> Result<(), LifecycleError> {
    use mllm_domain::completion::{
        verify_completion, CompletionEvidence, CompletionExpectation, Milestone,
    };
    let evidence = CompletionEvidence {
        token: v.context.token.clone(),
        identities: identities.to_vec(),
        observed_at_ms: a.observed_at_ms,
        control_receipt: Some(digest(a)?),
        milestones: vec![
            Milestone::AllocationsRestored,
            Milestone::WeightsUsable,
            Milestone::CacheValid,
            Milestone::ModelUsable,
        ],
    };
    verify_completion(
        &CompletionExpectation {
            token: v.context.token.clone(),
            identities: identities.to_vec(),
            target: v
                .context
                .completion_target
                .clone()
                .ok_or(LifecycleError::CorruptStoredData)?,
            issued_at_ms: v.context.issued_at_ms,
            deadline_ms: v.context.deadline_ms,
        },
        &evidence,
        now,
        ttl,
    )
    .map(|_| ())
    .map_err(|e| LifecycleError::Rejected(e.to_string()))
}
pub(super) fn validate_case_coverage(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
    case: &str,
    sources: &[(SourceKind, String)],
    id: &str,
) -> Result<(), LifecycleError> {
    let mut expected = std::collections::BTreeMap::new();
    for (kind, source_digest) in sources {
        let value = CoverageV3 {
            version: 3,
            scope: p.scope.clone(),
            case_id: case.into(),
            source_kind: kind.clone(),
            source_id: id.into(),
            source_digest: source_digest.clone(),
            origin: crate::qualification::recipe_v1::PROGRAM_REVISION.into(),
        };
        expected.insert(digest(&value)?, value);
    }
    let mut stmt=tx.prepare("SELECT evidence_digest,metadata_json FROM qualification_evidence_refs WHERE run_id=?1 AND case_id=?2")?;
    let rows = stmt
        .query_map(params![p.scope.run_id, case], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if rows.len() != expected.len() {
        return Err(LifecycleError::CorruptStoredData);
    }
    for (hash, raw) in rows {
        if expected.remove(&hash).as_ref() != Some(&decode::<CoverageV3>(&raw)?) {
            return Err(LifecycleError::CorruptStoredData);
        }
    }
    Ok(())
}
pub(super) fn validate_progress(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
    states: &[&str],
    run: &str,
    cleanup_resolved: bool,
) -> Result<(), LifecycleError> {
    spending(tx, p)?;
    let a = attempt(tx, p)?;
    let last = states.len() - 1;
    if states[0] == "planned" {
        if states.iter().any(|s| *s != "planned") || run != "queued" || a.is_some() {
            return Err(LifecycleError::CorruptStoredData);
        }
        return Ok(());
    }
    if states[1] == "planned" || (states[last] == "planned") != a.is_none() {
        return Err(LifecycleError::CorruptStoredData);
    }
    let e = probe_evidence(tx, p)?;
    if let Some((e, epoch)) = e {
        if Some(&e.attempt) != a.as_ref()
            || e.version != 3
            || e.origin != crate::qualification::recipe_v1::PROGRAM_REVISION
            || e.observed_at_ms < e.attempt.issued_at_ms
            || e.observed_at_ms > p.deadline_ms
            || e.response_digest.len() != 64
            || epoch
                > crate::resource_ledger::read_snapshot(tx)
                    .map_err(resource_error)?
                    .epoch
        {
            return Err(LifecycleError::CorruptStoredData);
        }
        crate::lifecycle::completion::members(&e.identities)?;
        crate::lifecycle::completion::nonempty_receipt(&e.receipt)?;
        let lease: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM request_leases WHERE id=?1)",
            [&e.attempt.lease_id],
            |r| r.get(0),
        )?;
        if lease != (e.terminal == Terminal::Uncertain && !cleanup_resolved) {
            return Err(LifecycleError::CorruptStoredData);
        }
        let operation_state: String = tx.query_row(
            "SELECT state FROM operations WHERE id=?1",
            [&e.attempt.request_operation_id],
            |r| r.get(0),
        )?;
        if operation_state
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
        if e.passes() {
            if states.iter().any(|s| *s != "completed") || run != "succeeded" {
                return Err(LifecycleError::CorruptStoredData);
            }
            let (raw, ae): (String, u64) = tx.query_row(
                "SELECT evidence_json,committed_epoch FROM lifecycle_evidence WHERE step_id=?1",
                [&p.scope.parent_step_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let aggregate: AggregateEvidenceV3 = decode(&raw)?;
            let source_digests = ready_sources(tx, p, &e, epoch)?;
            if ae != epoch
                || aggregate
                    != (AggregateEvidenceV3 {
                        version: 3,
                        kind: AggregateTag::Ready,
                        scope: p.scope.clone(),
                        origin: e.origin.clone(),
                        source_steps: p.effects.iter().map(|s| s.step_id.clone()).collect(),
                        source_digests,
                        observed_at_ms: e.observed_at_ms,
                        facts: vec![
                            Fact::AllocationsRestored,
                            Fact::WeightsUsable,
                            Fact::CacheValid,
                            Fact::ModelUsable,
                        ],
                    })
            {
                return Err(LifecycleError::CorruptStoredData);
            }
            for (case, kind, id, source_digest) in [
                (
                    &p.case_id,
                    SourceKind::LifecycleStep,
                    &p.scope.parent_step_id,
                    digest(&aggregate)?,
                ),
                (
                    &e.attempt.case_id,
                    SourceKind::RequestAttempt,
                    &e.attempt.request_operation_id,
                    digest(&e)?,
                ),
            ] {
                let expected = CoverageV3 {
                    version: 3,
                    scope: p.scope.clone(),
                    case_id: case.clone(),
                    source_kind: kind,
                    source_id: id.clone(),
                    source_digest,
                    origin: e.origin.clone(),
                };
                let mut stmt=tx.prepare("SELECT evidence_digest,metadata_json FROM qualification_evidence_refs WHERE run_id=?1 AND case_id=?2")?;
                let rows = stmt
                    .query_map(params![p.scope.run_id, case], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                if rows.len() != 1
                    || rows[0].0 != digest(&expected)?
                    || decode::<CoverageV3>(&rows[0].1)? != expected
                {
                    return Err(LifecycleError::CorruptStoredData);
                }
            }
        } else if states[0] != "uncertain"
            || states[last] != "uncertain"
            || states[1..last].iter().any(|s| *s != "completed")
            || run != "uncertain"
        {
            return Err(LifecycleError::CorruptStoredData);
        }
    } else if states[0] == "completed" || states[last] == "completed" || run == "succeeded" {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum AttemptTag {
    #[serde(rename = "candidate_ready_probe_request")]
    Probe,
    #[serde(rename = "candidate_marker_request")]
    Marker,
    #[serde(rename = "candidate_security_request")]
    Security,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestAttemptV3 {
    version: u8,
    kind: AttemptTag,
    scope: Scope,
    case_id: String,
    item_ordinal: u32,
    subcheck_id: String,
    child_step_id: String,
    request_operation_id: String,
    lease_id: String,
    command_scope: String,
    idempotency_key: String,
    request_hash: String,
    program_revision: String,
    case_kind: CandidateCaseKind,
    cycle: u32,
    issued_at_ms: i64,
    deadline_ms: i64,
}
fn probe_request(p: &CandidateActionPlanV3) -> Result<String, LifecycleError> {
    Ok(crate::qualification::recipe_v1::ready_request(
        &p.scope.deployment_id,
    ))
}
pub(super) fn spending(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
) -> Result<(), LifecycleError> {
    let (used,count):(u32,u32)=tx.query_row("SELECT requests_used,(SELECT COUNT(*) FROM qualification_request_attempts WHERE run_id=?1) FROM qualification_runs WHERE id=?1",[&p.scope.run_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
    if used != count || count > 4096 {
        return Err(LifecycleError::CorruptStoredData);
    }
    let snapshot = super::super::read_snapshot(tx, &p.scope.principal, &p.scope.run_id)
        .map_err(creation_error)?
        .ok_or(LifecycleError::CorruptStoredData)?;
    if used > snapshot.reviewed_manifest().limits().max_requests() {
        return Err(LifecycleError::CorruptStoredData);
    }
    let mut stmt=tx.prepare("SELECT receipt_json FROM qualification_request_attempts WHERE run_id=?1 ORDER BY request_operation_id")?;
    let rows = stmt
        .query_map([&p.scope.run_id], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut per_case = std::collections::BTreeMap::new();
    for raw in rows {
        let a: RequestAttemptV3 = decode(&raw)?;
        if a.scope.run_id != p.scope.run_id {
            return Err(LifecycleError::CorruptStoredData);
        }
        let plan = plan_for_step(tx, &a.scope.parent_step_id)?;
        let canonical = match a.kind {
            AttemptTag::Probe => attempt(tx, &plan)?.ok_or(LifecycleError::CorruptStoredData)?,
            AttemptTag::Marker => markers::load_attempt(tx, &a.request_operation_id)?.1,
            AttemptTag::Security => {
                let index = plan
                    .effects
                    .iter()
                    .position(|s| s.step_id == a.child_step_id)
                    .ok_or(LifecycleError::CorruptStoredData)?;
                security::attempt_for(tx, &plan, index)?.ok_or(LifecycleError::CorruptStoredData)?
            }
        };
        if canonical != a {
            return Err(LifecycleError::CorruptStoredData);
        }
        *per_case.entry(a.case_id).or_insert(0_u32) += 1;
    }
    for (case, spent) in per_case {
        let case = snapshot
            .reviewed_manifest()
            .cases()
            .iter()
            .find(|c| c.id() == case)
            .ok_or(LifecycleError::CorruptStoredData)?;
        if spent > case.request_budget() {
            return Err(LifecycleError::CorruptStoredData);
        }
    }
    Ok(())
}
fn attempt(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
) -> Result<Option<RequestAttemptV3>, LifecycleError> {
    let probe = p.effects.last().ok_or(LifecycleError::CorruptStoredData)?;
    let raw:Option<String>=tx.query_row("SELECT receipt_json FROM qualification_request_attempts WHERE parent_operation_id=?1 AND child_step_id=?2",params![p.scope.operation_id,probe.step_id],|r|r.get(0)).optional()?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let a: RequestAttemptV3 = decode(&raw)?;
    if a.version != 3
        || a.scope != p.scope
        || Some(a.case_id.as_str()) != p.ready_probe_case.as_deref()
        || a.item_ordinal != 0
        || !a.subcheck_id.is_empty()
        || a.child_step_id != probe.step_id
        || a.program_revision != crate::qualification::recipe_v1::PROGRAM_REVISION
        || a.case_kind != CandidateCaseKind::ReadyProbe
        || a.cycle != p.cycle
        || a.command_scope != "INTERNAL qualification ReadyProbe"
        || a.idempotency_key != format!("{}:{}", p.scope.operation_id, a.case_id)
        || a.request_hash != format!("{:x}", Sha256::digest(probe_request(p)?.as_bytes()))
        || !super::super::ulid(&a.request_operation_id)
        || !super::super::ulid(&a.lease_id)
        || a.issued_at_ms < p.accepted_at_ms
        || a.issued_at_ms >= p.deadline_ms
        || a.deadline_ms != p.deadline_ms
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    let matches:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM qualification_request_attempts a JOIN command_receipts c ON c.principal_id=a.principal_id AND c.command_scope=a.command_scope AND c.idempotency_key=a.idempotency_key JOIN operations o ON o.id=a.request_operation_id WHERE a.run_id=?1 AND a.case_id=?2 AND a.item_ordinal=0 AND a.subcheck_id='' AND a.request_operation_id=?3 AND a.lease_id=?4 AND a.principal_id=?5 AND a.command_scope=?6 AND a.idempotency_key=?7 AND a.parent_operation_id=?8 AND a.child_step_id=?9 AND c.operation_id=?3 AND c.request_hash=?10 AND c.response_json=a.receipt_json AND o.kind='candidate_probe_v3' AND o.deployment_id=?11 AND o.state IN ('running','succeeded','failed') AND o.idempotency_key IS NULL)",params![a.scope.run_id,a.case_id,a.request_operation_id,a.lease_id,a.scope.principal,a.command_scope,a.idempotency_key,a.scope.operation_id,a.child_step_id,a.request_hash,a.scope.deployment_id],|r|r.get(0))?;
    let raw: String = tx.query_row(
        "SELECT step_json FROM lifecycle_steps WHERE id=?1",
        [&a.child_step_id],
        |r| r.get(0),
    )?;
    let child: ArmedEffectV3 = decode(&raw)?;
    if !matches || child.issued_at_ms != a.issued_at_ms {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok(Some(a))
}

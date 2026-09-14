//! Version-three candidate action plans. Historical V1/V2 are never upgraded.
use crate::dispatch::CoordinatorSession;
use crate::lifecycle::completion::{check_session, decode, encode};
use crate::lifecycle::LifecycleError;
use crate::qualification::recipe_v1::QualificationProgram;
use crate::qualification_policy::read_candidate_policy;
use mllm_config::effective::candidate::{CandidateCaseKind, CandidatePhase};
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[path = "../qualification/catalog.rs"]
mod catalog;
#[path = "inference.rs"]
mod inference;
#[path = "warm.rs"]
mod warm;
pub use catalog::QualificationReceipt;
pub use inference::{
    CandidateDispatchResult, CandidateProbeDispatch, CandidateSecurityControlDispatch,
    CandidateSecurityDispatch,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    Initialize,
    Park,
    Restore,
    Security,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistedEffectKind {
    Initialize,
    Drain,
    Park,
    Restore,
    ReloadWeights,
    InvalidateCache,
    Probe,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Fact {
    Quiesced,
    MemoryReleased,
    AllocationsRestored,
    WeightsUsable,
    CacheValid,
    ModelUsable,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum ActionTag {
    #[serde(rename = "candidate_action_planned")]
    Planned,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum EffectTag {
    #[serde(rename = "candidate_effect_planned")]
    Planned,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scope {
    principal: String,
    host: String,
    run_id: String,
    creation_operation_id: String,
    operation_id: String,
    parent_step_id: String,
    deployment_id: String,
    revision: i64,
    generation: i64,
    session_id: String,
    binding_id: String,
    incarnation: String,
    qualification_id: String,
    descriptor: super::DescriptorRefV1,
    resource_policy_revision: i64,
    qualification_policy_revision: i64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EffectSpec {
    step_id: String,
    ordinal: u32,
    effect: PersistedEffectKind,
    predecessor: Option<String>,
    required_facts: Vec<Fact>,
    request_case: Option<String>,
    request_item: Option<u32>,
    request_subcheck: Option<String>,
    deadline_ms: i64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateActionPlanV3 {
    version: u8,
    scope: Scope,
    action: Action,
    case_id: String,
    case_kind: CandidateCaseKind,
    cycle: u32,
    accepted_at_ms: i64,
    deadline_ms: i64,
    completion_target: CandidatePhase,
    effects: Vec<EffectSpec>,
    ready_probe_case: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlannedActionV3 {
    version: u8,
    kind: ActionTag,
    plan: CandidateActionPlanV3,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlannedEffectV3 {
    version: u8,
    kind: EffectTag,
    parent: Scope,
    effect: EffectSpec,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum ArmedActionTag {
    #[serde(rename = "candidate_action_armed")]
    Armed,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum ArmedEffectTag {
    #[serde(rename = "candidate_effect_armed")]
    Armed,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArmedActionV3 {
    version: u8,
    kind: ArmedActionTag,
    plan: CandidateActionPlanV3,
    grant_id: String,
    issued_at_ms: i64,
    expected_epoch: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArmedEffectV3 {
    version: u8,
    kind: ArmedEffectTag,
    parent: Scope,
    effect: EffectSpec,
    grant_id: String,
    issued_at_ms: i64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeLinkV3 {
    version: u8,
    cycle: u32,
    descriptor: super::DescriptorRefV1,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptV3 {
    version: u8,
    principal: String,
    scope: String,
    key: String,
    request_hash: String,
    plan: CandidateActionPlanV3,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Command {
    expected_revision: i64,
    action: WireAction,
    deadline_ms: i64,
}
#[derive(Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum WireAction {
    Initialize,
    Park,
    Restore,
}

fn creation_error(e: super::CandidateCreationError) -> LifecycleError {
    super::initialize::CandidateInitializeError::from(e).into()
}
fn receipt(plan: &CandidateActionPlanV3) -> CandidateActionReceipt {
    CandidateActionReceipt {
        operation_id: plan.scope.operation_id.clone(),
        step_id: plan.scope.parent_step_id.clone(),
        effects: plan.effects.iter().map(|e| e.step_id.clone()).collect(),
    }
}

#[derive(Clone, Debug)]
pub struct CandidateActionReceipt {
    operation_id: String,
    step_id: String,
    effects: Vec<String>,
}
impl CandidateActionReceipt {
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }
    pub fn step_id(&self) -> &str {
        &self.step_id
    }
    pub fn effect_ids(&self) -> &[String] {
        &self.effects
    }
}
impl crate::Store {
    /// Closed Fake collector factory. External input cannot select an origin.
    pub fn candidate_collector(
        &self,
        session: &CoordinatorSession,
        principal: &str,
        run: &str,
    ) -> Result<CandidateCollector, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, session)?;
        let snapshot = super::read_snapshot(&tx, principal, run)
            .map_err(creation_error)?
            .ok_or(LifecycleError::Invalid)?;
        QualificationProgram::resolve(snapshot.reviewed_manifest())?;
        let r = snapshot.receipt();
        Ok(CandidateCollector {
            run: run.into(),
            binding: r.binding_id().into(),
            incarnation: r.incarnation().into(),
            descriptor: super::DescriptorRefV1 {
                deployment_id: r.deployment_id().into(),
                revision: r.revision(),
                manifest_digest: r.manifest_digest().into(),
                recipe_fingerprint: r.recipe_fingerprint().into(),
            },
        })
    }
    pub fn record_candidate_effect(
        &self,
        session: &CoordinatorSession,
        collector: &CandidateCollector,
        child: &str,
        observation: &mllm_domain::qualification::EffectObservation,
        now: i64,
    ) -> Result<(), LifecycleError> {
        use crate::lifecycle::completion::{
            canonical_members, fresh, identity_dtos, nonempty_receipt, policy_ttl,
        };
        use mllm_domain::completion::Milestone;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session)?;
        let p = plan_for_step(&tx, child)?;
        validate_plan(&tx, &p)?;
        collector.validate(&p)?;
        let spec = p
            .effects
            .iter()
            .find(|e| e.step_id == child)
            .ok_or(LifecycleError::Conflict)?;
        let facts = warm::facts(spec.effect)?;
        if spec.effect == PersistedEffectKind::Probe
            || observation.token != child_token(&p, child)
            || observation.binding_id != p.scope.binding_id
            || observation.incarnation != p.scope.incarnation
            || observation.facts
                != facts
                    .iter()
                    .map(warm::milestone)
                    .collect::<Vec<Milestone>>()
        {
            return Err(LifecycleError::Conflict);
        }
        nonempty_receipt(&observation.receipt)?;
        let v = validated_anchor(&tx, &p.scope.parent_step_id)?;
        let a = warm::owned(&tx, &p)?;
        let identities = identity_dtos(&canonical_members(&observation.identities)?);
        if identities != a.identities {
            return Err(LifecycleError::Conflict);
        }
        let value = EffectEvidenceV3 {
            version: 3,
            kind: EffectEvidenceTag::Effect,
            scope: p.scope.clone(),
            effect: spec.clone(),
            origin: crate::qualification::recipe_v1::PROGRAM_REVISION.into(),
            identities,
            observed_at_ms: observation.observed_at_ms,
            receipt: observation.receipt.clone(),
            facts,
        };
        let prior: Option<String> = tx
            .query_row(
                "SELECT evidence_json FROM lifecycle_evidence WHERE step_id=?1",
                [child],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(prior) = prior {
            return if decode::<EffectEvidenceV3>(&prior)? == value {
                Ok(())
            } else {
                Err(LifecycleError::Conflict)
            };
        }
        current_anchor(&tx, session, &p.scope.parent_step_id)?;
        warm::predecessors(&tx, &p, spec, observation.observed_at_ms)?;
        fresh(
            v.context.issued_at_ms,
            p.deadline_ms,
            observation.observed_at_ms,
            now,
            policy_ttl(&tx, &p.scope.host)?,
        )?;
        let epoch = crate::resource_ledger::advance_completion_epoch(&tx)?;
        one(tx.execute(
            "UPDATE lifecycle_steps SET state='completed' WHERE id=?1 AND state='armed'",
            [child],
        )?)?;
        tx.execute(
            "INSERT INTO lifecycle_evidence VALUES(?1,?2,?3)",
            params![child, encode(&value)?, epoch],
        )?;
        validate_plan(&tx, &p)?;
        tx.commit()?;
        Ok(())
    }
    /// Reads a child command; this method never grants permission to send it.
    pub fn candidate_effect_execution(
        &self,
        session: &CoordinatorSession,
        child: &str,
    ) -> Result<
        (
            PersistedEffectKind,
            mllm_domain::completion::StepExecutionContext,
        ),
        LifecycleError,
    > {
        use mllm_domain::completion::{ExecutionIdentities, StepExecutionContext, TransitionToken};
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, session)?;
        let p = plan_for_step(&tx, child)?;
        validate_plan(&tx, &p)?;
        current(&tx, session, &p)?;
        let spec = p
            .effects
            .iter()
            .find(|e| e.step_id == child)
            .ok_or(LifecycleError::Unsupported)?;
        let (state, json): (String, String) = tx.query_row(
            "SELECT state,step_json FROM lifecycle_steps WHERE id=?1",
            [child],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if state != "armed"
            || spec.effect == PersistedEffectKind::Probe
            || p.action == Action::Security
        {
            return Err(LifecycleError::Conflict);
        }
        let e: ArmedEffectV3 = decode(&json)?;
        if p.action != Action::Initialize {
            return Ok((spec.effect, warm::execution(&tx, &p, spec, &e)?));
        }
        let s = p.scope;
        Ok((
            spec.effect,
            StepExecutionContext {
                token: TransitionToken {
                    deployment_id: s.deployment_id,
                    revision: s.revision,
                    generation: s.generation,
                    operation_id: s.operation_id,
                    step_id: child.into(),
                    qualification_id: s.qualification_id,
                },
                binding_id: s.binding_id,
                incarnation: s.incarnation,
                issued_at_ms: e.issued_at_ms,
                deadline_ms: spec.deadline_ms,
                identities: ExecutionIdentities::OwnedLaunch,
                completion_target: None,
                grant_id: Some(e.grant_id),
                launch_settings: Some(mllm_domain::launch::ProfileLaunchSettings::Fake(
                    mllm_domain::launch::FakeLaunchSettings,
                )),
            },
        ))
    }
    pub fn arm_candidate_effect(
        &self,
        session: &CoordinatorSession,
        child: &str,
        context: mllm_scheduler::residency::AdmissionContext<'_>,
    ) -> Result<super::initialize::ArmResult, LifecycleError> {
        use mllm_domain::resources::{MemoryLimit, ResourcePhase};
        if !super::ulid(child) {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session)?;
        let p = plan_for_step(&tx, child)?;
        validate_plan(&tx, &p)?;
        let cancelled: bool = tx.query_row(
            "SELECT state='cancelled' FROM lifecycle_steps WHERE id=?1",
            [child],
            |r| r.get(0),
        )?;
        if cancelled {
            return Ok(super::initialize::ArmResult::AlreadyRecorded);
        }
        if matches!(p.action, Action::Park | Action::Restore) {
            let result = warm::arm(&tx, session, &p, child, context)?;
            tx.commit()?;
            return Ok(result);
        }
        let effect = p
            .effects
            .iter()
            .find(|e| e.step_id == child)
            .ok_or(LifecycleError::Unsupported)?;
        if effect.effect != PersistedEffectKind::Initialize {
            return Err(LifecycleError::Unsupported);
        }
        let snapshot = super::read_snapshot(&tx, &p.scope.principal, &p.scope.run_id)
            .map_err(creation_error)?
            .ok_or(LifecycleError::CorruptStoredData)?;
        current(&tx, session, &p)?;
        let state: String = tx.query_row(
            "SELECT state FROM lifecycle_steps WHERE id=?1",
            [child],
            |r| r.get(0),
        )?;
        if matches!(state.as_str(), "armed" | "uncertain" | "completed") {
            return Ok(super::initialize::ArmResult::AlreadyRecorded);
        }
        if state != "planned"
            || context.now_ms < p.accepted_at_ms
            || context.now_ms >= p.deadline_ms
        {
            return Err(LifecycleError::Conflict);
        }
        super::initialize::eligible(&tx, &snapshot, false)?;
        let resource = super::initialize::policy(&tx, &snapshot)?;
        let qualification = read_candidate_policy(&tx, &p.scope.host)
            .map_err(super::map_qualification)
            .map_err(creation_error)?
            .ok_or(LifecycleError::Conflict)?;
        if resource.revision != p.scope.resource_policy_revision
            || qualification.revision != p.scope.qualification_policy_revision
        {
            return Err(LifecycleError::Stale);
        }
        let limits: Vec<_> = resource
            .controls
            .domains
            .iter()
            .map(|(domain, p)| MemoryLimit {
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
        {
            return Err(LifecycleError::Rejected(
                "resource policy context mismatch".into(),
            ));
        }
        let cold = super::initialize::footprint(
            snapshot
                .reviewed_manifest()
                .effective_recipe()
                .resources()
                .cold(),
            ResourcePhase::Cold,
        )?;
        let ledger = crate::resource_ledger::read_snapshot(&tx).map_err(resource_error)?;
        let grant_id = ulid::Ulid::new().to_string();
        crate::resource_ledger::reserve_increase_in_transaction(
            &tx,
            &crate::resource_ledger::GrantRequest {
                id: grant_id.clone(),
                deployment_id: p.scope.deployment_id.clone(),
                operation_id: p.scope.operation_id.clone(),
                revision: p.scope.revision,
                generation: p.scope.generation,
                expected_epoch: ledger.epoch,
                next: cold,
            },
            context,
        )
        .map_err(resource_error)?;
        let anchor = ArmedActionV3 {
            version: 3,
            kind: ArmedActionTag::Armed,
            plan: p.clone(),
            grant_id: grant_id.clone(),
            issued_at_ms: context.now_ms,
            expected_epoch: ledger.epoch,
        };
        let armed = ArmedEffectV3 {
            version: 3,
            kind: ArmedEffectTag::Armed,
            parent: p.scope.clone(),
            effect: effect.clone(),
            grant_id: grant_id.clone(),
            issued_at_ms: context.now_ms,
        };
        one(tx.execute("UPDATE lifecycle_steps SET state='armed',step_json=?1,grant_id=?2 WHERE id=?3 AND state='planned' AND grant_id IS NULL",params![encode(&anchor)?,grant_id,p.scope.parent_step_id])?)?;
        one(tx.execute("UPDATE lifecycle_steps SET state='armed',step_json=?1 WHERE id=?2 AND state='planned' AND grant_id IS NULL",params![encode(&armed)?,child])?)?;
        one(tx.execute(
            "UPDATE operations SET state='running' WHERE id=?1 AND state='pending'",
            [&p.scope.operation_id],
        )?)?;
        one(tx.execute(
            "UPDATE lifecycle_runs SET state='running' WHERE operation_id=?1 AND state='queued'",
            [&p.scope.operation_id],
        )?)?;
        one(tx.execute(
            "UPDATE runtime_bindings SET state='uncertain' WHERE id=?1 AND state='reserved'",
            [&p.scope.binding_id],
        )?)?;
        one(tx.execute(
            "UPDATE qualification_runs SET state='running' WHERE id=?1 AND state='accepted'",
            [&p.scope.run_id],
        )?)?;
        let event_id = |id: &str| {
            id.parse()
                .map(crate::events::EventOperationId::generated)
                .map_err(|_| LifecycleError::CorruptStoredData)
        };
        crate::events::append_event(
            &tx,
            &crate::events::EventMetadata::CandidateInitializeArmed {
                operation_id: event_id(&p.scope.operation_id)?,
                deployment_id: event_id(&p.scope.deployment_id)?,
                run_id: event_id(&p.scope.run_id)?,
                step_id: event_id(child)?,
                revision: p.scope.revision,
                generation: p.scope.generation,
                session_epoch: session.epoch(),
            },
        )
        .map_err(|e| match e {
            crate::events::EventWriteError::Sql(e) => LifecycleError::Sql(e),
            _ => LifecycleError::CorruptStoredData,
        })?;
        validate_plan(&tx, &p)?;
        tx.commit()?;
        Ok(super::initialize::ArmResult::New {
            step_id: child.into(),
        })
    }
    pub fn accept_candidate_action(
        &self,
        session: &CoordinatorSession,
        principal: &str,
        run: &str,
        key: &str,
        text: &str,
        now_ms: i64,
    ) -> Result<CandidateActionReceipt, LifecycleError> {
        if text.len() > super::MAX_BYTES
            || !super::valid_id(principal)
            || !super::valid_id(key)
            || !super::ulid(run)
            || now_ms < 0
        {
            return Err(LifecycleError::Invalid);
        }
        let command: Command = serde_json::from_str(text).map_err(|_| LifecycleError::Invalid)?;
        let scope = format!("POST /management/v1/qualification-runs/{run}/actions");
        let hash = format!(
            "{:x}",
            Sha256::digest(encode(&(3_u8, principal, &scope, &command))?)
        );
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session)?;
        let prior: Option<(String,String,String)> = tx.query_row("SELECT request_hash,operation_id,response_json FROM command_receipts WHERE principal_id=?1 AND command_scope=?2 AND idempotency_key=?3",params![principal,scope,key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        if let Some((stored, operation, json)) = prior {
            if stored != hash {
                return Err(LifecycleError::Conflict);
            }
            let r: ReceiptV3 = decode(&json)?;
            if r.version != 3
                || r.principal != principal
                || r.scope != scope
                || r.key != key
                || r.request_hash != hash
                || r.plan.scope.operation_id != operation
                || r.plan.scope.run_id != run
            {
                return Err(LifecycleError::CorruptStoredData);
            }
            validate_plan(&tx, &r.plan)?;
            return Ok(receipt(&r.plan));
        }
        let snapshot = super::read_snapshot(&tx, principal, run)
            .map_err(creation_error)?
            .ok_or(LifecycleError::Invalid)?;
        QualificationProgram::resolve(snapshot.reviewed_manifest())?;
        let r = snapshot.receipt();
        if command.expected_revision != r.revision() {
            return Err(LifecycleError::Stale);
        }
        // The integrated writer never upgrades a legacy action. Later cases require
        // the closed evaluator's prior-case coverage before they can be selected.
        if command.action != WireAction::Initialize {
            let p = warm::accept(
                &tx, session, &snapshot, &command, principal, &scope, key, &hash, now_ms,
            )?;
            validate_plan(&tx, &p)?;
            tx.commit()?;
            return Ok(receipt(&p));
        }
        super::initialize::eligible(&tx, &snapshot, true)?;
        let resource = super::initialize::policy(&tx, &snapshot)?;
        let qualification = read_candidate_policy(&tx, r.host_id())
            .map_err(super::map_qualification)
            .map_err(creation_error)?
            .ok_or(LifecycleError::Conflict)?;
        if now_ms < r.accepted_at_ms()
            || command.deadline_ms <= now_ms
            || command.deadline_ms > r.deadline_ms()
        {
            return Err(LifecycleError::Invalid);
        }
        let cases = snapshot.reviewed_manifest().cases();
        let first = cases.first().ok_or(LifecycleError::CorruptStoredData)?;
        let probe = cases.get(1).ok_or(LifecycleError::CorruptStoredData)?;
        if first.kind() != CandidateCaseKind::ColdInitialize
            || probe.kind() != CandidateCaseKind::ReadyProbe
            || first.cycle() != 0
            || probe.cycle() != 0
        {
            return Err(LifecycleError::CorruptStoredData);
        }
        let operation = ulid::Ulid::new();
        let anchor = ulid::Ulid::new();
        let launch = ulid::Ulid::new().to_string();
        let probe_id = ulid::Ulid::new().to_string();
        let p = CandidateActionPlanV3 {
            version: 3,
            scope: Scope {
                principal: principal.into(),
                host: r.host_id().into(),
                run_id: run.into(),
                creation_operation_id: r.operation_id().into(),
                operation_id: operation.to_string(),
                parent_step_id: anchor.to_string(),
                deployment_id: r.deployment_id().into(),
                revision: r.revision(),
                generation: r.generation(),
                session_id: session.id().into(),
                binding_id: r.binding_id().into(),
                incarnation: r.incarnation().into(),
                qualification_id: format!("candidate:{run}"),
                descriptor: super::DescriptorRefV1 {
                    deployment_id: r.deployment_id().into(),
                    revision: r.revision(),
                    manifest_digest: r.manifest_digest().into(),
                    recipe_fingerprint: r.recipe_fingerprint().into(),
                },
                resource_policy_revision: resource.revision,
                qualification_policy_revision: qualification.revision,
            },
            action: Action::Initialize,
            case_id: first.id().into(),
            case_kind: first.kind(),
            cycle: 0,
            accepted_at_ms: now_ms,
            deadline_ms: command.deadline_ms,
            completion_target: snapshot
                .reviewed_manifest()
                .effective_recipe()
                .resources()
                .ready()
                .clone(),
            effects: vec![
                EffectSpec {
                    step_id: launch.clone(),
                    ordinal: 1,
                    effect: PersistedEffectKind::Initialize,
                    predecessor: None,
                    required_facts: vec![],
                    request_case: None,
                    request_item: None,
                    request_subcheck: None,
                    deadline_ms: command.deadline_ms,
                },
                EffectSpec {
                    step_id: probe_id.clone(),
                    ordinal: 2,
                    effect: PersistedEffectKind::Probe,
                    predecessor: Some(launch),
                    required_facts: vec![
                        Fact::AllocationsRestored,
                        Fact::WeightsUsable,
                        Fact::CacheValid,
                    ],
                    request_case: Some(probe.id().into()),
                    request_item: Some(0),
                    request_subcheck: Some(String::new()),
                    deadline_ms: command.deadline_ms,
                },
            ],
            ready_probe_case: Some(probe.id().into()),
        };
        tx.execute("INSERT INTO operations(id,deployment_id,kind,state) VALUES(?1,?2,'candidate_action_v3','pending')",params![p.scope.operation_id,p.scope.deployment_id])?;
        tx.execute("INSERT INTO lifecycle_runs(operation_id,deployment_id,revision,generation,session_id,action,state,deadline_ms,plan_json) VALUES(?1,?2,?3,?4,?5,'activate','queued',?6,?7)",params![p.scope.operation_id,p.scope.deployment_id,p.scope.revision,p.scope.generation,p.scope.session_id,p.deadline_ms,encode(&p)?])?;
        tx.execute("INSERT INTO lifecycle_claims(deployment_id,operation_id,revision,generation) VALUES(?1,?2,?3,?4)",params![p.scope.deployment_id,p.scope.operation_id,p.scope.revision,p.scope.generation])?;
        let anchor_json = encode(&PlannedActionV3 {
            version: 3,
            kind: ActionTag::Planned,
            plan: p.clone(),
        })?;
        tx.execute("INSERT INTO lifecycle_steps(id,operation_id,ordinal,deployment_id,binding_id,session_id,state,step_json) VALUES(?1,?2,0,?3,?4,?5,'planned',?6)",params![p.scope.parent_step_id,p.scope.operation_id,p.scope.deployment_id,p.scope.binding_id,p.scope.session_id,anchor_json])?;
        for effect in &p.effects {
            let json = encode(&PlannedEffectV3 {
                version: 3,
                kind: EffectTag::Planned,
                parent: p.scope.clone(),
                effect: effect.clone(),
            })?;
            tx.execute("INSERT INTO lifecycle_steps(id,operation_id,ordinal,deployment_id,binding_id,session_id,state,step_json) VALUES(?1,?2,?3,?4,?5,?6,'planned',?7)",params![effect.step_id,p.scope.operation_id,effect.ordinal,p.scope.deployment_id,p.scope.binding_id,p.scope.session_id,json])?;
        }
        tx.execute("INSERT INTO qualification_case_actions(run_id,case_id,operation_id,step_id) VALUES(?1,?2,?3,?4)",params![run,p.case_id,p.scope.operation_id,p.scope.parent_step_id])?;
        let link = ProbeLinkV3 {
            version: 3,
            cycle: 0,
            descriptor: p.scope.descriptor.clone(),
        };
        tx.execute("INSERT INTO qualification_ready_probes(run_id,case_id,parent_operation_id,parent_step_id,probe_step_id,linkage_json) VALUES(?1,?2,?3,?4,?5,?6)",params![run,probe.id(),p.scope.operation_id,p.scope.parent_step_id,probe_id,encode(&link)?])?;
        let response = ReceiptV3 {
            version: 3,
            principal: principal.into(),
            scope: scope.clone(),
            key: key.into(),
            request_hash: hash.clone(),
            plan: p.clone(),
        };
        tx.execute("INSERT INTO command_receipts(principal_id,command_scope,idempotency_key,request_hash,operation_id,response_json) VALUES(?1,?2,?3,?4,?5,?6)",params![principal,scope,key,hash,p.scope.operation_id,encode(&response)?])?;
        crate::events::append_event(
            &tx,
            &crate::events::EventMetadata::CandidateInitializeAccepted {
                operation_id: crate::events::EventOperationId::generated(operation),
                deployment_id: crate::events::EventOperationId::generated(
                    r.deployment_id()
                        .parse()
                        .map_err(|_| LifecycleError::CorruptStoredData)?,
                ),
                run_id: crate::events::EventOperationId::generated(
                    run.parse().map_err(|_| LifecycleError::CorruptStoredData)?,
                ),
                step_id: crate::events::EventOperationId::generated(anchor),
                revision: r.revision(),
                generation: r.generation(),
                session_epoch: session.epoch(),
            },
        )
        .map_err(|e| match e {
            crate::events::EventWriteError::Sql(e) => LifecycleError::Sql(e),
            _ => LifecycleError::CorruptStoredData,
        })?;
        validate_plan(&tx, &p)?;
        tx.commit()?;
        Ok(receipt(&p))
    }
}

#[derive(Debug)]
pub struct CandidateCollector {
    run: String,
    binding: String,
    incarnation: String,
    descriptor: super::DescriptorRefV1,
}
impl CandidateCollector {
    fn validate(&self, p: &CandidateActionPlanV3) -> Result<(), LifecycleError> {
        if self.run != p.scope.run_id
            || self.binding != p.scope.binding_id
            || self.incarnation != p.scope.incarnation
            || self.descriptor != p.scope.descriptor
        {
            return Err(LifecycleError::Conflict);
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum EffectEvidenceTag {
    #[serde(rename = "candidate_effect_observed")]
    Effect,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EffectEvidenceV3 {
    version: u8,
    kind: EffectEvidenceTag,
    scope: Scope,
    effect: EffectSpec,
    origin: String,
    identities: Vec<crate::lifecycle::IdentityDto>,
    observed_at_ms: i64,
    receipt: String,
    facts: Vec<Fact>,
}
fn child_token(p: &CandidateActionPlanV3, id: &str) -> mllm_domain::completion::TransitionToken {
    mllm_domain::completion::TransitionToken {
        deployment_id: p.scope.deployment_id.clone(),
        revision: p.scope.revision,
        generation: p.scope.generation,
        operation_id: p.scope.operation_id.clone(),
        step_id: id.into(),
        qualification_id: p.scope.qualification_id.clone(),
    }
}

fn one(count: usize) -> Result<(), LifecycleError> {
    if count == 1 {
        Ok(())
    } else {
        Err(LifecycleError::Conflict)
    }
}
fn resource_error(error: crate::resource_ledger::ResourceStoreError) -> LifecycleError {
    match error {
        crate::resource_ledger::ResourceStoreError::Sql(e) => LifecycleError::Sql(e),
        e => LifecycleError::Rejected(e.to_string()),
    }
}
fn plan_for_step(
    tx: &Transaction<'_>,
    child: &str,
) -> Result<CandidateActionPlanV3, LifecycleError> {
    let json:Option<String> = tx.query_row("SELECT r.plan_json FROM lifecycle_steps s JOIN lifecycle_runs r ON r.operation_id=s.operation_id WHERE s.id=?1",[child],|r|r.get(0)).optional()?;
    decode(&json.ok_or(LifecycleError::Invalid)?)
}

/// Explicit family dispatch; V2 completion must never use this decoder.
pub(crate) fn is_v3(tx: &Transaction<'_>, id: &str) -> Result<bool, LifecycleError> {
    Ok(tx.query_row("SELECT EXISTS(SELECT 1 FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id WHERE s.id=?1 AND o.kind='candidate_action_v3')", [id], |r| r.get(0))?)
}

pub(crate) fn validated_anchor(
    tx: &Transaction<'_>,
    id: &str,
) -> Result<super::initialize::ValidatedInitialize, LifecycleError> {
    anchor_context(tx, id, true)
}

/// Cleanup validates its own retained/released accounting. Its Initialize reader
/// must not recursively reenter the cleanup history that establishes that accounting.
pub(crate) fn immutable_initialize_anchor(
    tx: &Transaction<'_>,
    id: &str,
) -> Result<super::initialize::ValidatedInitialize, LifecycleError> {
    let p = plan_for_step(tx, id)?;
    if p.action != Action::Initialize {
        return Err(LifecycleError::CorruptStoredData);
    }
    anchor_context(tx, id, false)
}

fn immutable_anchor(
    tx: &Transaction<'_>,
    id: &str,
) -> Result<super::initialize::ValidatedInitialize, LifecycleError> {
    anchor_context(tx, id, false)
}

/// Proves the exact candidate action handed to cleanup. V1/V2 retain their
/// original Initialize-only contract; V3 may hand off its current warm parent.
pub(crate) fn validate_cleanup_predecessor(
    tx: &Transaction<'_>,
    initialize: &super::initialize::ValidatedInitialize,
    operation: &str,
) -> Result<(), LifecycleError> {
    if operation == initialize.context.token.operation_id {
        return Ok(());
    }
    if !is_v3(tx, &initialize.context.token.step_id)? {
        return Err(LifecycleError::Conflict);
    }
    let id: String = tx.query_row(
        "SELECT step_id FROM qualification_case_actions WHERE operation_id=?1 AND run_id=?2",
        params![operation, initialize.snapshot.receipt().run_id()],
        |r| r.get(0),
    )?;
    let p = plan_for_step(tx, &id)?;
    let cold = plan_for_step(tx, &initialize.context.token.step_id)?;
    let mut expected = cold.scope.clone();
    expected.operation_id = operation.into();
    expected.parent_step_id = id;
    if p.scope != expected || !matches!(p.action, Action::Park | Action::Restore | Action::Security)
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    validate_plan_inner(tx, &p, false)
}

fn anchor_context(
    tx: &Transaction<'_>,
    id: &str,
    check_accounting: bool,
) -> Result<super::initialize::ValidatedInitialize, LifecycleError> {
    use mllm_domain::completion::{ExecutionIdentities, StepExecutionContext, TransitionToken};
    let p = plan_for_step(tx, id)?;
    if p.scope.parent_step_id != id {
        return Err(LifecycleError::CorruptStoredData);
    }
    validate_plan_inner(tx, &p, check_accounting)?;
    let (state, raw): (String, String) = tx.query_row(
        "SELECT state,step_json FROM lifecycle_steps WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if state == "planned" {
        return Err(LifecycleError::Conflict);
    }
    let a: ArmedActionV3 = decode(&raw)?;
    let snapshot = super::read_snapshot(tx, &p.scope.principal, &p.scope.run_id)
        .map_err(creation_error)?
        .ok_or(LifecycleError::CorruptStoredData)?;
    let run_state = tx.query_row(
        "SELECT state FROM lifecycle_runs WHERE operation_id=?1",
        [&p.scope.operation_id],
        |r| r.get(0),
    )?;
    let s = p.scope;
    Ok(super::initialize::ValidatedInitialize {
        snapshot,
        session_id: s.session_id,
        state,
        run_state,
        context: StepExecutionContext {
            token: TransitionToken {
                deployment_id: s.deployment_id,
                revision: s.revision,
                generation: s.generation,
                operation_id: s.operation_id,
                step_id: s.parent_step_id,
                qualification_id: s.qualification_id,
            },
            binding_id: s.binding_id,
            incarnation: s.incarnation,
            issued_at_ms: a.issued_at_ms,
            deadline_ms: p.deadline_ms,
            identities: ExecutionIdentities::OwnedLaunch,
            completion_target: Some(super::initialize::footprint(
                &p.completion_target,
                if p.action == Action::Park {
                    mllm_domain::resources::ResourcePhase::Parked
                } else {
                    mllm_domain::resources::ResourcePhase::Ready
                },
            )?),
            grant_id: Some(a.grant_id),
            launch_settings: Some(mllm_domain::launch::ProfileLaunchSettings::Fake(
                mllm_domain::launch::FakeLaunchSettings,
            )),
        },
    })
}

pub(crate) fn current_anchor(
    tx: &Transaction<'_>,
    session: &CoordinatorSession,
    id: &str,
) -> Result<(), LifecycleError> {
    let p = plan_for_step(tx, id)?;
    let v = validated_anchor(tx, id)?;
    current(tx, session, &p)?;
    if v.state != "armed"
        || v.run_state != "running"
        || v.snapshot.state() != super::CandidateRunState::Running
    {
        return Err(LifecycleError::Conflict);
    }
    super::initialize::policy(tx, &v.snapshot)?;
    Ok(())
}
fn current(
    tx: &Transaction<'_>,
    session: &CoordinatorSession,
    p: &CandidateActionPlanV3,
) -> Result<(), LifecycleError> {
    if p.scope.session_id != session.id() {
        return Err(LifecycleError::Stale);
    }
    let fenced:bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM deployments d JOIN lifecycle_claims c ON c.deployment_id=d.id WHERE d.id=?1 AND d.revision=?2 AND d.current_generation=?3 AND c.revision=?2 AND c.generation=?3 AND c.operation_id=?4 AND d.desired_state='stopped' AND d.admission_enabled=0 AND d.dispatch_enabled=0)",params![p.scope.deployment_id,p.scope.revision,p.scope.generation,p.scope.operation_id],|r|r.get(0))?;
    if !fenced {
        return Err(LifecycleError::Stale);
    }
    Ok(())
}

fn validate_plan(tx: &Transaction<'_>, p: &CandidateActionPlanV3) -> Result<(), LifecycleError> {
    validate_plan_inner(tx, p, true)
}

fn validate_accounting(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
) -> Result<(), LifecycleError> {
    let ledger = crate::resource_ledger::read_snapshot(tx).map_err(resource_error)?;
    if let Some(owner) = ledger.owners.get(&p.scope.deployment_id) {
        if owner != &warm::reservation_at(tx, p, ledger.epoch)? {
            return Err(LifecycleError::CorruptStoredData);
        }
        let retained: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_bindings WHERE id=?1 AND state!='released') AND (SELECT COUNT(*) FROM endpoint_leases WHERE binding_id=?1)=1", [&p.scope.binding_id], |r| r.get(0))?;
        if !retained {
            return Err(LifecycleError::CorruptStoredData);
        }
        Ok(())
    } else {
        let cold = warm::cold(tx, &p.scope.run_id)?;
        let v = immutable_initialize_anchor(tx, &cold.scope.parent_step_id)?;
        super::cleanup::validate_gone_history(tx, &v)
    }
}

fn validate_plan_inner(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
    check_accounting: bool,
) -> Result<(), LifecycleError> {
    if p.action == Action::Security {
        inference::security::validate(tx, p)?;
        return if check_accounting {
            validate_accounting(tx, p)
        } else {
            Ok(())
        };
    }
    let bad = || LifecycleError::CorruptStoredData;
    let s = &p.scope;
    let snapshot = super::read_snapshot(tx, &s.principal, &s.run_id)
        .map_err(creation_error)?
        .ok_or_else(bad)?;
    let r = snapshot.receipt();
    QualificationProgram::resolve(snapshot.reviewed_manifest()).map_err(|_| bad())?;
    let cases = snapshot.reviewed_manifest().cases();
    let case_index = cases
        .iter()
        .position(|c| c.id() == p.case_id)
        .ok_or_else(bad)?;
    let expected_kind = match p.action {
        Action::Initialize => CandidateCaseKind::ColdInitialize,
        Action::Park => CandidateCaseKind::Park,
        Action::Restore => CandidateCaseKind::Restore,
        Action::Security => return Err(bad()),
    };
    let probe_case = if p.action == Action::Park {
        None
    } else {
        Some(cases.get(case_index + 1).ok_or_else(bad)?.id().to_owned())
    };
    let resources = snapshot.reviewed_manifest().effective_recipe().resources();
    if p.version != 3
        || p.case_kind != expected_kind
        || cases[case_index].kind() != expected_kind
        || p.cycle != cases[case_index].cycle()
        || (p.action == Action::Initialize && case_index != 0)
        || p.ready_probe_case != probe_case
        || s.principal != r.inner.principal_id
        || s.host != r.host_id()
        || s.creation_operation_id != r.operation_id()
        || s.deployment_id != r.deployment_id()
        || s.revision != r.revision()
        || s.generation != r.generation()
        || s.binding_id != r.binding_id()
        || s.incarnation != r.incarnation()
        || s.qualification_id != format!("candidate:{}", s.run_id)
        || s.descriptor.deployment_id != s.deployment_id
        || s.descriptor.revision != s.revision
        || s.descriptor.manifest_digest != r.manifest_digest()
        || s.descriptor.recipe_fingerprint != r.recipe_fingerprint()
        || s.resource_policy_revision <= 0
        || s.qualification_policy_revision <= 0
        || !super::ulid(&s.operation_id)
        || !super::ulid(&s.parent_step_id)
        || p.accepted_at_ms < r.accepted_at_ms()
        || p.deadline_ms <= p.accepted_at_ms
        || p.deadline_ms > r.deadline_ms()
        || p.completion_target
            != *if p.action == Action::Park {
                resources.parked()
            } else {
                resources.ready()
            }
        || p.effects.len() != if p.action == Action::Restore { 4 } else { 2 }
    {
        return Err(bad());
    }
    warm::validate_specs(p)?;
    if p.action != Action::Initialize {
        warm::prior(tx, p)?;
    }
    let mut row: (String,String,i64,i64,String,String,i64,String) = tx.query_row("SELECT deployment_id,session_id,revision,generation,action,state,deadline_ms,plan_json FROM lifecycle_runs WHERE operation_id=?1",[&s.operation_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?)))?;
    let handoff = crate::lifecycle::candidate_handoff_states(tx, &s.run_id, &s.operation_id)?;
    let cleanup_resolved = handoff.is_some() && row.5 == "failed";
    if let Some(history) = &handoff {
        if !matches!(row.5.as_str(), "uncertain" | "failed") {
            return Err(bad());
        }
        row.5 = match history.first().map(|(_, state)| state.as_str()) {
            Some("planned") => "queued",
            Some("armed") => "running",
            Some("uncertain") => "uncertain",
            _ => return Err(bad()),
        }
        .into();
    }
    if row.0 != s.deployment_id
        || row.1 != s.session_id
        || row.2 != s.revision
        || row.3 != s.generation
        || row.4
            != if p.action == Action::Park {
                "park"
            } else {
                "activate"
            }
        || !matches!(
            row.5.as_str(),
            "queued" | "running" | "uncertain" | "succeeded"
        )
        || row.6 != p.deadline_ms
        || decode::<CandidateActionPlanV3>(&row.7)? != *p
    {
        return Err(bad());
    }
    let operation_state = if row.5 == "queued" {
        "pending"
    } else if row.5 == "succeeded" {
        "succeeded"
    } else {
        "running"
    };
    let operation_matches: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1 AND deployment_id=?2 AND kind='candidate_action_v3' AND state=?3 AND idempotency_key IS NULL AND error_code IS ?4)",params![s.operation_id,s.deployment_id,if cleanup_resolved {"failed"} else {operation_state},cleanup_resolved.then_some("resolved_by_owned_cleanup")],|r|r.get(0))?;
    if !operation_matches || !super::ulid(&s.session_id) {
        return Err(bad());
    }
    let scope = format!(
        "POST /management/v1/qualification-runs/{}/actions",
        s.run_id
    );
    let command = Command {
        expected_revision: s.revision,
        action: match p.action {
            Action::Initialize => WireAction::Initialize,
            Action::Park => WireAction::Park,
            Action::Restore => WireAction::Restore,
            Action::Security => return Err(bad()),
        },
        deadline_ms: p.deadline_ms,
    };
    let expected_hash = format!(
        "{:x}",
        Sha256::digest(encode(&(3_u8, &s.principal, &scope, &command))?)
    );
    let mut receipt_statement = tx.prepare("SELECT principal_id,command_scope,idempotency_key,request_hash,response_json FROM command_receipts WHERE operation_id=?1")?;
    let receipts = receipt_statement
        .query_map([&s.operation_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if receipts.len() != 1 {
        return Err(bad());
    }
    let (principal, stored_scope, key, hash, json) = &receipts[0];
    let receipt: ReceiptV3 = decode(json)?;
    if principal != &s.principal
        || stored_scope != &scope
        || hash != &expected_hash
        || !super::valid_id(key)
        || receipt.version != 3
        || receipt.principal != *principal
        || receipt.scope != scope
        || receipt.key != *key
        || receipt.request_hash != *hash
        || receipt.plan != *p
    {
        return Err(bad());
    }
    let mut statement = tx.prepare("SELECT id,ordinal,deployment_id,binding_id,session_id,state,step_json,grant_id FROM lifecycle_steps WHERE operation_id=?1 ORDER BY ordinal")?;
    let mut rows = statement
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
    if rows.len() != p.effects.len() + 1 {
        return Err(bad());
    }
    if let Some(history) = &handoff {
        if history.len() != rows.len() {
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
            row.5 = original.clone();
        }
    }
    let mut execution: Option<ArmedActionV3> = None;
    for (index, row) in rows.iter().enumerate() {
        if row.1 != index as u32
            || row.2 != s.deployment_id
            || row.3 != s.binding_id
            || row.4 != s.session_id
            || !matches!(
                row.5.as_str(),
                "planned" | "armed" | "uncertain" | "completed"
            )
        {
            return Err(bad());
        }
        if index == 0 {
            if row.0 != s.parent_step_id {
                return Err(bad());
            }
            if row.5 == "planned" {
                let anchor: PlannedActionV3 = decode(&row.6)?;
                if anchor.version != 3
                    || anchor.plan != *p
                    || row.7.is_some()
                    || operation_state != "pending"
                {
                    return Err(bad());
                }
            } else {
                let anchor: ArmedActionV3 = decode(&row.6)?;
                if anchor.version != 3
                    || anchor.plan != *p
                    || row.7.as_deref() != Some(anchor.grant_id.as_str())
                    || !super::ulid(&anchor.grant_id)
                    || anchor.issued_at_ms < p.accepted_at_ms
                    || anchor.issued_at_ms >= p.deadline_ms
                    || !matches!(operation_state, "running" | "succeeded")
                {
                    return Err(bad());
                }
                let grant: (String,String,i64,String) = tx.query_row("SELECT deployment_id,operation_id,committed_epoch,request_json FROM resource_grants WHERE id=?1",[&anchor.grant_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
                let request: (String, String, i64, i64, u64, String) = decode(&grant.3)?;
                if grant.0 != s.deployment_id
                    || grant.1 != s.operation_id
                    || u64::try_from(grant.2).ok() != anchor.expected_epoch.checked_add(1)
                    || request.0 != s.deployment_id
                    || request.1 != s.operation_id
                    || request.2 != s.revision
                    || request.3 != s.generation
                    || request.4 != anchor.expected_epoch
                {
                    return Err(bad());
                }
                let expected = warm::reservation_at(tx, p, grant.2 as u64)?;
                // Validate the original grant payload independently from the retained owner.
                if crate::resource_ledger::decode(&request.5).map_err(resource_error)? != expected {
                    return Err(bad());
                }
                if check_accounting {
                    validate_accounting(tx, p)?;
                }
                execution = Some(anchor);
            }
        } else {
            if row.0 != p.effects[index - 1].step_id || row.7.is_some() {
                return Err(bad());
            }
            if row.5 == "planned" {
                let child: PlannedEffectV3 = decode(&row.6)?;
                if child.version != 3
                    || child.parent != *s
                    || child.effect != p.effects[index - 1]
                    || (index == 1 && execution.is_some())
                {
                    return Err(bad());
                }
            } else {
                let child: ArmedEffectV3 = decode(&row.6)?;
                let anchor = execution.as_ref().ok_or_else(bad)?;
                if child.version != 3
                    || child.parent != *s
                    || child.effect != p.effects[index - 1]
                    || child.grant_id != anchor.grant_id
                    || (index == 1 && child.issued_at_ms != anchor.issued_at_ms)
                    || child.issued_at_ms < anchor.issued_at_ms
                    || child.issued_at_ms >= p.deadline_ms
                    || (index > 1 && rows[index - 1].5 != "completed")
                {
                    return Err(bad());
                }
                if child.effect.effect != PersistedEffectKind::Probe && row.5 == "completed" {
                    let (raw,epoch):(String,u64) = tx.query_row("SELECT evidence_json,committed_epoch FROM lifecycle_evidence WHERE step_id=?1", [&row.0], |r|Ok((r.get(0)?,r.get(1)?)))?;
                    let e: EffectEvidenceV3 = decode(&raw)?;
                    if e.version != 3
                        || e.scope != *s
                        || e.effect != child.effect
                        || e.origin != crate::qualification::recipe_v1::PROGRAM_REVISION
                        || e.facts != warm::facts(child.effect.effect)?
                        || e.observed_at_ms < child.issued_at_ms
                        || e.observed_at_ms > p.deadline_ms
                        || epoch <= anchor.expected_epoch + 1
                        || epoch
                            > crate::resource_ledger::read_snapshot(tx)
                                .map_err(resource_error)?
                                .epoch
                    {
                        return Err(bad());
                    }
                    crate::lifecycle::completion::members(&e.identities)?;
                    crate::lifecycle::completion::nonempty_receipt(&e.receipt)?;
                }
            }
        }
    }
    if p.action == Action::Park {
        warm::validate_park(
            tx,
            p,
            &rows.iter().map(|r| r.5.as_str()).collect::<Vec<_>>(),
            &row.5,
        )?;
    } else {
        inference::validate_progress(
            tx,
            p,
            &rows.iter().map(|r| r.5.as_str()).collect::<Vec<_>>(),
            &row.5,
            cleanup_resolved,
        )?;
    }
    if handoff.is_some() && check_accounting {
        validate_accounting(tx, p)?;
    }
    let associated: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM qualification_case_actions WHERE run_id=?1 AND case_id=?2 AND operation_id=?3 AND step_id=?4)",params![s.run_id,p.case_id,s.operation_id,s.parent_step_id],|r|r.get(0))?;
    if !associated {
        return Err(bad());
    }
    if p.action == Action::Park {
        let count:u32=tx.query_row("SELECT COUNT(*) FROM qualification_ready_probes WHERE parent_operation_id=?1 OR parent_step_id=?2",params![s.operation_id,s.parent_step_id],|r|r.get(0))?;
        return if count == 0 { Ok(()) } else { Err(bad()) };
    }
    let link: (String,String,String,String,String) = tx.query_row("SELECT case_id,parent_operation_id,parent_step_id,probe_step_id,linkage_json FROM qualification_ready_probes WHERE run_id=?1 AND case_id=?2",params![s.run_id,p.ready_probe_case],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
    let payload: ProbeLinkV3 = decode(&link.4)?;
    if Some(link.0.as_str()) != p.ready_probe_case.as_deref()
        || link.1 != s.operation_id
        || link.2 != s.parent_step_id
        || link.3 != p.effects.last().ok_or_else(bad)?.step_id
        || payload.version != 3
        || payload.cycle != p.cycle
        || payload.descriptor != s.descriptor
    {
        return Err(bad());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate_creation::tests::{command, fixture, setup};
    use mllm_config::effective::candidate::validate_candidate_reviewed_snapshot_text;
    use serde_json::Value;

    fn created() -> (
        crate::Store,
        CoordinatorSession,
        super::super::CandidateCreationReceipt,
    ) {
        created_with(|_| {})
    }
    fn created_with(
        edit: impl FnOnce(&mut Value),
    ) -> (
        crate::Store,
        CoordinatorSession,
        super::super::CandidateCreationReceipt,
    ) {
        let (_, mut host, mut policy) = fixture("fake");
        host["runtime_profiles"]["local"]["build_fingerprint"] =
            serde_json::json!("qualification-fake-v1");
        host["runtime_profiles"]["local"]["security"]["admin_credential_ref"] =
            serde_json::json!("secret://admin-key");
        let mut manifest: Value = serde_json::from_str(include_str!(
            "../../../mllm-config/tests/fixtures/candidate-fake-qualification.json"
        ))
        .unwrap();
        edit(&mut manifest);
        let reviewed = validate_candidate_reviewed_snapshot_text(&manifest.to_string()).unwrap();
        policy
            .qualification_policy
            .as_mut()
            .unwrap()
            .allowed_manifest_digests = vec![reviewed.manifest_digest().into()];
        let store = crate::Store::open_in_memory().unwrap();
        let session = setup(&store, &policy);
        let created = store
            .create_candidate_run(
                &session,
                "owner",
                "create",
                &command(&manifest),
                &host,
                1000,
            )
            .unwrap();
        (store, session, created)
    }
    #[test]
    fn frozen_request_limits_reject_initialize_before_any_effect_or_spending() {
        for field in [
            "max_request_body_bytes",
            "max_input_tokens_per_request",
            "max_output_tokens_per_request",
        ] {
            let (store, session, created) =
                created_with(|v| v["limits"][field] = serde_json::json!(1));
            assert!(
                matches!(
                    store.accept_candidate_action(
                        &session,
                        "owner",
                        created.run_id(),
                        "initialize",
                        r#"{"expected_revision":1,"action":"initialize","deadline_ms":400000}"#,
                        1100
                    ),
                    Err(LifecycleError::Unsupported)
                ),
                "{field}"
            );
            let counts:(i64,i64,i64)=store.conn.query_row("SELECT (SELECT COUNT(*) FROM lifecycle_steps),(SELECT COUNT(*) FROM qualification_request_attempts),requests_used FROM qualification_runs",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
            assert_eq!(counts, (0, 0, 0));
        }
    }
    #[test]
    fn v3_initialize_freezes_two_children_and_reserves_linked_probe_without_spending() {
        let (store, session, created) = created();
        let body = r#"{"expected_revision":1,"action":"initialize","deadline_ms":400000}"#;
        let accepted = store
            .accept_candidate_action(
                &session,
                "owner",
                created.run_id(),
                "initialize",
                body,
                1100,
            )
            .unwrap();
        assert_eq!(accepted.effect_ids().len(), 2);
        let steps: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM lifecycle_steps WHERE operation_id=?1",
                [accepted.operation_id()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(steps, 3);
        let probe: (String, String) = store
            .conn
            .query_row(
                "SELECT case_id,probe_step_id FROM qualification_ready_probes WHERE run_id=?1",
                [created.run_id()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            probe,
            ("ready_probe-0".into(), accepted.effect_ids()[1].clone())
        );
        assert_eq!(
            store
                .candidate_run_snapshot("owner", created.run_id())
                .unwrap()
                .unwrap()
                .requests_used(),
            0
        );
        let again = store
            .accept_candidate_action(
                &session,
                "owner",
                created.run_id(),
                "initialize",
                body,
                600000,
            )
            .unwrap();
        assert_eq!(again.operation_id(), accepted.operation_id());
    }

    #[test]
    fn v3_replay_rejects_wrong_request_operation_kind() {
        let (store, session, created) = created();
        let body = r#"{"expected_revision":1,"action":"initialize","deadline_ms":400000}"#;
        let accepted = store
            .accept_candidate_action(&session, "owner", created.run_id(), "init", body, 1100)
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE operations SET kind='candidate_initialize' WHERE id=?1",
                [accepted.operation_id()],
            )
            .unwrap();
        assert!(matches!(
            store.accept_candidate_action(&session, "owner", created.run_id(), "init", body, 1200),
            Err(LifecycleError::CorruptStoredData)
        ));
    }

    #[test]
    fn v3_first_child_arms_once_with_one_parent_grant_and_retained_claim() {
        use mllm_domain::resources::{MemoryLimit, MemoryObservation};
        use mllm_scheduler::residency::AdmissionContext;
        let (store, session, created) = created();
        let body = r#"{"expected_revision":1,"action":"initialize","deadline_ms":400000}"#;
        let accepted = store
            .accept_candidate_action(&session, "owner", created.run_id(), "init", body, 1100)
            .unwrap();
        let resource = store.resource_policy("lab").unwrap().unwrap();
        let limits: Vec<_> = resource
            .controls
            .domains
            .iter()
            .map(|(domain, p)| MemoryLimit {
                domain: domain.clone(),
                managed_bytes: p.managed_limit,
                free_reserve_bytes: p.free_reserve,
                host_kv_bytes: p.host_kv_limit,
                parked_bytes: p.parked_limit,
            })
            .collect();
        let observations: Vec<_> = limits
            .iter()
            .map(|l| MemoryObservation {
                domain: l.domain.clone(),
                capacity_bytes: 1_i64 << 50,
                available_bytes: 1_i64 << 50,
                sampled_at_ms: 1200,
            })
            .collect();
        let context = || {
            AdmissionContext::new(
                &observations,
                &limits,
                1200,
                resource.controls.observation_ttl_ms,
                resource.controls.max_parked as usize,
            )
        };
        let child = &accepted.effect_ids()[0];
        assert!(matches!(
            store
                .arm_candidate_effect(&session, child, context())
                .unwrap(),
            super::super::initialize::ArmResult::New { .. }
        ));
        let epoch = store.resource_snapshot().unwrap().epoch;
        assert!(matches!(
            store
                .arm_candidate_effect(&session, child, context())
                .unwrap(),
            super::super::initialize::ArmResult::AlreadyRecorded
        ));
        assert_eq!(store.resource_snapshot().unwrap().epoch, epoch);
        let grant_counts: (i64,i64) = store.conn.query_row("SELECT count(grant_id),sum(CASE WHEN ordinal=0 AND grant_id IS NOT NULL THEN 1 ELSE 0 END) FROM lifecycle_steps WHERE operation_id=?1",[accepted.operation_id()],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(grant_counts, (1, 1));
        let claims: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM lifecycle_claims WHERE operation_id=?1",
                [accepted.operation_id()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(claims, 1);
        assert_eq!(
            store
                .candidate_run_snapshot("owner", created.run_id())
                .unwrap()
                .unwrap()
                .requests_used(),
            0
        );
        let (kind, execution) = store.candidate_effect_execution(&session, child).unwrap();
        assert_eq!(kind, PersistedEffectKind::Initialize);
        assert_eq!(execution.token.operation_id, accepted.operation_id());
        assert_eq!(execution.token.step_id, *child);
        assert_eq!(execution.binding_id, created.binding_id());
        assert_eq!(execution.completion_target, None);
        assert!(store
            .candidate_effect_execution(&session, accepted.step_id())
            .is_err());
        assert!(store
            .arm_candidate_effect(&session, &accepted.effect_ids()[1], context())
            .is_err());
        assert!(store
            .arm_candidate_effect(&session, accepted.step_id(), context())
            .is_err());
    }

    #[test]
    fn v3_replay_rejects_missing_or_reordered_child_and_stale_caller() {
        let (store, session, created) = created();
        let body = r#"{"expected_revision":1,"action":"initialize","deadline_ms":400000}"#;
        let accepted = store
            .accept_candidate_action(&session, "owner", created.run_id(), "init", body, 1100)
            .unwrap();
        let current = store.begin_coordinator_session().unwrap();
        assert!(matches!(
            store.accept_candidate_action(&session, "owner", created.run_id(), "init", body, 1200),
            Err(LifecycleError::Stale)
        ));
        assert_eq!(
            store
                .accept_candidate_action(&current, "owner", created.run_id(), "init", body, 600000)
                .unwrap()
                .operation_id(),
            accepted.operation_id()
        );
        store
            .conn
            .execute(
                "UPDATE lifecycle_steps SET ordinal=3 WHERE id=?1",
                [&accepted.effect_ids()[0]],
            )
            .unwrap();
        assert!(matches!(
            store.accept_candidate_action(&current, "owner", created.run_id(), "init", body, 1200),
            Err(LifecycleError::CorruptStoredData)
        ));
    }
}

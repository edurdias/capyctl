//! Immutable catalog authority derived from the closed program's actual sources.
use super::*;
use crate::qualification::recipe_v1::{PROGRAM_REVISION, SuiteCaseEvidence, evaluate_suite};
use mllm_config::effective::candidate::{CandidateHost, CandidateRecipe, CandidateResources};


#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QualificationReceipt {
    record: CatalogV3,
}
impl QualificationReceipt {
    pub fn qualification_id(&self) -> &str {
        &self.record.id
    }
    pub fn source_run_id(&self) -> &str {
        &self.record.source.run_id
    }
    pub fn recipe_fingerprint(&self) -> &str {
        &self.record.source.descriptor.recipe_fingerprint
    }
    pub fn finished_at_ms(&self) -> i64 {
        self.record.finished_at_ms
    }
    pub fn committed_epoch(&self) -> u64 {
        self.record.committed_epoch
    }
    /// Frozen recipe descriptor for the existing fresh-binding writer. This
    /// informational payload grants no lifecycle or inference authority.
    pub fn binding_payload(&self) -> Result<String, LifecycleError> {
        encode(&binding_descriptor(&self.record))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct QualifiedBindingV3 {
    version: u8,
    kind: CatalogKind,
    qualification_id: String,
    host: CandidateHost,
    recipe: CandidateRecipe,
}
fn binding_descriptor(c: &CatalogV3) -> QualifiedBindingV3 {
    QualifiedBindingV3 {
        version: 3,
        kind: CatalogKind::FakeQualification,
        qualification_id: c.id.clone(),
        host: c.host.clone(),
        recipe: c.recipe.clone(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CatalogKind {
    FakeQualification,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogV3 {
    version: u8,
    kind: CatalogKind,
    id: String,
    source: Scope,
    host: CandidateHost,
    recipe: CandidateRecipe,
    operation_id: String,
    session_id: String,
    idempotency_key: String,
    request_hash: String,
    program_revision: String,
    attribution_revision: String,
    phase_bounds: CandidateResources,
    evidence: Vec<CatalogReference>,
    requests_used: u32,
    finished_at_ms: i64,
    committed_epoch: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogReference {
    id: String,
    case_id: String,
    digest: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FinishCommand {
    expected_revision: i64,
    action: FinishAction,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FinishAction {
    Finish,
}

fn command_scope(run: &str) -> String {
    format!("POST /management/v1/qualification-runs/{run}/actions")
}
fn request_hash(principal: &str, run: &str, revision: i64) -> Result<String, LifecycleError> {
    inference::digest(&(
        3_u8,
        principal,
        command_scope(run),
        FinishCommand {
            expected_revision: revision,
            action: FinishAction::Finish,
        },
    ))
}

/// Read every case through its existing strict source decoder before pure evaluation.
/// A reference alone, a deleted lease or completed SQL flag cannot supply coverage.
fn suite(
    tx: &Transaction<'_>,
    p: &CandidateActionPlanV3,
) -> Result<Vec<CatalogReference>, LifecycleError> {
    let read=ReadValidation::new(tx);
    validate_plan_read(tx, p,true,&read)?;
    let snapshot = super::super::read_snapshot(tx, &p.scope.principal, &p.scope.run_id)
        .map_err(creation_error)?
        .ok_or(LifecycleError::CorruptStoredData)?;
    let program = QualificationProgram::resolve(snapshot.reviewed_manifest())?;
    let (refs, attempts, actions, outstanding): (u32,u32,u32,bool) = tx.query_row(
        "SELECT (SELECT COUNT(*) FROM qualification_evidence_refs WHERE run_id=?1),(SELECT COUNT(*) FROM qualification_request_attempts WHERE run_id=?1),(SELECT COUNT(*) FROM qualification_case_actions WHERE run_id=?1),EXISTS(SELECT 1 FROM request_leases WHERE deployment_id=?2)",
        params![p.scope.run_id,p.scope.deployment_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    if refs != program.required_references()
        || attempts != program.required_requests()
        || outstanding
        || actions as usize
            != snapshot
                .reviewed_manifest()
                .cases()
                .iter()
                .filter(|c| {
                    matches!(
                        c.kind(),
                        CandidateCaseKind::ColdInitialize
                            | CandidateCaseKind::Park
                            | CandidateCaseKind::Restore
                            | CandidateCaseKind::Security
                    )
                })
                .count()
    {
        return Err(LifecycleError::Conflict);
    }
    let mut cases = Vec::new();
    let mut references = Vec::new();
    for case in snapshot.reviewed_manifest().cases() {
        if matches!(
            case.kind(),
            CandidateCaseKind::ColdInitialize
                | CandidateCaseKind::Park
                | CandidateCaseKind::Restore
                | CandidateCaseKind::Security
        ) {
            let anchor: String = tx.query_row(
                "SELECT step_id FROM qualification_case_actions WHERE run_id=?1 AND case_id=?2",
                params![p.scope.run_id, case.id()],
                |r| r.get(0),
            )?;
            if !is_v3(tx, &anchor)? {
                return Err(LifecycleError::Unsupported);
            }
            let action = plan_for_step(tx, &anchor)?;
            validate_plan_read(tx, &action,true,&read)?;
            let complete: bool = tx.query_row(
                "SELECT state='completed' FROM lifecycle_steps WHERE id=?1",
                [&anchor],
                |r| r.get(0),
            )?;
            if !complete {
                return Err(LifecycleError::Conflict);
            }
            if matches!(
                case.kind(),
                CandidateCaseKind::ColdInitialize | CandidateCaseKind::Restore
            ) {
                inference::markers::baseline_read(tx, &action,&read)?;
            }
        }
        let mut statement = tx.prepare("SELECT id,case_id,evidence_digest,metadata_json FROM qualification_evidence_refs WHERE run_id=?1 AND case_id=?2 ORDER BY evidence_digest")?;
        let rows = statement
            .query_map(params![p.scope.run_id, case.id()], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut hashes = Vec::new();
        for (id, case_id, hash, raw) in rows {
            let evidence: inference::CoverageV3 = decode(&raw)?;
            if !super::super::ulid(&id)
                || evidence.case_id != case_id
                || evidence.scope.run_id != p.scope.run_id
                || evidence.origin != PROGRAM_REVISION
                || evidence.version != 3
                || inference::digest(&evidence)? != hash
            {
                return Err(LifecycleError::CorruptStoredData);
            }
            hashes.push(hash.clone());
            references.push(CatalogReference {
                id,
                case_id,
                digest: hash,
            });
        }
        let requests: u32 = tx.query_row(
            "SELECT COUNT(*) FROM qualification_request_attempts WHERE run_id=?1 AND case_id=?2",
            params![p.scope.run_id, case.id()],
            |r| r.get(0),
        )?;
        cases.push(SuiteCaseEvidence {
            id: case.id().into(),
            kind: case.kind(),
            cycle: case.cycle(),
            requests,
            references: hashes,
        });
    }
    evaluate_suite(
        snapshot.reviewed_manifest(),
        &cases,
        snapshot.requests_used(),
    )?;
    Ok(references)
}

/// Called only after all original texts have passed their strict source decoders.
/// These projections include final marker responses as well as parent/control
/// evidence; cleanup history is deliberately outside the qualification interval.
fn source_bounds(tx: &Transaction<'_>, run: &str) -> Result<(u64, i64), LifecycleError> {
    Ok(tx.query_row("SELECT MAX(epoch),MAX(observed) FROM (SELECT e.committed_epoch epoch,json_extract(e.evidence_json,'$.observed_at_ms') observed FROM lifecycle_evidence e JOIN lifecycle_steps s ON s.id=e.step_id JOIN qualification_case_actions a ON a.operation_id=s.operation_id WHERE a.run_id=?1 UNION ALL SELECT e.committed_epoch,json_extract(e.evidence_json,'$.observed_at_ms') FROM qualification_request_results e JOIN qualification_request_attempts a ON a.request_operation_id=e.request_operation_id WHERE a.run_id=?1 UNION ALL SELECT e.committed_epoch,json_extract(e.evidence_json,'$.observed_at_ms') FROM qualification_parked_status e JOIN qualification_case_actions a ON a.step_id=e.parent_step_id WHERE a.run_id=?1)",[run],|r|Ok((r.get(0)?,r.get(1)?)))?)
}

fn read(tx: &Transaction<'_>, id: &str) -> Result<Option<CatalogV3>, LifecycleError> {
    let row: Option<(String, String, String)> = tx
        .query_row(
            "SELECT source_run_id,recipe_fingerprint,record_json FROM qualifications WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((run, fingerprint, raw)) = row else {
        return Ok(None);
    };
    let c: CatalogV3 = decode(&raw)?;
    if c.version != 3
        || c.id != id
        || !super::super::ulid(id)
        || c.source.run_id != run
        || c.source.descriptor.recipe_fingerprint != fingerprint
        || c.program_revision != PROGRAM_REVISION
        || c.attribution_revision != "qualification-fake-v1:configured-phase-bounds"
        || !super::super::ulid(&c.operation_id)
        || !super::super::ulid(&c.session_id)
        || !super::super::valid_id(&c.idempotency_key)
        || c.evidence.len() > 4096
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    let p = warm::cold(tx, &run)?;
    let snapshot = super::super::read_snapshot(tx, &c.source.principal, &run)
        .map_err(creation_error)?
        .ok_or(LifecycleError::CorruptStoredData)?;
    if c.source != p.scope
        || c.phase_bounds != *snapshot.reviewed_manifest().effective_recipe().resources()
        || c.host != *snapshot.reviewed_manifest().host()
        || c.recipe != *snapshot.reviewed_manifest().effective_recipe()
        || c.requests_used != snapshot.requests_used()
        || c.finished_at_ms < p.accepted_at_ms
        || c.finished_at_ms >= snapshot.receipt().deadline_ms()
        || c.request_hash != request_hash(&c.source.principal, &run, c.source.revision)?
        || snapshot.state() != super::super::CandidateRunState::Passed
        || c.evidence != suite(tx, &p)?
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    let ledger = crate::resource_ledger::read_snapshot(tx).map_err(resource_error)?;
    let (max_source_epoch, last_observed) = source_bounds(tx, &run)?;
    let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1 AND deployment_id=?2 AND kind='candidate_finish_v3' AND state='succeeded' AND idempotency_key IS NULL AND error_code IS NULL) AND EXISTS(SELECT 1 FROM command_receipts WHERE operation_id=?1 AND principal_id=?3 AND command_scope=?4 AND idempotency_key=?5 AND request_hash=?6 AND response_json=?7)",params![c.operation_id,c.source.deployment_id,c.source.principal,command_scope(&run),c.idempotency_key,c.request_hash,raw],|r|r.get(0))?;
    if !valid
        || c.committed_epoch <= max_source_epoch
        || c.committed_epoch > ledger.epoch
        || c.finished_at_ms < last_observed
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok(Some(c))
}

/// Transaction-scoped catalog authority for an independently validated managed revision.
pub(crate) fn qualified_effective(
    tx: &Transaction<'_>,
    effective: &mllm_config::effective::EffectiveDeployment,
    deployment: &str,
) -> Result<QualificationReceipt, LifecycleError> {
    if effective.profile.engine != mllm_config::effective::Engine::Fake {
        return Err(LifecycleError::Unsupported);
    }
    let id = effective.profile.qualification_id.strip_prefix("qualified:").filter(|id| super::super::ulid(id)).ok_or(LifecycleError::Invalid)?;
    let record = read(tx, id)?.ok_or(LifecycleError::Conflict)?;
    if record.source.deployment_id == deployment
        || record.source.descriptor.recipe_fingerprint != effective.qualification_fingerprint
        || record.host.id() != effective.host.name
        || record.host.hardware_fingerprint() != effective.host.hardware_fingerprint
        || record.host.environment_fingerprint() != effective.host.environment_fingerprint
    {
        return Err(LifecycleError::Conflict);
    }
    let anchor = immutable_initialize_anchor(tx, &record.source.parent_step_id)?;
    super::super::cleanup::validate_gone_history(tx, &anchor)?;
    Ok(QualificationReceipt { record })
}

impl crate::Store {
    /// Read-only Fake eligibility for an actual fresh managed binding. A successful
    /// resolution does not open gates, allocate resources, arm steps or dispatch work.
    pub fn resolve_ordinary_qualification(
        &self,
        session: &CoordinatorSession,
        id: &str,
        fence: &crate::lifecycle::DeploymentFence,
        binding_id: &str,
    ) -> Result<QualificationReceipt, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, session)?;
        if !super::super::ulid(id)
            || !super::super::valid_id(binding_id)
            || !super::super::valid_id(&fence.deployment_id)
        {
            return Err(LifecycleError::Invalid);
        }
        let record = read(&tx, id)?.ok_or(LifecycleError::Conflict)?;
        let v = immutable_initialize_anchor(&tx, &record.source.parent_step_id)?;
        super::super::cleanup::validate_gone_history(&tx, &v)?;
        let row:Option<(String,i64,String,String,String,String,String)>=tx.query_row("SELECT deployment_id,revision,incarnation,ownership,binding_json,identities_json,state FROM runtime_bindings WHERE id=?1",[binding_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).optional()?;
        let (deployment, revision, incarnation, ownership, raw, identities, state) =
            row.ok_or(LifecycleError::Conflict)?;
        let binding: crate::lifecycle::BindingDto = decode(&raw)?;
        let descriptor: QualifiedBindingV3 = decode(&binding.payload)?;
        let members: Vec<crate::lifecycle::IdentityDto> = decode(&identities)?;
        if deployment != fence.deployment_id
            || revision != fence.revision
            || deployment == record.source.deployment_id
            || binding_id == record.source.binding_id
            || incarnation == record.source.incarnation
            || incarnation.is_empty()
            || ownership != "managed"
            || state != "reserved"
            || !members.is_empty()
            || binding.version != 1
            || binding.qualification_id != record.id
            || descriptor != binding_descriptor(&record)
            || binding.credential_ref.trim().is_empty()
        {
            return Err(LifecycleError::Conflict);
        }
        let endpoint: std::net::SocketAddr = binding
            .endpoint
            .parse()
            .map_err(|_| LifecycleError::CorruptStoredData)?;
        if endpoint.ip() != std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
            || endpoint.port() == 0
        {
            return Err(LifecycleError::CorruptStoredData);
        }
        let fresh:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM deployments WHERE id=?1 AND revision=?2 AND current_generation=?3 AND kind='model' AND desired_state='stopped' AND observed_state='stopped' AND dispatch_enabled=0) AND NOT EXISTS(SELECT 1 FROM qualification_runs WHERE deployment_id=?1) AND NOT EXISTS(SELECT 1 FROM lifecycle_steps WHERE binding_id=?4) AND NOT EXISTS(SELECT 1 FROM request_leases WHERE deployment_id=?1) AND NOT EXISTS(SELECT 1 FROM resource_owners WHERE owner_id=?1) AND (SELECT COUNT(*) FROM runtime_bindings WHERE deployment_id=?1)=1 AND (SELECT COUNT(*) FROM endpoint_leases WHERE binding_id=?4)=1 AND EXISTS(SELECT 1 FROM endpoint_leases WHERE binding_id=?4 AND host='127.0.0.1' AND port=?5)",params![deployment,revision,fence.generation,binding_id,endpoint.port()],|r|r.get(0))?;
        if !fresh {
            return Err(LifecycleError::Conflict);
        }
        Ok(QualificationReceipt { record })
    }
    pub fn read_qualification(
        &self,
        session: &CoordinatorSession,
        id: &str,
    ) -> Result<Option<QualificationReceipt>, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, session)?;
        if !super::super::ulid(id) {
            return Err(LifecycleError::Invalid);
        }
        Ok(read(&tx, id)?.map(|record| QualificationReceipt { record }))
    }
    #[allow(clippy::too_many_arguments)]
    pub fn finish_candidate_run(
        &self,
        session: &CoordinatorSession,
        principal: &str,
        run: &str,
        key: &str,
        text: &str,
        now: i64,
    ) -> Result<QualificationReceipt, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session)?;
        if text.len() > 1048576
            || !super::super::valid_id(principal)
            || !super::super::valid_id(key)
            || !super::super::ulid(run)
        {
            return Err(LifecycleError::Invalid);
        }
        let command: FinishCommand =
            serde_json::from_str(text).map_err(|_| LifecycleError::Invalid)?;
        let hash = request_hash(principal, run, command.expected_revision)?;
        let old: Option<(String,String)> = tx.query_row("SELECT request_hash,operation_id FROM command_receipts WHERE principal_id=?1 AND command_scope=?2 AND idempotency_key=?3",params![principal,command_scope(run),key],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if old.as_ref().is_some_and(|(h, _)| h != &hash) {
            return Err(LifecycleError::Conflict);
        }
        let snapshot = super::super::read_snapshot(&tx, principal, run)
            .map_err(creation_error)?
            .ok_or(LifecycleError::Conflict)?;
        if command.expected_revision != snapshot.receipt().revision() {
            return Err(LifecycleError::Stale);
        }
        let existing: Option<String> = tx
            .query_row(
                "SELECT id FROM qualifications WHERE source_run_id=?1",
                [run],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            let record = read(&tx, &id)?.ok_or(LifecycleError::CorruptStoredData)?;
            if old.is_some_and(|(_, op)| op != record.operation_id) {
                return Err(LifecycleError::CorruptStoredData);
            }
            return Ok(QualificationReceipt { record });
        }
        if old.is_some() {
            return Err(LifecycleError::CorruptStoredData);
        }
        QualificationProgram::resolve(snapshot.reviewed_manifest())?;
        let anchor: Option<String> = tx
            .query_row(
                "SELECT step_id FROM qualification_case_actions WHERE run_id=?1 AND case_id=?2",
                params![run, snapshot.reviewed_manifest().cases()[0].id()],
                |r| r.get(0),
            )
            .optional()?;
        let anchor = anchor.ok_or(LifecycleError::Conflict)?;
        if !is_v3(&tx, &anchor)? {
            return Err(LifecycleError::Unsupported);
        }
        let p = plan_for_step(&tx, &anchor)?;
        if snapshot.state() != super::super::CandidateRunState::Running
            || snapshot.cleanup_state() != super::super::CandidateCleanupState::Retained
            || now < p.accepted_at_ms
            || now >= snapshot.receipt().deadline_ms()
        {
            return Err(LifecycleError::Stale);
        }
        super::super::initialize::policy(&tx, &snapshot)?;
        let current:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM deployments WHERE id=?1 AND revision=?2 AND current_generation=?3 AND desired_state='stopped' AND admission_enabled=0 AND dispatch_enabled=0) AND NOT EXISTS(SELECT 1 FROM lifecycle_claims WHERE deployment_id=?1)",params![p.scope.deployment_id,p.scope.revision,p.scope.generation],|r|r.get(0))?;
        if !current {
            return Err(LifecycleError::Stale);
        }
        crate::lifecycle::completion::isolated(&tx, &p.scope.deployment_id)?;
        let evidence = suite(&tx, &p)?;
        if now < source_bounds(&tx, run)?.1 {
            return Err(LifecycleError::Invalid);
        }
        let epoch = crate::resource_ledger::advance_completion_epoch(&tx)?;
        let record = CatalogV3 {
            version: 3,
            kind: CatalogKind::FakeQualification,
            id: ulid::Ulid::new().to_string(),
            source: p.scope.clone(),
            host: snapshot.reviewed_manifest().host().clone(),
            recipe: snapshot.reviewed_manifest().effective_recipe().clone(),
            operation_id: ulid::Ulid::new().to_string(),
            session_id: session.id().into(),
            idempotency_key: key.into(),
            request_hash: hash,
            program_revision: PROGRAM_REVISION.into(),
            attribution_revision: "qualification-fake-v1:configured-phase-bounds".into(),
            phase_bounds: snapshot
                .reviewed_manifest()
                .effective_recipe()
                .resources()
                .clone(),
            evidence,
            requests_used: snapshot.requests_used(),
            finished_at_ms: now,
            committed_epoch: epoch,
        };
        let raw = encode(&record)?;
        tx.execute("INSERT INTO operations(id,deployment_id,kind,state) VALUES(?1,?2,'candidate_finish_v3','succeeded')",params![record.operation_id,p.scope.deployment_id])?;
        tx.execute(
            "INSERT INTO qualifications VALUES(?1,?2,?3,?4)",
            params![
                record.id,
                run,
                record.source.descriptor.recipe_fingerprint,
                raw
            ],
        )?;
        tx.execute(
            "INSERT INTO command_receipts VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                principal,
                command_scope(run),
                key,
                record.request_hash,
                record.operation_id,
                raw
            ],
        )?;
        tx.execute(
            "UPDATE qualification_runs SET state='passed' WHERE id=?1",
            [run],
        )?;
        crate::lifecycle::completion::event(
            &tx,
            session,
            &record.operation_id,
            &p.scope.deployment_id,
            &p.scope.parent_step_id,
            crate::events::CandidateLifecycleTransition::QualificationFinished,
            Some(epoch),
        )?;
        read(&tx, &record.id)?.ok_or(LifecycleError::CorruptStoredData)?;
        tx.commit()?;
        Ok(QualificationReceipt { record })
    }
}

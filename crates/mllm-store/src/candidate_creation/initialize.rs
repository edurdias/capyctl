//! Candidate Initialize acceptance, durable planning, and bounded atomic arm.
use super::{
    CandidateCreationError, CandidateReviewedSnapshot, CandidateRunSnapshot, DescriptorRefV1,
    MAX_BYTES,
};
use crate::dispatch::{CoordinatorSession, DispatchError, check_session};
use crate::events::{EventMetadata, EventOperationId, EventWriteError, append_event};
use crate::lifecycle::{
    DeploymentFence, LifecycleError, insert_candidate_initialize_run,
    validate_candidate_initialize_run,
};
use crate::qualification_policy::read_candidate_policy;
use crate::resource_ledger::{GrantRequest, ResourceStoreError, reserve_increase_in_transaction};
use crate::resource_policy::{ResourcePolicySnapshot, read_singleton_policy};
use mllm_config::effective::candidate::CandidateCaseKind;
use mllm_config::effective::candidate::{CandidateLaunch, CandidatePhase};
use mllm_domain::completion::{ExecutionIdentities, StepExecutionContext, TransitionToken};
use mllm_domain::resources::{
    Allocation, DeviceClaim, MemoryLimit, PhaseFootprint, ResourcePhase, Sharing,
};
use mllm_scheduler::residency::AdmissionContext;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArmResult {
    New { step_id: String },
    AlreadyRecorded,
}
impl crate::Store {
    /// Trusted controller read of an armed candidate, never a grant of send authority.
    /// No management handler may project the returned paths or references.
    /// `now_ms` must be freshly sampled from the service clock at each handoff boundary.
    #[doc(hidden)]
    pub fn candidate_native_launch(
        &self,
        s: &CoordinatorSession,
        id: &str,
        now_ms: i64,
    ) -> std::result::Result<mllm_domain::launch::NativeCandidateLaunch, LifecycleError> {
        use mllm_domain::launch::{
            NativeCandidateLaunch, NativeCandidateMetadata, NativeDeviceSelection,
            ProfileLaunchSettings,
        };
        if !super::ulid(id) {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        session(&tx, s)?;
        let (snapshot, _, read) = load_execution_step(&tx, id)?;
        current(&tx, s, &snapshot, &read)?;
        if read.state != "armed"
            || read.run_state != "running"
            || snapshot.state() != super::CandidateRunState::Running
        {
            return Err(LifecycleError::Conflict);
        }
        let running: bool = tx.query_row(
            "SELECT state='running' FROM operations WHERE id=?1",
            [&read.planned.operation_id],
            |r| r.get(0),
        )?;
        if !running {
            return Err(LifecycleError::Conflict);
        }
        let resource = policy(&tx, &snapshot)?;
        let qualification = read_candidate_policy(&tx, snapshot.receipt().host_id())
            .map_err(super::map_qualification)
            .map_err(Error::from)?
            .ok_or(LifecycleError::Conflict)?;
        let execution = read
            .execution
            .as_ref()
            .ok_or(LifecycleError::CorruptStoredData)?;
        if now_ms < execution.issued_at_ms
            || now_ms >= read.planned.deadline_ms
            || resource.revision != execution.resource_policy_revision
            || qualification.revision != execution.qualification_policy_revision
        {
            return Err(LifecycleError::Conflict);
        }
        let StoredLaunch::SglangPinned(native) = read
            .execution
            .ok_or(LifecycleError::CorruptStoredData)?
            .launch_settings
        else {
            return Err(LifecycleError::Unsupported);
        };
        let ProfileLaunchSettings::Sglang(settings) = launch_settings(
            snapshot
                .reviewed_manifest()
                .effective_recipe()
                .profile()
                .launch_settings(),
        ) else {
            return Err(LifecycleError::CorruptStoredData);
        };
        // load_execution_step already validates the binding against the immutable
        // descriptor, its endpoint lease, session, claim, and deployment fences.
        let endpoint: String = tx.query_row(
            "SELECT json_extract(binding_json,'$.endpoint') FROM runtime_bindings WHERE id=?1",
            [&native.binding_id],
            |r| r.get(0),
        )?;
        let manifest = snapshot.reviewed_manifest();
        let recipe = manifest.effective_recipe();
        let [selected] = recipe.devices() else {
            return Err(LifecycleError::Unsupported);
        };
        let device = recipe
            .host_devices()
            .get(&selected.id)
            .ok_or(LifecycleError::CorruptStoredData)?;
        let metadata = NativeCandidateMetadata {
            engine: "sglang".into(),
            recipe: settings.recipe.clone(),
            source_revision: mllm_config::effective::candidate::NATIVE_SGLANG_SOURCE_REVISION
                .into(),
            checkpoint_revision: native.checkpoint_revision,
            binding_id: native.binding_id,
            incarnation: native.incarnation,
            endpoint: format!("http://{endpoint}"),
            served_name: format!("candidate-{}", read.planned.binding_id),
            rendered_settings_digest: native.rendered_settings_digest,
            device: NativeDeviceSelection {
                host_id: manifest.host().id().into(),
                hardware_fingerprint: manifest.host().hardware_fingerprint().into(),
                device_id: selected.id.clone(),
                memory_domain: device.domain.clone(),
            },
        };
        let launch = NativeCandidateLaunch::from_frozen_store(
            metadata,
            native.checkpoint_root,
            native.executable,
            native.inference_credential_ref,
            native.admin_credential_ref,
            settings,
        );
        tx.commit()?;
        Ok(launch)
    }

    /// Records one spawn attempt and conservative grant atomically. Only New may lead to a later send.
    pub fn arm_step(
        &self,
        s: &CoordinatorSession,
        id: &str,
        context: AdmissionContext<'_>,
    ) -> std::result::Result<ArmResult, LifecycleError> {
        if !super::ulid(id) {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        session(&tx, s)?;
        if crate::ordinary_lifecycle::is_ordinary(&tx, id)? {
            let result = crate::ordinary_lifecycle::arm(&tx, s, id, context)?;
            tx.commit()?;
            return Ok(result);
        }
        let (snapshot, _, read) = load_execution_step(&tx, id)?;
        current(&tx, s, &snapshot, &read)?;
        if matches!(read.state.as_str(), "armed" | "uncertain") && read.execution.is_some() {
            return Ok(ArmResult::AlreadyRecorded);
        }
        if read.state != "planned" || read.run_state != "queued" || read.execution.is_some() {
            return Err(LifecycleError::Conflict);
        }
        let step = &read.planned;
        let pending: bool = tx.query_row(
            "SELECT state='pending' FROM operations WHERE id=?1",
            [&step.operation_id],
            |r| r.get(0),
        )?;
        if !pending || context.now_ms < step.accepted_at_ms || context.now_ms >= step.deadline_ms {
            return Err(LifecycleError::Conflict);
        }
        eligible(&tx, &snapshot, false)?;
        let resource = policy(&tx, &snapshot)?;
        let m = snapshot.reviewed_manifest();
        // Validate the closed descriptor before reserving resources. This is pure
        // validation; filesystem preflight and launch belong outside this transaction.
        let selected_launch =
            StoredLaunch::from_snapshot(&snapshot).map_err(|_| LifecycleError::Unsupported)?;
        let limits: Vec<_> = resource
            .controls
            .domains
            .iter()
            .map(|(id, p)| MemoryLimit {
                domain: id.clone(),
                managed_bytes: p.managed_limit,
                free_reserve_bytes: p.free_reserve,
                host_kv_bytes: p.host_kv_limit,
                parked_bytes: p.parked_limit,
            })
            .collect();
        let mut requested = context.limits.to_vec();
        requested.sort_by(|a, b| a.domain.cmp(&b.domain));
        if requested != limits
            || context.ttl_ms != resource.controls.observation_ttl_ms
            || context.max_parked != resource.controls.max_parked as usize
        {
            return Err(LifecycleError::Rejected(
                "resource policy context mismatch".into(),
            ));
        }
        let cold = footprint(m.effective_recipe().resources().cold(), ResourcePhase::Cold)?;
        let ready = footprint(
            m.effective_recipe().resources().ready(),
            ResourcePhase::Ready,
        )?;
        for claim in cold.devices.iter().chain(&ready.devices) {
            let frozen = m
                .effective_recipe()
                .host_devices()
                .get(&claim.device)
                .ok_or(LifecycleError::CorruptStoredData)?;
            let sharing = resource
                .controls
                .device_sharing_overrides
                .get(&claim.device)
                .ok_or(LifecycleError::Rejected("device removed".into()))?;
            if resource.context.device_domains.get(&claim.device) != Some(&frozen.domain)
                || (claim.sharing == Sharing::Shared
                    && (*sharing != mllm_config::effective::Sharing::Shared
                        || resource.controls.device_sharing
                            != mllm_config::effective::Sharing::Shared))
            {
                return Err(LifecycleError::Rejected("device policy mismatch".into()));
            }
        }
        let ledger = crate::resource_ledger::read_snapshot(&tx).map_err(resource_error)?;
        let grant = ulid::Ulid::new().to_string();
        reserve_increase_in_transaction(
            &tx,
            &GrantRequest {
                id: grant.clone(),
                deployment_id: step.deployment_id.clone(),
                operation_id: step.operation_id.clone(),
                revision: step.revision,
                generation: step.generation,
                expected_epoch: ledger.epoch,
                next: cold,
            },
            AdmissionContext {
                limits: &limits,
                ttl_ms: resource.controls.observation_ttl_ms,
                max_parked: resource.controls.max_parked as usize,
                ..context
            },
        )
        .map_err(resource_error)?;
        let qualification = read_candidate_policy(&tx, snapshot.receipt().host_id())
            .map_err(super::map_qualification)
            .map_err(Error::from)?
            .ok_or(LifecycleError::Conflict)?;
        let execution = ExecutionV2 {
            issued_at_ms: context.now_ms,
            grant_id: grant.clone(),
            launch_settings: selected_launch,
            completion_target: StoredTarget::from_footprint(&ready),
            resource_policy_revision: resource.revision,
            qualification_policy_revision: qualification.revision,
            expected_epoch: ledger.epoch,
        };
        let armed = InitializeStepV2 {
            version: 2,
            planned: step.clone(),
            execution,
        };
        one(tx.execute("UPDATE runtime_bindings SET state='uncertain' WHERE id=?1 AND deployment_id=?2 AND revision=?3 AND incarnation=?4 AND state='reserved'",params![step.binding_id,step.deployment_id,step.revision,step.incarnation])?)?;
        one(tx.execute(
            "UPDATE operations SET state='running' WHERE id=?1 AND state='pending'",
            [&step.operation_id],
        )?)?;
        one(tx.execute("UPDATE lifecycle_runs SET state='running' WHERE operation_id=?1 AND session_id=?2 AND state='queued'",params![step.operation_id,s.id()])?)?;
        one(tx.execute(
            "UPDATE qualification_runs SET state='running' WHERE id=?1 AND state='accepted'",
            [&step.run_id],
        )?)?;
        one(tx.execute("UPDATE lifecycle_steps SET state='armed',step_json=?1,grant_id=?2 WHERE id=?3 AND session_id=?4 AND state='planned' AND grant_id IS NULL",params![encode(&armed)?,grant,id,s.id()])?)?;
        let event_id = |id: &str| {
            id.parse()
                .map(EventOperationId::generated)
                .map_err(|_| LifecycleError::CorruptStoredData)
        };
        append_event(
            &tx,
            &EventMetadata::CandidateInitializeArmed {
                operation_id: event_id(&step.operation_id)?,
                deployment_id: event_id(&step.deployment_id)?,
                run_id: event_id(&step.run_id)?,
                step_id: event_id(&step.step_id)?,
                revision: step.revision,
                generation: step.generation,
                session_epoch: s.epoch(),
            },
        )
        .map_err(|e| match e {
            EventWriteError::Sql(e) => LifecycleError::Sql(e),
            _ => LifecycleError::CorruptStoredData,
        })?;
        let (_, _, reread) = load_execution_step(&tx, id)?;
        if reread.execution.as_ref() != Some(&armed.execution) {
            return Err(LifecycleError::CorruptStoredData);
        }
        tx.commit()?;
        Ok(ArmResult::New { step_id: id.into() })
    }
    /// Reads persisted Initialize context. Reading or cloning it never authorizes a send.
    pub fn candidate_initialize_execution(
        &self,
        s: &CoordinatorSession,
        id: &str,
    ) -> std::result::Result<StepExecutionContext, LifecycleError> {
        if !super::ulid(id) {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        session(&tx, s)?;
        let (snapshot, _, read) = load_execution_step(&tx, id)?;
        current(&tx, s, &snapshot, &read)?;
        if read.state != "armed" || read.run_state != "running" {
            return Err(LifecycleError::Conflict);
        }
        let running: bool = tx.query_row(
            "SELECT state='running' FROM operations WHERE id=?1",
            [&read.planned.operation_id],
            |r| r.get(0),
        )?;
        if !running || snapshot.state() != super::CandidateRunState::Running {
            return Err(LifecycleError::Conflict);
        }
        let e = read.execution.ok_or(LifecycleError::CorruptStoredData)?;
        let p = read.planned;
        let context = StepExecutionContext {
            token: TransitionToken {
                deployment_id: p.deployment_id,
                revision: p.revision,
                generation: p.generation,
                operation_id: p.operation_id,
                step_id: p.step_id,
                qualification_id: p.qualification_id,
            },
            binding_id: p.binding_id,
            incarnation: p.incarnation,
            issued_at_ms: e.issued_at_ms,
            deadline_ms: p.deadline_ms,
            identities: ExecutionIdentities::OwnedLaunch,
            completion_target: Some(e.completion_target.to_footprint()?),
            grant_id: Some(e.grant_id),
            launch_settings: Some(launch_settings(
                snapshot
                    .reviewed_manifest()
                    .effective_recipe()
                    .profile()
                    .launch_settings(),
            )),
        };
        tx.commit()?;
        Ok(context)
    }
}
fn one(count: usize) -> std::result::Result<(), LifecycleError> {
    if count == 1 {
        Ok(())
    } else {
        Err(LifecycleError::Conflict)
    }
}
fn resource_error(e: ResourceStoreError) -> LifecycleError {
    match e {
        ResourceStoreError::Sql(e) => LifecycleError::Sql(e),
        ResourceStoreError::Invalid | ResourceStoreError::Json(_) => {
            LifecycleError::CorruptStoredData
        }
        e => LifecycleError::Rejected(e.to_string()),
    }
}
impl From<Error> for LifecycleError {
    fn from(e: Error) -> Self {
        match e {
            Error::Sql(e) => Self::Sql(e),
            Error::StaleSession => Self::Stale,
            Error::CorruptStoredData => Self::CorruptStoredData,
            Error::InvalidCommand => Self::Invalid,
            Error::QualificationDenied => Self::Rejected("qualification denied".into()),
            _ => Self::Conflict,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CandidateInitializeError {
    #[error("invalid Initialize command")]
    InvalidCommand,
    #[error("stale coordinator session")]
    StaleSession,
    #[error("candidate not found")]
    NotFound,
    #[error("candidate revision conflict")]
    RevisionConflict,
    #[error("Initialize idempotency conflict")]
    IdempotencyConflict,
    #[error("candidate qualification denied")]
    QualificationDenied,
    #[error("candidate lifecycle conflict")]
    LifecycleConflict,
    #[error("corrupt stored Initialize data")]
    CorruptStoredData,
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}
type Result<T> = std::result::Result<T, CandidateInitializeError>;
use CandidateInitializeError as Error;
impl From<CandidateCreationError> for Error {
    fn from(e: CandidateCreationError) -> Self {
        match e {
            CandidateCreationError::Sql(e) => Self::Sql(e),
            CandidateCreationError::RevisionConflict => Self::RevisionConflict,
            CandidateCreationError::QualificationDenied => Self::QualificationDenied,
            _ => Self::CorruptStoredData,
        }
    }
}
fn session(tx: &Transaction<'_>, s: &CoordinatorSession) -> Result<()> {
    check_session(tx, s).map_err(|e| match e {
        DispatchError::Sql(e) => Error::Sql(e),
        _ => Error::StaleSession,
    })
}
fn decode<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
    if text.len() > MAX_BYTES {
        return Err(Error::CorruptStoredData);
    }
    serde_json::from_str(text).map_err(|_| Error::CorruptStoredData)
}
fn encode(value: &impl Serialize) -> Result<String> {
    let text = serde_json::to_string(value).map_err(|_| Error::InvalidCommand)?;
    if text.len() > MAX_BYTES {
        return Err(Error::InvalidCommand);
    }
    Ok(text)
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Method {
    #[serde(rename = "POST")]
    Post,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InitializeCommand {
    expected_revision: i64,
    action: InitializeAction,
    deadline_ms: i64,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum InitializeAction {
    Initialize,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InitializeReceiptV1 {
    version: u8,
    method: Method,
    target: String,
    principal_id: String,
    run_id: String,
    request_hash: String,
    operation_id: String,
    deployment_id: String,
    step_id: String,
    case_id: String,
    revision: i64,
    generation: i64,
    accepted_at_ms: i64,
    deadline_ms: i64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandidateInitializeReceipt {
    inner: InitializeReceiptV1,
}
impl CandidateInitializeReceipt {
    pub fn version(&self) -> u8 {
        self.inner.version
    }
    pub fn run_id(&self) -> &str {
        &self.inner.run_id
    }
    pub fn operation_id(&self) -> &str {
        &self.inner.operation_id
    }
    pub fn deployment_id(&self) -> &str {
        &self.inner.deployment_id
    }
    pub fn step_id(&self) -> &str {
        &self.inner.step_id
    }
    pub fn case_id(&self) -> &str {
        &self.inner.case_id
    }
    pub fn revision(&self) -> i64 {
        self.inner.revision
    }
    pub fn generation(&self) -> i64 {
        self.inner.generation
    }
    pub fn accepted_at_ms(&self) -> i64 {
        self.inner.accepted_at_ms
    }
    pub fn deadline_ms(&self) -> i64 {
        self.inner.deadline_ms
    }
}
#[derive(Clone, Debug)]
/// Informational descriptor for the sole Initialize action; never dispatch authority.
pub struct CandidateInitializePlan {
    receipt: CandidateInitializeReceipt,
    binding_id: String,
    incarnation: String,
    qualification_id: String,
    reviewed_manifest: CandidateReviewedSnapshot,
}
impl CandidateInitializePlan {
    pub fn receipt(&self) -> &CandidateInitializeReceipt {
        &self.receipt
    }
    pub fn binding_id(&self) -> &str {
        &self.binding_id
    }
    pub fn incarnation(&self) -> &str {
        &self.incarnation
    }
    pub fn qualification_id(&self) -> &str {
        &self.qualification_id
    }
    pub fn reviewed_manifest(&self) -> &CandidateReviewedSnapshot {
        &self.reviewed_manifest
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum PlannedKind {
    #[serde(rename = "candidate_initialize_planned")]
    Initialize,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum ColdKind {
    #[serde(rename = "cold_initialize")]
    Cold,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum OwnedIdentity {
    #[serde(rename = "owned_launch")]
    Owned,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InitializeStepV1 {
    version: u8,
    kind: PlannedKind,
    principal_id: String,
    run_id: String,
    creation_operation_id: String,
    operation_id: String,
    step_id: String,
    deployment_id: String,
    revision: i64,
    generation: i64,
    binding_id: String,
    incarnation: String,
    qualification_id: String,
    case_id: String,
    case_kind: ColdKind,
    cycle: u32,
    descriptor: DescriptorRefV1,
    accepted_at_ms: i64,
    deadline_ms: i64,
    identities: OwnedIdentity,
}
impl InitializeStepV1 {
    fn fence(&self) -> DeploymentFence {
        DeploymentFence {
            deployment_id: self.deployment_id.clone(),
            revision: self.revision,
            generation: self.generation,
        }
    }
}
fn target(run: &str) -> String {
    format!("/management/v1/qualification-runs/{run}/actions")
}
fn hash(principal: &str, run: &str, revision: i64, deadline: i64) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(
            encode(&(
                1_u8,
                "POST",
                target(run),
                principal,
                revision,
                "initialize",
                deadline
            ))?
            .as_bytes()
        )
    ))
}
fn selected(snapshot: &CandidateRunSnapshot) -> Result<&str> {
    let case = snapshot
        .reviewed_manifest
        .cases()
        .first()
        .ok_or(Error::CorruptStoredData)?;
    if case.kind() != CandidateCaseKind::ColdInitialize || case.cycle() != 0 || case.count() != 1 {
        return Err(Error::CorruptStoredData);
    }
    Ok(case.id())
}
pub(super) fn policy(
    tx: &Transaction<'_>,
    snapshot: &CandidateRunSnapshot,
) -> Result<ResourcePolicySnapshot> {
    let r = snapshot.receipt();
    let resource = read_singleton_policy(tx, r.host_id())
        .map_err(super::map_resource)?
        .ok_or(Error::QualificationDenied)?;
    let policy = read_candidate_policy(tx, r.host_id())
        .map_err(super::map_qualification)?
        .ok_or(Error::QualificationDenied)?;
    let p = &policy.policy;
    let m = snapshot.reviewed_manifest();
    let l = m.limits();
    if !p.allow_qualification_runs
        || !p
            .allowed_manifest_digests
            .iter()
            .any(|d| d == m.manifest_digest())
        || (m.effective_recipe().profile().experimental_controls()
            && !p.allow_experimental_controls)
        || policy.hardware_fingerprint != m.host().hardware_fingerprint()
        || policy.environment_fingerprint != m.host().environment_fingerprint()
        || m.cases().len() > p.max_cases as usize
        || l.max_requests() > p.max_requests
        || l.max_request_body_bytes() > p.max_request_body_bytes
        || l.max_input_tokens_per_request() > p.max_input_tokens_per_request
        || l.max_output_tokens_per_request() > p.max_output_tokens_per_request
        || l.max_run_duration_ms() > p.max_run_duration_ms
        || l.max_cleanup_duration_ms() > p.max_cleanup_duration_ms
    {
        return Err(Error::QualificationDenied);
    }
    Ok(resource)
}
fn fences(tx: &Transaction<'_>, snapshot: &CandidateRunSnapshot) -> Result<()> {
    let r = snapshot.receipt();
    let (revision, generation): (i64, i64) = tx.query_row(
        "SELECT revision,current_generation FROM deployments WHERE id=?1",
        [r.deployment_id()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if revision != r.revision() {
        return Err(Error::RevisionConflict);
    }
    if generation != r.generation() {
        return Err(Error::LifecycleConflict);
    }
    Ok(())
}
pub(super) fn eligible(
    tx: &Transaction<'_>,
    snapshot: &CandidateRunSnapshot,
    fresh: bool,
) -> Result<()> {
    fences(tx, snapshot)?;
    let r = snapshot.receipt();
    if snapshot.state() != super::CandidateRunState::Accepted
        || snapshot.cleanup_state() != super::CandidateCleanupState::Retained
        || snapshot.requests_used() != 0
    {
        return Err(Error::LifecycleConflict);
    }
    let (state, identities): (String, String) = tx.query_row(
        "SELECT state,identities_json FROM runtime_bindings WHERE id=?1",
        [r.binding_id()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let ids = identities_decode(&identities)?;
    if state != "reserved" || !ids.is_empty() {
        return Err(Error::LifecycleConflict);
    }
    let closed: bool = tx.query_row("SELECT desired_state='stopped' AND observed_state='stopped' AND suspended=0 AND admission_enabled=0 AND dispatch_enabled=0 FROM deployments WHERE id=?1",[r.deployment_id()],|r|r.get(0))?;
    let occupied: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM deployment_routes WHERE deployment_id=?1) OR EXISTS(SELECT 1 FROM resource_owners WHERE owner_id=?1) OR EXISTS(SELECT 1 FROM resource_grants WHERE deployment_id=?1) OR EXISTS(SELECT 1 FROM owners WHERE deployment_id=?1)",[r.deployment_id()],|r|r.get(0))?;
    if !closed || occupied {
        return Err(Error::LifecycleConflict);
    }
    if fresh {
        let active: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM qualification_case_actions WHERE run_id=?1) OR EXISTS(SELECT 1 FROM lifecycle_claims WHERE deployment_id=?2) OR EXISTS(SELECT 1 FROM lifecycle_runs WHERE deployment_id=?2 AND state IN ('queued','running','uncertain'))",params![r.run_id(),r.deployment_id()],|r|r.get(0))?;
        if active {
            return Err(Error::LifecycleConflict);
        }
    }
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    role: String,
    pid: u32,
    boot_id: String,
    start_ticks: u64,
}
fn identities_decode(text: &str) -> Result<Vec<Identity>> {
    let ids: Vec<Identity> = decode(text)?;
    for i in &ids {
        if i.role.is_empty() || i.pid == 0 || i.boot_id.is_empty() || i.start_ticks == 0 {
            return Err(Error::CorruptStoredData);
        }
    }
    Ok(ids)
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InitializeStepV2 {
    version: u8,
    planned: InitializeStepV1,
    execution: ExecutionV2,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecutionV2 {
    issued_at_ms: i64,
    grant_id: String,
    launch_settings: StoredLaunch,
    completion_target: StoredTarget,
    resource_policy_revision: i64,
    qualification_policy_revision: i64,
    expected_epoch: u64,
}
// Strict struct variants preserve the original Fake bytes and reject extra fields,
// including fields on a unit-like Fake variant. Native descriptors are versioned.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
enum StoredLaunch {
    Fake(StoredFakeLaunch),
    SglangPinned(Box<StoredSglangLaunch>),
}
impl std::fmt::Debug for StoredLaunch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fake(_) => f.write_str("StoredLaunch::Fake"),
            Self::SglangPinned(native) => f
                .debug_struct("StoredLaunch::SglangPinned")
                .field("version", &native.version)
                .field("binding_id", &native.binding_id)
                .field("incarnation", &native.incarnation)
                .field("rendered_settings_digest", &native.rendered_settings_digest)
                .finish_non_exhaustive(),
        }
    }
}
impl StoredLaunch {
    fn from_snapshot(snapshot: &CandidateRunSnapshot) -> Result<Self> {
        let m = snapshot.reviewed_manifest();
        if matches!(
            m.effective_recipe().profile().launch_settings(),
            CandidateLaunch::Fake
        ) {
            return Ok(Self::Fake(StoredFakeLaunch {
                engine: FakeEngine::Fake,
            }));
        }
        let metadata = m
            .native_launch_metadata(
                snapshot.runtime_credential_ref(),
                snapshot.admin_credential_ref(),
            )
            .map_err(|_| Error::CorruptStoredData)?;
        Ok(Self::SglangPinned(Box::new(StoredSglangLaunch {
            engine: SglangEngine::Sglang,
            version: 1,
            checkpoint_root: m.effective_recipe().model().path.clone(),
            checkpoint_revision: m.effective_recipe().model().revision.clone(),
            executable: m.effective_recipe().profile().executable().into(),
            binding_id: snapshot.receipt().binding_id().into(),
            incarnation: snapshot.receipt().incarnation().into(),
            inference_credential_ref: snapshot
                .runtime_credential_ref()
                .ok_or(Error::CorruptStoredData)?
                .into(),
            admin_credential_ref: snapshot
                .admin_credential_ref()
                .ok_or(Error::CorruptStoredData)?
                .into(),
            rendered_settings_digest: metadata.rendered_settings_digest().into(),
        })))
    }
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredFakeLaunch {
    engine: FakeEngine,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum FakeEngine {
    Fake,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredSglangLaunch {
    engine: SglangEngine,
    version: u8,
    checkpoint_root: String,
    checkpoint_revision: String,
    executable: String,
    binding_id: String,
    incarnation: String,
    inference_credential_ref: String,
    admin_credential_ref: String,
    rendered_settings_digest: String,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SglangEngine {
    Sglang,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredTarget {
    version: u8,
    phase: String,
    allocations: Vec<(String, i64, i64)>,
    devices: Vec<(String, bool)>,
}
impl StoredTarget {
    fn from_footprint(f: &PhaseFootprint) -> Self {
        let mut allocations: Vec<_> = f
            .allocations
            .iter()
            .map(|a| (a.domain.clone(), a.bytes, a.host_kv_bytes))
            .collect();
        allocations.sort();
        let mut devices: Vec<_> = f
            .devices
            .iter()
            .map(|d| (d.device.clone(), d.sharing == Sharing::Shared))
            .collect();
        devices.sort();
        Self {
            version: 1,
            phase: match f.phase {
                ResourcePhase::Cold => "cold",
                ResourcePhase::Ready => "ready",
                _ => unreachable!(),
            }
            .into(),
            allocations,
            devices,
        }
    }
    fn to_footprint(&self) -> Result<PhaseFootprint> {
        if self.version != 1 {
            return Err(Error::CorruptStoredData);
        }
        let phase = match self.phase.as_str() {
            "cold" => ResourcePhase::Cold,
            "ready" => ResourcePhase::Ready,
            _ => return Err(Error::CorruptStoredData),
        };
        let f = PhaseFootprint {
            phase,
            allocations: self
                .allocations
                .iter()
                .map(|(domain, bytes, host_kv_bytes)| Allocation {
                    domain: domain.clone(),
                    bytes: *bytes,
                    host_kv_bytes: *host_kv_bytes,
                })
                .collect(),
            devices: self
                .devices
                .iter()
                .map(|(device, shared)| DeviceClaim {
                    device: device.clone(),
                    sharing: if *shared {
                        Sharing::Shared
                    } else {
                        Sharing::Exclusive
                    },
                })
                .collect(),
        };
        mllm_domain::resources::validate_footprint(&f).map_err(|_| Error::CorruptStoredData)?;
        Ok(f)
    }
}
pub(super) fn footprint(p: &CandidatePhase, phase: ResourcePhase) -> Result<PhaseFootprint> {
    let f = PhaseFootprint {
        phase,
        allocations: p
            .allocations()
            .iter()
            .map(|a| Allocation {
                domain: a.domain().into(),
                bytes: a.bytes(),
                host_kv_bytes: a.host_kv_bytes(),
            })
            .collect(),
        devices: p
            .devices()
            .iter()
            .map(|d| DeviceClaim {
                device: d.id.clone(),
                sharing: match d.sharing {
                    mllm_config::effective::Sharing::Shared => Sharing::Shared,
                    mllm_config::effective::Sharing::Exclusive => Sharing::Exclusive,
                },
            })
            .collect(),
    };
    mllm_domain::resources::validate_footprint(&f).map_err(|_| Error::CorruptStoredData)?;
    Ok(f)
}
fn launch_settings(settings: &CandidateLaunch) -> mllm_domain::launch::ProfileLaunchSettings {
    use mllm_domain::launch::*;
    match settings {
        CandidateLaunch::Fake => ProfileLaunchSettings::Fake(FakeLaunchSettings),
        CandidateLaunch::Vllm {
            tensor_parallel_size,
            pipeline_parallel_size,
            enable_sleep_mode,
            kv_cache_dtype,
            block_size_tokens,
            cpu_offload_bytes,
            requested_budget: b,
        } => ProfileLaunchSettings::Vllm(VllmLaunchSettings {
            tensor_parallel_size: *tensor_parallel_size,
            pipeline_parallel_size: *pipeline_parallel_size,
            enable_sleep_mode: *enable_sleep_mode,
            kv_cache_dtype: kv_cache_dtype.clone(),
            block_size_tokens: *block_size_tokens,
            cpu_offload_bytes: *cpu_offload_bytes,
            requested_budget: VllmRequestedBudget {
                kv_cache_bytes: b.kv_cache_bytes(),
                swap_space_bytes: b.swap_space_bytes(),
                gpu_utilization_pct: b.gpu_utilization_pct(),
            },
        }),
        CandidateLaunch::Sglang {
            recipe,
            tensor_parallel_size,
            data_parallel_size,
            tokenizer_workers,
            model_dtype,
            context_tokens,
            max_running_requests,
            max_total_tokens,
            prefill_cuda_graphs,
            decode_cuda_graphs,
            memory_saver,
            cpu_weight_backup,
            speculative_decoding,
            lora,
            trust_remote_code,
            disaggregation,
            external_cache,
            cpu_kv_offload,
            native_grpc,
            weight_restore,
            requested_budget: b,
        } => ProfileLaunchSettings::Sglang(SglangLaunchSettings {
            recipe: recipe.clone(),
            tensor_parallel_size: *tensor_parallel_size,
            data_parallel_size: *data_parallel_size,
            tokenizer_workers: *tokenizer_workers,
            model_dtype: model_dtype.clone(),
            context_tokens: *context_tokens,
            max_running_requests: *max_running_requests,
            max_total_tokens: *max_total_tokens,
            prefill_cuda_graphs: *prefill_cuda_graphs,
            decode_cuda_graphs: *decode_cuda_graphs,
            memory_saver: *memory_saver,
            cpu_weight_backup: *cpu_weight_backup,
            speculative_decoding: *speculative_decoding,
            lora: *lora,
            trust_remote_code: *trust_remote_code,
            disaggregation: *disaggregation,
            external_cache: *external_cache,
            cpu_kv_offload: *cpu_kv_offload,
            native_grpc: *native_grpc,
            weight_restore: weight_restore.clone(),
            requested_budget: SglangRequestedBudget {
                kv_cache_bytes: b.kv_cache_bytes(),
                static_memory_fraction_bps: b.static_memory_fraction_bps(),
            },
        }),
    }
}
fn load_execution_step(
    tx: &Transaction<'_>,
    id: &str,
) -> std::result::Result<(CandidateRunSnapshot, InitializeReceiptV1, ReadStep), LifecycleError> {
    let result = load_execution_step_immutable(tx, id)?;
    retained(tx, &result.0, &result.2)?;
    Ok(result)
}
fn load_execution_step_immutable(
    tx: &Transaction<'_>,
    id: &str,
) -> std::result::Result<(CandidateRunSnapshot, InitializeReceiptV1, ReadStep), LifecycleError> {
    type ExecutionColumns = (String, String, String, String, String);
    let row:Option<ExecutionColumns>=tx.query_row("SELECT q.principal_id,a.run_id,c.response_json,c.request_hash,c.operation_id FROM lifecycle_steps s JOIN qualification_case_actions a ON a.step_id=s.id JOIN qualification_runs q ON q.id=a.run_id JOIN command_receipts c ON c.operation_id=a.operation_id WHERE s.id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
    let Some((principal, run, json, hash, operation)) = row else {
        let candidate:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id WHERE s.id=?1 AND o.kind='candidate_initialize')",[id],|r|r.get(0))?;
        return Err(if candidate {
            LifecycleError::CorruptStoredData
        } else {
            LifecycleError::Unsupported
        });
    };
    read_receipt_immutable(tx, &principal, &run, &json, &hash, &operation).map_err(Into::into)
}
pub(crate) struct ValidatedInitialize {
    pub snapshot: CandidateRunSnapshot,
    pub context: StepExecutionContext,
    pub session_id: String,
    pub state: String,
    pub run_state: String,
}

/// Immutable decoder only. Callers must separately validate retained accounting or
/// the recorded verified-cleanup chain before accepting historical replay.
pub(crate) fn validated_initialize(
    tx: &Transaction<'_>,
    id: &str,
) -> std::result::Result<ValidatedInitialize, LifecycleError> {
    if super::progression::is_v3(tx, id)? {
        return super::progression::immutable_initialize_anchor(tx, id);
    }
    let (snapshot, _, read) = load_execution_step_immutable(tx, id)?;
    let e = read.execution.ok_or(LifecycleError::Conflict)?;
    let p = read.planned;
    let context = StepExecutionContext {
        token: TransitionToken {
            deployment_id: p.deployment_id,
            revision: p.revision,
            generation: p.generation,
            operation_id: p.operation_id,
            step_id: p.step_id,
            qualification_id: p.qualification_id,
        },
        binding_id: p.binding_id,
        incarnation: p.incarnation,
        issued_at_ms: e.issued_at_ms,
        deadline_ms: p.deadline_ms,
        identities: ExecutionIdentities::OwnedLaunch,
        completion_target: Some(e.completion_target.to_footprint()?),
        grant_id: Some(e.grant_id),
        launch_settings: Some(launch_settings(
            snapshot
                .reviewed_manifest()
                .effective_recipe()
                .profile()
                .launch_settings(),
        )),
    };
    Ok(ValidatedInitialize {
        snapshot,
        context,
        session_id: read.session_id,
        state: read.state,
        run_state: read.run_state,
    })
}
pub(crate) fn validate_retained_initialize(
    tx: &Transaction<'_>,
    id: &str,
) -> std::result::Result<(), LifecycleError> {
    if super::progression::is_v3(tx, id)? {
        let released: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM lifecycle_steps s JOIN runtime_bindings b ON b.id=s.binding_id WHERE s.id=?1 AND b.state='released')", [id], |r| r.get(0))?;
        if released {
            return Err(LifecycleError::Conflict);
        }
        return super::progression::validated_anchor(tx, id).map(|_| ());
    }
    let (snapshot, _, read) = load_execution_step_immutable(tx, id)?;
    retained(tx, &snapshot, &read).map_err(Into::into)
}
pub(crate) fn validate_current_initialize(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    id: &str,
) -> std::result::Result<(), LifecycleError> {
    let (snapshot, _, read) = load_execution_step(tx, id)?;
    current(tx, s, &snapshot, &read)?;
    let running: bool = tx.query_row(
        "SELECT state='running' FROM operations WHERE id=?1",
        [&read.planned.operation_id],
        |r| r.get(0),
    )?;
    if read.state != "armed"
        || read.run_state != "running"
        || !running
        || snapshot.state() != super::CandidateRunState::Running
    {
        return Err(LifecycleError::Conflict);
    }
    Ok(())
}
fn current(
    tx: &Transaction<'_>,
    s: &CoordinatorSession,
    snapshot: &CandidateRunSnapshot,
    read: &ReadStep,
) -> std::result::Result<(), LifecycleError> {
    if read.session_id != s.id() {
        return Err(LifecycleError::Stale);
    }
    fences(tx, snapshot)?;
    claim(tx, &read.planned)?;
    if read.execution.is_some() {
        let uncertain:bool=tx.query_row("SELECT state='uncertain' FROM runtime_bindings WHERE id=?1 AND deployment_id=?2 AND incarnation=?3",params![read.planned.binding_id,read.planned.deployment_id,read.planned.incarnation],|r|r.get(0))?;
        if !uncertain {
            return Err(LifecycleError::Conflict);
        }
    }
    Ok(())
}
struct ReadStep {
    planned: InitializeStepV1,
    execution: Option<ExecutionV2>,
    session_id: String,
    state: String,
    run_state: String,
}
fn read_step(
    tx: &Transaction<'_>,
    snapshot: &CandidateRunSnapshot,
    receipt: &InitializeReceiptV1,
) -> Result<ReadStep> {
    type StepColumns = (
        String,
        String,
        String,
        String,
        String,
        i64,
        Option<String>,
        String,
    );
    let row: Option<StepColumns> = tx.query_row("SELECT operation_id,deployment_id,binding_id,session_id,state,ordinal,grant_id,step_json FROM lifecycle_steps WHERE id=?1",[&receipt.step_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?))).optional()?;
    let (operation, deployment, binding, session_id, state, ordinal, grant, json) =
        row.ok_or(Error::CorruptStoredData)?;
    // Both alternatives decode the original text directly, preserving duplicate-field rejection.
    let (step, execution) = match decode::<InitializeStepV1>(&json) {
        Ok(step) => (step, None),
        Err(_) => {
            let armed: InitializeStepV2 = decode(&json)?;
            if armed.version != 2 {
                return Err(Error::CorruptStoredData);
            }
            (armed.planned, Some(armed.execution))
        }
    };
    let r = snapshot.receipt();
    let identities: String = tx.query_row(
        "SELECT identities_json FROM runtime_bindings WHERE id=?1",
        [r.binding_id()],
        |r| r.get(0),
    )?;
    identities_decode(&identities)?;
    if step.version != 1
        || step.cycle != 0
        || step.principal_id != receipt.principal_id
        || step.run_id != r.run_id()
        || step.creation_operation_id != r.operation_id()
        || step.operation_id != receipt.operation_id
        || operation != step.operation_id
        || step.step_id != receipt.step_id
        || step.deployment_id != r.deployment_id()
        || deployment != step.deployment_id
        || step.revision != r.revision()
        || step.generation != r.generation()
        || step.binding_id != r.binding_id()
        || binding != step.binding_id
        || step.incarnation != r.incarnation()
        || step.qualification_id != format!("candidate:{}", r.run_id())
        || step.case_id != selected(snapshot)?
        || step.case_id != receipt.case_id
        || step.descriptor
            != (DescriptorRefV1 {
                deployment_id: r.deployment_id().into(),
                revision: r.revision(),
                manifest_digest: r.manifest_digest().into(),
                recipe_fingerprint: r.recipe_fingerprint().into(),
            })
        || step.accepted_at_ms != receipt.accepted_at_ms
        || step.deadline_ms != receipt.deadline_ms
        || !super::ulid(&session_id)
        || ordinal != 0
        || !matches!(
            state.as_str(),
            "planned" | "armed" | "uncertain" | "completed" | "cancelled"
        )
    {
        return Err(Error::CorruptStoredData);
    }
    let association: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM qualification_case_actions WHERE run_id=?1 AND case_id=?2 AND operation_id=?3 AND step_id=?4)",params![step.run_id,step.case_id,step.operation_id,step.step_id],|r|r.get(0))?;
    let valid_op: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1 AND deployment_id=?2 AND kind='candidate_initialize' AND state IN ('pending','running','succeeded','failed') AND idempotency_key IS NULL)",params![step.operation_id,step.deployment_id],|r|r.get(0))?;
    let count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM lifecycle_steps WHERE operation_id=?1",
        [&step.operation_id],
        |r| r.get(0),
    )?;
    if !association || !valid_op || count != 1 {
        return Err(Error::CorruptStoredData);
    }
    let run_state = validate_candidate_initialize_run(
        tx,
        &step.fence(),
        &step.operation_id,
        &session_id,
        step.deadline_ms,
    )
    .map_err(|e| match e {
        LifecycleError::Sql(e) => Error::Sql(e),
        _ => Error::CorruptStoredData,
    })?;
    match &execution {
        None if grant.is_some() || state == "armed" => return Err(Error::CorruptStoredData),
        Some(e) => {
            if state == "planned"
                || grant.as_deref() != Some(e.grant_id.as_str())
                || !super::ulid(&e.grant_id)
                || e.issued_at_ms < step.accepted_at_ms
                || e.issued_at_ms >= step.deadline_ms
                || e.resource_policy_revision <= 0
                || e.qualification_policy_revision <= 0
                || e.launch_settings != StoredLaunch::from_snapshot(snapshot)?
                || e.completion_target
                    != StoredTarget::from_footprint(&footprint(
                        snapshot
                            .reviewed_manifest()
                            .effective_recipe()
                            .resources()
                            .ready(),
                        ResourcePhase::Ready,
                    )?)
            {
                return Err(Error::CorruptStoredData);
            }
            let row:Option<(String,String,String,u64)>=tx.query_row("SELECT deployment_id,operation_id,request_json,committed_epoch FROM resource_grants WHERE id=?1",[&e.grant_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
            let (deployment, operation, json, epoch) = row.ok_or(Error::CorruptStoredData)?;
            let cold = footprint(
                snapshot
                    .reviewed_manifest()
                    .effective_recipe()
                    .resources()
                    .cold(),
                ResourcePhase::Cold,
            )?;
            let encoded = encode(&StoredTarget::from_footprint(&cold))?;
            let expected = encode(&(
                &step.deployment_id,
                &step.operation_id,
                step.revision,
                step.generation,
                e.expected_epoch,
                &encoded,
            ))?;
            if deployment != step.deployment_id
                || operation != step.operation_id
                || json != expected
                || e.expected_epoch.checked_add(1) != Some(epoch)
            {
                return Err(Error::CorruptStoredData);
            }
            let ledger = crate::resource_ledger::read_snapshot(tx).map_err(|e| match e {
                ResourceStoreError::Sql(e) => Error::Sql(e),
                _ => Error::CorruptStoredData,
            })?;
            // Ledger serialization canonicalizes allocation/device order; reviewed recipe
            // order is immutable but is not part of accounting identity.
            if ledger.epoch < epoch {
                return Err(Error::CorruptStoredData);
            }
        }
        _ => {}
    }
    Ok(ReadStep {
        planned: step,
        execution,
        session_id,
        state,
        run_state,
    })
}
fn read_receipt(
    tx: &Transaction<'_>,
    principal: &str,
    run: &str,
    json: &str,
    column_hash: &str,
    operation: &str,
) -> Result<(CandidateRunSnapshot, InitializeReceiptV1, ReadStep)> {
    let result = read_receipt_immutable(tx, principal, run, json, column_hash, operation)?;
    if result.2.execution.is_some() {
        let v = validated_initialize(tx, &result.2.planned.step_id).map_err(|e| match e {
            LifecycleError::Sql(e) => Error::Sql(e),
            _ => Error::CorruptStoredData,
        })?;
        crate::lifecycle::completion::accounting(tx, &v).map_err(|e| match e {
            LifecycleError::Sql(e) => Error::Sql(e),
            _ => Error::CorruptStoredData,
        })?;
    }
    Ok(result)
}
fn retained(tx: &Transaction<'_>, snapshot: &CandidateRunSnapshot, read: &ReadStep) -> Result<()> {
    if read.execution.is_some() {
        let cold = footprint(
            snapshot
                .reviewed_manifest()
                .effective_recipe()
                .resources()
                .cold(),
            ResourcePhase::Cold,
        )?;
        let ledger =
            crate::resource_ledger::read_snapshot(tx).map_err(|_| Error::CorruptStoredData)?;
        if ledger
            .owners
            .get(&read.planned.deployment_id)
            .is_none_or(|owner| {
                StoredTarget::from_footprint(owner) != StoredTarget::from_footprint(&cold)
            })
        {
            return Err(Error::CorruptStoredData);
        }
    }
    Ok(())
}
fn read_receipt_immutable(
    tx: &Transaction<'_>,
    principal: &str,
    run: &str,
    json: &str,
    column_hash: &str,
    operation: &str,
) -> Result<(CandidateRunSnapshot, InitializeReceiptV1, ReadStep)> {
    let snapshot = super::read_snapshot(tx, principal, run)?.ok_or(Error::CorruptStoredData)?;
    let receipt: InitializeReceiptV1 = decode(json)?;
    let r = snapshot.receipt();
    let rows:Vec<(String,String,String,String,String)>=tx.prepare("SELECT principal_id,command_scope,idempotency_key,request_hash,response_json FROM command_receipts WHERE operation_id=?1")?.query_map([operation],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?.collect::<std::result::Result<_,_>>()?;
    if rows.len() != 1 {
        return Err(Error::CorruptStoredData);
    }
    let (owner, scope, key, stored_hash, text) = &rows[0];
    if owner != principal
        || scope != &format!("POST {}", target(run))
        || !super::valid_id(key)
        || stored_hash != column_hash
        || text != json
    {
        return Err(Error::CorruptStoredData);
    }
    if receipt.version != 1
        || receipt.target != target(run)
        || receipt.principal_id != principal
        || receipt.run_id != run
        || receipt.request_hash != column_hash
        || receipt.request_hash != hash(principal, run, receipt.revision, receipt.deadline_ms)?
        || receipt.operation_id != operation
        || !super::ulid(operation)
        || operation == r.operation_id()
        || !super::ulid(&receipt.step_id)
        || receipt.deployment_id != r.deployment_id()
        || receipt.revision != r.revision()
        || receipt.generation != r.generation()
        || receipt.accepted_at_ms < r.accepted_at_ms()
        || receipt.deadline_ms <= receipt.accepted_at_ms
        || receipt.deadline_ms > r.deadline_ms()
        || receipt.case_id != selected(&snapshot)?
    {
        return Err(Error::CorruptStoredData);
    }
    let step = read_step(tx, &snapshot, &receipt)?;
    Ok((snapshot, receipt, step))
}
fn claim(tx: &Transaction<'_>, step: &InitializeStepV1) -> Result<()> {
    let exact: bool = tx.query_row("SELECT COUNT(*)=1 AND COALESCE(SUM(deployment_id=?2 AND revision=?3 AND generation=?4),0)=1 FROM lifecycle_claims WHERE operation_id=?1",params![step.operation_id,step.deployment_id,step.revision,step.generation],|r|r.get(0))?;
    let competing:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM lifecycle_runs WHERE deployment_id=?1 AND operation_id!=?2 AND state IN ('queued','running','uncertain'))",params![step.deployment_id,step.operation_id],|r|r.get(0))?;
    if !exact || competing {
        return Err(Error::LifecycleConflict);
    }
    Ok(())
}
fn constraint(e: rusqlite::Error) -> Error {
    if matches!(&e,rusqlite::Error::SqliteFailure(e,_) if matches!(e.extended_code,1555|2067)) {
        Error::LifecycleConflict
    } else {
        Error::Sql(e)
    }
}
impl crate::Store {
    pub fn accept_candidate_initialize(
        &self,
        s: &CoordinatorSession,
        principal: &str,
        run: &str,
        key: &str,
        text: &str,
        now: i64,
    ) -> Result<CandidateInitializeReceipt> {
        if text.len() > MAX_BYTES
            || !super::valid_id(principal)
            || !super::valid_id(key)
            || !super::ulid(run)
            || now < 0
        {
            return Err(Error::InvalidCommand);
        }
        let command: InitializeCommand =
            serde_json::from_str(text).map_err(|_| Error::InvalidCommand)?;
        let InitializeAction::Initialize = command.action;
        if command.expected_revision <= 0 || command.deadline_ms <= 0 {
            return Err(Error::InvalidCommand);
        }
        let hash = hash(
            principal,
            run,
            command.expected_revision,
            command.deadline_ms,
        )?;
        let scope = format!("POST {}", target(run));
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        session(&tx, s)?;
        let prior: Option<(String,String,String)> = tx.query_row("SELECT request_hash,operation_id,response_json FROM command_receipts WHERE principal_id=?1 AND command_scope=?2 AND idempotency_key=?3",params![principal,scope,key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        if let Some((stored_hash, operation, json)) = prior {
            if stored_hash != hash {
                return Err(Error::IdempotencyConflict);
            }
            let (_, receipt, _) =
                read_receipt(&tx, principal, run, &json, &stored_hash, &operation)?;
            tx.commit()?;
            return Ok(CandidateInitializeReceipt { inner: receipt });
        }
        let snapshot = super::read_snapshot(&tx, principal, run)?.ok_or(Error::NotFound)?;
        let r = snapshot.receipt();
        if command.expected_revision != r.revision() {
            return Err(Error::RevisionConflict);
        }
        eligible(&tx, &snapshot, true)?;
        policy(&tx, &snapshot)?;
        if now < r.accepted_at_ms()
            || command.deadline_ms <= now
            || command.deadline_ms > r.deadline_ms()
            || command.deadline_ms.checked_sub(now).is_none()
        {
            return Err(Error::QualificationDenied);
        }
        let op = ulid::Ulid::new();
        let id = ulid::Ulid::new();
        let receipt = InitializeReceiptV1 {
            version: 1,
            method: Method::Post,
            target: target(run),
            principal_id: principal.into(),
            run_id: run.into(),
            request_hash: hash.clone(),
            operation_id: op.to_string(),
            deployment_id: r.deployment_id().into(),
            step_id: id.to_string(),
            case_id: selected(&snapshot)?.into(),
            revision: r.revision(),
            generation: r.generation(),
            accepted_at_ms: now,
            deadline_ms: command.deadline_ms,
        };
        let step = InitializeStepV1 {
            version: 1,
            kind: PlannedKind::Initialize,
            principal_id: principal.into(),
            run_id: run.into(),
            creation_operation_id: r.operation_id().into(),
            operation_id: op.to_string(),
            step_id: id.to_string(),
            deployment_id: r.deployment_id().into(),
            revision: r.revision(),
            generation: r.generation(),
            binding_id: r.binding_id().into(),
            incarnation: r.incarnation().into(),
            qualification_id: format!("candidate:{run}"),
            case_id: receipt.case_id.clone(),
            case_kind: ColdKind::Cold,
            cycle: 0,
            descriptor: DescriptorRefV1 {
                deployment_id: r.deployment_id().into(),
                revision: r.revision(),
                manifest_digest: r.manifest_digest().into(),
                recipe_fingerprint: r.recipe_fingerprint().into(),
            },
            accepted_at_ms: now,
            deadline_ms: command.deadline_ms,
            identities: OwnedIdentity::Owned,
        };
        tx.execute("INSERT INTO operations(id,deployment_id,kind,state) VALUES(?1,?2,'candidate_initialize','pending')",params![receipt.operation_id,receipt.deployment_id])?;
        insert_candidate_initialize_run(
            &tx,
            s,
            &step.fence(),
            &step.operation_id,
            step.deadline_ms,
        )
        .map_err(|e| match e {
            LifecycleError::Sql(e) => constraint(e),
            _ => Error::LifecycleConflict,
        })?;
        tx.execute("INSERT INTO lifecycle_claims(deployment_id,operation_id,revision,generation) VALUES(?1,?2,?3,?4)",params![step.deployment_id,step.operation_id,step.revision,step.generation]).map_err(constraint)?;
        tx.execute("INSERT INTO lifecycle_steps(id,operation_id,ordinal,deployment_id,binding_id,session_id,state,step_json) VALUES(?1,?2,0,?3,?4,?5,'planned',?6)",params![step.step_id,step.operation_id,step.deployment_id,step.binding_id,s.id(),encode(&step)?])?;
        tx.execute("INSERT INTO qualification_case_actions(run_id,case_id,operation_id,step_id) VALUES(?1,?2,?3,?4)",params![run,step.case_id,step.operation_id,step.step_id]).map_err(constraint)?;
        let json = encode(&receipt)?;
        tx.execute("INSERT INTO command_receipts(principal_id,command_scope,idempotency_key,request_hash,operation_id,response_json) VALUES(?1,?2,?3,?4,?5,?6)",params![principal,scope,key,hash,receipt.operation_id,json])?;
        append_event(
            &tx,
            &EventMetadata::CandidateInitializeAccepted {
                operation_id: EventOperationId::generated(op),
                deployment_id: EventOperationId::generated(
                    r.deployment_id()
                        .parse()
                        .map_err(|_| Error::CorruptStoredData)?,
                ),
                run_id: EventOperationId::generated(
                    run.parse().map_err(|_| Error::CorruptStoredData)?,
                ),
                step_id: EventOperationId::generated(id),
                revision: r.revision(),
                generation: r.generation(),
                session_epoch: s.epoch(),
            },
        )
        .map_err(|e| match e {
            EventWriteError::Sql(e) => Error::Sql(e),
            _ => Error::CorruptStoredData,
        })?;
        let (_, read, _) = read_receipt(&tx, principal, run, &json, &hash, &receipt.operation_id)?;
        if read != receipt {
            return Err(Error::CorruptStoredData);
        }
        tx.commit()?;
        Ok(CandidateInitializeReceipt { inner: read })
    }
    pub fn candidate_initialize_plan(
        &self,
        s: &CoordinatorSession,
        principal: &str,
        run: &str,
        id: &str,
    ) -> Result<Option<CandidateInitializePlan>> {
        if !super::valid_id(principal) || !super::ulid(run) || !super::ulid(id) {
            return Err(Error::InvalidCommand);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        session(&tx, s)?;
        let Some(owned) = super::read_snapshot(&tx, principal, run)? else {
            return Ok(None);
        };
        let belongs: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM lifecycle_steps WHERE id=?1 AND deployment_id=?2)",
            params![id, owned.receipt().deployment_id()],
            |r| r.get(0),
        )?;
        if !belongs {
            return Ok(None);
        }
        let row: Option<(String,String,String)> = tx.query_row("SELECT c.response_json,c.request_hash,c.operation_id FROM qualification_case_actions a JOIN command_receipts c ON c.operation_id=a.operation_id WHERE a.run_id=?1 AND a.step_id=?2 AND c.principal_id=?3 AND c.command_scope=?4",params![run,id,principal,format!("POST {}",target(run))],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let Some((json, hash, op)) = row else {
            return Err(Error::CorruptStoredData);
        };
        let (snapshot, receipt, read) = read_receipt(&tx, principal, run, &json, &hash, &op)?;
        if read.session_id != s.id() {
            return Err(Error::StaleSession);
        }
        fences(&tx, &snapshot)?;
        claim(&tx, &read.planned)?;
        if read.state != "planned" || read.run_state != "queued" {
            return Err(Error::LifecycleConflict);
        }
        tx.commit()?;
        Ok(Some(CandidateInitializePlan {
            receipt: CandidateInitializeReceipt { inner: receipt },
            binding_id: read.planned.binding_id,
            incarnation: read.planned.incarnation,
            qualification_id: read.planned.qualification_id,
            reviewed_manifest: snapshot.reviewed_manifest,
        }))
    }
}
#[cfg(test)]
mod tests;

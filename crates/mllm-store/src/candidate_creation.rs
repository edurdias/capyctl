//! Durable candidate acceptance and informational historical reads. No execution authority.
pub mod initialize;
use crate::dispatch::{check_session, CoordinatorSession, DispatchError};
use crate::events::{append_event, EventMetadata, EventOperationId, EventWriteError};
use crate::lifecycle::{
    decode_binding, insert_prepared_binding, DecodedBinding, LifecycleError, PreparedBinding,
};
use crate::qualification_policy::{
    read_candidate_policy, CandidatePolicy, QualificationImportError,
};
use crate::resource_policy::{read_singleton_policy, ResourcePolicyError, ResourcePolicySnapshot};
use mllm_config::effective::candidate::{
    normalize_candidate_manifest, validate_candidate_reviewed_snapshot_text,
    CandidateReviewedSnapshot, NormalizedCandidateManifest,
};
use mllm_config::effective::Sharing;
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, value::RawValue, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const MAX_BYTES: usize = 1 << 20;
const METHOD: &str = "POST";
const TARGET: &str = "/management/v1/qualification-runs";
const SCOPE: &str = "POST /management/v1/qualification-runs";

#[derive(Debug, thiserror::Error)]
pub enum CandidateCreationError {
    #[error("invalid candidate command")]
    InvalidCommand,
    #[error("stale coordinator session")]
    StaleSession,
    #[error("candidate idempotency conflict")]
    IdempotencyConflict,
    #[error("candidate resource revision or host conflict")]
    RevisionConflict,
    #[error("candidate qualification denied")]
    QualificationDenied,
    #[error("candidate endpoint unavailable")]
    EndpointUnavailable,
    #[error("corrupt stored candidate data")]
    CorruptStoredData,
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}
type Result<T> = std::result::Result<T, CandidateCreationError>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Command<'a> {
    host_id: String,
    expected_host_revision: i64,
    recipe_digest: String,
    #[serde(borrow)]
    manifest: &'a RawValue,
    deadline_ms: i64,
    allow_owned_abort_cleanup: bool,
}

// deserialize_with makes null legal while still requiring the field to be present.
fn required_nullable<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<String>, D::Error> {
    Option::<String>::deserialize(d)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CredentialRefs {
    #[serde(deserialize_with = "required_nullable")]
    runtime: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    admin: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DescriptorV1 {
    version: u8,
    pub(crate) deployment_id: String,
    pub(crate) revision: i64,
    pub(crate) generation: i64,
    host_id: String,
    reviewed_manifest: Box<RawValue>,
    manifest_digest: String,
    recipe_fingerprint: String,
    pub(crate) credential_refs: CredentialRefs,
    total_case_request_budget: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DescriptorRefV1 {
    deployment_id: String,
    revision: i64,
    manifest_digest: String,
    recipe_fingerprint: String,
}
impl DescriptorV1 {
    pub(crate) fn reference(&self) -> DescriptorRefV1 {
        DescriptorRefV1 {
            deployment_id: self.deployment_id.clone(),
            revision: self.revision,
            manifest_digest: self.manifest_digest.clone(),
            recipe_fingerprint: self.recipe_fingerprint.clone(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CandidateBindingV2 {
    pub(crate) version: u8,
    pub(crate) qualification_id: String,
    pub(crate) endpoint: String,
    pub(crate) descriptor: DescriptorRefV1,
    pub(crate) auth: CredentialRefs,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorizationV1 {
    version: u8,
    principal_id: String,
    run_id: String,
    operation_id: String,
    deployment_id: String,
    revision: i64,
    generation: i64,
    binding_id: String,
    incarnation: String,
    host_id: String,
    hardware_fingerprint: String,
    environment_fingerprint: String,
    resource_policy_revision: i64,
    qualification_policy_revision: i64,
    manifest_digest: String,
    recipe_fingerprint: String,
    descriptor: DescriptorRefV1,
    accepted_at_ms: i64,
    deadline_ms: i64,
    allow_owned_abort_cleanup: bool,
    qualification_runs_permitted: bool,
    experimental_controls_permitted: bool,
    max_cleanup_duration_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptV1 {
    version: u8,
    method: String,
    target: String,
    principal_id: String,
    request_hash: String,
    run_id: String,
    operation_id: String,
    deployment_id: String,
    revision: i64,
    generation: i64,
    binding_id: String,
    incarnation: String,
    host_id: String,
    manifest_digest: String,
    recipe_fingerprint: String,
    resource_policy_revision: i64,
    qualification_policy_revision: i64,
    accepted_at_ms: i64,
    deadline_ms: i64,
    allow_owned_abort_cleanup: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandidateCreationReceipt {
    inner: ReceiptV1,
}
macro_rules! string_getters {
    ($($field:ident),* $(,)?) => {$ (pub fn $field(&self) -> &str { &self.inner.$field })*};
}
macro_rules! integer_getters {
    ($($field:ident),* $(,)?) => {$ (pub fn $field(&self) -> i64 { self.inner.$field })*};
}
impl CandidateCreationReceipt {
    pub fn version(&self) -> u8 {
        self.inner.version
    }
    string_getters!(
        run_id,
        operation_id,
        deployment_id,
        binding_id,
        incarnation,
        host_id,
        manifest_digest,
        recipe_fingerprint
    );
    integer_getters!(
        revision,
        generation,
        resource_policy_revision,
        qualification_policy_revision,
        accepted_at_ms,
        deadline_ms
    );
    pub fn allow_owned_abort_cleanup(&self) -> bool {
        self.inner.allow_owned_abort_cleanup
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateRunState {
    Accepted,
    Running,
    Passed,
    Failed,
    Uncertain,
    Aborted,
    Expired,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateCleanupState {
    Retained,
    VerifiedGone,
}

#[derive(Clone, Debug)]
pub struct CandidateRunSnapshot {
    receipt: CandidateCreationReceipt,
    reviewed_manifest: CandidateReviewedSnapshot,
    credential_refs: CredentialRefs,
    state: CandidateRunState,
    requests_used: u32,
    cleanup_state: CandidateCleanupState,
}
impl CandidateRunSnapshot {
    pub fn receipt(&self) -> &CandidateCreationReceipt {
        &self.receipt
    }
    pub fn reviewed_manifest(&self) -> &CandidateReviewedSnapshot {
        &self.reviewed_manifest
    }
    pub fn runtime_credential_ref(&self) -> Option<&str> {
        self.credential_refs.runtime.as_deref()
    }
    pub fn admin_credential_ref(&self) -> Option<&str> {
        self.credential_refs.admin.as_deref()
    }
    pub fn state(&self) -> CandidateRunState {
        self.state
    }
    pub fn requests_used(&self) -> u32 {
        self.requests_used
    }
    pub fn cleanup_state(&self) -> CandidateCleanupState {
        self.cleanup_state
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256
}
fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn ulid(value: &str) -> bool {
    value
        .parse::<ulid::Ulid>()
        .is_ok_and(|id| id.to_string() == value)
}
fn decode<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
    if text.len() > MAX_BYTES {
        return Err(CandidateCreationError::CorruptStoredData);
    }
    serde_json::from_str(text).map_err(|_| CandidateCreationError::CorruptStoredData)
}
fn encode(value: &impl Serialize) -> Result<String> {
    let text = serde_json::to_string(value).map_err(|_| CandidateCreationError::InvalidCommand)?;
    if text.len() > MAX_BYTES {
        return Err(CandidateCreationError::InvalidCommand);
    }
    Ok(text)
}
fn canonical(value: Value) -> Value {
    match value {
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(k, v)| (k, canonical(v)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(canonical).collect()),
        other => other,
    }
}
fn request_hash(
    host: &str,
    revision: i64,
    manifest_digest: &str,
    manifest: &CandidateReviewedSnapshot,
    deadline: i64,
    cleanup: bool,
) -> Result<String> {
    let reviewed: Value = serde_json::from_slice(manifest.reviewed_json())
        .map_err(|_| CandidateCreationError::InvalidCommand)?;
    let text = encode(&canonical(
        json!({"version":1,"method":METHOD,"target":TARGET,"host_id":host,
        "expected_host_revision":revision,"recipe_digest":manifest_digest,"manifest":reviewed,
        "deadline_ms":deadline,"allow_owned_abort_cleanup":cleanup}),
    ))?;
    let mut hash = Sha256::new();
    hash.update(b"mllm.candidate-create-command.v1\0");
    hash.update(text.as_bytes());
    Ok(format!("{:x}", hash.finalize()))
}
fn parse_command<'a>(
    text: &'a str,
    principal: &str,
    key: &str,
) -> Result<(Command<'a>, CandidateReviewedSnapshot, String)> {
    if text.len() > MAX_BYTES || !valid_id(principal) || !valid_id(key) {
        return Err(CandidateCreationError::InvalidCommand);
    }
    let command: Command<'_> =
        serde_json::from_str(text).map_err(|_| CandidateCreationError::InvalidCommand)?;
    let manifest = validate_candidate_reviewed_snapshot_text(command.manifest.get())
        .map_err(|_| CandidateCreationError::InvalidCommand)?;
    if !valid_id(&command.host_id)
        || command.expected_host_revision <= 0
        || command.deadline_ms <= 0
        || command.host_id != manifest.host_id()
        || command.recipe_digest != manifest.manifest_digest()
    {
        return Err(CandidateCreationError::InvalidCommand);
    }
    let hash = request_hash(
        &command.host_id,
        command.expected_host_revision,
        &command.recipe_digest,
        &manifest,
        command.deadline_ms,
        command.allow_owned_abort_cleanup,
    )?;
    Ok((command, manifest, hash))
}

fn map_session(error: DispatchError) -> CandidateCreationError {
    match error {
        DispatchError::Sql(e) => CandidateCreationError::Sql(e),
        DispatchError::StaleSession => CandidateCreationError::StaleSession,
        _ => CandidateCreationError::CorruptStoredData,
    }
}
fn map_resource(error: ResourcePolicyError) -> CandidateCreationError {
    match error {
        ResourcePolicyError::Sql(e) => CandidateCreationError::Sql(e),
        ResourcePolicyError::RevisionConflict => CandidateCreationError::RevisionConflict,
        _ => CandidateCreationError::CorruptStoredData,
    }
}
fn map_qualification(error: QualificationImportError) -> CandidateCreationError {
    match error {
        QualificationImportError::Sql(e) => CandidateCreationError::Sql(e),
        QualificationImportError::Conflict => CandidateCreationError::RevisionConflict,
        _ => CandidateCreationError::CorruptStoredData,
    }
}
fn map_binding(error: LifecycleError) -> CandidateCreationError {
    match error {
        LifecycleError::Sql(e) => CandidateCreationError::Sql(e),
        LifecycleError::Stale => CandidateCreationError::StaleSession,
        _ => CandidateCreationError::CorruptStoredData,
    }
}

fn compose_host(local: &Value, resource: &ResourcePolicySnapshot) -> Result<Value> {
    let context = &resource.context;
    let raw = &local["resource_policy"];
    let domain_ids: BTreeSet<_> = raw["domains"]
        .as_object()
        .ok_or(CandidateCreationError::RevisionConflict)?
        .keys()
        .cloned()
        .collect();
    let devices: BTreeMap<_, _> = raw["devices"]
        .as_object()
        .ok_or(CandidateCreationError::RevisionConflict)?
        .iter()
        .map(|(id, device)| {
            Ok((
                id.clone(),
                device["domain"]
                    .as_str()
                    .ok_or(CandidateCreationError::RevisionConflict)?
                    .to_owned(),
            ))
        })
        .collect::<Result<_>>()?;
    if local["name"].as_str() != Some(&context.host_id)
        || domain_ids != context.domain_ids
        || devices != context.device_domains
        || raw["endpoint_port_range"]["start"].as_u64()
            != Some(u64::from(context.endpoint_port_range.start))
        || raw["endpoint_port_range"]["end"].as_u64()
            != Some(u64::from(context.endpoint_port_range.end))
    {
        return Err(CandidateCreationError::RevisionConflict);
    }
    let controls = &resource.controls;
    let sharing = |s: Sharing| match s {
        Sharing::Shared => "shared",
        Sharing::Exclusive => "exclusive",
    };
    let mut domains = serde_json::Map::new();
    for (id, d) in &controls.domains {
        let mut value = json!({"managed_limit":format!("{}B",d.managed_limit),"free_reserve":format!("{}B",d.free_reserve)});
        if let Some(n) = d.host_kv_limit {
            value["host_kv_limit"] = json!(format!("{n}B"));
        }
        if let Some(n) = d.parked_limit {
            value["parked_limit"] = json!(format!("{n}B"));
        }
        domains.insert(id.clone(), value);
    }
    let devices: serde_json::Map<_, _> = context
        .device_domains
        .iter()
        .map(|(id, domain)| {
            let s = controls
                .device_sharing_overrides
                .get(id)
                .copied()
                .unwrap_or(controls.device_sharing);
            (id.clone(), json!({"domain":domain,"sharing":sharing(s)}))
        })
        .collect();
    let q = &controls.queue;
    let mut composed = local.clone();
    composed["resource_policy"] = json!({"domains":domains,"devices":devices,"device_sharing":sharing(controls.device_sharing),
        "endpoint_port_range":{"start":context.endpoint_port_range.start,"end":context.endpoint_port_range.end},
        "max_parked":controls.max_parked,"observation_ttl":format!("{}ms",controls.observation_ttl_ms),"planner_max_states":controls.planner_max_states,
        "queue":{"max_pending_per_deployment":q.max_pending_per_deployment,"max_pending_total":q.max_pending_total,
            "max_buffered_bytes_total":format!("{}B",q.max_buffered_bytes_total),"request_deadline":format!("{}ms",q.request_deadline_ms),"admission_window":format!("{}ms",q.admission_window_ms)}});
    Ok(composed)
}

fn check_policy(
    policy: &CandidatePolicy,
    normalized: &NormalizedCandidateManifest,
    now: i64,
    deadline: i64,
) -> Result<()> {
    let p = &policy.policy;
    let limits = normalized.limits();
    let remaining = deadline
        .checked_sub(now)
        .filter(|n| *n > 0)
        .ok_or(CandidateCreationError::QualificationDenied)?;
    if now < 0
        || !p.allow_qualification_runs
        || !p
            .allowed_manifest_digests
            .iter()
            .any(|d| d == normalized.manifest_digest())
        || (normalized
            .effective_recipe()
            .profile()
            .experimental_controls()
            && !p.allow_experimental_controls)
        || policy.hardware_fingerprint != normalized.host().hardware_fingerprint()
        || policy.environment_fingerprint != normalized.host().environment_fingerprint()
        || normalized.cases().len() > p.max_cases as usize
        || limits.max_requests() > p.max_requests
        || limits.max_request_body_bytes() > p.max_request_body_bytes
        || limits.max_input_tokens_per_request() > p.max_input_tokens_per_request
        || limits.max_output_tokens_per_request() > p.max_output_tokens_per_request
        || limits.max_run_duration_ms() > p.max_run_duration_ms
        || limits.max_cleanup_duration_ms() > p.max_cleanup_duration_ms
        || remaining > limits.max_run_duration_ms()
        || remaining > p.max_run_duration_ms
    {
        return Err(CandidateCreationError::QualificationDenied);
    }
    Ok(())
}

impl crate::Store {
    /// Internal numeric command facade. Management wire decoding and authentication are separate.
    #[allow(clippy::too_many_arguments)]
    pub fn create_candidate_run(
        &self,
        session: &CoordinatorSession,
        principal_id: &str,
        idempotency_key: &str,
        command_json: &str,
        trusted_local_host: &Value,
        now_ms: i64,
    ) -> Result<CandidateCreationReceipt> {
        let (command, manifest, hash) = parse_command(command_json, principal_id, idempotency_key)?;
        // Declared before transaction: every failure rolls back before releasing its listener.
        let prepared;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session).map_err(map_session)?;
        let existing: Option<(String,String,String)> = tx.query_row(
            "SELECT request_hash,operation_id,response_json FROM command_receipts WHERE principal_id=?1 AND command_scope=?2 AND idempotency_key=?3",
            params![principal_id,SCOPE,idempotency_key], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        if let Some((stored_hash, operation_id, json)) = existing {
            if stored_hash != hash {
                return Err(CandidateCreationError::IdempotencyConflict);
            }
            let receipt: ReceiptV1 = decode(&json)?;
            if receipt.request_hash != hash
                || receipt.operation_id != operation_id
                || receipt.principal_id != principal_id
            {
                return Err(CandidateCreationError::CorruptStoredData);
            }
            let snapshot = read_snapshot(&tx, principal_id, &receipt.run_id)?
                .ok_or(CandidateCreationError::CorruptStoredData)?;
            if snapshot.receipt.inner != receipt {
                return Err(CandidateCreationError::CorruptStoredData);
            }
            tx.commit()?;
            return Ok(snapshot.receipt);
        }
        let resource = read_singleton_policy(&tx, &command.host_id)
            .map_err(map_resource)?
            .ok_or(CandidateCreationError::RevisionConflict)?;
        let policy = read_candidate_policy(&tx, &command.host_id)
            .map_err(map_qualification)?
            .ok_or(CandidateCreationError::QualificationDenied)?;
        if resource.revision != command.expected_host_revision {
            return Err(CandidateCreationError::RevisionConflict);
        }
        let host = compose_host(trusted_local_host, &resource)?;
        let value: Value = serde_json::from_slice(manifest.reviewed_json())
            .map_err(|_| CandidateCreationError::InvalidCommand)?;
        let normalized = normalize_candidate_manifest(&value, &host)
            .map_err(|_| CandidateCreationError::InvalidCommand)?;
        if normalized.reviewed_json() != manifest.reviewed_json()
            || normalized.manifest_digest() != command.recipe_digest
        {
            return Err(CandidateCreationError::InvalidCommand);
        }
        check_policy(&policy, &normalized, now_ms, command.deadline_ms)?;
        let deployment_id = ulid::Ulid::new();
        let run_id = ulid::Ulid::new();
        let operation_id = ulid::Ulid::new();
        let binding_id = ulid::Ulid::new().to_string();
        let incarnation = ulid::Ulid::new().to_string();
        let descriptor = DescriptorV1 {
            version: 1,
            deployment_id: deployment_id.to_string(),
            revision: 1,
            generation: 1,
            host_id: command.host_id.clone(),
            reviewed_manifest: RawValue::from_string(
                String::from_utf8(normalized.reviewed_json().to_vec())
                    .map_err(|_| CandidateCreationError::InvalidCommand)?,
            )
            .map_err(|_| CandidateCreationError::InvalidCommand)?,
            manifest_digest: normalized.manifest_digest().into(),
            recipe_fingerprint: normalized.recipe_fingerprint().into(),
            credential_refs: CredentialRefs {
                runtime: normalized.credential_refs().runtime().map(str::to_owned),
                admin: normalized.credential_refs().admin().map(str::to_owned),
            },
            total_case_request_budget: normalized.total_case_request_budget(),
        };
        prepared = PreparedBinding::prepare_candidate(
            &tx,
            &descriptor,
            &run_id.to_string(),
            &binding_id,
            &incarnation,
            &resource.context.endpoint_port_range,
        )?;
        let receipt = ReceiptV1 {
            version: 1,
            method: METHOD.into(),
            target: TARGET.into(),
            principal_id: principal_id.into(),
            request_hash: hash.clone(),
            run_id: run_id.to_string(),
            operation_id: operation_id.to_string(),
            deployment_id: deployment_id.to_string(),
            revision: 1,
            generation: 1,
            binding_id,
            incarnation,
            host_id: command.host_id,
            manifest_digest: descriptor.manifest_digest.clone(),
            recipe_fingerprint: descriptor.recipe_fingerprint.clone(),
            resource_policy_revision: resource.revision,
            qualification_policy_revision: policy.revision,
            accepted_at_ms: now_ms,
            deadline_ms: command.deadline_ms,
            allow_owned_abort_cleanup: command.allow_owned_abort_cleanup,
        };
        let authorization = AuthorizationV1 {
            version: 1,
            principal_id: receipt.principal_id.clone(),
            run_id: receipt.run_id.clone(),
            operation_id: receipt.operation_id.clone(),
            deployment_id: receipt.deployment_id.clone(),
            revision: 1,
            generation: 1,
            binding_id: receipt.binding_id.clone(),
            incarnation: receipt.incarnation.clone(),
            host_id: receipt.host_id.clone(),
            hardware_fingerprint: normalized.host().hardware_fingerprint().into(),
            environment_fingerprint: normalized.host().environment_fingerprint().into(),
            resource_policy_revision: resource.revision,
            qualification_policy_revision: policy.revision,
            manifest_digest: receipt.manifest_digest.clone(),
            recipe_fingerprint: receipt.recipe_fingerprint.clone(),
            descriptor: descriptor.reference(),
            accepted_at_ms: now_ms,
            deadline_ms: receipt.deadline_ms,
            allow_owned_abort_cleanup: receipt.allow_owned_abort_cleanup,
            qualification_runs_permitted: true,
            experimental_controls_permitted: policy.policy.allow_experimental_controls,
            max_cleanup_duration_ms: normalized.limits().max_cleanup_duration_ms(),
        };
        let descriptor_json = encode(&descriptor)?;
        let auth_json = encode(&authorization)?;
        let receipt_json = encode(&receipt)?;
        tx.execute("INSERT INTO deployments(id,name,kind,desired_state,observed_state,admission_enabled,dispatch_enabled,suspended,current_generation,schema_version,revision) VALUES(?1,?2,'model','stopped','stopped',0,0,0,1,1,1)",params![receipt.deployment_id,format!("candidate-{}",receipt.deployment_id)])?;
        tx.execute("INSERT INTO effective_revisions(deployment_id,revision,effective_json,fingerprint) VALUES(?1,1,?2,?3)",params![receipt.deployment_id,descriptor_json,receipt.recipe_fingerprint])?;
        insert_prepared_binding(&tx, session, &prepared).map_err(map_binding)?;
        tx.execute("INSERT INTO operations(id,deployment_id,kind,state) VALUES(?1,?2,'candidate_create','succeeded')",params![receipt.operation_id,receipt.deployment_id])?;
        tx.execute("INSERT INTO qualification_runs(id,host_id,deployment_id,revision,binding_id,incarnation,operation_id,principal_id,recipe_digest,authorization_json,state,deadline_ms) VALUES(?1,?2,?3,1,?4,?5,?6,?7,?8,?9,'accepted',?10)",
            params![receipt.run_id,receipt.host_id,receipt.deployment_id,receipt.binding_id,receipt.incarnation,receipt.operation_id,principal_id,receipt.manifest_digest,auth_json,receipt.deadline_ms])?;
        tx.execute("INSERT INTO command_receipts(principal_id,command_scope,idempotency_key,request_hash,operation_id,response_json) VALUES(?1,?2,?3,?4,?5,?6)",params![principal_id,SCOPE,idempotency_key,hash,receipt.operation_id,receipt_json])?;
        append_event(
            &tx,
            &EventMetadata::CandidateRunAccepted {
                operation_id: EventOperationId::generated(operation_id),
                deployment_id: EventOperationId::generated(deployment_id),
                run_id: EventOperationId::generated(run_id),
                revision: 1,
                generation: 1,
                resource_policy_revision: resource.revision,
                qualification_policy_revision: policy.revision,
                session_epoch: session.epoch(),
            },
        )
        .map_err(|e| match e {
            EventWriteError::Sql(e) => CandidateCreationError::Sql(e),
            _ => CandidateCreationError::InvalidCommand,
        })?;
        let read = read_snapshot(&tx, principal_id, &receipt.run_id)?
            .ok_or(CandidateCreationError::CorruptStoredData)?;
        if read.receipt.inner != receipt {
            return Err(CandidateCreationError::CorruptStoredData);
        }
        #[cfg(test)]
        precommit_hook(&tx, prepared.port());
        tx.commit()?;
        drop(prepared);
        Ok(read.receipt)
    }

    /// Owner-scoped historical information, with no current session or admission check.
    pub fn candidate_run_snapshot(
        &self,
        principal_id: &str,
        run_id: &str,
    ) -> Result<Option<CandidateRunSnapshot>> {
        if !valid_id(principal_id) || !valid_id(run_id) {
            return Err(CandidateCreationError::InvalidCommand);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let snapshot = read_snapshot(&tx, principal_id, run_id)?;
        tx.commit()?;
        Ok(snapshot)
    }
}

struct RunRow {
    host: String,
    deployment: String,
    revision: i64,
    binding: String,
    incarnation: String,
    operation: String,
    digest: String,
    authorization: String,
    state: String,
    deadline: i64,
    requests: i64,
    cleanup: String,
    cleanup_step: Option<String>,
}

fn read_snapshot(
    tx: &Transaction<'_>,
    principal: &str,
    run_id: &str,
) -> Result<Option<CandidateRunSnapshot>> {
    let row = tx.query_row("SELECT host_id,deployment_id,revision,binding_id,incarnation,operation_id,recipe_digest,authorization_json,state,deadline_ms,requests_used,cleanup_state,cleanup_step_id FROM qualification_runs WHERE id=?1 AND principal_id=?2",
        params![run_id,principal], |r|Ok(RunRow { host:r.get(0)?,deployment:r.get(1)?,revision:r.get(2)?,binding:r.get(3)?,incarnation:r.get(4)?,operation:r.get(5)?,digest:r.get(6)?,authorization:r.get(7)?,state:r.get(8)?,deadline:r.get(9)?,requests:r.get(10)?,cleanup:r.get(11)?,cleanup_step:r.get(12)? })).optional()?;
    let Some(row) = row else {
        return Ok(None);
    };
    let receipts: Vec<(String,String,String,String,String)> = tx.prepare("SELECT principal_id,command_scope,idempotency_key,request_hash,response_json FROM command_receipts WHERE operation_id=?1")?
        .query_map([&row.operation],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?.collect::<std::result::Result<_,_>>()?;
    if receipts.len() != 1 {
        return Err(CandidateCreationError::CorruptStoredData);
    }
    let (column_principal, scope, key, hash, json) = &receipts[0];
    let receipt: ReceiptV1 = decode(json)?;
    if column_principal != principal
        || scope != SCOPE
        || !valid_id(key)
        || receipt.version != 1
        || receipt.method != METHOD
        || receipt.target != TARGET
        || receipt.principal_id != principal
        || !valid_id(principal)
        || receipt.request_hash != *hash
        || !digest(hash)
        || receipt.run_id != run_id
        || !ulid(run_id)
        || receipt.operation_id != row.operation
        || !ulid(&row.operation)
        || receipt.deployment_id != row.deployment
        || !ulid(&row.deployment)
        || receipt.binding_id != row.binding
        || !ulid(&row.binding)
        || receipt.incarnation != row.incarnation
        || !ulid(&row.incarnation)
        || receipt.host_id != row.host
        || !valid_id(&row.host)
        || receipt.revision != row.revision
        || receipt.revision != 1
        || receipt.generation != 1
        || receipt.manifest_digest != row.digest
        || !digest(&row.digest)
        || !digest(&receipt.recipe_fingerprint)
        || receipt.resource_policy_revision <= 0
        || receipt.qualification_policy_revision <= 0
        || receipt.accepted_at_ms < 0
        || receipt.deadline_ms != row.deadline
        || receipt.deadline_ms <= receipt.accepted_at_ms
    {
        return Err(CandidateCreationError::CorruptStoredData);
    }
    let operation_matches: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1 AND deployment_id=?2 AND kind='candidate_create' AND state='succeeded' AND idempotency_key IS NULL AND error_code IS NULL)",
        params![row.operation,row.deployment], |r|r.get(0))?;
    if !operation_matches {
        return Err(CandidateCreationError::CorruptStoredData);
    }
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM deployments WHERE id=?1 AND kind='model')",
        [&row.deployment],
        |r| r.get(0),
    )?;
    if !exists {
        return Err(CandidateCreationError::CorruptStoredData);
    }
    let descriptor_row: Option<(String,String)> = tx.query_row("SELECT effective_json,fingerprint FROM effective_revisions WHERE deployment_id=?1 AND revision=?2",
        params![row.deployment,row.revision],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let (descriptor_json, fingerprint) =
        descriptor_row.ok_or(CandidateCreationError::CorruptStoredData)?;
    let descriptor: DescriptorV1 = decode(&descriptor_json)?;
    let manifest = validate_candidate_reviewed_snapshot_text(descriptor.reviewed_manifest.get())
        .map_err(|_| CandidateCreationError::CorruptStoredData)?;
    let auth = &descriptor.credential_refs;
    let profile = manifest.effective_recipe().profile();
    let valid_ref = |r: &Option<String>| {
        r.as_ref().is_none_or(|s| {
            !s.is_empty()
                && s.len() <= 4096
                && serde_json::to_string(s).is_ok_and(|encoded| encoded.len() <= 4096)
        })
    };
    if descriptor.version != 1
        || descriptor.deployment_id != row.deployment
        || descriptor.revision != row.revision
        || descriptor.generation != receipt.generation
        || descriptor.host_id != row.host
        || manifest.host_id() != row.host
        || descriptor.manifest_digest != row.digest
        || manifest.manifest_digest() != row.digest
        || descriptor.recipe_fingerprint != fingerprint
        || fingerprint != receipt.recipe_fingerprint
        || descriptor.total_case_request_budget != manifest.total_case_request_budget()
        || !valid_ref(&auth.runtime)
        || !valid_ref(&auth.admin)
        || (auth.runtime.is_some() && auth.runtime == auth.admin)
        || auth.runtime.is_some() != profile.runtime_auth()
        || auth.admin.is_some() != profile.admin_auth()
    {
        return Err(CandidateCreationError::CorruptStoredData);
    }
    let expected_hash = request_hash(
        &row.host,
        receipt.resource_policy_revision,
        &row.digest,
        &manifest,
        receipt.deadline_ms,
        receipt.allow_owned_abort_cleanup,
    )
    .map_err(|_| CandidateCreationError::CorruptStoredData)?;
    if expected_hash != *hash {
        return Err(CandidateCreationError::CorruptStoredData);
    }
    let authorization: AuthorizationV1 = decode(&row.authorization)?;
    if authorization.version != 1
        || authorization.principal_id != principal
        || authorization.run_id != run_id
        || authorization.operation_id != row.operation
        || authorization.deployment_id != row.deployment
        || authorization.revision != row.revision
        || authorization.generation != receipt.generation
        || authorization.binding_id != row.binding
        || authorization.incarnation != row.incarnation
        || authorization.host_id != row.host
        || authorization.hardware_fingerprint != manifest.host().hardware_fingerprint()
        || authorization.environment_fingerprint != manifest.host().environment_fingerprint()
        || authorization.resource_policy_revision != receipt.resource_policy_revision
        || authorization.qualification_policy_revision != receipt.qualification_policy_revision
        || authorization.manifest_digest != row.digest
        || authorization.recipe_fingerprint != fingerprint
        || authorization.descriptor != descriptor.reference()
        || authorization.accepted_at_ms != receipt.accepted_at_ms
        || authorization.deadline_ms != row.deadline
        || authorization.allow_owned_abort_cleanup != receipt.allow_owned_abort_cleanup
        || !authorization.qualification_runs_permitted
        || (profile.experimental_controls() && !authorization.experimental_controls_permitted)
        || authorization.max_cleanup_duration_ms != manifest.limits().max_cleanup_duration_ms()
        || receipt
            .deadline_ms
            .checked_sub(receipt.accepted_at_ms)
            .is_none_or(|n| n <= 0 || n > manifest.limits().max_run_duration_ms())
    {
        return Err(CandidateCreationError::CorruptStoredData);
    }
    let binding_row: Option<(String,i64,String,String,String,String)> = tx.query_row("SELECT deployment_id,revision,incarnation,ownership,binding_json,state FROM runtime_bindings WHERE id=?1",[&row.binding],
        |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
    let (deployment, revision, incarnation, ownership, binding_json, binding_state) =
        binding_row.ok_or(CandidateCreationError::CorruptStoredData)?;
    let DecodedBinding::Candidate(binding) =
        decode_binding(&binding_json).map_err(|_| CandidateCreationError::CorruptStoredData)?
    else {
        return Err(CandidateCreationError::CorruptStoredData);
    };
    let endpoint: std::net::SocketAddrV4 = binding
        .endpoint
        .parse()
        .map_err(|_| CandidateCreationError::CorruptStoredData)?;
    if deployment != row.deployment
        || revision != row.revision
        || incarnation != row.incarnation
        || ownership != "managed"
        || binding.qualification_id != format!("candidate:{run_id}")
        || binding.descriptor != descriptor.reference()
        || binding.auth != descriptor.credential_refs
        || *endpoint.ip() != std::net::Ipv4Addr::LOCALHOST
        || endpoint.port() == 0
        || endpoint.to_string() != binding.endpoint
        || !matches!(
            binding_state.as_str(),
            "reserved" | "live" | "uncertain" | "released"
        )
    {
        return Err(CandidateCreationError::CorruptStoredData);
    }
    let leases: Vec<(String, u16)> = tx
        .prepare("SELECT host,port FROM endpoint_leases WHERE binding_id=?1")?
        .query_map([&row.binding], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    if (binding_state == "released" && !leases.is_empty())
        || (binding_state != "released" && leases != vec![("127.0.0.1".into(), endpoint.port())])
    {
        return Err(CandidateCreationError::CorruptStoredData);
    }
    let state: CandidateRunState = serde_json::from_value(json!(row.state))
        .map_err(|_| CandidateCreationError::CorruptStoredData)?;
    let cleanup: CandidateCleanupState = serde_json::from_value(json!(row.cleanup))
        .map_err(|_| CandidateCreationError::CorruptStoredData)?;
    let requests =
        u32::try_from(row.requests).map_err(|_| CandidateCreationError::CorruptStoredData)?;
    if requests > manifest.limits().max_requests()
        || (cleanup == CandidateCleanupState::Retained) != row.cleanup_step.is_none()
    {
        return Err(CandidateCreationError::CorruptStoredData);
    }
    if let Some(step) = row.cleanup_step {
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM lifecycle_steps WHERE id=?1 AND deployment_id=?2 AND binding_id=?3)",
            params![step,row.deployment,row.binding],
            |r| r.get(0),
        )?;
        if !exists || binding_state != "released" {
            return Err(CandidateCreationError::CorruptStoredData);
        }
    }
    Ok(Some(CandidateRunSnapshot {
        receipt: CandidateCreationReceipt { inner: receipt },
        reviewed_manifest: manifest,
        credential_refs: descriptor.credential_refs,
        state,
        requests_used: requests,
        cleanup_state: cleanup,
    }))
}

#[cfg(test)]
fn precommit_hook(_tx: &Transaction<'_>, port: u16) {
    assert!(
        std::net::TcpListener::bind(("127.0.0.1", port)).is_err(),
        "prepared listener must survive reread and event insertion"
    );
}

#[cfg(test)]
mod tests;

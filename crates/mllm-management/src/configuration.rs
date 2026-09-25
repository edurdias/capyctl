//! Stopped configuration commands only; no activation or engine callbacks.
use crate::{events::EventSource, AppState, SnapshotSource, SnapshotUnavailable};
use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use mllm_controller::OwnedCoordinatorState;
use mllm_store::{
    events::{EventPage, EventReadError},
    managed_configuration::{
        HostRefusal, HostTarget, ManagedConfigurationError, ManagedConfigurationReceipt,
    },
    snapshot::Snapshot,
};
use serde::Deserialize;
use serde_json::{value::RawValue, Value};
use std::{
    error::Error,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

mod json_shape;
const MAX_BODY: usize = 1 << 20;
const BODY_TIMEOUT: Duration = Duration::from_secs(5);
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(15);

/// Closed public categories; never carries raw SQL, input or provider errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigurationFailure {
    InvalidRequest,
    InvalidConfig,
    /// SPEC §15.3: the configuration was refused for a named reason, the same
    /// one `validate config` reports. The reason names fields and options,
    /// never their values.
    InvalidConfigReason {
        path: String,
        reason: String,
    },
    BodyTooLarge,
    QueueFull,
    DeadlineExceeded,
    Unsupported,
    NotFound,
    RevisionConflict,
    IdempotencyConflict,
    RouteConflict,
    RuntimeRetained,
    /// SPEC §6.3 (W6): `delete deployment` follows verified cleanup; something
    /// is still held, so the deployment must be stopped first.
    DeleteRequiresCleanup,
    ReconciliationRequired,
    HostPolicyDenied,
    CapacityBlocked,
    /// Owner decision 2026-09-25: a capacity block with its reason (which
    /// instance, which host, what it needs and what eviction could free).
    CapacityBlockedBecause(String),
    /// Owner decision 2026-09-25: nothing could be placed because no allowed
    /// host is eligible now; the message names each host and why (for a
    /// drain-only host, both versions). Not a capacity block.
    HostIneligible(String),
    /// Owner decision 2026-09-23: an unmeasured model too large to measure
    /// beside anything else starts only on an empty host (`--evict`).
    StartupRequiresEmptyHost,
    /// Owner decision 2026-09-23: `start --evict` could not make room (a
    /// victim did not drain within the switch drain timeout, or its release
    /// failed). Nothing was started; victims not released serve again.
    SwitchFailed,
    LifecycleConflict,
    /// ADR 0014 §7 (WE3): activation waits for the checkpoint digest.
    CheckpointDigestPending,
    /// ADR 0014 §7: the checkpoint is not the declared or recorded one.
    CheckpointMismatch,
    /// ADR 0008: activation waits for the declared remote model source.
    ModelSourcePending,
    /// ADR 0008: the declared remote model source failed terminally.
    ModelSourceFailed,
    Internal,
}
impl ConfigurationFailure {
    pub(crate) fn response(self) -> Response {
        use ConfigurationFailure::*;
        if let InvalidConfigReason { path, reason } = &self {
            let message = format!("Invalid deployment configuration: {reason}");
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"api_version":"1","error":{
                    "code":"invalid_config","message":message,"retryable":false,
                    "operation_id":null,"details":{"path":path}}})),
            )
                .into_response();
        }
        let detailed = |status: StatusCode, code: &str, message: &str| {
            (
                status,
                Json(serde_json::json!({"api_version":"1","error":{
                    "code":code,"message":message,"retryable":true,
                    "operation_id":null,"details":{}}})),
            )
                .into_response()
        };
        match &self {
            CapacityBlockedBecause(reason) => {
                return detailed(StatusCode::SERVICE_UNAVAILABLE, "capacity_blocked", reason)
            }
            HostIneligible(reason) => {
                let message = if reason.is_empty() {
                    "No allowed host is eligible for placement now; `mllm list hosts` shows each host's state"
                } else {
                    reason.as_str()
                };
                return detailed(StatusCode::SERVICE_UNAVAILABLE, "host_ineligible", message);
            }
            _ => {}
        }
        let (status, code, message, retryable) = match self {
            InvalidConfigReason { .. } | CapacityBlockedBecause(_) | HostIneligible(_) => {
                unreachable!("answered above")
            }
            LifecycleConflict => (
                StatusCode::CONFLICT,
                "lifecycle_conflict",
                "Lifecycle state does not permit this action",
                false,
            ),
            InvalidRequest => (
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Invalid request",
                false,
            ),
            InvalidConfig => (
                StatusCode::BAD_REQUEST,
                "invalid_config",
                "Invalid deployment configuration",
                false,
            ),
            BodyTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "body_too_large",
                "Command body exceeds limit",
                false,
            ),
            QueueFull => (
                StatusCode::TOO_MANY_REQUESTS,
                "queue_full",
                "Command capacity exhausted",
                true,
            ),
            DeadlineExceeded => (
                StatusCode::GATEWAY_TIMEOUT,
                "deadline_exceeded",
                "Command response deadline exceeded; retry with the same idempotency key",
                true,
            ),
            Unsupported => (
                StatusCode::SERVICE_UNAVAILABLE,
                "unsupported_capability",
                "Requested capability is unavailable",
                false,
            ),
            NotFound => (
                StatusCode::NOT_FOUND,
                "not_found",
                "Deployment not found",
                false,
            ),
            RevisionConflict => (
                StatusCode::CONFLICT,
                "revision_conflict",
                "Expected revision does not match",
                false,
            ),
            IdempotencyConflict => (
                StatusCode::CONFLICT,
                "idempotency_conflict",
                "Idempotency key identifies a different command",
                false,
            ),
            RouteConflict => (
                StatusCode::CONFLICT,
                "route_conflict",
                "Deployment name or route is already owned",
                false,
            ),
            RuntimeRetained => (
                StatusCode::CONFLICT,
                "runtime_retained",
                "Runtime ownership requires verified cleanup",
                false,
            ),
            DeleteRequiresCleanup => (
                StatusCode::CONFLICT,
                "delete_requires_cleanup",
                "The deployment still holds a runtime, reservation, lease or unresolved step on some instance; stop the deployment (or use delete deployment --stop) and retry the delete after the stop completes with verified cleanup",
                false,
            ),
            ReconciliationRequired => (
                StatusCode::SERVICE_UNAVAILABLE,
                "reconciliation_required",
                "Current owned state or resource policy is unavailable",
                true,
            ),
            Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Management command failed",
                false,
            ),
            CheckpointDigestPending => (
                StatusCode::SERVICE_UNAVAILABLE,
                "checkpoint_digest_pending",
                "The checkpoint digest is still being measured; retry shortly",
                true,
            ),
            CheckpointMismatch => (
                StatusCode::CONFLICT,
                "checkpoint_mismatch",
                "The checkpoint does not match its declared or recorded digest",
                false,
            ),
            ModelSourcePending => (
                StatusCode::SERVICE_UNAVAILABLE,
                "model_source_pending",
                "The declared model source is still being downloaded and verified on its host; status shows its progress; retry shortly",
                true,
            ),
            ModelSourceFailed => (
                StatusCode::CONFLICT,
                "model_source_failed",
                "The declared model source failed on its host (status shows the reason); fix the source or the host's model_sources policy and deploy a new revision",
                false,
            ),
            HostPolicyDenied => (
                StatusCode::FORBIDDEN,
                "host_policy_denied",
                "Host policy denies this command",
                false,
            ),
            CapacityBlocked => (
                StatusCode::SERVICE_UNAVAILABLE,
                "capacity_blocked",
                // Found live 2026-09-23: the old "Endpoint capacity" wording
                // sent operators after ports when memory was the limit.
                "Capacity is unavailable (memory, device or endpoint ports on the placed host); wait, stop another deployment, or start with --evict",
                true,
            ),
            SwitchFailed => (
                StatusCode::SERVICE_UNAVAILABLE,
                "switch_failed",
                "The eviction did not complete: a victim did not drain within the switch drain timeout or its release failed; nothing was started and victims not released serve again",
                true,
            ),
            StartupRequiresEmptyHost => (
                StatusCode::CONFLICT,
                "startup_requires_empty_host",
                "The model's startup peak is unmeasured and exceeds the host's managed limit, so its first start needs an empty host; start it with --evict to release the other engines there",
                false,
            ),
        };
        (status, Json(serde_json::json!({"api_version":"1","error":{"code":code,"message":message,"retryable":retryable,"operation_id":null,"details":{}}}))).into_response()
    }
}
impl From<ManagedConfigurationError> for ConfigurationFailure {
    fn from(value: ManagedConfigurationError) -> Self {
        use ManagedConfigurationError::*;
        match value {
            Invalid => Self::InvalidConfig,
            Rejected(error) => Self::from(error),
            StaleSession | PolicyConflict => Self::ReconciliationRequired,
            IdempotencyConflict => Self::IdempotencyConflict,
            RevisionConflict => Self::RevisionConflict,
            RouteConflict => Self::RouteConflict,
            RuntimeRetained => Self::RuntimeRetained,
            CorruptStoredData | Sql(_) => Self::Internal,
        }
    }
}

impl From<mllm_config::ConfigError> for ConfigurationFailure {
    fn from(error: mllm_config::ConfigError) -> Self {
        Self::InvalidConfigReason {
            path: error.path.clone(),
            reason: error.to_string(),
        }
    }
}

/// Request data only. No command variant authorizes activation or cleanup.
pub enum ConfigurationCommand {
    Create {
        config_json: String,
    },
    Replace {
        deployment_id: String,
        expected_revision: i64,
        config_json: String,
    },
}
pub trait ConfigurationSource: Send + Sync + 'static {
    /// Persist before returning a receipt. Request cancellation must not cancel
    /// a started call; callers retain the original idempotency key for retries.
    fn accept(
        &self,
        key: &str,
        command: ConfigurationCommand,
    ) -> Result<ManagedConfigurationReceipt, ConfigurationFailure>;

    /// ADR 0014 §7 (WE3): the accepted revision's checkpoint digest state
    /// (`pending` is the `checkpoint_digest_pending` condition), if known.
    fn checkpoint_digest_state(&self, _deployment_id: &str, _revision: i64) -> Option<String> {
        None
    }

    /// SPEC §8.2: the deployment's current effective configuration (by id or
    /// name), raw; the handler redacts it. `Ok(None)` when unknown.
    fn effective_configuration(
        &self,
        _deployment: &str,
    ) -> Result<Option<Value>, ConfigurationFailure> {
        Err(ConfigurationFailure::Unsupported)
    }

    /// SPEC §6.3, ADR 0008: every model-store key (`sources/...`) a deployment
    /// that still exists references. A deleted deployment's copies are not in
    /// it; that is what `mllm prune sources` may reclaim.
    fn referenced_model_sources(&self) -> Result<Vec<String>, ConfigurationFailure> {
        Err(ConfigurationFailure::Unsupported)
    }
}

/// Uses the worker's exact owned Store/session and lifetime process lock. It does
/// not open SQLite, begin sessions, import policy or construct runtime drivers.
pub struct SharedConfigurationSource {
    state: Arc<Mutex<OwnedCoordinatorState>>,
    host: HostSource,
    principal: String,
}
enum HostSource {
    Embedded { document: Value, id: String },
    Registry,
}
impl SharedConfigurationSource {
    pub(crate) fn owned_state(&self) -> &Arc<Mutex<OwnedCoordinatorState>> {
        &self.state
    }
    pub(crate) fn principal(&self) -> &str {
        &self.principal
    }
    pub fn new(
        state: Arc<Mutex<OwnedCoordinatorState>>,
        trusted_host: Value,
        principal: &str,
    ) -> Result<Self, ConfigurationFailure> {
        if !identifier(principal) || trusted_host.to_string().len() > MAX_BODY {
            return Err(ConfigurationFailure::Internal);
        }
        let host_id = trusted_host
            .get("name")
            .and_then(Value::as_str)
            .filter(|id| identifier(id))
            .ok_or(ConfigurationFailure::Internal)?
            .to_owned();
        Ok(Self {
            state,
            host: HostSource::Embedded {
                document: trusted_host,
                id: host_id,
            },
            principal: principal.into(),
        })
    }
    pub fn from_registry(
        state: Arc<Mutex<OwnedCoordinatorState>>,
        principal: &str,
    ) -> Result<Self, ConfigurationFailure> {
        if !identifier(principal) {
            return Err(ConfigurationFailure::Internal);
        }
        Ok(Self {
            state,
            host: HostSource::Registry,
            principal: principal.into(),
        })
    }
}
impl SnapshotSource for SharedConfigurationSource {
    fn snapshot(&self) -> Result<Snapshot, SnapshotUnavailable> {
        self.state
            .lock()
            .map_err(|_| SnapshotUnavailable)?
            .store()
            .snapshot()
            .map_err(|_| SnapshotUnavailable)
    }
}
impl EventSource for SharedConfigurationSource {
    fn events_after(&self, after: Option<&str>, limit: usize) -> Result<EventPage, EventReadError> {
        self.state
            .lock()
            .map_err(|_| EventReadError::InvalidLimit)?
            .store()
            .events_after(after, limit)
    }
}
impl ConfigurationSource for SharedConfigurationSource {
    fn effective_configuration(
        &self,
        deployment: &str,
    ) -> Result<Option<Value>, ConfigurationFailure> {
        let state = self
            .state
            .lock()
            .map_err(|_| ConfigurationFailure::Internal)?;
        let Some(found) = state
            .store()
            .effective_configuration(deployment)
            .map_err(|_| ConfigurationFailure::Internal)?
        else {
            return Ok(None);
        };
        Ok(Some(serde_json::json!({
            "deployment_id": found.deployment_id,
            "revision": found.revision,
            "effective": found.effective,
            "hosts": found.hosts.into_iter().map(|host| serde_json::json!({
                "host_id": host.host_id,
                "outcome": host.outcome,
                "effective": host.effective,
                "diagnostic": host.diagnostic,
            })).collect::<Vec<_>>(),
        })))
    }
    fn referenced_model_sources(&self) -> Result<Vec<String>, ConfigurationFailure> {
        let state = self
            .state
            .lock()
            .map_err(|_| ConfigurationFailure::Internal)?;
        state
            .store()
            .referenced_model_sources()
            .map_err(|_| ConfigurationFailure::Internal)
    }
    fn checkpoint_digest_state(&self, deployment_id: &str, revision: i64) -> Option<String> {
        let state = self.state.lock().ok()?;
        let record = state
            .store()
            .checkpoint_digest(deployment_id, revision)
            .ok()??;
        serde_json::to_value(record.state)
            .ok()?
            .as_str()
            .map(str::to_owned)
    }
    fn accept(
        &self,
        key: &str,
        command: ConfigurationCommand,
    ) -> Result<ManagedConfigurationReceipt, ConfigurationFailure> {
        let state = self
            .state
            .lock()
            .map_err(|_| ConfigurationFailure::Internal)?;
        let store = state.store();
        let now = i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| ConfigurationFailure::Internal)?
                .as_millis(),
        )
        .map_err(|_| ConfigurationFailure::Internal)?;
        let (targets, refusals) = match &self.host {
            HostSource::Embedded { document, id } => {
                let policy = store
                    .resource_policy(id)
                    .map_err(|_| ConfigurationFailure::Internal)?
                    .ok_or(ConfigurationFailure::ReconciliationRequired)?;
                let host = mllm_config::effective::compose_current_resource_controls(
                    document,
                    &policy.context,
                    &policy.controls,
                )
                .map_err(|_| ConfigurationFailure::ReconciliationRequired)?;
                (
                    vec![HostTarget {
                        host_id: id.clone(),
                        host_name: id.clone(),
                        trusted_host: host,
                        scoped: false,
                    }],
                    Vec::new(),
                )
            }
            HostSource::Registry => {
                let (ConfigurationCommand::Create { config_json }
                | ConfigurationCommand::Replace { config_json, .. }) = &command;
                registry_targets(store, config_json)?
            }
        };
        match command {
            ConfigurationCommand::Create { config_json } => store
                .create_managed_configuration_on_hosts(
                    state.session(),
                    &self.principal,
                    key,
                    &format!("{{\"config\":{config_json}}}"),
                    &targets,
                    &refusals,
                    now,
                )
                .map_err(Into::into),
            ConfigurationCommand::Replace {
                deployment_id,
                expected_revision,
                config_json,
            } => {
                if store
                    .get_deployment(&deployment_id)
                    .map_err(|_| ConfigurationFailure::Internal)?
                    .is_none()
                {
                    return Err(ConfigurationFailure::NotFound);
                }
                let replaced = store.replace_managed_configuration_on_hosts(
                    state.session(),
                    &self.principal,
                    key,
                    &deployment_id,
                    &format!(
                        "{{\"config\":{config_json},\"expected_revision\":{expected_revision}}}"
                    ),
                    &targets,
                    &refusals,
                    now,
                );
                // SPEC §6.3 (W6), §14: a deleted deployment keeps only a
                // tombstone, so a new revision of it finds nothing (404). An
                // exact retry was answered above from its receipt, and a key
                // reused for another request stays an idempotency conflict.
                match replaced.map_err(ConfigurationFailure::from) {
                    Err(failure)
                        if failure != ConfigurationFailure::IdempotencyConflict
                            && store.is_deleted(&deployment_id).unwrap_or(false) =>
                    {
                        Err(ConfigurationFailure::NotFound)
                    }
                    other => other,
                }
            }
        }
    }
}

/// ADR 0013 §3: every allowed host a registry deployment is resolved against.
///
/// The allowed set is `placement.hosts` (or the `host` shorthand), else every
/// enrolled host; a host whose published labels do not satisfy the selector
/// is not a candidate. Each allowed host that cannot be resolved against now
/// (not enrolled or published, no runtime profile, no current resource
/// policy, a label mismatch) is returned as a refusal with its closed reason,
/// so the deploy records it and status shows it. A single named host that
/// refuses keeps the answer a single-host deploy always gave.
fn registry_targets(
    store: &mllm_store::Store,
    config_json: &str,
) -> Result<(Vec<HostTarget>, Vec<HostRefusal>), ConfigurationFailure> {
    let config = mllm_config::parse_strict(mllm_config::ConfigKind::Deployment, config_json)?;
    let spec = mllm_config::instances::parse_instance_spec(&config)?;
    let allowed: Vec<String> = match &spec.placement.hosts {
        Some(hosts) => hosts.clone(),
        None => store
            .enrolled_hosts()
            .map_err(|_| ConfigurationFailure::Internal)?
            .into_iter()
            .filter(|host| !host.revoked)
            .map(|host| host.host_id)
            .collect(),
    };
    if allowed.iter().any(|host| !identifier(host)) {
        return Err(ConfigurationFailure::InvalidConfig);
    }
    let mut targets = Vec::new();
    let mut refusals = Vec::new();
    let mut single = None;
    for selector in &allowed {
        let refuse =
            |failure: ConfigurationFailure, reason: &str, refusals: &mut Vec<HostRefusal>| {
                refusals.push(HostRefusal {
                    host_id: selector.clone(),
                    diagnostic: reason.into(),
                });
                failure
            };
        let Some(publication) = store
            .host_publication(selector)
            .map_err(|_| ConfigurationFailure::Internal)?
        else {
            single = Some(refuse(
                ConfigurationFailure::ReconciliationRequired,
                "host_unpublished",
                &mut refusals,
            ));
            continue;
        };
        let original: Value = serde_json::from_str(&publication.config_json)
            .map_err(|_| ConfigurationFailure::Internal)?;
        let labels = mllm_config::instances::host_labels(&original).unwrap_or_default();
        if !spec.placement.selector_matches(&labels) {
            single = Some(refuse(
                ConfigurationFailure::HostPolicyDenied,
                "selector_mismatch",
                &mut refusals,
            ));
            continue;
        }
        if original["runtime_profiles"]
            .as_object()
            .is_none_or(|profiles| profiles.is_empty())
        {
            single = Some(refuse(
                ConfigurationFailure::HostPolicyDenied,
                "no_runtime_profiles",
                &mut refusals,
            ));
            continue;
        }
        let Ok(trusted) =
            mllm_config::remote_resources::scope_host_document(&publication.host_id, &original)
        else {
            single = Some(refuse(
                ConfigurationFailure::HostPolicyDenied,
                "host_policy_denied",
                &mut refusals,
            ));
            continue;
        };
        let Some(policy) = store
            .resource_policy(&publication.host_id)
            .map_err(|_| ConfigurationFailure::Internal)?
        else {
            single = Some(refuse(
                ConfigurationFailure::ReconciliationRequired,
                "resource_policy_unavailable",
                &mut refusals,
            ));
            continue;
        };
        let Ok(host) = mllm_config::effective::compose_current_resource_controls(
            &trusted,
            &policy.context,
            &policy.controls,
        ) else {
            single = Some(refuse(
                ConfigurationFailure::ReconciliationRequired,
                "resource_policy_unavailable",
                &mut refusals,
            ));
            continue;
        };
        let host_name = store
            .enrolled_hosts()
            .map_err(|_| ConfigurationFailure::Internal)?
            .into_iter()
            .find(|enrolled| enrolled.host_id == publication.host_id)
            .map(|enrolled| enrolled.host_name)
            .unwrap_or_else(|| publication.host_id.clone());
        targets.push(HostTarget {
            host_id: publication.host_id,
            host_name,
            trusted_host: host,
            scoped: true,
        });
    }
    if targets.is_empty() {
        return Err(match (allowed.len(), single) {
            (1, Some(failure)) => failure,
            (0, _) => ConfigurationFailure::ReconciliationRequired,
            _ => ConfigurationFailure::HostPolicyDenied,
        });
    }
    Ok((targets, refusals))
}

fn identifier(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}
fn single_header<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    let mut values = request.headers().get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    Some(value)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create<'a> {
    #[serde(borrow)]
    config: &'a RawValue,
    activate: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Replace<'a> {
    #[serde(borrow)]
    config: &'a RawValue,
    expected_revision: i64,
}

/// What replaces a secret in the effective view.
const REDACTED: &str = "[redacted]";

/// Field and option names whose values are secrets or references to them,
/// matched on whole words so `max_total_tokens` or `tokenizer_workers` stay
/// visible while `hf_token`, `api_key` or `credential_ref` do not.
fn secret_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase().replace('-', "_");
    let words: Vec<&str> = name.split('_').filter(|w| !w.is_empty()).collect();
    words.iter().enumerate().any(|(index, word)| match *word {
        "credential" | "credentials" | "secret" | "secrets" | "password" | "passwd" | "token"
        | "apikey" => true,
        "key" => index > 0 && matches!(words[index - 1], "api" | "private" | "admin" | "access"),
        _ => false,
    })
}

/// SPEC §8.2 / §13.3: redact secrets from an effective configuration before it
/// leaves the service: any field whose name marks a secret or credential
/// reference, and the value of any engine argument whose option name does
/// (`--hf-token value`, `--api-key=value`).
pub fn redact_effective(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            for (name, field) in fields.iter_mut() {
                if secret_name(name) && !field.is_null() {
                    *field = Value::String(REDACTED.into());
                } else {
                    redact_effective(field);
                }
            }
        }
        Value::Array(items) => {
            let mut redact_next = false;
            for item in items.iter_mut() {
                if let Value::String(token) = item {
                    if redact_next && !token.starts_with("--") {
                        *token = REDACTED.into();
                        redact_next = false;
                        continue;
                    }
                    redact_next = false;
                    if let Some(option) = token.strip_prefix("--") {
                        match option.split_once('=') {
                            Some((name, _)) if secret_name(name) => {
                                *token = format!("--{name}={REDACTED}");
                            }
                            Some(_) => {}
                            None => redact_next = secret_name(option),
                        }
                    }
                } else {
                    redact_next = false;
                    redact_effective(item);
                }
            }
        }
        _ => {}
    }
}

/// SPEC §6.3, ADR 0008: `GET /management/v1/model-sources`, the store keys
/// of every declared remote source a deployment that still exists references.
pub(crate) async fn model_sources(State(state): State<Arc<AppState>>) -> Response {
    use ConfigurationFailure::*;
    let Some(source) = state.configuration.clone() else {
        return Unsupported.response();
    };
    let Ok(permit) = state.reads.clone().try_acquire_owned() else {
        return QueueFull.response();
    };
    match accept_blocking(permit, move || source.referenced_model_sources()).await {
        Ok(keys) => (
            StatusCode::OK,
            Json(serde_json::json!({"api_version": "1", "referenced": keys})),
        )
            .into_response(),
        Err(error) => error.response(),
    }
}

/// SPEC §8.2: `GET /management/v1/deployments/{id}/effective-config`, the
/// resolved configuration with provenance, secrets redacted.
pub(crate) async fn effective_config(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    use ConfigurationFailure::*;
    let Some(source) = state.configuration.clone() else {
        return Unsupported.response();
    };
    if !identifier(&id) {
        return InvalidRequest.response();
    }
    let Ok(permit) = state.reads.clone().try_acquire_owned() else {
        return QueueFull.response();
    };
    match accept_blocking(permit, move || source.effective_configuration(&id)).await {
        Ok(Some(mut view)) => {
            redact_effective(&mut view);
            view["api_version"] = Value::String("1".into());
            (StatusCode::OK, Json(view)).into_response()
        }
        Ok(None) => NotFound.response(),
        Err(error) => error.response(),
    }
}

pub(crate) async fn accept(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let source = state.configuration.clone();
    match accept_inner(state, request).await {
        Ok(receipt) => {
            let mut body = serde_json::json!({"api_version":"1", "operation_id":receipt.operation_id,"deployment_id":receipt.deployment_id,"revision":receipt.revision.to_string(),"joined":false});
            // ADR 0014 §7 (WE3): the deploy returns with its checkpoint digest
            // state; `pending` until a host holding the checkpoint measures it.
            if let Some(digest) = source.and_then(|source| {
                source.checkpoint_digest_state(&receipt.deployment_id, receipt.revision)
            }) {
                body["checkpoint_digest"] = serde_json::json!(digest);
            }
            (StatusCode::ACCEPTED, Json(body)).into_response()
        }
        Err(error) => error.response(),
    }
}

async fn accept_inner(
    state: Arc<AppState>,
    request: Request,
) -> Result<ManagedConfigurationReceipt, ConfigurationFailure> {
    use ConfigurationFailure::*;
    let target = if request.method() == Method::PUT {
        let id = request
            .uri()
            .path()
            .strip_prefix("/management/v1/deployments/")
            .ok_or(InvalidRequest)?;
        let parsed = id.parse::<ulid::Ulid>().map_err(|_| InvalidRequest)?;
        if parsed.to_string() != id {
            return Err(InvalidRequest);
        }
        Some(id.to_owned())
    } else {
        None
    };
    let (key, body, permit) = read_command(&state, request).await?;
    let command = if let Some(deployment_id) = target {
        let input: Replace<'_> = serde_json::from_slice(&body).map_err(|_| InvalidRequest)?;
        if input.expected_revision < 1 {
            return Err(InvalidRequest);
        }
        ConfigurationCommand::Replace {
            deployment_id,
            expected_revision: input.expected_revision,
            config_json: input.config.get().into(),
        }
    } else {
        let input: Create<'_> = serde_json::from_slice(&body).map_err(|_| InvalidRequest)?;
        if input.activate {
            return Err(Unsupported);
        }
        ConfigurationCommand::Create {
            config_json: input.config.get().into(),
        }
    };
    let source = state.configuration.clone().ok_or(Unsupported)?;
    let receipt = accept_blocking(permit, move || source.accept(&key, command)).await?;
    if receipt.version != 1
        || receipt.revision < 1
        || receipt.generation < 1
        || receipt.resource_policy_revision < 1
        || receipt.accepted_at_ms < 0
        || ![&receipt.operation_id, &receipt.deployment_id]
            .iter()
            .all(|id| {
                id.parse::<ulid::Ulid>()
                    .is_ok_and(|parsed| parsed.to_string() == **id)
            })
    {
        return Err(Internal);
    }
    Ok(receipt)
}

/// Shared command budget, wire validation and body deadline for every mutation.
pub(crate) async fn read_command(
    state: &Arc<AppState>,
    request: Request,
) -> Result<(String, axum::body::Bytes, tokio::sync::OwnedSemaphorePermit), ConfigurationFailure> {
    use ConfigurationFailure::*;
    if request.uri().query().is_some() {
        return Err(InvalidRequest);
    }
    let key = single_header(&request, "idempotency-key")
        .filter(|key| identifier(key))
        .ok_or(InvalidRequest)?
        .to_owned();
    let content_type = single_header(&request, "content-type").ok_or(InvalidRequest)?;
    if !matches!(
        content_type.to_ascii_lowercase().as_str(),
        "application/json" | "application/json; charset=utf-8"
    ) {
        return Err(InvalidRequest);
    }
    if request.headers().contains_key("content-encoding") {
        return Err(InvalidRequest);
    }
    let permit = state
        .commands_in_flight
        .clone()
        .try_acquire_owned()
        .map_err(|_| QueueFull)?;
    let body = tokio::time::timeout(BODY_TIMEOUT, to_bytes(request.into_body(), MAX_BODY))
        .await
        .map_err(|_| DeadlineExceeded)?
        .map_err(|error| {
            if error
                .source()
                .is_some_and(|source| source.is::<http_body_util::LengthLimitError>())
            {
                BodyTooLarge
            } else {
                InvalidRequest
            }
        })?;
    json_shape::validate(&body).map_err(|_| InvalidRequest)?;
    Ok((key, body, permit))
}

pub(crate) async fn accept_blocking<T: Send + 'static>(
    permit: tokio::sync::OwnedSemaphorePermit,
    work: impl FnOnce() -> Result<T, ConfigurationFailure> + Send + 'static,
) -> Result<T, ConfigurationFailure> {
    use ConfigurationFailure::*;
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    });
    // Timeout/disconnect abandons observation, not an already-started transaction.
    tokio::time::timeout(ACCEPT_TIMEOUT, result)
        .await
        .map_err(|_| DeadlineExceeded)?
        .map_err(|_| Internal)?
}

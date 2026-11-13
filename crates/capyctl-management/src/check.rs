//! ADR 0040 §9: strict, pinned deployment previews with no ledger writes.
use crate::{
    configuration::{ConfigurationFailure, SharedConfigurationSource},
    error,
};
use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use capyctl_config::{
    deployment_defaults::{pin_hf, split_hf, HF_SHORTHANDS},
    model_source::ModelSource,
    ConfigError, ConfigKind,
};
use capyctl_protocol::pb;
use capyctl_store::managed_configuration::{HostFit, HostTarget};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

pub type PinFuture =
    Pin<Box<dyn Future<Output = Result<Vec<pb::PinnedSource>, ConfigurationFailure>> + Send>>;

/// ADR 0040 §§9–10: standalone calls its host in-process; the server's
/// implementation carries the same operation over the host protocol.
pub trait SourceProvider: Send + Sync {
    fn pin_and_size(&self, host: &HostTarget, sources: Vec<String>) -> PinFuture;
}
impl<F> SourceProvider for F
where
    F: Fn(&HostTarget, Vec<String>) -> PinFuture + Send + Sync,
{
    fn pin_and_size(&self, host: &HostTarget, sources: Vec<String>) -> PinFuture {
        self(host, sources)
    }
}

/// Until the server's host-admin adapter is installed, never pin on the
/// server machine or use its shell credentials (ADR 0040 §9).
pub struct UnavailableSources;
impl SourceProvider for UnavailableSources {
    fn pin_and_size(&self, _: &HostTarget, _: Vec<String>) -> PinFuture {
        Box::pin(async { Err(ConfigurationFailure::Unsupported) })
    }
}

/// The embedded host's actual pinning/sizing implementation.
pub struct StandaloneSources(pub Arc<capyctl_agent::host_admin::CachedSourcePinner>);
impl SourceProvider for StandaloneSources {
    fn pin_and_size(&self, _: &HostTarget, sources: Vec<String>) -> PinFuture {
        use capyctl_agent::host_admin_remote::SourcePinner;
        let pinner = self.0.clone();
        Box::pin(async move { Ok(pinner.pin_and_size(sources).await) })
    }
}

struct CheckState {
    configuration: Arc<SharedConfigurationSource>,
    sources: Arc<dyn SourceProvider>,
}

pub fn router(
    configuration: Arc<SharedConfigurationSource>,
    sources: Arc<dyn SourceProvider>,
) -> Router {
    Router::new()
        .route("/management/v1/deployments/check", post(check))
        .layer(middleware::from_fn(authenticate))
        .with_state(Arc::new(CheckState {
            configuration,
            sources,
        }))
}

async fn authenticate(request: Request, next: Next) -> Response {
    if !crate::authorized(&request) {
        error(StatusCode::UNAUTHORIZED, "unauthenticated", false)
    } else if request.uri().query().is_some() {
        ConfigurationFailure::InvalidRequest.response()
    } else {
        next.run(request).await
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    yaml: Option<String>,
    config: Option<Value>,
}

fn invalid_config(
    error: ConfigError,
    position: Option<capyctl_config::strict_yaml::Position>,
) -> Response {
    let position = position.map(|p| json!({"line":p.line,"column":p.column}));
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"api_version":"1", "config":null,"yaml":null,
        "errors":[{"message":error.to_string(),"path":error.path,"position":position}],
        "error":{"code":"invalid_config","message":error.to_string(),"retryable":false}})),
    )
        .into_response()
}

fn validate(document: Value) -> Result<Value, ConfigError> {
    let config = capyctl_config::parse_strict_value(ConfigKind::Deployment, document)?;
    capyctl_config::instances::parse_instance_spec(&config)?;
    capyctl_config::effective::validate_declared_timeouts(&config)?;
    capyctl_config::effective::validate_declared_startup(&config)?;
    capyctl_config::effective::validate_declared_resources(&config)?;
    Ok(config)
}

async fn check(State(state): State<Arc<CheckState>>, request: Request) -> Response {
    match run(&state, request).await {
        Ok(response) => response,
        Err(failure) => failure.response(),
    }
}

async fn run(state: &CheckState, request: Request) -> Result<Response, ConfigurationFailure> {
    let bytes = tokio::time::timeout(
        Duration::from_secs(5),
        to_bytes(request.into_body(), 1 << 20),
    )
    .await
    .map_err(|_| ConfigurationFailure::DeadlineExceeded)?
    .map_err(|_| ConfigurationFailure::BodyTooLarge)?;
    let input: Input =
        serde_json::from_slice(&bytes).map_err(|_| ConfigurationFailure::InvalidRequest)?;
    let raw = match (input.yaml, input.config) {
        (Some(text), None) => match capyctl_config::strict_yaml::parse_document_at(&text) {
            Ok(raw) => raw,
            Err((error, position)) => return Ok(invalid_config(error, Some(position))),
        },
        (None, Some(config)) => config,
        _ => return Err(ConfigurationFailure::InvalidRequest),
    };
    // Validate the whole document before contacting a source. Temporary commits
    // are only syntax placeholders: they never leave this read-only preview.
    let mut syntax = raw.clone();
    for at in HF_SHORTHANDS {
        if let Some(hf) = raw.pointer(&format!("{at}/hf")).and_then(Value::as_str) {
            let (repo, _) = split_hf(hf);
            pin_hf(&mut syntax, at, repo, &"0".repeat(40));
        }
    }
    let syntax = match validate(syntax) {
        Ok(config) => config,
        Err(error) => return Ok(invalid_config(error, None)),
    };
    let (targets, refusals) = {
        let owned = state
            .configuration
            .owned_state()
            .lock()
            .map_err(|_| ConfigurationFailure::Internal)?;
        state
            .configuration
            .check_targets(owned.store(), &syntax.to_string())?
    };
    let mut hosts = Vec::new();
    let mut normalized = None;
    for target in targets {
        let mut config = raw.clone();
        let mut sources = Vec::new();
        let mut locations = Vec::new();
        for at in HF_SHORTHANDS {
            let Some(model) = syntax.pointer(at) else {
                continue;
            };
            let source = if let Some(hf) = raw.pointer(&format!("{at}/hf")).and_then(Value::as_str)
            {
                Some(format!("hf:{hf}"))
            } else if let Some(path) = model["path"].as_str() {
                Some(path.to_owned())
            } else {
                match serde_json::from_value::<ModelSource>(model["source"].clone()) {
                    Ok(ModelSource::HuggingFace { repo, revision, .. }) => {
                        Some(format!("hf:{repo}@{revision}"))
                    }
                    Ok(ModelSource::Local { path }) => Some(path),
                    _ => None,
                }
            };
            if let Some(source) = source {
                locations.push(at);
                sources.push(source);
            }
        }
        let answers = state.sources.pin_and_size(&target, sources.clone()).await?;
        if answers.len() != sources.len()
            || answers
                .iter()
                .zip(&sources)
                .any(|(answer, source)| &answer.source != source)
        {
            return Err(ConfigurationFailure::Internal);
        }
        for (at, answer) in locations.iter().zip(&answers) {
            if !answer.error.is_empty() {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    Json(
                        json!({"api_version":"1","errors":[{"message":answer.error}],
                    "error":{"code":"invalid_config","message":answer.error,"retryable":false}}),
                    ),
                )
                    .into_response());
            }
            if raw.pointer(&format!("{at}/hf")).is_some() {
                let (repo, commit) = split_hf(&answer.pinned);
                let Some(commit) = commit else {
                    return Err(ConfigurationFailure::Internal);
                };
                pin_hf(&mut config, at, repo, commit);
            }
        }
        let config = match validate(config) {
            Ok(config) => config,
            Err(error) => return Ok(invalid_config(error, None)),
        };
        let source: ModelSource = if let Some(path) = config["model"]["path"].as_str() {
            ModelSource::Local {
                path: path.to_owned(),
            }
        } else {
            serde_json::from_value(config["model"]["source"].clone())
                .map_err(|_| ConfigurationFailure::InvalidConfig)?
        };
        let (checks, estimated) = {
            let owned = state
                .configuration
                .owned_state()
                .lock()
                .map_err(|_| ConfigurationFailure::Internal)?;
            let store = owned.store();
            let measured = store
                .recorded_weights(&source)
                .map_err(|_| ConfigurationFailure::Internal)?;
            let reported = locations
                .iter()
                .zip(&answers)
                .find(|(at, _)| **at == "/model")
                .and_then(|(_, answer)| answer.weight_bytes)
                .and_then(|bytes| i64::try_from(bytes).ok());
            (
                store
                    .check_deployment_fit(
                        &config,
                        std::slice::from_ref(&target),
                        measured.or(reported),
                    )
                    .map_err(|_| ConfigurationFailure::InvalidConfig)?,
                measured.is_none() && reported.is_some(),
            )
        };
        for check in checks {
            let (verdict, reason, limit) = match check.fit {
                HostFit::Fits => ("fits", None, None),
                HostFit::NeedsRoom => ("needs_room", None, None),
                HostFit::DoesNotFit(limit) => ("does_not_fit", None, Some(limit)),
                HostFit::Unknown(reason) => ("unknown", Some(reason.to_owned()), None),
                HostFit::Refused(reason) => ("unknown", Some(reason), None),
            };
            hosts.push(json!({"host_id":check.host_id,"verdict":verdict,"reason":reason,"limit":limit,"estimated":estimated,"config":config}));
        }
        normalized.get_or_insert(config);
    }
    for refusal in refusals {
        hosts.push(json!({"host_id":refusal.host_id,"verdict":"unknown","reason":refusal.diagnostic,"estimated":false}));
    }
    let config = normalized.unwrap_or(syntax);
    let verdict = hosts
        .iter()
        .find(|h| h["verdict"] == "fits")
        .or_else(|| hosts.first());
    Ok(Json(json!({"api_version":"1","yaml":capyctl_config::yaml_emit::to_block_yaml(&config),"config":config,
        "errors":[],"verdict":verdict.map(|h| &h["verdict"]).unwrap_or(&Value::String("unknown".into())),
        "estimated":verdict.map(|h| &h["estimated"]),"reason":verdict.map(|h| &h["reason"]),"limit":verdict.map(|h| &h["limit"]),"hosts":hosts})).into_response())
}

//! ADR 0030 §2: the public inference endpoints that dispatch to a deployment,
//! and the API each one answers. A deployment answers exactly one of them, the
//! one its `api` names; the other is refused before any activation.

use axum::http::StatusCode;
use axum::Json;
use capyctl_config::effective::DeploymentApi;

use crate::RouterDeps;

/// A public endpoint that dispatches to a deployment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// `POST /v1/chat/completions`, answered by `api: chat` deployments.
    ChatCompletions,
    /// `POST /v1/systemone`, answered by `api: decisions` deployments.
    Decisions,
}

impl Endpoint {
    /// The endpoint's path.
    pub const fn path(self) -> &'static str {
        match self {
            Self::ChatCompletions => "/v1/chat/completions",
            Self::Decisions => "/v1/systemone",
        }
    }

    /// The API a deployment needs to answer this endpoint.
    pub const fn api(self) -> DeploymentApi {
        match self {
            Self::ChatCompletions => DeploymentApi::Chat,
            Self::Decisions => DeploymentApi::Decisions,
        }
    }

    /// The path a deployment serving `api` answers. ADR 0036 §3: an
    /// embeddings deployment answers `POST /v1/embeddings`, which no endpoint
    /// here serves yet, so every request to it is `api_mismatch`.
    pub const fn serving_path(api: DeploymentApi) -> &'static str {
        match api {
            DeploymentApi::Chat => Self::ChatCompletions.path(),
            DeploymentApi::Decisions => Self::Decisions.path(),
            DeploymentApi::Embeddings => "/v1/embeddings",
        }
    }

    /// SPEC §10, ADR 0030 §2: refuse a request whose fields this endpoint can
    /// never forward, before any accounting, activation or engine is touched.
    /// Each endpoint has its own closed contract.
    pub fn validate(
        self,
        body: &serde_json::Value,
    ) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
        match self {
            Self::ChatCompletions => crate::chat::validate_request(body),
            Self::Decisions => capyctl_adapters::forward::validate_decision_request(body)
                .map_err(|refusal| crate::chat::refusal("invalid_request", &refusal.to_string())),
        }
    }
}

/// ADR 0030 §2: a deployment answers only the endpoint its `api` names. The
/// other endpoint is refused with `api_mismatch`, naming the one to use,
/// before the request waits, activates anything or takes a slot. The engine
/// itself may answer both (llama.cpp v0.7.0 still answers chat completions
/// for a decision model), so this refusal is the router's own.
pub(crate) fn check_api(
    deps: &RouterDeps,
    deployment: &str,
    model: &str,
    endpoint: Endpoint,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let api = deps
        .controller
        .deployment_api(deployment)
        .map_err(crate::chat::map_controller)?;
    if api == endpoint.api() {
        return Ok(());
    }
    Err(crate::chat::refusal(
        "api_mismatch",
        &format!(
            "model {model} serves the {} API, which {} does not answer; send its requests to POST {}",
            api.name(),
            endpoint.path(),
            Endpoint::serving_path(api),
        ),
    ))
}

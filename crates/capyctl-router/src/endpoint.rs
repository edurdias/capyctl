//! ADR 0030 §2: the public inference endpoints that dispatch to a deployment,
//! and the API each one answers. A deployment answers exactly one of them, the
//! one its `api` names; the others are refused before any activation (ADR
//! 0036 §3: embeddings is the third).

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
    /// ADR 0036 §3: `POST /v1/embeddings`, answered by `api: embeddings`
    /// deployments.
    Embeddings,
}

impl Endpoint {
    /// The endpoint's path.
    pub const fn path(self) -> &'static str {
        match self {
            Self::ChatCompletions => "/v1/chat/completions",
            Self::Decisions => "/v1/systemone",
            Self::Embeddings => "/v1/embeddings",
        }
    }

    /// The API a deployment needs to answer this endpoint.
    pub const fn api(self) -> DeploymentApi {
        match self {
            Self::ChatCompletions => DeploymentApi::Chat,
            Self::Decisions => DeploymentApi::Decisions,
            Self::Embeddings => DeploymentApi::Embeddings,
        }
    }

    /// The endpoint a deployment serving `api` answers.
    pub const fn serving(api: DeploymentApi) -> Self {
        match api {
            DeploymentApi::Chat => Self::ChatCompletions,
            DeploymentApi::Decisions => Self::Decisions,
            DeploymentApi::Embeddings => Self::Embeddings,
        }
    }

    /// SPEC §10, ADR 0030 §2: refuse a request whose fields this endpoint can
    /// never forward, before any accounting, activation or engine is touched.
    /// Each endpoint has its own closed contract (ADR 0036 §3: OpenAI's for
    /// embeddings).
    pub fn validate(
        self,
        body: &serde_json::Value,
    ) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
        match self {
            Self::ChatCompletions => crate::chat::validate_request(body),
            Self::Decisions => capyctl_adapters::forward::validate_decision_request(body)
                .map_err(|refusal| crate::chat::refusal("invalid_request", &refusal.to_string())),
            Self::Embeddings => capyctl_adapters::embedding_request::validate_embedding_request(
                body,
            )
            .map_err(|refusal| crate::chat::refusal("invalid_request", &refusal.to_string())),
        }
    }
}

/// ADR 0030 §2: a deployment answers only the endpoint its `api` names. The
/// others are refused with `api_mismatch`, naming the one to use,
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
            Endpoint::serving(api).path(),
        ),
    ))
}

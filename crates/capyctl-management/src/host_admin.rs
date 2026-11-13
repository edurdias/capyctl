//! ADR 0040 §§10–11: authenticated host-local operations, injected by the role.
//! Results retain the shared agent module's JSON shapes; this boundary only
//! validates transport inputs and maps refusals into the management envelope.
use axum::{
    body::to_bytes,
    extract::{Path, Query, Request, State},
    http::{header, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc, time::Duration};

/// Transport-independent operations; the source owns all host semantics.
#[derive(Debug, Clone, PartialEq)]
pub enum Operation {
    Detect { paths: Vec<PathBuf> },
    Add(Value),
    Remove { name: String, body: Value },
    Preview(Value),
    Confirm(Value),
    ModelStore,
    Prune(Value),
    Config,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub code: String,
    pub message: String,
}

pub trait HostAdminSource: Send + Sync {
    fn execute(
        &self,
        host: String,
        operation: Operation,
    ) -> Pin<Box<dyn Future<Output = Result<Value, Failure>> + Send + '_>>;
}

/// A server without its remote adapter must never act on its own machine.
pub struct UnavailableHostAdmin;
impl HostAdminSource for UnavailableHostAdmin {
    fn execute(
        &self,
        _: String,
        _: Operation,
    ) -> Pin<Box<dyn Future<Output = Result<Value, Failure>> + Send + '_>> {
        Box::pin(async {
            Err(Failure {
                code: "unsupported_capability".into(),
                message: "Host administration is unavailable on this role".into(),
            })
        })
    }
}

/// ADR 0040 §11: bound the whole request as well as the manifest itself.
pub const MAX_BODY_BYTES: usize = 64 * 1024;

pub fn router(source: Arc<dyn HostAdminSource>) -> Router {
    Router::new()
        .route("/management/v1/hosts/{host}/engines/detected", get(detect))
        .route("/management/v1/hosts/{host}/engines", post(add))
        .route(
            "/management/v1/hosts/{host}/engines/{name}/remove",
            post(remove),
        )
        .route("/management/v1/hosts/{host}/model-store", get(model_store))
        .route("/management/v1/hosts/{host}/model-store/prune", post(prune))
        .route("/management/v1/hosts/{host}/config", get(config))
        .route(
            "/management/v1/hosts/{host}/custom-engines/preview",
            post(preview),
        )
        .route("/management/v1/hosts/{host}/custom-engines", post(confirm))
        .method_not_allowed_fallback(crate::method_denied)
        .layer(middleware::from_fn(crate::authenticate))
        .with_state(source)
}

impl Failure {
    fn invalid() -> Self {
        Self {
            code: "invalid_request".into(),
            message: "Invalid request".into(),
        }
    }

    pub(crate) fn response(self) -> Response {
        let status = match self.code.as_str() {
            "invalid_request" | "invalid_config" | "custom_manifest_invalid" => {
                StatusCode::BAD_REQUEST
            }
            "unsupported_media_type" => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "body_too_large" => StatusCode::PAYLOAD_TOO_LARGE,
            "not_found" | "host_not_found" | "profile_not_found" => StatusCode::NOT_FOUND,
            "unsupported_capability" | "host_upgrade_required" => StatusCode::UNPROCESSABLE_ENTITY,
            "observation_stale" | "agent_unreachable" => StatusCode::SERVICE_UNAVAILABLE,
            "host_admin_busy" | "queue_full" => StatusCode::TOO_MANY_REQUESTS,
            "deadline_exceeded" => StatusCode::GATEWAY_TIMEOUT,
            "internal" => StatusCode::INTERNAL_SERVER_ERROR,
            code if code.starts_with("host_capability_missing:") => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            _ => StatusCode::CONFLICT,
        };
        // ADR 0040 §10: a lost answer is never automatically retried.
        (
            status,
            Json(json!({"api_version":"1","error":{
                "code":self.code,"message":self.message,"retryable":false,
                "operation_id":null,"details":{}
            }})),
        )
            .into_response()
    }
}

async fn execute(source: &dyn HostAdminSource, host: String, operation: Operation) -> Response {
    let status = if matches!(&operation, Operation::Remove { .. }) {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    match source.execute(host, operation).await {
        Ok(value) => (status, Json(value)).into_response(),
        Err(failure) => failure.response(),
    }
}

fn no_query(request: &Request) -> Result<(), Failure> {
    if request.uri().query().is_some() {
        Err(Failure::invalid())
    } else {
        Ok(())
    }
}

async fn empty(request: Request) -> Result<(), Failure> {
    no_query(&request)?;
    let bytes = read_body(request).await?;
    if bytes.is_empty() {
        Ok(())
    } else {
        Err(Failure::invalid())
    }
}

async fn read_body(request: Request) -> Result<axum::body::Bytes, Failure> {
    tokio::time::timeout(
        Duration::from_secs(5),
        to_bytes(request.into_body(), MAX_BODY_BYTES),
    )
    .await
    .map_err(|_| Failure {
        code: "deadline_exceeded".into(),
        message: "Request body deadline exceeded".into(),
    })?
    .map_err(|_| Failure {
        code: "body_too_large".into(),
        message: "Request body exceeds 64 KiB".into(),
    })
}

async fn body(request: Request) -> Result<Value, Failure> {
    no_query(&request)?;
    let json_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
        });
    if !json_type {
        return Err(Failure {
            code: "unsupported_media_type".into(),
            message: "JSON content type required".into(),
        });
    }
    let bytes = read_body(request).await?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| Failure::invalid())?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(Failure::invalid())
    }
}

async fn detect(
    State(source): State<Arc<dyn HostAdminSource>>,
    Path(host): Path<String>,
    request: Request,
) -> Response {
    let query = Query::<Vec<(String, String)>>::try_from_uri(request.uri());
    let Ok(Query(query)) = query else {
        return Failure::invalid().response();
    };
    let mut paths = Vec::with_capacity(query.len());
    for (key, value) in query {
        if key != "path" || value.is_empty() || value.contains('\0') {
            return Failure::invalid().response();
        }
        paths.push(PathBuf::from(value));
    }
    // A detection query is the only query accepted by these routes.
    let bytes = match read_body(request).await {
        Ok(bytes) => bytes,
        Err(failure) => return failure.response(),
    };
    if !bytes.is_empty() {
        return Failure::invalid().response();
    }
    execute(source.as_ref(), host, Operation::Detect { paths }).await
}

async fn model_store(
    State(source): State<Arc<dyn HostAdminSource>>,
    Path(host): Path<String>,
    request: Request,
) -> Response {
    if let Err(failure) = empty(request).await {
        return failure.response();
    }
    execute(source.as_ref(), host, Operation::ModelStore).await
}

async fn config(
    State(source): State<Arc<dyn HostAdminSource>>,
    Path(host): Path<String>,
    request: Request,
) -> Response {
    if let Err(failure) = empty(request).await {
        return failure.response();
    }
    execute(source.as_ref(), host, Operation::Config).await
}

fn fields(value: &Value, allowed: &[&str], required: &[&str]) -> bool {
    value.as_object().is_some_and(|object| {
        object.keys().all(|key| allowed.contains(&key.as_str()))
            && required.iter().all(|key| object.contains_key(*key))
    })
}

fn text(value: &Value, key: &str) -> bool {
    value.get(key).is_none_or(|value| {
        value
            .as_str()
            .is_some_and(|text| !text.is_empty() && !text.contains('\0'))
    })
}

fn strings(value: &Value, key: &str) -> bool {
    value.get(key).is_none_or(|value| {
        value
            .as_array()
            .is_some_and(|values| values.iter().all(Value::is_string))
    })
}

fn settings(value: &Value) -> bool {
    value.get("deep_park").is_none_or(Value::is_boolean)
        && value
            .get("drift")
            .is_none_or(|value| matches!(value.as_str(), Some("warn" | "refuse")))
        && value.get("env").is_none_or(|value| {
            value
                .as_object()
                .is_some_and(|env| env.values().all(Value::is_string))
        })
        && strings(value, "approved_env")
}

async fn add(
    State(source): State<Arc<dyn HostAdminSource>>,
    Path(host): Path<String>,
    request: Request,
) -> Response {
    let value = match body(request).await {
        Ok(value) => value,
        Err(failure) => return failure.response(),
    };
    if !fields(
        &value,
        &[
            "path",
            "name",
            "paths",
            "allow_remote_code",
            "digest",
            "confirm_name",
            "preview",
            "args",
            "env",
            "approved_env",
            "approved_options",
            "approved_paths",
            "deep_park",
            "drift",
        ],
        &["path"],
    ) || !["path", "name", "digest", "confirm_name"]
        .iter()
        .all(|key| text(&value, key))
        || ![
            "paths",
            "args",
            "approved_env",
            "approved_options",
            "approved_paths",
        ]
        .iter()
        .all(|key| strings(&value, key))
        || !["allow_remote_code", "preview"]
            .iter()
            .all(|key| value.get(*key).is_none_or(Value::is_boolean))
        || !settings(&value)
    {
        return Failure::invalid().response();
    }
    execute(source.as_ref(), host, Operation::Add(value)).await
}

async fn remove(
    State(source): State<Arc<dyn HostAdminSource>>,
    Path((host, name)): Path<(String, String)>,
    request: Request,
) -> Response {
    let value = match body(request).await {
        Ok(value) => value,
        Err(failure) => return failure.response(),
    };
    if !fields(&value, &["drain"], &[])
        || value.get("drain").is_some_and(|value| !value.is_boolean())
    {
        return Failure::invalid().response();
    }
    execute(
        source.as_ref(),
        host,
        Operation::Remove { name, body: value },
    )
    .await
}

async fn prune(
    State(source): State<Arc<dyn HostAdminSource>>,
    Path(host): Path<String>,
    request: Request,
) -> Response {
    let value = match body(request).await {
        Ok(value) => value,
        Err(failure) => return failure.response(),
    };
    if !fields(&value, &["apply"], &["apply"]) || !value["apply"].is_boolean() {
        return Failure::invalid().response();
    }
    execute(source.as_ref(), host, Operation::Prune(value)).await
}

fn custom(value: &Value, confirm: bool) -> Result<(), Failure> {
    const PREVIEW: &[&str] = &["operation", "name", "executable", "manifest"];
    const CONFIRM: &[&str] = &[
        "operation",
        "name",
        "executable",
        "manifest",
        "digest",
        "confirm_name",
    ];
    const SETTINGS: &[&str] = &["deep_park", "drift", "env", "approved_env"];
    let required = if confirm { CONFIRM } else { PREVIEW };
    if !value.as_object().is_some_and(|object| {
        object
            .keys()
            .all(|key| required.contains(&key.as_str()) || SETTINGS.contains(&key.as_str()))
    }) || !required
        .iter()
        .all(|key| value.get(*key).is_some() && text(value, key))
        || !matches!(value["operation"].as_str(), Some("add" | "change"))
        || !settings(value)
    {
        return Err(Failure::invalid());
    }
    if value["manifest"].as_str().is_some_and(|manifest| {
        manifest.len() > capyctl_agent::host_admin::MAX_REMOTE_MANIFEST_BYTES
    }) {
        return Err(Failure {
            code: "body_too_large".into(),
            message: "Manifest exceeds 48 KiB".into(),
        });
    }
    Ok(())
}

async fn preview(
    State(source): State<Arc<dyn HostAdminSource>>,
    Path(host): Path<String>,
    request: Request,
) -> Response {
    let value = match body(request).await {
        Ok(value) => value,
        Err(failure) => return failure.response(),
    };
    if let Err(failure) = custom(&value, false) {
        return failure.response();
    }
    execute(source.as_ref(), host, Operation::Preview(value)).await
}

async fn confirm(
    State(source): State<Arc<dyn HostAdminSource>>,
    Path(host): Path<String>,
    request: Request,
) -> Response {
    let value = match body(request).await {
        Ok(value) => value,
        Err(failure) => return failure.response(),
    };
    if let Err(failure) = custom(&value, true) {
        return failure.response();
    }
    execute(source.as_ref(), host, Operation::Confirm(value)).await
}

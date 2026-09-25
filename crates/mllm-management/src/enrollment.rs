//! Administrator-only invitation/revocation API on the existing protected
//! management listener. The same controller-owned authority serves bootstrap.
use crate::{error, ManagementCredentials};
use axum::{
    body::to_bytes,
    extract::{Path, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use mllm_controller::enrollment::{EnrollmentAuthority, EnrollmentRefusal};
use serde::Deserialize;
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;
struct EnrollmentState {
    credentials: ManagementCredentials,
    authority: Arc<EnrollmentAuthority>,
    bootstrap_address: String,
    control_address: String,
    capacity: Arc<Semaphore>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Invite {
    /// A new host's name; with `recover`, the revoked host's name or id.
    host_name: String,
    lifetime_seconds: i64,
    /// ADR 0016: re-enroll a revoked host under its same identity.
    #[serde(default)]
    recover: bool,
}
/// Mount only on the authenticated loopback/TLS management listener. Bootstrap
/// address is trusted server configuration, never request-controlled redirect data.
pub fn enrollment_router(
    credentials: ManagementCredentials,
    authority: Arc<EnrollmentAuthority>,
    bootstrap_address: String,
    control_address: String,
) -> Result<Router, &'static str> {
    validate_address(&control_address)?;
    validate_address(&bootstrap_address)?;
    let state = Arc::new(EnrollmentState {
        credentials,
        authority,
        bootstrap_address,
        control_address,
        capacity: Arc::new(Semaphore::new(2)),
    });
    Ok(Router::new()
        .route("/management/v1/host-invitations", post(invite))
        .route("/management/v1/hosts/{id}/revoke", post(revoke))
        .fallback(crate::not_found)
        .method_not_allowed_fallback(crate::method_denied)
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .with_state(state))
}
fn validate_address(bootstrap_address: &str) -> Result<(), &'static str> {
    let uri = bootstrap_address
        .parse::<axum::http::Uri>()
        .map_err(|_| "invalid bootstrap address")?;
    if uri.scheme_str() != Some("https")
        || uri.host().is_none()
        || uri
            .path_and_query()
            .is_some_and(|path| path.as_str() != "/")
        || bootstrap_address.len() > 2048
        || bootstrap_address.chars().any(char::is_control)
        || bootstrap_address.contains('@')
    {
        return Err("invalid bootstrap address");
    }
    Ok(())
}
async fn authenticate(
    State(state): State<Arc<EnrollmentState>>,
    request: Request,
    next: Next,
) -> Response {
    let mut response = if !state.credentials.accepts(&request) {
        error(StatusCode::UNAUTHORIZED, "unauthenticated", false)
    } else if request.uri().query().is_some() {
        error(StatusCode::BAD_REQUEST, "invalid_request", false)
    } else {
        next.run(request).await
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response
}
async fn invite(State(state): State<Arc<EnrollmentState>>, request: Request) -> Response {
    let Ok(permit) = state.capacity.clone().try_acquire_owned() else {
        return error(StatusCode::TOO_MANY_REQUESTS, "queue_full", true);
    };
    if request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        != Some("application/json")
    {
        return error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            false,
        );
    }
    let bytes =
        match tokio::time::timeout(Duration::from_secs(5), to_bytes(request.into_body(), 4096))
            .await
        {
            Ok(Ok(bytes)) => bytes,
            _ => return error(StatusCode::BAD_REQUEST, "invalid_request", false),
        };
    let input: Invite = match serde_json::from_slice(&bytes) {
        Ok(input) => input,
        Err(_) => return error(StatusCode::BAD_REQUEST, "invalid_request", false),
    };
    let address = state.bootstrap_address.clone();
    let control_address = state.control_address.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if input.recover {
            state
                .authority
                .invite_recovery(&input.host_name, input.lifetime_seconds, now())
        } else {
            state
                .authority
                .invite(&input.host_name, input.lifetime_seconds, now())
        }
    })
    .await;
    match result {
        Ok(Ok(invitation)) => {
            let mut body = serde_json::json!({"version":1,"server_address":address,"control_address":control_address,"server_ca":invitation.ca_pem,"invitation_id":invitation.id,"invitation_secret":invitation.secret,"host_name":invitation.host_name,"expires_unix":invitation.expires_unix});
            // ADR 0016: only a recovery invitation names the host id it
            // re-enrolls; an ordinary one keeps its exact earlier shape.
            if let Some(host_id) = invitation.recover_host_id {
                body["recover_host_id"] = host_id.into();
            }
            (
                StatusCode::CREATED,
                [(header::CACHE_CONTROL, "no-store")],
                Json(body),
            )
                .into_response()
        }
        Ok(Err(refusal)) => refused(refusal),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "internal", false),
    }
}
async fn revoke(
    State(state): State<Arc<EnrollmentState>>,
    path: Result<Path<String>, axum::extract::rejection::PathRejection>,
    request: Request,
) -> Response {
    let Ok(permit) = state.capacity.clone().try_acquire_owned() else {
        return error(StatusCode::TOO_MANY_REQUESTS, "queue_full", true);
    };
    let id = match path {
        Ok(Path(id)) => id,
        Err(_) => return error(StatusCode::BAD_REQUEST, "invalid_request", false),
    };
    match tokio::time::timeout(Duration::from_secs(5), to_bytes(request.into_body(), 1)).await {
        Ok(Ok(body)) if body.is_empty() => {}
        _ => return error(StatusCode::BAD_REQUEST, "invalid_request", false),
    }
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        state.authority.revoke(&id)
    })
    .await
    {
        // SPEC §§4.1, 6.4: the revoked identity, and whether this request
        // revoked it or found it already revoked (an idempotent retry).
        Ok(Ok(revocation)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "api_version": "1",
                "host_id": revocation.host_id,
                "name": revocation.host_name,
                "revoked": true,
                "newly_revoked": revocation.newly_revoked,
            })),
        )
            .into_response(),
        Ok(Err(refusal)) => refused(refusal),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "internal", false),
    }
}
/// SPEC §14: structured, typed errors. The request's own fault is a 400, a host
/// that does not exist a 404, an enrolled name a 409, and only a failure of the
/// authority itself a 500.
fn refused(refusal: EnrollmentRefusal) -> Response {
    match refusal {
        EnrollmentRefusal::Invalid => error(StatusCode::BAD_REQUEST, "invalid_request", false),
        EnrollmentRefusal::NotFound => error(StatusCode::NOT_FOUND, "not_found", false),
        EnrollmentRefusal::Conflict => error(StatusCode::CONFLICT, "identity_conflict", false),
        // ADR 0016: only a revoked host is recovered.
        EnrollmentRefusal::NotRevoked => error(StatusCode::CONFLICT, "host_not_revoked", false),
        EnrollmentRefusal::Internal => error(StatusCode::INTERNAL_SERVER_ERROR, "internal", false),
    }
}
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.as_secs() as i64)
        .unwrap_or(-1)
}

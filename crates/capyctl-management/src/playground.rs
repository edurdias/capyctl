//! ADR 0040 §12 / KTD13: browser-authorized requests use the role's keyless,
//! admission-gated inference service, never its inference credential.
use crate::ui_sessions::Principal;
use axum::{
    body::Body,
    extract::{Extension, Request, State},
    http::{header, Method, StatusCode},
    response::Response,
    routing::post,
    Router,
};
use futures::StreamExt;
use tower::ServiceExt;

/// Mount only through `compose`: it supplies the principal. The role supplies
/// the same shutdown gate and router dependencies as its inference listener.
pub fn router(inference: Router) -> Router {
    Router::new()
        .route("/management/v1/playground/chat/completions", post(chat))
        .route("/management/v1/playground/systemone", post(decision))
        .with_state(inference)
}

async fn chat(
    State(inference): State<Router>,
    Extension(principal): Extension<Principal>,
    request: Request,
) -> Response {
    forward(inference, principal, request, "/v1/chat/completions").await
}

async fn decision(
    State(inference): State<Router>,
    Extension(principal): Extension<Principal>,
    request: Request,
) -> Response {
    forward(inference, principal, request, "/v1/systemone").await
}

async fn forward(
    inference: Router,
    principal: Principal,
    request: Request,
    path: &'static str,
) -> Response {
    let (parts, body) = request.into_parts();
    // ADR 0040 §12: construct a new request, not a clone of the browser's.
    // Let the router read and bound the body itself; do not buffer or log it.
    let mut forwarded = Request::new(body);
    *forwarded.method_mut() = Method::POST;
    *forwarded.uri_mut() = path.parse().unwrap();
    for name in [header::CONTENT_TYPE.as_str(), "x-request-id"] {
        for value in parts.headers.get_all(name) {
            forwarded.headers_mut().append(name, value.clone());
        }
    }
    // KTD7: revocation also cancels a queued or waking request, before headers.
    let mut revoked = Box::pin(async move { principal.revoked().await });
    let response = tokio::select! {
        biased;
        _ = &mut revoked => return crate::error(StatusCode::UNAUTHORIZED, "unauthenticated", false),
        response = inference.oneshot(forwarded) => match response {
            Ok(response) => response,
            Err(never) => match never {},
        },
    };
    let (parts, body) = response.into_parts();
    // KTD7: no producer task or channel survives the browser. Dropping the
    // response drops the router body; revocation drops it on the next poll.
    let stream = futures::stream::unfold(
        (body.into_data_stream(), revoked),
        |(mut body, mut revoked)| async move {
            tokio::select! {
                biased;
                _ = &mut revoked => None,
                chunk = body.next() => chunk.map(|chunk| (chunk, (body, revoked))),
            }
        },
    );
    Response::from_parts(parts, Body::from_stream(stream))
}

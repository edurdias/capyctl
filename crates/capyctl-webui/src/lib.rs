//! ADR 0040 §§13–14: embedded UI assets, separate from management semantics.
use axum::{
    body::Body,
    extract::{Path, Request},
    http::{header, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Router,
};
use std::{borrow::Cow, path::Path as FilePath};

/// A bundle rooted at `web/dist`, replaceable with fixed assets in tests.
pub trait Assets: Send + Sync + 'static {
    fn get(path: &str) -> Option<Cow<'static, [u8]>>;
}

#[derive(rust_embed::RustEmbed)]
#[folder = "../../web/dist/"]
#[allow_missing = true]
struct Embedded;

impl Assets for Embedded {
    fn get(path: &str) -> Option<Cow<'static, [u8]>> {
        <Self as rust_embed::RustEmbed>::get(path).map(|file| file.data)
    }
}

/// The role's bundled UI; a build without assets explains that to the operator.
pub fn router() -> Router {
    router_with_assets::<Embedded>()
}

/// KTD17: explicit routes only, so this merges with either role's fallback.
pub fn router_with_assets<A: Assets>() -> Router {
    Router::new()
        .route("/", get(|| async { Redirect::permanent("/ui/") }))
        .route("/ui", get(|| async { Redirect::permanent("/ui/") }))
        .route("/ui/", get(|| async { asset::<A>("index.html") }))
        .route("/ui/{*path}", get(path::<A>))
        .layer(middleware::from_fn(headers))
}

async fn path<A: Assets>(Path(path): Path<String>) -> Response {
    // ADR 0040 §13: never resolve parent components, including decoded ones.
    if path.split('/').any(|part| part == ".." || part == ".") || path.contains(['\\', '\0']) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let name = if FilePath::new(&path).extension().is_none() {
        "index.html"
    } else {
        &path
    };
    asset::<A>(name)
}

const NO_UI: &str = "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><title>CapyCTL</title><h1>This build has no web UI</h1><p>Install a CapyCTL release with the web UI included, or use the command line.</p></html>";

fn asset<A: Assets>(name: &str) -> Response {
    let index = A::get("index.html");
    let Some(index) = index else {
        return (
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            NO_UI,
        )
            .into_response();
    };
    let data = if name == "index.html" {
        Some(index)
    } else {
        A::get(name)
    };
    let Some(data) = data else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let body = match data {
        Cow::Borrowed(bytes) => Body::from(bytes),
        Cow::Owned(bytes) => Body::from(bytes),
    };
    let mut response = Response::new(body);
    let mime = mime_guess::from_path(name).first_or_octet_stream();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime.as_ref()).unwrap(),
    );
    // ADR 0040 §14 / KTD18: Vite's content-hashed assets are immutable.
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if name.starts_with("assets/") {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        }),
    );
    response
}

async fn headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    // ADR 0040 §14 / KTD18: applies to redirects, missing assets and errors too.
    for (name, value) in [
        (header::CONTENT_SECURITY_POLICY, "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "same-origin"),
    ] {
        response.headers_mut().insert(name, HeaderValue::from_static(value));
    }
    response
}

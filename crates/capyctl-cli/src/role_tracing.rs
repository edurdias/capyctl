//! ADR 0037 §§1, 8, 12, 13: a role's tracing, built from its validated
//! settings only (never from `OTEL_*`), the start warning for plain HTTP off
//! loopback, and the status line `capyctl status` shows per role.
use capyctl_config::tracing::{CallerContext, TracingExport};
use capyctl_trace::{CallerMode, Service, Settings, Tracing};
use serde_json::{json, Value};

/// ADR 0037 §1: off unless the role names an endpoint. With one, the
/// `headers_file` is read (owner-only, SPEC §13.3) and the exporter's one
/// task starts. The error names the setting, never a header value.
pub fn start(
    export: Option<&TracingExport>,
    sample_ratio: f64,
    caller_context: CallerContext,
    service: Service,
) -> Result<Tracing, String> {
    let Some(export) = export else {
        return Ok(Tracing::off());
    };
    let headers = export.headers().map_err(|error| error.to_string())?;
    // ADR 0037 §8: said out loud at start, like `authentication: none` on a
    // network bind (ADR 0019).
    if let Some(warning) = plain_http_warning(export) {
        eprintln!("{warning}");
    }
    Tracing::start(Settings {
        endpoint: export.endpoint.clone(),
        headers,
        sample_ratio,
        caller_mode: caller_mode(caller_context),
        service,
    })
    .map_err(|error| error.to_string())
}

/// ADR 0037 §8: the start warning for an `http://` endpoint off loopback.
pub fn plain_http_warning(export: &TracingExport) -> Option<String> {
    export.plain_http_off_loopback().then(|| {
        format!(
            "warning: tracing exports over plain HTTP to {} (not loopback); spans travel unencrypted",
            export.endpoint
        )
    })
}

fn caller_mode(context: CallerContext) -> CallerMode {
    match context {
        CallerContext::Parent => CallerMode::Parent,
        CallerContext::Link => CallerMode::Link,
        CallerContext::Ignore => CallerMode::Ignore,
    }
}

/// ADR 0037 §13: the role's tracing as status reads it:
/// `{on, endpoint, spans_dropped, last_export_error}`; off is `{on: false}`.
pub fn status(tracing: &Tracing) -> Value {
    match (tracing.endpoint(), tracing.stats()) {
        (Some(endpoint), Some(stats)) => json!({
            "on": true,
            "endpoint": endpoint,
            "spans_dropped": stats.dropped,
            "last_export_error": stats.last_export_error,
        }),
        _ => json!({"on": false}),
    }
}

/// ADR 0037 §13: the per-role status line, `tracing: off` or
/// `tracing: on → <endpoint>` with spans dropped and the last export error
/// since the start. A view without the field (an older server) gives `None`.
pub fn status_line(view: &Value) -> Option<String> {
    let tracing = view.get("tracing").filter(|t| t.is_object())?;
    if tracing["on"].as_bool() != Some(true) {
        return Some("tracing: off".to_string());
    }
    let mut line = format!(
        "tracing: on → {}, spans dropped {}",
        tracing["endpoint"].as_str().unwrap_or("?"),
        tracing["spans_dropped"].as_u64().unwrap_or(0)
    );
    if let Some(error) = tracing["last_export_error"].as_str() {
        line.push_str(&format!(", last export error: {error}"));
    }
    Some(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn export(endpoint: &str) -> TracingExport {
        TracingExport {
            endpoint: endpoint.to_string(),
            headers_file: None,
        }
    }

    // T48 (ADR 0037 §1): no endpoint, no tracing.
    #[tokio::test]
    async fn no_endpoint_is_off() {
        let tracing = start(None, 1.0, CallerContext::Parent, Service::Server).unwrap();
        assert!(!tracing.is_on());
        assert_eq!(status(&tracing), json!({"on": false}));
    }

    // T48 (ADR 0037 §8): plain HTTP off loopback warns; loopback and HTTPS do not.
    #[test]
    fn plain_http_off_loopback_warns() {
        assert!(
            plain_http_warning(&export("http://10.0.0.5:4318/v1/traces"))
                .is_some_and(|w| w.contains("http://10.0.0.5:4318/v1/traces"))
        );
        assert_eq!(
            plain_http_warning(&export("http://127.0.0.1:4318/v1/traces")),
            None
        );
        assert_eq!(
            plain_http_warning(&export("https://collector.example:4318/v1/traces")),
            None
        );
    }

    // T48 T17 (ADR 0037 §8): a headers_file that cannot be read refuses the
    // start, naming the path and never a value.
    #[tokio::test]
    async fn a_missing_headers_file_refuses() {
        let export = TracingExport {
            endpoint: "http://127.0.0.1:4318/v1/traces".to_string(),
            headers_file: Some(PathBuf::from("/nonexistent/capyctl-otlp-headers")),
        };
        let error = start(Some(&export), 1.0, CallerContext::Parent, Service::Server).unwrap_err();
        assert!(
            error.contains("headers_file") || error.contains("/nonexistent"),
            "{error}"
        );
    }

    // T48 (ADR 0037 §13): the status line for off, on, and on with an error.
    #[test]
    fn status_line_follows_the_view() {
        assert_eq!(status_line(&json!({"id": "d"})), None);
        assert_eq!(
            status_line(&json!({"tracing": {"on": false}})).as_deref(),
            Some("tracing: off")
        );
        assert_eq!(
            status_line(
                &json!({"tracing": {"on": true, "endpoint": "http://127.0.0.1:4318/v1/traces",
                "spans_dropped": 3, "last_export_error": null}})
            )
            .as_deref(),
            Some("tracing: on → http://127.0.0.1:4318/v1/traces, spans dropped 3")
        );
        assert_eq!(
            status_line(&json!({"tracing": {"on": true, "endpoint": "https://c:4318/v1/traces",
                "spans_dropped": 0, "last_export_error": "HTTP 503"}}))
            .as_deref(),
            Some("tracing: on → https://c:4318/v1/traces, spans dropped 0, last export error: HTTP 503")
        );
    }
}

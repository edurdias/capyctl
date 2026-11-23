//! ADR 0040 §§2, 10–11: authenticated host-admin HTTP contracts. The fake
//! provider exercises routing and validation, not engine qualification.
mod support;

use axum::{
    body::{to_bytes, Body},
    http::Request,
    Router,
};
use capyctl_management::{
    host_admin::{self, Failure, HostAdminSource, Operation, UnavailableHostAdmin},
    ManagementCredentials,
};
use serde_json::{json, Value};
use std::{
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
};
use tower::ServiceExt;

const ADMIN: &str = "management-credential-012345678901234567890";
const INFERENCE: &str = "inference-credential-0123456789012345678901";
const HOST: &str = "/management/v1/hosts/lab%20east";

struct Fake {
    calls: Mutex<Vec<(String, Operation)>>,
    reply: Result<Value, Failure>,
}

impl Fake {
    fn new(reply: Result<Value, Failure>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            reply,
        })
    }
}

impl HostAdminSource for Fake {
    fn execute(
        &self,
        host: String,
        operation: Operation,
    ) -> Pin<Box<dyn Future<Output = Result<Value, Failure>> + Send + '_>> {
        self.calls.lock().unwrap().push((host, operation));
        let reply = self.reply.clone();
        Box::pin(async move { reply })
    }
}

fn router(source: Arc<dyn HostAdminSource>) -> Router {
    support::composed(
        ManagementCredentials::from_trusted_resolver(ADMIN, INFERENCE).unwrap(),
        host_admin::router(source),
    )
}

fn request(method: &str, uri: &str, body: Option<&str>, credential: Option<&str>) -> Request<Body> {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(credential) = credential {
        request = request.header("authorization", format!("Bearer {credential}"));
    }
    match body {
        Some(body) => request
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap(),
        None => request.body(Body::empty()).unwrap(),
    }
}

async fn response(router: &Router, request: Request<Body>) -> (u16, Value) {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    assert_eq!(response.headers()["cache-control"], "no-store");
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

fn custom() -> Value {
    json!({
        "operation": "add",
        "name": "custom",
        "executable": "/opt/custom/bin/server",
        "manifest": "version: 1\ncommand: ['{executable}', '--port', '{port}']\n"
    })
}

// T32: the route forwards the selected host, decoded profile name, and the
// original body, and preserves each operation's response (ADR 0040 §10).
#[tokio::test]
async fn every_operation_forwards_its_request_and_response() {
    let add = json!({"path": "/opt/vllm", "name": "local"});
    let remove = json!({"drain": true});
    let preview = custom();
    let mut confirm = preview.clone();
    confirm["digest"] = json!("sha256:preview-digest");
    confirm["confirm_name"] = json!("custom");
    let prune = json!({"apply": true});
    let cases = vec![
        (
            "GET",
            "/engines/detected?path=%2Fopt%2Ffirst+engine&path=%2Fopt%2Fsecond%2Bengine",
            None,
            Operation::Detect {
                paths: vec![
                    PathBuf::from("/opt/first engine"),
                    PathBuf::from("/opt/second+engine"),
                ],
            },
            200,
            json!({"candidates": [{"engine": "vllm", "version": "0.33.0", "custom": false, "entry": "/opt/vllm", "source": "path"}]}),
        ),
        (
            "POST",
            "/engines",
            Some(add.clone()),
            Operation::Add(add),
            200,
            json!({"profile": "local", "published": true}),
        ),
        (
            "POST",
            "/engines/custom%20one/remove",
            Some(remove.clone()),
            Operation::Remove {
                name: "custom one".into(),
                body: remove,
            },
            202,
            json!({"profile": "custom one", "retiring": true}),
        ),
        (
            "POST",
            "/custom-engines/preview",
            Some(preview.clone()),
            Operation::Preview(preview),
            200,
            json!({"command": ["/opt/custom/bin/server", "--port", "9000"], "summary": "Approve custom", "digest": "sha256:preview-digest"}),
        ),
        (
            "POST",
            "/custom-engines",
            Some(confirm.clone()),
            Operation::Confirm(confirm),
            200,
            json!({"profile": "custom", "pinned": "sha256:installation"}),
        ),
        (
            "GET",
            "/model-store",
            None,
            Operation::ModelStore,
            200,
            json!({"copies": [{"key": "used", "bytes": 42, "referenced": true}, {"key": "unused", "bytes": 12, "referenced": false}]}),
        ),
        (
            "POST",
            "/model-store/prune",
            Some(prune.clone()),
            Operation::Prune(prune),
            200,
            json!({"removed": [{"key": "unused", "bytes": 12}], "removed_bytes": 12}),
        ),
        (
            "GET",
            "/config",
            None,
            Operation::Config,
            200,
            json!({"schema_version": 1, "host": {"name": "lab east"}}),
        ),
    ];
    for (method, suffix, body, operation, expected_status, reply) in cases {
        let source = Fake::new(Ok(reply.clone()));
        let router = router(source.clone());
        let body = body.map(|value| value.to_string());
        let actual = response(
            &router,
            request(
                method,
                &format!("{HOST}{suffix}"),
                body.as_deref(),
                Some(ADMIN),
            ),
        )
        .await;
        assert_eq!(actual, (expected_status, reply), "{method} {suffix}");
        assert_eq!(
            *source.calls.lock().unwrap(),
            vec![("lab east".into(), operation)],
            "{method} {suffix}"
        );
    }
}

// T32: omission of extra detect directories and the optional add/remove
// fields retain the provider's defaults rather than inventing HTTP defaults.
#[tokio::test]
async fn optional_fields_are_not_invented() {
    let source = Fake::new(Ok(json!({})));
    let router = router(source.clone());
    for (method, suffix, body) in [
        ("GET", "/engines/detected", None),
        ("POST", "/engines", Some(r#"{"path":"/opt/vllm"}"#)),
        ("POST", "/engines/local/remove", Some("{}")),
        ("POST", "/model-store/prune", Some(r#"{"apply":false}"#)),
    ] {
        let (status, value) = response(
            &router,
            request(method, &format!("{HOST}{suffix}"), body, Some(ADMIN)),
        )
        .await;
        assert!(status == 200 || status == 202, "{suffix}: {value}");
    }
    assert_eq!(
        *source.calls.lock().unwrap(),
        vec![
            ("lab east".into(), Operation::Detect { paths: Vec::new() }),
            (
                "lab east".into(),
                Operation::Add(json!({"path": "/opt/vllm"}))
            ),
            (
                "lab east".into(),
                Operation::Remove {
                    name: "local".into(),
                    body: json!({})
                }
            ),
            ("lab east".into(), Operation::Prune(json!({"apply": false}))),
        ]
    );
}

// T32: malformed requests fail before invoking any host operation.
#[tokio::test]
async fn invalid_queries_and_bodies_do_not_reach_the_host() {
    let source = Fake::new(Ok(json!({})));
    let router = router(source.clone());
    let cases = [
        ("GET", "/engines/detected?unknown=directory", None),
        ("GET", "/engines/detected?path=", None),
        ("GET", "/config?path=%2Fopt", None),
        ("GET", "/model-store?apply=true", None),
        (
            "POST",
            "/engines?path=%2Fopt",
            Some(r#"{"path":"/opt/vllm"}"#),
        ),
        ("POST", "/engines/local/remove?drain=true", Some("{}")),
        ("POST", "/model-store/prune?apply=true", Some("{}")),
        ("POST", "/custom-engines/preview?operation=add", Some("{}")),
        ("POST", "/custom-engines?operation=add", Some("{}")),
        ("POST", "/engines", Some("")),
        ("POST", "/engines", Some("{")),
        ("POST", "/engines", Some("[]")),
        ("POST", "/engines", Some("null")),
        ("POST", "/engines", Some("{}")),
        ("POST", "/engines", Some(r#"{"path":1}"#)),
        ("POST", "/engines", Some(r#"{"path":""}"#)),
        (
            "POST",
            "/engines",
            Some(r#"{"path":"/opt/vllm","name":false}"#),
        ),
        (
            "POST",
            "/engines",
            Some(r#"{"path":"/opt/vllm","unknown":true}"#),
        ),
        ("POST", "/engines/local/remove", Some(r#"{"drain":"yes"}"#)),
        ("POST", "/engines/local/remove", Some(r#"{"unknown":true}"#)),
        ("POST", "/model-store/prune", Some("{}")),
        ("POST", "/model-store/prune", Some(r#"{"apply":"yes"}"#)),
        (
            "POST",
            "/model-store/prune",
            Some(r#"{"apply":true,"unknown":true}"#),
        ),
        ("POST", "/custom-engines/preview", Some("{}")),
        ("POST", "/custom-engines", Some("{}")),
    ];
    for (method, suffix, body) in cases {
        let (status, value) = response(
            &router,
            request(method, &format!("{HOST}{suffix}"), body, Some(ADMIN)),
        )
        .await;
        assert_eq!(status, 400, "{method} {suffix} {body:?}: {value}");
        assert_eq!(value["api_version"], "1");
        assert_eq!(value["error"]["code"], "invalid_request", "{value}");
    }
    assert!(source.calls.lock().unwrap().is_empty());
}

// T32: preview and confirm accept manifest content, not a manifest filename;
// the HTTP layer checks their required fields before calling the provider.
#[tokio::test]
async fn custom_requests_validate_fields_before_dispatch() {
    let source = Fake::new(Ok(json!({})));
    let router = router(source.clone());
    let mut confirm = custom();
    confirm["digest"] = json!("sha256:preview-digest");
    confirm["confirm_name"] = json!("custom");
    for (suffix, original, fields) in [
        (
            "/custom-engines/preview",
            custom(),
            vec!["operation", "name", "executable", "manifest"],
        ),
        (
            "/custom-engines",
            confirm,
            vec![
                "operation",
                "name",
                "executable",
                "manifest",
                "digest",
                "confirm_name",
            ],
        ),
    ] {
        for field in fields {
            for invalid in [None, Some(json!(42)), Some(json!(""))] {
                let mut body = original.clone();
                match invalid {
                    Some(value) => body[field] = value,
                    None => {
                        body.as_object_mut().unwrap().remove(field);
                    }
                }
                let (status, value) = response(
                    &router,
                    request(
                        "POST",
                        &format!("{HOST}{suffix}"),
                        Some(&body.to_string()),
                        Some(ADMIN),
                    ),
                )
                .await;
                assert_eq!(status, 400, "{suffix} {field}: {value}");
                assert_eq!(value["error"]["code"], "invalid_request", "{value}");
            }
        }
        let mut body = original;
        body["operation"] = json!("delete");
        let (status, value) = response(
            &router,
            request(
                "POST",
                &format!("{HOST}{suffix}"),
                Some(&body.to_string()),
                Some(ADMIN),
            ),
        )
        .await;
        assert_eq!(status, 400, "{value}");
        assert_eq!(value["error"]["code"], "invalid_request");
    }
    assert!(source.calls.lock().unwrap().is_empty());
}

// ADR 0040 §2: inference credentials cannot authorize host reads or writes,
// and rejected credentials never invoke the host provider.
#[tokio::test]
async fn all_host_admin_routes_require_management_authority() {
    let source = Fake::new(Ok(json!({})));
    let router = router(source.clone());
    let custom = custom().to_string();
    for (method, suffix, body) in [
        ("GET", "/engines/detected", None),
        ("POST", "/engines", Some(r#"{"path":"/opt/vllm"}"#)),
        ("POST", "/engines/local/remove", Some("{}")),
        ("POST", "/custom-engines/preview", Some(custom.as_str())),
        ("POST", "/custom-engines", Some(custom.as_str())),
        ("GET", "/model-store", None),
        ("POST", "/model-store/prune", Some(r#"{"apply":true}"#)),
        ("GET", "/config", None),
    ] {
        for credential in [None, Some(INFERENCE), Some("wrong-management-credential")] {
            let (status, value) = response(
                &router,
                request(method, &format!("{HOST}{suffix}"), body, credential),
            )
            .await;
            assert_eq!(status, 401, "{method} {suffix}: {value}");
        }
    }
    assert!(source.calls.lock().unwrap().is_empty());
}

// ADR 0040 §10: a role without an implementation refuses cleanly rather
// than pretending that the requested host has empty configuration or copies.
#[tokio::test]
async fn unavailable_provider_refuses_host_administration() {
    let router = router(Arc::new(UnavailableHostAdmin));
    let (status, value) = response(
        &router,
        request("GET", &format!("{HOST}/config"), None, Some(ADMIN)),
    )
    .await;
    assert_eq!(status, 422, "{value}");
    assert_eq!(value["api_version"], "1");
    assert_eq!(value["error"]["code"], "unsupported_capability");
}

// ADR 0040 §§10–11: provider refusals retain their stable codes and messages,
// including an ambiguous outcome, without inviting an automatic retry.
#[tokio::test]
async fn provider_failures_keep_the_management_error_envelope() {
    for (code, status) in [
        ("invalid_request", 400),
        ("invalid_config", 400),
        ("custom_manifest_invalid", 400),
        // ADR 0040 §10 / KTD11 / T16: bounded control messages fail locally.
        ("request_too_large", 413),
        ("host_not_found", 404),
        ("profile_not_found", 404),
        ("confirmation_required", 409),
        ("confirmation_stale", 409),
        ("profile_in_use", 409),
        ("outcome_unknown", 409),
        ("unsupported_capability", 422),
        ("host_capability_missing:host_admin", 422),
        ("host_upgrade_required", 422),
        ("host_admin_busy", 429),
        ("queue_full", 429),
        ("internal", 500),
        ("observation_stale", 503),
        ("agent_unreachable", 503),
        ("deadline_exceeded", 504),
    ] {
        let message = format!("Host refused: {code}");
        let source = Fake::new(Err(Failure {
            code: code.into(),
            message: message.clone(),
        }));
        let router = router(source.clone());
        let (actual_status, value) = response(
            &router,
            request("GET", &format!("{HOST}/config"), None, Some(ADMIN)),
        )
        .await;
        assert_eq!(actual_status, status, "{code}: {value}");
        assert_eq!(
            value,
            json!({
                "api_version": "1",
                "error": {
                    "code": code,
                    "message": message,
                    "retryable": false,
                    "operation_id": null,
                    "details": {}
                }
            })
        );
        assert_eq!(
            *source.calls.lock().unwrap(),
            vec![("lab east".into(), Operation::Config)]
        );
    }
}

// ADR 0040 §11: the manifest cap counts UTF-8 bytes, independently of the
// whole JSON body's cap. Both preview and confirmation enforce it.
#[tokio::test]
async fn manifest_and_request_body_limits_are_enforced_before_dispatch() {
    let source = Fake::new(Ok(json!({})));
    let router = router(source.clone());
    for suffix in ["/custom-engines/preview", "/custom-engines"] {
        for manifest in ["a".repeat(48 * 1024 + 1), "é".repeat(24 * 1024 + 1)] {
            let mut body = custom();
            body["manifest"] = json!(manifest);
            if suffix == "/custom-engines" {
                body["digest"] = json!("sha256:preview-digest");
                body["confirm_name"] = json!("custom");
            }
            let (status, value) = response(
                &router,
                request(
                    "POST",
                    &format!("{HOST}{suffix}"),
                    Some(&body.to_string()),
                    Some(ADMIN),
                ),
            )
            .await;
            assert_eq!(status, 413, "{suffix}: {value}");
            assert_eq!(value["error"]["code"], "body_too_large");
        }
    }
    let oversized = format!(r#"{{"path":"/opt/vllm"}}{}"#, " ".repeat(64 * 1024));
    let (status, value) = response(
        &router,
        request(
            "POST",
            &format!("{HOST}/engines"),
            Some(&oversized),
            Some(ADMIN),
        ),
    )
    .await;
    assert_eq!(status, 413, "{value}");
    assert_eq!(value["error"]["code"], "body_too_large");
    assert!(source.calls.lock().unwrap().is_empty());

    let mut body = custom();
    body["manifest"] = json!("a".repeat(48 * 1024));
    let (status, value) = response(
        &router,
        request(
            "POST",
            &format!("{HOST}/custom-engines/preview"),
            Some(&body.to_string()),
            Some(ADMIN),
        ),
    )
    .await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(
        *source.calls.lock().unwrap(),
        vec![("lab east".into(), Operation::Preview(body))]
    );
}

// ADR 0040 §§2, 10: writes require JSON, and read routes do not silently
// discard bodies. A JSON media type with a charset is still JSON.
#[tokio::test]
async fn content_types_and_unexpected_read_bodies_are_checked() {
    let source = Fake::new(Ok(json!({})));
    let router = router(source.clone());
    for content_type in [None, Some("text/plain")] {
        let mut ask = request(
            "POST",
            &format!("{HOST}/engines"),
            Some(r#"{"path":"/opt/vllm"}"#),
            Some(ADMIN),
        );
        ask.headers_mut().remove("content-type");
        if let Some(content_type) = content_type {
            ask.headers_mut()
                .insert("content-type", content_type.parse().unwrap());
        }
        let (status, value) = response(&router, ask).await;
        assert_eq!(status, 415, "{value}");
        assert_eq!(value["error"]["code"], "unsupported_media_type");
    }
    for suffix in ["/engines/detected", "/model-store", "/config"] {
        let (status, value) = response(
            &router,
            request("GET", &format!("{HOST}{suffix}"), Some("{}"), Some(ADMIN)),
        )
        .await;
        assert_eq!(status, 400, "{suffix}: {value}");
        assert_eq!(value["error"]["code"], "invalid_request");
    }
    assert!(source.calls.lock().unwrap().is_empty());
    let mut ask = request(
        "POST",
        &format!("{HOST}/engines"),
        Some(r#"{"path":"/opt/vllm"}"#),
        Some(ADMIN),
    );
    ask.headers_mut().insert(
        "content-type",
        "application/json; charset=utf-8".parse().unwrap(),
    );
    let (status, value) = response(&router, ask).await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(
        *source.calls.lock().unwrap(),
        vec![(
            "lab east".into(),
            Operation::Add(json!({"path": "/opt/vllm"}))
        )]
    );
}

// ADR 0040 §11: change previews and confirmations must retain the options
// included in the digest, including the exact environment and approval.
#[tokio::test]
async fn approval_settings_are_forwarded_without_rewriting() {
    let source = Fake::new(Ok(json!({})));
    let router = router(source.clone());
    let settings = json!({
        "deep_park": false,
        "drift": "warn",
        "env": {"CUSTOM_DEVICE": "gpu:1", "OPTION": "a=b"},
        "approved_env": ["CUSTOM_DEVICE", "OPTION"]
    });
    let mut preview = custom();
    preview["operation"] = json!("change");
    preview
        .as_object_mut()
        .unwrap()
        .extend(settings.as_object().unwrap().clone());
    let mut confirm = preview.clone();
    confirm["digest"] = json!("sha256:preview-digest");
    confirm["confirm_name"] = json!("custom");
    let mut add = settings;
    add["path"] = json!("/opt/vllm");
    add["paths"] = json!(["/opt/extra"]);
    add["allow_remote_code"] = json!(true);
    add["digest"] = json!("sha256:builtin-preview");
    add["confirm_name"] = json!("vllm");
    add["args"] = json!(["--trust-remote-code"]);
    add["approved_options"] = json!(["--trust-remote-code"]);
    add["approved_paths"] = json!(["/opt/extra"]);
    for (suffix, body) in [
        ("/custom-engines/preview", &preview),
        ("/custom-engines", &confirm),
        ("/engines", &add),
    ] {
        let (status, value) = response(
            &router,
            request(
                "POST",
                &format!("{HOST}{suffix}"),
                Some(&body.to_string()),
                Some(ADMIN),
            ),
        )
        .await;
        assert_eq!(status, 200, "{suffix}: {value}");
    }
    assert_eq!(
        *source.calls.lock().unwrap(),
        vec![
            ("lab east".into(), Operation::Preview(preview)),
            ("lab east".into(), Operation::Confirm(confirm)),
            ("lab east".into(), Operation::Add(add)),
        ]
    );
}

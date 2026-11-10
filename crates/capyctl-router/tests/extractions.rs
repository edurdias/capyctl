//! T50: ADR 0041 §§3, 6 extraction routing through a local HTTP engine.
//! CPU tests, not engine qualification.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
use capyctl_config::effective::DeploymentApi;
use capyctl_config::extractions::FieldType;
use capyctl_controller::{LeaseEnd, LeaseRefused, LifecycleFault, RequestLease};
use capyctl_router::{QueueLimits, RouterDeps};
use serde_json::{json, Value};
use tower::ServiceExt;

const DEPLOYMENT: &str = "dep-clef";
const ROUTE: &str = "clef";
const SERVED: &str = "served-clef";

/// One deployment serving `api`, parked until a request activates it, whose
/// engine runs at `endpoint`. Every lease it grants is recorded with its end.
struct Authority {
    api: DeploymentApi,
    ready: AtomicBool,
    activations: AtomicUsize,
    endpoint: String,
    leases: Mutex<Vec<Option<LeaseEnd>>>,
    field_types: Vec<FieldType>,
}

impl Authority {
    fn row(&self) -> capyctl_store::deployments::DeploymentRow {
        let observed = if self.ready.load(Ordering::SeqCst) {
            capyctl_domain::LifecycleState::Ready
        } else {
            capyctl_domain::LifecycleState::Parked
        };
        capyctl_store::deployments::DeploymentRow {
            id: DEPLOYMENT.into(),
            name: "clef".into(),
            kind: "llamacpp".into(),
            route_model_id: Some(ROUTE.into()),
            desired_state: capyctl_domain::LifecycleState::Ready,
            observed_state: observed,
            schema_version: 1,
            current_generation: 1,
        }
    }
    fn ends(&self) -> Vec<Option<LeaseEnd>> {
        self.leases.lock().unwrap().clone()
    }
}

fn refuse(what: &str) -> LifecycleFault {
    LifecycleFault::Blocked(format!("the test authority cannot {what}"))
}

#[async_trait::async_trait]
impl capyctl_controller::LifecyclePort for Authority {
    fn find_deployment_by_route(
        &self,
        route: &str,
    ) -> Result<Option<capyctl_store::deployments::DeploymentRow>, LifecycleFault> {
        Ok((route == ROUTE).then(|| self.row()))
    }
    fn list_enabled_routes(
        &self,
    ) -> Result<Vec<capyctl_store::deployments::PublicRoute>, LifecycleFault> {
        Ok(vec![capyctl_store::deployments::PublicRoute {
            id: ROUTE.into(),
            api: self.api,
            field_types: self.field_types.clone(),
        }])
    }
    fn deployment_api(&self, deployment: &str) -> Result<DeploymentApi, LifecycleFault> {
        assert_eq!(deployment, DEPLOYMENT);
        Ok(self.api)
    }
    fn deployment_field_types(&self, deployment: &str) -> Result<Vec<FieldType>, LifecycleFault> {
        assert_eq!(deployment, DEPLOYMENT);
        Ok(self.field_types.clone())
    }
    fn get_deployment(
        &self,
        id: &str,
    ) -> Result<Option<capyctl_store::deployments::DeploymentRow>, LifecycleFault> {
        Ok((id == DEPLOYMENT).then(|| self.row()))
    }
    fn latest_operation(
        &self,
        _d: &str,
    ) -> Result<Option<capyctl_store::deployments::OperationRow>, LifecycleFault> {
        Ok(None)
    }
    fn ready_deployments_excluding(&self, _d: &str) -> Result<Vec<String>, LifecycleFault> {
        Ok(Vec::new())
    }
    fn runtime_endpoint(
        &self,
        deployment: &str,
    ) -> Result<Option<capyctl_controller::RuntimeEndpoint>, LifecycleFault> {
        Ok(
            (deployment == DEPLOYMENT).then(|| capyctl_controller::RuntimeEndpoint {
                endpoint: self.endpoint.clone(),
                served_model: SERVED.into(),
                engine_key: Some("launch-key".into()),
                incarnation: "1".into(),
                engine: Some("llamacpp".into()),
                deployment_id: DEPLOYMENT.into(),
                revision: 1,
                request_abort: false,
                engine_trace: false,
            }),
        )
    }
    async fn open_request_lease(
        &self,
        _d: &str,
        _max: usize,
    ) -> Result<Option<RequestLease>, LeaseRefused> {
        assert!(
            self.ready.load(Ordering::SeqCst),
            "SPEC §10: nothing is dispatched before the deployment is READY"
        );
        let mut leases = self.leases.lock().unwrap();
        leases.push(None);
        Ok(Some(RequestLease::unrecorded(
            &(leases.len() - 1).to_string(),
        )))
    }
    async fn close_request_lease(
        &self,
        lease: RequestLease,
        end: LeaseEnd,
    ) -> Result<(), LeaseRefused> {
        let index: usize = lease.id().parse().unwrap();
        let mut leases = self.leases.lock().unwrap();
        assert!(leases[index].is_none(), "a lease is closed once");
        leases[index] = Some(end);
        Ok(())
    }
    fn clear_suspension(&self, _d: &str) -> Result<(), LifecycleFault> {
        Err(refuse("clear a suspension"))
    }
    fn journal(
        &self,
        _h: Option<&str>,
        _o: Option<&str>,
        _s: Option<&str>,
        _e: &str,
    ) -> Result<(), LifecycleFault> {
        Err(refuse("journal"))
    }
    async fn observe_adapter(
        &self,
        _d: &str,
    ) -> Result<capyctl_adapters::traits::WorkObservation, LifecycleFault> {
        Err(refuse("observe"))
    }
    async fn idle_stop(
        &self,
        _d: &str,
    ) -> Result<capyctl_controller::OperationHandle, LifecycleFault> {
        Err(refuse("stop"))
    }
    async fn wait_terminal(
        &self,
        _h: &capyctl_controller::OperationHandle,
    ) -> Result<capyctl_domain::LifecycleState, LifecycleFault> {
        Err(refuse("wait"))
    }
    async fn auto_activate(
        &self,
        _d: &str,
    ) -> Result<capyctl_controller::OperationHandle, LifecycleFault> {
        Err(refuse("activate outside a request"))
    }
    async fn request_transition(
        &self,
        _d: &str,
        _a: capyctl_domain::LifecycleAction,
    ) -> Result<capyctl_controller::OperationHandle, LifecycleFault> {
        Err(refuse("transition"))
    }
    async fn activate_for_request(&self, deployment: &str) -> Result<(), LifecycleFault> {
        assert_eq!(deployment, DEPLOYMENT);
        self.activations.fetch_add(1, Ordering::SeqCst);
        self.ready.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// How the engine double answers every request.
#[derive(Clone)]
enum Reply {
    /// A status, content type and body.
    Answer(u16, &'static str, String),
    /// A JSON answer whose body transfer fails part way.
    Broken,
}

/// What reached the engine: path, authorization and body.
type Seen = Arc<Mutex<Vec<(String, String, Value)>>>;

async fn engine(reply: Reply) -> (String, Seen) {
    let seen: Seen = Arc::default();
    let recorded = seen.clone();
    let app = axum::Router::new().fallback(
        move |uri: axum::http::Uri, headers: axum::http::HeaderMap, body: axum::body::Bytes| {
            let reply = reply.clone();
            let recorded = recorded.clone();
            async move {
                let authorization = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                recorded.lock().unwrap().push((
                    uri.path().to_owned(),
                    authorization,
                    serde_json::from_slice(&body).unwrap_or(Value::Null),
                ));
                match reply {
                    Reply::Answer(status, content_type, body) => {
                        axum::response::Response::builder()
                            .status(status)
                            .header("content-type", content_type)
                            .body(axum::body::Body::from(body))
                            .unwrap()
                    }
                    Reply::Broken => {
                        let parts = futures::stream::iter([
                            Ok(axum::body::Bytes::from_static(
                                b"{\"model\":\"served-clef\",\"answers\":{",
                            )),
                            Err(std::io::Error::other("the engine went away")),
                        ]);
                        axum::response::Response::builder()
                            .header("content-type", "application/json")
                            .body(axum::body::Body::from_stream(parts))
                            .unwrap()
                    }
                }
            }
        },
    );
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
    (base, seen)
}

fn answer() -> Value {
    json!({"model": SERVED, "answers": {"amount": {"type": "number", "value": 12.5,
        "guess": 12.5, "confidence": 0.9, "p_null": 0.1}},
        "usage": {"input_tokens": 87, "output_tokens": 0}})
}

fn extraction_body() -> Value {
    json!({"model": ROUTE, "state": "Paid 12.50 USD", "fields": {
        "amount": {"type": "number", "decimals": 2, "unit": "USD", "horizon": 2}},
        "reference_date": "2026-11-04", "options": {"include_quantiles": true},
        "images": ["data:image/png;base64,AA=="]})
}

fn stack(api: DeploymentApi, endpoint: &str) -> (Arc<Authority>, RouterDeps, axum::Router) {
    let authority = Arc::new(Authority {
        api,
        ready: AtomicBool::new(false),
        activations: AtomicUsize::new(0),
        endpoint: endpoint.into(),
        leases: Mutex::new(Vec::new()),
        field_types: FieldType::ALL.to_vec(),
    });
    let port = authority.clone() as Arc<dyn capyctl_controller::LifecyclePort>;
    let deps = RouterDeps {
        controller: port.clone(),
        forwards: Arc::new(capyctl_router::forwarders::LiveForwarders::new(port)),
        limits: QueueLimits {
            max_requests_per_deployment: 4,
            max_buffered_bytes_total: 1 << 20,
        },
        api_key: None,
        inflight: Arc::new(capyctl_router::admission::InFlight::default()),
        activation_join: Arc::new(capyctl_router::WakeJoin::new()),
        tracing: capyctl_trace::Tracing::off(),
    };
    let router = capyctl_router::serve_router(deps.clone());
    (authority, deps, router)
}

async fn send(router: &axum::Router, path: &str, body: Vec<u8>) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method(if body.is_empty() { "GET" } else { "POST" })
                .uri(path)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn decide(router: &axum::Router, body: &Value) -> (StatusCode, Value) {
    send(router, "/v1/systemone", body.to_string().into_bytes()).await
}

// T50
// ADR 0041 §3: extraction requests retain decision forwarding and settlement.
#[tokio::test]
async fn an_extraction_is_admitted_forwarded_and_settled_like_chat() {
    let (endpoint, seen) =
        engine(Reply::Answer(200, "application/json", answer().to_string())).await;
    let (authority, deps, router) = stack(DeploymentApi::Extractions, &endpoint);
    let sent = extraction_body();
    let (status, body) = decide(&router, &sent).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(authority.activations.load(Ordering::SeqCst), 1);
    let mut expected = sent.clone();
    expected["model"] = json!(SERVED);
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [(
            "/v1/systemone".to_owned(),
            "Bearer launch-key".to_owned(),
            expected
        )]
    );
    assert_eq!(body["model"], ROUTE);
    assert_eq!(body["answers"], answer()["answers"]);
    assert_eq!(body["usage"], answer()["usage"]);
    let metrics = &body["capyctl"]["metrics"];
    assert_eq!(
        metrics["input_tokens"],
        json!({"value": 87, "source": "engine"}),
        "{body}"
    );
    for figure in ["total_ms", "time_to_response_ms"] {
        assert_eq!(metrics[figure]["source"], "router", "{body}");
        assert!(metrics[figure]["value"].as_f64().unwrap() >= 0.0, "{body}");
    }
    for figure in ["ttft_ms", "decode_tokens_per_second"] {
        assert!(metrics.get(figure).is_none(), "{body}");
    }
    assert_eq!(authority.ends(), vec![Some(LeaseEnd::Completed)]);
    assert_eq!(deps.inflight.current(DEPLOYMENT), 0);

    let (status, _) = decide(&router, &sent).await;
    assert_eq!(status, 200);
    assert_eq!(
        authority.activations.load(Ordering::SeqCst),
        1,
        "already serving"
    );
    assert_eq!(authority.ends(), vec![Some(LeaseEnd::Completed); 2]);
}

// T50, SPEC §10 (#220, #223): extraction preserves the same byte contract
// as decisions across the integration line's raw request/answer transport.
#[tokio::test]
async fn extraction_preserves_field_order_and_numeric_spelling() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    let app = axum::Router::new().fallback(move |body: axum::body::Bytes| {
        let captured = captured.clone();
        async move {
            *captured.lock().unwrap() = body.to_vec();
            (
                [("content-type", "application/json")],
                r#"{"model":"served-clef","answers":{"z":{"type":"number","value":1.2500e1,"guess":12.500,"confidence":0.9000,"p_null":1e-1},"a":{"type":"text","value":"Paid"}},"usage":{"input_tokens":87,"output_tokens":0}}"#,
            )
        }
    });
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", socket.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
    let (authority, _, router) = stack(DeploymentApi::Extractions, &endpoint);
    let request = r#"{"model":"clef","state":"Paid 12.50 USD","fields":{"z":{"type":"number","instructions":"Amount"},"a":{"type":"text","instructions":"Action"}}}"#;
    let response = router
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/systemone")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(request))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        request
            .replacen("\"clef\"", "\"served-clef\"", 1)
            .as_bytes()
    );
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let body = std::str::from_utf8(&bytes).unwrap();
    assert!(
        body.contains(r#""value":1.2500e1,"guess":12.500,"confidence":0.9000,"p_null":1e-1"#),
        "{body}"
    );
    assert!(body.find("\"z\"").unwrap() < body.find("\"a\"").unwrap());
    assert_eq!(authority.ends(), vec![Some(LeaseEnd::Completed)]);
}

// T50
// ADR 0041 §3: extraction requests retain decision forwarding and settlement.
#[tokio::test]
async fn an_engine_rejection_is_relayed_and_closes_the_lease() {
    for (status, body, message) in [
        (
            400,
            r#"{"error":{"code":400,"message":"questions.q: \"instructions\" must be provided","type":"invalid_request_error"}}"#,
            "must be provided",
        ),
        (
            422,
            r#"{"detail":[{"type":"union_tag_invalid","loc":["body","questions","q"],"msg":"Input tag 'x' does not match any of the expected tags","input":{}}]}"#,
            "does not match",
        ),
    ] {
        let (endpoint, _) = engine(Reply::Answer(status, "application/json", body.into())).await;
        let (authority, deps, router) = stack(DeploymentApi::Extractions, &endpoint);
        let (answered, answer) = decide(&router, &extraction_body()).await;
        assert_eq!(answered.as_u16(), status, "{answer}");
        assert_eq!(answer["code"], "engine_rejected");
        assert!(
            answer["message"].as_str().unwrap().contains(message),
            "{answer}"
        );
        assert_eq!(authority.ends(), vec![Some(LeaseEnd::Completed)]);
        assert_eq!(deps.inflight.current(DEPLOYMENT), 0);
    }
}

// T50
// ADR 0041 §3: extraction requests retain decision forwarding and settlement.
#[tokio::test]
async fn an_unverified_extraction_answer_leaves_the_lease_uncertain() {
    let truncated = answer().to_string();
    let truncated = truncated[..truncated.len() / 2].to_owned();
    let duplicated =
        r#"{"model":"served-clef","answers":{"q":{"type":"noul","noul":0.5}},"answers":{}}"#;
    let cut_error = LLAMA_TOO_LARGE[..LLAMA_TOO_LARGE.len() / 2].to_owned();
    for reply in [
        Reply::Answer(200, "application/json", truncated),
        Reply::Answer(200, "application/json", r#"{"model":"served-clef"}"#.into()),
        Reply::Answer(200, "application/json", r#"{"answers":{"q":0.5}}"#.into()),
        Reply::Answer(200, "application/json", duplicated.into()),
        Reply::Answer(200, "text/plain", answer().to_string()),
        Reply::Answer(500, "application/json", cut_error),
        Reply::Broken,
    ] {
        let (endpoint, seen) = engine(reply).await;
        let (authority, deps, router) = stack(DeploymentApi::Extractions, &endpoint);
        let (status, answer) = decide(&router, &extraction_body()).await;
        assert_eq!(status, 500, "{answer}");
        assert_eq!(answer["code"], "engine_error");
        assert_eq!(authority.ends(), vec![Some(LeaseEnd::Uncertain)]);
        assert_eq!(seen.lock().unwrap().len(), 1, "never replayed");
        tokio::time::timeout(Duration::from_secs(5), async {
            while deps.inflight.current(DEPLOYMENT) != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the in-memory slot is released; the durable lease keeps the charge");
    }
}

// T50
// ADR 0041 §3: extraction requests retain decision forwarding and settlement.
#[tokio::test]
async fn an_engine_error_answer_is_relayed_and_leaves_the_lease_cancelling() {
    let (endpoint, seen) = engine(Reply::Answer(
        500,
        "application/json",
        LLAMA_TOO_LARGE.into(),
    ))
    .await;
    let (authority, deps, router) = stack(DeploymentApi::Extractions, &endpoint);
    let (status, answer) = decide(&router, &extraction_body()).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{answer}");
    assert_eq!(answer["code"], "engine_error");
    assert_eq!(answer["retryable"], false);
    assert!(
        answer["message"]
            .as_str()
            .unwrap()
            .starts_with("input (8195 tokens) is too large to process"),
        "{answer}"
    );
    assert_eq!(
        authority.ends(),
        vec![Some(LeaseEnd::Cancelling { answered: true })]
    );
    assert_eq!(seen.lock().unwrap().len(), 1, "never replayed");
    assert_eq!(deps.inflight.current(DEPLOYMENT), 0);
}

const LLAMA_TOO_LARGE: &str =
    r#"{"error":{"message":"input (8195 tokens) is too large to process"}}"#;

// T50
// ADR 0041 §3: closed shapes and unsupported declared types fail before activation.
#[tokio::test]
async fn invalid_extractions_never_activate_or_lease() {
    let (endpoint, seen) =
        engine(Reply::Answer(200, "application/json", answer().to_string())).await;
    let (_, mut deps, _) = stack(DeploymentApi::Extractions, &endpoint);
    let authority = Arc::new(Authority {
        api: DeploymentApi::Extractions,
        ready: AtomicBool::new(false),
        activations: AtomicUsize::new(0),
        endpoint,
        leases: Mutex::new(Vec::new()),
        field_types: vec![FieldType::Text],
    });
    let port = authority.clone() as Arc<dyn capyctl_controller::LifecyclePort>;
    deps.controller = port.clone();
    deps.forwards = Arc::new(capyctl_router::forwarders::LiveForwarders::new(port));
    let router = capyctl_router::serve_router(deps);
    let (status, body) = decide(&router, &extraction_body()).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], "field_type_unsupported");
    assert!(body["message"].as_str().unwrap().contains("amount"));
    assert!(body["message"].as_str().unwrap().contains("number"));
    assert_eq!(authority.activations.load(Ordering::SeqCst), 0);
    assert!(authority.ends().is_empty());
    assert!(seen.lock().unwrap().is_empty());

    let (authority, _deps, router) = stack(DeploymentApi::Extractions, "http://127.0.0.1:9");
    for (key, value) in [
        ("fields", json!({})),
        ("fields", json!({"bad name": {"type": "text"}})),
        ("fields", json!({"x": {"type": "unknown"}})),
        ("fields", json!({"x": {"type": "text", "unknown": true}})),
        ("fields", json!({"x": {"type": "number", "decimals": 7}})),
        ("fields", json!({"x": {"type": "list"}})),
        ("fields", json!({"x": {"type": "estimate", "min": 0}})),
        ("images", json!([1])),
        (
            "images",
            json!(["data:a", "data:b", "data:c", "data:d", "data:e"]),
        ),
        ("reference_date", json!("2026-02-30")),
        ("options", json!({"return_state_text": "yes"})),
        ("files", json!([])),
        ("state", json!(null)),
    ] {
        let mut sent = extraction_body();
        sent[key] = value;
        let (status, body) = decide(&router, &sent).await;
        assert_eq!(status, 400, "{sent}: {body}");
        assert_eq!(body["code"], "invalid_request", "{sent}: {body}");
    }
    assert_eq!(authority.activations.load(Ordering::SeqCst), 0);
    assert!(authority.ends().is_empty());
}

// T50
// ADR 0041 §3: presence of the other contract's key includes null and both keys.
#[tokio::test]
async fn systemone_shape_mismatches_are_refused_both_ways() {
    for (api, wrong, expected) in [
        (DeploymentApi::Extractions, "questions", "fields"),
        (DeploymentApi::Decisions, "fields", "questions"),
    ] {
        let (authority, _deps, router) = stack(api, "http://127.0.0.1:9");
        for other in [Value::Null, json!({}), json!({"q": {"type": "text"}})] {
            for both in [false, true] {
                let mut sent = json!({"model": ROUTE, "state": "hello"});
                sent[wrong] = other.clone();
                if both {
                    sent[expected] = json!({});
                }
                let (status, body) = decide(&router, &sent).await;
                assert_eq!(status, 400, "{sent}: {body}");
                assert_eq!(body["code"], "api_mismatch", "{sent}: {body}");
                assert!(
                    body["message"].as_str().unwrap().contains(expected),
                    "{body}"
                );
            }
        }
        assert_eq!(authority.activations.load(Ordering::SeqCst), 0);
        assert!(authority.ends().is_empty());
    }
}

// T50
// ADR 0041 §3: extraction deployments refuse chat and embedding endpoints.
#[tokio::test]
async fn extractions_refuse_other_endpoints_before_activation() {
    let (authority, _deps, router) = stack(DeploymentApi::Extractions, "http://127.0.0.1:9");
    for (path, sent) in [
        (
            "/v1/chat/completions",
            json!({"model": ROUTE, "messages": []}),
        ),
        ("/v1/embeddings", json!({"model": ROUTE, "input": "hello"})),
    ] {
        let (status, body) = send(&router, path, sent.to_string().into_bytes()).await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(body["code"], "api_mismatch");
        assert!(body["message"].as_str().unwrap().contains("/v1/systemone"));
    }
    assert_eq!(authority.activations.load(Ordering::SeqCst), 0);
    assert!(authority.ends().is_empty());
}

// T50
// ADR 0041 §6: discovery advertises canonical declared types without waking.
#[tokio::test]
async fn discovery_names_extractions_and_canonical_types_without_activation() {
    let (authority, _deps, router) = stack(DeploymentApi::Extractions, "http://127.0.0.1:9");
    let (status, body) = send(&router, "/v1/models", Vec::new()).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"][0]["id"], ROUTE);
    assert_eq!(body["data"][0]["capyctl"]["api"], "extractions");
    assert_eq!(
        body["data"][0]["capyctl"]["field_types"],
        json!([
            "text", "number", "date", "time", "datetime", "duration", "list", "estimate",
            "forecast"
        ])
    );
    assert_eq!(authority.activations.load(Ordering::SeqCst), 0);
    assert!(authority.ends().is_empty());
}

// T50
// ADR 0041 §3: request IDs follow the same public contract as decisions.
#[tokio::test]
async fn extraction_responses_preserve_request_ids() {
    let (endpoint, _) = engine(Reply::Answer(200, "application/json", answer().to_string())).await;
    let (authority, _deps, router) = stack(DeploymentApi::Extractions, &endpoint);
    for id in ["extraction-42", "invalid id"] {
        let response = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/systemone")
                    .header("content-type", "application/json")
                    .header("x-request-id", id)
                    .body(axum::body::Body::from(extraction_body().to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        if id == "extraction-42" {
            assert_eq!(response.status(), 200);
            assert_eq!(response.headers()["x-request-id"], id);
        } else {
            assert_eq!(response.status(), 400);
        }
    }
    assert_eq!(authority.ends(), vec![Some(LeaseEnd::Completed)]);
}

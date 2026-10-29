//! T43 (ADR 0030 §2, §5): `POST /v1/systemone` is validated, routed, admitted,
//! leased and settled like a chat completion, through the real engine
//! forwarder to an engine double; the endpoint that does not answer a
//! deployment's API is refused before activation; `/v1/models` names each
//! deployment's API without waking it. CPU tests, not qualification.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
use capyctl_config::effective::DeploymentApi;
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
    /// ADR 0030 §3: an engine whose activation is proven by the fixed
    /// decision probe, as an adapter's Initialize proves it; `None` is ready
    /// on activation.
    fake: Option<Arc<capyctl_testkit::FakeEngine>>,
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
        }])
    }
    fn deployment_api(&self, deployment: &str) -> Result<DeploymentApi, LifecycleFault> {
        assert_eq!(deployment, DEPLOYMENT);
        Ok(self.api)
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
        if let Some(fake) = &self.fake {
            use capyctl_adapters::traits::ChatForward;
            fake.decision_probe(SERVED, Duration::from_secs(5))
                .await
                .map_err(|_| refuse("prove the decision model ready"))?;
        }
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
    /// Nothing, ever.
    Stall,
}

/// What reached the engine: path, authorization and body.
type Seen = Arc<Mutex<Vec<(String, String, Value)>>>;

async fn engine(reply: Reply) -> (String, Seen) {
    let (base, seen, _) = engine_with_bodies(reply).await;
    (base, seen)
}

/// As [`engine`], also keeping each request body as the engine received it.
async fn engine_with_bodies(reply: Reply) -> (String, Seen, Arc<Mutex<Vec<String>>>) {
    let seen: Seen = Arc::default();
    let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
    let recorded = seen.clone();
    let kept = bodies.clone();
    let app = axum::Router::new().fallback(
        move |uri: axum::http::Uri, headers: axum::http::HeaderMap, body: axum::body::Bytes| {
            let reply = reply.clone();
            let recorded = recorded.clone();
            let kept = kept.clone();
            async move {
                let authorization = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                kept.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&body).into_owned());
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
                    Reply::Stall => futures::future::pending().await,
                }
            }
        },
    );
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
    (base, seen, bodies)
}

/// llama.cpp v0.7.0's answer for a decision model (observed live 2026-10-17).
fn answer() -> Value {
    json!({
        "model": SERVED,
        "answers": {
            "route": {"type": "choice", "choice": "billing",
                      "probabilities": {"billing": 0.9503, "shipping": 0.0157, "technical": 0.0339},
                      "confidence": 0.9255},
            "angry": {"type": "noul", "noul": 0.6226},
            "urgency": {"type": "score", "score": 1.1224,
                        "legend": {"0": "can wait", "1": "this week", "2": "today"},
                        "probabilities": {"0": 0.1, "1": 0.67, "2": 0.23}, "confidence": 0.5},
        },
        "usage": {"input_tokens": 87, "output_tokens": 0},
    })
}

fn decision_body() -> Value {
    json!({
        "model": ROUTE,
        "state": "I was charged twice for my order last week.",
        "questions": {
            "route": {"type": "choice", "instructions": "Which team should handle this?",
                      "criteria": {"billing": "payments and refunds", "shipping": null, "technical": null}},
            "angry": {"type": "noul", "instructions": "Is the customer angry?"},
            "urgency": {"type": "score", "instructions": "How urgent is this?",
                        "criteria": ["can wait", "this week", "today"]},
        },
        "images": ["data:image/png;base64,iVBORw0KGgo="],
    })
}

fn chat_body(stream: bool) -> Value {
    json!({"model": ROUTE, "stream": stream, "messages": [{"role": "user", "content": "hi"}]})
}

fn stack(api: DeploymentApi, endpoint: &str) -> (Arc<Authority>, RouterDeps, axum::Router) {
    let authority = Arc::new(Authority {
        api,
        ready: AtomicBool::new(false),
        activations: AtomicUsize::new(0),
        endpoint: endpoint.into(),
        leases: Mutex::new(Vec::new()),
        fake: None,
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
    };
    let router = capyctl_router::serve_router(deps.clone());
    (authority, deps, router)
}

// T43 (ADR 0030 §§2-3): a decisions deployment on the Fake engine becomes
// ready on its fixed decision (never a chat request) and answers
// `POST /v1/systemone` through the router, its lease closed completed.
#[tokio::test]
async fn a_fake_decision_model_becomes_ready_and_answers_through_the_router() {
    let fake = Arc::new(capyctl_testkit::FakeEngine::new().with_decisions());
    let authority = Arc::new(Authority {
        api: DeploymentApi::Decisions,
        ready: AtomicBool::new(false),
        activations: AtomicUsize::new(0),
        endpoint: "http://127.0.0.1:9".into(),
        leases: Mutex::new(Vec::new()),
        fake: Some(fake.clone()),
    });
    let port = authority.clone() as Arc<dyn capyctl_controller::LifecyclePort>;
    let forward = fake.clone() as Arc<dyn capyctl_adapters::traits::ChatForward>;
    let router = capyctl_router::serve_router(RouterDeps {
        controller: port,
        forwards: Arc::new(capyctl_router::forwarders::StaticForwarders(
            std::collections::HashMap::from([(DEPLOYMENT.to_owned(), forward)]),
        )),
        limits: QueueLimits {
            max_requests_per_deployment: 4,
            max_buffered_bytes_total: 1 << 20,
        },
        api_key: None,
        inflight: Arc::new(capyctl_router::admission::InFlight::default()),
        activation_join: Arc::new(capyctl_router::WakeJoin::new()),
    });
    let (status, body) = send(
        &router,
        "/v1/systemone",
        serde_json::to_vec(&decision_body()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["answers"]["route"]["type"], "choice");
    assert_eq!(authority.activations.load(Ordering::SeqCst), 1);
    assert!(authority.ready.load(Ordering::SeqCst));
    assert_eq!(authority.ends(), vec![Some(LeaseEnd::Completed)]);
    // One decision proved readiness, one answered the request; no chat.
    assert_eq!(fake.decision_requests(), 2);
    assert_eq!(fake.chat_requests(), 0);
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

// T43 (ADR 0030 §2): a decision request wakes its deployment, is leased and
// forwarded as one JSON body with only `model` replaced by the served name,
// and comes back whole with the public name and the engine's usage; its
// completion closes the lease and frees the slot. Its `capyctl.metrics` has
// the router's total and time to response and the engine's input tokens, and
// no figure of generated text (owner decision 2026-10-19).
#[tokio::test]
async fn a_decision_is_admitted_forwarded_and_settled_like_chat() {
    let (endpoint, seen) =
        engine(Reply::Answer(200, "application/json", answer().to_string())).await;
    let (authority, deps, router) = stack(DeploymentApi::Decisions, &endpoint);
    let sent = decision_body();
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

// T43 T19 (issue #220): the engine receives the caller's bytes with only
// what CapyCTL sets written anew. A decision's questions keep the caller's
// order (SGLang numbers them in its prompt in that order), and so does the
// property order of a chat request's JSON schema. A body with a duplicated
// key is refused before anything activates or is sent.
#[tokio::test]
async fn the_engine_receives_the_callers_key_order() {
    let served = |sent: &str| {
        sent.replace(
            &format!(r#""model":"{ROUTE}""#),
            &format!(r#""model":"{SERVED}""#),
        )
    };
    let (endpoint, _, bodies) =
        engine_with_bodies(Reply::Answer(200, "application/json", answer().to_string())).await;
    let (authority, _deps, router) = stack(DeploymentApi::Decisions, &endpoint);
    let decision = format!(
        r#"{{"state":"Patient, 35, mild headache.","model":"{ROUTE}","questions":{{"urgency":{{"type":"score","instructions":"Triage urgency","criteria":["self-care","routine","urgent"]}},"cause":{{"type":"choice","instructions":"Most likely cause","criteria":{{"tension":null,"caffeine":null}}}},"doctor":{{"type":"noul","instructions":"See a doctor this week?"}}}}}}"#
    );
    let (status, body) = send(&router, "/v1/systemone", decision.clone().into_bytes()).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(*bodies.lock().unwrap(), [served(&decision)]);

    let duplicated = format!(
        r#"{{"model":"{ROUTE}","state":"s","questions":{{"q":{{"type":"noul","type":"choice"}}}}}}"#
    );
    let (status, body) = send(&router, "/v1/systemone", duplicated.into_bytes()).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], "invalid_request", "{body}");
    assert_eq!(bodies.lock().unwrap().len(), 1, "nothing more was sent");
    assert_eq!(authority.ends(), vec![Some(LeaseEnd::Completed)]);

    let chunk = |delta: Value, finish: Value| {
        format!(
            "data: {}\n\n",
            json!({"id": "c", "object": "chat.completion.chunk", "created": 1, "model": SERVED,
                   "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
        )
    };
    let sse = [
        chunk(json!({"role": "assistant", "content": "{}"}), Value::Null),
        chunk(json!({}), json!("stop")),
        "data: [DONE]\n\n".to_owned(),
    ]
    .concat();
    let (endpoint, _, bodies) =
        engine_with_bodies(Reply::Answer(200, "text/event-stream", sse)).await;
    let (_, _deps, router) = stack(DeploymentApi::Chat, &endpoint);
    let chat = format!(
        r#"{{"messages":[{{"role":"user","content":"hi"}}],"model":"{ROUTE}","response_format":{{"type":"json_schema","json_schema":{{"name":"t","schema":{{"type":"object","properties":{{"zeta":{{"type":"string"}},"alpha":{{"type":"string"}}}},"required":["zeta","alpha"]}}}}}}}}"#
    );
    let (status, body) = send(&router, "/v1/chat/completions", chat.clone().into_bytes()).await;
    assert_eq!(status, 200, "{body}");
    // The caller's members first, as sent; what the collection adds follows.
    let forwarded = bodies.lock().unwrap()[0].clone();
    let caller = served(&chat);
    assert!(
        forwarded.starts_with(&caller[..caller.len() - 1]),
        "{forwarded}"
    );
    let forwarded: Value = serde_json::from_str(&forwarded).unwrap();
    assert_eq!(forwarded["stream"], true, "{forwarded}");
}

// T43 (ADR 0030 §2, SPEC §10): a request outside the decision contract is a
// 400 decided before any activation, lease or engine.
#[tokio::test]
async fn decision_requests_outside_the_contract_are_refused_before_activation() {
    let (endpoint, seen) =
        engine(Reply::Answer(200, "application/json", answer().to_string())).await;
    let (authority, _deps, router) = stack(DeploymentApi::Decisions, &endpoint);
    let with = |field: &str, value: Value| {
        let mut body = decision_body();
        body[field] = value;
        body.to_string().into_bytes()
    };
    let mut without_state = decision_body();
    without_state.as_object_mut().unwrap().remove("state");
    for body in [
        b"{not json".to_vec(),
        with("chat_template_kwargs", json!({"enable_thinking": false})),
        with("messages", json!([])),
        with("questions", json!({})),
        with(
            "questions",
            json!({"q": {"type": "maybe", "instructions": "x"}}),
        ),
        with(
            "questions",
            json!({"q": {"type": "choice", "criteria": ["a", "b"]}}),
        ),
        with("questions", json!({"q": {"type": "noul", "hint": "x"}})),
        with("images", json!([1])),
        without_state.to_string().into_bytes(),
    ] {
        let (status, answer) = send(&router, "/v1/systemone", body.clone()).await;
        assert_eq!(status, 400, "{}: {answer}", String::from_utf8_lossy(&body));
        assert_eq!(answer["code"], "invalid_request", "{answer}");
    }
    assert_eq!(authority.activations.load(Ordering::SeqCst), 0);
    assert!(authority.ends().is_empty());
    assert!(seen.lock().unwrap().is_empty());
}

// T43 (ADR 0030 §2): a decisions deployment refuses chat completions, and a
// chat deployment refuses decision requests, each with `api_mismatch` naming
// the endpoint to use, before anything activates, is leased or is sent.
#[tokio::test]
async fn the_mismatched_endpoint_is_refused_before_activation_both_ways() {
    let (endpoint, seen) =
        engine(Reply::Answer(200, "application/json", answer().to_string())).await;
    let (decisions, _deps, router) = stack(DeploymentApi::Decisions, &endpoint);
    for stream in [false, true] {
        let (status, answer) = send(
            &router,
            "/v1/chat/completions",
            chat_body(stream).to_string().into_bytes(),
        )
        .await;
        assert_eq!(status, 400, "{answer}");
        assert_eq!(answer["code"], "api_mismatch");
        assert!(
            answer["message"]
                .as_str()
                .unwrap()
                .contains("/v1/systemone"),
            "{answer}"
        );
    }
    assert_eq!(decisions.activations.load(Ordering::SeqCst), 0);
    assert!(decisions.ends().is_empty());

    let (chat, _deps, router) = stack(DeploymentApi::Chat, &endpoint);
    let (status, answer) = decide(&router, &decision_body()).await;
    assert_eq!(status, 400, "{answer}");
    assert_eq!(answer["code"], "api_mismatch");
    assert!(
        answer["message"]
            .as_str()
            .unwrap()
            .contains("/v1/chat/completions"),
        "{answer}"
    );
    assert_eq!(chat.activations.load(Ordering::SeqCst), 0);
    assert!(chat.ends().is_empty());
    assert!(seen.lock().unwrap().is_empty());
}

// T43 (ADR 0030 §2, SPEC §10): an engine's complete invalid-request answer
// (llama.cpp's 400, SGLang's 422 validation detail) is relayed with its status
// as `engine_rejected` and closes the lease as completed.
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
        let (authority, deps, router) = stack(DeploymentApi::Decisions, &endpoint);
        let (answered, answer) = decide(&router, &decision_body()).await;
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

// T43 T17 (ADR 0030 §2, SPEC §10): an answer that is not a whole, unambiguous
// JSON object with typed answers proves nothing about what the engine ran,
// so the lease stays charged as uncertain and nothing is replayed. An engine
// error answer whose body is cut short is no answer either.
#[tokio::test]
async fn an_unverified_decision_answer_leaves_the_lease_uncertain() {
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
        let (authority, deps, router) = stack(DeploymentApi::Decisions, &endpoint);
        let (status, answer) = decide(&router, &decision_body()).await;
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

/// llama.cpp v0.7.0's answer to a decision input over its batch size
/// (observed live 2026-10-17 on host A).
const LLAMA_TOO_LARGE: &str = r#"{"error":{"code":500,"message":"input (8195 tokens) is too large to process. increase the physical batch size","type":"server_error"}}"#;

// T43 T17 (SPEC §10, amended 2026-10-17, found live): an engine's complete
// error answer is relayed as a 502 `engine_error` with the engine's message,
// not retryable and never replayed; its lease is cancelling, charged until
// the engine reports quiescence, and the in-memory slot is released. Amended
// 2026-10-21: the complete answer counts as the engine's answer.
#[tokio::test]
async fn an_engine_error_answer_is_relayed_and_leaves_the_lease_cancelling() {
    let (endpoint, seen) = engine(Reply::Answer(
        500,
        "application/json",
        LLAMA_TOO_LARGE.into(),
    ))
    .await;
    let (authority, deps, router) = stack(DeploymentApi::Decisions, &endpoint);
    let (status, answer) = decide(&router, &decision_body()).await;
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

// T43 T19 (ADR 0030 §2, SPEC §10): a decision has no first-token event, so its
// whole answer is due by the request deadline whatever the idle bound; one
// cut there closed the engine connection and stays charged as cancelling.
// T17 (amended 2026-10-21): it was cut before its answer arrived, so it is
// unanswered and settles only at the instance's verified stop.
#[tokio::test]
async fn a_decision_is_bounded_by_the_request_deadline() {
    let (endpoint, _) = engine(Reply::Stall).await;
    let (authority, deps, router) = stack(DeploymentApi::Decisions, &endpoint);
    authority.ready.store(true, Ordering::SeqCst);
    deps.inflight
        .waiting
        .set_limits(capyctl_router::queue::WaitLimits {
            deadline: Duration::from_millis(300),
            stream_idle: Duration::from_secs(600),
            ..Default::default()
        });
    let (status, answer) =
        tokio::time::timeout(Duration::from_secs(10), decide(&router, &decision_body()))
            .await
            .expect("cut at the request deadline");
    assert_eq!(status, 500, "{answer}");
    assert_eq!(answer["code"], "engine_error");
    assert_eq!(
        authority.ends(),
        vec![Some(LeaseEnd::Cancelling { answered: false })]
    );
}

// T43 T17 (SPEC §10, amended 2026-10-21, owner decision for 0.1.7): a client
// that disconnects while its decision is being computed cancels the forward,
// so the engine sees its connection close within a bound. A decision comes
// whole, so it was never answered: it is held as cancelling until the
// instance's verified stop, and its in-flight slot is released.
#[tokio::test]
async fn a_hang_up_mid_decision_closes_the_engine_connection() {
    let engine = capyctl_testkit::HeldEngine::start().await;
    let (authority, deps, router) = stack(DeploymentApi::Decisions, &engine.endpoint());
    authority.ready.store(true, Ordering::SeqCst);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("http://{address}/v1/systemone"))
            .json(&decision_body())
            .send()
            .await
    });
    let bound = Duration::from_secs(5);
    assert!(
        engine.arrived_within(1, bound).await,
        "never reached the engine"
    );
    assert_eq!(engine.closed(), 0);
    client.abort();
    let _ = client.await;
    assert!(
        engine.closed_within(1, bound).await,
        "the engine connection stayed open after the hang-up"
    );
    let deadline = tokio::time::Instant::now() + bound;
    while authority.ends() != vec![Some(LeaseEnd::Cancelling { answered: false })]
        || deps.inflight.current(DEPLOYMENT) != 0
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{:?}",
            authority.ends()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// T43 (ADR 0030 §5): `/v1/models` names the API each model answers, read
// without waking anything.
#[tokio::test]
async fn models_name_each_api_without_waking() {
    for api in [DeploymentApi::Decisions, DeploymentApi::Chat] {
        let (authority, _deps, router) = stack(api, "http://127.0.0.1:9");
        let (status, listed) = send(&router, "/v1/models", Vec::new()).await;
        assert_eq!(status, 200, "{listed}");
        assert_eq!(listed["data"][0]["id"], ROUTE);
        assert_eq!(listed["data"][0]["object"], "model");
        assert_eq!(listed["data"][0]["capyctl"]["api"], api.name(), "{listed}");
        assert_eq!(authority.activations.load(Ordering::SeqCst), 0);
    }
}

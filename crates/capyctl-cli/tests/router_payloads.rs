//! SPEC §10 "Preserve supported payloads": a chat request's supported fields
//! reach the engine untouched through the router, the engine forwarder and a
//! host's inference ingress, and the engine's per-token `logprobs` come back
//! to the client in both streaming and collected (non-streaming) responses.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::{body::Body, http::Request, response::Response, routing::post, Router};
use capyctl_agent::ingress::{Ingress, IngressScope};
use capyctl_controller::Controller;
use capyctl_router::forwarders::StaticForwarders;
use capyctl_router::{QueueLimits, RouterDeps};
use capyctl_store::Store;
use capyctl_testkit::FakeEngine;
use serde_json::{json, Value};

const GATE: [u8; 32] = [1; 32];
const NATIVE: [u8; 32] = [2; 32];

fn hex(bytes: [u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

async fn serve(router: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    address
}

fn token_logprob(token: &str) -> Value {
    json!({
        "token": token,
        "logprob": -0.25,
        "bytes": token.as_bytes(),
        "top_logprobs": [
            {"token": token, "logprob": -0.25, "bytes": token.as_bytes()},
            {"token": "x", "logprob": -2.5, "bytes": [120]},
        ],
    })
}

fn chunk(delta: Value, logprobs: Value, finish: Value) -> String {
    let chunk = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "served",
        "system_fingerprint": "fp-1",
        "choices": [{
            "index": 0,
            "delta": delta,
            "logprobs": logprobs,
            "finish_reason": finish,
        }],
    });
    format!("data: {chunk}\n\n")
}

/// An engine that records every request body and answers with a stream whose
/// choices carry `logprobs`.
async fn engine(seen: Arc<Mutex<Vec<Value>>>) -> SocketAddr {
    let native = Router::new().route(
        "/v1/chat/completions",
        post(move |request: Request<Body>| {
            let seen = seen.clone();
            async move {
                let body = axum::body::to_bytes(request.into_body(), 1 << 20)
                    .await
                    .unwrap();
                seen.lock()
                    .unwrap()
                    .push(serde_json::from_slice(&body).unwrap());
                let stream = [
                    chunk(
                        json!({"role": "assistant", "content": ""}),
                        Value::Null,
                        Value::Null,
                    ),
                    chunk(
                        json!({"content": "Hel"}),
                        json!({"content": [token_logprob("Hel")]}),
                        Value::Null,
                    ),
                    chunk(
                        json!({"content": "lo"}),
                        json!({"content": [token_logprob("lo")]}),
                        json!("stop"),
                    ),
                    "data: [DONE]\n\n".to_string(),
                ]
                .concat();
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from(stream))
                    .unwrap()
            }
        }),
    );
    serve(native).await
}

/// Router → engine forwarder → host ingress → engine.
async fn stack(seen: Arc<Mutex<Vec<Value>>>) -> SocketAddr {
    let native = engine(seen).await;
    let ingress = Ingress::new().unwrap();
    let scope = IngressScope {
        host_id: "host".into(),
        deployment_id: "deployment".into(),
        binding_id: "binding-1".into(),
        incarnation: "incarnation-1".into(),
        member_id: "head".into(),
        generation: 1,
        revision: 1,
        instance_index: 0,
    };
    ingress
        .register(scope.clone(), native, "served".into(), GATE, NATIVE)
        .unwrap();
    ingress.open(&scope).unwrap();
    let ingress_address = serve(ingress.router()).await;

    let fake = Arc::new(FakeEngine::new());
    let controller = Arc::new(Controller::new(
        Arc::new(Mutex::new(Store::open_in_memory().unwrap())),
        fake.clone() as Arc<dyn capyctl_adapters::EngineAdapter>,
        Arc::new(capyctl_testkit::FakeLauncher::new()),
    ));
    let forward = capyctl_adapters::forward::engine_forwarder(
        format!("http://{ingress_address}").parse().unwrap(),
        "served".into(),
        Some(hex(GATE)),
        true,
    );
    let deps = RouterDeps {
        controller: controller.clone(),
        forwards: Arc::new(StaticForwarders(HashMap::from([(
            "fake".to_string(),
            forward,
        )]))),
        limits: QueueLimits {
            max_requests_per_deployment: 4,
            max_buffered_bytes_total: 1 << 20,
        },
        api_key: Some("test-key".into()),
        inflight: Arc::new(capyctl_router::admission::InFlight::default()),
        activation_join: Arc::new(capyctl_router::WakeJoin::new()),
    };
    let id = controller
        .submit_deploy(capyctl_controller::DeployRequest {
            name: "m1".into(),
            kind: "fake".into(),
            manifest: b"name: m1\nkind: fake\n".to_vec(),
            route_model_id: Some("m1".into()),
        })
        .await
        .unwrap();
    let op = controller
        .request_transition(&id, capyctl_domain::LifecycleAction::Start)
        .await
        .unwrap();
    controller.wait_terminal(&op).await.unwrap();
    serve(capyctl_router::serve_router(deps)).await
}

fn request(stream: bool) -> Value {
    json!({
        "model": "m1",
        "messages": [{"role": "user", "content": "hi"}],
        "stream": stream,
        "logprobs": true,
        "top_logprobs": 2,
        "temperature": 0.3,
        "top_p": 0.9,
        "max_tokens": 16,
        "seed": 7,
        "stop": ["\n\n"],
        "presence_penalty": 0.1,
        "frequency_penalty": 0.2,
        "logit_bias": {"50256": -100},
        "user": "caller-1",
        "response_format": {"type": "text"},
        "stream_options": {"include_usage": false},
    })
}

/// The engine sees the client's body with only the served model name and the
/// streaming transport substituted.
fn assert_forwarded_untouched(seen: &[Value], sent: &Value) {
    assert_eq!(seen.len(), 1);
    let mut expected = sent.clone();
    expected["model"] = json!("served");
    expected["stream"] = json!(true);
    assert_eq!(seen[0], expected);
}

// T37
#[tokio::test]
async fn collected_responses_keep_logprobs_and_requests_keep_supported_fields() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let router = stack(seen.clone()).await;
    let sent = request(false);
    let response = reqwest::Client::new()
        .post(format!("http://{router}/v1/chat/completions"))
        .bearer_auth("test-key")
        .json(&sent)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_forwarded_untouched(&seen.lock().unwrap(), &sent);
    assert_eq!(body["model"], "m1");
    assert_eq!(body["choices"][0]["message"]["content"], "Hello");
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert_eq!(
        body["choices"][0]["logprobs"],
        json!({"content": [token_logprob("Hel"), token_logprob("lo")]}),
        "{body}"
    );
}

// T37
#[tokio::test]
async fn streamed_responses_keep_logprobs_and_requests_keep_supported_fields() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let router = stack(seen.clone()).await;
    let sent = request(true);
    let response = reqwest::Client::new()
        .post(format!("http://{router}/v1/chat/completions"))
        .bearer_auth("test-key")
        .json(&sent)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert_forwarded_untouched(&seen.lock().unwrap(), &sent);
    let chunks: Vec<Value> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).unwrap())
        .collect();
    let logprobs: Vec<Value> = chunks
        .iter()
        .map(|chunk| chunk["choices"][0]["logprobs"].clone())
        .collect();
    assert_eq!(
        logprobs,
        vec![
            Value::Null,
            json!({"content": [token_logprob("Hel")]}),
            json!({"content": [token_logprob("lo")]}),
        ],
        "{text}"
    );
    assert!(chunks.iter().all(|chunk| chunk["model"] == "m1"));
    assert!(text.trim_end().ends_with("data: [DONE]"));
}

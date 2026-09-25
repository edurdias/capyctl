use axum::{
    body::Body,
    http::{Request, StatusCode},
    response::Response,
    routing::post,
    Router,
};
use mllm_agent::ingress::{Ingress, IngressScope};
use std::sync::{Arc, Mutex};
fn scope(generation: i64) -> IngressScope {
    IngressScope {
        host_id: "host".into(),
        deployment_id: "deployment".into(),
        binding_id: format!("binding-{generation}"),
        incarnation: format!("incarnation-{generation}"),
        member_id: "head".into(),
        generation,
        revision: generation,
        instance_index: 0,
    }
}
async fn serve(router: Router) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (address, task)
}
// T18, T21, T37: generation-bound inference authority never exposes engine administration.
#[tokio::test]
async fn ingress_requires_current_open_generation_and_replaces_internal_headers() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    let native = Router::new().route(
        "/v1/chat/completions",
        post(move |request: Request<Body>| {
            let seen = captured.clone();
            async move {
                seen.lock().unwrap().push(request.headers().clone());
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from("data: {\"ok\":true}\n\ndata: [DONE]\n\n"))
                    .unwrap()
            }
        }),
    );
    let (native_addr, native_task) = serve(native).await;
    let ingress = Ingress::new().unwrap();
    let first = scope(1);
    ingress
        .register(first.clone(), native_addr, "model".into(), [1; 32], [2; 32])
        .unwrap();
    let (address, task) = serve(ingress.clone().router()).await;
    let client = reqwest::Client::new();
    let send = |token: String, path: &str| {
        client
            .post(format!("http://{address}{path}"))
            .bearer_auth(token)
            .header("x-mllm-generation", "999")
            .header("x-mllm-admin-key", "must-not-forward")
            .json(&serde_json::json!({"model":"model","messages":[],"stream":true}))
            .send()
    };
    let token = hex::encode([1; 32]);
    assert_eq!(
        send(token.clone(), "/v1/chat/completions")
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    ingress.open(&first).unwrap();
    let response = send(token.clone(), "/v1/chat/completions").await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.text().await.unwrap().ends_with("data: [DONE]\n\n"));
    let headers = seen.lock().unwrap()[0].clone();
    assert_eq!(
        headers["authorization"],
        format!("Bearer {}", hex::encode([2; 32]))
    );
    assert!(!headers.contains_key("x-mllm-generation"));
    assert!(!headers.contains_key("x-mllm-admin-key"));
    // T21: vLLM's development controls are never reachable through ingress,
    // default-on deep parking included (ADR 0012).
    for path in [
        "/release_memory_occupation",
        "/sleep",
        "/wake_up",
        "/collective_rpc",
        "/v1/chat/completions?admin=true",
        "/%76%31/chat/completions",
    ] {
        assert!(!send(token.clone(), path)
            .await
            .unwrap()
            .status()
            .is_success());
    }
    let second = scope(2);
    ingress
        .register(
            second.clone(),
            native_addr,
            "model".into(),
            [3; 32],
            [4; 32],
        )
        .unwrap();
    assert!(ingress.open(&first).is_err());
    ingress.open(&second).unwrap();
    assert_eq!(
        send(token, "/v1/chat/completions").await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(seen.lock().unwrap().len(), 1);
    ingress.close(&second).unwrap();
    assert_eq!(
        send(hex::encode([3; 32]), "/v1/chat/completions")
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert!(ingress
        .register(
            scope(3),
            "192.0.2.1:8000".parse().unwrap(),
            "model".into(),
            [5; 32],
            [6; 32]
        )
        .is_err());
    task.abort();
    native_task.abort();
}

// T18, T38: a streamed response retains its forwarding slot after gate closure.
#[tokio::test]
async fn active_response_prevents_replacing_the_generation_until_body_drop() {
    use futures::StreamExt;
    let native = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            let stream = futures::stream::once(async {
                Ok::<_, std::convert::Infallible>(axum::body::Bytes::from_static(
                    b"data: first\n\n",
                ))
            })
            .chain(futures::stream::pending());
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }),
    );
    let (native_addr, native_task) = serve(native).await;
    let ingress = Ingress::new().unwrap();
    let first = scope(1);
    ingress
        .register(first.clone(), native_addr, "model".into(), [1; 32], [2; 32])
        .unwrap();
    ingress.open(&first).unwrap();
    let (address, task) = serve(ingress.clone().router()).await;
    let mut response = reqwest::Client::new()
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(hex::encode([1; 32]))
        .json(&serde_json::json!({"model":"model","messages":[],"stream":true}))
        .send()
        .await
        .unwrap();
    assert!(response.chunk().await.unwrap().is_some());
    ingress.close(&first).unwrap();
    assert_eq!(ingress.current_requests(&first).unwrap(), 1);
    assert!(ingress
        .register(scope(2), native_addr, "model".into(), [3; 32], [4; 32])
        .is_err());
    drop(response);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while ingress.current_requests(&first).unwrap() != 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    ingress
        .register(scope(2), native_addr, "model".into(), [3; 32], [4; 32])
        .unwrap();
    task.abort();
    native_task.abort();
}

// T16 T18 T20 (U5 recovery live run, host-a): a failed launch keeps its
// generation, so the retry registers the same deployment member at the same
// generation under a new binding. The terminated launch's entry outlived it and
// the retry was refused as stale, so no engine ever started. Once the host has
// gone evidence the entry is retired and the retry registers; a live entry, an
// open gate or a mismatched scope is never retired.
#[test]
fn a_terminated_launch_entry_is_retired_so_the_same_generation_can_launch_again() {
    let ingress = Ingress::new().unwrap();
    let target: std::net::SocketAddr = "127.0.0.1:8100".parse().unwrap();
    let failed = IngressScope {
        binding_id: "failed-binding".into(),
        incarnation: "failed".into(),
        ..scope(1)
    };
    let retry = IngressScope {
        binding_id: "retry-binding".into(),
        incarnation: "retry".into(),
        ..scope(1)
    };
    ingress
        .register(failed.clone(), target, "model".into(), [1; 32], [2; 32])
        .unwrap();
    // The live failure: the retry at the same generation is refused.
    assert!(ingress
        .register(retry.clone(), target, "model".into(), [3; 32], [4; 32])
        .is_err());
    // An open gate or another scope never retires the entry.
    ingress.open(&failed).unwrap();
    assert!(!ingress.retire(&failed).unwrap());
    ingress.close(&failed).unwrap();
    assert!(!ingress.retire(&retry).unwrap());
    // Gone evidence retires the closed, idle entry; the retry now registers.
    assert!(ingress.retire(&failed).unwrap());
    assert!(!ingress.retire(&failed).unwrap(), "retiring is idempotent");
    ingress
        .register(retry.clone(), target, "model".into(), [5; 32], [6; 32])
        .unwrap();
    // The retired launch cannot come back.
    assert!(ingress.open(&failed).is_err());
    // Nor can it re-register over the retry.
    assert!(ingress
        .register(failed, target, "model".into(), [1; 32], [7; 32])
        .is_err());
}

/// A native endpoint that answers every chat with its own name, so a test can
/// tell which instance a request reached.
async fn named_native(name: &'static str) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    serve(Router::new().route(
        "/v1/chat/completions",
        post(move || async move {
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(format!("{{\"instance\":\"{name}\"}}")))
                .unwrap()
        }),
    ))
    .await
}

// T24 T34 T38 (ADR 0013 §5, ADR 0015 §2): two instances of one deployment on
// one host hold separate gates. The later instance registers at a newer
// generation without displacing its Ready sibling; each gate forwards only to
// its own engine; closing or retiring one leaves the other serving; and a
// stale generation of either instance is still refused.
#[tokio::test]
async fn two_instances_of_one_deployment_hold_separate_gates_on_one_host() {
    let (first_native, first_task) = named_native("zero").await;
    let (second_native, second_task) = named_native("one").await;
    let ingress = Ingress::new().unwrap();
    let zero = scope(1);
    let one = IngressScope {
        binding_id: "binding-one".into(),
        incarnation: "incarnation-one".into(),
        generation: 2,
        instance_index: 1,
        ..scope(1)
    };
    ingress
        .register(zero.clone(), first_native, "model".into(), [1; 32], [2; 32])
        .unwrap();
    ingress.open(&zero).unwrap();
    // The sibling's newer generation registers beside, not over, instance 0.
    ingress
        .register(one.clone(), second_native, "model".into(), [3; 32], [4; 32])
        .unwrap();
    ingress.bind_handle(&zero, "launch-zero").unwrap();
    ingress.bind_handle(&one, "launch-one").unwrap();
    ingress.open(&one).unwrap();
    let (address, task) = serve(ingress.clone().router()).await;
    let client = reqwest::Client::new();
    let ask = |gate: [u8; 32]| {
        client
            .post(format!("http://{address}/v1/chat/completions"))
            .bearer_auth(hex::encode(gate))
            .json(&serde_json::json!({"model":"model","messages":[]}))
            .send()
    };
    let answered = |response: reqwest::Response| async move {
        assert_eq!(response.status(), StatusCode::OK);
        response.text().await.unwrap()
    };
    assert_eq!(
        answered(ask([1; 32]).await.unwrap()).await,
        "{\"instance\":\"zero\"}"
    );
    assert_eq!(
        answered(ask([3; 32]).await.unwrap()).await,
        "{\"instance\":\"one\"}"
    );
    // W8: both Ready gates report load under their own launch.
    let mut handles: Vec<_> = ingress
        .load_targets()
        .unwrap()
        .into_iter()
        .map(|t| (t.scope.instance_index, t.owned_handle))
        .collect();
    handles.sort();
    assert_eq!(
        handles,
        vec![(0, "launch-zero".to_owned()), (1, "launch-one".to_owned())]
    );
    // T34: an older generation of instance 1 is refused as stale; so is a
    // replay of instance 0 at its own generation with another gate.
    let stale = IngressScope {
        binding_id: "binding-stale".into(),
        incarnation: "incarnation-stale".into(),
        generation: 1,
        ..one.clone()
    };
    ingress.close(&one).unwrap();
    assert!(ingress
        .register(stale, second_native, "model".into(), [5; 32], [6; 32])
        .is_err());
    let replay = IngressScope {
        binding_id: "binding-replay".into(),
        ..zero.clone()
    };
    assert!(ingress
        .register(replay, first_native, "model".into(), [7; 32], [8; 32])
        .is_err());
    // T38: instance 1's closed gate fails honestly; instance 0 still serves.
    assert_eq!(ask([3; 32]).await.unwrap().status(), StatusCode::FORBIDDEN);
    assert_eq!(
        answered(ask([1; 32]).await.unwrap()).await,
        "{\"instance\":\"zero\"}"
    );
    // Retiring instance 1's exact entry never touches instance 0's.
    assert!(ingress.retire(&one).unwrap());
    assert!(
        !ingress.retire(&zero).unwrap(),
        "an open gate is never retired"
    );
    assert_eq!(ingress.current_requests(&zero).unwrap(), 0);
    assert!(ingress.current_requests(&one).is_err());
    assert_eq!(
        answered(ask([1; 32]).await.unwrap()).await,
        "{\"instance\":\"zero\"}"
    );
    task.abort();
    first_task.abort();
    second_task.abort();
}

// T18 T21: SPEC §§6, 13.3. A spent gate key is never accepted for another
// scope while it is remembered, but the memory of retired launches is bounded:
// a long-lived host keeps registering new launches after thousands of retired
// ones instead of refusing every launch once the spent set fills.
#[test]
fn retired_gate_keys_do_not_exhaust_registration() {
    let ingress = Ingress::new().unwrap();
    let target: std::net::SocketAddr = "127.0.0.1:9".parse().unwrap();
    let gate = |n: u32| {
        let mut key = [7u8; 32];
        key[..4].copy_from_slice(&n.to_be_bytes());
        key
    };
    for n in 1..=5000u32 {
        let scope = scope(i64::from(n));
        ingress
            .register(scope.clone(), target, "model".into(), gate(n), [2; 32])
            .unwrap_or_else(|_| panic!("registration {n} refused"));
        assert!(ingress.retire(&scope).unwrap());
    }
    // The live entry's key and the most recent spent keys stay refused for any
    // other scope.
    let live = scope(6000);
    ingress
        .register(live.clone(), target, "model".into(), gate(6000), [2; 32])
        .unwrap();
    let mut other = scope(6001);
    other.deployment_id = "other".into();
    assert!(ingress
        .register(other.clone(), target, "model".into(), gate(6000), [2; 32])
        .is_err());
    assert!(ingress
        .register(other, target, "model".into(), gate(5000), [2; 32])
        .is_err());
}

// T19 T21: SPEC §10. Host ingress forwards the supported chat payload and
// refuses engine-internal request fields before anything reaches the engine.
#[tokio::test]
async fn engine_internal_request_fields_never_reach_the_engine() {
    let hits = Arc::new(Mutex::new(0usize));
    let counted = hits.clone();
    let native = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let hits = counted.clone();
            async move {
                *hits.lock().unwrap() += 1;
                Response::builder()
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap()
            }
        }),
    );
    let (native_addr, native_task) = serve(native).await;
    let ingress = Ingress::new().unwrap();
    let live = scope(1);
    ingress
        .register(live.clone(), native_addr, "model".into(), [1; 32], [2; 32])
        .unwrap();
    ingress.open(&live).unwrap();
    let (address, task) = serve(ingress.clone().router()).await;
    let post = |body: serde_json::Value| async move {
        reqwest::Client::new()
            .post(format!("http://{address}/v1/chat/completions"))
            .bearer_auth(hex::encode([1; 32]))
            .json(&body)
            .send()
            .await
            .unwrap()
            .status()
    };
    for field in [
        "rid",
        "lora_path",
        "return_hidden_states",
        "custom_logit_processor",
        "bootstrap_room",
        "kv_transfer_params",
        "vllm_xargs",
        "priority",
    ] {
        let mut body = serde_json::json!({"model":"model","messages":[]});
        body[field] = serde_json::json!(1);
        assert_eq!(
            post(body).await,
            reqwest::StatusCode::BAD_REQUEST,
            "{field}"
        );
    }
    assert_eq!(*hits.lock().unwrap(), 0);
    let supported = serde_json::json!({"model":"model","messages":[],"tools":[],
        "tool_choice":"auto","response_format":{"type":"json_object"},
        "chat_template_kwargs":{"enable_thinking":false}});
    assert_eq!(post(supported).await, reqwest::StatusCode::OK);
    assert_eq!(*hits.lock().unwrap(), 1);
    task.abort();
    native_task.abort();
}

/// A native endpoint that answers every chat with one fixed status and body.
async fn answering_native(
    status: u16,
    content_type: &'static str,
    body: &'static str,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    serve(Router::new().route(
        "/v1/chat/completions",
        post(move || async move {
            Response::builder()
                .status(status)
                .header("content-type", content_type)
                .body(Body::from(body))
                .unwrap()
        }),
    ))
    .await
}

// T19 T17 (SPEC §10, found live 2026-09-24): an engine that rejects a request
// as invalid (a prompt over its context, a tool choice it was not launched
// for) has answered it completely; nothing runs on its behalf. The ingress
// relays that rejection as `engine_rejected` with the engine's own message,
// instead of flattening it into a 502 the router must treat as uncertain.
// Any other engine error, or a rejection whose body cannot be read as JSON,
// stays a 502.
#[tokio::test]
async fn an_engine_rejection_is_relayed_and_other_engine_errors_stay_bad_gateway() {
    let cases: [(u16, &'static str, &'static str, u16); 5] = [
        (
            400,
            "application/json",
            r#"{"error":{"message":"This model's maximum context length is 16384 tokens.","type":"BadRequestError","code":400}}"#,
            400,
        ),
        (
            422,
            "application/json",
            r#"{"object":"error","message":"tool_choice requires a parser","code":422}"#,
            422,
        ),
        (400, "text/plain", "bad", 502),
        (
            500,
            "application/json",
            r#"{"error":{"message":"boom"}}"#,
            502,
        ),
        (
            401,
            "application/json",
            r#"{"error":{"message":"Unauthorized"}}"#,
            502,
        ),
    ];
    for (engine_status, content_type, body, want) in cases {
        let (native, native_task) = answering_native(engine_status, content_type, body).await;
        let ingress = Ingress::new().unwrap();
        let first = scope(1);
        ingress
            .register(first.clone(), native, "model".into(), [1; 32], [2; 32])
            .unwrap();
        ingress.open(&first).unwrap();
        let (address, task) = serve(ingress.clone().router()).await;
        let response = reqwest::Client::new()
            .post(format!("http://{address}/v1/chat/completions"))
            .bearer_auth(hex::encode([1; 32]))
            .json(&serde_json::json!({"model":"model","messages":[],"stream":true}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status().as_u16(),
            want,
            "engine {engine_status} {body}"
        );
        let answer: serde_json::Value = response.json().await.unwrap();
        if want == 502 {
            assert_eq!(answer["error"]["code"], "inference_unavailable");
        } else {
            assert_eq!(answer["error"]["code"], "engine_rejected", "{answer}");
            let message = answer["error"]["message"].as_str().unwrap();
            assert!(
                message.contains("maximum context length") || message.contains("tool_choice"),
                "{answer}"
            );
        }
        task.abort();
        native_task.abort();
    }
}

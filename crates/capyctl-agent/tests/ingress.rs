use axum::{
    body::Body,
    http::{Request, StatusCode},
    response::Response,
    routing::post,
    Router,
};
use capyctl_agent::ingress::{EngineAccess, Ingress, IngressScope};
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
            .header("x-capyctl-generation", "999")
            .header("x-capyctl-admin-key", "must-not-forward")
            // SPEC §10 (owner decision 2026-10-19): request IDs are CapyCTL's;
            // neither reaches the engine.
            .header("x-request-id", "caller-1")
            .header("x-capyctl-request-id", "01JAAAAAAAAAAAAAAAAAAAAAAA")
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
    assert!(!headers.contains_key("x-capyctl-generation"));
    assert!(!headers.contains_key("x-capyctl-admin-key"));
    assert!(!headers.contains_key("x-request-id"));
    assert!(!headers.contains_key("x-capyctl-request-id"));
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
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
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
    ingress
        .bind_handle(&zero, "launch-zero", EngineAccess::builtin())
        .unwrap();
    ingress
        .bind_handle(&one, "launch-one", EngineAccess::builtin())
        .unwrap();
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
// SPEC §10 (amended 2026-10-17, found live): an engine's complete error
// answer (a 5xx but 503 with a JSON object body, llama.cpp's 500 for an input
// over its batch size) is relayed the same way as `engine_error`. Any other
// engine status, or an answer whose body cannot be read as a JSON object,
// stays a 502 of the ingress's own.
#[tokio::test]
async fn engine_answers_are_relayed_and_other_engine_errors_stay_bad_gateway() {
    let cases = [
        (
            400,
            "application/json",
            r#"{"error":{"message":"This model's maximum context length is 16384 tokens.","type":"BadRequestError","code":400}}"#,
            400,
            "engine_rejected",
            "maximum context length",
        ),
        (
            422,
            "application/json",
            r#"{"object":"error","message":"tool_choice requires a parser","code":422}"#,
            422,
            "engine_rejected",
            "tool_choice",
        ),
        (400, "text/plain", "bad", 502, "inference_unavailable", ""),
        (
            500,
            "application/json",
            r#"{"error":{"code":500,"message":"input (8195 tokens) is too large to process. increase the physical batch size","type":"server_error"}}"#,
            500,
            "engine_error",
            "too large to process",
        ),
        (
            504,
            "application/json",
            r#"{"message":"upstream timed out"}"#,
            504,
            "engine_error",
            "upstream timed out",
        ),
        (500, "text/plain", "boom", 502, "inference_unavailable", ""),
        (
            503,
            "application/json",
            r#"{"error":{"message":"loading model"}}"#,
            502,
            "inference_unavailable",
            "",
        ),
        (
            401,
            "application/json",
            r#"{"error":{"message":"Unauthorized"}}"#,
            502,
            "inference_unavailable",
            "",
        ),
    ];
    for (engine_status, content_type, body, want, code, needle) in cases {
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
        assert_eq!(answer["error"]["code"], code, "{answer}");
        let message = answer["error"]["message"].as_str().unwrap();
        assert!(message.contains(needle), "{answer}");
        task.abort();
        native_task.abort();
    }
}

// T43 T21 (ADR 0030 §2, SPEC §10): the ingress forwards exactly
// `POST /v1/systemone` besides chat completions, each with its own body
// contract and fixed upstream path, for the registered served name only. A
// decision body on the chat path, a chat body on the decision path, another
// model, or any other path never reaches the engine; an engine's invalid
// decision is relayed as `engine_rejected`.
#[tokio::test]
async fn decision_requests_pass_only_on_their_own_path_and_contract() {
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let recorded = seen.clone();
    let native = Router::new().fallback(move |request: Request<Body>| {
        let seen = recorded.clone();
        async move {
            let path = request.uri().path().to_owned();
            let body = axum::body::to_bytes(request.into_body(), 1 << 20)
                .await
                .unwrap();
            let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
            seen.lock().unwrap().push(path);
            let (status, answer) = if value["state"] == "reject me" {
                (400, r#"{"error":{"code":400,"message":"questions.q: \"instructions\" must be provided","type":"invalid_request_error"}}"#)
            } else {
                (200, r#"{"model":"model","answers":{"q":{"type":"noul","noul":0.6}},"usage":{"input_tokens":9,"output_tokens":0}}"#)
            };
            Response::builder()
                .status(status)
                .header("content-type", "application/json")
                .body(Body::from(answer))
                .unwrap()
        }
    });
    let (native_addr, native_task) = serve(native).await;
    let ingress = Ingress::new().unwrap();
    let live = scope(1);
    ingress
        .register(live.clone(), native_addr, "model".into(), [1; 32], [2; 32])
        .unwrap();
    ingress.open(&live).unwrap();
    let (address, task) = serve(ingress.clone().router()).await;
    let post = |path: &'static str, body: serde_json::Value| async move {
        reqwest::Client::new()
            .post(format!("http://{address}{path}"))
            .bearer_auth(hex::encode([1; 32]))
            .json(&body)
            .send()
            .await
            .unwrap()
    };
    let decision = |model: &str, state: &str| {
        serde_json::json!({"model": model, "state": state,
            "questions": {"q": {"type": "noul", "instructions": "Angry?"}}})
    };
    let response = post("/v1/systemone", decision("model", "hi")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let answer: serde_json::Value = response.json().await.unwrap();
    assert_eq!(answer["answers"]["q"]["noul"], 0.6);
    assert_eq!(seen.lock().unwrap().as_slice(), ["/v1/systemone"]);

    let chat = serde_json::json!({"model": "model", "messages": []});
    let mut extended = decision("model", "hi");
    extended["chat_template_kwargs"] = serde_json::json!({});
    for (path, body) in [
        ("/v1/systemone", decision("other", "hi")),
        ("/v1/systemone", chat.clone()),
        ("/v1/systemone", extended),
        ("/v1/chat/completions", decision("model", "hi")),
    ] {
        assert_eq!(
            post(path, body.clone()).await.status(),
            StatusCode::BAD_REQUEST,
            "{path} {body}"
        );
    }
    for path in [
        "/v1/systemone/",
        "/v1/systemone?stream=true",
        "/v1/completions",
    ] {
        assert!(
            !post(path, decision("model", "hi"))
                .await
                .status()
                .is_success(),
            "{path}"
        );
    }
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "nothing else reached the engine"
    );

    let response = post("/v1/systemone", decision("model", "reject me")).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let answer: serde_json::Value = response.json().await.unwrap();
    assert_eq!(answer["error"]["code"], "engine_rejected", "{answer}");
    assert!(answer["error"]["message"]
        .as_str()
        .unwrap()
        .contains("must be provided"));
    task.abort();
    native_task.abort();
}

/// What a recording engine saw: each request's path and its
/// `Authorization` header, if any.
type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// A fake engine that answers a chat completion as a stream and any other
/// read with an idle gauge, recording every request's path and header.
async fn recording_engine() -> (std::net::SocketAddr, Seen, tokio::task::JoinHandle<()>) {
    let seen: Seen = Arc::default();
    let captured = seen.clone();
    let engine = Router::new().fallback(move |request: Request<Body>| {
        let seen = captured.clone();
        async move {
            let path = request.uri().path().to_owned();
            let authorization = request
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            seen.lock().unwrap().push((path.clone(), authorization));
            if path == "/v1/chat/completions" {
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from("data: [DONE]\n\n"))
                    .unwrap()
            } else {
                Response::builder()
                    .body(Body::from("requests_running 0\n"))
                    .unwrap()
            }
        }
    });
    let (address, task) = serve(engine).await;
    (address, seen, task)
}

const KEYED_MANIFEST: &str = "kind: custom_engine\nschema_version: 1\napi: chat\n\
    args: [--host, \"{host}\", --port, \"{port}\", --model, \"{model_dir}\"]\n\
    env: {SERVE_AUTH: \"{api_key}\"}\n\
    idle: {url: /idle, gauges: [requests_running]}\n";
const UNKEYED_MANIFEST: &str = "kind: custom_engine\nschema_version: 1\napi: chat\n\
    args: [--host, \"{host}\", --port, \"{port}\", --model, \"{model_dir}\"]\n\
    idle: {url: /idle, gauges: [requests_running]}\n";

// T44 T13 T14 (ADR 0031 §7, amendment A1): the launch's native key is sent
// only to an engine that takes it. A built-in engine and a custom engine
// whose manifest uses `{api_key}` receive `Bearer <key>` on forwarded
// requests and on the load reporter's loopback reads; a custom engine whose
// manifest does not receives no `Authorization` header on either. No
// assertion message carries a key.
#[tokio::test]
async fn the_native_key_reaches_only_an_engine_that_takes_it() {
    use capyctl_config::custom::{CustomManifest, CustomProfile};
    let custom = |manifest: &str| CustomProfile::new(CustomManifest::parse(manifest).unwrap());
    let ingress = Ingress::new().unwrap();
    let mut engines = Vec::new();
    for (name, gate, native, access, read_path) in [
        ("builtin", 11u8, 21u8, EngineAccess::builtin(), "/metrics"),
        (
            "keyed",
            12,
            22,
            EngineAccess::custom(&custom(KEYED_MANIFEST)),
            "/idle",
        ),
        (
            "unkeyed",
            13,
            23,
            EngineAccess::custom(&custom(UNKEYED_MANIFEST)),
            "/idle",
        ),
    ] {
        let (target, seen, engine_task) = recording_engine().await;
        let launch = IngressScope {
            deployment_id: name.into(),
            ..scope(1)
        };
        ingress
            .register(
                launch.clone(),
                target,
                "model".into(),
                [gate; 32],
                [native; 32],
            )
            .unwrap();
        ingress
            .bind_handle(&launch, &format!("launch-{name}"), access)
            .unwrap();
        ingress.open(&launch).unwrap();
        engines.push((name, gate, native, read_path, seen, engine_task));
    }
    let (address, task) = serve(ingress.clone().router()).await;
    let client = reqwest::Client::new();
    for (name, gate, ..) in &engines {
        let response = client
            .post(format!("http://{address}/v1/chat/completions"))
            .bearer_auth(hex::encode([*gate; 32]))
            .json(&serde_json::json!({"model":"model","messages":[],"stream":true}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{name}");
        assert!(response.text().await.unwrap().ends_with("data: [DONE]\n\n"));
    }
    let reporter = capyctl_agent::load::LoadReporter::new(ingress.clone(), "host".into()).unwrap();
    assert!(!reporter.reports().await.is_empty());
    for (name, _, native, read_path, seen, engine_task) in engines {
        let expected = format!("Bearer {}", hex::encode([native; 32]));
        let seen = seen.lock().unwrap().clone();
        let paths: Vec<_> = seen.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(paths, ["/v1/chat/completions", read_path], "{name}");
        for (path, authorization) in &seen {
            if name == "unkeyed" {
                assert!(
                    authorization.is_none(),
                    "{name}: {path} carried an Authorization header"
                );
            } else {
                assert!(
                    authorization.as_deref() == Some(expected.as_str()),
                    "{name}: {path} did not carry the launch's own key"
                );
            }
        }
        engine_task.abort();
    }
    task.abort();
}

// T17 T38 T43 (SPEC §10, amended 2026-10-21, owner decision for 0.1.7): when
// the router's connection to the ingress goes away in the middle of a
// forwarded request (the router cancelled it: its client hung up or its
// deadline cut it), the ingress drops its engine connection within a bound
// and the request's forwarding slot is freed. A chat request before the
// engine's response head and after a head and an opening chunk, and a
// decision, whose answer comes whole.
#[tokio::test]
async fn the_engine_connection_closes_when_the_upstream_goes_away() {
    let chat = serde_json::json!({"model":"model","messages":[],"stream":true});
    let decision = serde_json::json!({"model": "model", "state": "hi",
        "questions": {"q": {"type": "noul", "instructions": "Angry?"}}});
    for (path, body, opening) in [
        ("/v1/chat/completions", chat.clone(), false),
        ("/v1/chat/completions", chat, true),
        ("/v1/systemone", decision, false),
    ] {
        let engine = if opening {
            capyctl_testkit::HeldEngine::opening().await
        } else {
            capyctl_testkit::HeldEngine::start().await
        };
        let ingress = Ingress::new().unwrap();
        let live = scope(1);
        ingress
            .register(
                live.clone(),
                engine.address(),
                "model".into(),
                [1; 32],
                [2; 32],
            )
            .unwrap();
        ingress.open(&live).unwrap();
        let (address, task) = serve(ingress.clone().router()).await;
        let mut upstream = reqwest::Client::new()
            .post(format!("http://{address}{path}"))
            .bearer_auth(hex::encode([1; 32]))
            .json(&body)
            .send();
        let bound = std::time::Duration::from_secs(5);
        // Drive the upstream request until the engine has it.
        let response = tokio::select! {
            response = &mut upstream => Some(response.unwrap()),
            arrived = engine.arrived_within(1, bound) => {
                assert!(arrived, "{path}: never reached the engine");
                None
            }
        };
        let mut body = match response {
            Some(response) => {
                assert!(opening, "{path}: answered without an engine answer");
                assert_eq!(response.status(), StatusCode::OK);
                Some(response)
            }
            None if opening => {
                let mut response = (&mut upstream).await.unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                assert!(response.chunk().await.unwrap().is_some());
                Some(response)
            }
            None => None,
        };
        assert_eq!(engine.closed(), 0, "{path} (opening: {opening})");
        assert_eq!(ingress.current_requests(&live).unwrap(), 1);
        // The router goes away with its connection.
        drop(body.take());
        drop(upstream);
        assert!(
            engine.closed_within(1, bound).await,
            "{path} (opening: {opening}): the engine connection stayed open"
        );
        tokio::time::timeout(bound, async {
            while ingress.current_requests(&live).unwrap() != 0 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the forwarding slot is freed");
        task.abort();
    }
}

// T17 (ADR 0032 §1, §2): the router's CapyCTL request ID is consumed by the
// ingress. On a launch whose engine takes it, a chat request reaches the
// engine with it as `rid` in the body; a launch without the capability, a
// decision, and a request without the header get none. Neither ID ever
// reaches the engine as a header. A malformed or repeated ID is refused
// (400) before the engine is reached, and a caller's own `rid` stays
// refused on a capable launch.
#[tokio::test]
async fn the_capyctl_request_id_becomes_rid_only_on_a_capable_chat_launch() {
    type Seen = Arc<Mutex<Vec<(String, axum::http::HeaderMap, serde_json::Value)>>>;
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = seen.clone();
    let native = Router::new().fallback(move |request: Request<Body>| {
        let seen = recorded.clone();
        async move {
            let path = request.uri().path().to_owned();
            let headers = request.headers().clone();
            let body = axum::body::to_bytes(request.into_body(), 1 << 20)
                .await
                .unwrap();
            let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
            seen.lock().unwrap().push((path.clone(), headers, value));
            let answer = if path == "/v1/systemone" {
                r#"{"model":"model","answers":{"q":{"type":"noul","noul":0.6}}}"#
            } else {
                "{}"
            };
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(answer))
                .unwrap()
        }
    });
    let (native_addr, native_task) = serve(native).await;
    let ingress = Ingress::new().unwrap();
    let capable = IngressScope {
        deployment_id: "capable".into(),
        ..scope(1)
    };
    let plain = IngressScope {
        deployment_id: "plain".into(),
        ..scope(1)
    };
    ingress
        .register(
            capable.clone(),
            native_addr,
            "model".into(),
            [1; 32],
            [2; 32],
        )
        .unwrap();
    ingress
        .bind_handle(
            &capable,
            "launch-capable",
            EngineAccess::builtin().with_request_abort(true),
        )
        .unwrap();
    ingress.open(&capable).unwrap();
    ingress
        .register(plain.clone(), native_addr, "model".into(), [3; 32], [4; 32])
        .unwrap();
    ingress
        .bind_handle(&plain, "launch-plain", EngineAccess::builtin())
        .unwrap();
    ingress.open(&plain).unwrap();
    let (address, task) = serve(ingress.clone().router()).await;
    const ID: &str = "01JAAAAAAAAAAAAAAAAAAAAAAA";
    let post = |gate: u8, path: &'static str, ids: Vec<&'static str>, body: serde_json::Value| async move {
        let mut request = reqwest::Client::new()
            .post(format!("http://{address}{path}"))
            .bearer_auth(hex::encode([gate; 32]))
            .header("x-request-id", "caller-1");
        for id in ids {
            request = request.header("x-capyctl-request-id", id);
        }
        request.json(&body).send().await.unwrap().status()
    };
    let chat = serde_json::json!({"model":"model","messages":[],"stream":true});
    let decision = serde_json::json!({"model": "model", "state": "hi",
        "questions": {"q": {"type": "noul", "instructions": "Angry?"}}});
    let take = || std::mem::take(&mut *seen.lock().unwrap());
    let no_id_headers = |headers: &axum::http::HeaderMap| {
        !headers.contains_key("x-request-id") && !headers.contains_key("x-capyctl-request-id")
    };

    // A capable launch: the ID becomes `rid`, the rest of the body unchanged.
    assert_eq!(
        post(1, "/v1/chat/completions", vec![ID], chat.clone()).await,
        StatusCode::OK
    );
    let forwarded = take();
    assert_eq!(forwarded.len(), 1);
    let (path, headers, body) = &forwarded[0];
    assert_eq!(path, "/v1/chat/completions");
    assert!(no_id_headers(headers), "{headers:?}");
    let mut expected = chat.clone();
    expected["rid"] = serde_json::json!(ID);
    assert_eq!(*body, expected);

    // No header: nothing is set.
    assert_eq!(
        post(1, "/v1/chat/completions", vec![], chat.clone()).await,
        StatusCode::OK
    );
    let (_, headers, body) = take().remove(0);
    assert!(no_id_headers(&headers));
    assert_eq!(body, chat);

    // A launch without the capability: dropped.
    assert_eq!(
        post(3, "/v1/chat/completions", vec![ID], chat.clone()).await,
        StatusCode::OK
    );
    let (_, headers, body) = take().remove(0);
    assert!(no_id_headers(&headers));
    assert_eq!(body, chat);

    // A decision, even on a capable launch: dropped.
    assert_eq!(
        post(1, "/v1/systemone", vec![ID], decision.clone()).await,
        StatusCode::OK
    );
    let (path, headers, body) = take().remove(0);
    assert_eq!(path, "/v1/systemone");
    assert!(no_id_headers(&headers));
    assert_eq!(body, decision);

    // A malformed or repeated ID is refused before the engine, on either
    // launch and either path.
    for (gate, path, ids, body) in [
        (1, "/v1/chat/completions", vec!["caller-1"], chat.clone()),
        (
            1,
            "/v1/chat/completions",
            vec!["01jaaaaaaaaaaaaaaaaaaaaaaa"],
            chat.clone(),
        ),
        (
            1,
            "/v1/chat/completions",
            vec!["01JAAAAAAAAAAAAAAAAAAAAAA"],
            chat.clone(),
        ),
        (1, "/v1/chat/completions", vec![ID, ID], chat.clone()),
        (3, "/v1/chat/completions", vec!["not-a-ulid"], chat.clone()),
        (1, "/v1/systemone", vec!["not-a-ulid"], decision.clone()),
    ] {
        assert_eq!(
            post(gate, path, ids.clone(), body).await,
            StatusCode::BAD_REQUEST,
            "{gate} {path} {ids:?}"
        );
    }
    // A caller's own `rid` is still refused on a capable launch, with or
    // without CapyCTL's ID.
    let mut with_rid = chat.clone();
    with_rid["rid"] = serde_json::json!(ID);
    for ids in [vec![ID], vec![]] {
        assert_eq!(
            post(1, "/v1/chat/completions", ids, with_rid.clone()).await,
            StatusCode::BAD_REQUEST
        );
    }
    assert!(take().is_empty());
    task.abort();
    native_task.abort();
}

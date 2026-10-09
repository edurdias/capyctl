//! ADR 0028 §9 (decided 2026-10-06): the completion probe, one per engine.
//! Each adapter asks its engine for the generated token ids in its own request
//! form, on the launch's own endpoint with its own key, and answers them with
//! the generated text (owner decision 2026-10-09); an answer with neither is
//! a failed probe. Against local stand-ins only: nothing here shows that a
//! native engine answers in this form (the live MN rows do).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::IntoResponse,
    Router,
};
use capyctl_adapters::completion_probe::{ProbeAnswer, PROMPT};
use capyctl_adapters::sglang::SglangAdapter;
use capyctl_adapters::tensorfold::TensorfoldAdapter;
use capyctl_adapters::traits::ChatForward;
use capyctl_adapters::vllm::VllmAdapter;
use capyctl_adapters::ParkPolicy;
use capyctl_domain::launch::{NativeLaunch, NativeLaunchMetadata};
use serde_json::{json, Value};

const BOUND: Duration = Duration::from_secs(5);

/// One request the stand-in saw: path, authorization and body.
type Request = (String, Option<String>, Value);

/// What the stand-in engine was asked.
#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<Request>>>);

/// A stand-in that records every POST and answers `answer`.
async fn engine(answer: Value) -> (String, Seen) {
    let seen = Seen::default();
    let app = Router::new()
        .fallback(
            |State((seen, answer)): State<(Seen, Value)>,
             uri: Uri,
             headers: HeaderMap,
             body: Bytes| async move {
                seen.0.lock().unwrap().push((
                    uri.path().to_owned(),
                    headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned),
                    serde_json::from_slice(&body).unwrap_or(Value::Null),
                ));
                (StatusCode::OK, axum::Json(answer)).into_response()
            },
        )
        .with_state((seen.clone(), answer));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (endpoint, seen)
}

fn sglang(endpoint: String) -> SglangAdapter {
    SglangAdapter::from_frozen(
        &NativeLaunch::from_frozen_store(
            NativeLaunchMetadata {
                binding_id: "01K00000000000000000000001".into(),
                incarnation: "01K00000000000000000000002".into(),
                endpoint,
                served_name: "toy".into(),
                engine: "sglang".into(),
                recipe: "sglang_engine_config_v2".into(),
                checkpoint_revision: "cdbee75f17c01a7cc42f958dc650907174af0554".into(),
                rendered_settings_digest: "a".repeat(64),
                placement_digest: None,
                device: capyctl_domain::launch::NativeDeviceSelection {
                    host_id: "host-a".into(),
                    hardware_fingerprint: "hardware-v1".into(),
                    device_id: "gpu0".into(),
                    memory_domain: "uma".into(),
                    physical_gpu_uuid: None,
                    cuda_pci_index: None,
                },
            },
            "/private/checkpoint".into(),
            "/opt/sglang/python".into(),
            "inference-ref".into(),
            "admin-ref".into(),
            capyctl_testkit::sglang_launch_settings(),
        ),
        None,
    )
    .unwrap()
}

// T30 (decided 2026-10-06): vLLM is asked on `/v1/completions` with
// `return_token_ids`, with its inference key; the ids come from the choice.
#[tokio::test]
async fn vllm_asks_for_token_ids_on_its_completions_route() {
    let (endpoint, seen) = engine(json!({"choices": [{"text": "ok", "token_ids": [7, 8]}]})).await;
    let adapter = VllmAdapter::new(
        endpoint.parse().unwrap(),
        Some("inference-secret".into()),
        "vllm-test-1".into(),
        ParkPolicy::Disabled,
        "toy".into(),
    );
    assert_eq!(
        adapter.complete_probe("toy", 2, BOUND).await.unwrap(),
        ProbeAnswer {
            tokens: vec![7, 8],
            text: "ok".into()
        }
    );
    let seen = seen.0.lock().unwrap().clone();
    let (path, key, body) = &seen[0];
    assert_eq!(path, "/v1/completions");
    assert_eq!(key.as_deref(), Some("Bearer inference-secret"));
    assert_eq!(
        body,
        &json!({"model": "toy", "prompt": PROMPT, "max_tokens": 2, "temperature": 0,
                "stream": false, "return_token_ids": true})
    );
    // Owner decision 2026-10-09: a completion without token ids answers its
    // text alone; one with neither is a failed probe.
    let (endpoint, _) = engine(json!({"choices": [{"text": "ok"}]})).await;
    let textual = VllmAdapter::new(
        endpoint.parse().unwrap(),
        Some("inference-secret".into()),
        "vllm-test-1".into(),
        ParkPolicy::Disabled,
        "toy".into(),
    );
    assert_eq!(
        textual.complete_probe("toy", 2, BOUND).await.unwrap(),
        ProbeAnswer {
            tokens: vec![],
            text: "ok".into()
        }
    );
    let (endpoint, _) = engine(json!({"choices": [{"text": ""}]})).await;
    let silent = VllmAdapter::new(
        endpoint.parse().unwrap(),
        Some("inference-secret".into()),
        "vllm-test-1".into(),
        ParkPolicy::Disabled,
        "toy".into(),
    );
    assert!(silent.complete_probe("toy", 2, BOUND).await.is_err());
}

// T30 (decided 2026-10-06): SGLang is asked on its native `/generate` with
// the inference key (never the admin key); the ids are `output_ids`.
#[tokio::test]
async fn sglang_asks_for_output_ids_on_generate() {
    let (endpoint, seen) = engine(json!({"text": "ok", "output_ids": [9]})).await;
    let adapter =
        sglang(endpoint).with_credentials("inference-secret".into(), "admin-secret".into());
    assert_eq!(
        adapter
            .complete_probe("toy", 1, BOUND)
            .await
            .unwrap()
            .tokens,
        vec![9]
    );
    let seen = seen.0.lock().unwrap().clone();
    let (path, key, body) = &seen[0];
    assert_eq!(path, "/generate");
    assert_eq!(key.as_deref(), Some("Bearer inference-secret"));
    assert_eq!(
        body,
        &json!({"text": PROMPT, "sampling_params": {"max_new_tokens": 1, "temperature": 0},
                "stream": false})
    );
    // An adapter without the launch's credentials cannot probe at all.
    let (endpoint, seen) = engine(json!({"output_ids": [9]})).await;
    assert!(sglang(endpoint)
        .complete_probe("toy", 1, BOUND)
        .await
        .is_err());
    assert!(seen.0.lock().unwrap().is_empty());
}

// T30 (decided 2026-10-06): TensorFold is asked in the OpenAI form without a
// key (ADR 0023 §3); the ids come from the choice.
#[tokio::test]
async fn tensorfold_asks_for_token_ids_without_a_key() {
    let (endpoint, seen) = engine(json!({"choices": [{"token_ids": [3]}]})).await;
    let adapter = TensorfoldAdapter::new(endpoint.parse().unwrap(), "0.6.0".into(), "toy".into());
    assert_eq!(
        adapter
            .complete_probe("toy", 1, BOUND)
            .await
            .unwrap()
            .tokens,
        vec![3]
    );
    let seen = seen.0.lock().unwrap().clone();
    let (path, key, body) = &seen[0];
    assert_eq!(path, "/v1/completions");
    assert_eq!(key, &None);
    assert_eq!(body["return_token_ids"], true);
    assert_eq!(body["max_tokens"], 1);
    assert_eq!(body["temperature"], 0);
}

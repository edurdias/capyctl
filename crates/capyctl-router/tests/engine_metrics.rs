//! SPEC §17 (owner decision 2026-10-09): every completed chat response carries
//! its per-request engine metrics, each figure with its source: in the body as
//! `capyctl.metrics` for a non-streaming answer, and as the SSE comment
//! `: x-capyctl-metrics {...}` before `data: [DONE]` for a stream.
//!
//! A scripted engine replays response and chunk shapes read from vLLM 0.30,
//! SGLang 0.5.21 and TensorFold 0.6.5 sources, with fixed delays. Fake engines
//! are not qualification.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use capyctl_adapters::traits::{AdapterError, ChatForward, ChatSink, StreamEnded};
use capyctl_controller::Controller;
use capyctl_router::admission::InFlight;
use capyctl_router::engine_metrics::METRICS_COMMENT;
use capyctl_router::forwarders::StaticForwarders;
use capyctl_router::{QueueLimits, RouterDeps};
use capyctl_store::Store;
use capyctl_testkit::FakeEngine;
use serde_json::{json, Value};
use tower::ServiceExt;

/// The delay before the first chunk, and between each later one.
const STEP: Duration = Duration::from_millis(30);

/// One engine's answer: the chunks it streams and the whole body a
/// non-streaming request collects.
struct Scripted {
    chunks: Vec<String>,
    whole: Value,
}

#[async_trait]
impl ChatForward for Scripted {
    async fn forward_chat(&self, _body: &Value) -> Result<Value, AdapterError> {
        Ok(self.whole.clone())
    }

    /// As a collecting forwarder does: every chunk is lent as it arrives.
    async fn forward_chat_observed(
        &self,
        _body: &Value,
        observer: &mut dyn ChatSink,
    ) -> Result<Value, AdapterError> {
        for chunk in &self.chunks {
            tokio::time::sleep(STEP).await;
            observer.progressed();
            observer.collected(chunk);
        }
        Ok(self.whole.clone())
    }

    async fn forward_chat_stream_async(
        &self,
        _body: &Value,
        sink: &mut dyn ChatSink,
    ) -> Result<StreamEnded, AdapterError> {
        for chunk in &self.chunks {
            tokio::time::sleep(STEP).await;
            let _ = sink.send(chunk.clone()).await;
        }
        Ok(StreamEnded::Completed)
    }
}

async fn app(engine: Scripted) -> axum::Router {
    let shared = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let fake = Arc::new(FakeEngine::new());
    let controller = Arc::new(Controller::new(
        shared,
        fake as Arc<dyn capyctl_adapters::EngineAdapter>,
        Arc::new(capyctl_testkit::FakeLauncher::new()),
    ));
    let deps = RouterDeps {
        controller: controller.clone(),
        forwards: Arc::new(StaticForwarders(HashMap::from([(
            "fake".to_string(),
            Arc::new(engine) as Arc<dyn ChatForward>,
        )]))),
        limits: QueueLimits {
            max_requests_per_deployment: 8,
            max_buffered_bytes_total: 64 * 1024,
        },
        api_key: Some("test-key".into()),
        inflight: Arc::new(InFlight::default()),
        activation_join: Arc::new(capyctl_router::WakeJoin::new()),
    };
    let id = controller
        .submit_deploy(capyctl_controller::DeployRequest {
            name: "measured".into(),
            kind: "fake".into(),
            manifest: br#"{"kind":"model","name":"measured"}"#.to_vec(),
            route_model_id: Some("measured".into()),
        })
        .await
        .unwrap();
    let op = controller
        .request_transition(&id, capyctl_domain::LifecycleAction::Start)
        .await
        .unwrap();
    controller.wait_terminal(&op).await.unwrap();
    capyctl_router::serve_router(deps)
}

fn chat(stream: bool) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", "Bearer test-key")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            json!({"model": "measured", "stream": stream,
                "messages": [{"role": "user", "content": "hi"}]})
            .to_string(),
        ))
        .unwrap()
}

/// The non-streaming answer and its `capyctl.metrics`.
async fn collected(engine: Scripted) -> (Value, Value) {
    let res = app(engine).await.oneshot(chat(false)).await.unwrap();
    assert_eq!(res.status(), 200);
    let body = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let answer: Value = serde_json::from_slice(&body).unwrap();
    let figures = answer["capyctl"]["metrics"].clone();
    assert!(figures.is_object(), "{answer}");
    (answer, figures)
}

/// The streamed `data:` payloads and the metrics comment, which comes after
/// every engine chunk and before the terminal.
async fn streamed(engine: Scripted) -> (Vec<String>, Value) {
    let res = app(engine).await.oneshot(chat(true)).await.unwrap();
    assert_eq!(res.status(), 200);
    let body = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.starts_with(&format!(": {METRICS_COMMENT} ")))
        .unwrap_or_else(|| panic!("no metrics comment: {text}"));
    let done = lines.iter().position(|l| *l == "data: [DONE]").unwrap();
    assert!(at < done, "{text}");
    let data: Vec<String> = lines[..at]
        .iter()
        .filter_map(|l| l.strip_prefix("data: "))
        .map(str::to_owned)
        .collect();
    let figures = serde_json::from_str(
        lines[at]
            .strip_prefix(&format!(": {METRICS_COMMENT} "))
            .unwrap(),
    )
    .unwrap();
    (data, figures)
}

fn figure<'a>(figures: &'a Value, name: &str) -> (f64, &'a str) {
    let figure = &figures[name];
    (
        figure["value"]
            .as_f64()
            .unwrap_or_else(|| panic!("no {name} in {figures}")),
        figure["source"].as_str().unwrap(),
    )
}

/// vLLM 0.30 with `--enable-per-request-metrics` and `include_usage`: the
/// usage chunk carries `metrics` (serialized without nulls).
fn vllm() -> Scripted {
    let chunk = |delta: Value, finish: Value| {
        json!({"id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1,
            "model": "m", "choices": [{"index": 0, "delta": delta, "logprobs": null,
            "finish_reason": finish}]})
        .to_string()
    };
    let metrics = json!({"time_to_first_token_ms": 41.5, "generation_time_ms": 70.0,
        "queue_time_ms": 0.75, "mean_itl_ms": 10.0, "tokens_per_second": 71.2});
    let usage = json!({"prompt_tokens": 12, "total_tokens": 20, "completion_tokens": 8});
    Scripted {
        chunks: vec![
            chunk(json!({"role": "assistant", "content": ""}), Value::Null),
            chunk(json!({"content": "Hi"}), Value::Null),
            chunk(json!({"content": "!"}), json!("stop")),
            json!({"id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1,
                "model": "m", "choices": [], "usage": usage, "metrics": metrics})
            .to_string(),
        ],
        whole: json!({"id": "chatcmpl-1", "object": "chat.completion", "created": 1,
            "model": "m", "choices": [{"index": 0, "message": {"role": "assistant",
            "content": "Hi!"}, "logprobs": null, "finish_reason": "stop"}],
            "usage": usage, "metrics": metrics}),
    }
}

/// SGLang 0.5.21 with `--enable-cache-report`: `"usage": null` on every
/// delta, then the usage chunk with `prompt_tokens_details`.
fn sglang(cached: Option<u64>) -> Scripted {
    let chunk = |delta: Value, finish: Value| {
        json!({"id": "abc", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "choices": [{"index": 0, "delta": delta, "logprobs": null,
            "finish_reason": finish, "matched_stop": null}], "usage": null})
        .to_string()
    };
    let details = cached.map(|cached| json!({"cached_tokens": cached}));
    let usage = json!({"prompt_tokens": 12, "total_tokens": 15, "completion_tokens": 3,
        "prompt_tokens_details": details, "reasoning_tokens": 0});
    Scripted {
        chunks: vec![
            chunk(
                json!({"role": "assistant", "content": "", "reasoning_content": null}),
                Value::Null,
            ),
            chunk(json!({"content": "Hi"}), Value::Null),
            chunk(json!({"content": "!"}), Value::Null),
            chunk(json!({}), json!("stop")),
            json!({"id": "abc", "object": "chat.completion.chunk", "created": 1,
                "model": "m", "choices": [], "usage": usage})
            .to_string(),
        ],
        whole: json!({"id": "abc", "object": "chat.completion", "created": 1, "model": "m",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hi!"},
            "finish_reason": "stop", "matched_stop": 151645}], "usage": usage,
            "metadata": {"weight_version": "default"}}),
    }
}

/// TensorFold 0.6.5: statistics on the finish chunk, usage in its own chunk.
fn tensorfold(tokens_per_second: f64) -> Scripted {
    let chunk = |delta: Value, finish: Value| {
        json!({"id": "chatcmpl-t", "object": "chat.completion.chunk", "created": 1,
            "model": "m", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
    };
    let stats = json!({"engine": "cuda", "tokens_per_second": tokens_per_second,
        "seconds": 0.5, "prefill_seconds": 0.2, "time_to_first_token": 0.3,
        "sampling": "greedy", "drafts": false});
    let usage = json!({"prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15,
        "prompt_tokens_details": {"cached_tokens": 0},
        "completion_tokens_details": {"reasoning_tokens": 0}});
    let mut finish = chunk(json!({}), json!("stop"));
    finish["exact_mode"] = json!("target-verified");
    finish["tensorfold"] = stats.clone();
    Scripted {
        chunks: vec![
            chunk(json!({"role": "assistant"}), Value::Null).to_string(),
            chunk(json!({"content": "Hi"}), Value::Null).to_string(),
            chunk(json!({"content": "!"}), Value::Null).to_string(),
            finish.to_string(),
            json!({"id": "chatcmpl-t", "object": "chat.completion.chunk", "created": 1,
                "model": "m", "choices": [], "usage": usage})
            .to_string(),
        ],
        whole: json!({"id": "chatcmpl-t", "object": "chat.completion", "created": 1,
            "model": "m", "choices": [{"index": 0, "message": {"role": "assistant",
            "content": "Hi!"}, "finish_reason": "stop"}], "usage": usage,
            "exact_mode": "target-verified", "tensorfold": stats}),
    }
}

// T40: vLLM's own timings fill every timing figure, labelled `engine`; its
// `metrics` object and usage reach the client unchanged.
#[tokio::test]
async fn vllm_reports_its_own_timings() {
    let engine = vllm();
    let whole = engine.whole.clone();
    let (answer, figures) = collected(engine).await;
    assert_eq!(answer["metrics"], whole["metrics"]);
    assert_eq!(answer["usage"], whole["usage"]);
    let expected = json!({
        "ttft_ms": {"value": 42.25, "source": "engine"},
        "queue_ms": {"value": 0.75, "source": "engine"},
        "prefill_ms": {"value": 41.5, "source": "engine"},
        "decode_tokens_per_second": {"value": 100.0, "source": "engine"},
    });
    // No `--enable-prompt-tokens-details`: no cached count, so none is shown.
    assert_eq!(figures, expected);
    let engine = vllm();
    let sent = engine.chunks.clone();
    let (data, figures) = streamed(engine).await;
    assert_eq!(data, sent, "every engine chunk is relayed byte for byte");
    assert_eq!(figures, expected);
}

// T40 T41: TensorFold's statistics map; its 0.0 tokens per second is
// unknown, so the router's own decode rate stands in, labelled `router`.
#[tokio::test]
async fn tensorfold_reports_its_statistics() {
    let (answer, figures) = collected(tensorfold(40.5)).await;
    assert_eq!(answer["tensorfold"]["tokens_per_second"], 40.5);
    let expected = json!({
        "ttft_ms": {"value": 300.0, "source": "engine"},
        "prefill_ms": {"value": 200.0, "source": "engine"},
        "decode_tokens_per_second": {"value": 40.5, "source": "engine"},
        "cached_tokens": {"value": 0, "source": "engine"},
    });
    assert_eq!(figures, expected);
    let engine = tensorfold(40.5);
    let sent = engine.chunks.clone();
    let (data, figures) = streamed(engine).await;
    assert_eq!(data, sent);
    assert_eq!(figures, expected);

    for stream in [false, true] {
        let figures = if stream {
            streamed(tensorfold(0.0)).await.1
        } else {
            collected(tensorfold(0.0)).await.1
        };
        let (_, source) = figure(&figures, "decode_tokens_per_second");
        assert_eq!(source, "router", "{figures}");
        assert!(figures.get("queue_ms").is_none(), "{figures}");
    }
}

// T40: SGLang reports cached tokens; the router times the rest from its own
// clock and says so. Nothing it cannot measure appears.
#[tokio::test]
async fn sglang_timings_are_the_routers() {
    for stream in [false, true] {
        let figures = if stream {
            streamed(sglang(Some(8))).await.1
        } else {
            collected(sglang(Some(8))).await.1
        };
        assert_eq!(
            figures["cached_tokens"],
            json!({"value": 8, "source": "engine"})
        );
        // The first text is the second chunk, two steps after the forward.
        let (ttft, source) = figure(&figures, "ttft_ms");
        assert_eq!(source, "router");
        assert!(ttft >= 60.0, "{figures}");
        // Two tokens after the first, over at least three steps.
        let (rate, source) = figure(&figures, "decode_tokens_per_second");
        assert_eq!(source, "router");
        assert!(rate > 0.0 && rate <= 2.0 / 0.09, "{figures}");
        for absent in ["queue_ms", "prefill_ms"] {
            assert!(figures.get(absent).is_none(), "{figures}");
        }
    }
}

// T40: absent, never zero. SGLang omits the details when nothing was cached;
// a stream without usage gives the router no token count to divide.
#[tokio::test]
async fn unknown_figures_are_absent() {
    let (_, figures) = collected(sglang(None)).await;
    assert!(figures.get("cached_tokens").is_none(), "{figures}");
    let mut engine = sglang(Some(8));
    // Without `stream_options.include_usage` the usage chunk is never sent.
    engine.chunks.pop();
    let (_, figures) = streamed(engine).await;
    assert_eq!(
        figures.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["ttft_ms"],
        "{figures}"
    );
}

use axum::{http::HeaderMap, routing::post, Json, Router};
use mllm_adapters::sglang::{SglangAdapter, SglangRuntimeObservation, SglangRuntimeObserver};
use mllm_adapters::RuntimeError;
use mllm_adapters::{fake::ParkPolicy, vllm::VllmAdapter, ChatForward, StreamEnded};
use mllm_domain::launch::{
    NativeLaunch, NativeLaunchMetadata, SglangLaunchSettings, SglangRequestedBudget,
};
use serde_json::{json, Value};
const BINDING: &str = "01K00000000000000000000001";
const INCARNATION: &str = "01K00000000000000000000002";
const MODEL: &str = "candidate-01K00000000000000000000001";

struct SlowSink {
    chunks: Vec<String>,
    fail: bool,
}

struct GateSink {
    entered: std::sync::Arc<tokio::sync::Notify>,
    release: std::sync::Arc<tokio::sync::Notify>,
}
#[async_trait::async_trait]
impl mllm_adapters::traits::ChatSink for GateSink {
    async fn send(&mut self, _: String) -> Result<(), mllm_adapters::traits::DeliveryFailed> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

#[tokio::test]
async fn backpressure_stops_parser_before_next_event_even_in_same_http_chunk() {
    for sglang in [false, true] {
        let (adapter, server) = engine(
            sglang,
            format!("{}data:invalid\n\n", chunk("first", Value::Null)),
        )
        .await;
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let mut sink = GateSink {
            entered: entered.clone(),
            release: release.clone(),
        };
        let task = tokio::spawn(async move {
            adapter
                .forward_chat_stream_async(&json!({"model":"public"}), &mut sink)
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
        assert!(
            !task.is_finished(),
            "must await sink before parsing malformed next event"
        );
        release.notify_one();
        assert!(matches!(
            task.await.unwrap(),
            Err(mllm_adapters::AdapterError::Uncertain(_))
        ));
        server.abort();
    }
}

#[tokio::test]
async fn timed_out_sink_is_not_called_again_but_backend_terminal_is_still_verified() {
    let mut tasks = vec![];
    for sglang in [false, true] {
        tasks.push(tokio::spawn(async move {
            let (adapter, server) = engine(
                sglang,
                format!(
                    "{}{}data:[DONE]\n\n",
                    chunk("first", Value::Null),
                    chunk("last", json!("stop"))
                ),
            )
            .await;
            let entered = std::sync::Arc::new(tokio::sync::Notify::new());
            let mut sink = GateSink {
                entered: entered.clone(),
                release: std::sync::Arc::new(tokio::sync::Notify::new()),
            };
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(12),
                adapter.forward_chat_stream_async(&json!({"model":"public"}), &mut sink),
            )
            .await
            .unwrap();
            assert_eq!(result.unwrap(), StreamEnded::Completed);
            server.abort();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
}
#[async_trait::async_trait]
impl mllm_adapters::traits::ChatSink for SlowSink {
    async fn send(&mut self, chunk: String) -> Result<(), mllm_adapters::traits::DeliveryFailed> {
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        self.chunks.push(chunk);
        if self.fail {
            Err(mllm_adapters::traits::DeliveryFailed)
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn async_sink_waits_in_order_and_failure_drains_without_resuming_delivery() {
    for sglang in [false, true] {
        for fail in [false, true] {
            for terminal in [false, true] {
                let sse = format!(
                    "{}{}{}",
                    chunk("one", Value::Null),
                    chunk("two", json!("stop")),
                    if terminal { "data:[DONE]\n\n" } else { "" }
                );
                let (adapter, task) = engine(sglang, sse).await;
                let mut sink = SlowSink {
                    chunks: vec![],
                    fail,
                };
                let result = adapter
                    .forward_chat_stream_async(&json!({"model":"public"}), &mut sink)
                    .await;
                assert_eq!(matches!(result, Ok(StreamEnded::Completed)), terminal);
                assert_eq!(sink.chunks.len(), if fail { 1 } else { 2 });
                assert_eq!(
                    serde_json::from_str::<Value>(&sink.chunks[0]).unwrap()["choices"][0]["delta"]
                        ["content"],
                    "one"
                );
                if !fail {
                    assert_eq!(
                        serde_json::from_str::<Value>(&sink.chunks[1]).unwrap()["choices"][0]
                            ["delta"]["content"],
                        "two"
                    );
                }
                task.abort();
            }
        }
    }
}

struct Observer;
#[async_trait::async_trait]
impl SglangRuntimeObserver for Observer {
    async fn observe(&self) -> Result<SglangRuntimeObservation, RuntimeError> {
        Err(RuntimeError::Unsupported)
    }
}

fn frozen(endpoint: String) -> NativeLaunch {
    NativeLaunch::from_frozen_store(
        NativeLaunchMetadata {
            binding_id: BINDING.into(),
            incarnation: INCARNATION.into(),
            endpoint,
            served_name: MODEL.into(),
            engine: "sglang".into(),
            recipe: "qwen3_4b_instruct2507_tp1_dp1_bf16_disk_reload_v1".into(),
            source_revision: "fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1".into(),
            checkpoint_revision: "cdbee75f17c01a7cc42f958dc650907174af0554".into(),
            rendered_settings_digest: "a".repeat(64),
            device: mllm_domain::launch::NativeDeviceSelection {
                host_id: "host-a".into(),
                hardware_fingerprint: "hardware-v1".into(),
                device_id: "gpu0".into(),
                memory_domain: "uma".into(),
            },
        },
        "/private/checkpoint".into(),
        "/opt/sglang/python".into(),
        "inference-ref".into(),
        "admin-ref".into(),
        SglangLaunchSettings {
            recipe: "qwen3_4b_instruct2507_tp1_dp1_bf16_disk_reload_v1".into(),
            tensor_parallel_size: 1,
            data_parallel_size: 1,
            tokenizer_workers: 1,
            model_dtype: "bfloat16".into(),
            context_tokens: 4096,
            max_running_requests: 8,
            max_total_tokens: 4096,
            prefill_cuda_graphs: false,
            decode_cuda_graphs: false,
            memory_saver: true,
            cpu_weight_backup: false,
            speculative_decoding: false,
            lora: false,
            trust_remote_code: false,
            disaggregation: false,
            external_cache: false,
            cpu_kv_offload: false,
            native_grpc: false,
            weight_restore: "disk_reload".into(),
            requested_budget: SglangRequestedBudget {
                kv_cache_bytes: 4_294_967_296,
                static_memory_fraction_bps: 7500,
            },
        },
    )
}

async fn engine(sglang: bool, sse: String) -> (Box<dyn ChatForward>, tokio::task::JoinHandle<()>) {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let sse = sse.clone();
            async move {
                assert_eq!(headers["authorization"], "Bearer inference-secret");
                assert_eq!(body["model"], MODEL);
                assert_eq!(body["stream"], true);
                ([("content-type", "text/event-stream")], sse)
            }
        }),
    );
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", socket.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(socket, app).await.unwrap();
    });
    let adapter: Box<dyn ChatForward> = if sglang {
        Box::new(
            SglangAdapter::from_frozen(
                &frozen(url),
                "inference-secret".into(),
                "admin-secret".into(),
                std::sync::Arc::new(Observer),
            )
            .unwrap(),
        )
    } else {
        Box::new(VllmAdapter::new(
            url.parse().unwrap(),
            Some("inference-secret".into()),
            "pin".into(),
            ParkPolicy::Disabled,
            MODEL.into(),
        ))
    };
    (adapter, task)
}

fn chunk(content: &str, finish: Value) -> String {
    format!(
        "data: {}\r\n\r\n",
        json!({"id":"chat-1", "object":"chat.completion.chunk", "created":1,"model":MODEL, "choices":[{"index":0,"delta":{"content":content},"finish_reason":finish}]})
    )
}

#[tokio::test]
async fn collects_ordered_content_preserves_public_identity_and_terminal_reason() {
    for sglang in [false, true] {
        let (adapter, task) = engine(
            sglang,
            format!(
                "{}{}data: [DONE]\n\n",
                chunk("a}{", Value::Null),
                chunk("β", json!("stop"))
            ),
        )
        .await;
        let result = adapter
            .forward_chat(&json!({"model":"public", "messages":[]}))
            .await
            .unwrap();
        assert_eq!(result["model"], "public");
        assert_eq!(result["choices"][0]["message"]["content"], "a}{β");
        assert_eq!(result["choices"][0]["finish_reason"], "stop");
        task.abort();
    }
}

#[tokio::test]
async fn rejects_malformed_mismatched_and_unfinished_streams() {
    for sglang in [false, true] {
        for sse in [
            "data: not-json\n\ndata: [DONE]\n\n".into(),
            format!("{}data: [DONE]\n\n", chunk("x", Value::Null)),
            format!(
                "{}data: [DONE]\n\n",
                chunk("x", json!("stop")).replace(MODEL, "wrong")
            ),
            chunk("partial", Value::Null),
        ] {
            let (adapter, task) = engine(sglang, sse).await;
            assert!(adapter
                .forward_chat(&json!({"model":"public"}))
                .await
                .is_err());
            task.abort();
        }
    }
}

#[tokio::test]
async fn streams_in_order_and_reports_premature_close_without_completion() {
    for sglang in [false, true] {
        let (adapter, task) = engine(sglang, chunk("partial", Value::Null)).await;
        let mut chunks = vec![];
        let end = adapter
            .forward_chat_stream(&json!({"model":"public"}), &mut |s| {
                chunks.push(serde_json::from_str::<Value>(&s).unwrap())
            })
            .await;
        assert!(matches!(
            end,
            Err(mllm_adapters::AdapterError::Uncertain(_))
        ));
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0]["model"], "public");
        task.abort();
    }
}

#[tokio::test]
async fn rejects_changed_response_identity_duplicate_fields_and_trailing_generation() {
    for sglang in [false, true] {
        for bad in [
            format!(
                "{}{}data: [DONE]\n\n",
                chunk("a", Value::Null),
                chunk("b", json!("stop")).replace("chat-1", "chat-2")
            ),
            format!(
                "{}{}data: [DONE]\n\n",
                chunk("a", json!("stop")),
                chunk("b", Value::Null)
            ),
            format!(
                "{}data: [DONE]\n\n",
                chunk("a", json!("stop")).replace("\"model\":", "\"model\":\"wrong\",\"model\":")
            ),
            format!(
                "{}data: [DONE]\n\n",
                chunk("x", json!("stop")).replace("\"index\":0", "\"index\":1")
            ),
            format!(
                "{}data: [DONE]\n\n",
                chunk("x", json!("stop")).replace("\"content\":\"x\"", "\"tool_calls\":[]")
            ),
            format!("data: {}\n\n", "x".repeat(65536)),
        ] {
            let (adapter, task) = engine(sglang, bad).await;
            assert!(adapter
                .forward_chat(&json!({"model":"public"}))
                .await
                .is_err());
            task.abort();
        }
    }
}

#[tokio::test]
async fn completes_stream_with_ordered_chunks_and_optional_usage_event() {
    for sglang in [false, true] {
        let usage = json!({"id":"chat-1","object":"chat.completion.chunk","created":1,"model":MODEL,"choices":[],"usage":{"completion_tokens":2,"prompt_tokens":1,"total_tokens":3}});
        let sse = format!(
            ": heartbeat\n\n{}{}data:{usage}\n\ndata:[DONE]\n\n",
            chunk("α", Value::Null),
            chunk("β", json!("length"))
        );
        let (adapter, task) = engine(sglang, sse).await;
        let mut chunks = vec![];
        let end = adapter
            .forward_chat_stream(&json!({"model":"public","stream":false}), &mut |s| {
                chunks.push(serde_json::from_str::<Value>(&s).unwrap())
            })
            .await
            .unwrap();
        assert_eq!(end, StreamEnded::Completed);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0]["choices"][0]["delta"]["content"], "α");
        assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "β");
        assert!(chunks.iter().all(|c| c["model"] == "public"));
        let collected = adapter
            .forward_chat(&json!({"model":"public"}))
            .await
            .unwrap();
        assert_eq!(collected["usage"]["total_tokens"], 3);
        assert_eq!(collected["choices"][0]["finish_reason"], "length");
        task.abort();
    }
}

#[tokio::test]
async fn unsupported_multi_choice_and_tool_requests_do_not_report_success() {
    for sglang in [false, true] {
        let (adapter, task) = engine(
            sglang,
            format!("{}data: [DONE]\n\n", chunk("ok", json!("stop"))),
        )
        .await;
        for body in [
            json!({"model":"public","n":2}),
            json!({"model":"public","tools":[]}),
            json!({"model":""}),
            json!([]),
        ] {
            assert!(adapter.forward_chat(&body).await.is_err());
        }
        task.abort();
    }
}

#[tokio::test]
async fn canceled_stream_does_not_prove_backend_quiescence_or_sglang_readiness() {
    use mllm_adapters::{CancellationOutcome, EngineAdapter, MemberRef, Readiness, RequestRef};
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            let body = axum::body::Body::from_stream(futures::stream::pending::<
                Result<String, std::io::Error>,
            >());
            ([("content-type", "text/event-stream")], body)
        }),
    );
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", socket.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(socket, app).await.unwrap();
    });
    let sglang = SglangAdapter::from_frozen(
        &frozen(url.clone()),
        "inference-secret".into(),
        "admin-secret".into(),
        std::sync::Arc::new(Observer),
    )
    .unwrap();
    let vllm = VllmAdapter::new(
        url.parse().unwrap(),
        None,
        "pin".into(),
        ParkPolicy::Disabled,
        MODEL.into(),
    );
    let member = MemberRef {
        deployment_id: "deployment".into(),
        member_id: "member".into(),
    };
    let request = RequestRef {
        id: "request".into(),
    };
    for adapter in [&sglang as &dyn ChatForward, &vllm as &dyn ChatForward] {
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(30),
            adapter.forward_chat_stream(&json!({"model":"public"}), &mut |_| panic!(
                "no chunks expected"
            ))
        )
        .await
        .is_err());
    }
    assert_eq!(
        sglang.cancel_work(&member, &request, true).await.unwrap(),
        CancellationOutcome::Uncertain
    );
    assert_eq!(
        vllm.cancel_work(&member, &request, true).await.unwrap(),
        CancellationOutcome::Uncertain
    );
    assert_eq!(
        sglang.check_readiness(&member).await.unwrap(),
        Readiness::Initializing
    );
    task.abort();
}

#[tokio::test]
async fn redirects_never_receive_the_inference_credential() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let hits = Arc::new(AtomicUsize::new(0));
    let count = hits.clone();
    let sink = Router::new().route(
        "/sink",
        post(move || {
            count.fetch_add(1, Ordering::SeqCst);
            async { "leaked" }
        }),
    );
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = format!("http://{}/sink", socket.local_addr().unwrap());
    let sink_task = tokio::spawn(async move {
        axum::serve(socket, sink).await.unwrap();
    });
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let target = target.clone();
            async move {
                (
                    axum::http::StatusCode::TEMPORARY_REDIRECT,
                    [("location", target)],
                )
            }
        }),
    );
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", socket.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(socket, app).await.unwrap();
    });
    let adapters: Vec<Box<dyn ChatForward>> = vec![
        Box::new(
            SglangAdapter::from_frozen(
                &frozen(url.clone()),
                "inference-secret".into(),
                "admin-secret".into(),
                std::sync::Arc::new(Observer),
            )
            .unwrap(),
        ),
        Box::new(VllmAdapter::new(
            url.parse().unwrap(),
            Some("inference-secret".into()),
            "pin".into(),
            ParkPolicy::Disabled,
            MODEL.into(),
        )),
    ];
    for adapter in adapters {
        let error = adapter
            .forward_chat(&json!({"model":"public"}))
            .await
            .unwrap_err();
        assert!(!format!("{error:?}").contains("inference-secret"));
    }
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    task.abort();
    sink_task.abort();
}

#[tokio::test]
async fn split_utf8_and_crlf_survive_but_invalid_utf8_never_completes() {
    for sglang in [false, true] {
        for invalid in [false, true] {
            let mut bytes =
                format!("{}data:[DONE]\r\n\r\n", chunk("β", json!("stop"))).into_bytes();
            if invalid {
                let offset = bytes.iter().position(|b| *b == 0xce).unwrap();
                bytes[offset] = 0xff;
            }
            let app = Router::new().route(
                "/v1/chat/completions",
                post(move || {
                    let chunks = bytes
                        .iter()
                        .map(|b| Ok::<_, std::io::Error>(axum::body::Bytes::copy_from_slice(&[*b])))
                        .collect::<Vec<_>>();
                    async move {
                        (
                            [("content-type", "text/event-stream")],
                            axum::body::Body::from_stream(futures::stream::iter(chunks)),
                        )
                    }
                }),
            );
            let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", socket.local_addr().unwrap());
            let task = tokio::spawn(async move {
                axum::serve(socket, app).await.unwrap();
            });
            let adapter: Box<dyn ChatForward> = if sglang {
                Box::new(
                    SglangAdapter::from_frozen(
                        &frozen(url),
                        "inference-secret".into(),
                        "admin-secret".into(),
                        std::sync::Arc::new(Observer),
                    )
                    .unwrap(),
                )
            } else {
                Box::new(VllmAdapter::new(
                    url.parse().unwrap(),
                    None,
                    "pin".into(),
                    ParkPolicy::Disabled,
                    MODEL.into(),
                ))
            };
            let result = adapter.forward_chat(&json!({"model":"public"})).await;
            if invalid {
                assert!(result.is_err());
            } else {
                assert_eq!(result.unwrap()["choices"][0]["message"]["content"], "β");
            }
            task.abort();
        }
    }
}

#[tokio::test]
async fn bounded_stream_rejects_many_small_events_before_done() {
    for sglang in [false, true] {
        let event = chunk(&"x".repeat(16000), Value::Null);
        let sse = format!(
            "{}{}data:[DONE]\n\n",
            event.repeat(1100),
            chunk("", json!("stop"))
        );
        let (adapter, task) = engine(sglang, sse).await;
        assert!(adapter
            .forward_chat_stream(&json!({"model":"public"}), &mut |_| {})
            .await
            .is_err());
        task.abort();
    }
}

fn reasoning_chunk(reasoning: &str, content: &str, finish: Value) -> String {
    let mut delta = json!({});
    if !reasoning.is_empty() {
        delta["reasoning_content"] = json!(reasoning);
    }
    if !content.is_empty() {
        delta["content"] = json!(content);
    }
    format!(
        "data: {}\r\n\r\n",
        json!({"id":"chat-1","object":"chat.completion.chunk","created":1,"model":MODEL,"choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    )
}

/// SPEC §10 requires reasoning fields to be preserved. A reasoning model streams its
/// trace as `reasoning_content`, so rejecting the key fails every chunk and the whole
/// stream while the engine is behaving correctly. Observed live on host-a with
/// Qwen3-30B-A3B and `--reasoning-parser qwen3`.
#[tokio::test]
async fn a_reasoning_trace_streams_through_and_survives_collection() {
    for sglang in [false, true] {
        let sse = format!(
            "{}{}{}data:[DONE]\n\n",
            reasoning_chunk("thinking ", "", Value::Null),
            reasoning_chunk("more", "", Value::Null),
            reasoning_chunk("", "OK", json!("stop")),
        );
        let (adapter, task) = engine(sglang, sse).await;
        let mut chunks = vec![];
        let end = adapter
            .forward_chat_stream(&json!({"model":"public","stream":true}), &mut |s| {
                chunks.push(serde_json::from_str::<Value>(&s).unwrap())
            })
            .await
            .expect("a reasoning stream is valid, not a protocol violation");
        assert_eq!(end, StreamEnded::Completed);
        assert_eq!(chunks.len(), 3);
        assert_eq!(
            chunks[0]["choices"][0]["delta"]["reasoning_content"],
            "thinking "
        );
        assert_eq!(chunks[2]["choices"][0]["delta"]["content"], "OK");

        let collected = adapter
            .forward_chat(&json!({"model":"public"}))
            .await
            .unwrap();
        assert_eq!(collected["choices"][0]["message"]["content"], "OK");
        assert_eq!(
            collected["choices"][0]["message"]["reasoning_content"], "thinking more",
            "collecting must not discard what a streaming caller would have seen"
        );
        drop(task);
    }
}

/// A response with no trace keeps its existing shape: the field is absent, not empty.
#[tokio::test]
async fn a_response_without_a_trace_gains_no_reasoning_field() {
    let sse = format!("{}data:[DONE]\n\n", chunk("OK", json!("stop")));
    let (adapter, task) = engine(false, sse).await;
    let collected = adapter
        .forward_chat(&json!({"model":"public"}))
        .await
        .unwrap();
    assert_eq!(collected["choices"][0]["message"]["content"], "OK");
    assert!(collected["choices"][0]["message"]
        .get("reasoning_content")
        .is_none());
    drop(task);
}

/// The allowlist stays closed: an unknown delta field is still never relayed.
#[tokio::test]
async fn an_unknown_delta_field_is_still_rejected() {
    let sse = format!(
        "data: {}\r\n\r\ndata:[DONE]\n\n",
        json!({"id":"chat-1","object":"chat.completion.chunk","created":1,"model":MODEL,"choices":[{"index":0,"delta":{"content":"OK","speculative_tokens":3},"finish_reason":"stop"}]})
    );
    let (adapter, task) = engine(false, sse).await;
    assert!(
        adapter
            .forward_chat_stream(&json!({"model":"public","stream":true}), &mut |_| {})
            .await
            .is_err(),
        "untested fields must not pass through"
    );
    drop(task);
}

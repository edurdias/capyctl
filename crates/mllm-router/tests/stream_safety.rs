//! Transitional F1 stream safety; this is not durable F2 request settlement.
use std::sync::Arc;

use async_trait::async_trait;
use axum::response::IntoResponse;
use futures::{FutureExt, StreamExt};
use mllm_adapters::traits::StreamEnded;
use mllm_adapters::{AdapterError, ChatForward};
use mllm_router::{admission::InFlight, stream::stream_response};
use serde_json::{Value, json};
use tokio::sync::Notify;

struct Forward {
    chunks: usize,
    chunk_bytes: usize,
    finish: Result<StreamEnded, AdapterError>,
    finish_gate: Option<Arc<Notify>>,
}

#[async_trait]
impl ChatForward for Forward {
    async fn forward_chat(&self, _: &Value) -> Result<Value, AdapterError> {
        if let Some(gate) = &self.finish_gate {
            gate.notified().await;
        }
        Err(AdapterError::Uncertain("private-backend-detail".into()))
    }

    async fn forward_chat_stream_async(
        &self,
        _: &Value,
        sink: &mut dyn mllm_adapters::traits::ChatSink,
    ) -> Result<StreamEnded, AdapterError> {
        for index in 0..self.chunks {
            if sink
                .send(format!("{index}:{}", "x".repeat(self.chunk_bytes)))
                .await
                .is_err()
            {
                break;
            }
        }
        if let Some(gate) = &self.finish_gate {
            gate.notified().await;
        }
        match self.finish {
            Ok(end) => Ok(end),
            Err(_) => Err(AdapterError::Uncertain("unverified terminal".into())),
        }
    }
}

fn response(forward: Forward, counts: &Arc<InFlight>) -> axum::response::Response {
    stream_response(
        Arc::new(forward),
        json!({"model":"m"}),
        counts.guard_arc("d"),
    )
    .into_response()
}

#[tokio::test]
async fn unverified_backend_end_never_sends_done_or_releases_accounting() {
    for finish in [
        Ok(StreamEnded::BackendClosed),
        Err(AdapterError::Uncertain("lost".into())),
    ] {
        let counts = Arc::new(InFlight::default());
        let response = response(
            Forward {
                chunks: 1,
                chunk_bytes: 4,
                finish,
                finish_gate: None,
            },
            &counts,
        );
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("[DONE]"));
        assert_eq!(
            counts.current("d"),
            1,
            "transport end is not completion proof"
        );
    }
}

#[tokio::test]
async fn slow_consumer_receives_all_chunks_before_success() {
    let counts = Arc::new(InFlight::default());
    let gate = Arc::new(Notify::new());
    let response = response(
        Forward {
            chunks: 20,
            chunk_bytes: 4,
            finish: Ok(StreamEnded::Completed),
            finish_gate: Some(gate.clone()),
        },
        &counts,
    );
    let mut body = response.into_body().into_data_stream();
    let mut text = String::new();
    // A burst larger than the bounded queue must wait, not drop chunks.
    for _ in 0..16 {
        text.push_str(&String::from_utf8_lossy(
            &body.next().await.unwrap().unwrap(),
        ));
    }
    gate.notify_one();
    while let Some(bytes) = body.next().await {
        text.push_str(&String::from_utf8_lossy(&bytes.unwrap()));
    }
    let expected = (0..20)
        .map(|index| format!("data: {index}:xxxx\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n";
    assert_eq!(text, expected);
    assert_eq!(
        counts.current("d"),
        0,
        "verified backend completion is independent of delivery"
    );
}

#[tokio::test]
async fn oversized_chunk_fails_delivery_without_fake_terminal() {
    let counts = Arc::new(InFlight::default());
    let response = response(
        Forward {
            chunks: 1,
            chunk_bytes: 65536,
            finish: Ok(StreamEnded::Completed),
            finish_gate: None,
        },
        &counts,
    );
    let bytes = axum::body::to_bytes(response.into_body(), 131072)
        .await
        .unwrap();
    assert!(bytes.is_empty());
    assert_eq!(counts.current("d"), 0);
}

#[tokio::test]
async fn successful_bounded_delivery_is_ordered_and_releases_once() {
    let counts = Arc::new(InFlight::default());
    let response = response(
        Forward {
            chunks: 3,
            chunk_bytes: 1,
            finish: Ok(StreamEnded::Completed),
            finish_gate: None,
        },
        &counts,
    );
    let bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert_eq!(
        text,
        "data: 0:x\n\ndata: 1:x\n\ndata: 2:x\n\ndata: [DONE]\n\n"
    );
    assert_eq!(counts.current("d"), 0);
}

async fn ready_deps(pending: bool) -> (mllm_router::RouterDeps, String) {
    let store = Arc::new(std::sync::Mutex::new(
        mllm_store::Store::open_in_memory().unwrap(),
    ));
    let fake = Arc::new(mllm_testkit::FakeEngine::new());
    let controller = Arc::new(mllm_controller::Controller::new(
        store.clone(),
        fake,
        Arc::new(mllm_testkit::FakeLauncher::new()),
    ));
    let id = controller
        .submit_deploy(mllm_controller::DeployRequest {
            name: "m".into(),
            kind: "fake".into(),
            manifest: b"name: m\nkind: fake\n".to_vec(),
            route_model_id: Some("m".into()),
        })
        .await
        .unwrap();
    let op = controller
        .request_transition(&id, mllm_domain::LifecycleAction::Start)
        .await
        .unwrap();
    controller.wait_terminal(&op).await.unwrap();
    let forward: Arc<dyn ChatForward> = Arc::new(Forward {
        chunks: 0,
        chunk_bytes: 0,
        finish: Ok(StreamEnded::Completed),
        finish_gate: pending.then(|| Arc::new(Notify::new())),
    });
    (
        mllm_router::RouterDeps {
            controller,
            forwards: Arc::new(mllm_router::forwarders::StaticForwarders(
                std::collections::HashMap::from([("fake".into(), forward)]),
            )),
            limits: mllm_router::QueueLimits {
                max_requests_per_deployment: 8,
                max_buffered_bytes_total: 65536,
            },
            api_key: None,
            inflight: Arc::new(InFlight::default()),
            activation_join: Arc::new(mllm_router::WakeJoin::new()),
        },
        id.to_string(),
    )
}

#[tokio::test]
async fn nonstream_failure_retains_accounting_and_redacts_backend_details() {
    let (deps, id) = ready_deps(false).await;
    let error = mllm_router::chat::dispatch(&deps, "m", &json!({"model":"m"}))
        .await
        .unwrap_err();
    assert_eq!(deps.inflight.current(&id), 1);
    assert!(!error.1.0.to_string().contains("private-backend-detail"));
}

#[tokio::test]
async fn dropping_nonstream_forward_keeps_uncertain_accounting() {
    let (deps, id) = ready_deps(true).await;
    assert!(
        mllm_router::chat::dispatch(&deps, "m", &json!({"model":"m"}))
            .now_or_never()
            .is_none()
    );
    assert_eq!(deps.inflight.current(&id), 1);
}

#[tokio::test]
async fn full_final_queue_waits_for_delivery_of_done() {
    let counts = Arc::new(InFlight::default());
    let response = response(
        Forward {
            chunks: 16,
            chunk_bytes: 1,
            finish: Ok(StreamEnded::Completed),
            finish_gate: None,
        },
        &counts,
    );
    let bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert_eq!(
        text.lines()
            .filter(|line| line.starts_with("data:"))
            .count(),
        17
    );
    assert!(text.ends_with("data: [DONE]\n\n"));
    assert_eq!(counts.current("d"), 0);
}

#[tokio::test]
async fn client_disconnect_waits_for_backend_completion_before_release() {
    let counts = Arc::new(InFlight::default());
    let gate = Arc::new(Notify::new());
    drop(response(
        Forward {
            chunks: 1,
            chunk_bytes: 1,
            finish: Ok(StreamEnded::Completed),
            finish_gate: Some(gate.clone()),
        },
        &counts,
    ));
    assert_eq!(counts.current("d"), 1);
    gate.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while counts.current("d") != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn stalled_delivery_times_out_without_done_but_settles_verified_backend() {
    let counts = Arc::new(InFlight::default());
    let response = response(
        Forward {
            chunks: 20,
            chunk_bytes: 1,
            finish: Ok(StreamEnded::Completed),
            finish_gate: None,
        },
        &counts,
    );
    // Do not poll the body until the bounded sink's wait expires.
    tokio::time::timeout(std::time::Duration::from_secs(12), async {
        while counts.current("d") != 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert_eq!(
        text.lines()
            .filter(|line| line.starts_with("data:"))
            .count(),
        16
    );
    assert!(!text.contains("[DONE]"));
}

struct PanicForward;
#[async_trait]
impl ChatForward for PanicForward {
    async fn forward_chat(&self, _: &Value) -> Result<Value, AdapterError> {
        unreachable!()
    }
    async fn forward_chat_stream_async(
        &self,
        _: &Value,
        _: &mut dyn mllm_adapters::traits::ChatSink,
    ) -> Result<StreamEnded, AdapterError> {
        panic!("backend task lost");
    }
}

#[tokio::test]
async fn backend_panic_keeps_abandoned_accounting() {
    let counts = Arc::new(InFlight::default());
    let response = stream_response(
        Arc::new(PanicForward),
        json!({"model":"m"}),
        counts.guard_arc("d"),
    )
    .into_response();
    assert!(
        axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(counts.current("d"), 1);
}

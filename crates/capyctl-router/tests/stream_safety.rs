//! Transitional F1 stream safety; this is not durable F2 request settlement.
use std::sync::Arc;

use async_trait::async_trait;
use axum::response::IntoResponse;
use capyctl_adapters::traits::StreamEnded;
use capyctl_adapters::{AdapterError, ChatForward};
use capyctl_router::{admission::InFlight, stream::stream_response};
use futures::{FutureExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::Notify;

struct Forward {
    chunks: usize,
    chunk_bytes: usize,
    finish: Result<StreamEnded, AdapterError>,
    finish_gate: Option<Arc<Notify>>,
    /// Return `Cancelled` at the first failed send, as the real forwarder does.
    cancel_on_failure: bool,
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
        sink: &mut dyn capyctl_adapters::traits::ChatSink,
    ) -> Result<StreamEnded, AdapterError> {
        for index in 0..self.chunks {
            if sink
                .send(format!("{index}:{}", "x".repeat(self.chunk_bytes)))
                .await
                .is_err()
            {
                if self.cancel_on_failure {
                    return Ok(StreamEnded::Cancelled);
                }
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
        None,
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
                cancel_on_failure: false,
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
            cancel_on_failure: false,
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
            cancel_on_failure: false,
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
            cancel_on_failure: false,
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

async fn ready_deps(pending: bool) -> (capyctl_router::RouterDeps, String) {
    let store = Arc::new(std::sync::Mutex::new(
        capyctl_store::Store::open_in_memory().unwrap(),
    ));
    let fake = Arc::new(capyctl_testkit::FakeEngine::new());
    let controller = Arc::new(capyctl_controller::Controller::new(
        store.clone(),
        fake,
        Arc::new(capyctl_testkit::FakeLauncher::new()),
    ));
    let id = controller
        .submit_deploy(capyctl_controller::DeployRequest {
            name: "m".into(),
            kind: "fake".into(),
            manifest: b"name: m\nkind: fake\n".to_vec(),
            route_model_id: Some("m".into()),
        })
        .await
        .unwrap();
    let op = controller
        .request_transition(&id, capyctl_domain::LifecycleAction::Start)
        .await
        .unwrap();
    controller.wait_terminal(&op).await.unwrap();
    let forward: Arc<dyn ChatForward> = Arc::new(Forward {
        chunks: 0,
        chunk_bytes: 0,
        finish: Ok(StreamEnded::Completed),
        finish_gate: pending.then(|| Arc::new(Notify::new())),
        cancel_on_failure: false,
    });
    (
        capyctl_router::RouterDeps {
            controller,
            forwards: Arc::new(capyctl_router::forwarders::StaticForwarders(
                std::collections::HashMap::from([("fake".into(), forward)]),
            )),
            limits: capyctl_router::QueueLimits {
                max_requests_per_deployment: 8,
                max_buffered_bytes_total: 65536,
            },
            api_key: None,
            inflight: Arc::new(InFlight::default()),
            activation_join: Arc::new(capyctl_router::WakeJoin::new()),
        },
        id.to_string(),
    )
}

#[tokio::test]
async fn nonstream_failure_retains_accounting_and_redacts_backend_details() {
    let (deps, id) = ready_deps(false).await;
    let error = capyctl_router::chat::dispatch(&deps, "m", &json!({"model":"m"}))
        .await
        .unwrap_err();
    assert_eq!(deps.inflight.current(&id), 1);
    assert!(!error.1 .0.to_string().contains("private-backend-detail"));
}

#[tokio::test]
async fn dropping_nonstream_forward_keeps_uncertain_accounting() {
    let (deps, id) = ready_deps(true).await;
    assert!(
        capyctl_router::chat::dispatch(&deps, "m", &json!({"model":"m"}))
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
            cancel_on_failure: false,
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
async fn a_backend_that_completes_after_a_hang_up_releases_on_completion() {
    let counts = Arc::new(InFlight::default());
    let gate = Arc::new(Notify::new());
    drop(response(
        Forward {
            chunks: 1,
            chunk_bytes: 1,
            finish: Ok(StreamEnded::Completed),
            finish_gate: Some(gate.clone()),
            cancel_on_failure: false,
        },
        &counts,
    ));
    assert_eq!(counts.current("d"), 1);
    gate.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
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
            cancel_on_failure: false,
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
        _: &mut dyn capyctl_adapters::traits::ChatSink,
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
        None,
    )
    .into_response();
    assert!(axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(counts.current("d"), 1);
}

/// Sends `chunks` events spaced by `gap`, optionally only reporting progress
/// (a stream draining after its client left), then stalls for `stall` before
/// its terminator.
struct Paced {
    chunks: usize,
    gap: std::time::Duration,
    stall: std::time::Duration,
    deliver: bool,
}

#[async_trait]
impl ChatForward for Paced {
    async fn forward_chat(&self, _: &Value) -> Result<Value, AdapterError> {
        unreachable!()
    }
    async fn forward_chat_stream_async(
        &self,
        _: &Value,
        sink: &mut dyn capyctl_adapters::traits::ChatSink,
    ) -> Result<StreamEnded, AdapterError> {
        for index in 0..self.chunks {
            tokio::time::sleep(self.gap).await;
            if self.deliver {
                let _ = sink.send(format!("{index}")).await;
            } else {
                sink.progressed();
            }
        }
        tokio::time::sleep(self.stall).await;
        Ok(StreamEnded::Completed)
    }
}

fn paced(
    forward: Paced,
    counts: &Arc<InFlight>,
    first_ms: u64,
    idle_ms: u64,
) -> axum::response::Response {
    let bounds = capyctl_router::stream::StreamBounds {
        first_event_by: tokio::time::Instant::now() + std::time::Duration::from_millis(first_ms),
        idle: std::time::Duration::from_millis(idle_ms),
    };
    capyctl_router::stream::stream_planned_timed(
        capyctl_router::balance::Attempt::direct(Arc::new(forward), None),
        None,
        json!({"model":"m"}),
        counts.guard_arc("d"),
        capyctl_router::timing::RequestTiming::untracked(),
        bounds,
    )
    .into_response()
}

/// SPEC §10: found live 2026-09-23 (matrix M30, vLLM), a fixed 300 s cap cut a
/// stream that was still producing and left its lease uncertain. A stream that
/// keeps producing runs past its first-event deadline and completes.
// T17 T19
#[tokio::test]
async fn progressing_stream_is_never_cut_at_a_fixed_wall_time() {
    let counts = Arc::new(InFlight::default());
    let response = paced(
        Paced {
            chunks: 12,
            gap: std::time::Duration::from_millis(100),
            stall: std::time::Duration::ZERO,
            deliver: true,
        },
        &counts,
        // The first event is due well inside the 1.2 s the stream runs, and
        // the idle bound leaves a slow runner room between 100 ms chunks.
        500,
        1_000,
    );
    let started = std::time::Instant::now();
    let bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    assert!(started.elapsed() > std::time::Duration::from_millis(1000));
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("data: 11\n\n"), "{text}");
    assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
    assert_eq!(counts.current("d"), 0, "completion closes the accounting");
}

/// SPEC §10: a stream whose backend stops producing is cut after the idle
/// bound, without a fake terminator; acceptance is unknown, so the charge
/// stays conservative.
// T17 T19
#[tokio::test]
async fn stalled_stream_is_cut_after_the_idle_bound_and_stays_uncertain() {
    let counts = Arc::new(InFlight::default());
    let response = paced(
        Paced {
            chunks: 1,
            gap: std::time::Duration::ZERO,
            stall: std::time::Duration::from_secs(30),
            deliver: true,
        },
        &counts,
        5_000,
        300,
    );
    let bytes = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        axum::body::to_bytes(response.into_body(), 4096),
    )
    .await
    .expect("the idle bound ends the stream")
    .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("data: 0\n\n"), "{text}");
    assert!(!text.contains("[DONE]"), "{text}");
    assert_eq!(counts.current("d"), 1);
}

/// SPEC §10: a backend that never produces a first event is cut at the
/// request deadline, again without releasing the charge.
// T17 T19
#[tokio::test]
async fn silent_stream_is_cut_at_the_request_deadline() {
    let counts = Arc::new(InFlight::default());
    let response = paced(
        Paced {
            chunks: 0,
            gap: std::time::Duration::ZERO,
            stall: std::time::Duration::from_secs(30),
            deliver: true,
        },
        &counts,
        300,
        60_000,
    );
    let bytes = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        axum::body::to_bytes(response.into_body(), 4096),
    )
    .await
    .expect("the request deadline ends the stream")
    .unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("[DONE]"));
    assert_eq!(counts.current("d"), 1);
}

/// SPEC §10 (amended 2026-10-01): a hung-up stream is cancelled upstream.
/// Without a durable ledger the in-memory slot is its only record, so it
/// stays charged, as for an uncertain end.
// T17 T38
#[tokio::test]
async fn a_hung_up_stream_without_a_ledger_keeps_its_slot() {
    let counts = Arc::new(InFlight::default());
    let response = response(
        Forward {
            chunks: 100,
            chunk_bytes: 1,
            finish: Ok(StreamEnded::Completed),
            finish_gate: None,
            cancel_on_failure: true,
        },
        &counts,
    );
    let mut body = response.into_body().into_data_stream();
    assert!(body.next().await.unwrap().is_ok());
    drop(body);
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    assert_eq!(counts.current("d"), 1);
}

// T17 T38, SPEC §10 (amended): the lease of a stream whose client hung up
// is cancelling, never completed; a cut stream stays uncertain.
#[test]
fn a_hung_up_stream_closes_its_lease_as_cancelling() {
    use capyctl_controller::LeaseEnd;
    use capyctl_router::stream::stream_lease_end;
    assert_eq!(
        stream_lease_end(&Ok(Ok(StreamEnded::Cancelled))),
        LeaseEnd::Cancelling
    );
    assert_eq!(
        stream_lease_end(&Ok(Ok(StreamEnded::Completed))),
        LeaseEnd::Completed
    );
    assert_eq!(stream_lease_end(&Err(())), LeaseEnd::Uncertain);
}

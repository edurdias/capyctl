//! SPEC §17 (owner decision 2026-09-23, M80): the router times every request
//! on its own clock and keeps bounded per-instance distributions; with the
//! timing header enabled each response carries its own timings.
//!
//! A fake engine with injected delays stands in for the backend, so the
//! expected phases are known. Fake engines are not qualification.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use capyctl_adapters::traits::{AdapterError, ChatForward, ChatSink, StreamEnded};
use capyctl_controller::Controller;
use capyctl_router::admission::InFlight;
use capyctl_router::forwarders::StaticForwarders;
use capyctl_router::timing::{latency_report, TIMING_HEADER};
use capyctl_router::{QueueLimits, RouterDeps};
use capyctl_store::Store;
use capyctl_testkit::FakeEngine;
use tower::ServiceExt;

/// A backend whose response timing is fixed: `first` before the first chunk
/// (role only), `content` more before the first text chunk, `tail` more before
/// the last chunk. A non-streaming request answers after all three.
struct DelayedEngine {
    first: Duration,
    content: Duration,
    tail: Duration,
}

#[async_trait]
impl ChatForward for DelayedEngine {
    async fn forward_chat(
        &self,
        _body: &serde_json::Value,
    ) -> Result<serde_json::Value, AdapterError> {
        tokio::time::sleep(self.first + self.content + self.tail).await;
        Ok(serde_json::json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}))
    }

    async fn forward_chat_stream_async(
        &self,
        _body: &serde_json::Value,
        sink: &mut dyn ChatSink,
    ) -> Result<StreamEnded, AdapterError> {
        tokio::time::sleep(self.first).await;
        let _ = sink
            .send(r#"{"choices":[{"delta":{"role":"assistant"}}]}"#.into())
            .await;
        tokio::time::sleep(self.content).await;
        let _ = sink
            .send(r#"{"choices":[{"delta":{"content":"Hello"}}]}"#.into())
            .await;
        tokio::time::sleep(self.tail).await;
        let _ = sink
            .send(r#"{"choices":[{"delta":{"content":"!"},"finish_reason":"stop"}]}"#.into())
            .await;
        Ok(StreamEnded::Completed)
    }
}

async fn app(header: bool) -> (axum::Router, Arc<InFlight>, String) {
    let shared = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let fake = Arc::new(FakeEngine::new());
    let controller = Arc::new(Controller::new(
        shared,
        fake as Arc<dyn capyctl_adapters::EngineAdapter>,
        Arc::new(capyctl_testkit::FakeLauncher::new()),
    ));
    let engine = Arc::new(DelayedEngine {
        first: Duration::from_millis(30),
        content: Duration::from_millis(60),
        tail: Duration::from_millis(30),
    });
    let inflight = Arc::new(InFlight::default());
    inflight.latency.set_timing_header(header);
    let deps = RouterDeps {
        controller: controller.clone(),
        forwards: Arc::new(StaticForwarders(HashMap::from([(
            "fake".to_string(),
            engine as Arc<dyn ChatForward>,
        )]))),
        limits: QueueLimits {
            max_requests_per_deployment: 8,
            max_buffered_bytes_total: 64 * 1024,
        },
        api_key: Some("test-key".into()),
        inflight: inflight.clone(),
        activation_join: Arc::new(capyctl_router::WakeJoin::new()),
    };
    let id = controller
        .submit_deploy(capyctl_controller::DeployRequest {
            name: "timed".into(),
            kind: "fake".into(),
            manifest: br#"{"kind":"model","name":"timed"}"#.to_vec(),
            route_model_id: Some("timed".into()),
        })
        .await
        .unwrap();
    let op = controller
        .request_transition(&id, capyctl_domain::LifecycleAction::Start)
        .await
        .unwrap();
    controller.wait_terminal(&op).await.unwrap();
    (capyctl_router::serve_router(deps), inflight, id)
}

fn chat(stream: bool) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", "Bearer test-key")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            serde_json::json!({"model": "timed", "stream": stream,
                "messages": [{"role": "user", "content": "hi"}]})
            .to_string(),
        ))
        .unwrap()
}

fn series<'a>(report: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    report["deployments"][0]["instances"][0]["series"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("no series {name} in {report}"))
}

fn p(series: &serde_json::Value, q: &str) -> f64 {
    series[format!("{q}_seconds")].as_f64().unwrap()
}

// SPEC §17 T38: streaming phases land in the right buckets; first byte, first
// content and last chunk are told apart; only completed requests count; the
// timing comment carries this request's exact values.
#[tokio::test]
async fn streaming_phases_follow_injected_delays() {
    let (router, inflight, id) = app(true).await;
    for _ in 0..6 {
        let res = router.clone().oneshot(chat(true)).await.unwrap();
        assert_eq!(res.status(), 200);
        let early: serde_json::Value =
            serde_json::from_str(res.headers()[TIMING_HEADER].to_str().unwrap()).unwrap();
        // Before the first byte only the pre-forward phases are known.
        assert!(early["selection_ms"].is_number() && early["total_ms"].is_null());
        let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        let comment = text
            .lines()
            .find_map(|l| l.strip_prefix(&format!(": {TIMING_HEADER} ")))
            .unwrap_or_else(|| panic!("no timing comment: {text}"));
        let own: serde_json::Value = serde_json::from_str(comment).unwrap();
        let ms = |k: &str| own[k].as_f64().unwrap();
        assert!(ms("time_to_first_byte_ms") >= 30.0, "{own}");
        assert!(ms("time_to_first_content_ms") >= 90.0, "{own}");
        assert!(ms("time_to_last_chunk_ms") >= 120.0, "{own}");
        assert!(ms("total_ms") >= ms("time_to_last_chunk_ms"));
        assert!(ms("upstream_first_byte_ms") <= ms("time_to_first_byte_ms"));
        assert_eq!(own["engine"], "fake");
        let data: Vec<&str> = text
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .collect();
        assert_eq!(
            data.last(),
            Some(&"[DONE]"),
            "the terminal still ends the stream"
        );
    }
    let report = latency_report(&inflight.latency, &[], Some(&id));
    assert_eq!(report["deployments"][0]["deployment_id"], id.as_str());
    let first_byte = series(&report, "router_time_to_first_byte");
    assert_eq!(first_byte["count"], 6);
    assert_eq!(
        (first_byte["tier"].as_str(), first_byte["source"].as_str()),
        (Some("router"), Some("capyctl"))
    );
    // Bucket estimates: 30 ms lies in (0.03, 0.05].
    assert!(
        (0.03..=0.05).contains(&p(first_byte, "p50")),
        "{first_byte}"
    );
    let content = series(&report, "router_time_to_first_content");
    assert!((0.07..=0.15).contains(&p(content, "p50")), "{content}");
    let last = series(&report, "router_time_to_last_chunk");
    assert!((0.1..=0.2).contains(&p(last, "p95")), "{last}");
    // Pre-forward work is small next to the engine's time.
    assert!(p(series(&report, "router_pre_forward"), "p99") < 0.03);
    assert_eq!(series(&report, "router_queue_wait")["count"], 6);
    // Nothing waited for an activation.
    assert!(report["deployments"][0]["instances"][0]["series"]
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s["name"] != "router_activation_wait"));
}

// SPEC §17 T38: a non-streaming response's timings are its header, off unless
// the server enables it; the distribution is kept either way.
#[tokio::test]
async fn non_streaming_timing_header_is_opt_in() {
    let (router, inflight, id) = app(false).await;
    let res = router.clone().oneshot(chat(false)).await.unwrap();
    assert_eq!(res.status(), 200);
    assert!(res.headers().get(TIMING_HEADER).is_none(), "off by default");
    inflight.latency.set_timing_header(true);
    let res = router.clone().oneshot(chat(false)).await.unwrap();
    let own: serde_json::Value =
        serde_json::from_str(res.headers()[TIMING_HEADER].to_str().unwrap()).unwrap();
    assert!(
        own["time_to_last_chunk_ms"].as_f64().unwrap() >= 120.0,
        "{own}"
    );
    assert!(
        own["time_to_first_byte_ms"].is_null(),
        "a whole response has no chunks"
    );
    let report = latency_report(&inflight.latency, &[], Some(&id));
    let total = series(&report, "router_total");
    assert_eq!(total["count"], 2);
    assert!((0.1..=0.2).contains(&p(total, "p50")), "{total}");
    // Another deployment's filter reads nothing.
    let none = latency_report(&inflight.latency, &[], Some("other"));
    assert_eq!(none["deployments"], serde_json::json!([]));
}

// SPEC §17 T18: host-reported series join the router's under the same
// instance incarnation, marked by tier and source.
#[tokio::test]
async fn host_series_are_grouped_with_the_router_series() {
    use capyctl_controller::latency_table::LatencyTable;
    use capyctl_protocol::reports::{LoadReport, LoadSample, SampleLatency};
    let (router, inflight, id) = app(false).await;
    router.clone().oneshot(chat(false)).await.unwrap();
    let mut engine = capyctl_domain::latency::Histogram::new(&[0.1, 0.5]).unwrap();
    engine.observe(0.2);
    let mut ingress = capyctl_domain::latency::Histogram::capyctl();
    ingress.observe(0.121);
    let hosts = LatencyTable::new();
    hosts.accept(
        "h1",
        &LoadReport {
            host_id: "h1".into(),
            samples: vec![LoadSample {
                deployment_id: id.clone(),
                generation: 7,
                owned_handle: "launch".into(),
                sampled_at_ms: 1,
                ingress_in_flight: 0,
                engine: None,
                latency: Some(SampleLatency {
                    engine: Some("vllm".into()),
                    histograms: vec![
                        ("engine_time_to_first_token".into(), engine),
                        ("ingress_time_to_last_byte".into(), ingress),
                    ],
                }),
            }],
        },
        1,
    );
    let report = latency_report(&inflight.latency, &hosts.snapshot(Some(&id)), Some(&id));
    let instances = report["deployments"][0]["instances"].as_array().unwrap();
    let host = instances.iter().find(|i| i["generation"] == 7).unwrap();
    assert_eq!(
        (host["engine"].as_str(), host["host_id"].as_str()),
        (Some("vllm"), Some("h1"))
    );
    let tiers: Vec<(&str, &str)> = host["series"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["tier"].as_str().unwrap(), s["source"].as_str().unwrap()))
        .collect();
    assert!(tiers.contains(&("engine", "engine")) && tiers.contains(&("ingress", "capyctl")));
    let buckets = host["series"][0]["buckets"].as_array().unwrap();
    assert!(buckets.iter().all(|b| b["count"].as_u64().unwrap() > 0));
    assert_eq!(report["bucket_counts"], "non_cumulative");
}

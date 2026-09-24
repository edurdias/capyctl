//! SPEC §10, ADR 0013 §10 (D9): host engine load reporting. Fake engines serve
//! `/metrics` shaped as the pinned vLLM and SGLang sources emit them. These
//! are CPU tests with Fake engines; they are not native engine qualification.
use axum::{
    http::{HeaderMap, StatusCode},
    routing::get,
    Router,
};
use mllm_agent::{
    ingress::{Ingress, IngressScope},
    load::{self, LoadReporter},
};
use mllm_protocol::reports::{LoadReport, LoadSample, MAX_LOAD_SAMPLES};
use std::{net::SocketAddr, time::Duration};

/// vLLM exposition shape (two engine cores, histograms and look-alike names).
const VLLM_METRICS: &str = "\
# HELP vllm:num_requests_running Number of requests in model execution batches.
# TYPE vllm:num_requests_running gauge
vllm:num_requests_running{engine=\"0\",model_name=\"qwen3-4b\"} 3.0
vllm:num_requests_running{engine=\"1\",model_name=\"qwen3-4b\"} 2.0
# HELP vllm:num_requests_waiting Number of requests waiting to be processed.
# TYPE vllm:num_requests_waiting gauge
vllm:num_requests_waiting{engine=\"0\",model_name=\"qwen3-4b\"} 4.0
vllm:num_requests_waiting{engine=\"1\",model_name=\"qwen3-4b\"} 0.0
vllm:num_requests_waiting_by_reason{engine=\"0\",model_name=\"qwen3-4b\",reason=\"capacity\"} 99.0
# HELP vllm:kv_cache_usage_perc KV-cache usage. 1 means 100 percent usage.
# TYPE vllm:kv_cache_usage_perc gauge
vllm:kv_cache_usage_perc{engine=\"0\",model_name=\"qwen3-4b\"} 0.25
vllm:kv_cache_usage_perc{engine=\"1\",model_name=\"qwen3-4b\"} 0.5
vllm:request_success_total{engine=\"0\",finished_reason=\"stop\",model_name=\"qwen3-4b\"} 7.0
vllm:time_to_first_token_seconds_bucket{engine=\"0\",le=\"0.001\",model_name=\"qwen3-4b\"} 0.0
";

/// SGLang exposition shape (labels include a `}` inside a quoted value).
const SGLANG_METRICS: &str = "\
# HELP sglang:num_running_reqs The number of running requests.
# TYPE sglang:num_running_reqs gauge
sglang:num_running_reqs{model_name=\"weird}name\",dp_rank=\"0\"} 6.0
# HELP sglang:num_queue_reqs The number of requests in the waiting queue.
# TYPE sglang:num_queue_reqs gauge
sglang:num_queue_reqs{model_name=\"weird}name\",dp_rank=\"0\"} 1.0
# HELP sglang:token_usage The token usage.
# TYPE sglang:token_usage gauge
sglang:token_usage{model_name=\"weird}name\",dp_rank=\"0\"} 0.123456
sglang:full_token_usage{model_name=\"weird}name\",dp_rank=\"0\"} 0.9
";

// T37 / §17: exact pinned names, summed over engine series, KV as the most
// pressured pool; look-alike names never count.
#[test]
fn parses_the_pinned_vllm_and_sglang_gauges() {
    let vllm = load::parse_engine_load(VLLM_METRICS).unwrap();
    assert_eq!(
        (vllm.running, vllm.waiting, vllm.kv_usage_ppm),
        (5, 4, 500_000)
    );
    let sglang = load::parse_engine_load(SGLANG_METRICS).unwrap();
    assert_eq!(
        (sglang.running, sglang.waiting, sglang.kv_usage_ppm),
        (6, 1, 123_456)
    );
}

// §17: a missing, malformed, out-of-range or ambiguous gauge is no sample,
// never a guessed zero.
#[test]
fn missing_or_malformed_gauges_are_not_guessed() {
    let without_kv: String = VLLM_METRICS
        .lines()
        .filter(|l| !l.contains("kv_cache_usage_perc"))
        .map(|l| format!("{l}\n"))
        .collect();
    assert!(load::parse_engine_load(&without_kv).is_none());
    // SGLang without `enable_metrics` serves no gauges at all.
    assert!(load::parse_engine_load("").is_none());
    assert!(load::parse_engine_load("# only comments\n").is_none());
    for bad in [
        "vllm:num_requests_running{engine=\"0\"} NaN\n",
        "vllm:num_requests_running{engine=\"0\"} -1\n",
        "vllm:num_requests_running{engine=\"0\"} many\n",
        "vllm:num_requests_running{engine=\"0\" 3\n",
        "vllm:num_requests_running{engine=\"0\"} 9999999999\n",
    ] {
        let text = format!("{bad}vllm:num_requests_waiting 0\nvllm:kv_cache_usage_perc 0.1\n");
        assert!(load::parse_engine_load(&text).is_none(), "{bad}");
    }
    let overfull =
        "vllm:num_requests_running 1\nvllm:num_requests_waiting 0\nvllm:kv_cache_usage_perc 1.5\n";
    assert!(load::parse_engine_load(overfull).is_none());
    // Both families at once is ambiguous.
    assert!(load::parse_engine_load(&format!("{VLLM_METRICS}{SGLANG_METRICS}")).is_none());
    // A timestamped unlabeled sample parses.
    let plain = "vllm:num_requests_running 2 1700000000000\nvllm:num_requests_waiting 1\nvllm:kv_cache_usage_perc 0\n";
    assert_eq!(load::parse_engine_load(plain).unwrap().running, 2);
}

// §17: the reporting period stays within the D9 bounds.
#[test]
fn reporting_period_bounds() {
    assert!(load::load_interval(Duration::from_millis(249)).is_err());
    assert!(load::load_interval(Duration::from_millis(5_001)).is_err());
    assert_eq!(
        load::load_interval(Duration::from_millis(250)).unwrap(),
        Duration::from_millis(250)
    );
    assert_eq!(load::DEFAULT_LOAD_INTERVAL, Duration::from_secs(1));
}

fn scope(deployment: &str, generation: i64) -> IngressScope {
    IngressScope {
        host_id: "host".into(),
        deployment_id: deployment.into(),
        binding_id: format!("binding-{generation}"),
        incarnation: format!("incarnation-{generation}"),
        member_id: "head".into(),
        generation,
        revision: generation,
        instance_index: 0,
    }
}

/// A fake engine whose `/metrics` requires its native key, as a keyed guard may.
async fn engine(native: [u8; 32], body: &'static str, delay: Duration) -> SocketAddr {
    let expected = format!("Bearer {}", hex::encode(native));
    let router = Router::new().route(
        "/metrics",
        get(move |headers: HeaderMap| {
            let expected = expected.clone();
            async move {
                tokio::time::sleep(delay).await;
                if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some(&expected) {
                    return (StatusCode::UNAUTHORIZED, String::new());
                }
                (StatusCode::OK, body.to_owned())
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    address
}

fn register(
    ingress: &Ingress,
    scope: &IngressScope,
    target: SocketAddr,
    gate: u8,
    native: [u8; 32],
) {
    ingress
        .register(scope.clone(), target, "model".into(), [gate; 32], native)
        .unwrap();
}

fn samples(reports: Vec<mllm_protocol::pb::ReportLoad>) -> Vec<LoadSample> {
    let mut all: Vec<_> = reports
        .into_iter()
        .flat_map(|r| {
            let report = LoadReport::try_from(r).unwrap();
            assert_eq!(report.host_id, "host");
            report.samples
        })
        .collect();
    all.sort_by(|a, b| a.deployment_id.cmp(&b.deployment_id));
    all
}

// T18 / T34 / T37: only open, handle-bound scopes report; each sample carries
// its generation, owned handle and ingress in-flight. The scrape uses the
// launch's native key on loopback; an engine without metrics, a wrong key or a
// scrape slower than the bound reports `scrape_ok = false`, never zero.
#[tokio::test]
async fn reports_open_scopes_with_scraped_gauges_or_a_failed_scrape() {
    let ingress = Ingress::new().unwrap();
    let vllm = scope("vllm", 3);
    let sglang = scope("sglang", 2);
    let slow = scope("slow", 1);
    let keyed = scope("keyed", 1);
    let closed = scope("closed", 1);
    let unbound = scope("unbound", 1);
    register(
        &ingress,
        &vllm,
        engine([2; 32], VLLM_METRICS, Duration::ZERO).await,
        1,
        [2; 32],
    );
    register(
        &ingress,
        &sglang,
        engine([4; 32], SGLANG_METRICS, Duration::ZERO).await,
        3,
        [4; 32],
    );
    register(
        &ingress,
        &slow,
        engine([6; 32], VLLM_METRICS, Duration::from_millis(800)).await,
        5,
        [6; 32],
    );
    // The engine expects a different key than the launch holds.
    register(
        &ingress,
        &keyed,
        engine([9; 32], VLLM_METRICS, Duration::ZERO).await,
        7,
        [8; 32],
    );
    register(
        &ingress,
        &closed,
        engine([11; 32], VLLM_METRICS, Duration::ZERO).await,
        10,
        [11; 32],
    );
    register(
        &ingress,
        &unbound,
        engine([13; 32], VLLM_METRICS, Duration::ZERO).await,
        12,
        [13; 32],
    );
    for (s, handle) in [
        (&vllm, "launch-v"),
        (&sglang, "launch-s"),
        (&slow, "launch-slow"),
        (&keyed, "launch-k"),
        (&closed, "launch-c"),
    ] {
        ingress.bind_handle(s, handle).unwrap();
    }
    for s in [&vllm, &sglang, &slow, &keyed, &unbound] {
        ingress.open(s).unwrap();
    }
    // A handle binds only to the exact current scope.
    assert!(ingress.bind_handle(&scope("vllm", 4), "other").is_err());
    let reporter = LoadReporter::new(ingress.clone(), "host".into()).unwrap();
    let started = std::time::Instant::now();
    let all = samples(reporter.reports().await);
    assert!(
        started.elapsed() < Duration::from_millis(700),
        "scrapes run concurrently within the bound"
    );
    let names: Vec<_> = all.iter().map(|s| s.deployment_id.as_str()).collect();
    assert_eq!(names, ["keyed", "sglang", "slow", "vllm"]);
    let by = |name: &str| {
        all.iter()
            .find(|s| s.deployment_id == name)
            .unwrap()
            .clone()
    };
    let v = by("vllm");
    assert_eq!(
        (v.generation, v.owned_handle.as_str(), v.ingress_in_flight),
        (3, "launch-v", 0)
    );
    assert_eq!(v.engine.unwrap().running, 5);
    assert_eq!(by("sglang").engine.unwrap().kv_usage_ppm, 123_456);
    assert!(by("slow").engine.is_none());
    assert!(by("keyed").engine.is_none());
    // Losing the session closes every gate: nothing is Ready, nothing reports.
    ingress.close_all().unwrap();
    assert!(reporter.reports().await.is_empty());
    // A new generation replaces the entry; until its launch binds a handle and
    // opens, the old generation is gone and the new one does not report.
    let next = scope("vllm", 4);
    register(
        &ingress,
        &next,
        engine([15; 32], VLLM_METRICS, Duration::ZERO).await,
        14,
        [15; 32],
    );
    ingress.open(&next).unwrap();
    assert!(reporter.reports().await.is_empty());
    ingress.bind_handle(&next, "launch-v4").unwrap();
    let after = samples(reporter.reports().await);
    assert_eq!((after.len(), after[0].generation), (1, 4));
}

// §17: many Ready scopes split into reports that each pass the W3 count and
// size bounds.
#[tokio::test]
async fn many_scopes_split_into_bounded_reports() {
    let ingress = Ingress::new().unwrap();
    let target: SocketAddr = "127.0.0.1:9".parse().unwrap();
    for i in 0..100u8 {
        let s = scope(&format!("d{i:03}"), 1);
        let mut gate = [i; 32];
        gate[0] = 0xAA;
        ingress
            .register(s.clone(), target, "model".into(), gate, [0xBB; 32])
            .unwrap();
        ingress.bind_handle(&s, &format!("launch-{i}")).unwrap();
        ingress.open(&s).unwrap();
    }
    let reporter = LoadReporter::new(ingress.clone(), "host".into()).unwrap();
    let reports = reporter.reports().await;
    assert!(reports.len() >= 2);
    assert!(reports.iter().all(|r| r.samples.len() <= MAX_LOAD_SAMPLES));
    let all = samples(reports);
    assert_eq!(all.len(), 100);
    // Nothing listens on the discard port: every sample is a failed scrape.
    assert!(all.iter().all(|s| s.engine.is_none()));
    // Oversized owned handles still batch within the byte bound.
    let big: Vec<LoadSample> = (0..20)
        .map(|i| LoadSample {
            deployment_id: format!("d{i}"),
            generation: 1,
            owned_handle: "h".repeat(4000),
            sampled_at_ms: 1,
            ingress_in_flight: 0,
            engine: None,
            latency: None,
        })
        .collect();
    let batched = load::batch("host", big);
    assert!(batched.len() >= 3);
    assert_eq!(samples(batched).len(), 20);
}

// T37 / M08 / SPEC §13.3: engine metrics are never reachable through ingress.
#[tokio::test]
async fn ingress_never_forwards_metrics() {
    let ingress = Ingress::new().unwrap();
    let s = scope("d", 1);
    register(
        &ingress,
        &s,
        engine([2; 32], VLLM_METRICS, Duration::ZERO).await,
        1,
        [2; 32],
    );
    ingress.open(&s).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = ingress.clone().router();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = reqwest::Client::new();
    for token in [hex::encode([1; 32]), hex::encode([2; 32])] {
        let get = client
            .get(format!("http://{address}/metrics"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        let post = client
            .post(format!("http://{address}/metrics"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        for response in [get, post] {
            assert!(matches!(
                response.status(),
                StatusCode::NOT_FOUND | StatusCode::FORBIDDEN
            ));
            assert!(!response.text().await.unwrap().contains("vllm:"));
        }
    }
}

/// vLLM 0.29.0 histogram exposition shape (`vllm/v1/metrics/loggers.py`,
/// labels `model_name` and `engine`; two engine cores). Fake data.
const VLLM_HISTOGRAMS: &str = "\
vllm:num_requests_running{engine=\"0\",model_name=\"m\"} 1.0
vllm:num_requests_waiting{engine=\"0\",model_name=\"m\"} 0.0
vllm:kv_cache_usage_perc{engine=\"0\",model_name=\"m\"} 0.1
# HELP vllm:time_to_first_token_seconds Histogram of time to first token in seconds.
# TYPE vllm:time_to_first_token_seconds histogram
vllm:time_to_first_token_seconds_bucket{engine=\"0\",le=\"0.01\",model_name=\"m\"} 1.0
vllm:time_to_first_token_seconds_bucket{engine=\"0\",le=\"0.1\",model_name=\"m\"} 3.0
vllm:time_to_first_token_seconds_bucket{engine=\"0\",le=\"+Inf\",model_name=\"m\"} 4.0
vllm:time_to_first_token_seconds_count{engine=\"0\",model_name=\"m\"} 4.0
vllm:time_to_first_token_seconds_sum{engine=\"0\",model_name=\"m\"} 1.5
vllm:time_to_first_token_seconds_bucket{engine=\"1\",le=\"0.01\",model_name=\"m\"} 0.0
vllm:time_to_first_token_seconds_bucket{engine=\"1\",le=\"0.1\",model_name=\"m\"} 1.0
vllm:time_to_first_token_seconds_bucket{engine=\"1\",le=\"+Inf\",model_name=\"m\"} 1.0
vllm:time_to_first_token_seconds_count{engine=\"1\",model_name=\"m\"} 1.0
vllm:time_to_first_token_seconds_sum{engine=\"1\",model_name=\"m\"} 0.05
vllm:time_to_first_token_seconds_created{engine=\"0\",model_name=\"m\"} 1.7e9
vllm:e2e_request_latency_seconds_bucket{engine=\"0\",le=\"1.0\",model_name=\"m\"} 2.0
vllm:e2e_request_latency_seconds_bucket{engine=\"0\",le=\"+Inf\",model_name=\"m\"} 5.0
vllm:e2e_request_latency_seconds_count{engine=\"0\",model_name=\"m\"} 5.0
vllm:e2e_request_latency_seconds_sum{engine=\"0\",model_name=\"m\"} 9.0
vllm:request_queue_time_seconds_bucket{engine=\"0\",le=\"0.3\",model_name=\"m\"} 5.0
vllm:request_queue_time_seconds_bucket{engine=\"0\",le=\"+Inf\",model_name=\"m\"} 5.0
vllm:request_queue_time_seconds_count{engine=\"0\",model_name=\"m\"} 5.0
vllm:request_queue_time_seconds_sum{engine=\"0\",model_name=\"m\"} 0.01
vllm:request_prefill_time_seconds_bucket{engine=\"0\",le=\"0.3\",model_name=\"m\"} 5.0
vllm:request_prefill_time_seconds_bucket{engine=\"0\",le=\"+Inf\",model_name=\"m\"} 5.0
vllm:request_prefill_time_seconds_count{engine=\"0\",model_name=\"m\"} 5.0
vllm:request_prefill_time_seconds_sum{engine=\"0\",model_name=\"m\"} 0.4
vllm:request_decode_time_seconds_bucket{engine=\"0\",le=\"0.3\",model_name=\"m\"} 0.0
vllm:request_decode_time_seconds_bucket{engine=\"0\",le=\"+Inf\",model_name=\"m\"} 5.0
vllm:request_decode_time_seconds_count{engine=\"0\",model_name=\"m\"} 5.0
vllm:request_decode_time_seconds_sum{engine=\"0\",model_name=\"m\"} 8.0
vllm:inter_token_latency_seconds_bucket{engine=\"0\",le=\"0.025\",model_name=\"m\"} 90.0
vllm:inter_token_latency_seconds_bucket{engine=\"0\",le=\"+Inf\",model_name=\"m\"} 100.0
vllm:inter_token_latency_seconds_count{engine=\"0\",model_name=\"m\"} 100.0
vllm:inter_token_latency_seconds_sum{engine=\"0\",model_name=\"m\"} 2.2
";

/// SGLang 0.5.20 shape (`sglang/srt/observability/metrics_collector.py`):
/// `is_streaming` splits TTFT and end-to-end; `queue_time` starts at a 0.0
/// bucket; no prefill or decode histogram. Fake data.
const SGLANG_HISTOGRAMS: &str = "\
sglang:num_running_reqs{model_name=\"m\"} 0.0
sglang:num_queue_reqs{model_name=\"m\"} 0.0
sglang:token_usage{model_name=\"m\"} 0.0
sglang:time_to_first_token_seconds_bucket{is_streaming=\"true\",le=\"0.1\",model_name=\"m\"} 2.0
sglang:time_to_first_token_seconds_bucket{is_streaming=\"true\",le=\"+Inf\",model_name=\"m\"} 2.0
sglang:time_to_first_token_seconds_count{is_streaming=\"true\",model_name=\"m\"} 2.0
sglang:time_to_first_token_seconds_sum{is_streaming=\"true\",model_name=\"m\"} 0.1
sglang:time_to_first_token_seconds_bucket{is_streaming=\"false\",le=\"0.1\",model_name=\"m\"} 0.0
sglang:time_to_first_token_seconds_bucket{is_streaming=\"false\",le=\"+Inf\",model_name=\"m\"} 1.0
sglang:time_to_first_token_seconds_count{is_streaming=\"false\",model_name=\"m\"} 1.0
sglang:time_to_first_token_seconds_sum{is_streaming=\"false\",model_name=\"m\"} 0.9
sglang:queue_time_seconds_bucket{le=\"0.0\",model_name=\"m\"} 1.0
sglang:queue_time_seconds_bucket{le=\"0.001\",model_name=\"m\"} 3.0
sglang:queue_time_seconds_bucket{le=\"+Inf\",model_name=\"m\"} 3.0
sglang:queue_time_seconds_count{model_name=\"m\"} 3.0
sglang:queue_time_seconds_sum{model_name=\"m\"} 0.0015
sglang:inter_token_latency_seconds_bucket{le=\"0.02\",model_name=\"m\"} 10.0
sglang:inter_token_latency_seconds_bucket{le=\"+Inf\",model_name=\"m\"} 10.0
sglang:inter_token_latency_seconds_count{model_name=\"m\"} 10.0
sglang:inter_token_latency_seconds_sum{model_name=\"m\"} 0.1
";

// SPEC §17 T37 (M80): the pinned vLLM and SGLang latency histograms parse,
// summed over label sets per `le`; each family forwards only what it exposes.
#[test]
fn parses_the_pinned_engine_latency_histograms() {
    let (engine, vllm) = load::parse_engine_histograms(VLLM_HISTOGRAMS).unwrap();
    assert_eq!(engine, "vllm");
    let names: Vec<&str> = vllm.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "engine_time_to_first_token",
            "engine_e2e_request_latency",
            "engine_queue_time",
            "engine_prefill_time",
            "engine_decode_time",
            "engine_inter_token_latency",
        ]
    );
    let ttft = &vllm[0].1;
    assert_eq!((ttft.bounds(), ttft.counts(), ttft.count()), (&[0.01, 0.1][..], &[1u64, 3, 1][..], 5));
    assert!((ttft.sum() - 1.55).abs() < 1e-9);
    let (engine, sglang) = load::parse_engine_histograms(SGLANG_HISTOGRAMS).unwrap();
    assert_eq!(engine, "sglang");
    let names: Vec<&str> = sglang.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["engine_time_to_first_token", "engine_queue_time", "engine_inter_token_latency"]);
    assert_eq!(sglang[0].1.counts(), &[2, 1]);
    assert_eq!((sglang[1].1.bounds(), sglang[1].1.counts()), (&[0.0, 0.001][..], &[1u64, 2, 0][..]));
    // Gauges of neither or both families: no engine histograms at all.
    assert!(load::parse_engine_histograms("").is_none());
    assert!(load::parse_engine_histograms(&format!("{VLLM_HISTOGRAMS}{SGLANG_HISTOGRAMS}")).is_none());
}

// SPEC §17: a malformed or inconsistent histogram is dropped, never guessed.
#[test]
fn malformed_engine_histograms_are_dropped() {
    let metric = "vllm:time_to_first_token_seconds";
    for bad in [
        // `_count` disagrees with the +Inf bucket.
        "m_bucket{le=\"1\"} 1\nm_bucket{le=\"+Inf\"} 2\nm_count 3\nm_sum 1\n",
        // Cumulative counts go down.
        "m_bucket{le=\"1\"} 3\nm_bucket{le=\"2\"} 1\nm_bucket{le=\"+Inf\"} 3\nm_count 3\nm_sum 1\n",
        // No `le` label.
        "m_bucket{x=\"1\"} 1\nm_count 1\nm_sum 1\n",
        // Not a number.
        "m_bucket{le=\"1\"} NaN\nm_bucket{le=\"+Inf\"} 1\nm_count 1\nm_sum 1\n",
    ] {
        let text = bad.replace("m_", &format!("{metric}_"));
        assert!(load::parse_histogram(&text, metric).is_none(), "{bad}");
    }
    let too_many: String = (0..70)
        .map(|i| format!("{metric}_bucket{{le=\"{i}\"}} 0\n"))
        .collect::<String>()
        + &format!("{metric}_bucket{{le=\"+Inf\"}} 0\n{metric}_count 0\n{metric}_sum 0\n");
    assert!(load::parse_histogram(&too_many, metric).is_none(), "bucket count is bounded");
}

// SPEC §17 T18 (M80): the first report of a launch carries its engine
// histograms since start with the family named; the next tick carries only
// growth, so an unchanged engine adds nothing.
#[tokio::test]
async fn engine_histograms_are_reported_as_deltas() {
    let ingress = Ingress::new().unwrap();
    let s = scope("timed", 1);
    register(&ingress, &s, engine([2; 32], VLLM_HISTOGRAMS, Duration::ZERO).await, 1, [2; 32]);
    ingress.bind_handle(&s, "launch-t").unwrap();
    ingress.open(&s).unwrap();
    let reporter = LoadReporter::new(ingress.clone(), "host".into()).unwrap();
    let first = samples(reporter.reports().await);
    let latency = first[0].latency.clone().unwrap();
    assert_eq!(latency.engine.as_deref(), Some("vllm"));
    assert_eq!(latency.histograms.len(), 6);
    let second = samples(reporter.reports().await);
    let latency = second[0].latency.clone().unwrap();
    assert_eq!((latency.engine.as_deref(), latency.histograms.len()), (Some("vllm"), 0));
}

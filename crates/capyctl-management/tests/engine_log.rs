//! SPEC §13.3 / T21: `GET /management/v1/deployments/{id}/engine-log` serves
//! a bounded, redacted tail of one instance's engine log to the management
//! credential only. The source here reads real files through the same
//! bounded reader a host uses; a raw development log is never served, and a
//! host without the capability is refused typed (T34).
use capyctl_controller::engine_logs::{local_tail, TailFailure};
use capyctl_management::{
    engine_log::{
        engine_log_router, EngineLog, EngineLogFailure, EngineLogFuture, EngineLogSource,
        InstancesFuture,
    },
    ManagementCredentials,
};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, sync::Mutex};
use tower::ServiceExt;

const MANAGEMENT: &str = "management-token-0123456789abcdefghijklmnop";
const INFERENCE: &str = "inference-token-0123456789abcdefghijklmnopq";

/// What one instance answers.
#[derive(Clone)]
enum Answer {
    Log(PathBuf),
    Fail(EngineLogFailure),
}

#[derive(Default)]
struct Fake {
    deployments: BTreeMap<String, BTreeMap<u32, Answer>>,
    asked: Mutex<Vec<(String, u32, usize)>>,
}

impl EngineLogSource for Fake {
    fn instances(&self, deployment: &str) -> InstancesFuture {
        let found = self
            .deployments
            .get(deployment)
            .map(|instances| instances.keys().copied().collect())
            .ok_or(EngineLogFailure::NotFound);
        Box::pin(async move { found })
    }
    fn tail(&self, deployment: &str, instance: u32, max_bytes: usize) -> EngineLogFuture {
        self.asked
            .lock()
            .unwrap()
            .push((deployment.to_owned(), instance, max_bytes));
        let answer = self.deployments[deployment][&instance].clone();
        Box::pin(async move {
            match answer {
                Answer::Log(path) => Ok(EngineLog {
                    host_id: "host-a".into(),
                    incarnation: "01K00000000000000000000002".into(),
                    tail: local_tail(&path, max_bytes).map_err(EngineLogFailure::Tail)?,
                }),
                Answer::Fail(failure) => Err(failure),
            }
        })
    }
}

fn router(fake: Arc<Fake>) -> axum::Router {
    engine_log_router(
        ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap(),
        fake,
    )
}

fn get(uri: &str, token: Option<&str>) -> axum::http::Request<axum::body::Body> {
    let mut request = axum::http::Request::builder().method("GET").uri(uri);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    request.body(axum::body::Body::empty()).unwrap()
}

async fn call(router: &axum::Router, uri: &str) -> (u16, serde_json::Value) {
    let response = router
        .clone()
        .oneshot(get(uri, Some(MANAGEMENT)))
        .await
        .unwrap();
    let status = response.status().as_u16();
    assert_eq!(response.headers()["cache-control"], "no-store");
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

fn one(log: PathBuf) -> Arc<Fake> {
    Arc::new(Fake {
        deployments: BTreeMap::from([("dep".to_owned(), BTreeMap::from([(0, Answer::Log(log))]))]),
        ..Default::default()
    })
}

fn log_with(lines: &str) -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("01K00000000000000000000002.log");
    std::fs::write(&path, lines).unwrap();
    (directory, path)
}

// T21: only the management credential reads an engine log.
#[tokio::test]
async fn an_unauthenticated_read_is_refused() {
    let (_directory, path) = log_with("hello\n");
    let fake = one(path);
    let router = router(fake.clone());
    for token in [None, Some(INFERENCE)] {
        let response = router
            .clone()
            .oneshot(get("/management/v1/deployments/dep/engine-log", token))
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
    }
    assert!(fake.asked.lock().unwrap().is_empty(), "nothing was read");
}

// T21: `kib` is an integer in 1..=256 and defaults to 64; anything else in
// the query is refused before any read.
#[tokio::test]
async fn the_bound_is_validated() {
    let (_directory, path) = log_with("hello\n");
    let fake = one(path);
    let router = router(fake.clone());
    for query in [
        "kib=0",
        "kib=257",
        "kib=abc",
        "kib=1.5",
        "kib=-1",
        "kib=",
        "instance=x",
        "instance=0&instance=0",
        "kib=1&kib=2",
        "other=1",
    ] {
        let (status, body) = call(
            &router,
            &format!("/management/v1/deployments/dep/engine-log?{query}"),
        )
        .await;
        assert_eq!(status, 400, "{query}: {body}");
        assert_eq!(body["error"]["code"], "invalid_request", "{query}");
    }
    assert!(fake.asked.lock().unwrap().is_empty(), "nothing was read");
    let (status, body) = call(&router, "/management/v1/deployments/dep/engine-log").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["kib"], 64);
    let (status, body) = call(
        &router,
        "/management/v1/deployments/dep/engine-log?kib=256&instance=0",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["kib"], 256);
    assert_eq!(
        *fake.asked.lock().unwrap(),
        vec![("dep".into(), 0, 64 * 1024), ("dep".into(), 0, 256 * 1024)]
    );
}

// T21: a log longer than the bound returns at most `kib` KiB of whole lines,
// marked truncated, in the documented shape.
#[tokio::test]
async fn a_long_log_is_bounded_and_truncated() {
    // Ordinary words: a long unbroken run would be redacted as credential-shaped.
    let line = "the engine reported progress on its warmup step number seven\n";
    let (_directory, path) = log_with(&line.repeat(50));
    let router = router(one(path));
    let (status, body) = call(&router, "/management/v1/deployments/dep/engine-log?kib=1").await;
    assert_eq!(status, 200, "{body}");
    let text = body["text"].as_str().unwrap();
    assert!(text.len() <= 1024, "{}", text.len());
    assert!(!text.is_empty());
    assert!(
        text.lines().all(|l| l == line.trim_end()),
        "whole lines only"
    );
    assert_eq!(body["truncated"], true);
    assert_eq!(body["api_version"], "1");
    assert_eq!(body["deployment_id"], "dep");
    assert_eq!(body["instance"], 0);
    assert_eq!(body["host_id"], "host-a");
    assert_eq!(body["incarnation"], "01K00000000000000000000002");
    assert_eq!(body["kib"], 1);
    assert!(body["read_at_ms"].as_i64().unwrap() > 0);
    // A short log is whole and not truncated.
    let (_short, path) = log_with("one\ntwo\n");
    let (_, body) = call(
        &router_for(path),
        "/management/v1/deployments/dep/engine-log",
    )
    .await;
    assert_eq!(body["text"], "one\ntwo\n");
    assert_eq!(body["truncated"], false);
}

fn router_for(path: PathBuf) -> axum::Router {
    router(one(path))
}

// T21: credentials in the log never reach the response; the marker does.
#[tokio::test]
async fn the_tail_is_redacted() {
    let key = "0123456789abcdef".repeat(4);
    let signature = "f".repeat(64);
    let log = format!(
        "engine start key={key}\n\
         settings api_key=s3cr3t-value-here\n\
         fetch https://h/p?X-Amz-Signature={signature}&X-Amz-Credential=AKIDEXAMPLE\n\
         origin https://user:pass@h/\n\
         ready\n"
    );
    let (_directory, path) = log_with(&log);
    let router = router(one(path));
    let (status, body) = call(&router, "/management/v1/deployments/dep/engine-log").await;
    assert_eq!(status, 200, "{body}");
    let text = body["text"].as_str().unwrap();
    for secret in [
        key.as_str(),
        "s3cr3t-value-here",
        signature.as_str(),
        "AKIDEXAMPLE",
        "user:pass",
    ] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    assert!(text.contains("<redacted>"), "{text}");
    assert!(text.contains("ready"), "{text}");
}

// T21 (SPEC §13.3): a log written under --debug-engine-logs is raw
// development output and is refused, typed, without its contents.
#[tokio::test]
async fn a_raw_debug_log_is_refused() {
    let (_directory, path) = log_with("token=raw-secret-value\n");
    std::fs::write(capyctl_adapters::engine_log::raw_marker(&path), "").unwrap();
    let router = router(one(path.clone()));
    let (status, body) = call(&router, "/management/v1/deployments/dep/engine-log").await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["code"], "forbidden");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("--debug-engine-logs"), "{message}");
    let raw = body.to_string();
    assert!(!raw.contains("raw-secret-value"), "{raw}");
    assert!(!raw.contains(path.to_str().unwrap()), "no path: {raw}");
}

// T21: a missing log, an unknown deployment or instance and an instance with
// no running launch are `not_found`; a choice is required among several.
#[tokio::test]
async fn unknown_and_missing_are_not_found() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("absent.log");
    let fake = Arc::new(Fake {
        deployments: BTreeMap::from([
            (
                "dep".to_owned(),
                BTreeMap::from([(0, Answer::Log(missing.clone()))]),
            ),
            (
                "pair".to_owned(),
                BTreeMap::from([
                    (0, Answer::Fail(EngineLogFailure::NotRunning)),
                    (1, Answer::Log(missing.clone())),
                ]),
            ),
        ]),
        ..Default::default()
    });
    let router = router(fake.clone());
    let (status, body) = call(&router, "/management/v1/deployments/dep/engine-log").await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["error"]["code"], "not_found");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("no engine log exists"), "{message}");
    assert!(!body.to_string().contains(missing.to_str().unwrap()));
    for uri in [
        "/management/v1/deployments/other/engine-log",
        "/management/v1/deployments/dep/engine-log?instance=3",
        "/management/v1/deployments/pair/engine-log?instance=0",
    ] {
        let (status, body) = call(&router, uri).await;
        assert_eq!(status, 404, "{uri}: {body}");
        assert_eq!(body["error"]["code"], "not_found", "{uri}");
    }
    let (status, body) = call(&router, "/management/v1/deployments/pair/engine-log").await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["code"], "invalid_request");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("instance"));
}

// T34 (ADR 0017): a host that did not declare `engine_log_tail` is refused
// with the typed reason; an offline host or an unanswered read keeps its
// existing mapping. None of them carries text.
#[tokio::test]
async fn host_refusals_keep_their_typed_codes() {
    let cases = [
        (
            TailFailure::CapabilityMissing("host_capability_missing:engine_log_tail".into()),
            503,
            "unsupported_capability",
        ),
        (TailFailure::HostOffline, 503, "observation_stale"),
        (TailFailure::DeadlineExceeded, 504, "deadline_exceeded"),
        (TailFailure::Unreadable, 500, "internal"),
    ];
    for (failure, status, code) in cases {
        let fake = Arc::new(Fake {
            deployments: BTreeMap::from([(
                "dep".to_owned(),
                BTreeMap::from([(0, Answer::Fail(EngineLogFailure::Tail(failure.clone())))]),
            )]),
            ..Default::default()
        });
        let (got, body) = call(&router(fake), "/management/v1/deployments/dep/engine-log").await;
        assert_eq!(got, status, "{failure:?}: {body}");
        assert_eq!(body["error"]["code"], code, "{failure:?}");
        if let TailFailure::CapabilityMissing(reason) = &failure {
            let message = body["error"]["message"].as_str().unwrap();
            assert!(message.contains(reason.as_str()), "{message}");
        }
    }
}

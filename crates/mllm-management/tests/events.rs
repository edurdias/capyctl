use axum::{body::Body, http::Request};
use futures::StreamExt;
use mllm_management::{ManagementCredentials, StoreSnapshotSource, read_only_router};
use mllm_store::Store;
use std::sync::Arc;
use tower::ServiceExt;

const MANAGEMENT: &str = "management-credential-012345678901234567890";
const INFERENCE: &str = "inference-credential-0123456789012345678901";

#[path = "../../mllm-controller/tests/qualification_support/fixture.rs"]
mod qualification_fixture;

#[tokio::test]
async fn unarmed_stop_writer_events_replay_to_sse_without_cleanup_epoch() {
    let source = qualification_fixture::owned_source().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stop.sqlite3");
    std::fs::copy(source.dir.path().join("srv.sqlite3"), &path).unwrap();
    let writer = Store::open(&path).unwrap();
    let session = writer.begin_coordinator_session().unwrap();
    writer
        .accept_qualified_start(&session, &source.fence, 1800, 10000)
        .unwrap();
    let cursor = writer.snapshot().unwrap().cursor.to_string();
    let stop = writer
        .accept_ordinary_stop_command(
            &session,
            "owner",
            &source.fence.deployment_id,
            1,
            "stop",
            1900,
            10000,
        )
        .unwrap();
    writer
        .complete_unarmed_stop(&session, &stop.step_id)
        .unwrap();
    let app = read_only_router(
        ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap(),
        Arc::new(StoreSnapshotSource::new(Store::open(&path).unwrap())),
    );
    let response = app
        .oneshot(
            request(&format!("/management/v1/events?after={cursor}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let mut body = response.into_body().into_data_stream();
    for (kind, transition) in [
        ("ordinary_unarmed_stop_accepted", "unarmed_stop_accepted"),
        ("ordinary_unarmed_stop_completed", "unarmed_stop_completed"),
    ] {
        let frame = next(&mut body).await;
        assert!(frame.contains(&format!("event: {kind}\n")), "{frame}");
        assert!(
            frame.contains(&format!("\"transition\":\"{transition}\"")),
            "{frame}"
        );
        assert!(
            frame.contains(&format!("\"operation_id\":\"{}\"", stop.operation_id)),
            "{frame}"
        );
        assert!(frame.contains("\"committed_epoch\":null"), "{frame}");
    }
    writer.begin_coordinator_session().unwrap();
    assert!(next(&mut body)
        .await
        .contains("event: coordinator_session_started\n"));
}

#[tokio::test]
async fn expired_unarmed_writer_event_replays_and_stream_continues() {
    let source = qualification_fixture::owned_source().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("expiry.sqlite3");
    std::fs::copy(source.dir.path().join("srv.sqlite3"), &path).unwrap();
    let writer = Store::open(&path).unwrap();
    let session = writer.begin_coordinator_session().unwrap();
    let accepted = writer
        .accept_qualified_start(&session, &source.fence, 1800, 1900)
        .unwrap();
    let cursor = writer.snapshot().unwrap().cursor.to_string();
    writer
        .expire_unarmed_qualified_initialize(&session, &accepted.step_id, 1900)
        .unwrap();
    let app = read_only_router(
        ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap(),
        Arc::new(StoreSnapshotSource::new(Store::open(&path).unwrap())),
    );
    let response = app
        .oneshot(
            request(&format!("/management/v1/events?after={cursor}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let mut body = response.into_body().into_data_stream();
    let first = next(&mut body).await;
    assert!(
        first.contains("event: qualified_initialize_expired_unarmed\n"),
        "{first}"
    );
    assert!(first.contains(&format!("\"operation_id\":\"{}\"", accepted.operation_id)));
    assert!(first.contains("\"transition\":\"expired_unarmed\""));
    assert!(first.contains("\"committed_epoch\":null"));
    writer.begin_coordinator_session().unwrap();
    let second = next(&mut body).await;
    assert!(
        second.contains("event: coordinator_session_started\n"),
        "{second}"
    );
}

#[tokio::test]
async fn authenticated_replay_recovers_write_after_snapshot_and_follows_live() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.sqlite");
    let writer = Store::open(&path).unwrap();
    let cursor = writer.snapshot().unwrap().cursor.to_string();
    writer.begin_coordinator_session().unwrap();
    let app = read_only_router(
        ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap(),
        Arc::new(StoreSnapshotSource::new(Store::open(&path).unwrap())),
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/management/v1/events?after={cursor}"))
                .header("authorization", format!("Bearer {MANAGEMENT}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let mut body = response.into_body().into_data_stream();
    let first = next(&mut body).await;
    assert!(first.contains("event: coordinator_session_started\n"));
    assert!(first.contains(&format!("id: {}:1\n", cursor.split(':').next().unwrap())));
    assert!(first.contains("\"api_version\":\"1\""));
    assert!(first.contains("\"session_epoch\":\"1\""));
    writer.begin_coordinator_session().unwrap();
    let second = next(&mut body).await;
    assert!(second.contains("\"session_epoch\":\"2\""));
}

async fn next(body: &mut axum::body::BodyDataStream) -> String {
    String::from_utf8(
        tokio::time::timeout(std::time::Duration::from_secs(3), body.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

fn request(uri: &str) -> axum::http::request::Builder {
    Request::builder()
        .uri(uri)
        .header("authorization", format!("Bearer {MANAGEMENT}"))
}

use mllm_management::{
    SnapshotSource, SnapshotUnavailable,
    events::{EventSource, EventStreamOptions},
    read_only_router_with_event_options,
};
use mllm_store::events::{EventCursor, EventPage, EventReadError, ManagementEvent};
use std::{
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
const INCARNATION: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
type ReadFn = dyn Fn(Option<&str>, usize) -> Result<EventPage, EventReadError> + Send + Sync;
struct Fake {
    calls: AtomicUsize,
    read: Box<ReadFn>,
}
impl SnapshotSource for Fake {
    fn snapshot(&self) -> Result<mllm_store::snapshot::Snapshot, SnapshotUnavailable> {
        Err(SnapshotUnavailable)
    }
}
impl EventSource for Fake {
    fn events_after(&self, after: Option<&str>, limit: usize) -> Result<EventPage, EventReadError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        (self.read)(after, limit)
    }
}
fn fake(
    read: impl Fn(Option<&str>, usize) -> Result<EventPage, EventReadError> + Send + Sync + 'static,
) -> Arc<Fake> {
    Arc::new(Fake {
        calls: AtomicUsize::new(0),
        read: Box::new(read),
    })
}
fn event(sequence: i64) -> ManagementEvent {
    ManagementEvent {
        cursor: EventCursor {
            incarnation: INCARNATION.into(),
            sequence,
        },
        recorded_at_ms: i64::MAX,
        kind: "coordinator_session_started".into(),
        deployment_id: None,
        operation_id: None,
        payload_json: format!("{{\"version\":\"1\",\"session_epoch\":{sequence}}}"),
    }
}
fn page(events: Vec<ManagementEvent>, high: i64) -> EventPage {
    EventPage {
        events,
        high_water: EventCursor {
            incarnation: INCARNATION.into(),
            sequence: high,
        },
    }
}
fn options() -> EventStreamOptions {
    EventStreamOptions {
        max_streams: 1,
        page_size: 1,
        channel_capacity: 1,
        poll_interval: Duration::from_millis(10),
        heartbeat_interval: Duration::from_millis(30),
        send_timeout: Duration::from_millis(50),
        lifetime: Duration::from_secs(2),
        ..EventStreamOptions::default()
    }
}
fn fake_app(source: Arc<Fake>, options: EventStreamOptions) -> axum::Router {
    read_only_router_with_event_options(
        ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap(),
        source,
        options,
    )
    .unwrap()
}

#[tokio::test]
async fn authentication_and_syntax_validation_precede_all_provider_work() {
    let source = fake(|_, _| panic!("provider must not be called"));
    let app = fake_app(source.clone(), options());
    for key in [None, Some(INFERENCE), Some("bad")] {
        let mut builder = Request::builder().uri("/management/v1/events?after=bad");
        if let Some(key) = key {
            builder = builder.header("authorization", format!("Bearer {key}"));
        }
        assert_eq!(
            app.clone()
                .oneshot(builder.body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status(),
            401
        );
    }
    assert_eq!(
        app.oneshot(
            request("/management/v1/events?after=bad")
                .body(Body::empty())
                .unwrap()
        )
        .await
        .unwrap()
        .status(),
        400
    );
    assert_eq!(source.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn pagination_never_skips_to_high_water_and_heartbeat_has_no_cursor() {
    let source = fake(|after, limit| {
        assert_eq!(limit, 1);
        match after {
            None => Ok(page(vec![event(1)], 3)),
            Some(s) if s.ends_with(":1") => Ok(page(vec![event(2)], 3)),
            Some(s) if s.ends_with(":2") => Ok(page(vec![event(3)], 3)),
            Some(s) if s.ends_with(":3") => Ok(page(vec![], 3)),
            _ => panic!("unexpected cursor"),
        }
    });
    let response = fake_app(source, options())
        .oneshot(
            request("/management/v1/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let mut body = response.into_body().into_data_stream();
    for n in 1..=3 {
        let text = next(&mut body).await;
        assert!(text.contains(&format!("id: {INCARNATION}:{n}\n")), "{text}");
        assert!(text.contains("\"recorded_at_ms\":\"9223372036854775807\""));
    }
    assert_eq!(next(&mut body).await, ": heartbeat\n\n");
}

#[tokio::test]
async fn hostile_unknown_duplicate_and_oversized_payloads_fail_before_headers() {
    for payload in [
        r#"{"version":"1","session_epoch":1,"prompt":"secret"}"#.to_string(),
        r#"{"version":"1","session_epoch":1,"session_epoch":2}"#.into(),
        r#"{"version":"1","session_epoch":"/secret/path"}"#.into(),
        r#"{"version":"1","session_epoch":-1}"#.into(),
        "x".repeat(16 * 1024 + 1),
    ] {
        let source = fake(move |_, _| {
            let mut e = event(1);
            e.payload_json = payload.clone();
            Ok(page(vec![e], 1))
        });
        let response = fake_app(source, options())
            .oneshot(
                request("/management/v1/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 500);
        let text = String::from_utf8(
            axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(!text.contains("secret"));
        assert!(text.contains("\"code\":\"internal\""));
    }
    for kind in ["future_event", "coordinator_session_started\ninjected"] {
        let source = fake(move |_, _| {
            let mut e = event(1);
            e.kind = kind.into();
            Ok(page(vec![e], 1))
        });
        assert_eq!(
            fake_app(source, options())
                .oneshot(
                    request("/management/v1/events")
                        .body(Body::empty())
                        .unwrap()
                )
                .await
                .unwrap()
                .status(),
            500
        );
    }
}

#[tokio::test]
async fn terminal_expiry_and_corruption_are_explicit_without_advancing_cursor() {
    for expired in [true, false] {
        let source = fake(move |after, _| {
            if after.is_none() {
                Ok(page(vec![event(1)], 1))
            } else if expired {
                Err(EventReadError::ExpiredCursor)
            } else {
                let mut e = event(2);
                e.payload_json = "secret".into();
                Ok(page(vec![e], 2))
            }
        });
        let mut body = fake_app(source, options())
            .oneshot(
                request("/management/v1/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .into_body()
            .into_data_stream();
        assert!(next(&mut body).await.contains("id:"));
        let terminal = next(&mut body).await;
        assert!(terminal.contains("event: management_stream_error\n"));
        assert!(terminal.contains(if expired {
            "cursor_expired"
        } else {
            "internal"
        }));
        assert!(!terminal.contains("id:"));
        assert!(!terminal.contains("secret"));
        assert!(body.next().await.is_none());
    }
}

#[tokio::test]
async fn slow_clients_disconnect_without_success_or_unsent_cursor_and_release_capacity() {
    let source = fake(|after, _| {
        let sequence = after
            .map(|s| s.split_once(':').unwrap().1.parse::<i64>().unwrap())
            .unwrap_or(0)
            + 1;
        Ok(page(vec![event(sequence)], sequence))
    });
    let app = fake_app(source.clone(), options());
    let response = app
        .clone()
        .oneshot(
            request("/management/v1/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(source.calls.load(Ordering::SeqCst) <= 2);
    let second = app
        .oneshot(
            request("/management/v1/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), 200);
    let mut body = response.into_body().into_data_stream();
    assert!(
        next(&mut body)
            .await
            .contains(&format!("id: {INCARNATION}:1"))
    );
    assert!(body.next().await.is_none());
}

#[tokio::test]
async fn dropping_body_stops_polling_and_releases_stream_slot() {
    let source = fake(|_, _| Ok(page(vec![], 0)));
    let app = fake_app(source.clone(), options());
    let response = app
        .clone()
        .oneshot(
            request("/management/v1/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        app.clone()
            .oneshot(
                request("/management/v1/events")
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        429
    );
    drop(response);
    tokio::time::sleep(Duration::from_millis(30)).await;
    let calls = source.calls.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(source.calls.load(Ordering::SeqCst), calls);
    assert_eq!(
        app.oneshot(
            request("/management/v1/events")
                .body(Body::empty())
                .unwrap()
        )
        .await
        .unwrap()
        .status(),
        200
    );
}

#[tokio::test]
async fn finite_lifetime_closes_idle_stream_and_releases_capacity() {
    let source = fake(|_, _| Ok(page(vec![], 0)));
    let mut config = options();
    config.lifetime = Duration::from_millis(40);
    config.heartbeat_interval = Duration::from_secs(1);
    let app = fake_app(source, config);
    let first = app
        .clone()
        .oneshot(
            request("/management/v1/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        app.oneshot(
            request("/management/v1/events")
                .body(Body::empty())
                .unwrap()
        )
        .await
        .unwrap()
        .status(),
        200
    );
    let mut body = first.into_body().into_data_stream();
    assert!(body.next().await.is_none());
}

#[tokio::test]
async fn dropped_streams_do_not_cancel_accepted_reads_or_release_global_worker_permits() {
    use std::sync::Condvar;
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let completed = Arc::new(AtomicUsize::new(0));
    let (entered, mut arrivals) = tokio::sync::mpsc::unbounded_channel();
    let source = fake({
        let release = release.clone();
        let completed = completed.clone();
        move |after, _| {
            if after.is_none() {
                return Ok(page(vec![event(1)], 1));
            }
            entered.send(()).unwrap();
            let mut unlocked = release.0.lock().unwrap();
            while !*unlocked {
                unlocked = release.1.wait(unlocked).unwrap();
            }
            completed.fetch_add(1, Ordering::SeqCst);
            Ok(page(vec![], 1))
        }
    });
    struct Release(Arc<(Mutex<bool>, Condvar)>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0.0.lock().unwrap() = true;
            self.0.1.notify_all();
        }
    }
    let release_on_exit = Release(release);
    let app = fake_app(source, options());
    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(
                request("/management/v1/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        tokio::time::timeout(Duration::from_secs(2), arrivals.recv())
            .await
            .unwrap()
            .unwrap();
        drop(response);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for uri in ["/management/v1/events", "/management/v1/snapshot"] {
        let response = app
            .clone()
            .oneshot(request(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 429);
    }
    assert_eq!(completed.load(Ordering::SeqCst), 0);
    drop(release_on_exit);
    tokio::time::timeout(Duration::from_secs(2), async {
        while completed.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        app.oneshot(
            request("/management/v1/events")
                .body(Body::empty())
                .unwrap()
        )
        .await
        .unwrap()
        .status(),
        200
    );
}

#[tokio::test]
async fn cancelled_preflight_retains_workers_and_sanitizes_provider_failure() {
    use std::sync::Condvar;
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let (entered, mut arrivals) = tokio::sync::mpsc::unbounded_channel();
    let source = fake({
        let release = release.clone();
        move |_, _| {
            entered.send(()).unwrap();
            let mut unlocked = release.0.lock().unwrap();
            while !*unlocked {
                unlocked = release.1.wait(unlocked).unwrap();
            }
            Err(EventReadError::Sql(rusqlite::Error::InvalidParameterName(
                "/secret/token".into(),
            )))
        }
    });
    struct Release(Arc<(Mutex<bool>, Condvar)>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0.0.lock().unwrap() = true;
            self.0.1.notify_all();
        }
    }
    let release_on_exit = Release(release);
    let mut config = options();
    config.max_streams = 3;
    let app = fake_app(source, config);
    for _ in 0..2 {
        let task = tokio::spawn(
            app.clone().oneshot(
                request("/management/v1/events")
                    .body(Body::empty())
                    .unwrap(),
            ),
        );
        tokio::time::timeout(Duration::from_secs(2), arrivals.recv())
            .await
            .unwrap()
            .unwrap();
        task.abort();
        let _ = task.await;
    }
    assert_eq!(
        app.clone()
            .oneshot(
                request("/management/v1/events")
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        429
    );
    drop(release_on_exit);
    tokio::time::sleep(Duration::from_millis(30)).await;
    let response = app
        .oneshot(
            request("/management/v1/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 500);
    let text = String::from_utf8(
        axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(!text.contains("secret"));
}

#[tokio::test]
async fn qualified_lifecycle_events_enforce_transition_and_commit_epoch() {
    for (kind, transition) in [
        ("qualified_initialize_accepted", "accepted"),
        ("qualified_initialize_armed", "armed"),
        (
            "qualified_owned_launch_associated",
            "owned_launch_associated",
        ),
        ("qualified_ready_committed", "ready"),
        ("qualified_initialize_uncertain", "uncertain"),
        ("qualified_initialize_expired_unarmed", "expired_unarmed"),
        ("ordinary_cleanup_accepted", "cleanup_accepted"),
        ("ordinary_cleanup_armed", "cleanup_armed"),
        ("ordinary_cleanup_completed", "cleanup_completed"),
    ] {
        for corruption in 0..4 {
            let source = fake(move |_, _| {
                let mut e = event(1);
                e.kind = kind.into();
                e.operation_id = Some(INCARNATION.into());
                e.deployment_id = Some(INCARNATION.into());
                let ready = matches!(transition, "ready" | "cleanup_completed");
                let epoch = if corruption == 3 {
                    serde_json::json!(0)
                } else if ready ^ (corruption == 2) {
                    serde_json::json!(u64::MAX)
                } else {
                    serde_json::Value::Null
                };
                e.payload_json = serde_json::json!({
                    "version":"1", "transition": if corruption == 1 { "wrong" } else { transition },
                    "operation_id":INCARNATION, "deployment_id":INCARNATION,
                    "step_id":INCARNATION, "session_epoch":1, "committed_epoch":epoch
                })
                .to_string();
                Ok(page(vec![e], 1))
            });
            let response = fake_app(source, options())
                .oneshot(
                    request("/management/v1/events")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                if corruption == 0 { 200 } else { 500 },
                "{kind}, corruption {corruption}"
            );
            if corruption == 0 {
                let mut body = response.into_body().into_data_stream();
                let text = next(&mut body).await;
                assert!(text.contains(&format!("event: {kind}\n")));
                let data = text
                    .lines()
                    .find_map(|line| line.strip_prefix("data: "))
                    .unwrap();
                let json: serde_json::Value = serde_json::from_str(data).unwrap();
                assert_eq!(json["payload"]["transition"], transition);
                assert_eq!(
                    json["payload"]["committed_epoch"],
                    if matches!(transition, "ready" | "cleanup_completed") {
                        serde_json::json!("18446744073709551615")
                    } else {
                        serde_json::Value::Null
                    }
                );
            }
        }
    }
}

#[tokio::test]
async fn every_supported_kind_projects_only_known_fields_and_wide_integer_strings() {
    let cases = [
        (
            "managed_configuration_accepted",
            "operation_id,deployment_id,revision,generation,session_epoch",
        ),
        (
            "candidate_initialize_armed",
            "operation_id,deployment_id,run_id,step_id,revision,generation,session_epoch",
        ),
        (
            "candidate_initialize_accepted",
            "operation_id,deployment_id,run_id,step_id,revision,generation,session_epoch",
        ),
        (
            "candidate_run_accepted",
            "operation_id,deployment_id,run_id,revision,generation,resource_policy_revision,qualification_policy_revision,session_epoch",
        ),
        (
            "host_resource_policy_bootstrapped",
            "revision,ledger_epoch,session_epoch",
        ),
        (
            "host_resource_policy_updated",
            "operation_id,previous_revision,current_revision,ledger_epoch,session_epoch",
        ),
        (
            "host_qualification_policy_changed",
            "change_kind,previous_revision,current_revision,session_epoch",
        ),
        (
            "candidate_qualification_finished",
            "transition,operation_id,deployment_id,step_id,session_epoch,committed_epoch",
        ),
        (
            "candidate_owned_launch_associated",
            "transition,operation_id,deployment_id,step_id,session_epoch,committed_epoch",
        ),
        (
            "candidate_ready_completed",
            "transition,operation_id,deployment_id,step_id,session_epoch,committed_epoch",
        ),
        (
            "candidate_park_completed",
            "transition,operation_id,deployment_id,step_id,session_epoch,committed_epoch",
        ),
        (
            "candidate_cleanup_accepted",
            "transition,operation_id,deployment_id,step_id,session_epoch,committed_epoch",
        ),
        (
            "candidate_cleanup_armed",
            "transition,operation_id,deployment_id,step_id,session_epoch,committed_epoch",
        ),
        (
            "candidate_cleanup_completed",
            "transition,operation_id,deployment_id,step_id,session_epoch,committed_epoch",
        ),
    ];
    for (kind, fields) in cases {
        let source = fake(move |_, _| {
            let mut e = event(1);
            e.kind = kind.into();
            let mut payload = serde_json::json!({"version":"1"});
            for field in fields.split(',') {
                payload[field] = if field.ends_with("_id") {
                    serde_json::json!(INCARNATION)
                } else if field == "transition" {
                    serde_json::json!(kind.strip_prefix("candidate_").unwrap())
                } else if field == "change_kind" {
                    serde_json::json!("imported")
                } else if matches!(field, "ledger_epoch" | "committed_epoch") {
                    serde_json::json!(u64::MAX)
                } else {
                    serde_json::json!(i64::MAX)
                };
            }
            e.operation_id = payload["operation_id"].as_str().map(str::to_owned);
            e.deployment_id = payload["deployment_id"].as_str().map(str::to_owned);
            e.payload_json = payload.to_string();
            Ok(page(vec![e], 1))
        });
        let response = fake_app(source, options())
            .oneshot(
                request("/management/v1/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{kind}");
        let mut body = response.into_body().into_data_stream();
        let text = next(&mut body).await;
        assert!(text.contains(&format!("event: {kind}\n")));
        let data = text
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap();
        let json: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(json["api_version"], "1");
        assert_eq!(json["recorded_at_ms"], "9223372036854775807");
        assert_eq!(
            json["payload"].as_object().unwrap().len(),
            fields.split(',').count()
        );
        for field in fields.split(',').filter(|field| {
            !field.ends_with("_id") && !matches!(*field, "transition" | "change_kind")
        }) {
            assert_eq!(
                json["payload"][field],
                if matches!(field, "ledger_epoch" | "committed_epoch") {
                    "18446744073709551615"
                } else {
                    "9223372036854775807"
                }
            );
        }
    }
}

#[tokio::test]
async fn page_count_bytes_order_and_identity_corruption_fail_before_headers() {
    for case in 0..5 {
        let source = fake(move |_, _| {
            let mut e = event(1);
            match case {
                0 => return Ok(page(vec![e.clone(), e], 1)),
                1 => e.cursor.sequence = 2,
                2 => e.cursor.incarnation = "00000000000000000000000000".into(),
                3 => e.operation_id = Some("/secret".into()),
                _ => (),
            }
            Ok(page(vec![e], 1))
        });
        let mut config = options();
        if case == 4 {
            config.page_bytes = 10;
        }
        assert_eq!(
            fake_app(source, config)
                .oneshot(
                    request("/management/v1/events")
                        .body(Body::empty())
                        .unwrap()
                )
                .await
                .unwrap()
                .status(),
            500
        );
    }
}

#[tokio::test]
async fn null_required_revision_is_corruption() {
    let source = fake(|_, _| {
        let mut e = event(1);
        e.kind = "host_resource_policy_updated".into();
        e.operation_id = Some(INCARNATION.into());
        e.payload_json = serde_json::json!({"version":"1", "operation_id":INCARNATION, "previous_revision":null, "current_revision":1, "ledger_epoch":1, "session_epoch":1}).to_string();
        Ok(page(vec![e], 1))
    });
    assert_eq!(
        fake_app(source, options())
            .oneshot(
                request("/management/v1/events")
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        500
    );
}

#[tokio::test]
async fn malformed_duplicate_and_disagreeing_cursors_and_methods_fail_closed() {
    let store = Store::open_in_memory().unwrap();
    let cursor = store.snapshot().unwrap().cursor.to_string();
    let app = read_only_router(
        ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap(),
        Arc::new(StoreSnapshotSource::new(store)),
    );
    for query in [
        "after=bad".to_string(),
        "extra=1".into(),
        "after=%GG".into(),
        "after=%FF".into(),
        format!("after={cursor}&after={cursor}"),
        format!("after={cursor}&"),
        format!("after={}1", "x".repeat(100)),
        format!("after={}:1", cursor.split(':').next().unwrap()),
    ] {
        let response = app
            .clone()
            .oneshot(
                request(&format!("/management/v1/events?{query}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 400, "{query}");
    }
    for builder in [
        request("/management/v1/events")
            .header("last-event-id", &cursor)
            .header("last-event-id", &cursor),
        request(&format!("/management/v1/events?after={cursor}"))
            .header("last-event-id", "00000000000000000000000000:0"),
    ] {
        assert_eq!(
            app.clone()
                .oneshot(builder.body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status(),
            400
        );
    }
    for method in ["POST", "HEAD", "OPTIONS"] {
        assert_eq!(
            app.clone()
                .oneshot(
                    request("/management/v1/events")
                        .method(method)
                        .body(Body::empty())
                        .unwrap()
                )
                .await
                .unwrap()
                .status(),
            405
        );
    }
}

#[tokio::test]
async fn real_store_retention_and_incarnation_require_resnapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.sqlite");
    let store = Store::open(&path).unwrap();
    let cursor = store.snapshot().unwrap().cursor.to_string();
    store.begin_coordinator_session().unwrap();
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute("UPDATE management_events SET recorded_at_ms=0", [])
        .unwrap();
    let app = read_only_router(
        ManagementCredentials::from_trusted_resolver(MANAGEMENT, INFERENCE).unwrap(),
        Arc::new(StoreSnapshotSource::new(store)),
    );
    for cursor in [cursor, "00000000000000000000000000:0".into()] {
        let response = app
            .clone()
            .oneshot(
                request("/management/v1/events")
                    .header("last-event-id", cursor)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 410);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["api_version"], "1");
        assert_eq!(json["error"]["code"], "cursor_expired");
        assert_eq!(json["error"]["details"]["resnapshot_required"], true);
    }
}

//! Spec §5, ADR 0023 §6: the one wait for TensorFold's own counters before a
//! stop signal, shared by the host agent and the embedded coordinator.
use std::future::Future;
use std::time::Duration;

use super::ENGINE_IDLE_BOUND;
use crate::traits::{EngineAdapter, EngineWork, MemberRef};

/// How often the idle check reads `/health`.
const IDLE_POLL: Duration = Duration::from_millis(250);

/// Whether the stop signal may be sent, read after CapyCTL's own drain.
///
/// `true` at once when the counters read idle or nothing listens (an exited
/// or not yet loaded engine serves nothing). Otherwise `/health` is read
/// until `within` (never longer than [`ENGINE_IDLE_BOUND`]) passes; the read
/// in flight then finishes (each read is bounded by
/// [`super::HEALTH_READ_TIMEOUT`]) and decides: an answer reporting work keeps the
/// signal back (`false`), a hung or unreadable `/health` reported none
/// (`true`). An engine without counters (`None`) is clear at once. `false`
/// when `stop` completes first.
pub async fn wait_idle<A, S>(adapter: &A, member: &MemberRef, within: Duration, stop: S) -> bool
where
    A: EngineAdapter + ?Sized,
    S: Future<Output = ()>,
{
    let until = tokio::time::Instant::now() + within.min(ENGINE_IDLE_BOUND);
    let poll = async {
        loop {
            let work = adapter.idle_before_signal(member).await;
            if work.is_none_or(EngineWork::signal_now) {
                return true;
            }
            if tokio::time::Instant::now() >= until {
                return work != Some(EngineWork::Busy);
            }
            tokio::time::sleep_until(until.min(tokio::time::Instant::now() + IDLE_POLL)).await;
        }
    };
    tokio::select! {
        idle = poll => idle,
        () = stop => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tensorfold::TensorfoldAdapter;

    /// A `/health` that always reads busy.
    async fn busy_engine() -> TensorfoldAdapter {
        let app = axum::Router::new().route(
            "/health",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({"ok": true, "busy": true, "requests_running": 1}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let endpoint = format!("http://127.0.0.1:{port}").parse().unwrap();
        TensorfoldAdapter::new(endpoint, "0.6.0".into(), "toy".into())
    }

    fn member() -> MemberRef {
        MemberRef {
            deployment_id: "d".into(),
            member_id: "m".into(),
        }
    }

    // T41 (spec §5): a stop ends the wait at once, as not idle.
    #[tokio::test]
    async fn a_stop_ends_the_wait_not_idle() {
        let adapter = busy_engine().await;
        let started = std::time::Instant::now();
        let stop = tokio::time::sleep(Duration::from_millis(300));
        assert!(!wait_idle(&adapter, &member(), Duration::from_secs(20), stop).await);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    // T41 (spec §5): busy at the bound is not idle.
    #[tokio::test]
    async fn busy_at_the_bound_is_not_idle() {
        let adapter = busy_engine().await;
        let within = Duration::from_millis(600);
        assert!(!wait_idle(&adapter, &member(), within, std::future::pending()).await);
    }

    /// An engine on `app`, or nothing at all on the port when `app` is `None`.
    async fn engine(app: Option<axum::Router>) -> TensorfoldAdapter {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        match app {
            Some(app) => {
                tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            }
            None => drop(listener),
        }
        let endpoint = format!("http://127.0.0.1:{port}").parse().unwrap();
        TensorfoldAdapter::new(endpoint, "0.6.0".into(), "toy".into())
    }

    /// A `/health` that never answers.
    fn hung() -> axum::Router {
        axum::Router::new().route(
            "/health",
            axum::routing::get(|| async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                "late"
            }),
        )
    }

    // T41 (spec §5, ADR 0023 §6): nothing listening (an engine that exited or
    // has not bound its port) has no work in flight; the wait ends at once.
    #[tokio::test]
    async fn a_port_with_no_listener_may_be_signalled_at_once() {
        let adapter = engine(None).await;
        let started = std::time::Instant::now();
        let within = Duration::from_secs(10);
        assert!(wait_idle(&adapter, &member(), within, std::future::pending()).await);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    // T41 (spec §5, ADR 0023 §6): an engine answering 503 has not loaded its
    // model, so it serves nothing; the wait ends at once.
    #[tokio::test]
    async fn an_engine_still_loading_may_be_signalled_at_once() {
        let app = axum::Router::new().route(
            "/health",
            axum::routing::get(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }),
        );
        let adapter = engine(Some(app)).await;
        let started = std::time::Instant::now();
        let within = Duration::from_secs(10);
        assert!(wait_idle(&adapter, &member(), within, std::future::pending()).await);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    // T41 (spec §5, ADR 0023 §6): a hung `/health` may be signalled once the
    // bound passes; it never reported work.
    #[tokio::test]
    async fn a_hung_engine_may_be_signalled_at_the_bound() {
        let adapter = engine(Some(hung())).await;
        let started = std::time::Instant::now();
        let within = Duration::from_millis(800);
        assert!(wait_idle(&adapter, &member(), within, std::future::pending()).await);
        assert!(started.elapsed() >= within);
    }

    // T41 (spec §5, ADR 0023 §6): a busy answer that arrives after the bound
    // still decides; a slow engine is not taken for a hung one.
    #[tokio::test]
    async fn a_slow_busy_answer_after_the_bound_is_not_idle() {
        let app = axum::Router::new().route(
            "/health",
            axum::routing::get(|| async {
                tokio::time::sleep(Duration::from_millis(400)).await;
                axum::Json(serde_json::json!({"ok": true, "busy": true, "requests_running": 1}))
            }),
        );
        let adapter = engine(Some(app)).await;
        let within = Duration::from_millis(100);
        assert!(!wait_idle(&adapter, &member(), within, std::future::pending()).await);
    }

    // T41 (spec §5): a stop ends the wait as not idle, even for an engine
    // that would be safe to signal at the bound.
    #[tokio::test]
    async fn a_stop_during_a_hung_read_is_not_idle() {
        let adapter = engine(Some(hung())).await;
        let stop = tokio::time::sleep(Duration::from_millis(200));
        assert!(!wait_idle(&adapter, &member(), Duration::from_secs(5), stop).await);
    }
}

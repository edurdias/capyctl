//! Spec §5 (ruling 9): the one wait for TensorFold's own counters to read idle
//! before a stop signal, shared by the host agent and the embedded coordinator.
use std::future::Future;
use std::time::Duration;

use super::ENGINE_IDLE_BOUND;
use crate::traits::{EngineAdapter, MemberRef};

/// How often the idle check reads `/health`.
const IDLE_POLL: Duration = Duration::from_millis(250);

/// `true` once the adapter's idle check reads idle; `false` when it does not
/// within `within` (never longer than [`ENGINE_IDLE_BOUND`]) or when `stop`
/// completes first. `false` means nothing may be signalled.
pub async fn wait_idle<A, S>(adapter: &A, member: &MemberRef, within: Duration, stop: S) -> bool
where
    A: EngineAdapter + ?Sized,
    S: Future<Output = ()>,
{
    let until = tokio::time::Instant::now() + within.min(ENGINE_IDLE_BOUND);
    let poll = async {
        loop {
            if adapter.idle_before_signal(member).await == Some(true) {
                return true;
            }
            tokio::time::sleep(IDLE_POLL).await;
        }
    };
    tokio::select! {
        idle = tokio::time::timeout_at(until, poll) => idle.unwrap_or(false),
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
}

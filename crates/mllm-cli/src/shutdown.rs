//! SPEC §4.3: ordinary role shutdown is a service restart, not a deployment deletion.
//!
//! Stopping or signalling any role (server, host, standalone) closes admission,
//! lets the requests already admitted finish within a bound, cancels whatever is
//! still streaming when the bound expires, and then exits without touching an
//! engine. Engines stay running and owned; the next start re-attaches them through
//! the fresh-probe path (owner decision P3, 2026-09-22). Explicitly terminating a
//! host's engines is a separate operator action (`mllm drain host`,
//! `mllm drain standalone`), never a side effect of a signal.
//!
//! This module holds the pieces every role shares: the signal listener, the
//! admission gate wrapped around an inference-carrying router, and the bounded
//! drain.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::{Body, BodyDataStream, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Router;
use tokio::sync::{watch, Notify};
use tokio_stream::Stream;

/// The drain bound when the role document names none (plan W11: 30 s).
pub const DEFAULT_DRAIN: Duration = mllm_config::remote_roles::DEFAULT_DRAIN_TIMEOUT;
/// After the bound, how long cancelled streams are given to observe the
/// cancellation before the role stops waiting and exits anyway.
const CANCEL_GRACE: Duration = Duration::from_secs(2);
/// As [`CANCEL_GRACE`], after a second signal forced the drain.
const FORCED_GRACE: Duration = Duration::from_millis(200);

/// The standalone role's drain bound: `shutdown.drain_timeout` of its role
/// document (`<state_dir>/config/standalone.yaml`), 30 s when the document or
/// the field is absent. SPEC §15.3: a value outside 0 s to 600 s refuses
/// startup rather than being read as the default, because an operator who set
/// it meant something by it.
pub fn standalone_drain_bound(state_dir: &std::path::Path) -> Result<Duration, String> {
    standalone_drain_bound_in(&state_dir.join("config").join("standalone.yaml"))
}

/// As [`standalone_drain_bound`], reading the role document at `path` (the
/// explicit `--config`). A missing document gives the default here; the boot
/// itself then refuses it (SPEC §15.2, R13).
pub fn standalone_drain_bound_in(path: &std::path::Path) -> Result<Duration, String> {
    standalone_drain_bound_with(
        path,
        &mllm_config::setting_overrides::SettingOverrides::none(
            mllm_config::ConfigKind::Standalone,
        ),
    )
}

/// As [`standalone_drain_bound_in`], with this run's generic overrides
/// (owner decision 2026-09-25: `--set shutdown.drain_timeout=…` or
/// `MLLM_SET__SHUTDOWN__DRAIN_TIMEOUT` win over the document).
pub fn standalone_drain_bound_with(
    path: &std::path::Path,
    overrides: &mllm_config::setting_overrides::SettingOverrides,
) -> Result<Duration, String> {
    let refused = |error: mllm_config::ConfigError| {
        format!("{}: {}", path.display(), overrides.annotate(error))
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // A missing document is generated at the boot; the overrides
            // still apply to it.
            let mut document = serde_json::json!({});
            overrides.apply(&mut document).map_err(refused)?;
            return mllm_config::remote_roles::drain_timeout(&document).map_err(refused);
        }
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let document = mllm_config::parse_document(&text)
        .and_then(|document| overrides.apply_and_validate(document))
        .map_err(refused)?;
    mllm_config::remote_roles::drain_timeout(&document).map_err(refused)
}

/// SIGTERM and SIGINT, installed before the role reports itself ready so a signal
/// that arrives right after readiness is a graceful restart rather than the
/// default disposition killing the process mid-request.
pub struct Signals {
    terminate: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
}

impl Signals {
    pub fn install() -> std::io::Result<Self> {
        use tokio::signal::unix::{signal, SignalKind};
        Ok(Self {
            terminate: signal(SignalKind::terminate())?,
            interrupt: signal(SignalKind::interrupt())?,
        })
    }

    pub async fn recv(&mut self) {
        tokio::select! {
            _ = self.terminate.recv() => {}
            _ = self.interrupt.recv() => {}
        }
    }

    /// SPEC §4.3: the next signal after the first, while the role drains. It
    /// forces the drain short ([`Admission::drain_unless`]); engines stay
    /// running and owned.
    pub async fn forced(&mut self) {
        self.recv().await;
        mllm_domain::role_log::notice(
            mllm_domain::role_log::Level::Notice,
            "second signal: cancelling in-flight requests now; engines are retained",
        );
    }
}

/// What one bounded drain observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DrainReport {
    /// Requests admitted and not yet finished when admission closed.
    pub in_flight_at_close: usize,
    /// Requests still streaming when the bound expired, whose bodies were cut.
    pub cancelled: usize,
    /// Whether every admitted request finished on its own inside the bound.
    pub drained: bool,
    /// SPEC §4.3: a second SIGTERM or SIGINT cut the drain short; whatever was
    /// still streaming was cancelled at once. Engines are untouched either way.
    pub forced: bool,
}

impl DrainReport {
    pub fn to_json(self) -> serde_json::Value {
        serde_json::json!({
            "in_flight_at_close": self.in_flight_at_close,
            "cancelled": self.cancelled,
            "drained": self.drained,
            "forced": self.forced,
        })
    }
}

/// Admission control for one inference-carrying listener.
///
/// Every admitted request is counted from the moment it is admitted until its
/// response body is finished or dropped, so a streamed answer counts for as long
/// as it streams. Closing refuses new requests with 503 while the admitted ones
/// complete.
pub struct Admission {
    closed: AtomicBool,
    active: AtomicUsize,
    idle: Notify,
    cancel: watch::Sender<bool>,
}

impl Admission {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            closed: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            idle: Notify::new(),
            cancel: watch::channel(false).0,
        })
    }

    /// Wrap `router` so its requests are admitted, counted and drainable.
    pub fn gate(self: &Arc<Self>, router: Router) -> Router {
        router.layer(axum::middleware::from_fn_with_state(self.clone(), admit))
    }

    pub fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Refuse every new request from now on. Idempotent.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    /// Close admission and wait up to `bound` for the admitted requests to finish.
    /// Whatever is still streaming at the bound is cancelled: its body ends where it
    /// is, and the role proceeds to exit without waiting on the client.
    pub async fn drain(&self, bound: Duration) -> DrainReport {
        self.drain_unless(bound, std::future::pending::<()>()).await
    }

    /// As [`Admission::drain`], cut short when `force` completes first: the
    /// operator signalled the role again while it drained (SPEC §4.3). Every
    /// admitted stream is then cancelled at once and the role goes on to exit
    /// within its remaining fixed bounds. Engines stay running and owned
    /// either way; a forced drain only stops waiting for clients.
    pub async fn drain_unless<F: Future>(&self, bound: Duration, force: F) -> DrainReport {
        self.close();
        let in_flight_at_close = self.active();
        let deadline = tokio::time::Instant::now() + bound;
        tokio::pin!(force);
        let forced = tokio::select! {
            idle = self.wait_idle(deadline) => {
                if idle {
                    return DrainReport {
                        in_flight_at_close,
                        cancelled: 0,
                        drained: true,
                        forced: false,
                    };
                }
                false
            }
            _ = &mut force => true,
        };
        if forced && self.active() == 0 {
            return DrainReport {
                in_flight_at_close,
                cancelled: 0,
                drained: true,
                forced: true,
            };
        }
        let cancelled = self.active();
        self.cancel.send_replace(true);
        // Cancelled bodies end on their next poll; give them a moment to be
        // dropped so the count is honest, but never hold the exit on a client,
        // and not at all once the operator has asked twice.
        let grace = if forced { FORCED_GRACE } else { CANCEL_GRACE };
        let _ = self.wait_idle(tokio::time::Instant::now() + grace).await;
        DrainReport {
            in_flight_at_close,
            cancelled,
            drained: false,
            forced,
        }
    }

    /// Whether the admitted count reached zero before `deadline`.
    async fn wait_idle(&self, deadline: tokio::time::Instant) -> bool {
        loop {
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.active() == 0 {
                return true;
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep_until(deadline) => return self.active() == 0,
            }
        }
    }
}

/// One admitted request, counted until its response body is gone.
struct Admitted(Arc<Admission>);

impl Admitted {
    fn enter(admission: &Arc<Admission>) -> Self {
        admission.active.fetch_add(1, Ordering::SeqCst);
        Self(admission.clone())
    }
}

impl Drop for Admitted {
    fn drop(&mut self) {
        if self.0.active.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

/// SPEC §4.3: a restarting role refuses new work with a retryable answer rather
/// than a connection reset, so a client can tell a restart from a failure.
fn refused() -> Response {
    let mut response = (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(serde_json::json!({
            "error": {
                "code": "shutting_down",
                "message": "This role is restarting; retry once it is back",
                "retryable": true,
            }
        })),
    )
        .into_response();
    let headers = response.headers_mut();
    headers.insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::CONNECTION, HeaderValue::from_static("close"));
    response
}

async fn admit(State(admission): State<Arc<Admission>>, request: Request, next: Next) -> Response {
    if admission.is_closed() {
        return refused();
    }
    let admitted = Admitted::enter(&admission);
    // Checked again after counting: a close that read the count as zero before
    // this increment must not have this request slip in behind it.
    if admission.is_closed() {
        return refused();
    }
    let response = next.run(request).await;
    let (parts, body) = response.into_parts();
    let mut cancel = admission.cancel.subscribe();
    let body = Body::from_stream(Counted {
        inner: body.into_data_stream(),
        cancelled: Box::pin(async move {
            let _ = cancel.wait_for(|cancelled| *cancelled).await;
        }),
        _admitted: admitted,
    });
    Response::from_parts(parts, body)
}

/// A response body that keeps its request counted and ends early on cancellation.
struct Counted {
    inner: BodyDataStream,
    cancelled: Pin<Box<dyn Future<Output = ()> + Send>>,
    _admitted: Admitted,
}

impl Stream for Counted {
    type Item = Result<Bytes, axum::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        if this.cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        Pin::new(&mut this.inner).poll_next(cx)
    }
}

/// Serve `router` until `stop` turns true, then stop accepting and let open
/// connections finish; the caller bounds how long it waits for that.
pub async fn serve(
    listener: tokio::net::TcpListener,
    router: Router,
    mut stop: watch::Receiver<bool>,
) -> std::io::Result<()> {
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = stop.wait_for(|stopped| *stopped).await;
        })
        .await
}

/// Wait for served listeners to wind down after `stop`, never longer than
/// `CANCEL_GRACE`: the drain already bounded the requests that mattered, and an
/// idle client holding a connection open must not hold the restart.
pub async fn join_listeners<F>(listeners: F) -> Option<F::Output>
where
    F: Future,
{
    tokio::time::timeout(CANCEL_GRACE, listeners).await.ok()
}

/// How long a role's shutdown waits for its supervisors to finish the pass or
/// probe they are in before it aborts them.
pub const SUPERVISION_JOIN_BOUND: Duration = Duration::from_secs(10);

/// SPEC §4.3, ADR 0015 invariant 6 (no effect outlives the worker): a role's
/// background supervisors (readiness, engine exits, checkpoint digests) as
/// one set with one cancel signal. Each is started with `spawn_until` on
/// [`Supervision::cancel_signal`], so a shutdown that [`Supervision::join`]s
/// them lets a pass in progress finish and then waits for every probe or
/// measurement it started, instead of aborting the loop and leaving a
/// blocking pass or a child probe still holding the coordinator's owned state
/// after the role has returned. Dropping the set without joining aborts them,
/// as before (a test that drops its app).
pub struct Supervision {
    cancel: watch::Sender<bool>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Default for Supervision {
    fn default() -> Self {
        Self {
            cancel: watch::channel(false).0,
            tasks: Vec::new(),
        }
    }
}

impl Supervision {
    pub fn new() -> Self {
        Self::default()
    }

    /// The signal a supervisor is started with (`spawn_until`).
    pub fn cancel_signal(&self) -> watch::Receiver<bool> {
        self.cancel.subscribe()
    }

    /// Hold `task` in the set.
    pub fn supervise(&mut self, task: tokio::task::JoinHandle<()>) {
        self.tasks.push(task);
    }

    /// Cancel every supervisor and wait up to `bound` for each to return;
    /// abort and await whatever is still running then. Returns how many were
    /// aborted.
    pub async fn join(mut self, bound: Duration) -> usize {
        self.cancel.send_replace(true);
        let deadline = tokio::time::Instant::now() + bound;
        let mut aborted = 0;
        for mut task in std::mem::take(&mut self.tasks) {
            if tokio::time::timeout_at(deadline, &mut task).await.is_err() {
                task.abort();
                let _ = task.await;
                aborted += 1;
            }
        }
        aborted
    }
}

impl Drop for Supervision {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Milliseconds since `started`, for the shutdown report.
pub fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;

    // T33 (ADR 0015 invariant 6): a joined supervision cancels every
    // supervisor, lets one that honours the signal finish the work it is in,
    // and aborts only one that outlives the bound.
    #[tokio::test]
    async fn a_joined_supervision_lets_cancelled_work_finish_and_aborts_the_rest() {
        let mut supervision = Supervision::new();
        let finished = Arc::new(AtomicBool::new(false));
        let mut cancel = supervision.cancel_signal();
        let done = finished.clone();
        supervision.supervise(tokio::spawn(async move {
            let _ = cancel.wait_for(|cancelled| *cancelled).await;
            // The pass in progress when the signal came still completes.
            tokio::time::sleep(Duration::from_millis(50)).await;
            done.store(true, Ordering::SeqCst);
        }));
        supervision.supervise(tokio::spawn(std::future::pending::<()>()));
        let aborted = supervision.join(Duration::from_millis(300)).await;
        assert!(
            finished.load(Ordering::SeqCst),
            "the cancelled supervisor finished"
        );
        assert_eq!(aborted, 1, "only the one ignoring the signal is aborted");
    }

    // T33: an app dropped without shutdown still aborts its supervisors.
    #[tokio::test]
    async fn a_dropped_supervision_aborts_its_supervisors() {
        let mut supervision = Supervision::new();
        let task = tokio::spawn(std::future::pending::<()>());
        let handle = task.abort_handle();
        supervision.supervise(task);
        drop(supervision);
        tokio::task::yield_now().await;
        assert!(handle.is_finished());
    }

    async fn slow_stream() -> Response {
        let chunks = tokio_stream::iter(0..50).then(|i| async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok::<_, std::io::Error>(Bytes::from(format!("chunk {i}\n")))
        });
        Body::from_stream(chunks).into_response()
    }

    use tokio_stream::StreamExt as _;

    async fn served(admission: &Arc<Admission>) -> (std::net::SocketAddr, watch::Sender<bool>) {
        let router = admission.gate(
            Router::new()
                .route("/slow", get(slow_stream))
                .route("/fast", get(|| async { "ok" })),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, receiver) = watch::channel(false);
        tokio::spawn(serve(listener, router, receiver));
        (address, stop)
    }

    // T17, T18: a closed gate refuses new work with a retryable 503 while the
    // request it already admitted keeps streaming to completion.
    #[tokio::test]
    async fn closing_refuses_new_requests_and_lets_admitted_streams_finish() {
        let admission = Admission::new();
        let (address, _stop) = served(&admission).await;
        let client = reqwest::Client::new();
        let streaming = client
            .get(format!("http://{address}/slow"))
            .send()
            .await
            .unwrap();
        assert_eq!(admission.active(), 1);
        let drain = {
            let admission = admission.clone();
            tokio::spawn(async move { admission.drain(Duration::from_secs(20)).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        let refused = client
            .get(format!("http://{address}/fast"))
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(refused.headers()[header::RETRY_AFTER], "5");
        let body = streaming.text().await.unwrap();
        assert!(
            body.contains("chunk 49"),
            "the admitted stream finished: {body}"
        );
        let report = drain.await.unwrap();
        assert_eq!(
            report,
            DrainReport {
                in_flight_at_close: 1,
                cancelled: 0,
                drained: true,
                forced: false,
            }
        );
    }

    // T17, T38: the drain is bounded; a stream still running at the bound is cut
    // rather than holding the restart.
    #[tokio::test]
    async fn a_stream_outliving_the_bound_is_cancelled() {
        let admission = Admission::new();
        let (address, _stop) = served(&admission).await;
        let streaming = reqwest::Client::new()
            .get(format!("http://{address}/slow"))
            .send()
            .await
            .unwrap();
        let started = Instant::now();
        let report = admission.drain(Duration::from_millis(300)).await;
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(report.cancelled, 1);
        assert!(!report.drained);
        let body = streaming.text().await.unwrap_or_default();
        assert!(!body.contains("chunk 49"), "the stream was cut: {body}");
        assert_eq!(admission.active(), 0);
    }

    // T12 T17 (SPEC §4.3): a second signal during the drain cuts it short:
    // the stream still running is cancelled at once instead of at the bound,
    // and the report says the drain was forced.
    #[tokio::test]
    async fn a_second_signal_forces_the_drain() {
        let admission = Admission::new();
        let (address, _stop) = served(&admission).await;
        let streaming = reqwest::Client::new()
            .get(format!("http://{address}/slow"))
            .send()
            .await
            .unwrap();
        let started = Instant::now();
        let report = admission
            .drain_unless(
                Duration::from_secs(60),
                tokio::time::sleep(Duration::from_millis(100)),
            )
            .await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert!(report.forced && !report.drained);
        assert_eq!(report.cancelled, 1);
        assert_eq!(report.to_json()["forced"], true);
        let body = streaming.text().await.unwrap_or_default();
        assert!(!body.contains("chunk 49"), "the stream was cut: {body}");
        // Nothing left to wait for: a forced drain of an idle gate is clean.
        let idle = Admission::new();
        let report = idle.drain_unless(Duration::from_secs(60), async {}).await;
        assert!(report.drained);
    }

    // T01: an idle role drains at once.
    #[tokio::test]
    async fn an_idle_gate_drains_immediately() {
        let admission = Admission::new();
        let report = admission.drain(Duration::from_secs(30)).await;
        assert_eq!(
            report,
            DrainReport {
                in_flight_at_close: 0,
                cancelled: 0,
                drained: true,
                forced: false,
            }
        );
    }

    // T03 T17: the standalone drain bound is `shutdown.drain_timeout` in its
    // role document; a malformed or out-of-range value is refused, never read
    // as the default.
    #[test]
    fn the_standalone_drain_bound_is_read_from_its_document() {
        let state = tempfile::tempdir().unwrap();
        assert_eq!(standalone_drain_bound(state.path()), Ok(DEFAULT_DRAIN));
        std::fs::create_dir_all(state.path().join("config")).unwrap();
        let path = state.path().join("config/standalone.yaml");
        let base = "schema_version: 1\nkind: standalone\nname: local\n";
        std::fs::write(&path, base).unwrap();
        assert_eq!(standalone_drain_bound(state.path()), Ok(DEFAULT_DRAIN));
        std::fs::write(&path, format!("{base}shutdown:\n  drain_timeout: 7s\n")).unwrap();
        assert_eq!(
            standalone_drain_bound(state.path()),
            Ok(Duration::from_secs(7))
        );
        for bad in ["later", "601s"] {
            std::fs::write(&path, format!("{base}shutdown:\n  drain_timeout: {bad}\n")).unwrap();
            let error = standalone_drain_bound(state.path()).unwrap_err();
            assert!(error.contains("shutdown.drain_timeout"), "{error}");
        }
    }

    // T03 (SPEC §15.2, R13): with `--config` the drain bound comes from the
    // explicit document, not from the implicit one under the state root.
    #[test]
    fn the_standalone_drain_bound_follows_an_explicit_document() {
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(state.path().join("config")).unwrap();
        let base = "schema_version: 1\nkind: standalone\nname: local\n";
        std::fs::write(
            state.path().join("config/standalone.yaml"),
            format!("{base}shutdown:\n  drain_timeout: 7s\n"),
        )
        .unwrap();
        let explicit = state.path().join("explicit.yaml");
        std::fs::write(
            &explicit,
            format!("{base}shutdown:\n  drain_timeout: 45s\n"),
        )
        .unwrap();
        assert_eq!(
            standalone_drain_bound_in(&explicit),
            Ok(Duration::from_secs(45))
        );
    }
}

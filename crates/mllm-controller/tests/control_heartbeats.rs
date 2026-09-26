//! Owner decision 2026-09-23: application heartbeats on the control session,
//! over real mTLS.
//!
//! The server sends a heartbeat every period to a host that declared them and
//! tracks when it last heard from that host's session. Silence past the suspend
//! bound suspends dispatch to the host (the session stays up, accounting stays);
//! silence past the lost bound ends the session. A peer that predates
//! heartbeats is never sent one and never suspended for silence. On the host
//! side, an agent that stops hearing the controller for the lost bound drops
//! the session and reconnects, leaving its engines alone.
//!
//! CPU-only transport tests: nothing here qualifies a native engine.

use mllm_agent::{
    enrollment::{JoinInvitation, PendingEnrollment},
    identity::{CertificateAuthority, HostKey},
    identity_storage::IdentityDirectory,
    journal::HostJournal,
};
use mllm_controller::{
    agent_sessions::{AgentSessions, HeartbeatPolicy},
    enrollment::EnrollmentAuthority,
    OwnedCoordinatorState,
};
use mllm_protocol::pb::{
    self,
    agent_control_client::AgentControlClient,
    agent_control_server::{AgentControl, AgentControlServer},
    agent_to_server,
    bootstrap_server::Bootstrap,
    server_to_agent,
};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};

fn now() -> i64 {
    mllm_protocol::now_unix_ms() / 1000
}

fn directory() -> tempfile::TempDir {
    let d = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    d
}

async fn eventually(within: Duration, mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(within, async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition never held");
}

/// A fast policy so the bounds are reached within a test.
const FAST: HeartbeatPolicy = HeartbeatPolicy {
    interval: Duration::from_millis(200),
    suspend_after: Duration::from_millis(1_000),
    lost_after: Duration::from_millis(3_000),
};

struct Enrolled {
    sessions: Arc<AgentSessions>,
    authority: Arc<EnrollmentAuthority>,
    identity: PendingEnrollment,
    host: String,
    listener: Option<tokio::net::TcpListener>,
    tls: ServerTlsConfig,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

/// An enrolled host and a controller TLS identity, with the control listener
/// bound but not yet served, so a test chooses which controller answers it.
async fn enrolled(policy: HeartbeatPolicy) -> Enrolled {
    let state_dir = directory();
    let state = Arc::new(Mutex::new(
        OwnedCoordinatorState::open(state_dir.path()).unwrap(),
    ));
    let ca = CertificateAuthority::generate(now()).unwrap();
    let ca_pem = ca.certificate_pem().to_owned();
    let key = HostKey::generate().unwrap();
    let cert = ca.issue_server("localhost", &key, now()).unwrap();
    let authority = Arc::new(EnrollmentAuthority::new(state, ca));
    let sessions = AgentSessions::with_heartbeats(authority.clone(), policy);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!(
        "https://localhost:{}",
        listener.local_addr().unwrap().port()
    );
    let tls = ServerTlsConfig::new()
        .identity(Identity::from_pem(cert.pem, key.private_key_pem()))
        .client_ca_root(Certificate::from_pem(&ca_pem));
    let storage_dir = directory();
    let storage = IdentityDirectory::open(storage_dir.path()).unwrap();
    let invite = authority.invite("spark", 300, now()).unwrap();
    let invitation = JoinInvitation {
        version: 1,
        server_address: address.clone(),
        control_address: address,
        server_ca: ca_pem,
        invitation_id: invite.id,
        invitation_secret: invite.secret,
        host_name: invite.host_name,
        expires_unix: invite.expires_unix,
        recover_host_id: None,
    };
    let mut identity = PendingEnrollment::prepare(&storage, &invitation).unwrap();
    let request = identity.request(&invitation).unwrap();
    let certificate = Bootstrap::enroll(authority.as_ref(), tonic::Request::new(request))
        .await
        .unwrap()
        .into_inner();
    let host = identity
        .accept_certificate(&storage, certificate, now())
        .unwrap();
    Enrolled {
        sessions,
        authority,
        identity,
        host,
        listener: Some(listener),
        tls,
        _dirs: (state_dir, storage_dir),
    }
}

impl Enrolled {
    /// Serve the real controller sessions on the control listener.
    fn serve_sessions(&mut self) -> tokio::task::JoinHandle<()> {
        let listener = self.listener.take().unwrap();
        let service = AgentControlServer::from_arc(self.sessions.clone());
        let tls = self.tls.clone();
        tokio::spawn(async move {
            let _ = Server::builder()
                .tls_config(tls)
                .unwrap()
                .add_service(service)
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await;
        })
    }

    /// Open a raw, reconciled control session as a host, declaring heartbeats
    /// or not. Returns the host's sender and the controller's stream, with the
    /// SessionReady already read.
    async fn raw_session(
        &self,
        heartbeats: bool,
    ) -> (
        tokio::sync::mpsc::Sender<pb::AgentToServer>,
        tonic::Streaming<pb::ServerToAgent>,
        pb::SessionReady,
    ) {
        let channel = self
            .identity
            .control_endpoint(now())
            .unwrap()
            .connect()
            .await
            .unwrap();
        let (send, recv) = tokio::sync::mpsc::channel(64);
        send.send(pb::AgentToServer {
            msg: Some(agent_to_server::Msg::Connect(pb::Connect {
                host_id: self.host.clone(),
                protocol_version: mllm_protocol::PROTOCOL_VERSION.into(),
                heartbeats,
                ..Default::default()
            })),
        })
        .await
        .unwrap();
        let mut stream = AgentControlClient::new(channel)
            .session(ReceiverStream::new(recv))
            .await
            .unwrap()
            .into_inner();
        send.send(pb::AgentToServer {
            msg: Some(agent_to_server::Msg::ReportInventory(pb::ReportInventory {
                envelope: Some(pb::Envelope {
                    host_id: self.host.clone(),
                    protocol_version: mllm_protocol::PROTOCOL_VERSION.into(),
                    ..Default::default()
                }),
                ..Default::default()
            })),
        })
        .await
        .unwrap();
        send.send(pb::AgentToServer {
            msg: Some(agent_to_server::Msg::ReconcileHistory(
                pb::ReconcileHistory {
                    records: vec![],
                    complete: true,
                },
            )),
        })
        .await
        .unwrap();
        let ready = match stream.message().await.unwrap().unwrap().msg {
            Some(server_to_agent::Msg::SessionReady(ready)) => ready,
            other => panic!("expected SessionReady, got {other:?}"),
        };
        (send, stream, ready)
    }
}

fn beat() -> pb::AgentToServer {
    pb::AgentToServer {
        msg: Some(agent_to_server::Msg::Heartbeat(pb::Heartbeat {
            sent_at_unix_ms: mllm_protocol::now_unix_ms(),
        })),
    }
}

/// Drain the controller's frames in the background, counting heartbeats and
/// noting when the stream ends.
fn read_frames(
    mut stream: tonic::Streaming<pb::ServerToAgent>,
) -> (Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let beats = Arc::new(AtomicUsize::new(0));
    let counted = beats.clone();
    let task = tokio::spawn(async move {
        while let Ok(Some(frame)) = stream.message().await {
            if matches!(frame.msg, Some(server_to_agent::Msg::Heartbeat(_))) {
                counted.fetch_add(1, Ordering::SeqCst);
            }
        }
    });
    (beats, task)
}

// T33 T38 T29 T17: a host that declared heartbeats is sent them; when it goes
// silent past the suspend bound its session stops counting as current and the
// suspension hook runs, while the session itself stays up; heard again, the
// same session is current again (readiness is re-proven by the supervisor);
// silent past the lost bound, the session is lost.
#[tokio::test]
async fn a_silent_host_is_suspended_then_lost_and_a_heard_host_is_current_again() {
    let mut enrolled = enrolled(FAST).await;
    let server = enrolled.serve_sessions();
    let suspended = Arc::new(Mutex::new(Vec::<String>::new()));
    {
        let suspended = suspended.clone();
        enrolled
            .sessions
            .on_host_unresponsive(Arc::new(move |host: &str| {
                suspended.lock().unwrap().push(host.to_owned());
            }));
    }
    let host = enrolled.host.clone();
    let sessions = enrolled.sessions.clone();
    let (send, stream, ready) = enrolled.raw_session(true).await;
    assert_eq!(ready.heartbeat_interval_ms, 200);
    assert_eq!(ready.heartbeat_lost_after_ms, 3_000);
    let (beats, reader) = read_frames(stream);
    let session = sessions.current_session(&host).expect("reconciled");

    // Heard every period: current, not unresponsive, and beating both ways.
    for _ in 0..8 {
        send.send(beat()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(
        sessions.current_session(&host).as_deref(),
        Some(session.as_str())
    );
    assert!(!sessions.unresponsive(&host));
    assert!(
        beats.load(Ordering::SeqCst) >= 4,
        "the controller sends heartbeats"
    );

    // Silent: suspended past the suspend bound, the session still online.
    let silent = std::time::Instant::now();
    eventually(Duration::from_secs(3), || sessions.unresponsive(&host)).await;
    // The last heartbeat went out one period before `silent` began.
    assert!(
        silent.elapsed() >= Duration::from_millis(700),
        "{:?}",
        silent.elapsed()
    );
    assert!(sessions.current_session(&host).is_none());
    let view = sessions.inspect(&host).unwrap();
    assert!(view.online && view.unresponsive && !view.eligible);
    eventually(Duration::from_secs(2), || {
        !suspended.lock().unwrap().is_empty()
    })
    .await;
    assert_eq!(*suspended.lock().unwrap(), vec![host.clone()]);
    use mllm_controller::coordinator::ServiceObservation;
    assert!(!sessions.eligible_hosts().unwrap().contains(&host));

    // Heard again inside the lost bound: the same session is current again.
    send.send(beat()).await.unwrap();
    eventually(Duration::from_secs(2), || !sessions.unresponsive(&host)).await;
    assert_eq!(
        sessions.current_session(&host).as_deref(),
        Some(session.as_str())
    );

    // Silent past the lost bound: the session is lost and its stream closed.
    let silent = std::time::Instant::now();
    eventually(Duration::from_secs(6), || {
        sessions.inspect(&host).is_some_and(|view| !view.online)
    })
    .await;
    assert!(
        silent.elapsed() >= Duration::from_millis(2_500),
        "{:?}",
        silent.elapsed()
    );
    assert!(sessions.current_session(&host).is_none());
    tokio::time::timeout(Duration::from_secs(5), reader)
        .await
        .expect("the lost session's stream ends")
        .unwrap();
    assert_eq!(
        suspended.lock().unwrap().len(),
        2,
        "suspended before it was lost"
    );
    drop(send);
    server.abort();
}

// T34 T33: a peer that predates heartbeats keeps working. It is never sent a
// heartbeat, is told none are expected, and silence never suspends or loses it.
#[tokio::test]
async fn a_peer_without_heartbeats_is_never_suspended_for_silence() {
    let mut enrolled = enrolled(FAST).await;
    let server = enrolled.serve_sessions();
    let host = enrolled.host.clone();
    let sessions = enrolled.sessions.clone();
    let (send, stream, ready) = enrolled.raw_session(false).await;
    assert_eq!(
        (ready.heartbeat_interval_ms, ready.heartbeat_lost_after_ms),
        (0, 0)
    );
    let (beats, _reader) = read_frames(stream);
    let session = sessions.current_session(&host).expect("reconciled");
    tokio::time::sleep(FAST.lost_after + Duration::from_millis(500)).await;
    assert_eq!(
        sessions.current_session(&host).as_deref(),
        Some(session.as_str())
    );
    assert!(!sessions.unresponsive(&host));
    assert!(sessions.inspect(&host).unwrap().online);
    assert_eq!(beats.load(Ordering::SeqCst), 0, "never sent a heartbeat");
    drop(send);
    server.abort();
}

/// A controller that reconciles every session and then says nothing more,
/// optionally asking for heartbeats: a frozen controller, as the host sees it.
struct SilentController {
    controller_id: String,
    ask_heartbeats: bool,
    sessions: AtomicUsize,
    beats: Arc<AtomicUsize>,
    /// Inventory reports whose every domain was unobserved (`-1`).
    unobserved: Arc<AtomicUsize>,
}

#[tonic::async_trait]
impl AgentControl for SilentController {
    type SessionStream = ReceiverStream<Result<pb::ServerToAgent, tonic::Status>>;
    async fn session(
        &self,
        request: tonic::Request<tonic::Streaming<pb::AgentToServer>>,
    ) -> Result<tonic::Response<Self::SessionStream>, tonic::Status> {
        let number = self.sessions.fetch_add(1, Ordering::SeqCst);
        let mut incoming = request.into_inner();
        let (outgoing, receiver) = tokio::sync::mpsc::channel(16);
        let controller_id = self.controller_id.clone();
        let ask = self.ask_heartbeats;
        let beats = self.beats.clone();
        let unobserved = self.unobserved.clone();
        tokio::spawn(async move {
            while let Ok(Some(frame)) = incoming.message().await {
                match frame.msg {
                    Some(agent_to_server::Msg::ReconcileHistory(page)) if page.complete => {
                        let _ = outgoing
                            .send(Ok(pb::ServerToAgent {
                                msg: Some(server_to_agent::Msg::SessionReady(pb::SessionReady {
                                    controller_id: controller_id.clone(),
                                    session_id: format!("session-{number}"),
                                    heartbeat_interval_ms: if ask { 250 } else { 0 },
                                    heartbeat_lost_after_ms: if ask { 3_000 } else { 0 },
                                    ..Default::default()
                                })),
                            }))
                            .await;
                    }
                    Some(agent_to_server::Msg::Heartbeat(_)) => {
                        beats.fetch_add(1, Ordering::SeqCst);
                    }
                    Some(agent_to_server::Msg::ReportInventory(inventory))
                        if !inventory.domains.is_empty()
                            && inventory.domains.iter().all(|d| d.available_bytes == -1) =>
                    {
                        unobserved.fetch_add(1, Ordering::SeqCst);
                    }
                    _ => {}
                }
            }
            drop(outgoing);
        });
        Ok(tonic::Response::new(ReceiverStream::new(receiver)))
    }
}

type Dirs = (tempfile::TempDir, tempfile::TempDir, tempfile::TempDir);

async fn run_agent_against(
    ask_heartbeats: bool,
) -> (
    Arc<SilentController>,
    tokio::sync::watch::Sender<bool>,
    tokio::task::JoinHandle<()>,
    Dirs,
) {
    run_agent_with(ask_heartbeats, None).await
}

async fn run_agent_with(
    ask_heartbeats: bool,
    execution: Option<Arc<dyn mllm_agent::session::SessionExecution>>,
) -> (
    Arc<SilentController>,
    tokio::sync::watch::Sender<bool>,
    tokio::task::JoinHandle<()>,
    Dirs,
) {
    let Enrolled {
        authority,
        identity,
        host,
        listener,
        tls,
        _dirs: (state_dir, storage_dir),
        ..
    } = enrolled(HeartbeatPolicy::default()).await;
    let controller = Arc::new(SilentController {
        controller_id: authority.controller_id(),
        ask_heartbeats,
        sessions: AtomicUsize::new(0),
        beats: Arc::new(AtomicUsize::new(0)),
        unobserved: Arc::new(AtomicUsize::new(0)),
    });
    let listener = listener.unwrap();
    let service = AgentControlServer::from_arc(controller.clone());
    let server = tokio::spawn(async move {
        let _ = Server::builder()
            .tls_config(tls)
            .unwrap()
            .add_service(service)
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await;
    });
    let journal_dir = directory();
    let journal = HostJournal::open(journal_dir.path(), &identity.controller_id(), &host).unwrap();
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let _ = mllm_agent::session::run_session_with_execution(
            &identity,
            journal,
            pb::ReportInventory::default(),
            shutdown,
            execution,
        )
        .await;
    });
    (
        controller,
        stop,
        server,
        (state_dir, storage_dir, journal_dir),
    )
}

// T33 T17: a host agent that stops hearing a controller that asked for
// heartbeats drops the session after the lost bound and reconnects; its own
// heartbeats reached the controller meanwhile. Engines are not touched: a
// session end retains every claim (SPEC §13).
#[tokio::test]
async fn an_agent_reconnects_when_controller_heartbeats_stop() {
    let (controller, stop, server, _dirs) = run_agent_against(true).await;
    eventually(Duration::from_secs(5), || {
        controller.sessions.load(Ordering::SeqCst) >= 1
    })
    .await;
    let first = std::time::Instant::now();
    eventually(Duration::from_secs(12), || {
        controller.sessions.load(Ordering::SeqCst) >= 2
    })
    .await;
    // The lost bound (3 s) plus the first reconnect delay.
    assert!(
        first.elapsed() >= Duration::from_millis(2_900),
        "{:?}",
        first.elapsed()
    );
    assert!(
        controller.beats.load(Ordering::SeqCst) >= 4,
        "the agent sent heartbeats"
    );
    stop.send(true).unwrap();
    server.abort();
}

// T34: against a controller that asked for no heartbeats the agent sends none
// and never drops a quiet session.
#[tokio::test]
async fn an_agent_keeps_a_quiet_session_with_a_controller_without_heartbeats() {
    let (controller, stop, server, _dirs) = run_agent_against(false).await;
    eventually(Duration::from_secs(5), || {
        controller.sessions.load(Ordering::SeqCst) >= 1
    })
    .await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(controller.sessions.load(Ordering::SeqCst), 1);
    assert_eq!(controller.beats.load(Ordering::SeqCst), 0);
    stop.send(true).unwrap();
    server.abort();
}

/// A host whose one device domain is read through the cached GPU sampler, as
/// the native host reads it, from a collector that hangs until released.
struct HungGpuHost {
    gpu: Arc<mllm_agent::gpu_memory::CachedGpuSampler>,
}

impl mllm_agent::session::SessionExecution for HungGpuHost {
    fn execute(
        &self,
        _session: u64,
        _command: mllm_protocol::execution::MemberCommand,
    ) -> mllm_agent::session::ExecutionFuture {
        Box::pin(async { Err(mllm_agent::session::SessionError) })
    }
    fn inventory(&self) -> Option<pb::ReportInventory> {
        let observed = self
            .gpu
            .current()
            .and_then(|sample| sample.devices.first()?.memory.clone());
        let (available, capacity) =
            observed.map_or((-1, -1), |memory| (memory.free_bytes, memory.total_bytes));
        Some(pb::ReportInventory {
            domains: vec![pb::DomainObservation {
                domain_id: "gpu0".into(),
                kind: "device".into(),
                observed_bytes: available,
                available_bytes: available,
                capacity_bytes: capacity,
                ..Default::default()
            }],
            ..Default::default()
        })
    }
}

// T33 T26 (review decision, discrete GPU): device memory is sampled off the
// session loop. A collector that hangs far longer than the heartbeat period
// never delays a heartbeat (5 s of silence suspends the host); the device is
// reported unobserved meanwhile instead of the loop waiting for it.
#[tokio::test]
async fn a_hung_gpu_collector_never_delays_heartbeats() {
    let (release, gate) = std::sync::mpsc::channel::<()>();
    let gate = Mutex::new(gate);
    let host = Arc::new(HungGpuHost {
        gpu: mllm_agent::gpu_memory::CachedGpuSampler::new(Arc::new(move || {
            gate.lock().unwrap().recv().ok()?;
            None
        })),
    });
    let (controller, stop, server, _dirs) = run_agent_with(true, Some(host)).await;
    eventually(Duration::from_secs(5), || {
        controller.beats.load(Ordering::SeqCst) >= 1
    })
    .await;
    let (beats, unobserved) = (
        controller.beats.load(Ordering::SeqCst),
        controller.unobserved.load(Ordering::SeqCst),
    );
    // Six 250 ms periods while the collector stays hung.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let sent = controller.beats.load(Ordering::SeqCst) - beats;
    assert!(sent >= 4, "{sent} heartbeats in 1.5 s");
    assert!(
        controller.unobserved.load(Ordering::SeqCst) - unobserved >= 2,
        "the device is reported unobserved while the collector hangs"
    );
    assert_eq!(controller.sessions.load(Ordering::SeqCst), 1);
    release.send(()).unwrap();
    stop.send(true).unwrap();
    server.abort();
}

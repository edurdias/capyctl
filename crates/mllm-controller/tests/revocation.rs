//! SPEC §4.1 (revocable certificates; a revoked host accepts no new commands
//! through an old connection), §13.1 (the agent rejects stale commands) and
//! §13.3 (revocation closes control sessions and prevents new work), over the
//! real mutual-TLS control stream with a real agent session and host journal.
//! F3 exit gate (§18): revocation and stale-command tests.
//!
//! CPU-only: these prove the control-plane contract, not a native engine.
use mllm_agent::{
    enrollment::{JoinInvitation, PendingEnrollment},
    identity::{CertificateAuthority, HostKey},
    identity_storage::IdentityDirectory,
    journal::HostJournal,
};
use mllm_controller::{
    agent_sessions::AgentSessions, enrollment::EnrollmentAuthority, OwnedCoordinatorState,
};
use mllm_protocol::{
    execution::{MemberAction, MemberCommand},
    pb::{self, agent_control_client::AgentControlClient, agent_control_server::AgentControlServer, agent_to_server, bootstrap_server::Bootstrap},
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
async fn eventually(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}

/// Inspect-only host policy: nothing here launches a process.
struct InspectPolicy;
impl mllm_agent::journal::LocalExecutionPolicy for InspectPolicy {
    fn authorize(&self, command: &MemberCommand) -> Result<(), mllm_agent::journal::JournalError> {
        if matches!(command.action, MemberAction::Inspect) {
            Ok(())
        } else {
            Err(mllm_agent::journal::JournalError::Unauthorized)
        }
    }
    fn render_launch(
        &self,
        _: &MemberCommand,
    ) -> Result<mllm_agent::journal::ApprovedLaunch, mllm_agent::journal::JournalError> {
        Err(mllm_agent::journal::JournalError::Unauthorized)
    }
}

/// The host side: every command goes through the host journal's fences, as
/// the native execution does. A command the journal refuses ends the session
/// with no result (SPEC §13: no effect, no fabricated evidence).
struct JournalExecutor {
    journal: Arc<HostJournal>,
    fresh: Arc<AtomicUsize>,
}
impl mllm_agent::session::SessionExecution for JournalExecutor {
    fn execute(&self, session: u64, command: MemberCommand) -> mllm_agent::session::ExecutionFuture {
        let journal = self.journal.clone();
        let fresh = self.fresh.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let now = mllm_protocol::now_unix_ms();
                match journal
                    .accept(session, &command, now, &InspectPolicy)
                    .map_err(|_| mllm_agent::session::SessionError)?
                {
                    mllm_agent::journal::Acceptance::Fresh(ticket) => {
                        fresh.fetch_add(1, Ordering::SeqCst);
                        journal
                            .execute(ticket, now, &InspectPolicy)
                            .map_err(|_| mllm_agent::session::SessionError)?;
                    }
                    mllm_agent::journal::Acceptance::Replay(_) => {}
                }
                journal
                    .execution_result(&command.identity.command_id, mllm_protocol::now_unix_ms())
                    .map_err(|_| mllm_agent::session::SessionError)
            })
            .await
            .map_err(|_| mllm_agent::session::SessionError)?
        })
    }
}

/// A server with an enrolled host whose agent session runs against a real
/// host journal over mutual TLS.
struct Harness {
    state: Arc<Mutex<OwnedCoordinatorState>>,
    authority: Arc<EnrollmentAuthority>,
    sessions: Arc<AgentSessions>,
    host: String,
    controller_id: String,
    journal: Arc<HostJournal>,
    fresh: Arc<AtomicUsize>,
    identity: Arc<PendingEnrollment>,
    stop: tokio::sync::watch::Sender<bool>,
    agent: tokio::task::JoinHandle<Result<(), mllm_agent::session::SessionError>>,
    server: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
    _dirs: Vec<tempfile::TempDir>,
}

impl Harness {
    async fn start() -> Self {
        let state_dir = directory();
        let state = Arc::new(Mutex::new(OwnedCoordinatorState::open(state_dir.path()).unwrap()));
        let ca = CertificateAuthority::generate(now()).unwrap();
        let ca_pem = ca.certificate_pem().to_owned();
        let key = HostKey::generate().unwrap();
        let cert = ca.issue_server("localhost", &key, now()).unwrap();
        let authority = Arc::new(EnrollmentAuthority::new(state.clone(), ca));
        let sessions = AgentSessions::new(authority.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("https://localhost:{}", listener.local_addr().unwrap().port());
        let server = tokio::spawn(
            Server::builder()
                .tls_config(
                    ServerTlsConfig::new()
                        .identity(Identity::from_pem(cert.pem, key.private_key_pem()))
                        .client_ca_root(Certificate::from_pem(&ca_pem)),
                )
                .unwrap()
                .add_service(
                    AgentControlServer::from_arc(sessions.clone()).max_decoding_message_size(65536),
                )
                .serve_with_incoming(TcpListenerStream::new(listener)),
        );
        let storage_dir = directory();
        let storage = IdentityDirectory::open(storage_dir.path()).unwrap();
        let invite = authority.invite("spark", 300, now()).unwrap();
        let invitation = JoinInvitation {
            version: 1,
            server_address: address.clone(),
            control_address: address.clone(),
            server_ca: ca_pem.clone(),
            invitation_id: invite.id,
            invitation_secret: invite.secret,
            host_name: invite.host_name,
            expires_unix: invite.expires_unix,
        };
        let mut identity = PendingEnrollment::prepare(&storage, &invitation).unwrap();
        let request = identity.request(&invitation).unwrap();
        let certificate = Bootstrap::enroll(authority.as_ref(), tonic::Request::new(request))
            .await
            .unwrap()
            .into_inner();
        let host = identity.accept_certificate(&storage, certificate, now()).unwrap();
        let controller_id = identity.controller_id();
        let identity = Arc::new(identity);
        let journal_dir = directory();
        let journal = HostJournal::open(journal_dir.path(), &controller_id, &host).unwrap();
        let fresh = Arc::new(AtomicUsize::new(0));
        let executor = Arc::new(JournalExecutor { journal: journal.clone(), fresh: fresh.clone() });
        let (stop, shutdown) = tokio::sync::watch::channel(false);
        let agent_identity = identity.clone();
        let agent_journal = journal.clone();
        let agent = tokio::spawn(async move {
            mllm_agent::session::run_session_with_execution(
                &agent_identity,
                agent_journal,
                pb::ReportInventory {
                    domains: vec![pb::DomainObservation {
                        domain_id: "system-memory".into(),
                        kind: "system".into(),
                        observed_bytes: 1024,
                        observed_at_unix: now(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                shutdown,
                Some(executor),
            )
            .await
        });
        let named = host.clone();
        let watched = sessions.clone();
        eventually(|| watched.inspect(&named).is_some_and(|s| s.online && s.reconciled)).await;
        Self {
            state,
            authority,
            sessions,
            host,
            controller_id,
            journal,
            fresh,
            identity,
            stop,
            agent,
            server,
            _dirs: vec![state_dir, storage_dir, journal_dir],
        }
    }

    /// An Inspect of instance 0 of `deployment` at `generation`.
    fn inspect(&self, id: &str, generation: i64, deadline_in: Duration) -> MemberCommand {
        let mut command = MemberCommand {
            identity: mllm_domain::group::CommandIdentity {
                controller_id: self.controller_id.clone(),
                member: mllm_domain::group::MemberKey {
                    host_id: self.host.clone(),
                    member_id: "head".into(),
                },
                deployment_id: "deployment".into(),
                operation_id: format!("operation-{id}"),
                command_id: id.into(),
                step_id: format!("step-{id}"),
                generation,
                revision: 1,
                deadline_ms: mllm_protocol::now_unix_ms() + deadline_in.as_millis() as i64,
                payload_digest: [0; 32],
                expected_state: "stopped".into(),
                profile_fingerprint: "local-profile".into(),
                instance_index: 0,
            },
            action: MemberAction::Inspect,
        };
        command.identity.payload_digest = command.canonical_digest();
        command
    }

    fn journaled(&self) -> Vec<String> {
        self.journal
            .history(0, 100)
            .unwrap()
            .into_iter()
            .map(|record| record.command_id)
            .collect()
    }

    fn online(&self) -> bool {
        self.sessions.inspect(&self.host).is_some_and(|s| s.online)
    }

    async fn finish(self) {
        self.stop.send(true).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(15), self.agent).await;
        self.server.abort();
    }
}

// T34 (SPEC §13.1, F3 stale-command gate, live row M44's intent): a replayed
// command is answered from the host journal without a second effect, and an
// older-generation command for the same instance is refused by the journal's
// fence however often it is redelivered: it never executes and yields no
// result. A forged command (payload not matching its digest) never leaves the
// controller.
#[tokio::test]
async fn stale_generation_replay_is_refused_end_to_end() {
    let h = Harness::start().await;
    let current = h.inspect("inspect-generation-2", 2, Duration::from_secs(10));
    let result = tokio::time::timeout(Duration::from_secs(12), h.sessions.execute(current.clone()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.state, "completed");
    mllm_protocol::execution::validate_result(&current, &result).unwrap();
    assert_eq!(h.fresh.load(Ordering::SeqCst), 1);

    // At-least-once redelivery of the same command: the journal replays its
    // result, and nothing executes again.
    let replay = tokio::time::timeout(Duration::from_secs(12), h.sessions.execute(current.clone()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replay.state, "completed");
    assert_eq!(h.fresh.load(Ordering::SeqCst), 1, "a replay executed again");

    // An old-generation command for the same instance, validly signed and
    // redelivered on every reconnect until its deadline: refused each time.
    let stale = h.inspect("inspect-generation-1", 1, Duration::from_secs(3));
    let outcome = tokio::time::timeout(Duration::from_secs(12), h.sessions.execute(stale.clone()))
        .await
        .expect("the stale command's observation ends by its deadline");
    assert!(outcome.is_err(), "a stale generation produced a result: {outcome:?}");
    assert_eq!(h.fresh.load(Ordering::SeqCst), 1, "a stale generation executed");
    assert_eq!(h.journaled(), vec!["inspect-generation-2".to_owned()]);

    // A forged command: its payload no longer matches its digest.
    let mut forged = h.inspect("inspect-forged", 3, Duration::from_secs(10));
    forged.identity.expected_state = "ready".into();
    assert!(h.sessions.dispatch(&h.host, forged.to_wire()).is_err());
    assert!(h.sessions.execute(forged).await.is_err());
    assert_eq!(h.fresh.load(Ordering::SeqCst), 1);

    // The fence did not wedge the host: a newer generation still executes
    // once its session is back.
    let host = h.host.clone();
    let sessions = h.sessions.clone();
    eventually(|| sessions.inspect(&host).is_some_and(|s| s.online && s.reconciled)).await;
    let newer = h.inspect("inspect-generation-3", 3, Duration::from_secs(10));
    let result = tokio::time::timeout(Duration::from_secs(12), h.sessions.execute(newer))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.state, "completed");
    assert_eq!(h.fresh.load(Ordering::SeqCst), 2);
    h.finish().await;
}

// T06 (SPEC §§4.1, 13.3; F3 revocation gate): revoking a host by name closes
// its live control session at once; the host cannot reconnect with its old
// certificate; no command reaches it; the revocation is journaled once, and a
// repeated revocation (by id) is an idempotent no-op.
#[tokio::test]
async fn revocation_closes_the_session_and_refuses_reconnect_and_commands() {
    let h = Harness::start().await;
    let before = h.inspect("inspect-before", 1, Duration::from_secs(10));
    let result = tokio::time::timeout(Duration::from_secs(12), h.sessions.execute(before))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.state, "completed");
    assert_eq!(h.fresh.load(Ordering::SeqCst), 1);

    let revocation = h.authority.revoke("spark").unwrap();
    assert_eq!(revocation.host_id, h.host);
    assert_eq!(revocation.host_name, "spark");
    assert!(revocation.newly_revoked);
    // The live session closes promptly, without waiting for any timeout.
    let started = std::time::Instant::now();
    let host = h.host.clone();
    let sessions = h.sessions.clone();
    eventually(|| !sessions.inspect(&host).is_some_and(|s| s.online)).await;
    assert!(started.elapsed() < Duration::from_secs(5));

    // No new command reaches the host: dispatch refuses, and an execution
    // observes nothing before its deadline.
    let after = h.inspect("inspect-after", 2, Duration::from_secs(2));
    assert!(h.sessions.dispatch(&h.host, after.to_wire()).is_err());
    let outcome = tokio::time::timeout(Duration::from_secs(10), h.sessions.execute(after))
        .await
        .expect("the refused command's observation ends by its deadline");
    assert!(outcome.is_err());

    // The agent keeps retrying with its old certificate; every attempt is
    // refused, so the host never comes back online.
    for _ in 0..30 {
        assert!(!h.online(), "a revoked host reconnected");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // An explicit fresh connection with the old identity is refused too.
    let channel = h.identity.control_endpoint(now()).unwrap().connect().await.unwrap();
    let (send, recv) = tokio::sync::mpsc::channel(16);
    send.send(pb::AgentToServer {
        msg: Some(agent_to_server::Msg::Connect(pb::Connect {
            host_id: h.host.clone(),
            protocol_version: mllm_protocol::PROTOCOL_VERSION.into(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    assert!(AgentControlClient::new(channel)
        .session(ReceiverStream::new(recv))
        .await
        .is_err());
    assert_eq!(h.fresh.load(Ordering::SeqCst), 1, "a revoked host executed a command");
    assert_eq!(h.journaled(), vec!["inspect-before".to_owned()]);

    // Idempotent: revoking again, by id, changes nothing.
    let again = h.authority.revoke(&h.host).unwrap();
    assert_eq!(again.host_id, h.host);
    assert!(!again.newly_revoked);
    assert!(matches!(
        h.authority.revoke("unknown-host"),
        Err(mllm_controller::enrollment::EnrollmentRefusal::NotFound)
    ));
    let events = h.state.lock().unwrap().store().events_after(None, 1000).unwrap();
    let revoked: Vec<_> = events.events.iter().filter(|e| e.kind == "host_revoked").collect();
    assert_eq!(revoked.len(), 1, "one journal entry per revocation");
    assert!(revoked[0].payload_json.contains(&h.host));
    let hosts = h.state.lock().unwrap().store().enrolled_hosts().unwrap();
    assert!(hosts.iter().all(|host| host.revoked));
    h.finish().await;
}

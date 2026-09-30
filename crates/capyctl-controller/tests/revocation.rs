//! SPEC §4.1 (revocable certificates; a revoked host accepts no new commands
//! through an old connection), §13.1 (the agent rejects stale commands) and
//! §13.3 (revocation closes control sessions and prevents new work), over the
//! real mutual-TLS control stream with a real agent session and host journal.
//! F3 exit gate (§18): revocation and stale-command tests.
//!
//! CPU-only: these prove the control-plane contract, not a native engine.
use capyctl_agent::{
    enrollment::{JoinInvitation, PendingEnrollment},
    identity::{CertificateAuthority, HostKey},
    identity_storage::IdentityDirectory,
    journal::HostJournal,
};
use capyctl_controller::{
    agent_sessions::AgentSessions, enrollment::EnrollmentAuthority, OwnedCoordinatorState,
};
use capyctl_protocol::{
    execution::{MemberAction, MemberCommand},
    pb::{
        self, agent_control_client::AgentControlClient, agent_control_server::AgentControlServer,
        agent_to_server, bootstrap_server::Bootstrap,
    },
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
    capyctl_protocol::now_unix_ms() / 1000
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
impl capyctl_agent::journal::LocalExecutionPolicy for InspectPolicy {
    fn authorize(
        &self,
        command: &MemberCommand,
    ) -> Result<(), capyctl_agent::journal::JournalError> {
        if matches!(command.action, MemberAction::Inspect) {
            Ok(())
        } else {
            Err(capyctl_agent::journal::JournalError::Unauthorized)
        }
    }
    fn render_launch(
        &self,
        _: &MemberCommand,
    ) -> Result<capyctl_agent::journal::ApprovedLaunch, capyctl_agent::journal::JournalError> {
        Err(capyctl_agent::journal::JournalError::Unauthorized)
    }
}

/// The host side: every command goes through the host journal's fences, as
/// the native execution does. A command the journal refuses ends the session
/// with no result (SPEC §13: no effect, no fabricated evidence).
struct JournalExecutor {
    journal: Arc<HostJournal>,
    fresh: Arc<AtomicUsize>,
}
impl capyctl_agent::session::SessionExecution for JournalExecutor {
    fn execute(
        &self,
        session: u64,
        command: MemberCommand,
    ) -> capyctl_agent::session::ExecutionFuture {
        let journal = self.journal.clone();
        let fresh = self.fresh.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let now = capyctl_protocol::now_unix_ms();
                match journal
                    .accept(session, &command, now, &InspectPolicy)
                    .map_err(|_| capyctl_agent::session::SessionError)?
                {
                    capyctl_agent::journal::Acceptance::Fresh(ticket) => {
                        fresh.fetch_add(1, Ordering::SeqCst);
                        journal
                            .execute(ticket, now, &InspectPolicy)
                            .map_err(|_| capyctl_agent::session::SessionError)?;
                    }
                    capyctl_agent::journal::Acceptance::Replay(_) => {}
                }
                journal
                    .execution_result(
                        &command.identity.command_id,
                        capyctl_protocol::now_unix_ms(),
                    )
                    .map_err(|_| capyctl_agent::session::SessionError)
            })
            .await
            .map_err(|_| capyctl_agent::session::SessionError)?
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
    agent: tokio::task::JoinHandle<Result<(), capyctl_agent::session::HostRevoked>>,
    server: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
    address: String,
    _dirs: Vec<tempfile::TempDir>,
}

impl Harness {
    async fn start() -> Self {
        let state_dir = directory();
        let state = Arc::new(Mutex::new(
            OwnedCoordinatorState::open(state_dir.path()).unwrap(),
        ));
        let ca = CertificateAuthority::generate(now()).unwrap();
        let ca_pem = ca.certificate_pem().to_owned();
        let key = HostKey::generate().unwrap();
        let cert = ca.issue_server("localhost", &key, now()).unwrap();
        let authority = Arc::new(EnrollmentAuthority::new(state.clone(), ca));
        let sessions = AgentSessions::new(authority.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!(
            "https://localhost:{}",
            listener.local_addr().unwrap().port()
        );
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
        let controller_id = identity.controller_id();
        let identity = Arc::new(identity);
        let journal_dir = directory();
        let journal = HostJournal::open(journal_dir.path(), &controller_id, &host).unwrap();
        let fresh = Arc::new(AtomicUsize::new(0));
        let executor = Arc::new(JournalExecutor {
            journal: journal.clone(),
            fresh: fresh.clone(),
        });
        let (stop, shutdown) = tokio::sync::watch::channel(false);
        let agent_identity = identity.clone();
        let agent_journal = journal.clone();
        let agent = tokio::spawn(async move {
            capyctl_agent::session::run_session_with_execution(
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
        eventually(|| {
            watched
                .inspect(&named)
                .is_some_and(|s| s.online && s.reconciled)
        })
        .await;
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
            address,
            _dirs: vec![state_dir, storage_dir, journal_dir],
        }
    }

    /// An Inspect of instance 0 of `deployment` at `generation`.
    fn inspect(&self, id: &str, generation: i64, deadline_in: Duration) -> MemberCommand {
        let mut command = MemberCommand {
            identity: capyctl_domain::group::CommandIdentity {
                controller_id: self.controller_id.clone(),
                member: capyctl_domain::group::MemberKey {
                    host_id: self.host.clone(),
                    member_id: "head".into(),
                },
                deployment_id: "deployment".into(),
                operation_id: format!("operation-{id}"),
                command_id: id.into(),
                step_id: format!("step-{id}"),
                generation,
                revision: 1,
                deadline_ms: capyctl_protocol::now_unix_ms() + deadline_in.as_millis() as i64,
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
        let _ = self.stop.send(true);
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
    capyctl_protocol::execution::validate_result(&current, &result).unwrap();
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
    assert!(
        outcome.is_err(),
        "a stale generation produced a result: {outcome:?}"
    );
    assert_eq!(
        h.fresh.load(Ordering::SeqCst),
        1,
        "a stale generation executed"
    );
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
    eventually(|| {
        sessions
            .inspect(&host)
            .is_some_and(|s| s.online && s.reconciled)
    })
    .await;
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
    let mut h = Harness::start().await;
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

    // SPEC §4.1, ADR 0016 (owner decision 2026-09-24): the controller told
    // the agent its certificate is revoked, so the agent stops reconnecting
    // and returns `HostRevoked` instead of retrying forever. It never came
    // back online meanwhile.
    assert!(!h.online(), "a revoked host reconnected");
    let agent = std::mem::replace(&mut h.agent, tokio::spawn(async { Ok(()) }));
    let ended = tokio::time::timeout(Duration::from_secs(10), agent)
        .await
        .expect("a revoked agent stops reconnecting")
        .unwrap();
    assert!(
        matches!(ended, Err(capyctl_agent::session::HostRevoked)),
        "{ended:?}"
    );
    assert!(!h.online(), "a revoked host reconnected");
    // An explicit fresh connection with the old identity is refused with the
    // typed revocation answer, which is what the agent acted on.
    let channel = h
        .identity
        .control_endpoint(now())
        .unwrap()
        .connect()
        .await
        .unwrap();
    let (send, recv) = tokio::sync::mpsc::channel(16);
    send.send(pb::AgentToServer {
        msg: Some(agent_to_server::Msg::Connect(pb::Connect {
            host_id: h.host.clone(),
            protocol_version: capyctl_protocol::PROTOCOL_VERSION.into(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    let refusal = AgentControlClient::new(channel)
        .session(ReceiverStream::new(recv))
        .await
        .unwrap_err();
    assert!(
        capyctl_protocol::is_host_revoked_refusal(&refusal),
        "{refusal:?}"
    );
    assert_eq!(
        h.fresh.load(Ordering::SeqCst),
        1,
        "a revoked host executed a command"
    );
    assert_eq!(h.journaled(), vec!["inspect-before".to_owned()]);

    // Idempotent: revoking again, by id, changes nothing.
    let again = h.authority.revoke(&h.host).unwrap();
    assert_eq!(again.host_id, h.host);
    assert!(!again.newly_revoked);
    assert!(matches!(
        h.authority.revoke("unknown-host"),
        Err(capyctl_controller::enrollment::EnrollmentRefusal::NotFound)
    ));
    let events = h
        .state
        .lock()
        .unwrap()
        .store()
        .events_after(None, 1000)
        .unwrap();
    let revoked: Vec<_> = events
        .events
        .iter()
        .filter(|e| e.kind == "host_revoked")
        .collect();
    assert_eq!(revoked.len(), 1, "one journal entry per revocation");
    assert!(revoked[0].payload_json.contains(&h.host));
    let hosts = h.state.lock().unwrap().store().enrolled_hosts().unwrap();
    assert!(hosts.iter().all(|host| host.revoked));
    h.finish().await;
}

impl Harness {
    /// The join file a recovery invitation for `host` becomes.
    fn recovery_invitation(&self, host: &str) -> JoinInvitation {
        let invite = self.authority.invite_recovery(host, 300, now()).unwrap();
        JoinInvitation {
            version: 1,
            server_address: self.address.clone(),
            control_address: self.address.clone(),
            server_ca: invite.ca_pem,
            invitation_id: invite.id,
            invitation_secret: invite.secret,
            host_name: invite.host_name,
            expires_unix: invite.expires_unix,
            recover_host_id: invite.recover_host_id,
        }
    }

    /// Redeem `invitation` from the identity directory at `dir`.
    async fn redeem(
        &self,
        dir: &std::path::Path,
        invitation: &JoinInvitation,
    ) -> Result<PendingEnrollment, ()> {
        let storage = IdentityDirectory::open(dir).unwrap();
        let mut identity =
            PendingEnrollment::prepare_recovery(&storage, invitation).map_err(|_| ())?;
        let request = identity.request(invitation).map_err(|_| ())?;
        let issued = Bootstrap::enroll(self.authority.as_ref(), tonic::Request::new(request))
            .await
            .map_err(|_| ())?
            .into_inner();
        identity
            .accept_certificate(&storage, issued, now())
            .map_err(|_| ())?;
        Ok(identity)
    }
}

// T05 T06 (ADR 0016, SPEC §4.1): a revoked host recovers under its same host
// id over the real mutual-TLS control stream. Recovery of a host that is not
// revoked is refused; after revocation, a recovery invitation redeemed from
// the host's retained identity directory issues a new certificate for the
// same id; the host reconnects with it and its retained journal and executes
// commands again; its old certificate stays refused; the invitation is
// single-use; and the steps are journaled.
#[tokio::test]
async fn a_revoked_host_recovers_its_same_identity_and_the_old_certificate_stays_refused() {
    let mut h = Harness::start().await;
    let before = h.inspect("inspect-before", 1, Duration::from_secs(10));
    tokio::time::timeout(Duration::from_secs(12), h.sessions.execute(before))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        h.authority.invite_recovery("spark", 300, now()),
        Err(capyctl_controller::enrollment::EnrollmentRefusal::NotRevoked)
    ));
    assert!(matches!(
        h.authority.invite_recovery("no-such-host", 300, now()),
        Err(capyctl_controller::enrollment::EnrollmentRefusal::NotFound)
    ));
    h.authority.revoke("spark").unwrap();
    let host = h.host.clone();
    let sessions = h.sessions.clone();
    eventually(|| !sessions.inspect(&host).is_some_and(|s| s.online)).await;
    // The revoked host role stops by itself (ADR 0016, owner decision
    // 2026-09-24); its journal and identity directory stay for recovery.
    let agent = std::mem::replace(&mut h.agent, tokio::spawn(async { Ok(()) }));
    let ended = tokio::time::timeout(Duration::from_secs(15), agent)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(ended, Err(capyctl_agent::session::HostRevoked)),
        "{ended:?}"
    );

    // By id this time; the join file names the host id it re-enrolls.
    let invitation = h.recovery_invitation(&h.host.clone());
    assert_eq!(invitation.recover_host_id.as_deref(), Some(h.host.as_str()));
    assert_eq!(invitation.host_name, "spark");
    // An ordinary enrollment refuses a recovery invitation.
    let other = directory();
    assert!(PendingEnrollment::prepare(
        &IdentityDirectory::open(other.path()).unwrap(),
        &invitation
    )
    .is_err());
    let storage_dir = h._dirs[1].path().to_path_buf();
    let identity = h.redeem(&storage_dir, &invitation).await.unwrap();
    assert_eq!(
        identity.host_id(),
        Some(h.host.as_str()),
        "the same host id"
    );
    // Single use: another enrollment transaction on the same invitation, even
    // from fresh identity files, is refused.
    let fresh = directory();
    assert!(h.redeem(fresh.path(), &invitation).await.is_err());
    // An exact retry from the same identity replays the same certificate.
    let replayed = h.redeem(&storage_dir, &invitation).await.unwrap();
    assert_eq!(replayed.host_id(), Some(h.host.as_str()));

    // The recovered host reconnects with its new certificate and its retained
    // journal, and executes commands again.
    let identity = Arc::new(identity);
    let executor = Arc::new(JournalExecutor {
        journal: h.journal.clone(),
        fresh: h.fresh.clone(),
    });
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let agent_identity = identity.clone();
    let agent_journal = h.journal.clone();
    let agent = tokio::spawn(async move {
        capyctl_agent::session::run_session_with_execution(
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
    let host = h.host.clone();
    let sessions = h.sessions.clone();
    eventually(|| {
        sessions
            .inspect(&host)
            .is_some_and(|s| s.online && s.reconciled)
    })
    .await;
    let after = h.inspect("inspect-after-recovery", 2, Duration::from_secs(10));
    let result = tokio::time::timeout(Duration::from_secs(12), h.sessions.execute(after))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.state, "completed");
    assert_eq!(h.fresh.load(Ordering::SeqCst), 2);
    assert_eq!(
        h.journaled(),
        vec![
            "inspect-before".to_owned(),
            "inspect-after-recovery".to_owned()
        ],
        "the retained journal carried over"
    );

    // The old certificate stays refused: a fresh connection with it fails.
    let channel = h
        .identity
        .control_endpoint(now())
        .unwrap()
        .connect()
        .await
        .unwrap();
    let (send, recv) = tokio::sync::mpsc::channel(16);
    send.send(pb::AgentToServer {
        msg: Some(agent_to_server::Msg::Connect(pb::Connect {
            host_id: h.host.clone(),
            protocol_version: capyctl_protocol::PROTOCOL_VERSION.into(),
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    assert!(AgentControlClient::new(channel)
        .session(ReceiverStream::new(recv))
        .await
        .is_err());
    // The recovered session was not displaced by that attempt.
    assert!(h.online());

    let (hosts, kinds) = {
        let store = h.state.lock().unwrap();
        let hosts = store.store().enrolled_hosts().unwrap();
        let events = store.store().events_after(None, 1000).unwrap();
        let kinds: Vec<String> = events.events.iter().map(|e| e.kind.clone()).collect();
        (hosts, kinds)
    };
    assert_eq!(hosts.len(), 1, "no second host record");
    assert!(!hosts[0].revoked);
    assert_eq!(
        kinds
            .iter()
            .filter(|k| *k == "host_recovery_invited")
            .count(),
        1
    );
    assert_eq!(kinds.iter().filter(|k| *k == "host_recovered").count(), 1);
    stop.send(true).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(15), agent).await;
    h.finish().await;
}

/// A control endpoint that answers every session with `answer`, to prove what
/// the agent does with refusals that are not the controller's own.
struct Refuser {
    answer: Arc<Mutex<tonic::Status>>,
    sessions: Arc<AtomicUsize>,
}
#[tonic::async_trait]
impl pb::agent_control_server::AgentControl for Refuser {
    type SessionStream = ReceiverStream<Result<pb::ServerToAgent, tonic::Status>>;
    async fn session(
        &self,
        _: tonic::Request<tonic::Streaming<pb::AgentToServer>>,
    ) -> Result<tonic::Response<Self::SessionStream>, tonic::Status> {
        self.sessions.fetch_add(1, Ordering::SeqCst);
        Err(self.answer.lock().unwrap().clone())
    }
}

/// A host enrolled with a real controller CA whose control address is a
/// `Refuser` presenting a server certificate from `server_ca` (the real CA,
/// or an impostor's). Returns the refuser's answer and session count and the
/// running agent.
struct Refused {
    answer: Arc<Mutex<tonic::Status>>,
    sessions: Arc<AtomicUsize>,
    stop: tokio::sync::watch::Sender<bool>,
    agent: tokio::task::JoinHandle<Result<(), capyctl_agent::session::HostRevoked>>,
    server: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
    _dirs: Vec<tempfile::TempDir>,
}
impl Refused {
    async fn start(impostor: bool, answer: tonic::Status) -> Self {
        let state_dir = directory();
        let state = Arc::new(Mutex::new(
            OwnedCoordinatorState::open(state_dir.path()).unwrap(),
        ));
        let ca = CertificateAuthority::generate(now()).unwrap();
        let ca_pem = ca.certificate_pem().to_owned();
        let key = HostKey::generate().unwrap();
        let cert = if impostor {
            CertificateAuthority::generate(now())
                .unwrap()
                .issue_server("localhost", &key, now())
                .unwrap()
        } else {
            ca.issue_server("localhost", &key, now()).unwrap()
        };
        let authority = Arc::new(EnrollmentAuthority::new(state, ca));
        let answer = Arc::new(Mutex::new(answer));
        let sessions = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!(
            "https://localhost:{}",
            listener.local_addr().unwrap().port()
        );
        let server = tokio::spawn(
            Server::builder()
                .tls_config(
                    ServerTlsConfig::new()
                        .identity(Identity::from_pem(cert.pem, key.private_key_pem()))
                        .client_ca_root(Certificate::from_pem(&ca_pem)),
                )
                .unwrap()
                .add_service(AgentControlServer::new(Refuser {
                    answer: answer.clone(),
                    sessions: sessions.clone(),
                }))
                .serve_with_incoming(TcpListenerStream::new(listener)),
        );
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
        let journal_dir = directory();
        let journal =
            HostJournal::open(journal_dir.path(), &identity.controller_id(), &host).unwrap();
        let (stop, shutdown) = tokio::sync::watch::channel(false);
        let agent = tokio::spawn(async move {
            capyctl_agent::session::run_session(
                &identity,
                journal,
                pb::ReportInventory::default(),
                shutdown,
            )
            .await
        });
        Self {
            answer,
            sessions,
            stop,
            agent,
            server,
            _dirs: vec![state_dir, storage_dir, journal_dir],
        }
    }
}

// T06 (SPEC §4.1, ADR 0016): only the controller's own, exact revocation
// answer stops the agent. Over an authentic mutual-TLS channel, a generic
// refusal and look-alike ("garbled") revocation messages keep the agent
// retrying with its backoff; once the
// same channel carries the exact answer, the agent returns `HostRevoked`.
#[tokio::test]
async fn only_the_controllers_exact_revocation_answer_stops_the_agent() {
    let r = Refused::start(
        false,
        tonic::Status::permission_denied("host session authorization failed"),
    )
    .await;
    let sessions = r.sessions.clone();
    eventually(|| sessions.load(Ordering::SeqCst) >= 2).await;
    assert!(
        !r.agent.is_finished(),
        "a generic refusal stopped the agent"
    );
    // The next attempt after the switch is answered with the look-alike, and
    // the one after it proves the agent retried. (The unit tests in
    // `capyctl_agent::session` cover every other look-alike without backoff.)
    let garbled =
        tonic::Status::permission_denied(format!("{} ", capyctl_protocol::HOST_REVOKED_REFUSAL));
    *r.answer.lock().unwrap() = garbled.clone();
    let seen = r.sessions.load(Ordering::SeqCst);
    let sessions = r.sessions.clone();
    eventually(|| sessions.load(Ordering::SeqCst) >= seen + 2).await;
    assert!(!r.agent.is_finished(), "{garbled:?} stopped the agent");
    *r.answer.lock().unwrap() = capyctl_protocol::host_revoked_refusal();
    let ended = tokio::time::timeout(Duration::from_secs(15), r.agent)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(ended, Err(capyctl_agent::session::HostRevoked)),
        "{ended:?}"
    );
    drop(r.stop);
    r.server.abort();
}

// T05 T06 (SPEC §4.1, ADR 0016): an endpoint that is not the controller (its
// certificate is not from the CA pinned at enrollment) cannot stop the agent
// by sending the exact revocation answer: the agent refuses the handshake,
// never sees the answer, and keeps retrying.
#[tokio::test]
async fn an_impostors_revocation_answer_never_reaches_the_agent() {
    let r = Refused::start(true, capyctl_protocol::host_revoked_refusal()).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!r.agent.is_finished(), "an impostor stopped the agent");
    assert_eq!(
        r.sessions.load(Ordering::SeqCst),
        0,
        "an impostor was answered a session"
    );
    r.stop.send(true).unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(15), r.agent)
        .await
        .unwrap()
        .unwrap();
    assert!(ended.is_ok(), "{ended:?}");
    r.server.abort();
}

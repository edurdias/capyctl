//! ADR 0018 §3: live re-publication of a host's runtime profiles over a real
//! mTLS control session. CPU-only transport tests: nothing here qualifies a
//! native engine.

use mllm_agent::{
    enrollment::{JoinInvitation, PendingEnrollment},
    identity::{CertificateAuthority, HostKey},
    identity_storage::IdentityDirectory,
};
use mllm_controller::{
    agent_sessions::AgentSessions, enrollment::EnrollmentAuthority,
    ownership::SharedCoordinatorState, OwnedCoordinatorState,
};
use mllm_protocol::{
    capabilities,
    pb::{
        self, agent_control_client::AgentControlClient, agent_control_server::AgentControlServer,
        agent_to_server, bootstrap_server::Bootstrap, server_to_agent,
    },
    version::BINARY_VERSION,
};
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
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

/// An approved host preparation with one SGLang runtime profile.
fn prepared_document() -> Value {
    let text = include_str!("../../mllm-config/tests/fixtures/effective-sglang-golden.json");
    let mut host = serde_json::from_str::<Value>(text).unwrap()["input"]["host"].clone();
    host["state_dir"] = json!("/home/operator/.local/state/mllm");
    host["identity_dir"] = json!("/home/operator/.local/state/mllm/identity");
    host
}

/// The inventory a prepared host reports.
fn inventory(host: &str) -> pb::ReportInventory {
    let document = prepared_document();
    let config = mllm_config::remote_roles::HostConfig::parse(&document.to_string()).unwrap();
    let now = mllm_protocol::now_unix_ms();
    pb::ReportInventory {
        domains: vec![pb::DomainObservation {
            domain_id: "unified".into(),
            kind: "system".into(),
            observed_bytes: 100 << 30,
            observed_at_unix: now / 1000,
            capacity_bytes: 128 << 30,
            available_bytes: 100 << 30,
            observed_at_unix_ms: now,
            ..Default::default()
        }],
        profiles: config
            .profiles
            .iter()
            .map(|(name, profile)| pb::RuntimeProfileStatus {
                name: name.clone(),
                build_fingerprint: profile["build_fingerprint"]
                    .as_str()
                    .unwrap_or("unknown")
                    .into(),
                eligibility: "unknown".into(),
                ..Default::default()
            })
            .collect(),
        approved_host_config_json: config.document.to_string(),
        host_boot_id: "boot".into(),
        policy_fingerprint: mllm_config::remote_resources::policy_fingerprint(&config.document),
        envelope: Some(pb::Envelope {
            host_id: host.into(),
            protocol_version: mllm_protocol::PROTOCOL_VERSION.into(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

struct Harness {
    state: SharedCoordinatorState,
    authority: Arc<EnrollmentAuthority>,
    sessions: Arc<AgentSessions>,
    identity: PendingEnrollment,
    host: String,
    server: tokio::task::JoinHandle<()>,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

async fn enrolled() -> Harness {
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
    let tls = ServerTlsConfig::new()
        .identity(Identity::from_pem(cert.pem, key.private_key_pem()))
        .client_ca_root(Certificate::from_pem(&ca_pem));
    let service = AgentControlServer::from_arc(sessions.clone());
    let server = tokio::spawn(async move {
        let _ = Server::builder()
            .tls_config(tls)
            .unwrap()
            .add_service(service)
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await;
    });
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
    Harness {
        state,
        authority,
        sessions,
        identity,
        host,
        server,
        _dirs: (state_dir, storage_dir),
    }
}

type Opened = (
    tokio::sync::mpsc::Sender<pb::AgentToServer>,
    tonic::Streaming<pb::ServerToAgent>,
);

impl Harness {
    /// Open a control session as a host declaring `version` and
    /// `capabilities`. `Err` is the controller's refusal of the session.
    async fn open(
        &self,
        version: &str,
        capabilities: Vec<String>,
    ) -> Result<Opened, Box<tonic::Status>> {
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
                binary_version: version.into(),
                capabilities,
                ..Default::default()
            })),
        })
        .await
        .unwrap();
        let stream = AgentControlClient::new(channel)
            .session(ReceiverStream::new(recv))
            .await
            .map_err(Box::new)?
            .into_inner();
        Ok((send, stream))
    }

    /// As `open`, then publish a prepared inventory and reconcile.
    async fn reconciled(&self, version: &str, capabilities: Vec<String>) -> Opened {
        let (send, mut stream) = self
            .open(version, capabilities)
            .await
            .expect("session accepted");
        send.send(pb::AgentToServer {
            msg: Some(agent_to_server::Msg::ReportInventory(inventory(&self.host))),
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
        match stream.message().await.unwrap().unwrap().msg {
            Some(server_to_agent::Msg::SessionReady(_)) => {}
            other => panic!("expected SessionReady, got {other:?}"),
        }
        (send, stream)
    }
}

fn all() -> Vec<String> {
    capabilities::agent_capabilities()
}

/// The prepared inventory with one more runtime profile, `vllm`.
fn with_vllm(host: &str) -> pb::ReportInventory {
    let mut inventory = inventory(host);
    let mut document: Value = serde_json::from_str(&inventory.approved_host_config_json).unwrap();
    document["runtime_profiles"]["vllm"] =
        mllm_config::registration::profile_document(&mllm_config::registration::ProfileSpec {
            engine: mllm_config::engine_policy::Engine::Vllm,
            executable: "/home/operator/venv/bin/vllm".into(),
            build_fingerprint: "0.29.0".into(),
            deep_park: true,
            installation_drift: mllm_config::effective::InstallationDrift::Warn,
            args: vec![],
        });
    inventory.approved_host_config_json = document.to_string();
    inventory.policy_fingerprint = mllm_config::remote_resources::policy_fingerprint(&document);
    inventory.profiles.push(pb::RuntimeProfileStatus {
        name: "vllm".into(),
        build_fingerprint: "0.29.0".into(),
        eligibility: "unknown".into(),
        ..Default::default()
    });
    inventory
}

fn publish(request_id: &str, inventory: pb::ReportInventory) -> pb::AgentToServer {
    pb::AgentToServer {
        msg: Some(agent_to_server::Msg::PublishProfiles(pb::PublishProfiles {
            request_id: request_id.into(),
            inventory: Some(inventory),
        })),
    }
}

async fn verdict(stream: &mut tonic::Streaming<pb::ServerToAgent>) -> pb::ProfilesPublished {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), stream.message())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .msg
        {
            Some(server_to_agent::Msg::ProfilesPublished(v)) => return v,
            Some(server_to_agent::Msg::Heartbeat(_)) => continue,
            other => panic!("expected ProfilesPublished, got {other:?}"),
        }
    }
}

// T34: the server tells a host it can take live profile updates.
#[tokio::test]
async fn session_ready_advertises_live_profile_update() {
    let h = enrolled().await;
    let (send, mut stream) = h.open(BINARY_VERSION, all()).await.unwrap();
    send.send(pb::AgentToServer {
        msg: Some(agent_to_server::Msg::ReportInventory(inventory(&h.host))),
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
    match stream.message().await.unwrap().unwrap().msg {
        Some(server_to_agent::Msg::SessionReady(ready)) => {
            assert!(ready
                .capabilities
                .contains(&capabilities::LIVE_PROFILE_UPDATE.to_owned()));
        }
        other => panic!("{other:?}"),
    }
    h.server.abort();
}

// T07 T34: an accepted re-publication replaces the approved snapshot at once,
// the session view lists the new profile, and the session carries on.
#[tokio::test]
async fn an_accepted_republication_replaces_the_snapshot_live() {
    let h = enrolled().await;
    let (send, mut stream) = h.reconciled(BINARY_VERSION, all()).await;
    let next = with_vllm(&h.host);
    send.send(publish("01K00000000000000000000010", next.clone()))
        .await
        .unwrap();
    let answer = verdict(&mut stream).await;
    assert_eq!(answer.request_id, "01K00000000000000000000010");
    assert!(answer.accepted, "{}", answer.reason);
    let approved = h
        .state
        .lock()
        .unwrap()
        .store()
        .host_publication(&h.host)
        .unwrap()
        .unwrap();
    assert_eq!(approved.fingerprint, next.policy_fingerprint);
    let view = h.sessions.inspect(&h.host).unwrap();
    let names: Vec<&str> = view.profiles.iter().map(|p| p.name.as_str()).collect();
    assert!(names.contains(&"vllm"), "{names:?}");
    assert!(view.eligible);
    // The next refresh carries the new document and is accepted.
    send.send(pb::AgentToServer {
        msg: Some(agent_to_server::Msg::ReportInventory(next)),
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(h.sessions.current_session(&h.host).is_some());
    h.server.abort();
}

// T03 T07: a refused re-publication keeps the previous snapshot and the
// session, and says why.
#[tokio::test]
async fn a_refused_republication_keeps_the_snapshot_and_the_session() {
    let h = enrolled().await;
    let (send, mut stream) = h.reconciled(BINARY_VERSION, all()).await;
    let before = h
        .state
        .lock()
        .unwrap()
        .store()
        .host_publication(&h.host)
        .unwrap()
        .unwrap();
    let mut edited = with_vllm(&h.host);
    let mut document: Value = serde_json::from_str(&edited.approved_host_config_json).unwrap();
    // Within the 250ms..5s bound, so the document stays valid.
    document["load_report_interval"] = json!("2s");
    edited.approved_host_config_json = document.to_string();
    edited.policy_fingerprint = mllm_config::remote_resources::policy_fingerprint(&document);
    send.send(publish("r1", edited)).await.unwrap();
    let answer = verdict(&mut stream).await;
    assert!(!answer.accepted);
    assert!(
        answer.reason.contains("only runtime profiles"),
        "{}",
        answer.reason
    );
    let after = h
        .state
        .lock()
        .unwrap()
        .store()
        .host_publication(&h.host)
        .unwrap()
        .unwrap();
    assert_eq!(after.fingerprint, before.fingerprint);
    assert!(
        h.sessions.current_session(&h.host).is_some(),
        "the session stays"
    );
    h.server.abort();
}

// T34: a host that did not declare the capability may not send it.
#[tokio::test]
async fn an_undeclared_republication_ends_the_session() {
    let h = enrolled().await;
    let mut declared = all();
    declared.retain(|c| c != capabilities::LIVE_PROFILE_UPDATE);
    let (send, mut stream) = h.reconciled(BINARY_VERSION, declared).await;
    send.send(publish("r2", with_vllm(&h.host))).await.unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match stream.message().await {
                Err(status) => return status.code(),
                Ok(None) => return tonic::Code::Ok,
                Ok(Some(_)) => continue,
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(ended, tonic::Code::PermissionDenied);
    h.server.abort();
}

// T07 T33 (ADR 0018 §3, §4): the controller's validation of a live
// re-publication, called directly. An accepted one replaces the approved
// document; each refusal says why and keeps the previous document.
#[tokio::test]
async fn the_controller_republish_accepts_and_refuses_with_a_reason() {
    let h = enrolled().await;
    let startup = inventory(&h.host);
    h.authority.publish_inventory(&h.host, &startup).unwrap();
    let approved = |h: &Harness| {
        h.state
            .lock()
            .unwrap()
            .store()
            .host_publication(&h.host)
            .unwrap()
            .unwrap()
            .fingerprint
    };
    // A document that does not match its fingerprint.
    let mut forged = with_vllm(&h.host);
    forged.policy_fingerprint = startup.policy_fingerprint.clone();
    let reason = h
        .authority
        .republish_inventory(&h.host, &forged, &startup)
        .unwrap_err();
    assert!(reason.contains("fingerprint"), "{reason}");
    assert_eq!(approved(&h), startup.policy_fingerprint);
    // An added profile that the resolution rules refuse.
    let mut invalid = with_vllm(&h.host);
    let mut document: Value = serde_json::from_str(&invalid.approved_host_config_json).unwrap();
    document["runtime_profiles"]["vllm"]["executable"] = json!("vllm");
    invalid.approved_host_config_json = document.to_string();
    invalid.policy_fingerprint = mllm_config::remote_resources::policy_fingerprint(&document);
    let reason = h
        .authority
        .republish_inventory(&h.host, &invalid, &startup)
        .unwrap_err();
    assert!(reason.starts_with("profile vllm: "), "{reason}");
    assert_eq!(approved(&h), startup.policy_fingerprint);
    // Accepted: the approved document is the new one.
    let next = with_vllm(&h.host);
    h.authority
        .republish_inventory(&h.host, &next, &startup)
        .unwrap();
    assert_eq!(approved(&h), next.policy_fingerprint);
    // Based on a publication that is no longer the approved one: refused.
    let reason = h
        .authority
        .republish_inventory(&h.host, &next, &startup)
        .unwrap_err();
    assert!(reason.contains("changed since"), "{reason}");
    // Dropping a profile the server did not retire: refused, then accepted
    // once the retirement is confirmed.
    let reason = h
        .authority
        .republish_inventory(&h.host, &startup, &next)
        .unwrap_err();
    assert!(reason.contains("profile vllm"), "{reason}");
    assert_eq!(approved(&h), next.policy_fingerprint);
    assert!(matches!(
        h.state
            .lock()
            .unwrap()
            .store()
            .begin_profile_retirement(&h.host, "vllm", "k", 1, 10, false)
            .unwrap(),
        mllm_store::profile_retirement::RetirementStart::Clear
    ));
    h.authority
        .republish_inventory(&h.host, &startup, &next)
        .unwrap();
    assert_eq!(approved(&h), startup.policy_fingerprint);
    h.server.abort();
}

use mllm_controller::profile_retirement::{ProfileRetirements, RetirementStep};

/// A scripted retirement service: `begin` answers `first`; `poll` answers
/// `None` `waits` times, then `last`.
struct Scripted {
    first: RetirementStep,
    waits: std::sync::atomic::AtomicUsize,
    last: RetirementStep,
    seen: Mutex<Vec<(String, String, String, bool)>>,
}

impl ProfileRetirements for Scripted {
    fn begin(&self, host: &str, profile: &str, key: &str, drain: bool) -> RetirementStep {
        self.seen
            .lock()
            .unwrap()
            .push((host.into(), profile.into(), key.into(), drain));
        self.first.clone()
    }
    fn poll(&self, _: &str, _: &str, _: &str) -> Option<RetirementStep> {
        if self.waits.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            self.waits.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            None
        } else {
            Some(self.last.clone())
        }
    }
}

fn retire(request_id: &str, profile: &str, drain: bool) -> pb::AgentToServer {
    pb::AgentToServer {
        msg: Some(agent_to_server::Msg::RetireProfile(pb::RetireProfile {
            request_id: request_id.into(),
            profile: profile.into(),
            drain,
        })),
    }
}

async fn retirement(stream: &mut tonic::Streaming<pb::ServerToAgent>) -> pb::ProfileRetirement {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), stream.message())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .msg
        {
            Some(server_to_agent::Msg::ProfileRetirement(r)) => return r,
            Some(server_to_agent::Msg::Heartbeat(_)) => continue,
            other => panic!("expected ProfileRetirement, got {other:?}"),
        }
    }
}

// T16 T32: in use without drain is refused with the list; with drain the
// host hears `draining`, then `confirmed` only when the service confirms.
#[tokio::test]
async fn a_retirement_answers_in_use_or_drains_then_confirms() {
    let h = enrolled().await;
    let service = Arc::new(Scripted {
        first: RetirementStep::InUse(vec!["q14".into()]),
        waits: 0.into(),
        last: RetirementStep::Confirmed,
        seen: Mutex::new(vec![]),
    });
    h.sessions.with_profile_retirements(service.clone());
    let (send, mut stream) = h.reconciled(BINARY_VERSION, all()).await;
    send.send(retire("req-1", "local", false)).await.unwrap();
    let answer = retirement(&mut stream).await;
    assert_eq!(
        (answer.request_id.as_str(), answer.outcome.as_str()),
        ("req-1", "in_use")
    );
    assert_eq!(answer.deployments, vec!["q14".to_string()]);
    assert_eq!(
        service.seen.lock().unwrap()[0],
        (
            h.host.clone(),
            "local".into(),
            format!("{}:req-1", h.host),
            false
        )
    );

    let draining = Arc::new(Scripted {
        first: RetirementStep::Draining(vec!["q14".into()]),
        waits: 2.into(),
        last: RetirementStep::Confirmed,
        seen: Mutex::new(vec![]),
    });
    h.sessions.with_profile_retirements(draining);
    send.send(retire("req-2", "local", true)).await.unwrap();
    assert_eq!(retirement(&mut stream).await.outcome, "draining");
    let last = retirement(&mut stream).await;
    assert_eq!(
        (last.request_id.as_str(), last.outcome.as_str()),
        ("req-2", "confirmed")
    );
    h.server.abort();
}

// T32: an unsettled drain answers `holding`, naming what is unsettled.
#[tokio::test]
async fn an_unsettled_drain_answers_holding() {
    let h = enrolled().await;
    h.sessions.with_profile_retirements(Arc::new(Scripted {
        first: RetirementStep::Draining(vec!["q14".into()]),
        waits: 0.into(),
        last: RetirementStep::Holding(vec!["q14".into()]),
        seen: Mutex::new(vec![]),
    }));
    let (send, mut stream) = h.reconciled(BINARY_VERSION, all()).await;
    send.send(retire("req-3", "local", true)).await.unwrap();
    assert_eq!(retirement(&mut stream).await.outcome, "draining");
    let last = retirement(&mut stream).await;
    assert_eq!(last.outcome, "holding");
    assert_eq!(last.deployments, vec!["q14".to_string()]);
    h.server.abort();
}

// T37: an invalid profile name, or a server with no retirement service, is
// refused without ending the session.
#[tokio::test]
async fn a_malformed_or_unserved_retirement_is_refused() {
    let h = enrolled().await;
    let (send, mut stream) = h.reconciled(BINARY_VERSION, all()).await;
    send.send(retire("req-4", "local", false)).await.unwrap();
    let answer = retirement(&mut stream).await;
    assert_eq!(answer.outcome, "refused");
    assert!(!answer.reason.is_empty());
    send.send(retire("req-5", "Not A Name", false))
        .await
        .unwrap();
    assert_eq!(retirement(&mut stream).await.outcome, "refused");
    assert!(h.sessions.current_session(&h.host).is_some());
    h.server.abort();
}

// T37: with a retirement service installed, an invalid profile name is
// refused before the service is asked, and the session stays up.
#[tokio::test]
async fn an_invalid_profile_name_is_refused_with_a_service_installed() {
    let h = enrolled().await;
    let service = Arc::new(Scripted {
        first: RetirementStep::Confirmed,
        waits: 0.into(),
        last: RetirementStep::Confirmed,
        seen: Mutex::new(vec![]),
    });
    h.sessions.with_profile_retirements(service.clone());
    let (send, mut stream) = h.reconciled(BINARY_VERSION, all()).await;
    send.send(retire("req-6", "Not A Name", true))
        .await
        .unwrap();
    let answer = retirement(&mut stream).await;
    assert_eq!(
        (answer.request_id.as_str(), answer.outcome.as_str()),
        ("req-6", "refused")
    );
    assert!(!answer.reason.is_empty());
    assert!(service.seen.lock().unwrap().is_empty());
    assert!(h.sessions.current_session(&h.host).is_some());
    h.server.abort();
}

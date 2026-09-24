//! U5-G4 (W12): host eligibility is derived from evidence the controller
//! accepted, never declared (SPEC §4.2) and never a qualification claim
//! (ADR 0011). A host is eligible while its authenticated session is live and
//! reconciled, its approved preparation was published, and at least one of its
//! reported runtime profiles resolves in that preparation.

use mllm_agent::{
    enrollment::{JoinInvitation, PendingEnrollment},
    identity::{CertificateAuthority, HostKey},
    identity_storage::IdentityDirectory,
    journal::HostJournal,
};
use mllm_controller::{
    agent_sessions::AgentSessions, enrollment::EnrollmentAuthority, host_publication,
    OwnedCoordinatorState,
};
use mllm_protocol::pb::{
    self, agent_control_server::AgentControlServer, bootstrap_server::Bootstrap,
};
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_stream::wrappers::TcpListenerStream;
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

/// An approved host preparation with one SGLang runtime profile.
fn prepared_document() -> Value {
    let text = include_str!("../../mllm-config/tests/fixtures/effective-sglang-golden.json");
    let mut host = serde_json::from_str::<Value>(text).unwrap()["input"]["host"].clone();
    host["state_dir"] = json!("/home/operator/.local/state/mllm");
    host["identity_dir"] = json!("/home/operator/.local/state/mllm/identity");
    host
}

/// The inventory a host reports for `document`, as `start host` builds it.
fn inventory(document: &Value) -> pb::ReportInventory {
    let config = mllm_config::remote_roles::HostConfig::parse(&document.to_string()).unwrap();
    let now = mllm_protocol::now_unix_ms();
    pb::ReportInventory {
        domains: vec![pb::DomainObservation {
            residents: vec![],
            domain_id: "unified".into(),
            kind: "system".into(),
            observed_bytes: 100 << 30,
            observed_at_unix: now / 1000,
            capacity_bytes: 128 << 30,
            available_bytes: 100 << 30,
            observed_at_unix_ms: now,
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
                reason: String::new(),
                ..Default::default()
            })
            .collect(),
        approved_host_config_json: config.document.to_string(),
        host_boot_id: "boot".into(),
        policy_fingerprint: mllm_config::remote_resources::policy_fingerprint(&config.document),
        ..Default::default()
    }
}

// T07 T22
#[test]
fn eligibility_derives_from_the_approved_preparation_not_from_claims() {
    let prepared = inventory(&prepared_document());
    assert!(!prepared.profiles.is_empty());
    assert!(host_publication::eligible(&prepared));

    // Connected but unprepared: no approved preparation, no profile authority.
    let unprepared = pb::ReportInventory {
        approved_host_config_json: String::new(),
        profiles: vec![],
        ..prepared.clone()
    };
    assert!(!host_publication::eligible(&unprepared));

    // Prepared, but with no runtime profile: nothing can be placed there.
    let mut empty = prepared_document();
    empty["runtime_profiles"] = json!({});
    assert!(!host_publication::eligible(&inventory(&empty)));

    // A reported profile the approved document does not name does not resolve.
    let mut unknown = prepared.clone();
    for profile in &mut unknown.profiles {
        profile.name = format!("{}-unapproved", profile.name);
    }
    assert!(!host_publication::eligible(&unknown));

    // A build fingerprint other than the approved one does not resolve.
    let mut drifted = prepared.clone();
    for profile in &mut drifted.profiles {
        profile.build_fingerprint = "sha256:other-build".into();
    }
    assert!(!host_publication::eligible(&drifted));

    // Negative host evidence always counts against a profile.
    for refused in ["disabled", "unsupported"] {
        let mut host = prepared.clone();
        for profile in &mut host.profiles {
            profile.eligibility = refused.into();
        }
        assert!(!host_publication::eligible(&host), "{refused}");
    }

    // ADR 0011: a `qualified` claim is not an input. It neither grants nor
    // withholds anything that `unknown` would not.
    let mut claimed = prepared.clone();
    for profile in &mut claimed.profiles {
        profile.eligibility = "qualified".into();
    }
    assert!(host_publication::eligible(&claimed));
    let mut claimed_unapproved = unknown.clone();
    for profile in &mut claimed_unapproved.profiles {
        profile.eligibility = "qualified".into();
    }
    assert!(!host_publication::eligible(&claimed_unapproved));
}

// T05 T07 T33: the session view is eligible only while the prepared host's
// authenticated session is live and reconciled, and loses it when it ends.
#[tokio::test]
async fn a_prepared_host_is_eligible_only_while_its_reconciled_session_lives() {
    let state_dir = directory();
    let state = Arc::new(Mutex::new(
        OwnedCoordinatorState::open(state_dir.path()).unwrap(),
    ));
    let ca = CertificateAuthority::generate(now()).unwrap();
    let ca_pem = ca.certificate_pem().to_owned();
    let key = HostKey::generate().unwrap();
    let cert = ca.issue_server("localhost", &key, now()).unwrap();
    let authority = Arc::new(EnrollmentAuthority::new(state, ca));
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
        control_address: address,
        server_ca: ca_pem,
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
    let host = identity
        .accept_certificate(&storage, certificate, now())
        .unwrap();
    let journal_dir = directory();
    let journal = HostJournal::open(journal_dir.path(), &identity.controller_id(), &host).unwrap();
    let mut report = inventory(&prepared_document());
    report.envelope = Some(pb::Envelope {
        host_id: host.clone(),
        protocol_version: mllm_protocol::PROTOCOL_VERSION.into(),
        ..Default::default()
    });
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        mllm_agent::session::run_session_with_execution(&identity, journal, report, shutdown, None)
            .await
    });
    eventually(|| sessions.inspect(&host).is_some_and(|s| s.reconciled)).await;
    let view = sessions.inspect(&host).unwrap();
    assert!(view.online);
    assert!(view.eligible, "a reconciled, prepared host is eligible");
    assert!(view.profiles.iter().all(|p| p.eligibility == "unknown"));

    // Revocation ends the session; eligibility goes with it.
    authority.revoke(&host).unwrap();
    eventually(|| sessions.inspect(&host).is_some_and(|s| !s.online)).await;
    assert!(!sessions.inspect(&host).unwrap().eligible);
    stop.send(true).unwrap();
    let _ = task.await;
    server.abort();
}

/// Owner decision 4 (2026-09-22): a host takes no new placements while a drain
/// of it has a Stop that has not settled. The durable marker survives the host
/// reconnecting; the host is a candidate again once every Stop settled.
// T10 T33
#[tokio::test]
async fn a_host_with_a_pending_drain_is_not_a_placement_candidate() {
    use mllm_controller::coordinator::ServiceObservation;
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
        control_address: address,
        server_ca: ca_pem,
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
    let host = identity
        .accept_certificate(&storage, certificate, now())
        .unwrap();
    let journal_dir = directory();
    let journal = HostJournal::open(journal_dir.path(), &identity.controller_id(), &host).unwrap();
    let mut report = inventory(&prepared_document());
    report.envelope = Some(pb::Envelope {
        host_id: host.clone(),
        protocol_version: mllm_protocol::PROTOCOL_VERSION.into(),
        ..Default::default()
    });
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        mllm_agent::session::run_session_with_execution(&identity, journal, report, shutdown, None)
            .await
    });
    eventually(|| sessions.inspect(&host).is_some_and(|s| s.reconciled)).await;
    assert!(sessions.eligible_hosts().unwrap().contains(&host));
    assert!(sessions.online_hosts().unwrap().contains(&host));

    // T10 T33 (router review item 14): the drain intent alone, before any
    // Stop exists, takes the host out of placement.
    state
        .lock()
        .unwrap()
        .store()
        .begin_host_drain(&host, "drain", 1)
        .unwrap();
    assert!(
        !sessions.eligible_hosts().unwrap().contains(&host),
        "a host with an open drain intent is not a placement candidate"
    );
    assert!(sessions.inspect(&host).unwrap().drain_pending);

    // A drain's Stop is accepted and still open.
    let sql = rusqlite::Connection::open(state_dir.path().join("srv.sqlite3")).unwrap();
    sql.execute(
        "INSERT INTO operations(id,kind,state) VALUES('drain-stop','ordinary_cleanup','running')",
        [],
    )
    .unwrap();
    state
        .lock()
        .unwrap()
        .store()
        .record_host_drain(&host, &["drain-stop".to_string()], 1)
        .unwrap();
    // The drain issued and marked every Stop: its intent completes, and the
    // marker holds the host from here.
    state
        .lock()
        .unwrap()
        .store()
        .complete_host_drain(&host, "drain", 2)
        .unwrap();
    assert!(state
        .lock()
        .unwrap()
        .store()
        .host_drain_pending(&host)
        .unwrap());
    assert!(
        !sessions.eligible_hosts().unwrap().contains(&host),
        "a draining host is not a placement candidate"
    );
    let view = sessions.inspect(&host).unwrap();
    assert!(view.online && view.reconciled);
    assert!(view.drain_pending && !view.eligible);
    assert!(
        sessions.online_hosts().unwrap().contains(&host),
        "its cleanups can still be delivered"
    );

    // Every Stop settled: the marker clears and the host is a candidate again.
    sql.execute(
        "UPDATE operations SET state='succeeded' WHERE id='drain-stop'",
        [],
    )
    .unwrap();
    assert!(!state
        .lock()
        .unwrap()
        .store()
        .host_drain_pending(&host)
        .unwrap());
    assert!(sessions.eligible_hosts().unwrap().contains(&host));
    let view = sessions.inspect(&host).unwrap();
    assert!(!view.drain_pending && view.eligible);
    stop.send(true).unwrap();
    let _ = task.await;
    server.abort();
}

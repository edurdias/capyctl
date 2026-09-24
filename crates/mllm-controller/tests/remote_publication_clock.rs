//! U5 recovery live run, server side: an enrolled host's clock may lead the
//! controller's by a few milliseconds. Publication tolerates a bounded lead
//! (SPEC §7: freshness is judged on the controller clock), so a sample that
//! arrives 12 ms "in the future" must publish instead of ending every host
//! session with `host inventory publication refused`. A lead beyond the bound
//! is still refused. Nothing here runs an engine.

use mllm_agent::{
    enrollment::{JoinInvitation, PendingEnrollment},
    identity::CertificateAuthority,
    identity_storage::IdentityDirectory,
};
use mllm_controller::{enrollment::EnrollmentAuthority, OwnedCoordinatorState};
use mllm_protocol::pb::{self, bootstrap_server::Bootstrap};
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
};

fn directory() -> tempfile::TempDir {
    let d = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    d
}

fn host_document() -> Value {
    let text = include_str!("../../mllm-config/tests/fixtures/effective-sglang-golden.json");
    let mut host = serde_json::from_str::<Value>(text).unwrap()["input"]["host"].clone();
    host["state_dir"] = json!("/home/operator/.local/state/mllm");
    host["identity_dir"] = json!("/home/operator/.local/state/mllm/identity");
    host["ingress"] = json!({
        "transport": "trusted_private_link",
        "address": "http://100.64.0.2:9443",
        "bind": "100.64.0.2:9443"
    });
    host
}

/// Enroll one host and return the controller state, its host ID, and a
/// publication whose single domain sample is `lead_ms` ahead of the controller.
async fn enrolled(
    lead_ms: i64,
) -> (
    tempfile::TempDir,
    tempfile::TempDir,
    Arc<Mutex<OwnedCoordinatorState>>,
    String,
    pb::ReportInventory,
) {
    let now = mllm_protocol::now_unix_ms();
    let state_dir = directory();
    let state = Arc::new(Mutex::new(
        OwnedCoordinatorState::open(state_dir.path()).unwrap(),
    ));
    let ca = CertificateAuthority::generate(now / 1000).unwrap();
    let ca_pem = ca.certificate_pem().to_owned();
    let authority = EnrollmentAuthority::new(state.clone(), ca);
    let invite = authority.invite("spark", 300, now / 1000).unwrap();
    let invitation = JoinInvitation {
        version: 1,
        server_address: "https://localhost:7444".into(),
        control_address: "https://localhost:7445".into(),
        server_ca: ca_pem,
        invitation_id: invite.id,
        invitation_secret: invite.secret,
        host_name: invite.host_name,
        expires_unix: invite.expires_unix,
    };
    let storage_dir = directory();
    let storage = IdentityDirectory::open(storage_dir.path()).unwrap();
    let mut identity = PendingEnrollment::prepare(&storage, &invitation).unwrap();
    let request = identity.request(&invitation).unwrap();
    let certificate = Bootstrap::enroll(&authority, tonic::Request::new(request))
        .await
        .unwrap()
        .into_inner();
    let host = identity
        .accept_certificate(&storage, certificate, now / 1000)
        .unwrap();
    let config =
        mllm_config::remote_roles::HostConfig::parse(&host_document().to_string()).unwrap();
    let sampled = mllm_protocol::now_unix_ms() + lead_ms;
    let inventory = pb::ReportInventory {
        domains: vec![pb::DomainObservation {
            residents: vec![],
            domain_id: "unified".into(),
            kind: "system".into(),
            observed_bytes: 100 << 30,
            observed_at_unix: sampled / 1000,
            capacity_bytes: 128 << 30,
            available_bytes: 100 << 30,
            observed_at_unix_ms: sampled,
        }],
        profiles: config
            .profiles
            .iter()
            .map(|(name, profile)| pb::RuntimeProfileStatus {
                name: name.clone(),
                build_fingerprint: profile["build_fingerprint"].as_str().unwrap().into(),
                eligibility: "unknown".into(),
                reason: String::new(),
                ..Default::default()
            })
            .collect(),
        approved_host_config_json: config.document.to_string(),
        host_boot_id: "boot".into(),
        policy_fingerprint: mllm_config::remote_resources::policy_fingerprint(&config.document),
        ..Default::default()
    };
    (state_dir, storage_dir, state, host, inventory)
}

// T29 T07
#[tokio::test]
async fn a_host_clock_leading_within_the_bound_still_publishes() {
    // The live U5 recovery run measured a 12 ms lead from host-a.
    let (_state_dir, _storage_dir, state, host, inventory) = enrolled(150).await;
    mllm_controller::host_publication::publish(&state, &host, &inventory)
        .expect("a sample within the tolerated clock lead publishes");
    assert!(state
        .lock()
        .unwrap()
        .store()
        .host_publication(&host)
        .unwrap()
        .is_some());
}

// T29 T07
#[tokio::test]
async fn a_host_clock_leading_beyond_the_bound_is_refused() {
    let (_state_dir, _storage_dir, state, host, inventory) = enrolled(5_000).await;
    assert!(mllm_controller::host_publication::publish(&state, &host, &inventory).is_err());
    assert!(state
        .lock()
        .unwrap()
        .store()
        .host_publication(&host)
        .unwrap()
        .is_none());
}

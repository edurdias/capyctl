//! G01, server side: an enrolled host's approved preparation may carry vLLM
//! runtime profiles beside SGLang ones (SPEC §§4.2, 7). Publication is
//! engine-neutral, and the stored snapshot is exactly what the remote binding
//! later resolves a vLLM deployment against. Nothing here runs an engine.

use capyctl_agent::{
    enrollment::{JoinInvitation, PendingEnrollment},
    identity::CertificateAuthority,
    identity_storage::IdentityDirectory,
};
use capyctl_controller::{enrollment::EnrollmentAuthority, OwnedCoordinatorState};
use capyctl_protocol::pb::{self, bootstrap_server::Bootstrap};
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

fn input(name: &str) -> Value {
    let text = match name {
        "vllm" => include_str!("../../capyctl-config/tests/fixtures/effective-vllm-golden.json"),
        _ => include_str!("../../capyctl-config/tests/fixtures/effective-sglang-golden.json"),
    };
    serde_json::from_str::<Value>(text).unwrap()["input"].clone()
}

/// The host role document: SGLang and vLLM profiles approved side by side.
fn mixed_host_document() -> Value {
    let mut host = input("sglang")["host"].clone();
    host["runtime_profiles"]["qwen-vllm"] =
        input("vllm")["host"]["runtime_profiles"]["local"].clone();
    host["state_dir"] = json!("/home/operator/.local/state/capyctl");
    host["identity_dir"] = json!("/home/operator/.local/state/capyctl/identity");
    host["ingress"] = json!({
        "transport": "trusted_private_link",
        "address": "http://100.64.0.2:9443",
        "bind": "100.64.0.2:9443"
    });
    host
}

// T07 T22
#[tokio::test]
async fn an_enrolled_host_publishes_vllm_profiles_beside_sglang() {
    let now = capyctl_protocol::now_unix_ms();
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
        recover_host_id: None,
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

    let document = mixed_host_document();
    let config = capyctl_config::remote_roles::HostConfig::parse(&document.to_string()).unwrap();
    let inventory = pb::ReportInventory {
        domains: vec![pb::DomainObservation {
            device_id: String::new(),
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
                build_fingerprint: profile["build_fingerprint"].as_str().unwrap().into(),
                eligibility: "unknown".into(),
                reason: String::new(),
                ..Default::default()
            })
            .collect(),
        approved_host_config_json: config.document.to_string(),
        host_boot_id: "boot".into(),
        policy_fingerprint: capyctl_config::remote_resources::policy_fingerprint(&config.document),
        ..Default::default()
    };
    capyctl_controller::host_publication::publish(&state, &host, &inventory)
        .expect("a vLLM-bearing preparation publishes");
    // SPEC §§3.1, 7.3 (T24 T33): an inventory that does not advertise
    // per-launch claims keeps the host single-claim; one that does records it;
    // an older agent's next publication withdraws it.
    let per_launch = || {
        state
            .lock()
            .unwrap()
            .store()
            .host_has_per_launch_claims(&host)
            .unwrap()
    };
    assert!(!per_launch());
    let advertised = pb::ReportInventory {
        launch_claims: "per_launch".into(),
        ..inventory.clone()
    };
    capyctl_controller::host_publication::publish(&state, &host, &advertised).unwrap();
    assert!(per_launch());
    // ADR 0013 §4, §5 (T24 T34): `per_instance` is per-launch claims whose
    // journal also fences per instance; `per_launch` alone is not.
    let per_instance = || {
        state
            .lock()
            .unwrap()
            .store()
            .host_has_per_instance_fencing(&host)
            .unwrap()
    };
    assert!(!per_instance());
    let fencing = pb::ReportInventory {
        launch_claims: "per_instance".into(),
        ..inventory.clone()
    };
    capyctl_controller::host_publication::publish(&state, &host, &fencing).unwrap();
    assert!(per_launch() && per_instance());
    capyctl_controller::host_publication::publish(&state, &host, &inventory).unwrap();
    assert!(!per_launch() && !per_instance());

    let publication = state
        .lock()
        .unwrap()
        .store()
        .host_publication(&host)
        .unwrap()
        .expect("the approved snapshot is stored");
    let stored = capyctl_config::remote_roles::HostConfig::parse(&publication.config_json).unwrap();
    assert_eq!(stored.profiles["qwen-vllm"]["engine"], "vllm");
    assert_eq!(stored.profiles["local"]["engine"], "sglang");
    assert_eq!(publication.fingerprint, inventory.policy_fingerprint);
    // The server resolves a vLLM deployment against the stored snapshot the
    // same way `RemoteProfileBindings` does, and reaches the vLLM profile.
    let mut deployment = input("vllm")["deployment"].clone();
    deployment["runtime_profile"] = json!("qwen-vllm");
    let scoped_host =
        capyctl_config::remote_resources::scope_host_document(&host, &stored.document).unwrap();
    let scoped =
        capyctl_config::remote_resources::scope_deployment_document(&host, &deployment).unwrap();
    let effective = capyctl_config::effective::resolve_effective(&scoped, &scoped_host).unwrap();
    assert_eq!(
        effective.profile.engine,
        capyctl_config::engine_policy::Engine::Vllm
    );
}

//! ADR 0017: `list hosts` / `inspect host` show each host's release version
//! and the version skew verdict of its latest session, with the reason, even
//! while the host is offline. CPU-only; not qualification of any engine.

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use capyctl_controller::{
    agent_sessions::{AgentSessions, SERVER_VERSION},
    enrollment::EnrollmentAuthority,
    OwnedCoordinatorState,
};
use capyctl_management::{hosts::hosts_router, ManagementCredentials};
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
};
use tower::ServiceExt;

// T06 T33: the listing carries the server's version, and per host the
// recorded version, compatibility and reason; a host never seen since the
// policy shows none.
#[tokio::test]
async fn list_hosts_shows_versions_and_upgrade_required() {
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let owned = Arc::new(Mutex::new(OwnedCoordinatorState::open(dir.path()).unwrap()));
    let authority = Arc::new(EnrollmentAuthority::new(
        owned.clone(),
        capyctl_agent::identity::CertificateAuthority::generate(100).unwrap(),
    ));
    {
        let owner = owned.lock().unwrap();
        let store = owner.store();
        for (digest, name) in [("b", "host-a"), ("e", "host-b")] {
            store
                .create_host_invitation(&digest.repeat(64), name, 100, 0)
                .unwrap();
            let cert = store
                .redeem_host_invitation(
                    &capyctl_store::enrollment::Redemption {
                        invitation_digest: digest.repeat(64),
                        transaction_id: format!("tx-{name}"),
                        host_name: name.into(),
                        key_digest: "c".repeat(64),
                        csr_digest: "d".repeat(64),
                    },
                    1,
                    |id| {
                        Ok(capyctl_store::enrollment::CertificateRecord {
                            host_id: id.into(),
                            fingerprint: format!("{digest}f").repeat(32),
                            certificate_pem: "certificate".into(),
                            expires_unix: 500,
                        })
                    },
                )
                .unwrap();
            if name == "host-a" {
                store
                    .record_host_version(
                        &cert.host_id,
                        &capyctl_store::host_versions::HostVersion {
                            binary_version: String::new(),
                            compatibility: "upgrade_required".into(),
                            reason: "the host reports no version (it predates the version skew policy); it is drain-only until it is upgraded to 0.1".into(),
                            capabilities: vec!["heartbeats".into()],
                            recorded_at_ms: 7,
                        },
                    )
                    .unwrap();
            }
        }
    }
    let admin = "a".repeat(32);
    let router = hosts_router(
        ManagementCredentials::from_trusted_resolver(&admin, &"b".repeat(32)).unwrap(),
        owned,
        AgentSessions::new(authority),
    );
    let response = router
        .oneshot(
            Request::builder()
                .uri("/management/v1/hosts")
                .header("authorization", format!("Bearer {admin}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["server_version"], SERVER_VERSION);
    let host = |name: &str| {
        body["hosts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["name"] == name)
            .unwrap()
            .clone()
    };
    let old = host("host-a");
    assert_eq!(old["compatibility"], "upgrade_required");
    assert_eq!(old["binary_version"], "");
    assert!(old["compatibility_reason"]
        .as_str()
        .unwrap()
        .contains("drain-only"));
    assert_eq!(old["capabilities"], json!(["heartbeats"]));
    assert_eq!(old["online"], false);
    let unseen = host("host-b");
    assert!(unseen["compatibility"].is_null());
    assert!(unseen["binary_version"].is_null());
}

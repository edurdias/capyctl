//! SPEC §9.1, T21, ADR 0012, owner decision P4: status and inspect mark every
//! deployment and host installation whose launch enables vLLM development mode.
//! CPU tests of this surface are not qualification of any native engine recipe.

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use capyctl_adapters::vllm::{plan_from_effective, render_command};
use capyctl_config::effective::resolve_effective;
use capyctl_controller::{
    agent_sessions::AgentSessions, enrollment::EnrollmentAuthority, OwnedCoordinatorState,
};
use capyctl_management::{hosts::hosts_router, ManagementCredentials};
use capyctl_store::development_controls::for_effective;
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
};
use tower::ServiceExt;

fn golden() -> (Value, Value) {
    let source: Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    (
        source["input"]["deployment"].clone(),
        source["input"]["host"].clone(),
    )
}

// T21: the status mark is derived, so it must never drift from the launch
// the renderer actually builds. Every resolvable combination is checked
// against the rendered `VLLM_SERVER_DEV_MODE`.
#[test]
fn the_status_mark_matches_the_rendered_development_mode() {
    let mut checked = 0;
    for deep_park in [None, Some("enabled"), Some("disabled")] {
        for residency in ["deep", "restart_only"] {
            let (mut deployment, mut host) = golden();
            let profile = &mut host["runtime_profiles"]["local"];
            // ADR 0014 §4: sleep mode is derived, not an installation setting.
            profile.as_object_mut().unwrap().remove("launch_settings");
            match deep_park {
                Some(value) => profile["security"]["deep_park"] = json!(value),
                None => {
                    profile["security"]
                        .as_object_mut()
                        .unwrap()
                        .remove("deep_park");
                }
            }
            deployment["residency"] = json!(residency);
            // ADR 0014 §5: the golden checkpoint has no manifest, so the KV cache is declared.
            deployment["engine_config"] = json!({"memory": {"kv_cache": "4GiB"}});
            // ADR 0012: a parking residency on an opted-out host does not resolve.
            let effective = match resolve_effective(&deployment, &host) {
                Ok(effective) => effective,
                Err(error) => {
                    eprintln!("{deep_park:?} {residency}: {error}");
                    continue;
                }
            };
            let plan =
                plan_from_effective(&effective, 8123, "/tmp/log".into(), "/tmp/rt".into()).unwrap();
            let rendered = render_command(&plan).unwrap();
            let dev_mode =
                rendered.env.get("VLLM_SERVER_DEV_MODE").map(String::as_str) == Some("1");
            assert_eq!(
                for_effective(&effective).is_exposed(),
                dev_mode,
                "{deep_park:?} {residency}"
            );
            checked += 1;
        }
    }
    assert_eq!(checked, 5);
}

// T21, T14: the host view marks each published installation, with the
// mitigations and the deep-park provenance, and a host that published nothing
// is `unknown` rather than safe.
#[tokio::test]
async fn the_host_view_marks_published_installations() {
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let owned = Arc::new(Mutex::new(OwnedCoordinatorState::open(dir.path()).unwrap()));
    let authority = Arc::new(EnrollmentAuthority::new(
        owned.clone(),
        capyctl_agent::identity::CertificateAuthority::generate(100).unwrap(),
    ));
    let (_, golden_host) = golden();
    let mut defaulted = golden_host["runtime_profiles"]["local"].clone();
    defaulted.as_object_mut().unwrap().remove("launch_settings");
    defaulted["security"]
        .as_object_mut()
        .unwrap()
        .remove("deep_park");
    let mut opted_out = defaulted.clone();
    opted_out["security"]["deep_park"] = json!("disabled");
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
            if name != "host-a" {
                continue;
            }
            let mut document: Value =
                serde_json::from_str(&capyctl_config::remote_roles::HostConfig::template(
                    std::path::Path::new("/home/operator/host"),
                ))
                .unwrap();
            document["name"] = json!(name);
            document["runtime_profiles"] =
                json!({"vllm-default": defaulted, "vllm-off": opted_out});
            store
                .publish_host_configuration(&capyctl_store::host_publication::HostPublication {
                    host_id: cert.host_id.clone(),
                    config_json: document.to_string(),
                    boot_id: "boot-a".into(),
                    fingerprint: capyctl_config::remote_resources::policy_fingerprint(&document),
                    received_at_ms: 100,
                })
                .unwrap();
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
    let host = |name: &str| {
        body["hosts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["name"] == name)
            .unwrap()
            .clone()
    };
    let published = host("host-a");
    let controls = &published["development_controls"];
    assert_eq!(controls["state"], "exposed", "{published}");
    let installations = controls["installations"].as_array().unwrap();
    assert_eq!(installations.len(), 2);
    let exposed = installations
        .iter()
        .find(|i| i["profile"] == "vllm-default")
        .unwrap();
    assert_eq!(exposed["state"], "exposed");
    assert_eq!(exposed["deep_park_source"], "default");
    assert_eq!(exposed["production_safe"], false);
    assert_eq!(exposed["applies_to"], "parking_deployments");
    assert_eq!(
        exposed["mitigations"],
        json!([
            "loopback_engine_listener",
            "per_launch_engine_key",
            "engine_key_guard_middleware",
            "no_ingress_or_router_path"
        ])
    );
    let off = installations
        .iter()
        .find(|i| i["profile"] == "vllm-off")
        .unwrap();
    assert_eq!(off["state"], "not_exposed");
    assert!(off.get("mitigations").is_none());
    // Additive: the existing host fields are unchanged.
    for field in [
        "host_id", "name", "revoked", "online", "eligible", "session",
    ] {
        assert!(published.get(field).is_some(), "{field}");
    }
    let silent = host("host-b");
    assert_eq!(silent["development_controls"]["state"], "unknown");
    assert_eq!(silent["development_controls"]["installations"], json!([]));
}

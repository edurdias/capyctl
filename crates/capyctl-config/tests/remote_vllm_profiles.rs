//! A remote host document may approve vLLM and SGLang profiles side by side
//! (SPEC §§4, 7, 9). The role parser, the policy fingerprint the host publishes,
//! and the host-scoped resolution the server admits against are engine-neutral:
//! each deployment resolves to the engine its named profile declares.

use capyctl_config::effective::resolve_effective;
use capyctl_config::engine_policy::Engine;
use capyctl_config::remote_resources::{
    local_host_document, policy_fingerprint, scope_deployment_document, scope_host_document,
};
use capyctl_config::remote_roles::HostConfig;
use serde_json::{json, Value};

fn golden(name: &str) -> Value {
    let text = match name {
        "vllm" => include_str!("fixtures/effective-vllm-golden.json"),
        _ => include_str!("fixtures/effective-sglang-golden.json"),
    };
    serde_json::from_str::<Value>(text).unwrap()["input"].clone()
}

/// The host role document an operator would write: the SGLang golden host with
/// the vLLM golden profile added under its own name, plus role-local settings.
fn mixed_host() -> (Value, Value, Value) {
    let sglang = golden("sglang");
    let vllm = golden("vllm");
    let mut host = sglang["host"].clone();
    host["runtime_profiles"]["qwen-vllm"] = vllm["host"]["runtime_profiles"]["local"].clone();
    host["state_dir"] = json!("/home/operator/.local/state/capyctl");
    host["identity_dir"] = json!("/home/operator/.local/state/capyctl/identity");
    host["ingress"] = json!({
        "transport": "trusted_private_link",
        "address": "http://100.64.0.2:9443",
        "bind": "100.64.0.2:9443"
    });
    let mut deployment = vllm["deployment"].clone();
    deployment["runtime_profile"] = json!("qwen-vllm");
    deployment["routes"] = json!(["toy-vllm"]);
    (host, deployment, sglang["deployment"].clone())
}

/// G01: host preparation publishes vLLM profiles exactly like SGLang ones; the
/// published document keeps both, and the fingerprint covers both.
// T07 T22
#[test]
fn a_host_document_approves_vllm_and_sglang_profiles_together() {
    let (host, _, _) = mixed_host();
    let config = HostConfig::parse(&host.to_string()).unwrap();
    assert_eq!(config.profiles.len(), 2);
    assert_eq!(config.profiles["qwen-vllm"]["engine"], "vllm");
    assert_eq!(config.profiles["local"]["engine"], "sglang");
    let fingerprint = policy_fingerprint(&config.document);
    let mut changed = host.clone();
    changed["runtime_profiles"]["qwen-vllm"]["build_fingerprint"] = json!("other-build");
    assert_ne!(
        policy_fingerprint(&HostConfig::parse(&changed.to_string()).unwrap().document),
        fingerprint,
        "a changed vLLM profile must change the published policy fingerprint"
    );
}

/// The agent resolves against its local document and the server against the
/// host-scoped one; both select the engine the named profile declares.
// T07 T22
#[test]
fn each_deployment_resolves_to_its_profiles_engine_on_both_sides() {
    let (host, vllm, sglang) = mixed_host();
    let config = HostConfig::parse(&host.to_string()).unwrap();
    let local = local_host_document(&config.document).unwrap();
    assert!(local.get("ingress").is_none() && local.get("state_dir").is_none());
    let agent_vllm = resolve_effective(&vllm, &local).unwrap();
    let agent_sglang = resolve_effective(&sglang, &local).unwrap();
    assert_eq!(agent_vllm.profile.engine, Engine::Vllm);
    assert_eq!(agent_sglang.profile.engine, Engine::Sglang);

    let scoped_host = scope_host_document("host-a", &config.document).unwrap();
    let scoped = scope_deployment_document("host-a", &vllm).unwrap();
    let server = resolve_effective(&scoped, &scoped_host).unwrap();
    assert_eq!(server.profile.engine, Engine::Vllm);
    // The controller's frozen fingerprints are the ones the agent checks.
    assert_eq!(
        server.profile.build_fingerprint,
        agent_vllm.profile.build_fingerprint
    );
    assert_eq!(
        server.model.content_fingerprint,
        agent_vllm.model.content_fingerprint
    );
}

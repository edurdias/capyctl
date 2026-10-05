use capyctl_config::engine_settings::{boolean, EngineOverrides};
use capyctl_config::groups_policy::{host_groups_policy, DEFAULT_RENDEZVOUS_PORTS};
use capyctl_config::remote_roles::HostConfig;
use serde_json::json;
use sha2::{Digest, Sha256};

// T03: an absent block is the default and names no peer address.
#[test]
fn absent_block_is_default() {
    let p = host_groups_policy(&json!({"resource_policy": {}})).unwrap();
    assert_eq!(p.peer_address, None);
    assert_eq!(p.rendezvous_ports, DEFAULT_RENDEZVOUS_PORTS);
    assert!(!p.require_rdma);
}

// T03: a declared block parses.
#[test]
fn declared_block_parses() {
    let p = host_groups_policy(&json!({"resource_policy": {"groups": {
        "peer_address": "192.0.2.10",
        "rendezvous_port_range": {"start": 26000, "end": 26009},
        "require_rdma": true}}}))
    .unwrap();
    assert_eq!(p.peer_address, Some("192.0.2.10".parse().unwrap()));
    assert_eq!(p.rendezvous_ports, 26000..=26009);
    assert!(p.require_rdma);
}

// T03, T37: loopback, unspecified, multicast, reversed and privileged ranges are refused.
#[test]
fn unsafe_values_are_refused() {
    for groups in [
        json!({"peer_address": "127.0.0.1"}),
        json!({"peer_address": "0.0.0.0"}),
        json!({"peer_address": "224.0.0.1"}),
        json!({"peer_address": "::1"}),
        json!({"peer_address": "::ffff:127.0.0.1"}),
        json!({"peer_address": "::ffff:0.0.0.0"}),
        json!({"peer_address": "::ffff:224.0.0.1"}),
        json!({"peer_address": "255.255.255.255"}),
        json!({"peer_address": "not-an-ip"}),
        json!({"rendezvous_port_range": {"start": 26009, "end": 26000}}),
        json!({"rendezvous_port_range": {"start": 80, "end": 90}}),
        json!({"rendezvous_port_range": {"start": 26000}}),
        json!({"require_rdma": "maybe"}),
        json!({"unknown": 1}),
    ] {
        let host = json!({"resource_policy": {"groups": groups.clone()}});
        assert!(host_groups_policy(&host).is_err(), "{groups}");
    }
}

fn lab_host() -> serde_json::Value {
    let all: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/f2-deployment.json")).unwrap();
    all["host"].clone()
}

fn policy_digest(host: &serde_json::Value) -> String {
    let policy = capyctl_config::effective::normalize_host_policy(host).unwrap();
    hex::encode(Sha256::digest(serde_json::to_vec(&policy).unwrap()))
}

// T39: a host without the block keeps its policy digest, and declaring the
// block leaves the published policy as it was (the policy digest does not
// cover groups).
#[test]
fn host_without_groups_keeps_its_digest() {
    let host = lab_host();
    let before = policy_digest(&host);
    let _ = host_groups_policy(&host).unwrap();
    assert_eq!(before, policy_digest(&host));
    let mut declared = host.clone();
    declared["resource_policy"]["groups"] = json!({"peer_address": "192.0.2.10"});
    assert_eq!(before, policy_digest(&declared));
}

// T03: a malformed groups block is refused with the host document.
#[test]
fn malformed_block_refuses_the_host() {
    let mut host = lab_host();
    host["resource_policy"]["groups"] = json!({"peer_address": "127.0.0.1"});
    assert!(capyctl_config::effective::normalize_host_policy(&host).is_err());
}

fn resolved(
    yaml: &serde_json::Value,
    env: &EngineOverrides,
    flags: &EngineOverrides,
) -> serde_json::Value {
    let mut document = json!({
        "schema_version": 1, "kind": "host", "name": "h",
        "state_dir": "/s", "identity_dir": "/s/identity",
        "model_store": {"path": "/s/models"}, "runtime_profiles": {}
    });
    document["resource_policy"] = yaml["resource_policy"].clone();
    let config = HostConfig::parse(&document.to_string())
        .unwrap()
        .with_engines(flags, env, &|_, _| Ok("fp".into()))
        .unwrap();
    config.document["resource_policy"].clone()
}

// T14: flag > env > YAML > default for every groups setting.
#[test]
fn groups_settings_precedence() {
    let yaml = json!({"resource_policy": {"groups": {"peer_address": "192.0.2.10"}}});
    let env = EngineOverrides::from_env(&|key| {
        [
            ("CAPYCTL_PEER_ADDRESS", "192.0.2.11"),
            ("CAPYCTL_RENDEZVOUS_PORTS", "26000-26009"),
            ("CAPYCTL_REQUIRE_RDMA", "true"),
        ]
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| (*v).to_owned())
    })
    .unwrap();
    let flags = EngineOverrides {
        peer_address: Some("192.0.2.12".parse().unwrap()),
        ..Default::default()
    };
    let policy = resolved(&yaml, &env, &flags);
    assert_eq!(policy["groups"]["peer_address"], "192.0.2.12");
    assert_eq!(
        policy["groups"]["rendezvous_port_range"],
        json!({"start": 26000, "end": 26009})
    );
    assert_eq!(policy["groups"]["require_rdma"], true);
    let policy = resolved(
        &yaml,
        &EngineOverrides::default(),
        &EngineOverrides::default(),
    );
    assert_eq!(policy["groups"]["peer_address"], "192.0.2.10");
    assert!(policy["groups"].get("rendezvous_port_range").is_none());
    // Each setting resolves independently across all three layers.
    let yaml = json!({"resource_policy": {"groups": {
        "rendezvous_port_range": {"start": 27000, "end": 27009}, "require_rdma": true}}});
    let env = EngineOverrides::from_env(&|key| {
        [
            ("CAPYCTL_RENDEZVOUS_PORTS", "26000-26009"),
            ("CAPYCTL_REQUIRE_RDMA", "false"),
        ]
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| (*v).to_owned())
    })
    .unwrap();
    let none = EngineOverrides::default();
    let policy = resolved(&yaml, &env, &none);
    assert_eq!(
        policy["groups"]["rendezvous_port_range"],
        json!({"start": 26000, "end": 26009})
    );
    assert_eq!(policy["groups"]["require_rdma"], false);
    let flags = EngineOverrides {
        rendezvous_ports: Some((28000, 28009)),
        require_rdma: Some(true),
        ..Default::default()
    };
    let policy = resolved(&yaml, &env, &flags);
    assert_eq!(
        policy["groups"]["rendezvous_port_range"],
        json!({"start": 28000, "end": 28009})
    );
    assert_eq!(policy["groups"]["require_rdma"], true);
    let policy = resolved(&yaml, &none, &none);
    assert_eq!(
        policy["groups"]["rendezvous_port_range"],
        json!({"start": 27000, "end": 27009})
    );
    assert_eq!(policy["groups"]["require_rdma"], true);
    // Nothing stated anywhere: no block is injected.
    let policy = resolved(
        &json!({"resource_policy": {}}),
        &EngineOverrides::default(),
        &EngineOverrides::default(),
    );
    assert!(policy.get("groups").is_none());
}

// T03: a malformed environment value is refused with the variable's name.
#[test]
fn malformed_environment_is_refused() {
    for (name, value) in [
        ("CAPYCTL_PEER_ADDRESS", "127.0.0.1"),
        ("CAPYCTL_RENDEZVOUS_PORTS", "26009-26000"),
        ("CAPYCTL_REQUIRE_RDMA", "maybe"),
    ] {
        let error =
            EngineOverrides::from_env(&|key| (key == name).then(|| value.to_owned())).unwrap_err();
        assert!(error.to_string().contains(name), "{error}");
    }
    assert!(boolean("x", "true").unwrap());
}

//! ADR 0017 (owner decision 2026-09-24): the version skew policy and the
//! capability gate for every protocol feature added after the version 2
//! baseline. CPU-only; nothing here qualifies a native engine.
use mllm_domain::{
    completion::ProcessIdentity,
    group::{CommandIdentity, MemberKey},
};
use mllm_protocol::{
    capabilities,
    execution::{DigestCheckpointPlan, MemberAction, MemberCommand, SingleLaunchPlan},
    pb,
    version::{assess, Compatibility, Version},
};
use prost::Message;
use std::collections::BTreeSet;

// T06 T34: the policy table. Same line (patch, pre-release, build metadata)
// is supported; N-1 is supported with an upgrade recommended; older or
// another major is drain-only; newer is refused; unreadable is drain-only.
#[test]
fn the_skew_policy_table() {
    let server = "0.4.2";
    let cases: &[(&str, Compatibility)] = &[
        ("0.4.2", Compatibility::Supported),
        ("0.4.0", Compatibility::Supported),
        ("0.4.9", Compatibility::Supported),
        ("0.4.2+build.7", Compatibility::Supported),
        ("0.4.3-rc.1", Compatibility::Supported),
        ("0.4.0-alpha", Compatibility::Supported),
        ("0.3.9", Compatibility::UpgradeRecommended),
        ("0.3.0-rc.2+sha.abc", Compatibility::UpgradeRecommended),
        ("0.2.7", Compatibility::UpgradeRequired),
        ("0.0.1", Compatibility::UpgradeRequired),
        ("0.5.0", Compatibility::Refused),
        ("0.5.0-rc.1", Compatibility::Refused),
        ("1.0.0", Compatibility::Refused),
        ("", Compatibility::UpgradeRequired),
        ("v0.4.2", Compatibility::UpgradeRequired),
        ("0.4", Compatibility::UpgradeRequired),
        ("0.04.2", Compatibility::UpgradeRequired),
        ("0.4.2-", Compatibility::UpgradeRequired),
        ("0.4.2-rc.01", Compatibility::UpgradeRequired),
        ("0.4.2+", Compatibility::UpgradeRequired),
        (" 0.4.2", Compatibility::UpgradeRequired),
        ("0.4.2.1", Compatibility::UpgradeRequired),
    ];
    for (host, expected) in cases {
        let verdict = assess(host, server);
        assert_eq!(verdict.state, *expected, "host {host:?}");
        assert_eq!(verdict.state.drain_only(), *expected == Compatibility::UpgradeRequired);
        assert_eq!(verdict.reason.is_empty(), *expected == Compatibility::Supported, "{host:?}");
    }
    // Majors: N-1 across a major boundary is not N-1; another major is
    // drain-only, a newer major refused.
    assert_eq!(assess("1.9.0", "2.0.0").state, Compatibility::UpgradeRequired);
    assert_eq!(assess("2.0.5", "2.1.0").state, Compatibility::UpgradeRecommended);
    assert_eq!(assess("3.0.0", "2.7.0").state, Compatibility::Refused);
    // The reasons say what to do.
    assert!(assess("0.5.0", server).reason.contains("upgrade the server first"));
    assert!(assess("0.3.1", server).reason.contains("upgrade the host"));
    assert!(assess("0.2.0", server).reason.contains("drain-only"));
    assert!(assess("", server).reason.contains("predates"));
    // An unreadable version is never echoed back.
    assert!(!assess("evil\nline", server).reason.contains("evil"));
    // This build's own version is strict semver and supported by itself.
    let own = mllm_protocol::version::BINARY_VERSION;
    Version::parse(own).expect("the Cargo version is strict semver");
    assert_eq!(assess(own, own).state, Compatibility::Supported);
}

#[test]
fn semver_parsing_is_strict_and_orders_by_precedence() {
    let v = Version::parse("1.2.3-rc.1+build.5").unwrap();
    assert_eq!((v.major, v.minor, v.patch), (1, 2, 3));
    assert_eq!(v.pre, vec!["rc", "1"]);
    assert_eq!(v.build, vec!["build", "5"]);
    assert_eq!(v.to_string(), "1.2.3-rc.1+build.5");
    for bad in ["", "1", "1.2", "1.2.x", "01.2.3", "1.2.3-", "1.2.3-a..b", "1.2.3+a_b", "1.2.3 "] {
        assert!(Version::parse(bad).is_err(), "{bad:?}");
    }
    assert!(Version::parse(&"1".repeat(200)).is_err());
    let order = ["1.0.0-alpha", "1.0.0-alpha.1", "1.0.0-alpha.beta", "1.0.0-beta", "1.0.0-beta.2", "1.0.0-beta.11", "1.0.0-rc.1", "1.0.0"];
    for pair in order.windows(2) {
        let (a, b) = (Version::parse(pair[0]).unwrap(), Version::parse(pair[1]).unwrap());
        assert_eq!(a.precedence(&b), std::cmp::Ordering::Less, "{pair:?}");
    }
    assert_eq!(
        Version::parse("1.0.0+a").unwrap().precedence(&Version::parse("1.0.0+b").unwrap()),
        std::cmp::Ordering::Equal
    );
}

fn identity(instance_index: u32) -> CommandIdentity {
    CommandIdentity {
        controller_id: "controller".into(),
        member: MemberKey { host_id: "host".into(), member_id: "head".into() },
        deployment_id: "deployment".into(),
        operation_id: "operation".into(),
        command_id: "01J00000000000000000000001".into(),
        step_id: "01J00000000000000000000001".into(),
        generation: 3,
        revision: 1,
        deadline_ms: 2_000_000_000_000,
        payload_digest: [0; 32],
        expected_state: "retained".into(),
        profile_fingerprint: "profile".into(),
        instance_index,
    }
}

fn wire(action: MemberAction, instance_index: u32) -> pb::ExecuteMember {
    let mut command = MemberCommand { identity: identity(instance_index), action };
    command.identity.payload_digest = command.canonical_digest();
    command.to_wire()
}

fn deployment() -> String {
    serde_json::json!({
        "schema_version": 1, "kind": "deployment", "name": "demo",
        "model": {"source": {"type": "local", "path": "/models/demo"}},
        "engine": {"type": "vllm"}
    })
    .to_string()
}

fn launch(digest: bool, startup: bool) -> MemberAction {
    MemberAction::LaunchSingle(SingleLaunchPlan {
        deployment_config: deployment(),
        profile_name: "local".into(),
        checkpoint_fingerprint: "checkpoint".into(),
        host_policy_fingerprint: "a".repeat(64),
        binding_id: "01J00000000000000000000002".into(),
        incarnation: "01J00000000000000000000003".into(),
        grant_id: "01J00000000000000000000004".into(),
        service_port: 8000,
        issued_at_ms: 1,
        coordinator_session_id: "01J00000000000000000000005".into(),
        checkpoint_digest: if digest { format!("sha256:{}", "b".repeat(64)) } else { String::new() },
        checkpoint_weights_bytes: digest.then_some(1 << 30),
        startup_bytes: startup.then_some(2 << 30),
    })
}

fn process() -> ProcessIdentity {
    ProcessIdentity { role: "api".into(), pid: 42, boot_id: "boot".into(), start_ticks: 7 }
}

// T34: every additive server-to-host field and action names the capability it
// needs; the baseline shapes need none.
#[test]
fn every_post_baseline_field_names_its_capability() {
    use capabilities::*;
    let digest = |size_only| {
        MemberAction::DigestCheckpoint(DigestCheckpointPlan {
            deployment_config: deployment(),
            host_policy_fingerprint: "a".repeat(64),
            expected_digest: None,
            size_only,
        })
    };
    let handle = || "01J00000000000000000000001".to_owned();
    let cases: Vec<(pb::ExecuteMember, Vec<&str>)> = vec![
        (wire(MemberAction::Inspect, 0), vec![]),
        (wire(MemberAction::CloseIngress, 0), vec![]),
        (wire(MemberAction::Probe { owned_handle: handle() }, 0), vec![]),
        (wire(MemberAction::Park { owned_handle: handle() }, 0), vec![]),
        (wire(MemberAction::Restore { owned_handle: handle(), checkpoint_digest: String::new() }, 0), vec![]),
        (wire(MemberAction::Terminate { owned_handle: handle(), recorded: vec![] }, 0), vec![]),
        (wire(launch(false, false), 0), vec![]),
        (wire(launch(true, false), 0), vec![CHECKPOINT_DIGEST]),
        (wire(launch(true, true), 0), vec![CHECKPOINT_DIGEST, STARTUP_BYTES]),
        (wire(launch(true, true), 1), vec![CHECKPOINT_DIGEST, STARTUP_BYTES, INSTANCE_INDEX]),
        (wire(MemberAction::Inspect, 2), vec![INSTANCE_INDEX]),
        (
            wire(MemberAction::Restore { owned_handle: handle(), checkpoint_digest: format!("sha256:{}", "c".repeat(64)) }, 0),
            vec![RESTORE_CHECKPOINT_DIGEST],
        ),
        (
            wire(MemberAction::Terminate { owned_handle: handle(), recorded: vec![process()] }, 0),
            vec![TERMINATE_RECORDED_PROCESSES],
        ),
        (wire(digest(false), 0), vec![CHECKPOINT_DIGEST]),
        (wire(digest(true), 0), vec![CHECKPOINT_DIGEST, CHECKPOINT_SIZE_ONLY]),
    ];
    for (command, expected) in cases {
        assert_eq!(required(&command), expected, "{command:?}");
    }
    // MaterializeSource needs model_sources.
    let mut source = wire(MemberAction::Inspect, 0);
    source.action = Some(pb::execute_member::Action::MaterializeSource(pb::MaterializeSourceRequest::default()));
    assert_eq!(required(&source), vec![MODEL_SOURCES]);
    // Every name `required` can return is a server-to-host catalogue entry.
    for (name, direction) in CATALOGUE {
        assert!(is_gate_refusal(&missing(name)));
        if [HEARTBEATS, MODEL_SOURCES, CHECKPOINT_DIGEST, CHECKPOINT_SIZE_ONLY, STARTUP_BYTES,
            INSTANCE_INDEX, RESTORE_CHECKPOINT_DIGEST, TERMINATE_RECORDED_PROCESSES].contains(name)
        {
            assert_eq!(*direction, Direction::ServerToHost, "{name}");
        } else {
            assert_eq!(*direction, Direction::HostToServer, "{name}");
        }
    }
    assert!(!is_gate_refusal("host_capability_missing:"));
    assert!(!is_gate_refusal("host_capability_missing:everything"));
}

// T34 T33: a host that lacks a capability is refused the command that needs
// it (typed), a drain-only host is refused everything but stop, close, probe
// and inspect, and a host with everything is refused nothing.
#[test]
fn the_gate_refuses_typed_before_anything_is_sent() {
    use capabilities::*;
    let everything: BTreeSet<String> = agent_capabilities().into_iter().collect();
    let baseline = BTreeSet::new();
    let start = wire(launch(true, true), 0);
    let stop = wire(MemberAction::Terminate { owned_handle: "h".into(), recorded: vec![] }, 0);
    let park = wire(MemberAction::Park { owned_handle: "h".into() }, 0);
    let probe = wire(MemberAction::Probe { owned_handle: "h".into() }, 0);
    assert_eq!(refusal(false, &everything, &start), None);
    assert_eq!(refusal(false, &baseline, &start).as_deref(), Some("host_capability_missing:checkpoint_digest"));
    let mut partial = everything.clone();
    partial.remove(STARTUP_BYTES);
    assert_eq!(refusal(false, &partial, &start).as_deref(), Some("host_capability_missing:startup_bytes"));
    // Drain-only: start and park refused as upgrade_required; stop and probe
    // pass (and need no capability in their baseline shape).
    assert_eq!(refusal(true, &everything, &start).as_deref(), Some(HOST_UPGRADE_REQUIRED));
    assert_eq!(refusal(true, &everything, &park).as_deref(), Some(HOST_UPGRADE_REQUIRED));
    assert_eq!(refusal(true, &baseline, &stop), None);
    assert_eq!(refusal(true, &baseline, &probe), None);
    // A drain-only host is never sent recorded identities it cannot decode.
    let recorded = wire(MemberAction::Terminate { owned_handle: "h".into(), recorded: vec![process()] }, 0);
    assert_eq!(
        refusal(true, &baseline, &recorded).as_deref(),
        Some("host_capability_missing:terminate_recorded_processes")
    );
}

// T34: an absent additive field encodes exactly as the baseline, so an older
// host recomputes the same payload digest; a present one does not, which is
// why it is never sent to a host that did not declare it.
#[test]
fn absent_fields_encode_as_the_baseline() {
    let without = wire(MemberAction::Terminate { owned_handle: "h".into(), recorded: vec![] }, 0);
    let mut baseline = without.clone();
    baseline.terminate_recorded_processes.clear();
    baseline.restore_checkpoint_digest.clear();
    assert_eq!(without.encode_to_vec(), baseline.encode_to_vec());
    // A present one adds bytes an older host does not know.
    let with = wire(MemberAction::Terminate { owned_handle: "h".into(), recorded: vec![process()] }, 0);
    assert!(with.encode_to_vec().len() > without.encode_to_vec().len());
    // An older host drops field 13 on decode and recomputes a different
    // digest than the one the command carries: it would refuse the command.
    let mut dropped = with.clone();
    dropped.terminate_recorded_processes.clear();
    let decoded = MemberCommand::try_from(pb::ServerToAgent {
        msg: Some(pb::server_to_agent::Msg::ExecuteMember(dropped)),
    })
    .unwrap();
    assert!(decoded.verify_digest().is_err());
    // Instance 0 encodes no instance_index at all: the identity's encoding
    // is one field shorter than the same identity at instance 1.
    let zero = wire(MemberAction::Inspect, 0).identity.unwrap();
    let mut one = zero.clone();
    one.instance_index = 1;
    assert_eq!(one.encode_to_vec().len(), zero.encode_to_vec().len() + 2);
}

// T06: the Connect capability declaration is bounded and folds in the two
// earlier boolean flags.
#[test]
fn a_connect_declaration_is_bounded() {
    let mut connect = pb::Connect { heartbeats: true, ..Default::default() };
    assert_eq!(
        capabilities::declared(&connect).unwrap(),
        BTreeSet::from([capabilities::HEARTBEATS.to_owned()])
    );
    connect.capabilities = capabilities::agent_capabilities();
    assert_eq!(capabilities::declared(&connect).unwrap().len(), capabilities::CATALOGUE.len());
    connect.capabilities = vec!["Bad-Name".into()];
    assert!(capabilities::declared(&connect).is_none());
    connect.capabilities = (0..65).map(|i| format!("c{i}")).collect();
    assert!(capabilities::declared(&connect).is_none());
    // A name this build does not know is kept but grants nothing.
    connect.capabilities = vec!["future_feature".into()];
    assert!(capabilities::declared(&connect).unwrap().contains("future_feature"));
}

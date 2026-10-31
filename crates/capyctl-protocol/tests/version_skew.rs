//! ADR 0017 (owner decision 2026-09-24): the version skew policy and the
//! capability gate for every protocol feature added after the version 2
//! baseline. CPU-only; nothing here qualifies a native engine.
use capyctl_domain::{
    completion::ProcessIdentity,
    group::{CommandIdentity, MemberKey},
};
use capyctl_protocol::{
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
        assert_eq!(
            verdict.state.drain_only(),
            *expected == Compatibility::UpgradeRequired
        );
        assert_eq!(
            verdict.reason.is_empty(),
            *expected == Compatibility::Supported,
            "{host:?}"
        );
    }
    // Majors: N-1 across a major boundary is not N-1; another major is
    // drain-only, a newer major refused.
    assert_eq!(
        assess("1.9.0", "2.0.0").state,
        Compatibility::UpgradeRequired
    );
    assert_eq!(
        assess("2.0.5", "2.1.0").state,
        Compatibility::UpgradeRecommended
    );
    assert_eq!(assess("3.0.0", "2.7.0").state, Compatibility::Refused);
    // The reasons say what to do.
    assert!(assess("0.5.0", server)
        .reason
        .contains("upgrade the server first"));
    assert!(assess("0.3.1", server).reason.contains("upgrade the host"));
    assert!(assess("0.2.0", server).reason.contains("drain-only"));
    assert!(assess("", server).reason.contains("predates"));
    // An unreadable version is never echoed back.
    assert!(!assess("evil\nline", server).reason.contains("evil"));
    // This build's own version is strict semver and supported by itself.
    let own = capyctl_protocol::version::BINARY_VERSION;
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
    for bad in [
        "",
        "1",
        "1.2",
        "1.2.x",
        "01.2.3",
        "1.2.3-",
        "1.2.3-a..b",
        "1.2.3+a_b",
        "1.2.3 ",
    ] {
        assert!(Version::parse(bad).is_err(), "{bad:?}");
    }
    assert!(Version::parse(&"1".repeat(200)).is_err());
    let order = [
        "1.0.0-alpha",
        "1.0.0-alpha.1",
        "1.0.0-alpha.beta",
        "1.0.0-beta",
        "1.0.0-beta.2",
        "1.0.0-beta.11",
        "1.0.0-rc.1",
        "1.0.0",
    ];
    for pair in order.windows(2) {
        let (a, b) = (
            Version::parse(pair[0]).unwrap(),
            Version::parse(pair[1]).unwrap(),
        );
        assert_eq!(a.precedence(&b), std::cmp::Ordering::Less, "{pair:?}");
    }
    assert_eq!(
        Version::parse("1.0.0+a")
            .unwrap()
            .precedence(&Version::parse("1.0.0+b").unwrap()),
        std::cmp::Ordering::Equal
    );
}

fn identity(instance_index: u32) -> CommandIdentity {
    CommandIdentity {
        controller_id: "controller".into(),
        member: MemberKey {
            host_id: "host".into(),
            member_id: "head".into(),
        },
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
    let mut command = MemberCommand {
        identity: identity(instance_index),
        action,
    };
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
        checkpoint_digest: if digest {
            format!("sha256:{}", "b".repeat(64))
        } else {
            String::new()
        },
        checkpoint_weights_bytes: digest.then_some(1 << 30),
        checkpoint_state_slot_bytes: None,
        checkpoint_layout: None,
        checkpoint_tables: None,
        checkpoint_gguf: None,
        startup_bytes: startup.then_some(2 << 30),
        startup_host_bytes: None,
        lmcache: None,
        devices: Vec::new(),
    })
}

fn process() -> ProcessIdentity {
    ProcessIdentity {
        role: "api".into(),
        pid: 42,
        boot_id: "boot".into(),
        start_ticks: 7,
    }
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
        (
            wire(
                MemberAction::Probe {
                    owned_handle: handle(),
                    max_tokens: None,
                    decision: false,
                },
                0,
            ),
            vec![],
        ),
        // T34, T43 (ADR 0030 §4): an older host would answer a readiness probe
        // instead; only a host with `decisions` is sent a decision probe.
        (
            wire(
                MemberAction::Probe {
                    owned_handle: handle(),
                    max_tokens: None,
                    decision: true,
                },
                0,
            ),
            vec![DECISIONS],
        ),
        (
            wire(
                MemberAction::Park {
                    owned_handle: handle(),
                },
                0,
            ),
            vec![],
        ),
        (
            wire(
                MemberAction::Restore {
                    owned_handle: handle(),
                    checkpoint_digest: String::new(),
                },
                0,
            ),
            vec![],
        ),
        (
            wire(
                MemberAction::Terminate {
                    owned_handle: handle(),
                    recorded: vec![],
                },
                0,
            ),
            vec![],
        ),
        (wire(launch(false, false), 0), vec![]),
        (wire(launch(true, false), 0), vec![CHECKPOINT_DIGEST]),
        (
            wire(launch(true, true), 0),
            vec![CHECKPOINT_DIGEST, STARTUP_BYTES],
        ),
        (
            wire(launch(true, true), 1),
            vec![CHECKPOINT_DIGEST, STARTUP_BYTES, INSTANCE_INDEX],
        ),
        (
            wire(
                match launch(true, true) {
                    MemberAction::LaunchSingle(mut plan) => {
                        plan.checkpoint_state_slot_bytes = Some(10 << 20);
                        MemberAction::LaunchSingle(plan)
                    }
                    _ => unreachable!(),
                },
                0,
            ),
            vec![CHECKPOINT_DIGEST, STARTUP_BYTES, CHECKPOINT_STATE_SLOT],
        ),
        // ADR 0019 §3: a discrete launch measured per domain carries the host
        // RAM charge beside the card's.
        (
            wire(
                match launch(true, true) {
                    MemberAction::LaunchSingle(mut plan) => {
                        plan.startup_host_bytes = Some(6 << 30);
                        MemberAction::LaunchSingle(plan)
                    }
                    _ => unreachable!(),
                },
                0,
            ),
            vec![CHECKPOINT_DIGEST, STARTUP_BYTES, STARTUP_HOST_BYTES],
        ),
        (wire(MemberAction::Inspect, 2), vec![INSTANCE_INDEX]),
        (
            wire(
                MemberAction::Restore {
                    owned_handle: handle(),
                    checkpoint_digest: format!("sha256:{}", "c".repeat(64)),
                },
                0,
            ),
            vec![RESTORE_CHECKPOINT_DIGEST],
        ),
        (
            wire(
                MemberAction::Terminate {
                    owned_handle: handle(),
                    recorded: vec![process()],
                },
                0,
            ),
            vec![TERMINATE_RECORDED_PROCESSES],
        ),
        (wire(digest(false), 0), vec![CHECKPOINT_DIGEST]),
        (
            wire(digest(true), 0),
            vec![CHECKPOINT_DIGEST, CHECKPOINT_SIZE_ONLY],
        ),
    ];
    for (command, expected) in cases {
        assert_eq!(required(&command), expected, "{command:?}");
    }
    // MaterializeSource needs model_sources.
    let mut source = wire(MemberAction::Inspect, 0);
    source.action = Some(pb::execute_member::Action::MaterializeSource(
        pb::MaterializeSourceRequest::default(),
    ));
    assert_eq!(required(&source), vec![MODEL_SOURCES]);
    // Every name `required` can return is a server-to-host catalogue entry.
    for (name, direction) in CATALOGUE {
        assert!(is_gate_refusal(&missing(name)));
        if [
            HEARTBEATS,
            MODEL_SOURCES,
            CHECKPOINT_DIGEST,
            CHECKPOINT_SIZE_ONLY,
            STARTUP_BYTES,
            INSTANCE_INDEX,
            RESTORE_CHECKPOINT_DIGEST,
            TERMINATE_RECORDED_PROCESSES,
            LIVE_PROFILE_UPDATE,
            DEVICE_MEMORY_DOMAINS,
            CHECKPOINT_STATE_SLOT,
            ENGINE_GROUPS,
            ENGINE_LOG_TAIL,
            CHECKPOINT_TABLES,
            CHECKPOINT_GGUF,
            DECISIONS,
            CUSTOM_ENGINES,
            STARTUP_HOST_BYTES,
            LMCACHE,
            RETENTION,
            REQUEST_ABORT,
            LOCAL_TENSOR_PARALLEL,
        ]
        .contains(name)
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
    let stop = wire(
        MemberAction::Terminate {
            owned_handle: "h".into(),
            recorded: vec![],
        },
        0,
    );
    let park = wire(
        MemberAction::Park {
            owned_handle: "h".into(),
        },
        0,
    );
    let probe = wire(
        MemberAction::Probe {
            owned_handle: "h".into(),
            max_tokens: None,
            decision: false,
        },
        0,
    );
    assert_eq!(refusal(false, &everything, &start), None);
    assert_eq!(
        refusal(false, &baseline, &start).as_deref(),
        Some("host_capability_missing:checkpoint_digest")
    );
    let mut partial = everything.clone();
    partial.remove(STARTUP_BYTES);
    assert_eq!(
        refusal(false, &partial, &start).as_deref(),
        Some("host_capability_missing:startup_bytes")
    );
    // Drain-only: start and park refused as upgrade_required; stop and probe
    // pass (and need no capability in their baseline shape).
    assert_eq!(
        refusal(true, &everything, &start).as_deref(),
        Some(HOST_UPGRADE_REQUIRED)
    );
    assert_eq!(
        refusal(true, &everything, &park).as_deref(),
        Some(HOST_UPGRADE_REQUIRED)
    );
    assert_eq!(refusal(true, &baseline, &stop), None);
    assert_eq!(refusal(true, &baseline, &probe), None);
    // A drain-only host is never sent recorded identities it cannot decode.
    let recorded = wire(
        MemberAction::Terminate {
            owned_handle: "h".into(),
            recorded: vec![process()],
        },
        0,
    );
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
    let without = wire(
        MemberAction::Terminate {
            owned_handle: "h".into(),
            recorded: vec![],
        },
        0,
    );
    let mut baseline = without.clone();
    baseline.terminate_recorded_processes.clear();
    baseline.restore_checkpoint_digest.clear();
    assert_eq!(without.encode_to_vec(), baseline.encode_to_vec());
    // A present one adds bytes an older host does not know.
    let with = wire(
        MemberAction::Terminate {
            owned_handle: "h".into(),
            recorded: vec![process()],
        },
        0,
    );
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
    let mut connect = pb::Connect {
        heartbeats: true,
        ..Default::default()
    };
    assert_eq!(
        capabilities::declared(&connect).unwrap(),
        BTreeSet::from([capabilities::HEARTBEATS.to_owned()])
    );
    connect.capabilities = capabilities::agent_capabilities();
    assert_eq!(
        capabilities::declared(&connect).unwrap().len(),
        capabilities::CATALOGUE.len()
    );
    connect.capabilities = vec!["Bad-Name".into()];
    assert!(capabilities::declared(&connect).is_none());
    connect.capabilities = (0..65).map(|i| format!("c{i}")).collect();
    assert!(capabilities::declared(&connect).is_none());
    // A name this build does not know is kept but grants nothing.
    connect.capabilities = vec!["future_feature".into()];
    assert!(capabilities::declared(&connect)
        .unwrap()
        .contains("future_feature"));
}

// T34 (ADR 0018 §3): live profile updates are a server-to-host capability
// that every current agent declares and the server advertises back; the new
// messages are additive and an absent SessionReady list encodes as before.
#[test]
fn live_profile_update_is_declared_both_ways() {
    use capabilities::*;
    assert!(CATALOGUE.contains(&(LIVE_PROFILE_UPDATE, Direction::ServerToHost)));
    assert!(agent_capabilities().contains(&LIVE_PROFILE_UPDATE.to_owned()));
    // ADR 0033 §12: and `lmcache`, so a host sends its LMCache samples;
    // #198: and `installation_unsupported`, so a host reports that refusal.
    assert_eq!(
        server_capabilities(),
        vec![
            LIVE_PROFILE_UPDATE.to_owned(),
            LMCACHE.to_owned(),
            INSTALLATION_UNSUPPORTED.to_owned()
        ]
    );
    let before = pb::SessionReady {
        controller_id: "c".into(),
        session_id: "s".into(),
        ..Default::default()
    };
    let mut after = before.clone();
    assert_eq!(before.encode_to_vec(), after.encode_to_vec());
    after.capabilities = server_capabilities();
    assert!(after.encode_to_vec().len() > before.encode_to_vec().len());
    let publish = pb::AgentToServer {
        msg: Some(pb::agent_to_server::Msg::PublishProfiles(
            pb::PublishProfiles {
                request_id: "01J00000000000000000000001".into(),
                inventory: Some(pb::ReportInventory::default()),
            },
        )),
    };
    let decoded = pb::AgentToServer::decode(publish.encode_to_vec().as_slice()).unwrap();
    assert_eq!(decoded, publish);
    let retire = pb::ServerToAgent {
        msg: Some(pb::server_to_agent::Msg::ProfileRetirement(
            pb::ProfileRetirement {
                request_id: "r".into(),
                outcome: "in_use".into(),
                deployments: vec!["q14".into()],
                reason: String::new(),
            },
        )),
    };
    assert_eq!(
        pb::ServerToAgent::decode(retire.encode_to_vec().as_slice()).unwrap(),
        retire
    );
}

// T34 #198 (ADR 0017, ADR 0008 amendment 2026-10-30): a 0.1.10 controller's
// refusal check does not know `installation_unsupported:editable` and would
// end the whole control session on it. A host reports it only to a server
// whose SessionReady lists `installation_unsupported`; every refusal an
// earlier server knows still reaches it.
#[test]
fn installation_unsupported_reaches_only_a_server_that_takes_it() {
    use capabilities::*;
    use capyctl_protocol::execution::{is_policy_refusal, POLICY_REFUSALS};
    const EDITABLE: &str = "installation_unsupported:editable";
    assert_eq!(INSTALLATION_UNSUPPORTED, "installation_unsupported");
    assert!(CATALOGUE.contains(&(INSTALLATION_UNSUPPORTED, Direction::HostToServer)));
    assert!(agent_capabilities().contains(&INSTALLATION_UNSUPPORTED.to_owned()));
    assert!(!PLACEMENT_REQUIRED.contains(&INSTALLATION_UNSUPPORTED));
    assert!(is_policy_refusal(EDITABLE));
    // What a 0.1.10 controller lists in its SessionReady, and none at all.
    let older = vec![LIVE_PROFILE_UPDATE.to_owned(), LMCACHE.to_owned()];
    for server in [older, Vec::new()] {
        assert!(!server_takes_refusal(&server, EDITABLE), "{server:?}");
        for reason in POLICY_REFUSALS.iter().filter(|reason| **reason != EDITABLE) {
            assert!(server_takes_refusal(&server, reason), "{reason}");
        }
    }
    let current = server_capabilities();
    for reason in POLICY_REFUSALS {
        assert!(server_takes_refusal(&current, reason), "{reason}");
    }
}

// T43 T34 (ADR 0030 §1): `decisions` is a capability every current agent
// declares. It is no placement requirement for other deployments: only a
// decisions deployment is refused, typed, on a host without it.
#[test]
fn decisions_gates_only_decision_deployments() {
    use capabilities::*;
    assert_eq!(DECISIONS, "decisions");
    assert!(CATALOGUE.contains(&(DECISIONS, Direction::ServerToHost)));
    assert!(agent_capabilities().contains(&DECISIONS.to_owned()));
    assert!(!PLACEMENT_REQUIRED.contains(&DECISIONS));
    assert!(is_gate_refusal(&missing(DECISIONS)));
    let older: BTreeSet<String> = agent_capabilities()
        .into_iter()
        .filter(|name| name != DECISIONS)
        .collect();
    assert_eq!(
        decisions_refusal(&older).as_deref(),
        Some("host_capability_missing:decisions")
    );
    assert_eq!(
        decisions_refusal(&agent_capabilities().into_iter().collect()),
        None
    );
}

// T44 T34 (ADR 0031 §9): `custom_engines` is a capability every current
// agent declares. It is no placement requirement for other deployments:
// only a deployment on a custom engine is refused, typed, on a host without
// it.
#[test]
fn custom_engines_gates_only_custom_engine_deployments() {
    use capabilities::*;
    assert_eq!(CUSTOM_ENGINES, "custom_engines");
    assert!(CATALOGUE.contains(&(CUSTOM_ENGINES, Direction::ServerToHost)));
    assert!(agent_capabilities().contains(&CUSTOM_ENGINES.to_owned()));
    assert!(!PLACEMENT_REQUIRED.contains(&CUSTOM_ENGINES));
    assert!(is_gate_refusal(&missing(CUSTOM_ENGINES)));
    let older: BTreeSet<String> = agent_capabilities()
        .into_iter()
        .filter(|name| name != CUSTOM_ENGINES)
        .collect();
    assert_eq!(
        custom_engines_refusal(&older).as_deref(),
        Some("host_capability_missing:custom_engines")
    );
    assert_eq!(decisions_refusal(&older), None);
    assert_eq!(
        custom_engines_refusal(&agent_capabilities().into_iter().collect()),
        None
    );
}

/// A single launch of the test deployment with `settings` merged into its
/// document's root.
fn launch_with(settings: serde_json::Value) -> pb::ExecuteMember {
    let MemberAction::LaunchSingle(mut plan) = launch(true, false) else {
        unreachable!()
    };
    let mut document: serde_json::Value = serde_json::from_str(&plan.deployment_config).unwrap();
    for (key, value) in settings.as_object().unwrap() {
        document[key] = value.clone();
    }
    plan.deployment_config = document.to_string();
    wire(MemberAction::LaunchSingle(plan), 0)
}

// T45 T34 (ADR 0033 §13): `lmcache` and `retention` are capabilities every
// current agent declares. Neither is a placement requirement for other
// deployments: only `lmcache: required` and `retention: zero` need them,
// at placement (typed, before anything is sent) and as the launch's own
// requirement, so the session's last fence refuses such a launch to an
// older host too.
#[test]
fn lmcache_and_retention_gate_only_the_deployments_that_need_them() {
    use capabilities::*;
    use capyctl_config::lmcache::{HostLmcache, LmcacheMode, Retention};
    assert_eq!(LMCACHE, "lmcache");
    assert_eq!(RETENTION, "retention");
    for name in [LMCACHE, RETENTION] {
        assert!(CATALOGUE.contains(&(name, Direction::ServerToHost)));
        assert!(agent_capabilities().contains(&name.to_owned()));
        assert!(!PLACEMENT_REQUIRED.contains(&name));
        assert!(is_gate_refusal(&missing(name)));
    }
    assert!(agent_capabilities().len() <= MAX_DECLARED);
    let current: BTreeSet<String> = agent_capabilities().into_iter().collect();
    let older: BTreeSet<String> = current
        .iter()
        .filter(|name| *name != LMCACHE && *name != RETENTION)
        .cloned()
        .collect();
    let standard = (HostLmcache::Auto, Retention::Standard);
    let refusal = |declared: &BTreeSet<String>, mode, retention| {
        lmcache_refusal(declared, mode, retention, standard.0, standard.1)
    };
    assert_eq!(
        refusal(&older, LmcacheMode::Required, Retention::Standard),
        Some("host_capability_missing:lmcache")
    );
    assert_eq!(
        refusal(&older, LmcacheMode::Auto, Retention::Zero),
        Some("host_capability_missing:retention")
    );
    assert_eq!(
        refusal(&older, LmcacheMode::Auto, Retention::Standard),
        None
    );
    assert_eq!(refusal(&older, LmcacheMode::Off, Retention::Standard), None);
    assert_eq!(
        refusal(&current, LmcacheMode::Required, Retention::Standard),
        None
    );
    assert_eq!(refusal(&current, LmcacheMode::Auto, Retention::Zero), None);
    assert_eq!(decisions_refusal(&older), None);
    assert_eq!(custom_engines_refusal(&older), None);

    // The launch carries the deployment's needs.
    let plain = launch_with(serde_json::json!({}));
    assert_eq!(required(&plain), vec![CHECKPOINT_DIGEST]);
    let auto = launch_with(serde_json::json!({"lmcache": "auto", "retention": "standard"}));
    assert_eq!(required(&auto), vec![CHECKPOINT_DIGEST]);
    let needs_lmcache = launch_with(serde_json::json!({"lmcache": "required"}));
    assert_eq!(required(&needs_lmcache), vec![CHECKPOINT_DIGEST, LMCACHE]);
    let zero = launch_with(serde_json::json!({"lmcache": "off", "retention": "zero"}));
    assert_eq!(required(&zero), vec![CHECKPOINT_DIGEST, RETENTION]);
    assert_eq!(
        refusal_for(&older, &needs_lmcache).as_deref(),
        Some("host_capability_missing:lmcache")
    );
    assert_eq!(
        refusal_for(&older, &zero).as_deref(),
        Some("host_capability_missing:retention")
    );
    assert_eq!(refusal_for(&older, &plain), None);
    assert_eq!(refusal_for(&current, &needs_lmcache), None);
    assert_eq!(refusal_for(&current, &zero), None);
}

fn refusal_for(declared: &BTreeSet<String>, command: &pb::ExecuteMember) -> Option<String> {
    capabilities::refusal(false, declared, command)
}

// T45 T35 T36 T34 (ADR 0033 §§3, 6, 13): a launch's LMCache grant is an
// additive field that needs `lmcache`: a launch without one encodes exactly
// as before, a host that did not declare it is refused typed before anything
// is sent, and an older host that dropped the field would recompute another
// digest. A grant beside `lmcache: required` names the capability once.
#[test]
fn an_lmcache_grant_is_sent_only_to_a_host_that_declared_lmcache() {
    use capabilities::*;
    use capyctl_protocol::execution::LmcacheGrant;
    let grant = LmcacheGrant {
        rpc_port: 8100,
        http_port: 8101,
        pool_bytes: 4 << 30,
        overhead_bytes: 1280 << 20,
        device_bytes: 512 << 20,
        held_kv_bytes: 0,
    };
    let typed = |settings: serde_json::Value| {
        let MemberAction::LaunchSingle(mut plan) = launch(true, false) else {
            unreachable!()
        };
        let mut document: serde_json::Value =
            serde_json::from_str(&plan.deployment_config).unwrap();
        for (key, value) in settings.as_object().unwrap() {
            document[key] = value.clone();
        }
        plan.deployment_config = document.to_string();
        plan.lmcache = Some(grant);
        let mut command = MemberCommand {
            identity: identity(0),
            action: MemberAction::LaunchSingle(plan),
        };
        command.identity.payload_digest = command.canonical_digest();
        command
    };
    let with_grant = |settings: serde_json::Value| typed(settings).to_wire();
    let plain = launch_with(serde_json::json!({}));
    let granted = with_grant(serde_json::json!({}));
    assert_eq!(required(&plain), vec![CHECKPOINT_DIGEST]);
    assert_eq!(required(&granted), vec![CHECKPOINT_DIGEST, LMCACHE]);
    let required_and_granted = with_grant(serde_json::json!({"lmcache": "required"}));
    assert_eq!(
        required(&required_and_granted),
        vec![CHECKPOINT_DIGEST, LMCACHE]
    );
    let current: BTreeSet<String> = agent_capabilities().into_iter().collect();
    let older: BTreeSet<String> = current
        .iter()
        .filter(|name| *name != LMCACHE)
        .cloned()
        .collect();
    assert_eq!(
        refusal_for(&older, &granted).as_deref(),
        Some("host_capability_missing:lmcache")
    );
    assert_eq!(refusal_for(&older, &plain), None);
    assert_eq!(refusal_for(&current, &granted), None);
    // Absent, the plan encodes exactly as an older server's: no field 19.
    let Some(pb::execute_member::Action::LaunchSingle(plan)) = granted.action.clone() else {
        unreachable!()
    };
    let mut baseline = plan.clone();
    baseline.lmcache = None;
    let mut tail = Vec::new();
    prost::encoding::message::encode(19, &grant.to_wire(), &mut tail);
    assert_eq!(
        plan.encode_to_vec(),
        [baseline.encode_to_vec(), tail].concat(),
        "the grant is only field 19, appended"
    );
    // An older host drops field 19 and recomputes a different digest.
    let kept = typed(serde_json::json!({}));
    let mut dropped = kept.clone();
    let MemberAction::LaunchSingle(plan) = &mut dropped.action else {
        unreachable!()
    };
    plan.lmcache = None;
    assert_ne!(dropped.canonical_digest(), kept.identity.payload_digest);
}

// T34 T26 (ADR 0019, discrete GPU design §8): device memory domains are one
// capability every current agent declares. Its fields are additive: an
// observation or resident without them encodes exactly as an older host's,
// and one with them decodes back unchanged. It is not a placement
// requirement, so a unified host without it stays placeable.
#[test]
fn device_memory_domains_is_one_additive_capability() {
    use capabilities::*;
    assert_eq!(DEVICE_MEMORY_DOMAINS, "device_memory_domains");
    assert!(CATALOGUE.contains(&(DEVICE_MEMORY_DOMAINS, Direction::ServerToHost)));
    assert!(agent_capabilities().contains(&DEVICE_MEMORY_DOMAINS.to_owned()));
    assert!(!PLACEMENT_REQUIRED.contains(&DEVICE_MEMORY_DOMAINS));
    assert!(is_gate_refusal(&missing(DEVICE_MEMORY_DOMAINS)));
    assert_eq!(
        missing(DEVICE_MEMORY_DOMAINS),
        "host_capability_missing:device_memory_domains"
    );

    let older = pb::DomainObservation {
        domain_id: "unified".into(),
        kind: "system".into(),
        capacity_bytes: 128 << 30,
        available_bytes: 100 << 30,
        residents: vec![pb::ProcessResidency {
            pid: 7,
            boot_id: "b".into(),
            start_ticks: 9,
            resident_bytes: 3 << 30,
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut same = older.clone();
    same.device_id.clear();
    same.residents[0].device_bytes = 0;
    same.residents[0].host_bytes = 0;
    assert_eq!(older.encode_to_vec(), same.encode_to_vec());

    let device = pb::DomainObservation {
        domain_id: "gpu0".into(),
        kind: "device".into(),
        device_id: "gpu0".into(),
        capacity_bytes: 16376 << 20,
        available_bytes: 14000 << 20,
        ..Default::default()
    };
    let decoded = pb::DomainObservation::decode(device.encode_to_vec().as_slice()).unwrap();
    assert_eq!(decoded, device);
    let split = pb::ProcessResidency {
        device_bytes: 2 << 30,
        host_bytes: 1 << 30,
        resident_bytes: 3 << 30,
        ..older.residents[0].clone()
    };
    assert!(split.encode_to_vec().len() > older.residents[0].encode_to_vec().len());
    assert_eq!(
        pb::ProcessResidency::decode(split.encode_to_vec().as_slice()).unwrap(),
        split
    );
}

/// A launch on `devices` of one host, with a checkpoint layout when `layout`.
fn local_launch(devices: &[&str], layout: bool) -> MemberAction {
    let MemberAction::LaunchSingle(mut plan) = launch(true, true) else {
        unreachable!()
    };
    plan.devices = devices.iter().map(|d| (*d).to_owned()).collect();
    plan.checkpoint_layout = layout.then_some(capyctl_domain::member_weights::CheckpointLayout {
        sharded_bytes: 1 << 29,
        layer_count: 4,
        largest_layer_bytes: 1 << 27,
    });
    MemberAction::LaunchSingle(plan)
}

// T46 T34 (ADR 0035 §11, ADR 0017): a launch on several GPUs of one host
// needs `local_tensor_parallel`, never `engine_groups`, even with the layout
// its rank share is sized with; a launch on one GPU needs nothing new, and a
// group member's layout still needs `engine_groups`. A host without the
// capability is refused before anything is sent.
#[test]
fn a_launch_on_several_gpus_needs_local_tensor_parallel() {
    use capabilities::*;
    assert_eq!(LOCAL_TENSOR_PARALLEL, "local_tensor_parallel");
    assert!(CATALOGUE.contains(&(LOCAL_TENSOR_PARALLEL, Direction::ServerToHost)));
    assert!(agent_capabilities().contains(&LOCAL_TENSOR_PARALLEL.to_owned()));
    assert!(!PLACEMENT_REQUIRED.contains(&LOCAL_TENSOR_PARALLEL));
    let base = required(&wire(launch(true, true), 0));
    let local = wire(local_launch(&["gpu0", "gpu1"], true), 0);
    let mut expected = base.clone();
    expected.push(LOCAL_TENSOR_PARALLEL);
    assert_eq!(required(&local), expected);
    assert!(!required(&local).contains(&ENGINE_GROUPS));
    assert!(required(&wire(local_launch(&[], true), 0)).contains(&ENGINE_GROUPS));
    assert_eq!(required(&wire(local_launch(&[], false), 0)), base);

    let everything: BTreeSet<String> = agent_capabilities().into_iter().collect();
    let older: BTreeSet<String> = everything
        .iter()
        .filter(|c| *c != LOCAL_TENSOR_PARALLEL)
        .cloned()
        .collect();
    assert_eq!(refusal(false, &everything, &local), None);
    assert_eq!(
        refusal(false, &older, &local).as_deref(),
        Some("host_capability_missing:local_tensor_parallel")
    );
    assert_eq!(refusal(false, &older, &wire(launch(true, true), 0)), None);
    assert_eq!(local_tensor_parallel_refusal(&everything, 2), None);
    assert_eq!(local_tensor_parallel_refusal(&older, 1), None);
    assert_eq!(
        local_tensor_parallel_refusal(&older, 2).as_deref(),
        Some("host_capability_missing:local_tensor_parallel")
    );
}

// T46 (ADR 0035 §8): the per-GPU readings are additive: a resident without
// them encodes exactly as before, and one with them decodes back unchanged.
#[test]
fn per_gpu_readings_are_additive() {
    let older = pb::ProcessResidency {
        pid: 7,
        boot_id: "b".into(),
        start_ticks: 9,
        resident_bytes: 3 << 30,
        device_bytes: 2 << 30,
        host_bytes: 1 << 30,
        ..Default::default()
    };
    let mut same = older.clone();
    same.per_device.clear();
    assert_eq!(older.encode_to_vec(), same.encode_to_vec());
    let split = pb::ProcessResidency {
        per_device: vec![
            pb::DeviceResidency {
                device_id: "gpu0".into(),
                bytes: 3 << 29,
            },
            pb::DeviceResidency {
                device_id: "gpu1".into(),
                bytes: 1 << 29,
            },
        ],
        ..older.clone()
    };
    assert_eq!(
        pb::ProcessResidency::decode(split.encode_to_vec().as_slice()).unwrap(),
        split
    );
}

// T34 T26 (SPEC §7.2): `memory_source` is additive and status only: an
// observation without it encodes exactly as an older host's, and one with it
// decodes back unchanged (an older server skips the unknown field).
#[test]
fn the_memory_source_is_an_additive_field() {
    let older = pb::DomainObservation {
        domain_id: "unified".into(),
        kind: "system".into(),
        capacity_bytes: 128 << 30,
        available_bytes: 100 << 30,
        ..Default::default()
    };
    let mut same = older.clone();
    same.memory_source.clear();
    assert_eq!(older.encode_to_vec(), same.encode_to_vec());
    let bounded = pb::DomainObservation {
        memory_source: "cgroup_v2:/system.slice/docker-1.scope".into(),
        ..older
    };
    let decoded = pb::DomainObservation::decode(bounded.encode_to_vec().as_slice()).unwrap();
    assert_eq!(decoded, bounded);
}

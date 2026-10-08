//! SPEC §13.3 / T21: the EngineLogTail member action. Additive: a new oneof
//! field number (17 in ExecuteMember) and a new result field (18 in
//! MemberExecutionResult), so every command journaled before it keeps its
//! canonical digest. It names a launch by its incarnation, never a path, and
//! its result is nothing but bounded tail evidence.
use capyctl_protocol::capabilities::{
    agent_capabilities, drain_only_permits, refusal, required, ENGINE_LOG_TAIL,
};
use capyctl_protocol::execution::{
    validate_result, EngineLogTailPlan, MemberAction, MemberCommand, MAX_ENGINE_LOG_TAIL_BYTES,
};
use capyctl_protocol::{pb, COMMAND_ENCODING_VERSION};
use std::collections::BTreeSet;

fn identity() -> pb::CommandIdentity {
    pb::CommandIdentity {
        controller_id: "controller".into(),
        host_id: "host".into(),
        member_id: "head".into(),
        deployment_id: "deployment".into(),
        operation_id: "op".into(),
        command_id: "tail".into(),
        step_id: "tail".into(),
        generation: 1,
        revision: 1,
        deadline_unix_ms: 100,
        payload_digest: vec![1; 32],
        expected_state: "engine_log".into(),
        profile_fingerprint: "engine_log".into(),
        protocol_version: COMMAND_ENCODING_VERSION.into(),
        instance_index: 0,
    }
}

fn wire(incarnation: &str, max_bytes: u32) -> pb::ExecuteMember {
    pb::ExecuteMember {
        identity: Some(identity()),
        action: Some(pb::execute_member::Action::EngineLogTail(
            pb::EngineLogTailRequest {
                incarnation: incarnation.into(),
                max_bytes,
            },
        )),
        restore_checkpoint_digest: String::new(),
        terminate_recorded_processes: Vec::new(),
        group_member_launch: None,
        probe_max_tokens: 0,
    }
}

fn decode(wire: pb::ExecuteMember) -> Result<MemberCommand, ()> {
    MemberCommand::try_from(pb::ServerToAgent {
        msg: Some(pb::server_to_agent::Msg::ExecuteMember(wire)),
    })
    .map_err(|_| ())
}

const INCARNATION: &str = "01K00000000000000000000002";

// T21 T34: the action round-trips, and only a bounded request naming an
// incarnation (never a path) decodes.
#[test]
fn engine_log_tail_round_trips_and_names_an_incarnation_only() {
    let command = decode(wire(INCARNATION, 64 * 1024)).expect("decodes");
    assert_eq!(
        command.action,
        MemberAction::EngineLogTail(EngineLogTailPlan {
            incarnation: INCARNATION.into(),
            max_bytes: 64 * 1024,
        })
    );
    assert_eq!(decode(command.to_wire()).unwrap(), command);
    assert!(decode(wire(INCARNATION, MAX_ENGINE_LOG_TAIL_BYTES)).is_ok());
    for (incarnation, max_bytes) in [
        (INCARNATION, 0),
        (INCARNATION, MAX_ENGINE_LOG_TAIL_BYTES + 1),
        ("", 1024),
        ("../etc/passwd", 1024),
        ("a/b", 1024),
        ("01K0000000000000000000000.", 1024),
    ] {
        assert!(
            decode(wire(incarnation, max_bytes)).is_err(),
            "{incarnation:?} {max_bytes}"
        );
    }
}

fn result(
    command: &MemberCommand,
    evidence: pb::EngineLogTailEvidence,
) -> pb::MemberExecutionResult {
    pb::MemberExecutionResult {
        identity: command.to_wire().identity,
        state: "completed".into(),
        observed_at_unix_ms: 1,
        engine_log: Some(evidence),
        ..Default::default()
    }
}

fn evidence(state: &str, text: &str, truncated: bool) -> pb::EngineLogTailEvidence {
    pb::EngineLogTailEvidence {
        state: state.into(),
        text: text.into(),
        truncated,
    }
}

// T21: a result carries only tail evidence, never more text than the plan
// asked for, and a failed read carries no text.
#[test]
fn tail_evidence_is_bounded_by_its_plan() {
    let command = decode(wire(INCARNATION, 16)).unwrap();
    for ok in [
        evidence("served", "0123456789abcdef", true),
        evidence("served", "", false),
        evidence("missing", "", false),
        evidence("raw", "", false),
        evidence("unreadable", "", false),
    ] {
        validate_result(&command, &result(&command, ok.clone()))
            .unwrap_or_else(|_| panic!("{ok:?}"));
    }
    for bad in [
        evidence("served", "0123456789abcdefg", false),
        evidence("raw", "line\n", false),
        evidence("missing", "", true),
        evidence("sent", "", false),
    ] {
        assert!(
            validate_result(&command, &result(&command, bad.clone())).is_err(),
            "{bad:?}"
        );
    }
    // No evidence, or evidence beside anything else, is refused.
    let mut none = result(&command, evidence("served", "", false));
    none.engine_log = None;
    assert!(validate_result(&command, &none).is_err());
    let mut with_binding = result(&command, evidence("served", "", false));
    with_binding.binding_id = "binding".into();
    assert!(validate_result(&command, &with_binding).is_err());
    let mut pending = result(&command, evidence("served", "", false));
    pending.state = "accepted".into();
    assert!(validate_result(&command, &pending).is_err());
    // Tail evidence on any other action's result is refused.
    let mut inspect = decode(wire(INCARNATION, 16)).unwrap();
    inspect.action = MemberAction::Inspect;
    assert!(validate_result(&inspect, &result(&inspect, evidence("served", "", false))).is_err());
}

// T34 (ADR 0017): the action needs `engine_log_tail`; a host that did not
// declare it is refused typed before anything is sent. A drain-only host may
// still be asked: the read changes nothing.
#[test]
fn engine_log_tail_needs_its_capability() {
    let wire = wire(INCARNATION, 1024);
    assert_eq!(required(&wire), vec![ENGINE_LOG_TAIL]);
    assert!(agent_capabilities().contains(&ENGINE_LOG_TAIL.to_owned()));
    assert_eq!(
        refusal(false, &BTreeSet::new(), &wire).as_deref(),
        Some("host_capability_missing:engine_log_tail")
    );
    let declared = BTreeSet::from([ENGINE_LOG_TAIL.to_owned()]);
    assert_eq!(refusal(false, &declared, &wire), None);
    assert!(drain_only_permits(&wire));
    assert_eq!(refusal(true, &declared, &wire), None);
}

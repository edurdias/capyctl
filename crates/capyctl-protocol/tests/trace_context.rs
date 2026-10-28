//! ADR 0037 §6: a lifecycle command's trace context rides the command
//! envelope (`ExecuteMember.trace_parent`), outside the command: the
//! canonical digest, the typed command a host journals and its replay never
//! carry it. Only a host declaring `trace_context` is sent one.
use capyctl_protocol::capabilities::{
    agent_capabilities, refusal, required, CATALOGUE, TRACE_CONTEXT,
};
use capyctl_protocol::execution::{is_traceparent, MemberCommand, TracedCommand};
use capyctl_protocol::{pb, COMMAND_ENCODING_VERSION};
use prost::Message;
use std::collections::BTreeSet;

const PARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

fn signed() -> MemberCommand {
    let mut command = MemberCommand::try_from(pb::ServerToAgent {
        msg: Some(pb::server_to_agent::Msg::ExecuteMember(pb::ExecuteMember {
            identity: Some(pb::CommandIdentity {
                controller_id: "controller".into(),
                host_id: "host".into(),
                member_id: "head".into(),
                deployment_id: "deployment".into(),
                operation_id: "wake".into(),
                command_id: "wake".into(),
                step_id: "wake".into(),
                generation: 3,
                revision: 2,
                deadline_unix_ms: 100,
                payload_digest: vec![1; 32],
                expected_state: "parked".into(),
                profile_fingerprint: "profile".into(),
                protocol_version: COMMAND_ENCODING_VERSION.into(),
                instance_index: 0,
            }),
            action: Some(pb::execute_member::Action::RestoreOwnedHandle(
                "launch".into(),
            )),
            ..Default::default()
        })),
    })
    .expect("decodes");
    command.identity.payload_digest = command.canonical_digest();
    command
}

fn traced(wire: pb::ExecuteMember) -> TracedCommand {
    TracedCommand::try_from(pb::ServerToAgent {
        msg: Some(pb::server_to_agent::Msg::ExecuteMember(wire)),
    })
    .expect("decodes")
}

// T48 (ADR 0037 §6, SPEC §13.1): the envelope carries the context while the
// typed command, its journaled encoding and its canonical digest are exactly
// those of the same command sent without one.
#[test]
fn the_envelope_carries_the_context_and_the_command_is_unchanged() {
    let command = signed();
    let plain = command.to_wire();
    assert!(plain.trace_parent.is_empty(), "a command never carries one");
    let mut wire = plain.clone();
    wire.trace_parent = PARENT.into();
    let received = traced(wire.clone());
    assert_eq!(received.trace_parent.as_deref(), Some(PARENT));
    assert_eq!(received.command, command);
    // What a host journals is the typed command's encoding: no context.
    assert_eq!(
        received.command.to_wire().encode_to_vec(),
        plain.encode_to_vec()
    );
    assert_eq!(
        received.command.canonical_digest(),
        command.canonical_digest()
    );
    received
        .command
        .verify_digest()
        .expect("the digest verifies");
    // The plain decoder ignores the field: an older host acts the same.
    let older = MemberCommand::try_from(pb::ServerToAgent {
        msg: Some(pb::server_to_agent::Msg::ExecuteMember(wire)),
    })
    .unwrap();
    assert_eq!(older, command);
    older
        .verify_digest()
        .expect("an older host verifies it too");
    // A replay (decoded from the journal's encoding) carries none.
    let replayed = traced(pb::ExecuteMember::decode(plain.encode_to_vec().as_slice()).unwrap());
    assert_eq!(replayed.trace_parent, None);
}

// T48 (ADR 0037 §2, §6): a malformed context is ignored, never an error.
#[test]
fn a_malformed_context_is_ignored() {
    assert!(is_traceparent(PARENT));
    for bad in [
        "",
        "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
        "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
        "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
        "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
        "00-4bf92f3577b34da6a3ce929d0e0e4736_00f067aa0ba902b7-01",
        "not a traceparent",
    ] {
        assert!(!is_traceparent(bad), "{bad:?}");
        let mut wire = signed().to_wire();
        wire.trace_parent = bad.into();
        let received = traced(wire);
        assert_eq!(received.trace_parent, None, "{bad:?}");
        assert_eq!(received.command, signed());
    }
}

// T48 (ADR 0017, ADR 0037 §6): every current host declares `trace_context`;
// the context never makes a command need it (a host without it is sent the
// command without one, never refused).
#[test]
fn trace_context_is_declared_and_never_required() {
    assert_eq!(TRACE_CONTEXT, "trace_context");
    assert!(agent_capabilities().contains(&TRACE_CONTEXT.to_owned()));
    assert!(CATALOGUE.iter().any(|(name, _)| *name == TRACE_CONTEXT));
    let mut wire = signed().to_wire();
    wire.trace_parent = PARENT.into();
    assert!(!required(&wire).contains(&TRACE_CONTEXT));
    assert_eq!(refusal(false, &BTreeSet::new(), &wire), None);
}

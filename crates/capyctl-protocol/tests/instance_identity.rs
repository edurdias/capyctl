//! ADR 0013 §5 (per-instance host fencing): a command names the deployment
//! instance it belongs to. The field is additive: instance 0 is never
//! encoded, so every command journaled before it keeps its exact bytes and
//! digest, and a peer that cannot read a nonzero index fails closed on the
//! digest instead of fencing the command as another instance.
use capyctl_protocol::execution::MemberCommand;
use capyctl_protocol::{pb, COMMAND_ENCODING_VERSION};
use prost::Message;

fn wire(instance_index: u32) -> pb::ExecuteMember {
    pb::ExecuteMember {
        identity: Some(pb::CommandIdentity {
            controller_id: "controller".into(),
            host_id: "host".into(),
            member_id: "head".into(),
            deployment_id: "model".into(),
            operation_id: "op".into(),
            command_id: "command".into(),
            step_id: "step".into(),
            generation: 3,
            revision: 1,
            deadline_unix_ms: 100,
            payload_digest: vec![1; 32],
            expected_state: "retained".into(),
            profile_fingerprint: "pinned".into(),
            protocol_version: COMMAND_ENCODING_VERSION.into(),
            instance_index,
        }),
        action: Some(pb::execute_member::Action::Inspect(true)),
        restore_checkpoint_digest: String::new(),
        terminate_recorded_processes: Vec::new(),
        group_member_launch: None,
        probe_max_tokens: 0,
    }
}

fn decode(message: pb::ExecuteMember) -> Result<MemberCommand, ()> {
    MemberCommand::try_from(pb::ServerToAgent {
        msg: Some(pb::server_to_agent::Msg::ExecuteMember(message)),
    })
    .map_err(|_| ())
}

/// Instance 0 encodes exactly as before the field existed; a nonzero index
/// round-trips and is bound by the payload digest; an index beyond the
/// 64-instance bound is refused.
// T33 T34 T13
#[test]
fn the_instance_is_additive_digest_bound_and_bounded() {
    let zero = wire(0).identity.unwrap().encode_to_vec();
    let one = wire(1).identity.unwrap().encode_to_vec();
    assert_eq!(one.len(), zero.len() + 2, "only a nonzero index is encoded");
    assert_eq!(&one[..zero.len()], zero.as_slice());

    let mut command = decode(wire(1)).unwrap();
    assert_eq!(command.identity.instance_index, 1);
    command.identity.payload_digest = command.canonical_digest();
    command.verify_digest().unwrap();
    assert_eq!(
        decode(command.to_wire()).unwrap(),
        command,
        "the index survives the journal's encoding"
    );
    let mut instance_zero = command.clone();
    instance_zero.identity.instance_index = 0;
    assert_ne!(instance_zero.canonical_digest(), command.canonical_digest());
    // A peer that drops the field sees another command than the one signed.
    let mut dropped = command.to_wire();
    dropped.identity.as_mut().unwrap().instance_index = 0;
    assert!(decode(dropped).unwrap().verify_digest().is_err());

    assert!(decode(wire(63)).is_ok());
    assert!(decode(wire(64)).is_err());
}

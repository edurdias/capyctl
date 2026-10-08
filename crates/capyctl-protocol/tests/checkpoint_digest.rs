//! ADR 0014 §7 (WE3): the DigestCheckpoint member action and the recorded
//! checkpoint a launch plan carries. Protocol version 2, additive: the new
//! action and fields use new field numbers, so every command journaled before
//! them keeps its canonical digest.
use capyctl_protocol::execution::{
    validate_result, DigestCheckpointPlan, MemberAction, MemberCommand, SingleLaunchPlan,
};
use capyctl_protocol::{pb, COMMAND_ENCODING_VERSION};
use prost::Message;

const DIGEST: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
const OTHER: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

fn deployment() -> String {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/f2-deployment.json"
    ))
    .unwrap();
    fixture["deployment"].to_string()
}

fn identity() -> pb::CommandIdentity {
    pb::CommandIdentity {
        controller_id: "controller".into(),
        host_id: "host".into(),
        member_id: "head".into(),
        deployment_id: "deployment".into(),
        operation_id: "op".into(),
        command_id: "digest".into(),
        step_id: "digest".into(),
        generation: 1,
        revision: 1,
        deadline_unix_ms: 100,
        payload_digest: vec![1; 32],
        expected_state: "checkpoint".into(),
        profile_fingerprint: "build".into(),
        protocol_version: COMMAND_ENCODING_VERSION.into(),
        instance_index: 0,
    }
}

fn decode(action: pb::execute_member::Action) -> Result<MemberCommand, ()> {
    MemberCommand::try_from(pb::ServerToAgent {
        msg: Some(pb::server_to_agent::Msg::ExecuteMember(pb::ExecuteMember {
            identity: Some(identity()),
            action: Some(action),
            restore_checkpoint_digest: String::new(),
            terminate_recorded_processes: Vec::new(),
            group_member_launch: None,
            probe_max_tokens: 0,
        })),
    })
    .map_err(|_| ())
}

fn request(expected: &str) -> pb::DigestCheckpointRequest {
    pb::DigestCheckpointRequest {
        deployment_config: deployment(),
        host_policy_fingerprint: "a".repeat(64),
        expected_digest: expected.into(),
        size_only: false,
    }
}

fn digest_command(expected: &str) -> MemberCommand {
    let mut command = decode(pb::execute_member::Action::DigestCheckpoint(request(
        expected,
    )))
    .unwrap();
    command.identity.payload_digest = command.canonical_digest();
    command
}

fn launch_plan() -> pb::SingleLaunchPlan {
    pb::SingleLaunchPlan {
        deployment_config: deployment(),
        profile_name: "local".into(),
        checkpoint_fingerprint: "sha256:model".into(),
        host_policy_fingerprint: "b".repeat(64),
        binding_id: "01K00000000000000000000001".into(),
        incarnation: "01K00000000000000000000002".into(),
        grant_id: "01K00000000000000000000003".into(),
        service_port: 8100,
        issued_at_unix_ms: 1,
        coordinator_session_id: "01K00000000000000000000004".into(),
        checkpoint_digest: String::new(),
        checkpoint_weights_bytes: None,
        checkpoint_state_slot_bytes: None,
        checkpoint_layout: None,
        startup_bytes: None,
    }
}

fn evidence(state: &str, digest: &str) -> pb::CheckpointDigestEvidence {
    pb::CheckpointDigestEvidence {
        state: state.into(),
        digest: digest.into(),
        weights_bytes: 10,
        file_count: 3,
        total_bytes: 12,
        reason: String::new(),
        full_rehash: true,
        state_slot_bytes: None,
        layout: None,
    }
}

fn result(
    command: &MemberCommand,
    evidence: pb::CheckpointDigestEvidence,
) -> pb::MemberExecutionResult {
    pb::MemberExecutionResult {
        identity: command.to_wire().identity,
        state: "completed".into(),
        observed_at_unix_ms: 5,
        checkpoint: Some(evidence),
        ..Default::default()
    }
}

// T34 T37: DigestCheckpoint survives the wire, is digest-bound to its
// deployment and expectation, and is refused when malformed.
#[test]
fn digest_checkpoint_roundtrips_and_is_digest_bound() {
    for expected in ["", DIGEST] {
        let command = digest_command(expected);
        command.verify_digest().unwrap();
        let bytes = pb::ServerToAgent {
            msg: Some(pb::server_to_agent::Msg::ExecuteMember(command.to_wire())),
        }
        .encode_to_vec();
        let decoded =
            MemberCommand::try_from(pb::ServerToAgent::decode(bytes.as_slice()).unwrap()).unwrap();
        assert_eq!(decoded, command);
        let MemberAction::DigestCheckpoint(plan) = &decoded.action else {
            panic!("not a digest command");
        };
        assert_eq!(
            plan.expected_digest.as_deref(),
            (!expected.is_empty()).then_some(expected)
        );
        let mut moved = command.clone();
        moved.action = MemberAction::DigestCheckpoint(DigestCheckpointPlan {
            expected_digest: Some(OTHER.into()),
            ..plan.clone()
        });
        assert!(
            moved.verify_digest().is_err(),
            "the expectation is digest-bound"
        );
    }
    let refused: &[fn(&mut pb::DigestCheckpointRequest)] = &[
        |r| r.expected_digest = "sha256:model".into(),
        |r| r.expected_digest = DIGEST.to_uppercase(),
        |r| r.host_policy_fingerprint = "a".repeat(63),
        |r| r.host_policy_fingerprint = "A".repeat(64),
        |r| r.deployment_config = "{".into(),
        |r| r.deployment_config = "x".repeat(24 * 1024 + 1),
    ];
    for edit in refused {
        let mut wire = request("");
        edit(&mut wire);
        assert!(decode(pb::execute_member::Action::DigestCheckpoint(wire)).is_err());
    }
}

// T34: a launch plan carries the recorded digest and the weights the server
// resolved with; both are digest-bound, and a plan journaled before WE3 (no
// such fields) keeps exactly its canonical digest.
#[test]
fn a_launch_plan_carries_the_recorded_checkpoint_additively() {
    let legacy = decode(pb::execute_member::Action::LaunchSingle(launch_plan())).unwrap();
    let MemberAction::LaunchSingle(plan) = &legacy.action else {
        panic!("not a launch");
    };
    assert!(plan.checkpoint_digest.is_empty());
    assert_eq!(plan.checkpoint_weights_bytes, None);
    // Encoding a plan without the new fields is byte-identical to the old
    // message, so a journaled command's digest still verifies.
    let old = pb::SingleLaunchPlan { ..launch_plan() }.encode_to_vec();
    let MemberAction::LaunchSingle(typed) = &legacy.action else {
        unreachable!()
    };
    let reencoded = match legacy.to_wire().action.unwrap() {
        pb::execute_member::Action::LaunchSingle(plan) => plan.encode_to_vec(),
        _ => unreachable!(),
    };
    assert_eq!(old, reencoded);
    let recorded = SingleLaunchPlan {
        checkpoint_digest: DIGEST.into(),
        checkpoint_weights_bytes: Some(4 << 30),
        checkpoint_state_slot_bytes: None,
        startup_bytes: None,
        ..typed.clone()
    };
    let mut command = legacy.clone();
    command.action = MemberAction::LaunchSingle(recorded.clone());
    command.identity.payload_digest = command.canonical_digest();
    command.verify_digest().unwrap();
    let mut moved = command.clone();
    moved.action = MemberAction::LaunchSingle(SingleLaunchPlan {
        checkpoint_digest: OTHER.into(),
        ..recorded.clone()
    });
    assert!(moved.verify_digest().is_err());
    let refused: &[fn(&mut pb::SingleLaunchPlan)] = &[
        |p| p.checkpoint_digest = "sha256:model".into(),
        |p| {
            p.checkpoint_digest = DIGEST.into();
            p.checkpoint_weights_bytes = Some(-1);
        },
        // Weights without a digest name nothing the host could verify.
        |p| p.checkpoint_weights_bytes = Some(1),
        // ADR 0014 amendment A16: likewise a state slot, and never zero.
        |p| p.checkpoint_state_slot_bytes = Some(1),
        |p| {
            p.checkpoint_digest = DIGEST.into();
            p.checkpoint_state_slot_bytes = Some(0);
        },
    ];
    for edit in refused {
        let mut plan = launch_plan();
        edit(&mut plan);
        assert!(decode(pb::execute_member::Action::LaunchSingle(plan)).is_err());
    }
}

// T34: a digest result is only digest evidence, completed, with no process,
// claim or usable model; computed matches any expectation and mismatch differs
// from a stated one; a refusal carries only a closed reason.
#[test]
fn digest_results_carry_only_bounded_checkpoint_evidence() {
    let open = digest_command("");
    let expecting = digest_command(DIGEST);
    validate_result(&open, &result(&open, evidence("computed", DIGEST))).unwrap();
    validate_result(
        &expecting,
        &result(&expecting, evidence("computed", DIGEST)),
    )
    .unwrap();
    validate_result(&expecting, &result(&expecting, evidence("mismatch", OTHER))).unwrap();
    // ADR 0014 amendment A16: a measured hybrid state slot rides along.
    let with_state = pb::CheckpointDigestEvidence {
        state_slot_bytes: Some(10 << 20),
        ..evidence("computed", DIGEST)
    };
    validate_result(&open, &result(&open, with_state)).unwrap();
    let refusal = pb::CheckpointDigestEvidence {
        state: "refused".into(),
        reason: "unsafe_file".into(),
        ..Default::default()
    };
    validate_result(&open, &result(&open, refusal.clone())).unwrap();
    let invalid: &[(&MemberCommand, pb::CheckpointDigestEvidence)] = &[
        (&expecting, evidence("computed", OTHER)),
        (&expecting, evidence("mismatch", DIGEST)),
        (&open, evidence("mismatch", DIGEST)),
        (&open, evidence("computed", "sha256:short")),
        (&open, evidence("unknown", DIGEST)),
        (
            &open,
            pb::CheckpointDigestEvidence {
                weights_bytes: -1,
                ..evidence("computed", DIGEST)
            },
        ),
        (
            &open,
            pb::CheckpointDigestEvidence {
                weights_bytes: 13,
                ..evidence("computed", DIGEST)
            },
        ),
        (
            &open,
            pb::CheckpointDigestEvidence {
                reason: "x".into(),
                ..evidence("computed", DIGEST)
            },
        ),
        (
            &open,
            pb::CheckpointDigestEvidence {
                state_slot_bytes: Some(0),
                ..evidence("computed", DIGEST)
            },
        ),
        (
            &open,
            pb::CheckpointDigestEvidence {
                state_slot_bytes: Some(1),
                ..refusal.clone()
            },
        ),
        (
            &open,
            pb::CheckpointDigestEvidence {
                reason: "a path".into(),
                ..refusal.clone()
            },
        ),
        (
            &open,
            pb::CheckpointDigestEvidence {
                digest: DIGEST.into(),
                ..refusal.clone()
            },
        ),
    ];
    for (command, evidence) in invalid {
        assert!(
            validate_result(command, &result(command, evidence.clone())).is_err(),
            "{evidence:?}"
        );
    }
    let shapes: &[fn(&mut pb::MemberExecutionResult)] = &[
        |r| r.state = "launched".into(),
        |r| r.claim_retained = true,
        |r| r.model_usable = true,
        |r| r.owned_handle = "launch".into(),
        |r| r.binding_id = "binding".into(),
        |r| r.checkpoint = None,
        |r| {
            r.processes.push(pb::OwnedProcessObservation {
                role: "api".into(),
                pid: 1,
                boot_id: "boot".into(),
                start_ticks: 1,
                presence: "alive".into(),
            })
        },
    ];
    for edit in shapes {
        let mut refused = result(&open, evidence("computed", DIGEST));
        edit(&mut refused);
        assert!(validate_result(&open, &refused).is_err(), "{refused:?}");
    }
}

// T03 T34 (ADR 0028 §5, amendment of 2026-10-07): the checkpoint layout rides
// beside the weights it splits, in range, on the evidence and on a launch
// plan, and a plan naming one needs `engine_groups`.
#[test]
fn the_checkpoint_layout_rides_beside_the_weights() {
    let layout = pb::CheckpointLayout {
        sharded_bytes: 8,
        layer_count: 2,
        largest_layer_bytes: 4,
    };
    let open = digest_command("");
    let with_layout = pb::CheckpointDigestEvidence {
        layout: Some(layout),
        ..evidence("computed", DIGEST)
    };
    validate_result(&open, &result(&open, with_layout)).unwrap();
    let out_of_range = [
        pb::CheckpointLayout {
            sharded_bytes: 11,
            ..layout
        },
        pb::CheckpointLayout {
            largest_layer_bytes: 9,
            ..layout
        },
        pb::CheckpointLayout {
            layer_count: 0,
            ..layout
        },
        pb::CheckpointLayout {
            sharded_bytes: -1,
            ..layout
        },
    ];
    for bad in out_of_range {
        let evidence = pb::CheckpointDigestEvidence {
            layout: Some(bad),
            ..evidence("computed", DIGEST)
        };
        assert!(validate_result(&open, &result(&open, evidence)).is_err());
    }
    let refusal = pb::CheckpointDigestEvidence {
        state: "refused".into(),
        reason: "unsafe_file".into(),
        layout: Some(layout),
        ..Default::default()
    };
    assert!(validate_result(&open, &result(&open, refusal)).is_err());

    let plan = pb::SingleLaunchPlan {
        checkpoint_digest: DIGEST.into(),
        checkpoint_weights_bytes: Some(10),
        checkpoint_layout: Some(layout),
        ..launch_plan()
    };
    let command = decode(pb::execute_member::Action::LaunchSingle(plan.clone())).unwrap();
    let MemberAction::LaunchSingle(typed) = &command.action else {
        panic!("not a launch");
    };
    assert_eq!(
        typed.checkpoint_layout,
        Some(capyctl_domain::member_weights::CheckpointLayout {
            sharded_bytes: 8,
            layer_count: 2,
            largest_layer_bytes: 4,
        })
    );
    assert!(capyctl_protocol::capabilities::required(&command.to_wire())
        .contains(&capyctl_protocol::capabilities::ENGINE_GROUPS));
    let refused = [
        pb::SingleLaunchPlan {
            checkpoint_weights_bytes: None,
            ..plan.clone()
        },
        pb::SingleLaunchPlan {
            checkpoint_layout: Some(pb::CheckpointLayout {
                layer_count: 0,
                ..layout
            }),
            ..plan.clone()
        },
    ];
    for plan in refused {
        assert!(decode(pb::execute_member::Action::LaunchSingle(plan)).is_err());
    }
}

// T34: checkpoint evidence on any other action is refused.
#[test]
fn checkpoint_evidence_is_refused_on_other_actions() {
    let mut inspect = decode(pb::execute_member::Action::Inspect(true)).unwrap();
    inspect.identity.payload_digest = inspect.canonical_digest();
    let mut other = result(&inspect, evidence("computed", DIGEST));
    assert!(validate_result(&inspect, &other).is_err());
    other.checkpoint = None;
    validate_result(&inspect, &other).unwrap();
}

/// SPEC §13 (WE3 limit 1): a launch the host's policy refused before any
/// effect is a terminal result, not a session failure. It completes with no
/// claim, no process and no usable model, names the launch it refused, and
/// carries only a closed reason; a refusal on any other shape is refused.
// T14 T20 T34
#[test]
fn a_policy_refused_launch_is_terminal_bounded_evidence() {
    let mut command = decode(pb::execute_member::Action::LaunchSingle(launch_plan())).unwrap();
    command.identity.expected_state = "reserved".into();
    command.identity.payload_digest = command.canonical_digest();
    let refused = pb::MemberExecutionResult {
        identity: command.to_wire().identity,
        state: "completed".into(),
        owned_handle: command.identity.command_id.clone(),
        observed_at_unix_ms: 5,
        refused: "checkpoint_mismatch".into(),
        ..Default::default()
    };
    validate_result(&command, &refused).unwrap();
    let malformed: &[fn(&mut pb::MemberExecutionResult)] = &[
        |r| r.refused = "digest_changed".into(),
        |r| r.state = "accepted".into(),
        |r| r.claim_retained = true,
        |r| r.owned_handle = "other".into(),
        |r| {
            r.processes = vec![pb::OwnedProcessObservation {
                role: "api".into(),
                pid: 10,
                boot_id: "boot".into(),
                start_ticks: 7,
                presence: "gone".into(),
            }]
        },
    ];
    for edit in malformed {
        let mut result = refused.clone();
        edit(&mut result);
        assert!(validate_result(&command, &result).is_err(), "{result:?}");
    }
    // A digest command never carries a policy refusal.
    let digest = digest_command("");
    let mut on_digest = result(&digest, evidence("computed", DIGEST));
    on_digest.refused = "unauthorized".into();
    assert!(validate_result(&digest, &on_digest).is_err());
}

/// Owner decision 2026-09-23 (solo first start): a size-only request is
/// answered `sized`, with the weights and no digest, so the startup estimate is
/// known before a first start; it is never answered with a hash, and a full
/// request is never answered `sized`.
// T34
#[test]
fn a_size_only_request_is_answered_with_weights_and_no_digest() {
    let mut wire = request("");
    wire.size_only = true;
    let mut sizing = decode(pb::execute_member::Action::DigestCheckpoint(wire)).unwrap();
    sizing.identity.payload_digest = sizing.canonical_digest();
    let MemberAction::DigestCheckpoint(plan) = &sizing.action else {
        panic!("not a digest command");
    };
    assert!(plan.size_only);
    let sized = pb::CheckpointDigestEvidence {
        state: "sized".into(),
        weights_bytes: 10,
        file_count: 2,
        total_bytes: 12,
        ..Default::default()
    };
    validate_result(&sizing, &result(&sizing, sized.clone())).unwrap();
    // The flag is part of the command's identity.
    let mut full = sizing.clone();
    full.action = MemberAction::DigestCheckpoint(DigestCheckpointPlan {
        size_only: false,
        ..plan.clone()
    });
    assert!(full.verify_digest().is_err());
    let open = digest_command("");
    for (command, evidence) in [
        (&sizing, evidence("computed", DIGEST)),
        (
            &sizing,
            pb::CheckpointDigestEvidence {
                digest: DIGEST.into(),
                ..sized.clone()
            },
        ),
        (
            &sizing,
            pb::CheckpointDigestEvidence {
                weights_bytes: 13,
                ..sized.clone()
            },
        ),
        (&open, sized.clone()),
    ] {
        assert!(
            validate_result(command, &result(command, evidence.clone())).is_err(),
            "{evidence:?}"
        );
    }
}

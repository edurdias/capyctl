use capyctl_protocol::{execution::MemberCommand, pb};
use prost::Message;
fn command() -> pb::ServerToAgent {
    pb::ServerToAgent {
        msg: Some(pb::server_to_agent::Msg::ExecuteMember(pb::ExecuteMember {
            identity: Some(pb::CommandIdentity {
                controller_id: "controller".into(),
                host_id: "host".into(),
                member_id: "rank-0".into(),
                deployment_id: "model".into(),
                operation_id: "op".into(),
                command_id: "command".into(),
                step_id: "launch".into(),
                generation: 1,
                revision: 1,
                deadline_unix_ms: 100,
                payload_digest: vec![1; 32],
                expected_state: "reserved".into(),
                profile_fingerprint: "pinned".into(),
                protocol_version: capyctl_protocol::COMMAND_ENCODING_VERSION.into(),
                instance_index: 0,
            }),
            action: Some(pb::execute_member::Action::Inspect(true)),
            restore_checkpoint_digest: String::new(),
            terminate_recorded_processes: Vec::new(),
        })),
    }
}
// T37: the wire boundary carries per-step identity and rejects legacy authority.
#[test]
fn typed_command_roundtrip_and_legacy_rejection() {
    let message = command();
    let decoded = pb::ServerToAgent::decode(message.encode_to_vec().as_slice()).unwrap();
    let accepted = MemberCommand::try_from(decoded).unwrap();
    assert_eq!(accepted.identity.command_id, "command");
    assert_eq!(accepted.identity.step_id, "launch");
    let legacy = pb::ServerToAgent {
        msg: Some(pb::server_to_agent::Msg::LaunchMember(pb::LaunchMember {
            rendered_command_json: "{\"argv\":[\"sh\"]}".into(),
            ..Default::default()
        })),
    };
    assert!(MemberCommand::try_from(legacy).is_err());
}
// T37: missing digest and operation-only identity never reach acceptance.
#[test]
fn incomplete_command_identity_is_rejected() {
    for field in ["digest", "step", "generation", "version"] {
        let mut message = command();
        if let Some(pb::server_to_agent::Msg::ExecuteMember(command)) = &mut message.msg {
            let identity = command.identity.as_mut().unwrap();
            match field {
                "digest" => identity.payload_digest.clear(),
                "step" => identity.step_id.clear(),
                "version" => identity.protocol_version = "unsupported".into(),
                _ => identity.generation = 0,
            }
        }
        assert!(MemberCommand::try_from(message).is_err());
    }
}

// T13/T34: all immutable command fields and the action are digest-bound.
#[test]
fn canonical_digest_binds_identity_deadline_and_action() {
    use capyctl_domain::group::CommandIdentity;
    use capyctl_protocol::execution::MemberAction;
    let mut original = MemberCommand::try_from(command()).unwrap();
    original.identity.payload_digest = original.canonical_digest();
    original.verify_digest().unwrap();
    let edits: &[fn(&mut CommandIdentity)] = &[
        |id| id.controller_id.push('x'),
        |id| id.member.host_id.push('x'),
        |id| id.member.member_id.push('x'),
        |id| id.deployment_id.push('x'),
        |id| id.operation_id.push('x'),
        |id| id.command_id.push('x'),
        |id| id.step_id.push('x'),
        |id| id.generation += 1,
        |id| id.revision += 1,
        |id| id.deadline_ms += 1,
        |id| id.expected_state.push('x'),
        |id| id.profile_fingerprint.push('x'),
    ];
    for edit in edits {
        let mut changed = original.clone();
        edit(&mut changed.identity);
        assert!(changed.verify_digest().is_err());
    }
    for action in [
        MemberAction::CloseIngress,
        MemberAction::Terminate {
            owned_handle: "owned".into(),
            recorded: Vec::new(),
        },
    ] {
        let mut changed = original.clone();
        changed.action = action;
        assert!(changed.verify_digest().is_err());
    }
    let wire = original.to_wire();
    let recovered = MemberCommand::try_from(pb::ServerToAgent {
        msg: Some(pb::server_to_agent::Msg::ExecuteMember(
            pb::ExecuteMember::decode(wire.encode_to_vec().as_slice()).unwrap(),
        )),
    })
    .unwrap();
    assert_eq!(recovered, original);
    recovered.verify_digest().unwrap();
}

// T27/T34: member ordering is not a new effect, but rank topology is bound.
#[test]
fn group_digest_is_stable_and_binds_rendezvous() {
    use capyctl_domain::group::{
        member_id, GroupEngine, GroupPlan, GroupTopology, MemberKey, MemberPlan, MemberRole,
    };
    use capyctl_protocol::execution::MemberAction;
    let members: Vec<_> = (0..2)
        .map(|rank| MemberPlan {
            member: MemberKey {
                host_id: if rank == 0 {
                    "host".into()
                } else {
                    "other".into()
                },
                member_id: member_id(rank),
            },
            rank,
            role: if rank == 0 {
                MemberRole::Head
            } else {
                MemberRole::Worker
            },
            profile_name: "sglang".into(),
            profile_fingerprint: "pinned".into(),
            checkpoint_fingerprint: "checkpoint".into(),
            model_path: "/models/m".into(),
            devices: vec!["gpu-0".into()],
            peer_address: format!("192.0.2.{}", rank + 10).parse().unwrap(),
            service_port: (rank == 0).then_some(30000),
            worker_port: (rank > 0).then_some(30001),
        })
        .collect();
    let topology = GroupTopology {
        tensor_parallel: 2,
        pipeline_parallel: 1,
        local_ranks: 1,
    };
    let plan = |members: Vec<MemberPlan>, port: u16| {
        GroupPlan::new(GroupEngine::Sglang, members, topology, port, 1).unwrap()
    };
    let mut original = MemberCommand::try_from(command()).unwrap();
    original.identity.member.member_id = member_id(0);
    original.action = MemberAction::Launch(plan(members.clone(), 29500));
    original.identity.payload_digest = original.canonical_digest();
    original.verify_digest().unwrap();
    // ADR 0028 §4: members are held in rank order, so the digest has no order to ignore.
    let mut reordered = original.clone();
    reordered.action = MemberAction::Launch(plan(members.clone(), 29500));
    assert_eq!(reordered.canonical_digest(), original.canonical_digest());
    reordered.verify_digest().unwrap();
    reordered.action = MemberAction::Launch(plan(members, 29501));
    assert!(reordered.verify_digest().is_err());
}

// T37: a digest never turns a malformed in-process value into a valid command.
#[test]
fn digest_verification_revalidates_typed_shape() {
    let mut invalid = MemberCommand::try_from(command()).unwrap();
    invalid.identity.generation = 0;
    invalid.identity.payload_digest = invalid.canonical_digest();
    assert!(invalid.verify_digest().is_err());
}

// T37: a remote standalone launch has its own closed shape; it never weakens TP2.
#[test]
fn single_host_launch_binds_local_resolution_and_retained_grant() {
    use capyctl_protocol::execution::{MemberAction, SingleLaunchPlan};
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let mut typed = MemberCommand::try_from(command()).unwrap();
    typed.action = MemberAction::LaunchSingle(SingleLaunchPlan {
        deployment_config: fixture["input"]["deployment"].to_string(),
        profile_name: "approved-profile".into(),
        checkpoint_fingerprint: "sha256:model".into(),
        host_policy_fingerprint: "a".repeat(64),
        binding_id: "01K00000000000000000000001".into(),
        incarnation: "01K00000000000000000000002".into(),
        grant_id: "01K00000000000000000000003".into(),
        service_port: 30000,
        issued_at_ms: 1,
        coordinator_session_id: "01K00000000000000000000004".into(),
        checkpoint_digest: String::new(),
        checkpoint_weights_bytes: None,
        checkpoint_state_slot_bytes: None,
        startup_bytes: None,
    });
    typed.identity.payload_digest = typed.canonical_digest();
    typed.verify_digest().unwrap();
    let recovered = MemberCommand::try_from(pb::ServerToAgent {
        msg: Some(pb::server_to_agent::Msg::ExecuteMember(typed.to_wire())),
    })
    .unwrap();
    assert_eq!(recovered, typed);
    let MemberAction::LaunchSingle(plan) = &mut typed.action else {
        panic!()
    };
    plan.grant_id.push('x');
    assert!(typed.verify_digest().is_err());
    typed.identity.payload_digest = typed.canonical_digest();
    assert!(typed.verify_digest().is_err());
}

fn probe(handle: &str) -> MemberCommand {
    use capyctl_protocol::execution::MemberAction;
    let mut command = MemberCommand::try_from(command()).unwrap();
    command.identity.expected_state = "ready".into();
    command.action = MemberAction::Probe {
        owned_handle: handle.into(),
    };
    command.identity.payload_digest = command.canonical_digest();
    command
}

fn ready_result(command: &MemberCommand) -> pb::MemberExecutionResult {
    let process = |role: &str, pid| pb::OwnedProcessObservation {
        role: role.into(),
        pid,
        boot_id: "boot".into(),
        start_ticks: 7,
        presence: "alive".into(),
    };
    pb::MemberExecutionResult {
        identity: command.to_wire().identity,
        state: "completed".into(),
        owned_handle: "launch".into(),
        processes: vec![process("api", 10), process("worker-0", 11)],
        observed_at_unix_ms: 50,
        claim_retained: true,
        model_usable: true,
        binding_id: "binding".into(),
        incarnation: "incarnation".into(),
        residency: None,
        checkpoint: None,
        refused: String::new(),
        launch_failure: String::new(),
        source: None,
        kernel_builds: Vec::new(),
    }
}

// T33 T34 T37: a fresh-probe request names exactly one retained launch. The
// handle is digest-bound, survives the wire, and cannot be empty or oversized.
#[test]
fn probe_names_one_retained_launch_and_is_digest_bound() {
    use capyctl_protocol::execution::MemberAction;
    let command = probe("launch");
    command.verify_digest().unwrap();
    let wire = pb::ServerToAgent {
        msg: Some(pb::server_to_agent::Msg::ExecuteMember(command.to_wire())),
    };
    let decoded = MemberCommand::try_from(
        pb::ServerToAgent::decode(wire.encode_to_vec().as_slice()).unwrap(),
    )
    .unwrap();
    assert_eq!(decoded, command);
    let mut moved = command.clone();
    moved.action = MemberAction::Probe {
        owned_handle: "other-launch".into(),
    };
    assert_ne!(moved.canonical_digest(), command.canonical_digest());
    assert!(moved.verify_digest().is_err());
    for handle in ["", " ", &"x".repeat(4097)] {
        let mut bad = command.clone();
        bad.action = MemberAction::Probe {
            owned_handle: handle.to_string(),
        };
        bad.identity.payload_digest = bad.canonical_digest();
        assert!(bad.verify_digest().is_err(), "{handle:?}");
    }
    // A probe is not an Inspect: the actions digest differently.
    let mut inspect = command.clone();
    inspect.action = MemberAction::Inspect;
    assert_ne!(inspect.canonical_digest(), command.canonical_digest());
}

// T33 T37 (G2): only a probe result for the probed launch, still owned, with its
// api and worker alive and a named binding, may claim a usable model.
#[test]
fn probe_result_claims_readiness_only_for_the_owned_live_group() {
    use capyctl_protocol::execution::validate_result;
    let command = probe("launch");
    let good = ready_result(&command);
    validate_result(&command, &good).unwrap();
    let edits: &[fn(&mut pb::MemberExecutionResult)] = &[
        |r| r.owned_handle = "other".into(),
        |r| r.claim_retained = false,
        |r| r.state = "launched".into(),
        |r| r.binding_id.clear(),
        |r| r.incarnation.clear(),
        |r| r.processes.retain(|p| p.role != "api"),
        |r| r.processes[1].presence = "gone".into(),
    ];
    for edit in edits {
        let mut result = good.clone();
        edit(&mut result);
        assert!(validate_result(&command, &result).is_err(), "{result:?}");
    }
    // T41 (ADR 0023 §6): TensorFold serves from one process, so the api
    // process alone, alive, is a live group.
    let mut single = good.clone();
    single.processes.retain(|p| p.role == "api");
    validate_result(&command, &single).unwrap();
    // An unusable probe answer is ordinary evidence, never a readiness claim.
    let mut unusable = good.clone();
    unusable.model_usable = false;
    unusable.processes[1].presence = "gone".into();
    validate_result(&command, &unusable).unwrap();
    // Readiness remains exclusive to launch and probe results.
    let mut inspect = command.clone();
    inspect.action = capyctl_protocol::execution::MemberAction::Inspect;
    inspect.identity.payload_digest = inspect.canonical_digest();
    let mut claimed = good.clone();
    claimed.identity = inspect.to_wire().identity;
    assert!(validate_result(&inspect, &claimed).is_err());
}

fn launch_single() -> MemberCommand {
    use capyctl_protocol::execution::{MemberAction, SingleLaunchPlan};
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let mut typed = MemberCommand::try_from(command()).unwrap();
    typed.action = MemberAction::LaunchSingle(SingleLaunchPlan {
        deployment_config: fixture["input"]["deployment"].to_string(),
        profile_name: "approved-profile".into(),
        checkpoint_fingerprint: "sha256:model".into(),
        host_policy_fingerprint: "a".repeat(64),
        binding_id: "01K00000000000000000000001".into(),
        incarnation: "01K00000000000000000000002".into(),
        grant_id: "01K00000000000000000000003".into(),
        service_port: 30000,
        issued_at_ms: 1,
        coordinator_session_id: "01K00000000000000000000004".into(),
        checkpoint_digest: String::new(),
        checkpoint_weights_bytes: None,
        checkpoint_state_slot_bytes: None,
        startup_bytes: None,
    });
    typed.identity.payload_digest = typed.canonical_digest();
    typed
}

// T20 T29 (SPEC §§6.4, 13.2): a launch whose engine exited before readiness
// says why in one bounded printable line; nothing else carries that reason,
// and it never claims a usable model.
#[test]
fn a_launch_failure_is_one_bounded_line_on_an_exited_launch_only() {
    use capyctl_protocol::execution::validate_result;
    let command = launch_single();
    let mut result = ready_result(&command);
    result.state = "launched".into();
    result.owned_handle = command.identity.command_id.clone();
    result.model_usable = false;
    for process in &mut result.processes {
        process.presence = "gone".into();
    }
    result.launch_failure =
        "the engine exited before readiness with exit code 2; it rejected argument --moe-backend"
            .into();
    validate_result(&command, &result).unwrap();
    for bad in ["two\nlines", &"x".repeat(257), "tab\there"] {
        let mut refused = result.clone();
        refused.launch_failure = bad.to_string();
        assert!(validate_result(&command, &refused).is_err(), "{bad:?}");
    }
    let mut usable = result.clone();
    usable.model_usable = true;
    assert!(validate_result(&command, &usable).is_err());
    let mut alive = result.clone();
    alive.processes[0].presence = "alive".into();
    assert!(validate_result(&command, &alive).is_err());
    let probe = probe("launch");
    let mut on_probe = ready_result(&probe);
    on_probe.launch_failure = "the engine exited before readiness".into();
    assert!(validate_result(&probe, &on_probe).is_err());
}

// T29 (ADR 0014 amendment A12): kernel build spans ride a usable launch only,
// ordered and bounded; any other result carrying them is refused.
#[test]
fn kernel_builds_ride_a_usable_launch_only() {
    use capyctl_protocol::execution::{validate_result, MAX_KERNEL_BUILDS};
    let span = |from_unix_ms, until_unix_ms| pb::KernelBuildSpan {
        from_unix_ms,
        until_unix_ms,
    };
    let command = launch_single();
    let mut ready = ready_result(&command);
    ready.state = "launched".into();
    ready.owned_handle = command.identity.command_id.clone();
    ready.binding_id = "01K00000000000000000000001".into();
    ready.incarnation = "01K00000000000000000000002".into();
    validate_result(&command, &ready).unwrap();
    ready.kernel_builds = vec![span(10, 20), span(30, 30)];
    validate_result(&command, &ready).unwrap();
    for bad in [
        vec![span(20, 10)],
        vec![span(-1, 10)],
        vec![span(1, 2); MAX_KERNEL_BUILDS + 1],
    ] {
        let mut refused = ready.clone();
        refused.kernel_builds = bad;
        assert!(validate_result(&command, &refused).is_err());
    }
    let mut unusable = ready.clone();
    unusable.model_usable = false;
    assert!(validate_result(&command, &unusable).is_err());
    let probe = probe("launch");
    let mut on_probe = ready_result(&probe);
    on_probe.kernel_builds = vec![span(10, 20)];
    assert!(validate_result(&probe, &on_probe).is_err());
}

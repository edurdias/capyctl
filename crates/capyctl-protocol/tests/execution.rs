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
            group_member_launch: None,
            probe_max_tokens: 0,
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
            checkpoint_fingerprint: CHECKPOINT_DIGEST.into(),
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
    let launch = |plan: GroupPlan| MemberAction::Launch {
        member: group_member_launch("sglang", CHECKPOINT_DIGEST, 30000),
        plan,
    };
    let mut original = MemberCommand::try_from(command()).unwrap();
    original.identity.member.member_id = member_id(0);
    original.action = launch(plan(members.clone(), 29500));
    original.identity.payload_digest = original.canonical_digest();
    original.verify_digest().unwrap();
    // ADR 0028 §4: members are held in rank order, so the digest has no order to ignore.
    let mut reordered = original.clone();
    reordered.action = launch(plan(members.clone(), 29500));
    assert_eq!(reordered.canonical_digest(), original.canonical_digest());
    reordered.verify_digest().unwrap();
    reordered.action = launch(plan(members, 29501));
    assert!(reordered.verify_digest().is_err());
}

/// A checkpoint digest in the canonical form a group plan records.
const CHECKPOINT_DIGEST: &str =
    "sha256:abababababababababababababababababababababababababababababababab";

/// ADR 0028 §8 (ruling R29): one host's launch of its member of a group plan,
/// shaped as a single-rank launch: `port` is the head's service port, or zero
/// on a worker.
fn group_member_launch(
    profile: &str,
    digest: &str,
    port: u16,
) -> capyctl_protocol::execution::SingleLaunchPlan {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    capyctl_protocol::execution::SingleLaunchPlan {
        deployment_config: fixture["input"]["deployment"].to_string(),
        profile_name: profile.into(),
        checkpoint_fingerprint: "sha256:model".into(),
        host_policy_fingerprint: "a".repeat(64),
        binding_id: "01K00000000000000000000001".into(),
        incarnation: "01K00000000000000000000002".into(),
        grant_id: "01K00000000000000000000003".into(),
        service_port: port,
        issued_at_ms: 1,
        coordinator_session_id: "01K00000000000000000000004".into(),
        checkpoint_digest: digest.into(),
        checkpoint_weights_bytes: None,
        checkpoint_state_slot_bytes: None,
        checkpoint_layout: None,
        startup_bytes: None,
    }
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
        checkpoint_layout: None,
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
        max_tokens: None,
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
        escalated: false,
        probe_tokens: Vec::new(),
        engine_log: None,
        probe_text: String::new(),
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
        max_tokens: None,
    };
    assert_ne!(moved.canonical_digest(), command.canonical_digest());
    assert!(moved.verify_digest().is_err());
    for handle in ["", " ", &"x".repeat(4097)] {
        let mut bad = command.clone();
        bad.action = MemberAction::Probe {
            owned_handle: handle.to_string(),
            max_tokens: None,
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
        checkpoint_layout: None,
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

mod groups {
    use super::{command, group_member_launch, MemberCommand, CHECKPOINT_DIGEST};
    use capyctl_domain::group::{
        member_id, GroupEngine, GroupPlan, GroupTopology, MemberKey, MemberPlan, MemberRole,
    };
    use capyctl_protocol::capabilities::{
        agent_capabilities, drain_only_permits, group_refusal, required, CATALOGUE, ENGINE_GROUPS,
    };
    use capyctl_protocol::execution::MemberAction;
    use capyctl_protocol::pb;
    use std::collections::BTreeSet;

    struct Shape {
        engine: GroupEngine,
        topology: GroupTopology,
        generation: i64,
        model_path: &'static str,
        worker_port: u16,
    }
    fn shape(engine: GroupEngine) -> Shape {
        Shape {
            engine,
            topology: GroupTopology {
                tensor_parallel: 2,
                pipeline_parallel: 1,
                local_ranks: 1,
            },
            generation: 1,
            model_path: "/models/m",
            worker_port: 25001,
        }
    }
    fn plan_of(shape: &Shape) -> GroupPlan {
        let members = (0..2)
            .map(|rank| MemberPlan {
                member: MemberKey {
                    host_id: format!("host-{rank}"),
                    member_id: member_id(rank),
                },
                rank,
                role: if rank == 0 {
                    MemberRole::Head
                } else {
                    MemberRole::Worker
                },
                profile_name: "profile".into(),
                profile_fingerprint: "pinned".into(),
                checkpoint_fingerprint: CHECKPOINT_DIGEST.into(),
                model_path: shape.model_path.into(),
                devices: vec!["gpu0".into()],
                peer_address: format!("192.0.2.{}", rank + 10).parse().unwrap(),
                service_port: (rank == 0).then_some(30000),
                worker_port: (rank > 0 && shape.engine == GroupEngine::Sglang)
                    .then_some(shape.worker_port),
            })
            .collect();
        GroupPlan::new(
            shape.engine,
            members,
            shape.topology,
            25000,
            shape.generation,
        )
        .unwrap()
    }
    fn sample_group_plan(engine: GroupEngine) -> GroupPlan {
        plan_of(&shape(engine))
    }
    fn command_with(action: MemberAction) -> MemberCommand {
        let mut command = MemberCommand::try_from(command()).unwrap();
        command.identity.member.host_id = "host-0".into();
        command.identity.member.member_id = member_id(0);
        command.action = action;
        command.identity.payload_digest = command.canonical_digest();
        command
    }
    /// ADR 0028 §8 (ruling R29): the head's Launch of `plan`, with its member
    /// launch.
    fn launch(plan: GroupPlan) -> MemberAction {
        MemberAction::Launch {
            member: group_member_launch("profile", CHECKPOINT_DIGEST, 30000),
            plan,
        }
    }
    fn digest(plan: GroupPlan) -> [u8; 32] {
        command_with(launch(plan)).canonical_digest()
    }

    // T34: group actions round-trip every member field, and the digest binds them.
    #[test]
    fn group_launch_round_trips_new_fields() {
        for engine in [
            GroupEngine::Vllm,
            GroupEngine::Sglang,
            GroupEngine::Tensorfold,
        ] {
            for action in [
                launch(sample_group_plan(engine)),
                MemberAction::Prepare(sample_group_plan(engine)),
            ] {
                let command = command_with(action);
                let wire = command.to_wire();
                let back = MemberCommand::try_from(pb::ServerToAgent {
                    msg: Some(pb::server_to_agent::Msg::ExecuteMember(wire)),
                })
                .unwrap();
                assert_eq!(back, command);
                back.verify_digest().unwrap();
            }
        }
    }

    // T34: each wire field is bound by the payload digest.
    #[test]
    fn group_digest_binds_engine_topology_generation_path_and_port() {
        let base = digest(sample_group_plan(GroupEngine::Sglang));
        let variants = vec![
            digest(sample_group_plan(GroupEngine::Vllm)),
            digest(plan_of(&Shape {
                worker_port: 25002,
                ..shape(GroupEngine::Sglang)
            })),
            digest(plan_of(&Shape {
                generation: 2,
                ..shape(GroupEngine::Sglang)
            })),
            digest(plan_of(&Shape {
                model_path: "/models/other",
                ..shape(GroupEngine::Sglang)
            })),
            // Same members and ranks, different split: tp1 x pp2.
            digest(plan_of(&Shape {
                topology: GroupTopology {
                    tensor_parallel: 1,
                    pipeline_parallel: 2,
                    local_ranks: 1,
                },
                ..shape(GroupEngine::Sglang)
            })),
        ];
        // Only the engine differs.
        assert_ne!(
            digest(sample_group_plan(GroupEngine::Vllm)),
            digest(sample_group_plan(GroupEngine::Tensorfold))
        );
        for variant in &variants {
            assert_ne!(*variant, base);
        }
        let distinct: BTreeSet<_> = variants.iter().collect();
        assert_eq!(distinct.len(), variants.len());
        // Two vLLM plans differing only in topology split differ too.
        assert_ne!(
            digest(sample_group_plan(GroupEngine::Vllm)),
            digest(plan_of(&Shape {
                topology: GroupTopology {
                    tensor_parallel: 1,
                    pipeline_parallel: 2,
                    local_ranks: 1,
                },
                ..shape(GroupEngine::Vllm)
            }))
        );
    }

    fn launch_plan_mut(wire: &mut pb::ExecuteMember) -> &mut pb::GroupLaunchPlan {
        match wire.action.as_mut().unwrap() {
            pb::execute_member::Action::Launch(plan) => plan,
            _ => unreachable!(),
        }
    }
    fn decodes(wire: pb::ExecuteMember) -> bool {
        MemberCommand::try_from(pb::ServerToAgent {
            msg: Some(pb::server_to_agent::Msg::ExecuteMember(wire)),
        })
        .is_ok()
    }

    // T34: a malformed wire plan never becomes a domain plan.
    #[test]
    fn malformed_wire_plan_is_refused() {
        let good = command_with(launch(sample_group_plan(GroupEngine::Sglang))).to_wire();
        assert!(decodes(good.clone()));
        let edits: &[fn(&mut pb::GroupLaunchPlan)] = &[
            |p| p.members[1].rank = 0,
            |p| p.members[1].role = "head".into(),
            |p| p.members[0].role = "worker".into(),
            |p| p.members[1].role = "observer".into(),
            |p| p.members[1].role.clear(),
            |p| p.members[1].model_path.clear(),
            |p| p.members[1].worker_port = 0,
            |p| p.members[0].worker_port = 25001,
            |p| p.members[1].worker_port = 70000,
            |p| p.engine = "triton".into(),
            |p| p.engine.clear(),
            |p| p.tensor_parallel = 0,
            |p| p.pipeline_parallel = 0,
            |p| p.local_ranks = 0,
            |p| p.tensor_parallel = 4,
            |p| p.generation = 0,
        ];
        for (index, edit) in edits.iter().enumerate() {
            let mut wire = good.clone();
            edit(launch_plan_mut(&mut wire));
            assert!(!decodes(wire), "edit {index} was accepted");
        }
    }

    // T34: a host without engine_groups is refused typed; one with it is not.
    #[test]
    fn engine_groups_gates_group_placement() {
        let none = BTreeSet::new();
        assert_eq!(
            group_refusal(&none).as_deref(),
            Some("host_capability_missing:engine_groups")
        );
        let with = BTreeSet::from([ENGINE_GROUPS.to_owned()]);
        assert_eq!(group_refusal(&with), None);
        assert!(agent_capabilities().contains(&ENGINE_GROUPS.to_owned()));
        assert!(CATALOGUE.iter().any(|(name, _)| *name == ENGINE_GROUPS));
        for action in [
            launch(sample_group_plan(GroupEngine::Vllm)),
            MemberAction::Prepare(sample_group_plan(GroupEngine::Vllm)),
        ] {
            assert_eq!(required(&command_with(action).to_wire()), [ENGINE_GROUPS]);
        }
        assert!(required(&command_with(MemberAction::Inspect).to_wire()).is_empty());
    }

    // T34: drain-only hosts may terminate a member but never prepare or launch one.
    #[test]
    fn drain_only_refuses_group_prepare_and_launch() {
        for action in [
            MemberAction::Prepare(sample_group_plan(GroupEngine::Vllm)),
            launch(sample_group_plan(GroupEngine::Vllm)),
        ] {
            assert!(!drain_only_permits(&command_with(action).to_wire()));
        }
        let terminate = MemberAction::Terminate {
            owned_handle: "owned".into(),
            recorded: Vec::new(),
        };
        assert!(drain_only_permits(&command_with(terminate).to_wire()));
    }

    // R11: the inventory carries a peer address and findings, nothing else.
    #[test]
    fn group_inventory_round_trips() {
        use prost::Message;
        let inventory = pb::ReportInventory {
            group: Some(pb::GroupInventory {
                peer_address: "192.0.2.10".into(),
                findings: vec!["rdma_absent".into()],
            }),
            ..Default::default()
        };
        let back = pb::ReportInventory::decode(inventory.encode_to_vec().as_slice()).unwrap();
        assert_eq!(back, inventory);
    }

    // T30, Review Focus 2: a Prepare refusal is one closed group code on a
    // completed result that claims nothing; a policy code, a malformed port
    // or a result claiming anything is not.
    #[test]
    fn prepare_refusal_carries_a_closed_group_code() {
        use capyctl_protocol::execution::{is_prepare_refusal, validate_result};
        let command = command_with(MemberAction::Prepare(sample_group_plan(GroupEngine::Vllm)));
        let result = |refused: &str| pb::MemberExecutionResult {
            identity: command.to_wire().identity,
            state: "completed".into(),
            refused: refused.into(),
            ..Default::default()
        };
        assert!(validate_result(&command, &result("")).is_ok());
        for code in [
            // R35 (ADR 0028 §2): more than one rank per member.
            "group_topology_invalid",
            "group_profile_mismatch",
            "group_checkpoint_mismatch",
            "peer_address_not_local",
            "rendezvous_port_in_use:25000",
            "service_port_in_use:8101",
            "host_tuning_missing:memlock",
            "host_tuning_missing:infiniband",
        ] {
            assert!(is_prepare_refusal(code), "{code}");
            assert!(validate_result(&command, &result(code)).is_ok(), "{code}");
        }
        for code in [
            "unauthorized",
            "rendezvous_port_in_use:0",
            "rendezvous_port_in_use:65536",
            "service_port_in_use:",
            "service_port_in_use:+80",
            "host_tuning_missing:compaction",
            "host_tuning_warning:memlock",
        ] {
            assert!(!is_prepare_refusal(code), "{code}");
            assert!(validate_result(&command, &result(code)).is_err(), "{code}");
        }
        // T30 (ADR 0028 §7): every Prepare result, passed or refused, is
        // completed and effect-free; any state or field claiming more is not.
        type Mutation = fn(&mut pb::MemberExecutionResult);
        let mutations: [(&str, Mutation); 8] = [
            ("accepted", |r| r.state = "accepted".into()),
            ("attempted", |r| r.state = "attempted".into()),
            ("launched", |r| r.state = "launched".into()),
            ("claim_retained", |r| r.claim_retained = true),
            ("processes", |r| {
                r.processes.push(pb::OwnedProcessObservation {
                    pid: 7,
                    start_ticks: 1,
                    role: "api".into(),
                    boot_id: "boot".into(),
                    presence: "alive".into(),
                })
            }),
            ("owned_handle", |r| r.owned_handle = "command".into()),
            ("binding_id", |r| r.binding_id = "binding".into()),
            ("incarnation", |r| r.incarnation = "incarnation".into()),
        ];
        for refused in ["", "group_profile_mismatch"] {
            for (name, mutate) in mutations {
                let mut mutated = result(refused);
                mutate(&mut mutated);
                assert!(
                    validate_result(&command, &mutated).is_err(),
                    "{name} with refused {refused:?}"
                );
            }
        }
    }

    /// The worker's Launch of `plan` on host-1, its member launch naming `port`.
    fn worker_launch(plan: GroupPlan, port: u16) -> MemberCommand {
        let mut command = MemberCommand::try_from(command()).unwrap();
        command.identity.member.host_id = "host-1".into();
        command.identity.member.member_id = member_id(1);
        command.action = MemberAction::Launch {
            member: group_member_launch("profile", CHECKPOINT_DIGEST, port),
            plan,
        };
        command.identity.payload_digest = command.canonical_digest();
        command
    }

    // T30 T34 (ADR 0028 §8, ruling R29): a group Launch carries this host's own
    // member launch, required there and refused anywhere else; it must be this
    // member's (profile, recorded checkpoint, port) and the digest binds it.
    #[test]
    fn group_launch_carries_its_member_launch() {
        let head = command_with(launch(sample_group_plan(GroupEngine::Vllm)));
        head.verify_digest().unwrap();
        // A worker serves nothing: its member launch names no port.
        worker_launch(sample_group_plan(GroupEngine::Vllm), 0)
            .verify_digest()
            .unwrap();
        assert!(worker_launch(sample_group_plan(GroupEngine::Vllm), 30000)
            .verify_digest()
            .is_err());
        let member = head.to_wire().group_member_launch;
        let mut bare = head.to_wire();
        bare.group_member_launch = None;
        assert!(!decodes(bare));
        for other in [
            MemberAction::Prepare(sample_group_plan(GroupEngine::Vllm)),
            MemberAction::Inspect,
        ] {
            let mut wire = command_with(other).to_wire();
            assert!(decodes(wire.clone()));
            wire.group_member_launch = member.clone();
            assert!(!decodes(wire));
        }
        type Edit = fn(&mut pb::SingleLaunchPlan);
        let edits: [Edit; 6] = [
            |m| m.profile_name = "other".into(),
            |m| m.checkpoint_digest = format!("sha256:{}", "cd".repeat(32)),
            |m| m.checkpoint_digest.clear(),
            |m| m.service_port = 30001,
            |m| m.service_port = 0,
            |m| m.binding_id = "binding".into(),
        ];
        for (index, edit) in edits.iter().enumerate() {
            let mut wire = head.to_wire();
            edit(wire.group_member_launch.as_mut().unwrap());
            assert!(!decodes(wire), "edit {index} was accepted");
        }
        let mut moved = head.clone();
        let MemberAction::Launch { member, .. } = &mut moved.action else {
            unreachable!()
        };
        member.grant_id = "01K00000000000000000000009".into();
        assert_ne!(moved.canonical_digest(), head.canonical_digest());
        assert!(moved.verify_digest().is_err());
    }

    // T30 (ADR 0028 §8, R7): a group Launch whose host checks fail again at
    // launch is refused with one closed code (a group code or a policy
    // category) on a completed result that claims nothing and names no binding.
    #[test]
    fn group_launch_refusal_is_closed_and_effect_free() {
        use capyctl_protocol::execution::validate_result;
        let command = command_with(launch(sample_group_plan(GroupEngine::Sglang)));
        let result = |refused: &str| pb::MemberExecutionResult {
            identity: command.to_wire().identity,
            state: "completed".into(),
            owned_handle: command.identity.command_id.clone(),
            refused: refused.into(),
            ..Default::default()
        };
        for code in [
            "group_topology_invalid",
            "peer_address_not_local",
            "rendezvous_port_in_use:25001",
            "group_checkpoint_mismatch",
            "host_tuning_missing:memlock",
            "insufficient_memory",
            "checkpoint_mismatch",
            "unauthorized",
        ] {
            validate_result(&command, &result(code)).unwrap();
        }
        for code in [
            "host_tuning_warning:memlock",
            "rendezvous_port_in_use:0",
            "group_drift:tp",
        ] {
            assert!(validate_result(&command, &result(code)).is_err(), "{code}");
        }
        type Mutation = fn(&mut pb::MemberExecutionResult);
        let mutations: [Mutation; 6] = [
            |r| r.state = "launched".into(),
            |r| r.claim_retained = true,
            |r| r.owned_handle = "other".into(),
            |r| r.binding_id = "01K00000000000000000000001".into(),
            |r| r.incarnation = "01K00000000000000000000002".into(),
            |r| {
                r.processes.push(pb::OwnedProcessObservation {
                    pid: 7,
                    start_ticks: 1,
                    role: "api".into(),
                    boot_id: "boot".into(),
                    presence: "alive".into(),
                })
            },
        ];
        for (index, mutate) in mutations.iter().enumerate() {
            let mut mutated = result("peer_address_not_local");
            mutate(&mut mutated);
            assert!(
                validate_result(&command, &mutated).is_err(),
                "mutation {index}"
            );
        }
    }

    // T30 (ADR 0028 §9): the head's launch may claim a usable model on its
    // readiness; a worker's never does.
    #[test]
    fn only_the_head_launch_claims_a_usable_model() {
        use capyctl_protocol::execution::validate_result;
        let ready = |command: &MemberCommand| {
            let process = |role: &str, pid| pb::OwnedProcessObservation {
                role: role.into(),
                pid,
                boot_id: "boot".into(),
                start_ticks: 7,
                presence: "alive".into(),
            };
            pb::MemberExecutionResult {
                identity: command.to_wire().identity,
                state: "launched".into(),
                owned_handle: command.identity.command_id.clone(),
                processes: vec![process("api", 10), process("worker-0", 11)],
                claim_retained: true,
                model_usable: true,
                binding_id: "01K00000000000000000000001".into(),
                incarnation: "01K00000000000000000000002".into(),
                ..Default::default()
            }
        };
        let head = command_with(launch(sample_group_plan(GroupEngine::Vllm)));
        validate_result(&head, &ready(&head)).unwrap();
        let worker = worker_launch(sample_group_plan(GroupEngine::Vllm), 0);
        assert!(validate_result(&worker, &ready(&worker)).is_err());
        let mut unusable = ready(&worker);
        unusable.model_usable = false;
        validate_result(&worker, &unusable).unwrap();
    }

    // T31 (ADR 0028 §11, Review Focus 6): `escalated` is evidence of a
    // completed Terminate only.
    #[test]
    fn escalation_rides_a_completed_terminate_only() {
        use capyctl_protocol::execution::validate_result;
        let terminate = command_with(MemberAction::Terminate {
            owned_handle: "owned".into(),
            recorded: Vec::new(),
        });
        let gone = pb::MemberExecutionResult {
            identity: terminate.to_wire().identity,
            state: "completed".into(),
            owned_handle: "owned".into(),
            escalated: true,
            ..Default::default()
        };
        validate_result(&terminate, &gone).unwrap();
        let mut accepted = gone.clone();
        accepted.state = "accepted".into();
        assert!(validate_result(&terminate, &accepted).is_err());
        let head = command_with(launch(sample_group_plan(GroupEngine::Vllm)));
        let launched = pb::MemberExecutionResult {
            identity: head.to_wire().identity,
            state: "completed".into(),
            escalated: true,
            ..Default::default()
        };
        assert!(validate_result(&head, &launched).is_err());
    }
}

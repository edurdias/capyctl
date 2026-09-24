//! Session protocol version 2 additions (plan W3): Park and Restore member
//! actions, load reports and member exit reports. Shape and binding only; no
//! host or controller behavior is exercised here.
use mllm_protocol::execution::{validate_result, MemberAction, MemberCommand};
use mllm_protocol::reports::{
    ExitStatus, LoadReport, MemberExit, MAX_LOAD_REPORT_BYTES, MAX_LOAD_SAMPLES,
};
use mllm_protocol::{pb, COMMAND_ENCODING_VERSION, PROTOCOL_VERSION};
use prost::Message;

fn base() -> MemberCommand {
    MemberCommand::try_from(pb::ServerToAgent {
        msg: Some(pb::server_to_agent::Msg::ExecuteMember(pb::ExecuteMember {
            identity: Some(pb::CommandIdentity {
                controller_id: "controller".into(),
                host_id: "host".into(),
                member_id: "rank-0".into(),
                deployment_id: "model".into(),
                operation_id: "op".into(),
                command_id: "command".into(),
                step_id: "park".into(),
                generation: 3,
                revision: 1,
                deadline_unix_ms: 100,
                payload_digest: vec![1; 32],
                expected_state: "ready".into(),
                profile_fingerprint: "pinned".into(),
                protocol_version: COMMAND_ENCODING_VERSION.into(),
                instance_index: 0,
            }),
            action: Some(pb::execute_member::Action::Inspect(true)),
            restore_checkpoint_digest: String::new(),
            terminate_recorded_processes: Vec::new(),
        })),
    })
    .unwrap()
}

fn with_action(action: MemberAction) -> MemberCommand {
    let mut command = base();
    command.action = action;
    command.identity.payload_digest = command.canonical_digest();
    command
}
fn park(handle: &str) -> MemberCommand {
    with_action(MemberAction::Park {
        owned_handle: handle.into(),
    })
}
fn restore(handle: &str) -> MemberCommand {
    with_action(MemberAction::Restore {
        owned_handle: handle.into(),
        checkpoint_digest: String::new(),
    })
}
fn wire(command: &MemberCommand) -> pb::ServerToAgent {
    pb::ServerToAgent {
        msg: Some(pb::server_to_agent::Msg::ExecuteMember(command.to_wire())),
    }
}

// T34 T37: Park and Restore survive the wire unchanged, are digest-bound to
// the launch they name, and are distinct actions from each other and Probe.
#[test]
fn park_and_restore_roundtrip_and_are_digest_bound() {
    for command in [park("launch"), restore("launch")] {
        command.verify_digest().unwrap();
        let bytes = wire(&command).encode_to_vec();
        let decoded =
            MemberCommand::try_from(pb::ServerToAgent::decode(bytes.as_slice()).unwrap()).unwrap();
        assert_eq!(decoded, command);
        decoded.verify_digest().unwrap();
        // Moving the command to another launch invalidates its digest.
        let mut moved = command.clone();
        moved.action = match &command.action {
            MemberAction::Park { .. } => MemberAction::Park {
                owned_handle: "other".into(),
            },
            _ => MemberAction::Restore {
                owned_handle: "other".into(),
                checkpoint_digest: String::new(),
            },
        };
        assert!(moved.verify_digest().is_err());
        for handle in ["", " ", &"x".repeat(4097)] {
            let mut bad = command.clone();
            bad.action = match &command.action {
                MemberAction::Park { .. } => MemberAction::Park {
                    owned_handle: handle.into(),
                },
                _ => MemberAction::Restore {
                    owned_handle: handle.into(),
                    checkpoint_digest: String::new(),
                },
            };
            bad.identity.payload_digest = bad.canonical_digest();
            assert!(bad.verify_digest().is_err(), "{handle:?}");
        }
    }
    let digests = [
        park("launch").canonical_digest(),
        restore("launch").canonical_digest(),
        with_action(MemberAction::Probe {
            owned_handle: "launch".into(),
        })
        .canonical_digest(),
        with_action(MemberAction::Terminate {
            owned_handle: "launch".into(),
            recorded: Vec::new(),
        })
        .canonical_digest(),
    ];
    for (i, a) in digests.iter().enumerate() {
        for b in &digests[i + 1..] {
            assert_ne!(a, b);
        }
    }
    // The new actions use the next free ExecuteMember oneof fields.
    let Some(pb::server_to_agent::Msg::ExecuteMember(parked)) = wire(&park("h")).msg else {
        panic!()
    };
    assert!(matches!(
        parked.action,
        Some(pb::execute_member::Action::ParkOwnedHandle(_))
    ));
    let encoded = parked.encode_to_vec();
    assert!(encoded.ends_with(&[(9 << 3) | 2, 1, b'h']), "field 9");
    let Some(pb::server_to_agent::Msg::ExecuteMember(restored)) = wire(&restore("h")).msg else {
        panic!()
    };
    assert!(
        restored
            .encode_to_vec()
            .ends_with(&[(10 << 3) | 2, 1, b'h']),
        "field 10"
    );
}

// T34: an action this build does not know is refused, never guessed. A newer
// peer's action on an unknown field decodes as no action at all.
#[test]
fn unknown_or_missing_action_is_rejected() {
    let mut message = park("launch").to_wire();
    message.action = None;
    let mut bytes = message.encode_to_vec();
    // Field 12, length-delimited: a future action this build cannot interpret
    // (field 11 is WE3's DigestCheckpoint).
    bytes.extend_from_slice(&[(12 << 3) | 2, 6]);
    bytes.extend_from_slice(b"launch");
    let decoded = pb::ExecuteMember::decode(bytes.as_slice()).unwrap();
    assert!(decoded.action.is_none());
    assert!(MemberCommand::try_from(pb::ServerToAgent {
        msg: Some(pb::server_to_agent::Msg::ExecuteMember(decoded)),
    })
    .is_err());
}

// T34: a stale or zero generation and a mismatched encoding version never
// become a Park command, and a result for another generation never settles it.
#[test]
fn stale_generation_and_version_are_rejected() {
    let command = park("launch");
    let mut zero = command.clone();
    zero.identity.generation = 0;
    zero.identity.payload_digest = zero.canonical_digest();
    assert!(zero.verify_digest().is_err());
    // A command identity carries the command encoding version, not the session
    // protocol version.
    let mut message = command.to_wire();
    message.identity.as_mut().unwrap().protocol_version = PROTOCOL_VERSION.into();
    assert!(MemberCommand::try_from(pb::ServerToAgent {
        msg: Some(pb::server_to_agent::Msg::ExecuteMember(message)),
    })
    .is_err());
    let mut stale = parked_result(&command);
    stale.identity.as_mut().unwrap().generation -= 1;
    assert!(validate_result(&command, &stale).is_err());
}

// SPEC §13.1 T34: only a peer on this session protocol version is compatible.
#[test]
fn incompatible_peer_versions_are_refused() {
    assert!(mllm_protocol::compatible_peer(PROTOCOL_VERSION));
    for version in ["1", "", "3", "2 ", "v2"] {
        assert!(!mllm_protocol::compatible_peer(version), "{version:?}");
    }
}

fn process(role: &str, pid: u32) -> pb::OwnedProcessObservation {
    pb::OwnedProcessObservation {
        role: role.into(),
        pid,
        boot_id: "boot".into(),
        start_ticks: 7,
        presence: "alive".into(),
    }
}
fn residency(state: &str) -> pb::ResidencyEvidence {
    pb::ResidencyEvidence {
        state: state.into(),
        mem_available_before_bytes: 10,
        mem_available_after_bytes: 90,
        milestones: vec!["sleep_level_2".into(), "released".into()],
    }
}
fn parked_result(command: &MemberCommand) -> pb::MemberExecutionResult {
    pb::MemberExecutionResult {
        identity: command.to_wire().identity,
        state: "completed".into(),
        owned_handle: "launch".into(),
        processes: vec![process("api", 10), process("worker-0", 11)],
        observed_at_unix_ms: 50,
        claim_retained: true,
        model_usable: false,
        binding_id: "binding".into(),
        incarnation: "incarnation".into(),
        residency: Some(residency("parked")),
        checkpoint: None,
        refused: String::new(),
        launch_failure: String::new(),
        source: None,
    }
}

// T16 T20 T34: parked is claimed only by a completed Park of the named launch,
// still owned, with its api and worker alive (process identity unchanged) and
// a named binding. A park never claims a usable model.
#[test]
fn park_result_claims_parked_only_with_owned_live_group() {
    let command = park("launch");
    let good = parked_result(&command);
    validate_result(&command, &good).unwrap();
    let edits: &[fn(&mut pb::MemberExecutionResult)] = &[
        |r| r.model_usable = true,
        |r| r.owned_handle = "other".into(),
        |r| r.state = "accepted".into(),
        |r| r.state = "attempted".into(),
        |r| r.state = "launched".into(),
        |r| r.claim_retained = false,
        |r| r.binding_id.clear(),
        |r| r.incarnation.clear(),
        |r| r.processes.retain(|p| p.role != "api"),
        |r| r.processes.retain(|p| p.role == "api"),
        |r| r.processes[1].presence = "gone".into(),
        |r| r.processes[1].boot_id = "other-boot".into(),
        |r| r.residency.as_mut().unwrap().state = "restored".into(),
        |r| r.residency.as_mut().unwrap().state = "evicted".into(),
        |r| r.residency.as_mut().unwrap().mem_available_after_bytes = -2,
        |r| r.residency.as_mut().unwrap().milestones.push(String::new()),
        |r| {
            r.residency
                .as_mut()
                .unwrap()
                .milestones
                .push("Has Spaces".into())
        },
        |r| {
            r.residency
                .as_mut()
                .unwrap()
                .milestones
                .push("x".repeat(65))
        },
        |r| {
            r.residency.as_mut().unwrap().milestones = vec!["step".into(); 33];
        },
    ];
    for edit in edits {
        let mut result = good.clone();
        edit(&mut result);
        assert!(validate_result(&command, &result).is_err(), "{result:?}");
    }
    // A failed or ambiguous park is ordinary evidence: no claim, no readiness.
    let mut failed = good.clone();
    failed.residency = Some(residency("unchanged"));
    failed.processes[1].presence = "gone".into();
    validate_result(&command, &failed).unwrap();
    let mut pending = good.clone();
    pending.state = "attempted".into();
    pending.residency = None;
    validate_result(&command, &pending).unwrap();
    let mut unobserved = good.clone();
    unobserved
        .residency
        .as_mut()
        .unwrap()
        .mem_available_before_bytes = -1;
    unobserved
        .residency
        .as_mut()
        .unwrap()
        .mem_available_after_bytes = -1;
    validate_result(&command, &unobserved).unwrap();
}

// T15 T16 T34: restored is claimed only by a completed Restore of the named
// launch; a usable model only with restored evidence and a live owned group
// (the fresh native probe that closes a restore, SPEC §6.1).
#[test]
fn restore_result_claims_usable_only_after_restored() {
    let command = restore("launch");
    let mut good = parked_result(&command);
    good.residency = Some(residency("restored"));
    good.model_usable = true;
    validate_result(&command, &good).unwrap();
    // Restored without a successful probe is valid evidence, not readiness.
    let mut unprobed = good.clone();
    unprobed.model_usable = false;
    validate_result(&command, &unprobed).unwrap();
    let edits: &[fn(&mut pb::MemberExecutionResult)] = &[
        |r| r.residency = None,
        |r| r.residency.as_mut().unwrap().state = "unchanged".into(),
        |r| r.residency.as_mut().unwrap().state = "parked".into(),
        |r| r.owned_handle = "other".into(),
        |r| r.state = "attempted".into(),
        |r| r.claim_retained = false,
        |r| r.processes[0].presence = "unknown".into(),
        |r| r.binding_id.clear(),
    ];
    for edit in edits {
        let mut result = good.clone();
        edit(&mut result);
        assert!(validate_result(&command, &result).is_err(), "{result:?}");
    }
}

// T34 T37: residency evidence belongs to Park and Restore results only.
#[test]
fn residency_evidence_is_refused_on_other_actions() {
    for action in [
        MemberAction::Probe {
            owned_handle: "launch".into(),
        },
        MemberAction::Terminate {
            owned_handle: "launch".into(),
            recorded: Vec::new(),
        },
        MemberAction::Inspect,
    ] {
        let command = with_action(action);
        let mut result = parked_result(&command);
        result.model_usable = false;
        for state in ["parked", "restored", "unchanged", "unknown"] {
            result.residency = Some(residency(state));
            assert!(validate_result(&command, &result).is_err(), "{state}");
        }
        result.residency = None;
        validate_result(&command, &result).unwrap();
    }
}

fn sample(deployment: &str, generation: i64) -> pb::LoadSample {
    pb::LoadSample {
        deployment_id: deployment.into(),
        generation,
        owned_handle: "01K00000000000000000000001".into(),
        sampled_at_unix_ms: 1_000,
        ingress_in_flight: 2,
        running: 3,
        waiting: 4,
        kv_usage_ppm: 250_000,
        scrape_ok: true,
        latency: None,
    }
}
fn load(samples: Vec<pb::LoadSample>) -> pb::ReportLoad {
    pb::ReportLoad {
        host_id: "host".into(),
        samples,
    }
}

// SPEC §17 T34: a load report round-trips on AgentToServer field 7 and keeps a
// failed scrape distinct from an idle engine.
#[test]
fn load_report_roundtrips_on_the_session_stream() {
    let mut failed = sample("other", 1);
    failed.scrape_ok = false;
    failed.running = 0;
    failed.waiting = 0;
    failed.kv_usage_ppm = 0;
    let report = LoadReport::try_from(load(vec![sample("model", 3), failed])).unwrap();
    assert!(report.samples[0].engine.is_some());
    assert!(report.samples[1].engine.is_none());
    assert_eq!(report.samples[1].ingress_in_flight, 2);
    let message = pb::AgentToServer {
        msg: Some(pb::agent_to_server::Msg::ReportLoad(report.to_wire())),
    };
    let bytes = message.encode_to_vec();
    assert_eq!(bytes[0], (7 << 3) | 2, "AgentToServer field 7");
    let Some(pb::agent_to_server::Msg::ReportLoad(decoded)) =
        pb::AgentToServer::decode(bytes.as_slice()).unwrap().msg
    else {
        panic!()
    };
    assert_eq!(LoadReport::try_from(decoded).unwrap(), report);
}

// SPEC §17 cardinality: batches are bounded in count and bytes, gauges are
// bounded, and one scope appears once per report.
#[test]
fn load_report_is_bounded() {
    let full: Vec<_> = (0..MAX_LOAD_SAMPLES)
        .map(|i| sample(&format!("d{i}"), 1))
        .collect();
    LoadReport::try_from(load(full.clone())).unwrap();
    let mut over = full.clone();
    over.push(sample("extra", 1));
    assert!(LoadReport::try_from(load(over)).is_err());
    assert!(LoadReport::try_from(load(Vec::new())).is_err());
    let mut heavy = full;
    for s in &mut heavy {
        s.owned_handle = "h".repeat(4096);
    }
    assert!(load(heavy.clone()).encoded_len() > MAX_LOAD_REPORT_BYTES);
    assert!(LoadReport::try_from(load(heavy)).is_err());
    let edits: &[fn(&mut pb::LoadSample)] = &[
        |s| s.generation = 0,
        |s| s.generation = -1,
        |s| s.deployment_id.clear(),
        |s| s.deployment_id = "d".repeat(129),
        |s| s.owned_handle = " ".into(),
        |s| s.sampled_at_unix_ms = -1,
        |s| s.kv_usage_ppm = 1_000_001,
        |s| s.running = u32::MAX,
        |s| s.waiting = (1 << 20) + 1,
        |s| s.ingress_in_flight = u32::MAX,
        |s| s.scrape_ok = false,
    ];
    for edit in edits {
        let mut s = sample("model", 1);
        edit(&mut s);
        assert!(
            LoadReport::try_from(load(vec![s.clone()])).is_err(),
            "{s:?}"
        );
    }
    assert!(LoadReport::try_from(load(vec![sample("model", 1), sample("model", 1)])).is_err());
    let mut anonymous = load(vec![sample("model", 1)]);
    anonymous.host_id.clear();
    assert!(LoadReport::try_from(anonymous).is_err());
}

// T18 T34: a sample for another generation is stale and dropped by the receiver.
#[test]
fn load_sample_is_fenced_by_generation() {
    let report = LoadReport::try_from(load(vec![sample("model", 3)])).unwrap();
    assert!(report.samples[0].is_for_generation(3));
    assert!(!report.samples[0].is_for_generation(4));
    assert!(!report.samples[0].is_for_generation(2));
}

fn exit() -> pb::MemberExit {
    pb::MemberExit {
        host_id: "host".into(),
        deployment_id: "model".into(),
        generation: 3,
        owned_handle: "01K00000000000000000000001".into(),
        process: Some(pb::OwnedProcessObservation {
            presence: "gone".into(),
            ..process("worker-0", 11)
        }),
        exit_code: None,
        exit_signal: Some(9),
        observed_at_unix_ms: 1_000,
    }
}

// SPEC §13.2 T20 T33: an exit report names one owned process by PID plus start
// identity, round-trips on AgentToServer field 8, and keeps its status exact.
#[test]
fn member_exit_roundtrips_with_exact_status() {
    for (code, signal, status) in [
        (None, Some(9), ExitStatus::Signal(9)),
        (Some(0), None, ExitStatus::Code(0)),
        (Some(137), None, ExitStatus::Code(137)),
        (None, None, ExitStatus::Unobserved),
    ] {
        let mut wire = exit();
        wire.exit_code = code;
        wire.exit_signal = signal;
        let typed = MemberExit::try_from(wire).unwrap();
        assert_eq!(typed.status, status);
        let message = pb::AgentToServer {
            msg: Some(pb::agent_to_server::Msg::MemberExit(typed.to_wire())),
        };
        let bytes = message.encode_to_vec();
        assert_eq!(bytes[0], (8 << 3) | 2, "AgentToServer field 8");
        let Some(pb::agent_to_server::Msg::MemberExit(decoded)) =
            pb::AgentToServer::decode(bytes.as_slice()).unwrap().msg
        else {
            panic!()
        };
        assert_eq!(MemberExit::try_from(decoded).unwrap(), typed);
    }
}

// SPEC §13.2 T34: a PID alone, a live process, an ambiguous status or a stale
// generation never becomes an exit report.
#[test]
fn member_exit_is_rejected_when_malformed_or_stale() {
    let edits: &[fn(&mut pb::MemberExit)] = &[
        |e| e.generation = 0,
        |e| e.host_id.clear(),
        |e| e.deployment_id.clear(),
        |e| e.owned_handle.clear(),
        |e| e.observed_at_unix_ms = -1,
        |e| e.process = None,
        |e| e.process.as_mut().unwrap().pid = 0,
        |e| e.process.as_mut().unwrap().start_ticks = 0,
        |e| e.process.as_mut().unwrap().boot_id.clear(),
        |e| e.process.as_mut().unwrap().role.clear(),
        |e| e.process.as_mut().unwrap().presence = "alive".into(),
        |e| e.process.as_mut().unwrap().presence = "unknown".into(),
        |e| e.exit_code = Some(0),
        |e| e.exit_signal = Some(0),
        |e| e.exit_signal = Some(65),
    ];
    for edit in edits {
        let mut wire = exit();
        edit(&mut wire);
        assert!(MemberExit::try_from(wire.clone()).is_err(), "{wire:?}");
    }
    let typed = MemberExit::try_from(exit()).unwrap();
    assert!(typed.is_for_generation(3));
    assert!(!typed.is_for_generation(4));
}

/// SPEC §§9.1, 10, 13 (W4): a Park or Restore the host's policy refused before
/// anything was journaled (for example a `restart_only` launch, ADR 0012) is
/// terminal evidence that the launch is `unchanged`, with a closed reason.
// T21 T34
#[test]
fn a_policy_refused_park_is_unchanged_with_a_closed_reason() {
    for command in [park("launch"), restore("launch")] {
        let mut result = parked_result(&command);
        result.residency = Some(pb::ResidencyEvidence {
            state: "unchanged".into(),
            mem_available_before_bytes: -1,
            mem_available_after_bytes: -1,
            milestones: vec![],
        });
        result.refused = "residency_tier".into();
        validate_result(&command, &result).unwrap();
        result.refused = "not_a_reason".into();
        assert!(validate_result(&command, &result).is_err());
        result.refused = "residency_tier".into();
        result.residency = Some(residency("unknown"));
        assert!(validate_result(&command, &result).is_err());
    }
    let probe = with_action(MemberAction::Probe {
        owned_handle: "launch".into(),
    });
    let mut result = parked_result(&probe);
    result.residency = None;
    result.refused = "unauthorized".into();
    assert!(validate_result(&probe, &result).is_err());
}

/// Owner decision 5 (2026-09-22): a Restore may carry the checkpoint digest the
/// server recorded, so a launch journaled before WE3 (whose plan has none) can
/// be woken against it. The digest is bound into the command; an empty one
/// encodes exactly as before; it belongs to Restore alone and must be a
/// checkpoint digest.
// T14 T15 T34
#[test]
fn restore_carries_a_recorded_checkpoint_digest() {
    let digest = format!("sha256:{}", "ab".repeat(32));
    let command = with_action(MemberAction::Restore {
        owned_handle: "launch".into(),
        checkpoint_digest: digest.clone(),
    });
    command.verify_digest().unwrap();
    assert_eq!(command.to_wire().restore_checkpoint_digest, digest);
    let bytes = wire(&command).encode_to_vec();
    let decoded =
        MemberCommand::try_from(pb::ServerToAgent::decode(bytes.as_slice()).unwrap()).unwrap();
    assert_eq!(decoded, command);
    // The digest is bound: another digest invalidates the command.
    let mut swapped = command.clone();
    swapped.action = MemberAction::Restore {
        owned_handle: "launch".into(),
        checkpoint_digest: format!("sha256:{}", "cd".repeat(32)),
    };
    assert!(swapped.verify_digest().is_err());
    assert_ne!(command.canonical_digest(), restore("launch").canonical_digest());
    // Without a digest the wire is the pre-decision encoding.
    assert!(restore("launch").to_wire().restore_checkpoint_digest.is_empty());
    // Malformed digests are refused.
    for bad in ["sha256:short", "md5:abc", " "] {
        let mut wire = command.to_wire();
        wire.restore_checkpoint_digest = bad.into();
        let decoded = MemberCommand::try_from(pb::ServerToAgent {
            msg: Some(pb::server_to_agent::Msg::ExecuteMember(wire)),
        });
        assert!(decoded.is_err(), "{bad:?}");
    }
    // Only a Restore may carry one.
    let mut parked = park("launch").to_wire();
    parked.restore_checkpoint_digest = digest;
    assert!(MemberCommand::try_from(pb::ServerToAgent {
        msg: Some(pb::server_to_agent::Msg::ExecuteMember(parked)),
    })
    .is_err());
}

fn recorded(pid: u32) -> mllm_domain::completion::ProcessIdentity {
    mllm_domain::completion::ProcessIdentity {
        role: "api".into(),
        pid,
        boot_id: "boot".into(),
        start_ticks: 7,
    }
}

// T34 (ADR 0016, additive): a Terminate carries the identities the server
// recorded. Empty, it encodes and digests exactly as before; carried, they
// round-trip and are bound by the digest; they are bounded, well formed and
// distinct, and belong to a Terminate only.
#[test]
fn terminate_recorded_identities_are_additive_and_bound() {
    let plain = with_action(MemberAction::Terminate {
        owned_handle: "launch".into(),
        recorded: Vec::new(),
    });
    let Some(pb::server_to_agent::Msg::ExecuteMember(encoded)) = wire(&plain).msg else {
        panic!()
    };
    assert!(encoded.terminate_recorded_processes.is_empty());
    let mut legacy = encoded.clone();
    legacy.terminate_recorded_processes.clear();
    assert_eq!(encoded.encode_to_vec(), legacy.encode_to_vec());

    let carried = with_action(MemberAction::Terminate {
        owned_handle: "launch".into(),
        recorded: vec![recorded(10), recorded(11)],
    });
    assert_ne!(carried.canonical_digest(), plain.canonical_digest());
    carried.verify_digest().unwrap();
    let decoded = MemberCommand::try_from(wire(&carried)).unwrap();
    assert_eq!(decoded, carried);
    // Changing a recorded identity breaks the bound digest.
    let mut forged = carried.clone();
    if let MemberAction::Terminate { recorded, .. } = &mut forged.action {
        recorded[0].pid = 12;
    }
    assert!(forged.verify_digest().is_err());

    let decode = |processes: Vec<pb::RecordedProcess>, action: pb::execute_member::Action| {
        let Some(pb::server_to_agent::Msg::ExecuteMember(mut message)) = wire(&plain).msg else {
            panic!()
        };
        message.terminate_recorded_processes = processes;
        message.action = Some(action);
        MemberCommand::try_from(pb::ServerToAgent {
            msg: Some(pb::server_to_agent::Msg::ExecuteMember(message)),
        })
    };
    let one = |pid: u32, ticks: u64, role: &str, boot: &str| pb::RecordedProcess {
        role: role.into(),
        pid,
        boot_id: boot.into(),
        start_ticks: ticks,
    };
    let terminate = || pb::execute_member::Action::TerminateOwnedHandle("launch".into());
    assert!(decode(vec![one(1, 7, "api", "boot")], terminate()).is_ok());
    for bad in [
        vec![one(0, 7, "api", "boot")],
        vec![one(1, 0, "api", "boot")],
        vec![one(1, 7, "", "boot")],
        vec![one(1, 7, "api", "")],
        vec![one(1, 7, &"r".repeat(65), "boot")],
        vec![one(1, 7, "api", "boot"), one(1, 7, "worker-0", "boot")],
        (1..=65).map(|pid| one(pid, 7, "api", "boot")).collect(),
    ] {
        assert!(decode(bad, terminate()).is_err());
    }
    // Only a Terminate carries recorded identities.
    assert!(decode(
        vec![one(1, 7, "api", "boot")],
        pb::execute_member::Action::ProbeOwnedHandle("launch".into())
    )
    .is_err());
}

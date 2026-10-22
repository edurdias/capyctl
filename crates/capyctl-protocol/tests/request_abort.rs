//! ADR 0032 §3: the AbortRequest member action. Additive: a new oneof field
//! number (19 in ExecuteMember), so every command journaled before it keeps
//! its canonical digest. It names a retained launch by its owned handle and a
//! request by CapyCTL's request ID, a ULID, and its result names only that
//! launch: posted (`completed`), refused with a closed code, or unanswered
//! (`attempted`).
use capyctl_protocol::capabilities::{
    agent_capabilities, drain_only_permits, refusal, required, CATALOGUE, REQUEST_ABORT,
};
use capyctl_protocol::execution::{
    is_request_id, validate_result, MemberAction, MemberCommand, ABORT_REFUSALS,
    ABORT_REQUEST_STATE,
};
use capyctl_protocol::{pb, COMMAND_ENCODING_VERSION};
use std::collections::BTreeSet;

const REQUEST: &str = "01JAAAAAAAAAAAAAAAAAAAAAAA";

fn identity() -> pb::CommandIdentity {
    pb::CommandIdentity {
        controller_id: "controller".into(),
        host_id: "host".into(),
        member_id: "head".into(),
        deployment_id: "deployment".into(),
        operation_id: "abort".into(),
        command_id: "abort".into(),
        step_id: "abort".into(),
        generation: 3,
        revision: 2,
        deadline_unix_ms: 100,
        payload_digest: vec![1; 32],
        expected_state: ABORT_REQUEST_STATE.into(),
        profile_fingerprint: "profile".into(),
        protocol_version: COMMAND_ENCODING_VERSION.into(),
        instance_index: 0,
    }
}

fn wire(owned_handle: &str, request_id: &str) -> pb::ExecuteMember {
    pb::ExecuteMember {
        identity: Some(identity()),
        action: Some(pb::execute_member::Action::AbortRequest(
            pb::AbortRequestCommand {
                owned_handle: owned_handle.into(),
                request_id: request_id.into(),
            },
        )),
        restore_checkpoint_digest: String::new(),
        terminate_recorded_processes: Vec::new(),
        group_member_launch: None,
        probe_max_tokens: 0,
        probe_decision: false,
    }
}

fn decode(wire: pb::ExecuteMember) -> Result<MemberCommand, ()> {
    MemberCommand::try_from(pb::ServerToAgent {
        msg: Some(pb::server_to_agent::Msg::ExecuteMember(wire)),
    })
    .map_err(|_| ())
}

// T17: the request ID is one canonical ULID: 26 Crockford base32 characters,
// upper case, at most `7` first; nothing else is ever an engine's `rid`.
#[test]
fn a_request_id_is_one_canonical_ulid() {
    assert!(is_request_id(REQUEST));
    assert!(is_request_id(&ulid::Ulid::new().to_string()));
    assert!(is_request_id("7ZZZZZZZZZZZZZZZZZZZZZZZZZ"));
    for bad in [
        "",
        "01JAAAAAAAAAAAAAAAAAAAAAA",
        "01JAAAAAAAAAAAAAAAAAAAAAAAA",
        "01jaaaaaaaaaaaaaaaaaaaaaaa",
        "8ZZZZZZZZZZZZZZZZZZZZZZZZZ",
        "01JAAAAAAAAAAAAAAAAAAAAAAI",
        "01JAAAAAAAAAAAAAAAAAAAAAAL",
        "01JAAAAAAAAAAAAAAAAAAAAAAO",
        "01JAAAAAAAAAAAAAAAAAAAAAAU",
        "01JAAAAAAAAAAAAAAAAAAAAA-A",
        "01JAAAAAAAAAAAAAAAAAAAAAA ",
        "caller-1",
    ] {
        assert!(!is_request_id(bad), "{bad:?}");
    }
}

// T17: the action round-trips through the wire and the canonical digest, and
// only a bounded handle with a ULID request ID decodes.
#[test]
fn abort_request_round_trips_and_names_a_ulid_only() {
    let command = decode(wire("launch-1", REQUEST)).expect("decodes");
    assert_eq!(
        command.action,
        MemberAction::AbortRequest {
            owned_handle: "launch-1".into(),
            request_id: REQUEST.into(),
        }
    );
    assert_eq!(decode(command.to_wire()).unwrap(), command);
    // The digest binds the request ID and the handle.
    let mut signed = command.clone();
    signed.identity.payload_digest = signed.canonical_digest();
    signed.verify_digest().expect("verifies");
    let mut other = signed.clone();
    other.action = MemberAction::AbortRequest {
        owned_handle: "launch-1".into(),
        request_id: "01JBBBBBBBBBBBBBBBBBBBBBBB".into(),
    };
    assert_ne!(other.canonical_digest(), signed.canonical_digest());
    assert!(other.verify_digest().is_err());
    other.action = MemberAction::AbortRequest {
        owned_handle: "launch-2".into(),
        request_id: REQUEST.into(),
    };
    assert!(other.verify_digest().is_err());
    // An unverifiable ID or a blank or unbounded handle never decodes.
    for (handle, request) in [
        ("launch-1", ""),
        ("launch-1", "caller-1"),
        ("launch-1", "01jaaaaaaaaaaaaaaaaaaaaaaa"),
        ("", REQUEST),
        ("   ", REQUEST),
        (&"h".repeat(4097), REQUEST),
    ] {
        assert!(
            decode(wire(handle, request)).is_err(),
            "{handle:?} {request:?}"
        );
    }
    // A probe field never rides an abort.
    let mut with_tokens = wire("launch-1", REQUEST);
    with_tokens.probe_max_tokens = 8;
    assert!(decode(with_tokens).is_err());
    let mut with_decision = wire("launch-1", REQUEST);
    with_decision.probe_decision = true;
    assert!(decode(with_decision).is_err());
}

fn result(command: &MemberCommand, state: &str, refused: &str) -> pb::MemberExecutionResult {
    pb::MemberExecutionResult {
        identity: command.to_wire().identity,
        state: state.into(),
        owned_handle: "launch-1".into(),
        observed_at_unix_ms: 1,
        refused: refused.into(),
        ..Default::default()
    }
}

// T17: an abort's result names its launch and nothing else: posted
// (`completed`), refused with a closed code, or unanswered (`attempted`).
#[test]
fn abort_results_name_their_launch_and_nothing_else() {
    let command = decode(wire("launch-1", REQUEST)).unwrap();
    validate_result(&command, &result(&command, "completed", "")).expect("posted");
    validate_result(&command, &result(&command, "attempted", "")).expect("unanswered");
    for code in ABORT_REFUSALS {
        validate_result(&command, &result(&command, "completed", code))
            .unwrap_or_else(|_| panic!("{code}"));
    }
    for (state, refused) in [
        ("attempted", "launch_not_retained"),
        ("completed", "unauthorized"),
        ("completed", "checkpoint_mismatch"),
        ("accepted", ""),
        ("launched", ""),
        ("tombstone", ""),
    ] {
        assert!(
            validate_result(&command, &result(&command, state, refused)).is_err(),
            "{state} {refused}"
        );
    }
    let mut other_launch = result(&command, "completed", "");
    other_launch.owned_handle = "launch-2".into();
    assert!(validate_result(&command, &other_launch).is_err());
    let mut claimed = result(&command, "completed", "");
    claimed.claim_retained = true;
    assert!(validate_result(&command, &claimed).is_err());
    let mut usable = result(&command, "completed", "");
    usable.model_usable = true;
    assert!(validate_result(&command, &usable).is_err());
    let mut bound = result(&command, "completed", "");
    bound.binding_id = "binding".into();
    bound.incarnation = "incarnation".into();
    assert!(validate_result(&command, &bound).is_err());
    let mut observed = result(&command, "completed", "");
    observed.processes.push(pb::OwnedProcessObservation {
        role: "api".into(),
        pid: 7,
        boot_id: "boot".into(),
        start_ticks: 9,
        presence: "alive".into(),
    });
    assert!(validate_result(&command, &observed).is_err());
    let mut tail = result(&command, "completed", "");
    tail.engine_log = Some(pb::EngineLogTailEvidence {
        state: "missing".into(),
        ..Default::default()
    });
    assert!(validate_result(&command, &tail).is_err());
    // An abort's refusal codes refuse no other action.
    let mut inspect = command.clone();
    inspect.action = MemberAction::Inspect;
    let mut refused_inspect = result(&inspect, "completed", "launch_not_retained");
    refused_inspect.owned_handle.clear();
    assert!(validate_result(&inspect, &refused_inspect).is_err());
}

// T17 (ADR 0017, version skew): the action needs `request_abort`, which this
// build's agent declares. A host that did not declare it (an older agent) is
// refused typed before anything is sent, so the server never sends it an
// abort, and its lanes are never capable. A drain-only host may still be
// asked: an abort only stops work its retained launch runs.
#[test]
fn abort_request_needs_its_capability() {
    assert_eq!(REQUEST_ABORT, "request_abort");
    assert!(CATALOGUE.iter().any(|(name, _)| *name == REQUEST_ABORT));
    assert!(agent_capabilities().contains(&REQUEST_ABORT.to_owned()));
    let wire = wire("launch-1", REQUEST);
    assert_eq!(required(&wire), vec![REQUEST_ABORT]);
    let older: BTreeSet<String> = agent_capabilities()
        .into_iter()
        .filter(|name| name != REQUEST_ABORT)
        .collect();
    assert_eq!(
        refusal(false, &older, &wire).as_deref(),
        Some("host_capability_missing:request_abort")
    );
    assert_eq!(
        refusal(false, &BTreeSet::new(), &wire).as_deref(),
        Some("host_capability_missing:request_abort")
    );
    let declared = BTreeSet::from([REQUEST_ABORT.to_owned()]);
    assert_eq!(refusal(false, &declared, &wire), None);
    assert!(drain_only_permits(&wire));
    assert_eq!(refusal(true, &declared, &wire), None);
    assert!(capyctl_protocol::capabilities::is_gate_refusal(
        "host_capability_missing:request_abort"
    ));
}

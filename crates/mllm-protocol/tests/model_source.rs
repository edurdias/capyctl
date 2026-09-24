//! ADR 0008: the MaterializeSource member action. Additive: a new oneof field
//! number (14 in ExecuteMember) and a new result field (14 in
//! MemberExecutionResult), so every command journaled before
//! it keeps its canonical digest. It names a remote source only, and its
//! result is nothing but source evidence bound to that source's store key.
use mllm_protocol::execution::{
    validate_result, MaterializeSourcePlan, MemberAction, MemberCommand,
};
use mllm_protocol::{pb, COMMAND_ENCODING_VERSION};

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

fn deployment(source: serde_json::Value) -> String {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../mllm-config/tests/fixtures/f2-deployment.json"
    ))
    .unwrap();
    let mut deployment = fixture["deployment"].clone();
    let model = deployment["model"].as_object_mut().unwrap();
    model.remove("path");
    model.insert("source".into(), source);
    deployment.to_string()
}

fn hf() -> serde_json::Value {
    serde_json::json!({"huggingface": {"repo": "Qwen/Qwen3-4B", "revision": SHA,
        "token_ref": "secret://hf"}})
}

fn identity() -> pb::CommandIdentity {
    pb::CommandIdentity {
        controller_id: "controller".into(),
        host_id: "host".into(),
        member_id: "head".into(),
        deployment_id: "deployment".into(),
        operation_id: "op".into(),
        command_id: "source".into(),
        step_id: "source".into(),
        generation: 1,
        revision: 1,
        deadline_unix_ms: 100,
        payload_digest: vec![1; 32],
        expected_state: "source".into(),
        profile_fingerprint: "build".into(),
        protocol_version: COMMAND_ENCODING_VERSION.into(),
        instance_index: 0,
    }
}

fn decode(config: String) -> Result<MemberCommand, ()> {
    MemberCommand::try_from(pb::ServerToAgent {
        msg: Some(pb::server_to_agent::Msg::ExecuteMember(pb::ExecuteMember {
            identity: Some(identity()),
            action: Some(pb::execute_member::Action::MaterializeSource(
                pb::MaterializeSourceRequest {
                    deployment_config: config,
                    host_policy_fingerprint: "a".repeat(64),
                },
            )),
            restore_checkpoint_digest: String::new(),
            terminate_recorded_processes: Vec::new(),
        })),
    })
    .map_err(|_| ())
}

fn result(command: &MemberCommand, evidence: pb::ModelSourceEvidence) -> pb::MemberExecutionResult {
    pb::MemberExecutionResult {
        identity: command.to_wire().identity,
        state: "completed".into(),
        observed_at_unix_ms: 1,
        source: Some(evidence),
        ..Default::default()
    }
}

// T34 (ADR 0008): the action round-trips, derives its key from the document,
// and refuses a local or unpinned source.
#[test]
fn materialize_source_round_trips_and_names_remote_sources_only() {
    let command = decode(deployment(hf())).expect("decodes");
    let MemberAction::MaterializeSource(plan) = &command.action else {
        panic!("{:?}", command.action);
    };
    assert_eq!(plan.source_key, format!("sources/huggingface/Qwen--Qwen3-4B@{SHA}"));
    assert!(plan.source().unwrap().is_remote());
    let again = MemberCommand::try_from(pb::ServerToAgent {
        msg: Some(pb::server_to_agent::Msg::ExecuteMember(command.to_wire())),
    })
    .unwrap();
    assert_eq!(again, command);
    assert_eq!(
        MaterializeSourcePlan::new(&deployment(hf()), &"a".repeat(64)).unwrap(),
        *plan
    );
    for source in [
        serde_json::json!({"type": "local", "path": "toy"}),
        serde_json::json!({"huggingface": {"repo": "Qwen/Qwen3-4B", "revision": "main"}}),
        serde_json::json!({"http": {"url": "http://example.test/w", "sha256": "a".repeat(64)}}),
    ] {
        assert!(decode(deployment(source.clone())).is_err(), "{source}");
    }
}

// T34 (ADR 0008): a result carries only source evidence of the plan's key,
// in a shape its state allows.
#[test]
fn source_evidence_is_bound_to_its_plan() {
    let command = decode(deployment(hf())).unwrap();
    let MemberAction::MaterializeSource(plan) = &command.action else {
        unreachable!()
    };
    let evidence = |state: &str, done: u64, total: u64, reason: &str, retained: bool| {
        pb::ModelSourceEvidence {
            state: state.into(),
            bytes_done: done,
            bytes_total: total,
            reason: reason.into(),
            reservation_retained: retained,
            source_key: plan.source_key.clone(),
        }
    };
    for ok in [
        evidence("pending", 0, 0, "", false),
        evidence("downloading", 0, 0, "", false),
        evidence("downloading", 10, 100, "", false),
        evidence("verified", 100, 100, "", false),
        evidence("failed", 0, 0, "hash_mismatch", false),
        evidence("failed", 0, 0, "network", true),
    ] {
        validate_result(&command, &result(&command, ok.clone())).unwrap_or_else(|_| panic!("{ok:?}"));
    }
    let mut other_key = evidence("verified", 1, 1, "", false);
    other_key.source_key = "sources/http/x".into();
    for bad in [
        evidence("downloading", 101, 100, "", false),
        evidence("verified", 1, 2, "", false),
        evidence("failed", 0, 0, "the token was hf_abc", false),
        evidence("failed", 0, 0, "", false),
        evidence("verified", 1, 1, "network", false),
        evidence("done", 0, 0, "", false),
        other_key,
    ] {
        assert!(validate_result(&command, &result(&command, bad.clone())).is_err(), "{bad:?}");
    }
    // No evidence, or evidence on another action, is refused.
    let mut empty = result(&command, evidence("pending", 0, 0, "", false));
    empty.source = None;
    assert!(validate_result(&command, &empty).is_err());
    let mut launched = result(&command, evidence("verified", 1, 1, "", false));
    launched.claim_retained = true;
    assert!(validate_result(&command, &launched).is_err());
}

//! ADR 0008: the MaterializeSource member action. Additive: a new oneof field
//! number (14 in ExecuteMember) and a new result field (14 in
//! MemberExecutionResult), so every command journaled before
//! it keeps its canonical digest. It names a remote source only, and its
//! result is nothing but source evidence bound to that source's store key.
use capyctl_protocol::execution::{
    validate_result, MaterializeSourcePlan, MemberAction, MemberCommand,
};
use capyctl_protocol::{pb, COMMAND_ENCODING_VERSION};

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

fn deployment(source: serde_json::Value) -> String {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/f2-deployment.json"
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
            group_member_launch: None,
            probe_max_tokens: 0,
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
    assert_eq!(
        plan.source_key,
        format!("sources/huggingface/Qwen--Qwen3-4B@{SHA}")
    );
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
        serde_json::json!({"http": {"url": "ftp://example.test/w", "sha256": "a".repeat(64)}}),
        serde_json::json!({"http": {"url": "http://user:pw@example.test/w", "sha256": "a".repeat(64)}}),
        serde_json::json!({"http": {"sha256": "a".repeat(64)}}),
    ] {
        assert!(decode(deployment(source.clone())).is_err(), "{source}");
    }
}

// T34 T14 (ADR 0008 amendment 2026-10-08): a deployment's remote drafter is
// materialized by the same action as its weights, one plan per source. Each
// plan's document names its one source as `model.source` and carries no
// drafter, so it decodes on any host that executes MaterializeSource, and
// its key is that source's.
#[test]
fn a_drafter_is_materialized_by_a_plan_of_its_own() {
    let draft = serde_json::json!({"http": {"url": "https://d.example.test/d",
        "sha256": "b".repeat(64)}});
    let with_draft = |source: serde_json::Value| {
        let mut document: serde_json::Value = serde_json::from_str(&deployment(source)).unwrap();
        document["model"]["draft"] = draft.clone();
        document.to_string()
    };
    let policy = "a".repeat(64);
    let text = with_draft(hf());
    let plans = MaterializeSourcePlan::all(&text, &policy);
    assert_eq!(
        plans
            .iter()
            .map(|p| p.source_key.as_str())
            .collect::<Vec<_>>(),
        [
            format!("sources/huggingface/Qwen--Qwen3-4B@{SHA}"),
            format!("sources/http/{}", "b".repeat(64))
        ]
    );
    assert_eq!(
        MaterializeSourcePlan::new(&text, &policy).as_ref(),
        Some(&plans[0])
    );
    assert_eq!(
        MaterializeSourcePlan::for_draft(&text, &policy).as_ref(),
        Some(&plans[1])
    );
    for plan in &plans {
        let document: serde_json::Value = serde_json::from_str(&plan.deployment_config).unwrap();
        assert!(document["model"].get("draft").is_none(), "{document}");
        let decoded = decode(plan.deployment_config.clone()).expect("decodes");
        assert_eq!(
            decoded.action,
            MemberAction::MaterializeSource(plan.clone())
        );
    }
    assert_eq!(
        plans[1].source(),
        serde_json::from_value(draft.clone()).ok()
    );
    // Local weights with a remote drafter: the drafter's plan alone.
    let text = with_draft(serde_json::json!({"type": "local", "path": "toy"}));
    assert!(MaterializeSourcePlan::new(&text, &policy).is_none());
    let plans = MaterializeSourcePlan::all(&text, &policy);
    assert_eq!(plans.len(), 1);
    assert_eq!(
        plans[0].source_key,
        format!("sources/http/{}", "b".repeat(64))
    );
    // A local drafter needs no plan.
    let mut document: serde_json::Value = serde_json::from_str(&deployment(hf())).unwrap();
    document["model"]["draft"] = serde_json::json!({"local": {"path": "drafts/d"}});
    assert!(MaterializeSourcePlan::for_draft(&document.to_string(), &policy).is_none());
    assert_eq!(
        MaterializeSourcePlan::all(&document.to_string(), &policy).len(),
        1
    );
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
        validate_result(&command, &result(&command, ok.clone()))
            .unwrap_or_else(|_| panic!("{ok:?}"));
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
        assert!(
            validate_result(&command, &result(&command, bad.clone())).is_err(),
            "{bad:?}"
        );
    }
    // No evidence, or evidence on another action, is refused.
    let mut empty = result(&command, evidence("pending", 0, 0, "", false));
    empty.source = None;
    assert!(validate_result(&command, &empty).is_err());
    let mut launched = result(&command, evidence("verified", 1, 1, "", false));
    launched.claim_retained = true;
    assert!(validate_result(&command, &launched).is_err());
}

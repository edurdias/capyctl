//! Owner decision 2026-09-22 (1), ADR 0014 amendment A1: deployment
//! `timeouts.initialize` and `timeouts.wake`. CPU-only tests; the derivation
//! formula is a placeholder until M16 measures real starts and wakes.

use capyctl_config::effective::{
    decode_effective_snapshot, deployment_command_fingerprint, resolve_effective,
    resolve_effective_with_checkpoint, resolve_snapshot_with_checkpoint, CheckpointFacts,
    TimeoutBasis, TimeoutSource, PENDING_INITIALIZE_MS,
};
use capyctl_config::{parse_strict, ConfigErrorCode, ConfigKind};
use serde_json::{json, Value};

const GB: i64 = 1_000_000_000;

fn fixture() -> (Value, Value) {
    let all: Value =
        serde_json::from_str(include_str!("fixtures/f2-deployment.json")).expect("fixture JSON");
    let (mut deployment, host) = (all["deployment"].clone(), all["host"].clone());
    // Long enough that the derived values are not lowered to it.
    deployment["request_deadline"] = json!("600s");
    (deployment, host)
}

/// T14: declared values keep `declared` provenance; omitted ones are derived
/// from the checkpoint's weights with the placeholder formula.
// T14
#[test]
fn declared_and_derived_timeouts_carry_their_provenance() {
    let (mut deployment, host) = fixture();
    deployment["timeouts"] = json!({"initialize": "5m"});
    let facts = CheckpointFacts {
        weights_bytes: Some(3 * GB),
        ..Default::default()
    };
    let effective = resolve_effective_with_checkpoint(&deployment, &host, facts).unwrap();
    let t = &effective.timeouts;
    assert_eq!(t.initialize_ms, 300_000);
    assert_eq!(t.provenance["initialize"], TimeoutSource::Declared);
    // 60 s + 5 s per GB of 3 GB.
    assert_eq!(t.wake_ms, 75_000);
    assert_eq!(t.provenance["wake"], TimeoutSource::Derived);
    assert_eq!(t.basis, Some(TimeoutBasis::CheckpointWeights));
    let shown = serde_json::to_value(&effective).unwrap();
    assert_eq!(shown["timeouts"]["provenance"]["initialize"], "declared");
    assert_eq!(shown["timeouts"]["provenance"]["wake"], "derived");
}

/// T14: while the digest is pending, the conservative Initialize value applies,
/// lowered to the request deadline.
// T14 T20
#[test]
fn a_pending_checkpoint_uses_the_conservative_value_within_the_request_deadline() {
    let (mut deployment, host) = fixture();
    deployment["request_deadline"] = json!("1200s");
    // The fixture host allows 600 s at most; a longer deployment deadline is
    // refused, so the pending 900 s is lowered to the 600 s the host allows.
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(error.path, "request_deadline");
    deployment["request_deadline"] = json!("600s");
    let effective = resolve_effective(&deployment, &host).unwrap();
    const { assert!(PENDING_INITIALIZE_MS > 600_000) };
    assert_eq!(effective.timeouts.initialize_ms, 600_000);
    assert_eq!(
        effective.timeouts.basis,
        Some(TimeoutBasis::CheckpointDigestPending)
    );
}

/// T14: a frozen revision decodes exactly, and re-resolution with measured
/// facts re-derives only what was derived.
// T14
#[test]
fn snapshots_round_trip_and_rederive_only_derived_timeouts() {
    let (mut deployment, host) = fixture();
    deployment["timeouts"] = json!({"wake": "90s"});
    let pending = resolve_effective(&deployment, &host).unwrap();
    let text = serde_json::to_string(&pending).unwrap();
    assert_eq!(decode_effective_snapshot(&text).unwrap(), pending);
    let measured = resolve_snapshot_with_checkpoint(
        &text,
        CheckpointFacts {
            weights_bytes: Some(3 * GB),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(measured.timeouts.wake_ms, 90_000);
    // 120 s + 10 s per GB of 3 GB.
    assert_eq!(measured.timeouts.initialize_ms, 150_000);
    // A snapshot claiming a derived value other than the derivation is refused.
    let mut forged: Value = serde_json::from_str(&text).unwrap();
    forged["timeouts"]["initialize_ms"] = json!(1);
    assert!(decode_effective_snapshot(&forged.to_string()).is_err());
}

/// T14: a revision frozen before `timeouts` existed still decodes; its
/// timeouts are derived again from the same facts.
// T14
#[test]
fn a_revision_frozen_before_timeouts_still_decodes() {
    let (deployment, host) = fixture();
    let effective = resolve_effective(&deployment, &host).unwrap();
    let mut value = serde_json::to_value(&effective).unwrap();
    value.as_object_mut().unwrap().remove("timeouts");
    let decoded = decode_effective_snapshot(&value.to_string()).unwrap();
    assert_eq!(decoded.timeouts, effective.timeouts);
}

/// T03: unknown fields, wrong units, floors and a timeout beyond the request
/// deadline are refused, through the strict YAML walk and resolution alike.
// T03
#[test]
fn invalid_timeouts_are_refused() {
    let yaml = |block: &str| {
        format!(
            "schema_version: 1\nkind: deployment\nname: toy\nmodel: {{path: /m, content_fingerprint: x, revision: r}}\ntimeouts:\n{block}\n"
        )
    };
    let error = parse_strict(ConfigKind::Deployment, &yaml("  start: 60s")).unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::UnknownField);
    let error = parse_strict(ConfigKind::Deployment, &yaml("  initialize: 10GiB")).unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::InvalidUnit);
    parse_strict(
        ConfigKind::Deployment,
        &yaml("  initialize: 10m\n  wake: 2m"),
    )
    .unwrap();

    for (block, path) in [
        (json!({"initialize": "20s"}), "timeouts.initialize"),
        (json!({"initialize": "601s"}), "timeouts.initialize"),
        (json!({"wake": "5s"}), "timeouts.wake"),
        (json!({"wake": "11m"}), "timeouts.wake"),
    ] {
        let (mut deployment, host) = fixture();
        deployment["timeouts"] = block.clone();
        let error = resolve_effective(&deployment, &host).unwrap_err();
        assert_eq!(error.path, path, "{block}");
    }
}

/// Declared timeouts are part of what a deploy command asks for; equivalent
/// spellings are the same command and omitting them changes nothing.
// T14
#[test]
fn declared_timeouts_are_part_of_the_command_identity() {
    let (deployment, _) = fixture();
    let base = deployment_command_fingerprint(&deployment, 600_000).unwrap();
    let mut minutes = deployment.clone();
    minutes["timeouts"] = json!({"initialize": "10m"});
    let mut seconds = deployment.clone();
    seconds["timeouts"] = json!({"initialize": "600s"});
    let a = deployment_command_fingerprint(&minutes, 600_000).unwrap();
    assert_eq!(
        a,
        deployment_command_fingerprint(&seconds, 600_000).unwrap()
    );
    assert_ne!(a, base);
}

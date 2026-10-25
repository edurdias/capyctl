//! ADR 0033 (LMCache) on the session wire: a launch's grant, its LMCache
//! report, a restart-park's evidence and the server's load samples. Shape and
//! binding only; no host or controller behavior is exercised here.
use capyctl_config::lmcache::{LmcacheCharge, LmcacheOffReason};
use capyctl_protocol::execution::{
    is_lmcache_refusal, validate_result, LaunchLmcache, LaunchLmcacheState, LmcacheGrant,
    MemberAction, MemberCommand, SingleLaunchPlan, MAX_LMCACHE_DETAIL_BYTES,
};
use capyctl_protocol::reports::{LmcacheSample, LoadReport};
use capyctl_protocol::{pb, COMMAND_ENCODING_VERSION};
use prost::Message;

const GIB: i64 = 1 << 30;

fn base(step: &str) -> MemberCommand {
    MemberCommand::try_from(pb::ServerToAgent {
        msg: Some(pb::server_to_agent::Msg::ExecuteMember(pb::ExecuteMember {
            identity: Some(pb::CommandIdentity {
                controller_id: "controller".into(),
                host_id: "host".into(),
                member_id: "rank-0".into(),
                deployment_id: "model".into(),
                operation_id: "op".into(),
                command_id: "command".into(),
                step_id: step.into(),
                generation: 3,
                revision: 1,
                deadline_unix_ms: 100,
                payload_digest: vec![1; 32],
                expected_state: "reserved".into(),
                profile_fingerprint: "pinned".into(),
                protocol_version: COMMAND_ENCODING_VERSION.into(),
                instance_index: 0,
            }),
            action: Some(pb::execute_member::Action::Inspect(true)),
            ..Default::default()
        })),
    })
    .unwrap()
}

fn with_action(action: MemberAction) -> MemberCommand {
    let mut command = base("step");
    command.action = action;
    command.identity.payload_digest = command.canonical_digest();
    command
}

fn grant() -> LmcacheGrant {
    LmcacheGrant {
        rpc_port: 8100,
        http_port: 8101,
        pool_bytes: 4 * GIB,
        overhead_bytes: 1280 << 20,
        device_bytes: 512 << 20,
        held_kv_bytes: 3 * GIB,
    }
}

fn plan(lmcache: Option<LmcacheGrant>) -> SingleLaunchPlan {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    SingleLaunchPlan {
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
        checkpoint_tables: None,
        checkpoint_gguf: None,
        startup_bytes: None,
        startup_host_bytes: None,
        lmcache,
        devices: Vec::new(),
    }
}

fn decode(
    command: &MemberCommand,
) -> Result<MemberCommand, capyctl_domain::group::GroupIdentityError> {
    MemberCommand::try_from(pb::ServerToAgent {
        msg: Some(pb::server_to_agent::Msg::ExecuteMember(
            pb::ExecuteMember::decode(command.to_wire().encode_to_vec().as_slice()).unwrap(),
        )),
    })
}

// T45 T37 (ADR 0033 §§3, 6): a launch's grant survives the wire unchanged, is
// bound by the payload digest and carries the store's charge exactly; a grant
// out of range never decodes.
#[test]
fn a_grant_roundtrips_is_digest_bound_and_carries_the_charge() {
    let command = with_action(MemberAction::LaunchSingle(plan(Some(grant()))));
    let decoded = decode(&command).unwrap();
    assert_eq!(decoded, command);
    decoded.verify_digest().unwrap();
    assert_eq!(
        grant().charge(),
        LmcacheCharge {
            pool_bytes: 4 * GIB,
            overhead_bytes: 1280 << 20,
            device_bytes: 512 << 20,
            held_kv_bytes: 3 * GIB,
        }
    );
    let mut moved = command.clone();
    let MemberAction::LaunchSingle(p) = &mut moved.action else {
        unreachable!()
    };
    p.lmcache.as_mut().unwrap().http_port = 8102;
    assert!(moved.verify_digest().is_err(), "the digest binds the grant");
    let edits: &[fn(&mut LmcacheGrant)] = &[
        |g| g.rpc_port = 0,
        |g| g.http_port = 0,
        |g| g.http_port = g.rpc_port,
        |g| g.rpc_port = 30000,
        |g| g.http_port = 30000,
        |g| g.pool_bytes = 0,
        |g| g.pool_bytes = GIB / 2,
        |g| g.pool_bytes = GIB + 1,
        |g| g.overhead_bytes = -1,
        |g| g.device_bytes = -1,
        |g| g.held_kv_bytes = -1,
    ];
    for edit in edits {
        let mut bad = grant();
        edit(&mut bad);
        let mut command = with_action(MemberAction::LaunchSingle(plan(Some(bad))));
        command.identity.payload_digest = command.canonical_digest();
        assert!(decode(&command).is_err(), "{bad:?}");
    }
    // A plan without a grant (the store planned LMCache off) decodes as before.
    let off = with_action(MemberAction::LaunchSingle(plan(None)));
    assert_eq!(decode(&off).unwrap(), off);
}

fn alive(role: &str, pid: u32) -> pb::OwnedProcessObservation {
    pb::OwnedProcessObservation {
        role: role.into(),
        pid,
        boot_id: "boot".into(),
        start_ticks: 7,
        presence: "alive".into(),
        rank: None,
    }
}

fn launched(
    command: &MemberCommand,
    lmcache: Option<pb::LaunchLmcacheReport>,
) -> pb::MemberExecutionResult {
    pb::MemberExecutionResult {
        identity: command.to_wire().identity,
        state: "launched".into(),
        owned_handle: command.identity.command_id.clone(),
        processes: vec![
            alive("api", 10),
            alive("worker-0", 11),
            alive("lmcache", 20),
            alive("lmcache-0", 21),
        ],
        observed_at_unix_ms: 50,
        claim_retained: true,
        model_usable: true,
        binding_id: "01K00000000000000000000001".into(),
        incarnation: "01K00000000000000000000002".into(),
        lmcache,
        ..Default::default()
    }
}

// T45 T37 (ADR 0033 §§8, 9, lifecycle design §10): a LaunchSingle result
// says what became of its LMCache: `on` with the server's ports and pool,
// `off` with a closed reason and a bounded detail. The server's group beside
// the engine's is one live group. Nothing else carries the report.
#[test]
fn a_launch_reports_its_lmcache_on_or_off_with_a_closed_reason() {
    let command = with_action(MemberAction::LaunchSingle(plan(Some(grant()))));
    let on = LaunchLmcache::on(8100, 8101, 4 * GIB);
    let result = launched(&command, Some(on.to_wire()));
    validate_result(&command, &result).unwrap();
    let decoded = pb::MemberExecutionResult::decode(result.encode_to_vec().as_slice()).unwrap();
    assert_eq!(
        LaunchLmcache::try_from(decoded.lmcache.as_ref().unwrap()).unwrap(),
        on
    );
    let off = LaunchLmcache::off(
        LmcacheOffReason::EngineStartFailed,
        "engine log line one\nline two",
        true,
    );
    assert_eq!(off.state, LaunchLmcacheState::Off);
    assert!(off.connector_failed);
    let mut failed = launched(&command, Some(off.to_wire()));
    failed.model_usable = false;
    for p in &mut failed.processes {
        p.presence = "gone".into();
    }
    validate_result(&command, &failed).unwrap();
    assert_eq!(
        LaunchLmcache::try_from(failed.lmcache.as_ref().unwrap()).unwrap(),
        off
    );
    // connector_failed only beside engine_start_failed.
    assert!(!LaunchLmcache::off(LmcacheOffReason::CheckFailed, "x", true).connector_failed);
    // The detail keeps its newest bytes, within the bound, without controls.
    let long = format!("{}\u{7}é{}", "a".repeat(3000), "z".repeat(10));
    let bounded = LaunchLmcache::off(LmcacheOffReason::EngineStartFailed, &long, true);
    assert!(bounded.detail.len() <= MAX_LMCACHE_DETAIL_BYTES);
    assert!(bounded.detail.ends_with("zzzzzzzzzz"));
    assert!(!bounded.detail.contains('\u{7}'));
    LaunchLmcache::try_from(&bounded.to_wire()).unwrap();
    let edits: &[fn(&mut pb::LaunchLmcacheReport)] = &[
        |r| r.state = "maybe".into(),
        |r| r.state = String::new(),
        |r| r.rpc_port = 0,
        |r| r.http_port = r.rpc_port,
        |r| r.rpc_port = 70_000,
        |r| r.pool_bytes = GIB - 1,
        |r| r.reason = "check_failed".into(),
        |r| r.detail = "on carries no detail".into(),
        |r| r.connector_failed = true,
    ];
    for edit in edits {
        let mut report = on.to_wire();
        edit(&mut report);
        assert!(LaunchLmcache::try_from(&report).is_err(), "{report:?}");
        assert!(validate_result(&command, &launched(&command, Some(report))).is_err());
    }
    let off_edits: &[fn(&mut pb::LaunchLmcacheReport)] = &[
        |r| r.reason = "made_up".into(),
        |r| r.reason = String::new(),
        |r| r.detail = "x".repeat(MAX_LMCACHE_DETAIL_BYTES + 1),
        |r| r.detail = "bell\u{7}".into(),
        |r| r.rpc_port = 8100,
        |r| r.pool_bytes = GIB,
        |r| r.reason = "server_start_failed".into(),
    ];
    for edit in off_edits {
        let mut report = off.to_wire();
        edit(&mut report);
        assert!(LaunchLmcache::try_from(&report).is_err(), "{report:?}");
    }
    // Every reason is a closed code that survives the wire.
    for reason in LmcacheOffReason::ALL {
        let off = LaunchLmcache::off(reason, reason.sentence(), false);
        assert_eq!(LaunchLmcache::try_from(&off.to_wire()).unwrap(), off);
    }
    // Only a LaunchSingle or Restore result carries the report.
    let probe = with_action(MemberAction::Probe {
        owned_handle: "launch".into(),
        max_tokens: None,
        decision: false,
    });
    let mut probed = launched(&probe, Some(on.to_wire()));
    probed.state = "completed".into();
    probed.owned_handle = "launch".into();
    assert!(validate_result(&probe, &probed).is_err());
    probed.lmcache = None;
    validate_result(&probe, &probed).unwrap();
}

fn residency(state: &str, engine_stopped: bool) -> pb::ResidencyEvidence {
    pb::ResidencyEvidence {
        state: state.into(),
        mem_available_before_bytes: 10,
        mem_available_after_bytes: 90,
        milestones: vec!["engine_stopped".into()],
        engine_stopped,
    }
}

// T45 T16 T34 (ADR 0033 §7): a restart-park stops the engine and keeps the
// server: its parked claim names the server's group alone, alive, and says
// `engine_stopped`. Only a parked Park says so; a parked claim without it
// still needs the engine's live group; a Restore reports its LMCache.
#[test]
fn a_restart_park_claims_parked_with_the_server_group_alone() {
    let park = with_action(MemberAction::Park {
        owned_handle: "launch".into(),
    });
    let parked = pb::MemberExecutionResult {
        identity: park.to_wire().identity,
        state: "completed".into(),
        owned_handle: "launch".into(),
        processes: vec![
            pb::OwnedProcessObservation {
                presence: "gone".into(),
                ..alive("api", 10)
            },
            pb::OwnedProcessObservation {
                presence: "gone".into(),
                ..alive("worker-0", 11)
            },
            alive("lmcache", 20),
            alive("lmcache-0", 21),
        ],
        observed_at_unix_ms: 50,
        claim_retained: true,
        binding_id: "binding".into(),
        incarnation: "incarnation".into(),
        residency: Some(residency("parked", true)),
        ..Default::default()
    };
    validate_result(&park, &parked).unwrap();
    let edits: &[fn(&mut pb::MemberExecutionResult)] = &[
        // The engine must be gone.
        |r| r.processes[0].presence = "alive".into(),
        // The server must be alive.
        |r| r.processes[2].presence = "gone".into(),
        // Without the flag the engine's live group is needed.
        |r| r.residency.as_mut().unwrap().engine_stopped = false,
        // Only a parked claim says the engine stopped.
        |r| r.residency.as_mut().unwrap().state = "unchanged".into(),
        |r| r.model_usable = true,
        |r| r.lmcache = Some(LaunchLmcache::on(8100, 8101, GIB).to_wire()),
    ];
    for edit in edits {
        let mut result = parked.clone();
        edit(&mut result);
        assert!(validate_result(&park, &result).is_err(), "{result:?}");
    }
    let restore = with_action(MemberAction::Restore {
        owned_handle: "launch".into(),
        checkpoint_digest: String::new(),
    });
    let restored = pb::MemberExecutionResult {
        identity: restore.to_wire().identity,
        state: "completed".into(),
        owned_handle: "launch".into(),
        processes: vec![
            alive("api", 30),
            alive("worker-0", 31),
            alive("lmcache", 20),
            alive("lmcache-0", 21),
        ],
        observed_at_unix_ms: 60,
        claim_retained: true,
        model_usable: true,
        binding_id: "binding".into(),
        incarnation: "incarnation".into(),
        residency: Some(residency("restored", false)),
        lmcache: Some(LaunchLmcache::on(8100, 8101, 4 * GIB).to_wire()),
        ..Default::default()
    };
    validate_result(&restore, &restored).unwrap();
    let mut stopped = restored.clone();
    stopped.residency.as_mut().unwrap().engine_stopped = true;
    assert!(validate_result(&restore, &stopped).is_err());
    // Absent, the evidence encodes exactly as an older host's.
    let mut baseline = residency("parked", false);
    assert_eq!(
        baseline.encode_to_vec(),
        pb::ResidencyEvidence {
            engine_stopped: false,
            ..baseline.clone()
        }
        .encode_to_vec()
    );
    baseline.engine_stopped = true;
    assert!(baseline.encode_to_vec().len() > residency("parked", false).encode_to_vec().len());
}

// T45 (ADR 0033 §§9, 14): a `lmcache: required` launch whose host found
// LMCache unusable before any effect is refused with the reason's code; the
// codes found only once something ran fail the launch instead.
#[test]
fn a_required_launch_is_refused_with_its_host_found_reason() {
    for code in [
        "lmcache_unavailable:check_failed",
        "lmcache_unavailable:not_installed",
        "lmcache_unavailable:not_registered",
        "lmcache_unavailable:version_unsupported",
        "lmcache_unavailable:host_off",
        "retention_zero:lmcache",
    ] {
        assert!(is_lmcache_refusal(code), "{code}");
    }
    for code in [
        "lmcache_unavailable:server_start_failed",
        "lmcache_unavailable:listener_exposed",
        "lmcache_unavailable:engine_start_failed",
        "lmcache_unavailable:disabled",
        "lmcache_unavailable:",
        "lmcache_unavailable:anything",
    ] {
        assert!(!is_lmcache_refusal(code), "{code}");
    }
    let command = with_action(MemberAction::LaunchSingle(plan(Some(grant()))));
    let refused = pb::MemberExecutionResult {
        identity: command.to_wire().identity,
        state: "completed".into(),
        owned_handle: command.identity.command_id.clone(),
        refused: "lmcache_unavailable:check_failed".into(),
        ..Default::default()
    };
    validate_result(&command, &refused).unwrap();
    let park = with_action(MemberAction::Park {
        owned_handle: "launch".into(),
    });
    let refused_park = pb::MemberExecutionResult {
        identity: park.to_wire().identity,
        state: "completed".into(),
        owned_handle: "launch".into(),
        refused: "lmcache_unavailable:check_failed".into(),
        residency: Some(residency("unchanged", false)),
        ..Default::default()
    };
    assert!(validate_result(&park, &refused_park).is_err());
}

fn lmcache_sample(deployment: &str, generation: i64) -> pb::LmcacheSample {
    pb::LmcacheSample {
        deployment_id: deployment.into(),
        generation,
        owned_handle: "launch".into(),
        sampled_at_unix_ms: 1_000,
        scrape_ok: true,
        used_bytes: Some(1 << 30),
        usage_ppm: Some(250_000),
        inflight_bytes: Some(0),
        alloc_failures: Some(0),
        lookups: Some(260),
        hits: Some(212),
        l2_bytes: None,
        rss_bytes: Some(5 << 30),
        device_bytes: None,
    }
}

// T45 T34 (ADR 0033 §12): LMCache samples ride on the load report beside the
// engine's, alone for a parked launch; every figure is optional (unknown is
// never zero), a failed scrape carries none, and a report stays bounded.
#[test]
fn lmcache_samples_roundtrip_with_unknown_figures_kept_unknown() {
    let mut unknown = lmcache_sample("other", 2);
    unknown.used_bytes = None;
    unknown.lookups = None;
    unknown.hits = None;
    let mut failed = lmcache_sample("third", 1);
    failed.scrape_ok = false;
    failed.used_bytes = None;
    failed.usage_ppm = None;
    failed.inflight_bytes = None;
    failed.alloc_failures = None;
    failed.lookups = None;
    failed.hits = None;
    let report = pb::ReportLoad {
        host_id: "host".into(),
        samples: Vec::new(),
        lmcache: vec![lmcache_sample("model", 3), unknown, failed],
    };
    let typed = LoadReport::try_from(report.clone()).unwrap();
    assert!(typed.samples.is_empty(), "a parked launch's server alone");
    assert_eq!(typed.lmcache[0].hits, Some(212));
    assert_eq!(typed.lmcache[1].used_bytes, None);
    assert_eq!(typed.lmcache[1].lookups, None);
    assert!(!typed.lmcache[2].scrape_ok);
    assert_eq!(
        typed.lmcache[2].rss_bytes,
        Some(5 << 30),
        "measured, not scraped"
    );
    let decoded = pb::ReportLoad::decode(typed.to_wire().encode_to_vec().as_slice()).unwrap();
    assert_eq!(decoded, report);
    assert_eq!(LoadReport::try_from(decoded).unwrap(), typed);
    let mut l2 = lmcache_sample("model", 3);
    assert!(!LmcacheSample::clone(&typed.lmcache[0]).l2_degraded());
    l2.l2_bytes = Some(0);
    let report = |sample: pb::LmcacheSample| pb::ReportLoad {
        host_id: "host".into(),
        samples: Vec::new(),
        lmcache: vec![sample],
    };
    assert!(!LoadReport::try_from(report(l2.clone())).unwrap().lmcache[0].l2_degraded());
    l2.l2_bytes = Some(1);
    assert!(LoadReport::try_from(report(l2)).unwrap().lmcache[0].l2_degraded());
    let edits: &[fn(&mut pb::LmcacheSample)] = &[
        |s| s.deployment_id.clear(),
        |s| s.generation = 0,
        |s| s.owned_handle.clear(),
        |s| s.sampled_at_unix_ms = -1,
        |s| s.usage_ppm = Some(1_000_001),
        |s| s.hits = Some(261),
        |s| s.scrape_ok = false,
    ];
    for edit in edits {
        let mut sample = lmcache_sample("model", 3);
        edit(&mut sample);
        assert!(
            LoadReport::try_from(report(sample.clone())).is_err(),
            "{sample:?}"
        );
    }
    let duplicate = pb::ReportLoad {
        host_id: "host".into(),
        samples: Vec::new(),
        lmcache: vec![lmcache_sample("model", 3), lmcache_sample("model", 3)],
    };
    assert!(LoadReport::try_from(duplicate).is_err());
    let empty = pb::ReportLoad {
        host_id: "host".into(),
        samples: Vec::new(),
        lmcache: Vec::new(),
    };
    assert!(LoadReport::try_from(empty).is_err());
    let over = pb::ReportLoad {
        host_id: "host".into(),
        samples: Vec::new(),
        lmcache: (0..65)
            .map(|i| lmcache_sample(&format!("m{i}"), 1))
            .collect(),
    };
    assert!(LoadReport::try_from(over).is_err());
}

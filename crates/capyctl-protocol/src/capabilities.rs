//! ADR 0017: protocol features added after the protocol version 2 baseline,
//! by name, and which of them a server-to-host command needs.
//!
//! The baseline is protocol version 2 as introduced: the typed member actions
//! (Prepare, Launch, LaunchSingle with its original plan fields, Inspect,
//! Terminate, CloseIngress, Probe, Park, Restore), member results with
//! residency evidence, load reports and member exit reports. Everything added
//! since is additive on new field numbers, and an absent field encodes exactly
//! as before, so journaled command digests stay verifiable.
//!
//! What an additive field cannot do is reach an older host safely: the host
//! decodes the command, drops the field it does not know, recomputes the
//! payload digest over what it kept, and refuses the command as a digest
//! mismatch. So a host declares every post-baseline feature it implements in
//! `Connect.capabilities`, and the server never sends a command needing one the
//! host did not declare. It refuses that operation for that host instead, with
//! `host_capability_missing:<name>`, before anything is sent.
//!
//! Host-to-server features are listed too. The server needs no gate for them
//! (an older host simply never sends them, and absence already means "not
//! reported"), but declaring them makes a host's feature set visible in status.
use crate::pb;
use std::collections::BTreeSet;

/// Owner decision 2026-09-23: application heartbeats, both directions, and the
/// SessionReady heartbeat bounds (`Connect.heartbeats`).
pub const HEARTBEATS: &str = "heartbeats";
/// ADR 0008: the MaterializeSource action (`Connect.model_sources`).
pub const MODEL_SOURCES: &str = "model_sources";
/// ADR 0014 §7 (WE3): the DigestCheckpoint action and a LaunchSingle plan's
/// `checkpoint_digest` / `checkpoint_weights_bytes`.
pub const CHECKPOINT_DIGEST: &str = "checkpoint_digest";
/// Owner decision 2026-09-23 (solo first start): `DigestCheckpointRequest.size_only`.
pub const CHECKPOINT_SIZE_ONLY: &str = "checkpoint_size_only";
/// Owner decision 2026-09-23: a LaunchSingle plan's `startup_bytes`.
pub const STARTUP_BYTES: &str = "startup_bytes";
/// ADR 0014 amendment A16: a LaunchSingle plan's
/// `checkpoint_state_slot_bytes` (and `CheckpointDigestEvidence.state_slot_bytes`
/// from the host).
pub const CHECKPOINT_STATE_SLOT: &str = "checkpoint_state_slot";
/// ADR 0013 §5: a nonzero `CommandIdentity.instance_index`.
pub const INSTANCE_INDEX: &str = "instance_index";
/// ADR 0014 §7, owner decision 5 (2026-09-22): `ExecuteMember.restore_checkpoint_digest`.
pub const RESTORE_CHECKPOINT_DIGEST: &str = "restore_checkpoint_digest";
/// ADR 0016: `ExecuteMember.terminate_recorded_processes`.
pub const TERMINATE_RECORDED_PROCESSES: &str = "terminate_recorded_processes";
/// SPEC §§6.4, 13.2: `MemberExecutionResult.launch_failure`.
pub const LAUNCH_FAILURE: &str = "launch_failure";
/// SPEC §13: `MemberExecutionResult.refused` and `IngressProvisioned.refused`.
pub const POLICY_REFUSAL: &str = "policy_refusal";
/// SPEC §17 (M80): `LoadSample.latency`.
pub const LOAD_LATENCY: &str = "load_latency";
/// SPEC §4.3: the HostDraining notice and its acknowledgement.
pub const HOST_DRAINING: &str = "host_draining";
/// ADR 0007: `DomainObservation.residents`.
pub const PROCESS_RESIDENCY: &str = "process_residency";
/// ADR 0008 (owner decision 2026-09-23): the installation fields of
/// `RuntimeProfileStatus`.
pub const INSTALLATION_FINGERPRINT: &str = "installation_fingerprint";
/// ADR 0018: PublishProfiles / RetireProfile from the host and
/// ProfilesPublished / ProfileRetirement from the server. Server-to-host: the
/// server sends its two messages only to a host that declared it, and a host
/// sends its two only to a server whose SessionReady lists it.
pub const LIVE_PROFILE_UPDATE: &str = "live_profile_update";
/// ADR 0019 (discrete GPU design §8): the host observes a discrete GPU's
/// memory as its own `device` domain. One capability covers the three
/// additions a discrete host needs: `DomainObservation.kind = "device"` with
/// its `device_id`, the split `ProcessResidency.device_bytes` / `host_bytes`,
/// and launches whose footprint and chosen GPU name a device domain. The
/// server places such a launch only on a host that declared it; any other
/// host is refused `host_capability_missing:device_memory_domains` before
/// anything is sent. A unified host needs it for nothing.
pub const DEVICE_MEMORY_DOMAINS: &str = "device_memory_domains";
/// ADR 0028 §14: multi-node engine groups. Covers the Prepare and Launch group
/// plans' engine, topology, generation, role, model path and worker port, the
/// member launch plan's `checkpoint_layout` (ADR 0028 §5, amendment of
/// 2026-10-07), and `ReportInventory.group`. The server places a group member
/// only on a host that declared it; any other host is refused
/// `host_capability_missing:engine_groups` before anything is sent.
pub const ENGINE_GROUPS: &str = "engine_groups";
/// SPEC §13.3 / T21: the EngineLogTail action and its result's
/// `engine_log` evidence. A host that did not declare it is refused
/// `host_capability_missing:engine_log_tail` and sent nothing.
pub const ENGINE_LOG_TAIL: &str = "engine_log_tail";
/// ADR 0014 amendment A20 (owner decision 2026-10-09): a LaunchSingle plan's
/// `checkpoint_tables` (and `CheckpointDigestEvidence.tables` from the host),
/// sent only when the engine keeps the checkpoint's tables on disk. A host
/// without it is refused `host_capability_missing:checkpoint_tables` for such
/// a launch before anything is sent; every other launch needs nothing.
pub const CHECKPOINT_TABLES: &str = "checkpoint_tables";
/// ADR 0029 §9: a LaunchSingle plan's `checkpoint_gguf` (and
/// `CheckpointDigestEvidence.gguf` from the host), sent only for a llama.cpp
/// revision. A host without it is refused
/// `host_capability_missing:checkpoint_gguf` for such a launch before
/// anything is sent; every other launch needs nothing.
pub const CHECKPOINT_GGUF: &str = "checkpoint_gguf";
/// ADR 0030 §1: the host serves decision models (`api: decisions`): it
/// resolves the field, proves such a deployment ready with a decision rather
/// than generated text, and forwards `/v1/systemone`. The server places a
/// decisions deployment only on a host that declared it; any other host is
/// refused `host_capability_missing:decisions` before anything is sent, so an
/// older host never receives a deployment whose checks it would run as chat.
pub const DECISIONS: &str = capyctl_config::decisions::HOST_CAPABILITY;
/// ADR 0036 §1: the host serves embedding models (`api: embeddings`): it
/// resolves the field, proves such a deployment ready with a fixed embedding
/// rather than generated text, and forwards `/v1/embeddings`. The server
/// places an embeddings deployment only on a host that declared it; any
/// other host is refused `host_capability_missing:embeddings` before
/// anything is sent, so an older host never runs a chat probe against an
/// embedding server.
pub const EMBEDDINGS: &str = capyctl_config::embeddings::HOST_CAPABILITY;
/// ADR 0031 §9: the host runs custom engines (a profile of kind `custom`:
/// the manifest's command, its placeholders, the loopback check). The server
/// places a deployment whose selected profile is a custom engine only on a
/// host that declared it; any other is refused
/// `host_capability_missing:custom_engines` before anything is sent.
pub const CUSTOM_ENGINES: &str = capyctl_config::custom::CAPABILITY;
/// ADR 0019 §3: a LaunchSingle plan's `startup_host_bytes`, sent only for a
/// launch on a discrete GPU whose startup peak the server measured per memory
/// domain. A host without it is refused
/// `host_capability_missing:startup_host_bytes` for such a launch before
/// anything is sent; every other launch needs nothing.
pub const STARTUP_HOST_BYTES: &str = "startup_host_bytes";
/// ADR 0033 §13: the host runs LMCache (ADR 0033): it renders the connector,
/// owns the private server per instance and reads the `lmcache` setting of
/// its own document and of a deployment's. The server places a deployment
/// with `lmcache: required` only on a host that declared it; any other host
/// is refused `host_capability_missing:lmcache` before anything is sent. An
/// `auto` deployment on such a host runs without LMCache, as before. It also
/// covers a LaunchSingle plan's `lmcache` grant (sent only to such a host),
/// and from the host `MemberExecutionResult.lmcache`,
/// `ResidencyEvidence.engine_stopped` and `ReportLoad.lmcache`. The last is
/// sent only to a server whose SessionReady lists it ([`server_capabilities`]),
/// since an older server would refuse a report left with no load sample.
pub const LMCACHE: &str = capyctl_config::lmcache::CAPABILITY;
/// ADR 0033 §§10, 13: the host honours zero data retention (memory engine
/// logs, no core dumps, no LMCache), its own `retention` setting and a
/// deployment's. The server places a deployment with `retention: zero` only
/// on a host that declared it; any other host is refused
/// `host_capability_missing:retention` before anything is sent, since it
/// would not honour it.
pub const RETENTION: &str = capyctl_config::lmcache::RETENTION_CAPABILITY;
/// ADR 0032 §2, §3: the host's ingress sets CapyCTL's request ID as SGLang's
/// `rid` on a capable launch, and the host runs the AbortRequest action (an
/// abort posted to a retained launch's engine on loopback). The server sends
/// an AbortRequest only to a host that declared it, and counts a remote lane
/// as capable only on such a host; any other is refused
/// `host_capability_missing:request_abort` before anything is sent and keeps
/// today's settlement rule.
pub const REQUEST_ABORT: &str = "request_abort";
/// ADR 0035 §11: one model on several GPUs of one host. Covers a LaunchSingle
/// plan's rank-ordered `devices` (sent only for a launch on two or more GPUs),
/// the per-GPU process readings (`ProcessResidency.per_device`, ADR 0035 §8),
/// the `rank-N` process role and a result's per-process `rank` (§6), and the
/// listener check on built-in engines (§5). The server sends a launch on
/// several GPUs only to a host that declared it; any other host is refused
/// `host_capability_missing:local_tensor_parallel` before anything is sent.
/// Any other launch needs nothing new.
pub const LOCAL_TENSOR_PARALLEL: &str = "local_tensor_parallel";

/// ADR 0018: bounds on the new messages' strings and lists.
pub const MAX_REQUEST_ID: usize = 64;
pub const MAX_RETIREMENT_DEPLOYMENTS: usize = 256;
pub const MAX_REASON: usize = 512;

/// ADR 0018: what this server advertises in `SessionReady.capabilities`.
/// ADR 0033 §12: `lmcache`, so a host sends its LMCache samples.
pub fn server_capabilities() -> Vec<String> {
    vec![LIVE_PROFILE_UPDATE.to_owned(), LMCACHE.to_owned()]
}

/// Which way a feature's messages flow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// The server sends it; the host must declare it before it is sent.
    ServerToHost,
    /// The host sends it; absence already means "not reported".
    HostToServer,
}

/// Every post-baseline feature this build knows.
pub const CATALOGUE: &[(&str, Direction)] = &[
    (HEARTBEATS, Direction::ServerToHost),
    (MODEL_SOURCES, Direction::ServerToHost),
    (CHECKPOINT_DIGEST, Direction::ServerToHost),
    (CHECKPOINT_SIZE_ONLY, Direction::ServerToHost),
    (STARTUP_BYTES, Direction::ServerToHost),
    (INSTANCE_INDEX, Direction::ServerToHost),
    (RESTORE_CHECKPOINT_DIGEST, Direction::ServerToHost),
    (TERMINATE_RECORDED_PROCESSES, Direction::ServerToHost),
    (LAUNCH_FAILURE, Direction::HostToServer),
    (POLICY_REFUSAL, Direction::HostToServer),
    (LOAD_LATENCY, Direction::HostToServer),
    (HOST_DRAINING, Direction::HostToServer),
    (PROCESS_RESIDENCY, Direction::HostToServer),
    (INSTALLATION_FINGERPRINT, Direction::HostToServer),
    (LIVE_PROFILE_UPDATE, Direction::ServerToHost),
    (DEVICE_MEMORY_DOMAINS, Direction::ServerToHost),
    (CHECKPOINT_STATE_SLOT, Direction::ServerToHost),
    (ENGINE_GROUPS, Direction::ServerToHost),
    (ENGINE_LOG_TAIL, Direction::ServerToHost),
    (CHECKPOINT_TABLES, Direction::ServerToHost),
    (CHECKPOINT_GGUF, Direction::ServerToHost),
    (DECISIONS, Direction::ServerToHost),
    (EMBEDDINGS, Direction::ServerToHost),
    (CUSTOM_ENGINES, Direction::ServerToHost),
    (STARTUP_HOST_BYTES, Direction::ServerToHost),
    (LMCACHE, Direction::ServerToHost),
    (RETENTION, Direction::ServerToHost),
    (REQUEST_ABORT, Direction::ServerToHost),
    (LOCAL_TENSOR_PARALLEL, Direction::ServerToHost),
];

/// What this build's agent declares: it implements every feature it knows.
pub fn agent_capabilities() -> Vec<String> {
    CATALOGUE
        .iter()
        .map(|(name, _)| (*name).to_owned())
        .collect()
}

/// The server-to-host features a host must have for this server to place new
/// work on it: every launch carries the recorded checkpoint digest (WE3) and
/// its startup reservation, and every wake carries the recorded digest. A
/// host without them could only have its existing work stopped. A Terminate's
/// recorded identities are not among them: the server leaves them out for a
/// host without the feature, which then acts on its own journal as before.
pub const PLACEMENT_REQUIRED: &[&str] =
    &[CHECKPOINT_DIGEST, STARTUP_BYTES, RESTORE_CHECKPOINT_DIGEST];

/// The most capability names one Connect may declare.
pub const MAX_DECLARED: usize = 64;

/// The capability set a Connect declares: its list plus the two earlier
/// boolean flags. `None` when the list is malformed (too long, or a name that
/// is not a short lowercase identifier); the session is refused then. A name
/// this build does not know is kept: it grants nothing here.
pub fn declared(connect: &pb::Connect) -> Option<BTreeSet<String>> {
    if connect.capabilities.len() > MAX_DECLARED
        || !connect.capabilities.iter().all(|name| {
            !name.is_empty()
                && name.len() <= 64
                && name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
    {
        return None;
    }
    let mut set: BTreeSet<String> = connect.capabilities.iter().cloned().collect();
    if connect.heartbeats {
        set.insert(HEARTBEATS.into());
    }
    if connect.model_sources {
        set.insert(MODEL_SOURCES.into());
    }
    Some(set)
}

/// The server-to-host features `command` carries, in catalogue order. An
/// absent (default) field needs nothing: it encodes exactly as the baseline.
/// The exception is a group action: every Prepare and Launch always needs
/// `engine_groups` (ADR 0028 §14), whatever its plan holds.
pub fn required(command: &pb::ExecuteMember) -> Vec<&'static str> {
    use pb::execute_member::Action;
    let mut needs = Vec::new();
    match &command.action {
        Some(Action::LaunchSingle(plan)) => {
            if !plan.checkpoint_digest.is_empty() || plan.checkpoint_weights_bytes.is_some() {
                needs.push(CHECKPOINT_DIGEST);
            }
            if plan.startup_bytes.is_some() {
                needs.push(STARTUP_BYTES);
            }
            if plan.startup_host_bytes.is_some() {
                needs.push(STARTUP_HOST_BYTES);
            }
            if plan.checkpoint_state_slot_bytes.is_some() {
                needs.push(CHECKPOINT_STATE_SLOT);
            }
            // ADR 0035 §§11, 12: a launch on several GPUs of one host is a
            // single-host launch, never a group: its device list (and the
            // layout its rank share is sized with) needs
            // `local_tensor_parallel`, never `engine_groups`.
            if !plan.devices.is_empty() {
                needs.push(LOCAL_TENSOR_PARALLEL);
            } else if plan.checkpoint_layout.is_some() {
                needs.push(ENGINE_GROUPS);
            }
            if plan.checkpoint_tables.is_some() {
                needs.push(CHECKPOINT_TABLES);
            }
            if plan.checkpoint_gguf.is_some() {
                needs.push(CHECKPOINT_GGUF);
            }
            // ADR 0033 §§3, 6: an older host would drop the grant.
            if plan.lmcache.is_some() {
                needs.push(LMCACHE);
            }
            lmcache_needs(&plan.deployment_config, &mut needs);
            api_needs(&plan.deployment_config, &mut needs);
        }
        // ADR 0028 §14: a group plan carries fields an older host would drop.
        Some(Action::Prepare(_) | Action::Launch(_)) => needs.push(ENGINE_GROUPS),
        Some(Action::DigestCheckpoint(request)) => {
            needs.push(CHECKPOINT_DIGEST);
            if request.size_only {
                needs.push(CHECKPOINT_SIZE_ONLY);
            }
        }
        Some(Action::MaterializeSource(_)) => needs.push(MODEL_SOURCES),
        Some(Action::EngineLogTail(_)) => needs.push(ENGINE_LOG_TAIL),
        Some(Action::AbortRequest(_)) => needs.push(REQUEST_ABORT),
        _ => {}
    }
    if command
        .identity
        .as_ref()
        .is_some_and(|id| id.instance_index != 0)
    {
        needs.push(INSTANCE_INDEX);
    }
    if !command.restore_checkpoint_digest.is_empty() {
        needs.push(RESTORE_CHECKPOINT_DIGEST);
    }
    if !command.terminate_recorded_processes.is_empty() {
        needs.push(TERMINATE_RECORDED_PROCESSES);
    }
    // ADR 0028 §9, §14 (decided 2026-10-06): an older host would drop the
    // completion probe's token bound and answer a readiness probe instead.
    if command.probe_max_tokens != 0 {
        needs.push(ENGINE_GROUPS);
    }
    // ADR 0030 §1, ADR 0017: an older host would drop the decision flag and
    // answer a readiness probe instead.
    if command.probe_decision {
        needs.push(DECISIONS);
    }
    // ADR 0014 amendment A20: a group member's own launch carries the tables
    // as a single launch does.
    if let Some(plan) = command.group_member_launch.as_ref() {
        if plan.checkpoint_tables.is_some() {
            needs.push(CHECKPOINT_TABLES);
        }
        if plan.lmcache.is_some() {
            needs.push(LMCACHE);
        }
        lmcache_needs(&plan.deployment_config, &mut needs);
        api_needs(&plan.deployment_config, &mut needs);
    }
    needs.sort_by_key(|need| CATALOGUE.iter().position(|(name, _)| name == need));
    // ADR 0033: a grant and `lmcache: required` both name `lmcache`.
    needs.dedup();
    needs
}

/// ADR 0033 §13: what a launch's deployment document needs of the host: a
/// deployment with `lmcache: required` needs `lmcache`, one with
/// `retention: zero` needs `retention`. An older host would not honour
/// either. A document that does not parse needs nothing here; the host
/// refuses it on its own.
fn lmcache_needs(deployment_config: &str, needs: &mut Vec<&'static str>) {
    use capyctl_config::lmcache::{deployment_settings, LmcacheMode, Retention};
    // Only a document that names either setting is parsed.
    if !deployment_config.contains("lmcache") && !deployment_config.contains("retention") {
        return;
    }
    let Ok(document) = serde_json::from_str::<serde_json::Value>(deployment_config) else {
        return;
    };
    let (mode, retention) = deployment_settings(&document);
    if mode == LmcacheMode::Required {
        needs.push(LMCACHE);
    }
    if retention == Retention::Zero {
        needs.push(RETENTION);
    }
}

/// ADR 0036 §1: a launch whose deployment document declares
/// `api: embeddings` needs `embeddings`; an older host would launch it as
/// chat and prove it with a chat probe. A document that does not parse
/// needs nothing here; the host refuses it on its own.
fn api_needs(deployment_config: &str, needs: &mut Vec<&'static str>) {
    // Only a document that names the value is parsed.
    if !deployment_config.contains("embeddings") {
        return;
    }
    let Ok(document) = serde_json::from_str::<serde_json::Value>(deployment_config) else {
        return;
    };
    if document.get("api").and_then(serde_json::Value::as_str)
        == Some(capyctl_config::effective::DeploymentApi::Embeddings.name())
    {
        needs.push(EMBEDDINGS);
    }
}

/// ADR 0017: the actions a drain-only host may still be sent. They stop,
/// close, re-prove or observe what the server already owns there; none of
/// them places, starts, wakes, parks, measures or downloads anything. An
/// engine log tail (SPEC §13.3) only reads what a retained launch wrote; a
/// request abort (ADR 0032 §3) only stops work a retained launch is running.
pub fn drain_only_permits(command: &pb::ExecuteMember) -> bool {
    use pb::execute_member::Action;
    matches!(
        command.action,
        Some(
            Action::Inspect(_)
                | Action::TerminateOwnedHandle(_)
                | Action::CloseIngress(_)
                | Action::ProbeOwnedHandle(_)
                | Action::EngineLogTail(_)
                | Action::AbortRequest(_)
        )
    )
}

/// The typed refusal for a host that is drain-only under the version policy.
pub const HOST_UPGRADE_REQUIRED: &str = "host_upgrade_required";
/// The prefix of the typed refusal for a missing capability.
pub const HOST_CAPABILITY_MISSING: &str = "host_capability_missing";

/// `host_capability_missing:<name>`.
pub fn missing(name: &str) -> String {
    format!("{HOST_CAPABILITY_MISSING}:{name}")
}

/// Whether `reason` is one of this module's typed refusals.
pub fn is_gate_refusal(reason: &str) -> bool {
    reason == HOST_UPGRADE_REQUIRED
        || reason
            .strip_prefix(HOST_CAPABILITY_MISSING)
            .and_then(|rest| rest.strip_prefix(':'))
            .is_some_and(|name| CATALOGUE.iter().any(|(known, _)| *known == name))
}

/// ADR 0028 §14: the typed refusal for placing a group member on a host that
/// did not declare `engine_groups`, or `None` when it may be placed. The server
/// calls this before it sends anything for the group.
pub fn group_refusal(host_capabilities: &BTreeSet<String>) -> Option<String> {
    (!host_capabilities.contains(ENGINE_GROUPS)).then(|| missing(ENGINE_GROUPS))
}

/// ADR 0030 §1: the typed refusal for placing a decisions deployment on a
/// host that did not declare `decisions`, or `None` when it may be placed.
pub fn decisions_refusal(host_capabilities: &BTreeSet<String>) -> Option<String> {
    (!host_capabilities.contains(DECISIONS)).then(|| missing(DECISIONS))
}

/// ADR 0036 §1: the typed refusal for placing an embeddings deployment on a
/// host that did not declare `embeddings`, or `None` when it may be placed.
pub fn embeddings_refusal(host_capabilities: &BTreeSet<String>) -> Option<String> {
    (!host_capabilities.contains(EMBEDDINGS)).then(|| missing(EMBEDDINGS))
}

/// ADR 0031 §9: the typed refusal for placing a deployment on a custom
/// engine on a host that did not declare `custom_engines`, or `None` when it
/// may be placed. A decisions custom engine needs [`decisions_refusal`] too.
pub fn custom_engines_refusal(host_capabilities: &BTreeSet<String>) -> Option<String> {
    (!host_capabilities.contains(CUSTOM_ENGINES)).then(|| missing(CUSTOM_ENGINES))
}

/// ADR 0033 §§9, 10, 13: the typed refusal for placing a deployment with
/// these declared settings on a host with these settings and capabilities,
/// or `None` when it may be placed ([`capyctl_config::lmcache::placement_refusal`]).
pub fn lmcache_refusal(
    host_capabilities: &BTreeSet<String>,
    mode: capyctl_config::lmcache::LmcacheMode,
    retention: capyctl_config::lmcache::Retention,
    host_lmcache: capyctl_config::lmcache::HostLmcache,
    host_retention: capyctl_config::lmcache::Retention,
) -> Option<&'static str> {
    capyctl_config::lmcache::placement_refusal(
        mode,
        retention,
        host_lmcache,
        host_retention,
        |name| host_capabilities.contains(name),
    )
}

/// ADR 0035 §11: the typed refusal for placing a launch on `devices` GPUs of
/// one host on a host that did not declare `local_tensor_parallel`, or `None`
/// when it may be placed. A launch on one GPU needs nothing new.
pub fn local_tensor_parallel_refusal(
    host_capabilities: &BTreeSet<String>,
    devices: usize,
) -> Option<String> {
    (devices > 1 && !host_capabilities.contains(LOCAL_TENSOR_PARALLEL))
        .then(|| missing(LOCAL_TENSOR_PARALLEL))
}

/// ADR 0017: why `command` may not be sent to a host with this policy state
/// and capability set, or `None` when it may.
pub fn refusal(
    drain_only: bool,
    capabilities: &BTreeSet<String>,
    command: &pb::ExecuteMember,
) -> Option<String> {
    if drain_only && !drain_only_permits(command) {
        return Some(HOST_UPGRADE_REQUIRED.into());
    }
    required(command)
        .into_iter()
        .find(|need| !capabilities.contains(*need))
        .map(missing)
}

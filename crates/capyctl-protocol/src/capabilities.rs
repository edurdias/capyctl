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
/// plans' engine, topology, generation, role, model path and worker port, and
/// `ReportInventory.group`. The server places a group member only on a host
/// that declared it; any other host is refused
/// `host_capability_missing:engine_groups` before anything is sent.
pub const ENGINE_GROUPS: &str = "engine_groups";

/// ADR 0018: bounds on the new messages' strings and lists.
pub const MAX_REQUEST_ID: usize = 64;
pub const MAX_RETIREMENT_DEPLOYMENTS: usize = 256;
pub const MAX_REASON: usize = 512;

/// ADR 0018: what this server advertises in `SessionReady.capabilities`.
pub fn server_capabilities() -> Vec<String> {
    vec![LIVE_PROFILE_UPDATE.to_owned()]
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
            if plan.checkpoint_state_slot_bytes.is_some() {
                needs.push(CHECKPOINT_STATE_SLOT);
            }
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
    needs.sort_by_key(|need| CATALOGUE.iter().position(|(name, _)| name == need));
    needs
}

/// ADR 0017: the actions a drain-only host may still be sent. They stop,
/// close, re-prove or observe what the server already owns there; none of
/// them places, starts, wakes, parks, measures or downloads anything.
pub fn drain_only_permits(command: &pb::ExecuteMember) -> bool {
    use pb::execute_member::Action;
    matches!(
        command.action,
        Some(
            Action::Inspect(_)
                | Action::TerminateOwnedHandle(_)
                | Action::CloseIngress(_)
                | Action::ProbeOwnedHandle(_)
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

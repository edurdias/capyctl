//! A fail-closed typed boundary; generated protobuf messages confer no authority.
//! This validates shape only. The authenticated agent's durable acceptance path
//! must recompute the canonical payload digest, fence generations and deduplicate
//! command identities before acknowledging or performing any effect.
use crate::pb;
use capyctl_domain::group::{
    CommandIdentity, GroupEngine, GroupIdentityError, GroupPlan, GroupTopology, MemberKey,
    MemberPlan, MemberRole,
};

/// SPEC §§3, 7, 13: only local approved profile resolution may render a launch.
/// The server supplies immutable requests and a grant identity, never native argv.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SingleLaunchPlan {
    pub deployment_config: String,
    pub profile_name: String,
    pub checkpoint_fingerprint: String,
    pub host_policy_fingerprint: String,
    pub binding_id: String,
    pub incarnation: String,
    pub grant_id: String,
    pub service_port: u16,
    pub issued_at_ms: i64,
    pub coordinator_session_id: String,
    /// ADR 0014 §7 (WE3): the recorded checkpoint digest the host must measure
    /// before launching or waking this launch. Empty only in a command
    /// journaled before WE3, which may be adopted but never launched or woken.
    pub checkpoint_digest: String,
    /// ADR 0014 §5: the weights bytes the server resolved this revision with,
    /// so both sides resolve the same memory request.
    pub checkpoint_weights_bytes: Option<i64>,
    /// Owner decision 2026-09-23: the startup peak the server reserves for
    /// this launch from arm until Ready. The host charges it for the launch's
    /// starting phase instead of its own placeholder, never below the steady
    /// request it resolves itself.
    pub startup_bytes: Option<i64>,
    /// ADR 0014 amendment A16: the hybrid state slot the server resolved this
    /// revision with, beside the weights.
    pub checkpoint_state_slot_bytes: Option<i64>,
}
/// ADR 0014 §7: an empty digest (pre-WE3 journal) or a canonical one; weights
/// only alongside a digest, and never negative.
fn recorded_checkpoint_ok(digest: &str, weights: Option<i64>) -> bool {
    (digest.is_empty() || capyctl_config::effective::is_checkpoint_digest(digest))
        && weights.is_none_or(|bytes| bytes >= 0 && !digest.is_empty())
}
impl TryFrom<pb::SingleLaunchPlan> for SingleLaunchPlan {
    type Error = GroupIdentityError;
    fn try_from(plan: pb::SingleLaunchPlan) -> Result<Self, Self::Error> {
        Self::decode(plan, false)
    }
}
impl SingleLaunchPlan {
    /// The wire plan, validated. ADR 0028 §8 (ruling R29): a group member's
    /// launch (`group_member`) may name no service port, as a worker serves
    /// nothing; every other field is held to the single-rank rules.
    fn decode(plan: pb::SingleLaunchPlan, group_member: bool) -> Result<Self, GroupIdentityError> {
        fn ulid(value: &str) -> bool {
            value.len() == 26
                && value
                    .bytes()
                    .all(|b| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&b))
                && value.as_bytes()[0] <= b'7'
        }
        if plan.deployment_config.len() > 24 * 1024
            || plan.profile_name.trim().is_empty()
            || plan.profile_name.len() > 256
            || plan.checkpoint_fingerprint.trim().is_empty()
            || plan.checkpoint_fingerprint.len() > 256
            || plan.host_policy_fingerprint.len() != 64
            || !plan
                .host_policy_fingerprint
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || !ulid(&plan.coordinator_session_id)
            || !ulid(&plan.binding_id)
            || !ulid(&plan.incarnation)
            || !ulid(&plan.grant_id)
            || (plan.service_port == 0 && !group_member)
            || plan.issued_at_unix_ms < 0
            || !recorded_checkpoint_ok(&plan.checkpoint_digest, plan.checkpoint_weights_bytes)
            || plan.startup_bytes.is_some_and(|bytes| bytes <= 0)
            || plan
                .checkpoint_state_slot_bytes
                .is_some_and(|bytes| bytes <= 0 || plan.checkpoint_digest.is_empty())
        {
            return Err(GroupIdentityError);
        }
        let config = capyctl_config::parse_strict(
            capyctl_config::ConfigKind::Deployment,
            &plan.deployment_config,
        )
        .map_err(|_| GroupIdentityError)?;
        Ok(Self {
            deployment_config: serde_json::to_string(&config).map_err(|_| GroupIdentityError)?,
            profile_name: plan.profile_name,
            checkpoint_fingerprint: plan.checkpoint_fingerprint,
            host_policy_fingerprint: plan.host_policy_fingerprint,
            binding_id: plan.binding_id,
            incarnation: plan.incarnation,
            grant_id: plan.grant_id,
            service_port: plan
                .service_port
                .try_into()
                .map_err(|_| GroupIdentityError)?,
            issued_at_ms: plan.issued_at_unix_ms,
            coordinator_session_id: plan.coordinator_session_id,
            checkpoint_digest: plan.checkpoint_digest,
            checkpoint_weights_bytes: plan.checkpoint_weights_bytes,
            startup_bytes: plan.startup_bytes,
            checkpoint_state_slot_bytes: plan.checkpoint_state_slot_bytes,
        })
    }
    fn to_wire(&self) -> pb::SingleLaunchPlan {
        pb::SingleLaunchPlan {
            deployment_config: capyctl_config::parse_strict(
                capyctl_config::ConfigKind::Deployment,
                &self.deployment_config,
            )
            .ok()
            .and_then(|value| serde_json::to_string(&value).ok())
            .unwrap_or_else(|| self.deployment_config.clone()),
            profile_name: self.profile_name.clone(),
            checkpoint_fingerprint: self.checkpoint_fingerprint.clone(),
            host_policy_fingerprint: self.host_policy_fingerprint.clone(),
            binding_id: self.binding_id.clone(),
            incarnation: self.incarnation.clone(),
            grant_id: self.grant_id.clone(),
            service_port: self.service_port.into(),
            issued_at_unix_ms: self.issued_at_ms,
            coordinator_session_id: self.coordinator_session_id.clone(),
            checkpoint_digest: self.checkpoint_digest.clone(),
            checkpoint_weights_bytes: self.checkpoint_weights_bytes,
            startup_bytes: self.startup_bytes,
            checkpoint_state_slot_bytes: self.checkpoint_state_slot_bytes,
        }
    }
}

/// ADR 0014 §7 (WE3): measure one deployment's checkpoint on its host. Like a
/// launch plan it carries the deployment document, never a path: the host
/// locates the checkpoint from its own approved model store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DigestCheckpointPlan {
    pub deployment_config: String,
    pub host_policy_fingerprint: String,
    /// The recorded or declared digest the result is compared with, if any.
    pub expected_digest: Option<String>,
    /// Owner decision 2026-09-23 (solo first start): size the weight files
    /// only (a stat walk, no hashing); the answer is `sized`.
    pub size_only: bool,
}
impl TryFrom<pb::DigestCheckpointRequest> for DigestCheckpointPlan {
    type Error = GroupIdentityError;
    fn try_from(plan: pb::DigestCheckpointRequest) -> Result<Self, Self::Error> {
        if plan.deployment_config.len() > 24 * 1024
            || plan.host_policy_fingerprint.len() != 64
            || !plan
                .host_policy_fingerprint
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || !(plan.expected_digest.is_empty()
                || capyctl_config::effective::is_checkpoint_digest(&plan.expected_digest))
        {
            return Err(GroupIdentityError);
        }
        let config = capyctl_config::parse_strict(
            capyctl_config::ConfigKind::Deployment,
            &plan.deployment_config,
        )
        .map_err(|_| GroupIdentityError)?;
        Ok(Self {
            deployment_config: serde_json::to_string(&config).map_err(|_| GroupIdentityError)?,
            host_policy_fingerprint: plan.host_policy_fingerprint,
            expected_digest: (!plan.expected_digest.is_empty()).then_some(plan.expected_digest),
            size_only: plan.size_only,
        })
    }
}
impl DigestCheckpointPlan {
    fn to_wire(&self) -> pb::DigestCheckpointRequest {
        pb::DigestCheckpointRequest {
            deployment_config: capyctl_config::parse_strict(
                capyctl_config::ConfigKind::Deployment,
                &self.deployment_config,
            )
            .ok()
            .and_then(|value| serde_json::to_string(&value).ok())
            .unwrap_or_else(|| self.deployment_config.clone()),
            host_policy_fingerprint: self.host_policy_fingerprint.clone(),
            expected_digest: self.expected_digest.clone().unwrap_or_default(),
            size_only: self.size_only,
        }
    }
}

/// ADR 0008 (additive): materialize one deployment's declared remote model
/// source on its host, or report on it. Like a digest request it carries the
/// deployment document, never a path or a secret; the host checks its own
/// model-source policy. `source_key` is derived from the document, never
/// taken from the wire, and binds the result to the source it answers for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterializeSourcePlan {
    pub deployment_config: String,
    pub host_policy_fingerprint: String,
    pub source_key: String,
}
impl MaterializeSourcePlan {
    /// A plan for `deployment_config`, whose `model` must name a valid remote
    /// source. `None` otherwise.
    pub fn new(deployment_config: &str, host_policy_fingerprint: &str) -> Option<Self> {
        Self::try_from(pb::MaterializeSourceRequest {
            deployment_config: deployment_config.into(),
            host_policy_fingerprint: host_policy_fingerprint.into(),
        })
        .ok()
    }
    /// The declared source this plan names.
    pub fn source(&self) -> Option<capyctl_config::model_source::ModelSource> {
        let value: serde_json::Value = serde_json::from_str(&self.deployment_config).ok()?;
        remote_source(&value)
    }
    fn to_wire(&self) -> pb::MaterializeSourceRequest {
        pb::MaterializeSourceRequest {
            deployment_config: self.deployment_config.clone(),
            host_policy_fingerprint: self.host_policy_fingerprint.clone(),
        }
    }
}
/// The deployment's remote `model.source`, validated; `None` for a local one.
fn remote_source(
    deployment: &serde_json::Value,
) -> Option<capyctl_config::model_source::ModelSource> {
    let source: capyctl_config::model_source::ModelSource =
        serde_json::from_value(deployment.get("model")?.get("source")?.clone()).ok()?;
    (source.is_remote() && source.validate().is_ok()).then_some(source)
}
impl TryFrom<pb::MaterializeSourceRequest> for MaterializeSourcePlan {
    type Error = GroupIdentityError;
    fn try_from(plan: pb::MaterializeSourceRequest) -> Result<Self, Self::Error> {
        if plan.deployment_config.len() > 24 * 1024
            || plan.host_policy_fingerprint.len() != 64
            || !plan
                .host_policy_fingerprint
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(GroupIdentityError);
        }
        let config = capyctl_config::parse_strict(
            capyctl_config::ConfigKind::Deployment,
            &plan.deployment_config,
        )
        .map_err(|_| GroupIdentityError)?;
        let source = remote_source(&config).ok_or(GroupIdentityError)?;
        Ok(Self {
            deployment_config: serde_json::to_string(&config).map_err(|_| GroupIdentityError)?,
            host_policy_fingerprint: plan.host_policy_fingerprint,
            source_key: source.store_key().ok_or(GroupIdentityError)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemberAction {
    Prepare(GroupPlan),
    /// ADR 0028 §8 (ruling R29): launch this host's member of a group plan.
    /// `member` is this host's own launch of it, carried as a single-rank
    /// launch is (deployment document, binding, incarnation, grant, checkpoint
    /// facts); the host resolves and renders it from its own policy and the
    /// plan's group arguments.
    Launch {
        plan: GroupPlan,
        member: SingleLaunchPlan,
    },
    LaunchSingle(SingleLaunchPlan),
    Inspect,
    /// SPEC §§6.1, 13.2: terminate one owned launch and report its processes'
    /// presence. ADR 0016: `recorded` carries the identities the server
    /// recorded for the launch. A host whose journal knows the handle acts on
    /// its own record only; one with no record of it (a lost journal) never
    /// signals anything and only observes these identities, so the launch can
    /// still be settled on gone evidence by identity, and never released
    /// while any of them is alive.
    Terminate {
        owned_handle: String,
        recorded: Vec<capyctl_domain::completion::ProcessIdentity>,
    },
    CloseIngress,
    /// SPEC §§6.1, 13.2: re-prove model readiness of one retained launch with a
    /// fresh native probe, after a session loss cleared its readiness authority.
    /// It names the launch by its owned handle and can never launch or release.
    Probe {
        owned_handle: String,
    },
    /// SPEC §§9.1, 10 (protocol version 2): park one retained, drained launch in
    /// place. The process group and its reservation stay owned; a park never
    /// launches, releases or proves a usable model.
    Park {
        owned_handle: String,
    },
    /// SPEC §§6.1, 9.1 (protocol version 2): restore one parked launch in place.
    /// Only a fresh native probe after the restore may claim a usable model.
    ///
    /// ADR 0014 §7, owner decision 5 (2026-09-22): `checkpoint_digest` is the
    /// digest the server recorded for the launch's revision, or empty. A launch
    /// journaled before WE3 has none in its plan and is woken against this one;
    /// a launch whose plan records one must be sent the same or none.
    Restore {
        owned_handle: String,
        checkpoint_digest: String,
    },
    /// ADR 0014 §7 (WE3, protocol version 2, additive): measure one
    /// deployment's checkpoint. Read-only: it never launches, releases or
    /// changes anything, and its result carries only digest evidence.
    DigestCheckpoint(DigestCheckpointPlan),
    /// ADR 0008 (additive): materialize (or report on) one deployment's
    /// declared remote model source. It never launches, releases or reserves
    /// engine resources; the store bytes a download reserves are the host's
    /// own filesystem accounting. Its result carries only source evidence.
    MaterializeSource(MaterializeSourcePlan),
}

impl MemberAction {
    /// The single-rank launch plan a launch action carries: a LaunchSingle's
    /// own, or a group Launch's member launch (ADR 0028 §8). `None` for any
    /// other action.
    pub fn launch_plan(&self) -> Option<&SingleLaunchPlan> {
        match self {
            Self::LaunchSingle(plan) | Self::Launch { member: plan, .. } => Some(plan),
            _ => None,
        }
    }

    /// The group plan of a group Launch; `None` for any other action.
    pub fn group_launch(&self) -> Option<&GroupPlan> {
        match self {
            Self::Launch { plan, .. } => Some(plan),
            _ => None,
        }
    }
}

/// ADR 0028 §8 (ruling R29): whether `member` is a coherent launch of the
/// member `key` names in `plan`: that member's profile, its recorded
/// checkpoint (the group plan records the checkpoint digest) and its leased
/// port, the head's service port or none on a worker.
pub fn group_member_launch_ok(
    plan: &GroupPlan,
    key: &MemberKey,
    member: &SingleLaunchPlan,
) -> bool {
    plan.members().iter().any(|m| {
        m.member == *key
            && m.profile_name == member.profile_name
            && !member.checkpoint_digest.is_empty()
            && m.checkpoint_fingerprint == member.checkpoint_digest
            && member.service_port == m.service_port.unwrap_or(0)
    })
}

/// The most recorded process identities one Terminate may carry.
pub const MAX_RECORDED_PROCESSES: usize = 64;

/// ADR 0016: a Terminate's recorded identities are bounded, well formed and
/// distinct, like the process observations a result may carry.
fn recorded_processes(
    wire: Vec<pb::RecordedProcess>,
) -> Result<Vec<capyctl_domain::completion::ProcessIdentity>, GroupIdentityError> {
    if wire.len() > MAX_RECORDED_PROCESSES {
        return Err(GroupIdentityError);
    }
    let mut seen = std::collections::BTreeSet::new();
    wire.into_iter()
        .map(|p| {
            if p.pid == 0
                || p.start_ticks == 0
                || p.start_ticks > i64::MAX as u64
                || p.role.is_empty()
                || p.role.len() > 64
                || p.boot_id.is_empty()
                || p.boot_id.len() > 64
                || !seen.insert((p.pid, p.boot_id.clone(), p.start_ticks))
            {
                return Err(GroupIdentityError);
            }
            Ok(capyctl_domain::completion::ProcessIdentity {
                role: p.role,
                pid: p.pid,
                boot_id: p.boot_id,
                start_ticks: p.start_ticks,
            })
        })
        .collect()
}

/// Owned handles name a retained launch; they are bounded and never blank.
fn owned_handle_ok(handle: &str) -> bool {
    !handle.trim().is_empty() && handle.len() <= 4096
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberCommand {
    pub identity: CommandIdentity,
    pub action: MemberAction,
}
impl TryFrom<pb::GroupLaunchPlan> for GroupPlan {
    type Error = GroupIdentityError;
    // ADR 0028 §14: every field is decoded from the wire and the plan is
    // validated by `GroupPlan::new`; nothing is assumed. A legacy group entry
    // without role or engine is refused: no production path sent a group Launch
    // before ADR 0028.
    fn try_from(plan: pb::GroupLaunchPlan) -> Result<Self, Self::Error> {
        let port = |value: u32| -> Result<Option<u16>, GroupIdentityError> {
            match value {
                0 => Ok(None),
                value => value.try_into().map(Some).map_err(|_| GroupIdentityError),
            }
        };
        let members = plan
            .members
            .into_iter()
            .map(|m| {
                Ok(MemberPlan {
                    member: MemberKey {
                        host_id: m.host_id,
                        member_id: m.member_id,
                    },
                    rank: m.rank,
                    role: match m.role.as_str() {
                        "head" => MemberRole::Head,
                        "worker" => MemberRole::Worker,
                        _ => return Err(GroupIdentityError),
                    },
                    profile_name: m.profile_name,
                    profile_fingerprint: m.profile_fingerprint,
                    checkpoint_fingerprint: m.checkpoint_fingerprint,
                    model_path: m.model_path,
                    devices: m.devices,
                    peer_address: m.peer_address.parse().map_err(|_| GroupIdentityError)?,
                    // A worker serves no API; the wire writes its port as zero.
                    service_port: port(m.service_port)?,
                    worker_port: port(m.worker_port)?,
                })
            })
            .collect::<Result<Vec<_>, GroupIdentityError>>()?;
        let engine = match plan.engine.as_str() {
            "vllm" => GroupEngine::Vllm,
            "sglang" => GroupEngine::Sglang,
            "tensorfold" => GroupEngine::Tensorfold,
            _ => return Err(GroupIdentityError),
        };
        Self::new(
            engine,
            members,
            GroupTopology {
                tensor_parallel: plan.tensor_parallel,
                pipeline_parallel: plan.pipeline_parallel,
                local_ranks: plan.local_ranks,
            },
            plan.rendezvous_port
                .try_into()
                .map_err(|_| GroupIdentityError)?,
            plan.generation,
        )
    }
}
impl TryFrom<pb::ServerToAgent> for MemberCommand {
    type Error = GroupIdentityError;
    fn try_from(message: pb::ServerToAgent) -> Result<Self, Self::Error> {
        // SPEC §13.3: arbitrary argv/env and legacy messages are never execution authority.
        let Some(pb::server_to_agent::Msg::ExecuteMember(command)) = message.msg else {
            return Err(GroupIdentityError);
        };
        let id = command.identity.ok_or(GroupIdentityError)?;
        if id.protocol_version != crate::COMMAND_ENCODING_VERSION {
            return Err(GroupIdentityError);
        }
        let identity = CommandIdentity {
            controller_id: id.controller_id,
            member: MemberKey {
                host_id: id.host_id,
                member_id: id.member_id,
            },
            deployment_id: id.deployment_id,
            operation_id: id.operation_id,
            command_id: id.command_id,
            step_id: id.step_id,
            generation: id.generation,
            revision: id.revision,
            deadline_ms: id.deadline_unix_ms,
            expected_state: id.expected_state,
            profile_fingerprint: id.profile_fingerprint,
            payload_digest: id
                .payload_digest
                .try_into()
                .map_err(|_| GroupIdentityError)?,
            instance_index: id.instance_index,
        };
        identity.validate()?;
        use pb::execute_member::Action;
        let restore_digest = command.restore_checkpoint_digest;
        let recorded = recorded_processes(command.terminate_recorded_processes)?;
        if !recorded.is_empty() && !matches!(command.action, Some(Action::TerminateOwnedHandle(_)))
        {
            return Err(GroupIdentityError);
        }
        if !(restore_digest.is_empty()
            || capyctl_config::effective::is_checkpoint_digest(&restore_digest))
            || (!restore_digest.is_empty()
                && !matches!(command.action, Some(Action::RestoreOwnedHandle(_))))
        {
            return Err(GroupIdentityError);
        }
        // ADR 0028 §8 (ruling R29): a group Launch carries this host's member
        // launch; nothing else does.
        let mut member_launch = command.group_member_launch;
        if member_launch.is_some() != matches!(command.action, Some(Action::Launch(_))) {
            return Err(GroupIdentityError);
        }
        let action = match command.action.ok_or(GroupIdentityError)? {
            Action::Prepare(plan) => MemberAction::Prepare(plan.try_into()?),
            Action::Launch(plan) => MemberAction::Launch {
                plan: plan.try_into()?,
                member: SingleLaunchPlan::decode(
                    member_launch.take().ok_or(GroupIdentityError)?,
                    true,
                )?,
            },
            Action::LaunchSingle(plan) => MemberAction::LaunchSingle(plan.try_into()?),
            Action::Inspect(true) => MemberAction::Inspect,
            Action::TerminateOwnedHandle(owned_handle) if owned_handle_ok(&owned_handle) => {
                MemberAction::Terminate {
                    owned_handle,
                    recorded,
                }
            }
            Action::CloseIngress(true) => MemberAction::CloseIngress,
            Action::ProbeOwnedHandle(owned_handle) if owned_handle_ok(&owned_handle) => {
                MemberAction::Probe { owned_handle }
            }
            Action::ParkOwnedHandle(owned_handle) if owned_handle_ok(&owned_handle) => {
                MemberAction::Park { owned_handle }
            }
            Action::RestoreOwnedHandle(owned_handle) if owned_handle_ok(&owned_handle) => {
                MemberAction::Restore {
                    owned_handle,
                    checkpoint_digest: restore_digest,
                }
            }
            Action::DigestCheckpoint(plan) => MemberAction::DigestCheckpoint(plan.try_into()?),
            Action::MaterializeSource(plan) => MemberAction::MaterializeSource(plan.try_into()?),
            // SPEC §13.1, T34: an action this build does not know (including a
            // newer peer's field, which decodes as no action) is never guessed.
            _ => return Err(GroupIdentityError),
        };
        if let MemberAction::Prepare(plan) | MemberAction::Launch { plan, .. } = &action {
            if !plan.members().iter().any(|m| {
                m.member == identity.member && m.profile_fingerprint == identity.profile_fingerprint
            }) {
                return Err(GroupIdentityError);
            }
        }
        if let MemberAction::Launch { plan, member } = &action {
            if !group_member_launch_ok(plan, &identity.member, member) {
                return Err(GroupIdentityError);
            }
        }
        if let Some(plan) = action.launch_plan() {
            if plan.issued_at_ms >= identity.deadline_ms {
                return Err(GroupIdentityError);
            }
        }
        Ok(Self { identity, action })
    }
}

impl MemberCommand {
    /// SPEC §13.1: a stable typed encoding for the durable command journal.
    /// Members are ordered by rank; no map ordering or raw argv can alter it.
    pub fn to_wire(&self) -> pb::ExecuteMember {
        use pb::execute_member::Action;
        let identity = &self.identity;
        pb::ExecuteMember {
            identity: Some(pb::CommandIdentity {
                controller_id: identity.controller_id.clone(),
                host_id: identity.member.host_id.clone(),
                member_id: identity.member.member_id.clone(),
                deployment_id: identity.deployment_id.clone(),
                operation_id: identity.operation_id.clone(),
                command_id: identity.command_id.clone(),
                step_id: identity.step_id.clone(),
                generation: identity.generation,
                revision: identity.revision,
                deadline_unix_ms: identity.deadline_ms,
                payload_digest: identity.payload_digest.to_vec(),
                expected_state: identity.expected_state.clone(),
                profile_fingerprint: identity.profile_fingerprint.clone(),
                protocol_version: crate::COMMAND_ENCODING_VERSION.into(),
                // ADR 0013 §5: zero is not encoded, so instance 0 keeps the
                // exact digest every earlier command was journaled with.
                instance_index: identity.instance_index,
            }),
            action: Some(match &self.action {
                MemberAction::Prepare(plan) => Action::Prepare(group_wire(plan)),
                MemberAction::Launch { plan, .. } => Action::Launch(group_wire(plan)),
                MemberAction::LaunchSingle(plan) => Action::LaunchSingle(plan.to_wire()),
                MemberAction::Inspect => Action::Inspect(true),
                MemberAction::Terminate { owned_handle, .. } => {
                    Action::TerminateOwnedHandle(owned_handle.clone())
                }
                MemberAction::CloseIngress => Action::CloseIngress(true),
                MemberAction::Probe { owned_handle } => {
                    Action::ProbeOwnedHandle(owned_handle.clone())
                }
                MemberAction::Park { owned_handle } => {
                    Action::ParkOwnedHandle(owned_handle.clone())
                }
                MemberAction::Restore { owned_handle, .. } => {
                    Action::RestoreOwnedHandle(owned_handle.clone())
                }
                MemberAction::DigestCheckpoint(plan) => Action::DigestCheckpoint(plan.to_wire()),
                MemberAction::MaterializeSource(plan) => Action::MaterializeSource(plan.to_wire()),
            }),
            restore_checkpoint_digest: match &self.action {
                MemberAction::Restore {
                    checkpoint_digest, ..
                } => checkpoint_digest.clone(),
                _ => String::new(),
            },
            terminate_recorded_processes: match &self.action {
                MemberAction::Terminate { recorded, .. } => recorded
                    .iter()
                    .map(|p| pb::RecordedProcess {
                        role: p.role.clone(),
                        pid: p.pid,
                        boot_id: p.boot_id.clone(),
                        start_ticks: p.start_ticks,
                    })
                    .collect(),
                _ => Vec::new(),
            },
            group_member_launch: match &self.action {
                MemberAction::Launch { member, .. } => Some(member.to_wire()),
                _ => None,
            },
        }
    }

    /// Digest all immutable identity and action fields, excluding the digest
    /// field itself. This binds content; it supplies no authentication or grant.
    pub fn canonical_digest(&self) -> [u8; 32] {
        use prost::Message;
        use sha2::{Digest, Sha256};
        let mut wire = self.to_wire();
        if let Some(identity) = &mut wire.identity {
            identity.payload_digest.clear();
        }
        let mut digest = Sha256::new();
        digest.update(b"capyctl/member-command/v1\0");
        digest.update(wire.encode_to_vec());
        digest.finalize().into()
    }

    /// Revalidate public in-process values as well as the content digest.
    /// The agent must separately authenticate, fence and durably deduplicate.
    pub fn verify_digest(&self) -> Result<(), GroupIdentityError> {
        Self::try_from(pb::ServerToAgent {
            msg: Some(pb::server_to_agent::Msg::ExecuteMember(self.to_wire())),
        })?;
        if self.identity.payload_digest != self.canonical_digest() {
            return Err(GroupIdentityError);
        }
        Ok(())
    }
}

fn group_wire(plan: &GroupPlan) -> pb::GroupLaunchPlan {
    let mut members: Vec<_> = plan.members().iter().collect();
    members.sort_by_key(|member| member.rank);
    pb::GroupLaunchPlan {
        members: members
            .into_iter()
            .map(|member| pb::GroupMemberPlan {
                host_id: member.member.host_id.clone(),
                member_id: member.member.member_id.clone(),
                rank: member.rank,
                profile_name: member.profile_name.clone(),
                profile_fingerprint: member.profile_fingerprint.clone(),
                checkpoint_fingerprint: member.checkpoint_fingerprint.clone(),
                devices: member.devices.clone(),
                peer_address: member.peer_address.to_string(),
                service_port: member.service_port.unwrap_or(0).into(),
                role: match member.role {
                    MemberRole::Head => "head",
                    MemberRole::Worker => "worker",
                }
                .into(),
                model_path: member.model_path.clone(),
                worker_port: member.worker_port.unwrap_or(0).into(),
            })
            .collect(),
        rendezvous_port: plan.rendezvous_port().into(),
        engine: plan.engine().as_str().into(),
        tensor_parallel: plan.topology().tensor_parallel,
        pipeline_parallel: plan.topology().pipeline_parallel,
        local_ranks: plan.topology().local_ranks,
        generation: plan.generation(),
    }
}

/// ADR 0014 amendment A12: the most kernel build spans one launch reports.
pub const MAX_KERNEL_BUILDS: usize = 64;

/// SPEC §13: shape and exact command binding only. The caller must establish the
/// authenticated host/session and observation freshness before consuming evidence.
pub fn validate_result(
    command: &MemberCommand,
    result: &pb::MemberExecutionResult,
) -> Result<(), GroupIdentityError> {
    if result.identity != command.to_wire().identity
        || !matches!(
            result.state.as_str(),
            "accepted" | "attempted" | "launched" | "completed" | "tombstone"
        )
        || result.processes.len() > 256
        || result.observed_at_unix_ms < 0
    {
        return Err(GroupIdentityError);
    }
    let mut seen = std::collections::BTreeSet::new();
    for process in &result.processes {
        if process.pid == 0
            || process.start_ticks == 0
            || process.role.is_empty()
            || process.role.len() > 128
            || process.boot_id.is_empty()
            || process.boot_id.len() > 128
            || !matches!(process.presence.as_str(), "alive" | "gone" | "unknown")
            || !seen.insert((process.pid, &process.boot_id, process.start_ticks))
        {
            return Err(GroupIdentityError);
        }
    }
    // ADR 0014 amendment A12: kernel builds belong to a usable launch only
    // (ADR 0028 §9: a group's head is one).
    if !result.kernel_builds.is_empty()
        && (command.action.launch_plan().is_none()
            || !result.model_usable
            || result.kernel_builds.len() > MAX_KERNEL_BUILDS
            || result
                .kernel_builds
                .iter()
                .any(|b| b.from_unix_ms < 0 || b.from_unix_ms > b.until_unix_ms))
    {
        return Err(GroupIdentityError);
    }
    // ADR 0028 §11: escalation is evidence of a completed Terminate only.
    if result.escalated
        && !(matches!(command.action, MemberAction::Terminate { .. })
            && result.state == "completed")
    {
        return Err(GroupIdentityError);
    }
    // A probe, park or restore reports on exactly the launch it names, whatever
    // the outcome.
    if let MemberAction::Probe { owned_handle }
    | MemberAction::Park { owned_handle }
    | MemberAction::Restore { owned_handle, .. } = &command.action
    {
        if result.owned_handle != *owned_handle {
            return Err(GroupIdentityError);
        }
    }
    // Residency evidence belongs to Park and Restore results only.
    let residency = match (&command.action, &result.residency) {
        (MemberAction::Park { .. } | MemberAction::Restore { .. }, residency) => residency.as_ref(),
        (_, None) => None,
        (_, Some(_)) => return Err(GroupIdentityError),
    };
    let claim = match residency {
        Some(evidence) => validate_residency(&command.action, evidence)?,
        None => ResidencyClaim::None,
    };
    // ADR 0014 §7 (WE3): checkpoint evidence belongs to DigestCheckpoint
    // results only, and a DigestCheckpoint result is nothing but that evidence.
    match (&command.action, &result.checkpoint) {
        (MemberAction::DigestCheckpoint(plan), Some(evidence)) => {
            validate_checkpoint(plan, evidence)?;
            if result.state != "completed"
                || result.claim_retained
                || result.model_usable
                || !result.processes.is_empty()
                || !result.owned_handle.is_empty()
                || !result.binding_id.is_empty()
                || !result.incarnation.is_empty()
            {
                return Err(GroupIdentityError);
            }
        }
        (MemberAction::DigestCheckpoint(_), None) => return Err(GroupIdentityError),
        (_, Some(_)) => return Err(GroupIdentityError),
        (_, None) => {}
    }
    // ADR 0008: source evidence belongs to MaterializeSource results only, and
    // such a result is nothing but that evidence.
    match (&command.action, &result.source) {
        (MemberAction::MaterializeSource(plan), Some(evidence)) => {
            validate_source(plan, evidence)?;
            if result.state != "completed"
                || result.claim_retained
                || result.model_usable
                || !result.processes.is_empty()
                || !result.owned_handle.is_empty()
                || !result.binding_id.is_empty()
                || !result.incarnation.is_empty()
                || result.checkpoint.is_some()
                || result.residency.is_some()
            {
                return Err(GroupIdentityError);
            }
        }
        (MemberAction::MaterializeSource(_), None) => return Err(GroupIdentityError),
        (_, Some(_)) => return Err(GroupIdentityError),
        (_, None) => {}
    }
    validate_prepare(command, result)?;
    validate_refusal(command, result)?;
    validate_launch_failure(command, result)?;
    let named_binding = !result.binding_id.is_empty()
        && result.binding_id.len() <= 128
        && !result.incarnation.is_empty()
        && result.incarnation.len() <= 128;
    // SPEC §§9.1, 10: parked or restored is claimed only on completion, by the
    // still-owned launch it names, whose api and worker processes are alive.
    // Process identity unchanged against the retained launch is the caller's
    // comparison; this checks that the evidence names one coherent live group.
    if claim != ResidencyClaim::None
        && (result.state != "completed"
            || !result.claim_retained
            || !named_binding
            || !live_group(result, false))
    {
        return Err(GroupIdentityError);
    }
    if result.model_usable {
        // SPEC §6.1: only the native readiness evidence of the launch itself, or of
        // a fresh probe of the retained launch (including the probe that closes
        // a restore), can claim a usable model. A park never can.
        let bound = match &command.action {
            MemberAction::LaunchSingle(plan) => {
                result.state == "launched"
                    && result.binding_id == plan.binding_id
                    && result.incarnation == plan.incarnation
                    && result.owned_handle == command.identity.command_id
            }
            // ADR 0028 §9: only the head serves; its readiness is the group's.
            // A worker (no service port) never claims a usable model.
            MemberAction::Launch { member, .. } => {
                member.service_port != 0
                    && result.state == "launched"
                    && result.binding_id == member.binding_id
                    && result.incarnation == member.incarnation
                    && result.owned_handle == command.identity.command_id
            }
            MemberAction::Probe { .. } => result.state == "completed" && named_binding,
            MemberAction::Restore { .. } => {
                result.state == "completed" && named_binding && claim == ResidencyClaim::Restored
            }
            _ => false,
        };
        if !bound || !result.claim_retained || !live_group(result, true) {
            return Err(GroupIdentityError);
        }
    }
    Ok(())
}

/// The alive processes of a result form one local group with an api process and
/// at least one worker, all on one boot with distinct roles and PIDs. ADR 0023 §6:
/// for a readiness claim (`single`), a group of one process is its api process
/// alone, as TensorFold serves from one; a residency claim keeps its worker, as
/// only the engines with workers park.
fn live_group(result: &pb::MemberExecutionResult, single: bool) -> bool {
    let current: Vec<_> = result
        .processes
        .iter()
        .filter(|p| p.presence == "alive")
        .map(|p| capyctl_domain::completion::ProcessIdentity {
            role: p.role.clone(),
            pid: p.pid,
            boot_id: p.boot_id.clone(),
            start_ticks: p.start_ticks,
        })
        .collect();
    let single = single && result.processes.len() == 1;
    current.iter().any(|p| p.role == "api")
        && (single || current.iter().any(|p| p.role.starts_with("worker-")))
        && capyctl_domain::group::validate_local_processes(&current).is_ok()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResidencyClaim {
    None,
    Parked,
    Restored,
}

/// The closed refusal categories a host may report for a checkpoint.
pub const CHECKPOINT_REFUSALS: &[&str] = &[
    "invalid_root",
    "unsafe_file",
    "too_large",
    "changed",
    "io_error",
    "unauthorized",
    "not_materializable",
];

/// SPEC §13: the closed categories a host reports when its own policy refused
/// a launch, park or restore before any effect. `checkpoint_mismatch`: the
/// checkpoint no longer measures to the recorded digest (ADR 0014 §7);
/// `checkpoint_unverified`: it could not be measured; `insufficient_memory`:
/// the host's pool cannot hold the allocation now; `residency_tier`: the
/// launch's declared tier does not park (ADR 0012); `runtime_integrity`: a
/// module the engine imports from capyctl's runtime directory is missing, not a
/// regular file, not the agent user's, writable by other, or writable by a
/// group other than the agent user's private group (SPEC §9.1, §13.3);
/// `device_conflict`: the launch's device claim conflicts with a launch the
/// host still claims (an exclusive claim on a shared device, SPEC §7.3);
/// `port_conflict`: the leased port belongs to a launch the host still claims
/// (per-launch claims, SPEC §3.1), or another program listens on it; `unauthorized`: any other local policy
/// refusal. `insufficient_memory` also covers the host's managed or host-KV
/// budget with every claimed launch counted. ADR 0008 (owner decision
/// 2026-09-23): `installation_drift`: the installation no longer measures to
/// its registered fingerprint and its host policy says `refuse`;
/// `capability_missing:deep_park`: a launch declared `deep`, or a Park, needs
/// internals the installation's launch-time probe found missing (declare
/// `restart_only` instead); `capability_missing:core`: the installation lacks
/// an interface every launch needs. Discrete GPU design §4:
/// `insufficient_device_memory`: a GPU's own memory domain cannot hold the
/// launch's allocation there with its free reserve kept, now or beside every
/// claimed launch.
pub const POLICY_REFUSALS: &[&str] = &[
    "checkpoint_mismatch",
    "checkpoint_unverified",
    "insufficient_memory",
    "insufficient_device_memory",
    "residency_tier",
    "runtime_integrity",
    "unauthorized",
    "device_conflict",
    "port_conflict",
    "installation_drift",
    "capability_missing:deep_park",
    "capability_missing:core",
];

/// Whether `reason` is one of the closed [`POLICY_REFUSALS`].
pub fn is_policy_refusal(reason: &str) -> bool {
    POLICY_REFUSALS.contains(&reason)
}

/// ADR 0028 §7, §16: the closed codes a host refuses a group Prepare (and a
/// group Launch's re-run of its checks) with. The port codes name one nonzero
/// port. `group_topology_invalid`: the plan gives a member more than one rank
/// (ADR 0028 §2: one rank per member in this version).
pub const PREPARE_REFUSALS: &[&str] = &[
    "group_topology_invalid",
    "group_profile_mismatch",
    "group_checkpoint_mismatch",
    "peer_address_not_local",
    "host_tuning_missing:memlock",
    "host_tuning_missing:infiniband",
];

/// Whether `reason` is a closed Prepare refusal: one of [`PREPARE_REFUSALS`],
/// or `rendezvous_port_in_use:<port>` / `service_port_in_use:<port>`.
pub fn is_prepare_refusal(reason: &str) -> bool {
    PREPARE_REFUSALS.contains(&reason)
        || ["rendezvous_port_in_use:", "service_port_in_use:"]
            .iter()
            .filter_map(|prefix| reason.strip_prefix(prefix))
            .any(|port| {
                port.bytes().all(|b| b.is_ascii_digit())
                    && port.parse::<u16>().is_ok_and(|port| port != 0)
            })
}

/// SPEC §13: a policy refusal is terminal evidence that nothing happened. A
/// refused launch completed with no claim, no process and no usable model; a
/// refused Park or Restore left the launch `unchanged`. ADR 0028 §7: a refused
/// Prepare carries one closed group code (its effect-free shape is checked for
/// every Prepare result by [`validate_prepare`]). ADR 0028 §8 (R7): a group
/// Launch whose host checks fail again at launch is refused like a single
/// launch, with either a closed group code or a closed policy category, and
/// names no binding. No other action carries a refusal.
fn validate_refusal(
    command: &MemberCommand,
    result: &pb::MemberExecutionResult,
) -> Result<(), GroupIdentityError> {
    if result.refused.is_empty() {
        return Ok(());
    }
    let unchanged = result
        .residency
        .as_ref()
        .is_some_and(|r| r.state == "unchanged");
    let (shape, closed) = match &command.action {
        MemberAction::LaunchSingle(_) => (
            !result.claim_retained
                && result.processes.is_empty()
                && result.owned_handle == command.identity.command_id,
            is_policy_refusal(&result.refused),
        ),
        MemberAction::Park { .. } | MemberAction::Restore { .. } => {
            (unchanged, is_policy_refusal(&result.refused))
        }
        MemberAction::Prepare(_) => (true, is_prepare_refusal(&result.refused)),
        MemberAction::Launch { .. } => (
            !result.claim_retained
                && result.processes.is_empty()
                && result.owned_handle == command.identity.command_id
                && result.binding_id.is_empty()
                && result.incarnation.is_empty(),
            is_prepare_refusal(&result.refused) || is_policy_refusal(&result.refused),
        ),
        _ => (false, false),
    };
    if !shape || !closed || result.state != "completed" || result.model_usable {
        return Err(GroupIdentityError);
    }
    Ok(())
}

/// ADR 0028 §7: a Prepare has no process effect. Every Prepare result, passed
/// or refused, is completed and claims nothing: no claim, no process, no owned
/// handle, binding or incarnation.
fn validate_prepare(
    command: &MemberCommand,
    result: &pb::MemberExecutionResult,
) -> Result<(), GroupIdentityError> {
    if matches!(command.action, MemberAction::Prepare(_))
        && (result.state != "completed"
            || result.claim_retained
            || !result.processes.is_empty()
            || !result.owned_handle.is_empty()
            || !result.binding_id.is_empty()
            || !result.incarnation.is_empty())
    {
        return Err(GroupIdentityError);
    }
    Ok(())
}

/// The longest launch failure a host may report.
pub const MAX_LAUNCH_FAILURE_BYTES: usize = 256;

/// SPEC §§6.4, 13.2: a launch failure is one printable line of at most
/// [`MAX_LAUNCH_FAILURE_BYTES`], carried only by a LaunchSingle that launched,
/// is not usable, and whose recorded processes are all gone.
pub fn is_launch_failure_text(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_LAUNCH_FAILURE_BYTES
        && text.bytes().all(|b| (0x20..0x7f).contains(&b))
}

fn validate_launch_failure(
    command: &MemberCommand,
    result: &pb::MemberExecutionResult,
) -> Result<(), GroupIdentityError> {
    if result.launch_failure.is_empty() {
        return Ok(());
    }
    let exited = command.action.launch_plan().is_some()
        && result.state == "launched"
        && !result.model_usable
        && !result.processes.is_empty()
        && result.processes.iter().all(|p| p.presence == "gone");
    if !exited || !is_launch_failure_text(&result.launch_failure) {
        return Err(GroupIdentityError);
    }
    Ok(())
}

/// ADR 0014 §7: `computed` carries a canonical digest and its weights;
/// `mismatch` additionally differs from a stated expectation; `refused` carries
/// only a closed reason; `sized` (a size-only request) carries the weights and
/// no digest. File counts and bytes are bounded by the host's walk.
fn validate_checkpoint(
    plan: &DigestCheckpointPlan,
    evidence: &pb::CheckpointDigestEvidence,
) -> Result<(), GroupIdentityError> {
    let measured = capyctl_config::effective::is_checkpoint_digest(&evidence.digest)
        && evidence.weights_bytes >= 0
        && u64::try_from(evidence.weights_bytes).is_ok_and(|w| w <= evidence.total_bytes)
        && evidence.file_count <= 65_536
        && evidence.reason.is_empty()
        && evidence.state_slot_bytes.is_none_or(|bytes| bytes > 0);
    let ok = match evidence.state.as_str() {
        // A size-only request is answered `sized` (or refused), never hashed.
        "computed" | "mismatch" if plan.size_only => false,
        "sized" => {
            plan.size_only
                && evidence.digest.is_empty()
                && evidence.weights_bytes >= 0
                && u64::try_from(evidence.weights_bytes).is_ok_and(|w| w <= evidence.total_bytes)
                && evidence.file_count <= 65_536
                && evidence.reason.is_empty()
                && !evidence.full_rehash
                && evidence.state_slot_bytes.is_none()
        }
        "computed" => {
            measured
                && plan
                    .expected_digest
                    .as_ref()
                    .is_none_or(|expected| *expected == evidence.digest)
        }
        "mismatch" => {
            measured
                && plan
                    .expected_digest
                    .as_ref()
                    .is_some_and(|expected| *expected != evidence.digest)
        }
        "refused" => {
            evidence.digest.is_empty()
                && evidence.weights_bytes == 0
                && evidence.file_count == 0
                && evidence.total_bytes == 0
                && !evidence.full_rehash
                && evidence.state_slot_bytes.is_none()
                && CHECKPOINT_REFUSALS.contains(&evidence.reason.as_str())
        }
        _ => false,
    };
    if !ok {
        return Err(GroupIdentityError);
    }
    Ok(())
}

/// ADR 0008: `verified` carries its bytes; `downloading` its progress (a
/// total of zero until sized); `failed` only a closed reason; `pending`
/// nothing. The key must be the one the plan's source names.
fn validate_source(
    plan: &MaterializeSourcePlan,
    evidence: &pb::ModelSourceEvidence,
) -> Result<(), GroupIdentityError> {
    let quiet = evidence.reason.is_empty() && !evidence.reservation_retained;
    let ok = evidence.source_key == plan.source_key
        && match evidence.state.as_str() {
            "pending" => quiet && evidence.bytes_done == 0 && evidence.bytes_total == 0,
            "downloading" => {
                quiet
                    && (evidence.bytes_total == 0 && evidence.bytes_done == 0
                        || evidence.bytes_done <= evidence.bytes_total)
            }
            "verified" => quiet && evidence.bytes_done == evidence.bytes_total,
            "failed" => {
                evidence.bytes_done == 0
                    && evidence.bytes_total == 0
                    && capyctl_config::model_source::reason::ALL.contains(&evidence.reason.as_str())
            }
            _ => false,
        };
    if !ok {
        return Err(GroupIdentityError);
    }
    Ok(())
}

/// Bounds on residency evidence. At most this many milestones, each a short
/// engine-generic step name (SPEC §17: never unbounded or user-derived labels).
pub const MAX_RESIDENCY_MILESTONES: usize = 32;
pub const MAX_MILESTONE_LEN: usize = 64;

fn validate_residency(
    action: &MemberAction,
    evidence: &pb::ResidencyEvidence,
) -> Result<ResidencyClaim, GroupIdentityError> {
    let claim = match (action, evidence.state.as_str()) {
        (MemberAction::Park { .. }, "parked") => ResidencyClaim::Parked,
        (MemberAction::Restore { .. }, "restored") => ResidencyClaim::Restored,
        (_, "unchanged" | "unknown") => ResidencyClaim::None,
        // A park can never claim restored, nor a restore parked.
        _ => return Err(GroupIdentityError),
    };
    if evidence.mem_available_before_bytes < -1
        || evidence.mem_available_after_bytes < -1
        || evidence.milestones.len() > MAX_RESIDENCY_MILESTONES
        || evidence.milestones.iter().any(|m| {
            m.is_empty()
                || m.len() > MAX_MILESTONE_LEN
                || !m
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b))
        })
    {
        return Err(GroupIdentityError);
    }
    Ok(claim)
}

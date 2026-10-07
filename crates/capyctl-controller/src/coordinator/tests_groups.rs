//! ADR 0028: the group world, the coordinator's harness for multi-node
//! engine groups.
//!
//! A [`GroupWorld`] is the coordinator and its store with one scripted agent
//! ([`GroupHost`]) per member host, every host running its rank of one shared
//! [`FakeGroup`]. Each scripted host answers the commands the real agent
//! answers for a group member, under the same contracts (Tasks 9 and 13):
//!
//! - `Prepare` runs the agent's own member checks
//!   (`capyctl_agent::host_checks::prepare_member`) against this host's
//!   scripted facts and replies `completed`, with a closed code in `refused`
//!   or nothing;
//! - `Launch` re-runs those checks, journals the member before it starts the
//!   rank, and replies with the member's identities (`api`/`worker-0` on the
//!   head, `worker-<r>` and `worker-<r>/<role>` on a worker). The head replies
//!   with a usable model only once every rank has launched and the engine
//!   finished initializing; a worker replies at once. A re-sent Launch (the
//!   same command id) replays the recorded reply and starts nothing (R23);
//! - `Terminate` ends the rank of a launch it recorded and reports it gone,
//!   with `escalated` when the rank sat in a hung collective; a host that lost
//!   its journal only observes the identities the server recorded and signals
//!   nothing (ADR 0016), so a live member is never reported gone.
//!
//! Every reply is checked with `validate_result` before it is returned, so a
//! scripted answer is one a real agent could have sent. Nothing here
//! qualifies an engine, a host or a group: CPU and Fake-engine tests are not
//! qualification; the live rows MN1–MN9 are.
use super::*;
use crate::group_activation::{GroupActivation, GroupActivationError};
use capyctl_agent::host_checks::{prepare_member, HostFacts, InfinibandAccess};
use capyctl_config::groups_policy::GroupsPolicy;
use capyctl_domain::completion::ProcessIdentity;
use capyctl_domain::group::{
    member_id, CommandIdentity, GroupEngine, GroupPlan, GroupTopology, MemberKey, MemberPlan,
    MemberRole,
};
use capyctl_protocol::execution::{MemberAction, MemberCommand, SingleLaunchPlan};
use capyctl_protocol::{capabilities, pb};
use capyctl_testkit::{FakeGroup, JournalState};
use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr};

/// The checkpoint digest every scripted host measures for the group's model.
const GROUP_CHECKPOINT_DIGEST: &str =
    "sha256:5e1f2f0c6a0d4c7b8e9a1b2c3d4e5f60718293a4b5c6d7e8f9a0b1c2d3e4f5a6";
/// The model path the golden deployment names, the same on every host.
const GROUP_MODEL_PATH: &str = "/srv/models/toy";
/// The golden host's runtime profile and its recorded build.
const GROUP_PROFILE: &str = "local";
const GROUP_PROFILE_BUILD: &str = "vllm-build-1";
/// The controller every scripted agent is enrolled with.
const GROUP_CONTROLLER: &str = "controller";
/// The private ingress every world host publishes (SPEC §15: loopback is a
/// protected link).
const WORLD_INGRESS: &str = "http://127.0.0.1:9443";

/// Host `index`'s peer address: documentation addresses only (RFC 5737).
fn peer_address(index: usize) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10 + index as u8))
}

/// Why a scripted host answered no result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum HostError {
    /// The host is disconnected: nothing was received.
    Unreachable,
    /// The command needs a capability the host did not declare.
    CapabilityMissing(String),
    /// The command does not decode, or names another host.
    Malformed,
    /// The agent's authorization refused it (another controller, a wrong
    /// expected state, another owner, or a group Park, Restore or Probe,
    /// which the agent admits for single-rank launches only).
    Unauthorized,
    /// ADR 0028 §8 (R23): another command already claims this member's launch.
    Uncertain,
    /// A command id the journal holds for a different command (another
    /// digest).
    Conflict,
    /// An action the scripted host does not model.
    NotScripted(&'static str),
}

/// One member launch as the scripted agent journaled it.
struct Recorded {
    command: MemberCommand,
    rank: u32,
    identities: Vec<ProcessIdentity>,
    claim_retained: bool,
    /// The reply the launch produced, replayed for a re-sent Launch.
    reply: Option<pb::MemberExecutionResult>,
}

/// One rank process tree this host started: the host's process table, which
/// outlives a lost journal.
struct Spawned {
    rank: u32,
    identities: Vec<ProcessIdentity>,
}

/// Called with every Launch a host receives, before it does anything.
type LaunchObserver = Arc<dyn Fn(&MemberCommand) + Send + Sync>;

struct HostState {
    capabilities: BTreeSet<String>,
    /// Profile name to its recorded build.
    profiles: BTreeMap<String, String>,
    /// Model path to the digest this host measured.
    digests: BTreeMap<String, String>,
    /// The path this host's copy of the group's weights resolves to.
    model_path: String,
    /// Ports something outside CapyCTL holds on this host.
    held_ports: BTreeSet<u16>,
    journal: BTreeMap<String, Recorded>,
    spawned: Vec<Spawned>,
    received: Vec<MemberCommand>,
    next_pid: u32,
    /// A closed code every Prepare is refused with, whatever the checks say.
    prepare_refusal: Option<String>,
    /// The head's completion probes generate no token.
    probe_fails: bool,
    /// Every Launch is carried out but its reply never reaches the server.
    lose_launch_replies: bool,
    /// Every Launch starts its rank, which exits at once (a launch failure).
    launch_fails: bool,
    launch_observer: Option<LaunchObserver>,
    /// The world's own observer of every Launch, besides a test's.
    witness: Option<LaunchObserver>,
    /// The engine environment the last Launch rendered here.
    launch_env: Option<BTreeMap<String, String>>,
}

/// The scripted agent of one member host.
pub(super) struct GroupHost {
    name: String,
    peer: IpAddr,
    group: FakeGroup,
    state: Mutex<HostState>,
}

impl GroupHost {
    fn new(name: &str, peer: IpAddr, group: FakeGroup) -> Self {
        Self {
            name: name.into(),
            peer,
            group,
            state: Mutex::new(HostState {
                // ADR 0028 §14: Prepare and Launch with a group plan need
                // `engine_groups`, which a current agent declares.
                capabilities: capabilities::agent_capabilities().into_iter().collect(),
                profiles: BTreeMap::from([(GROUP_PROFILE.into(), GROUP_PROFILE_BUILD.into())]),
                digests: BTreeMap::from([(
                    GROUP_MODEL_PATH.into(),
                    GROUP_CHECKPOINT_DIGEST.into(),
                )]),
                model_path: GROUP_MODEL_PATH.into(),
                held_ports: BTreeSet::new(),
                journal: BTreeMap::new(),
                spawned: Vec::new(),
                received: Vec::new(),
                next_pid: 4000,
                prepare_refusal: None,
                probe_fails: false,
                lose_launch_replies: false,
                launch_fails: false,
                launch_observer: None,
                witness: None,
                launch_env: None,
            }),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, HostState> {
        self.state.lock().unwrap()
    }

    /// Something outside CapyCTL listens on `port` on this host.
    pub(super) fn hold_port(&self, port: u16) {
        self.state().held_ports.insert(port);
    }

    /// Every command this host received, in order.
    pub(super) fn received(&self) -> Vec<MemberCommand> {
        self.state().received.clone()
    }

    /// The identities this host's journal recorded for `command_id`.
    pub(super) fn recorded(&self, command_id: &str) -> Option<Vec<ProcessIdentity>> {
        self.state()
            .journal
            .get(command_id)
            .map(|r| r.identities.clone())
    }

    /// ADR 0028 §7: the host facts this agent reads: its peer address on an
    /// interface, unlimited memlock, read-write infiniband, no compaction.
    fn facts(&self) -> (HostFacts, GroupsPolicy) {
        (
            HostFacts {
                compaction_proactiveness: Some(0),
                memlock_soft: None,
                infiniband: InfinibandAccess::ReadWrite,
                local_addresses: vec![self.peer],
            },
            GroupsPolicy {
                peer_address: Some(self.peer),
                ..GroupsPolicy::default()
            },
        )
    }

    /// ADR 0028 §7: the agent's member checks, with this host's facts.
    fn check(&self, plan: &GroupPlan) -> Result<(), String> {
        let (facts, policy) = self.facts();
        let state = self.state();
        prepare_member(
            plan,
            &self.name,
            &facts,
            &policy,
            |_, port| !state.held_ports.contains(&port),
            |path| state.digests.get(path).cloned(),
            |profile| state.profiles.get(profile).cloned(),
        )
        .map(drop)
    }

    /// Whether `identity` is a process this host started that still runs:
    /// one of its rank's newest tree, while that rank lives.
    fn running(&self, state: &HostState, identity: &ProcessIdentity) -> bool {
        state.spawned.last().is_some_and(|spawned| {
            spawned.identities.contains(identity) && self.group.alive(spawned.rank)
        })
    }

    fn observed(
        &self,
        state: &HostState,
        identities: &[ProcessIdentity],
    ) -> Vec<pb::OwnedProcessObservation> {
        identities
            .iter()
            .map(|p| pb::OwnedProcessObservation {
                role: p.role.clone(),
                pid: p.pid,
                boot_id: p.boot_id.clone(),
                start_ticks: p.start_ticks,
                presence: if self.running(state, p) {
                    "alive"
                } else {
                    "gone"
                }
                .into(),
            })
            .collect()
    }

    /// Review Focus 3: a restarted agent with an empty journal knows no
    /// launch; the processes it started run on.
    fn reconcile_journal(&self) {
        if self.group.take_journal_loss(&self.name) {
            self.state().journal.clear();
        }
    }

    /// SPEC §13.2 (W13), ADR 0028 §11: the exits this host's agent reports,
    /// as its exit watcher does: every launch it still claims whose rank's
    /// process (of the newest tree) is gone, a head once it proved readiness,
    /// a worker from its Launch on, named by the launch's own handle and its
    /// leader's reported identity. A disconnected host reports nothing.
    fn exits(&self) -> Vec<capyctl_protocol::reports::MemberExit> {
        if !self.group.connected(&self.name) {
            return Vec::new();
        }
        self.reconcile_journal();
        let state = self.state();
        let Some(newest) = state.spawned.last() else {
            return Vec::new();
        };
        state
            .journal
            .values()
            .filter(|r| {
                r.claim_retained
                    && !r.identities.is_empty()
                    && r.identities == newest.identities
                    && !self.group.alive(r.rank)
                    && (r.rank != 0 || r.reply.as_ref().is_some_and(|reply| reply.model_usable))
            })
            .map(|r| capyctl_protocol::reports::MemberExit {
                host_id: self.name.clone(),
                deployment_id: r.command.identity.deployment_id.clone(),
                generation: r.command.identity.generation,
                owned_handle: r.command.identity.command_id.clone(),
                process: r.identities[0].clone(),
                status: match self.group.ended(r.rank) {
                    Some(capyctl_testkit::RankEnd::Killed) => {
                        capyctl_protocol::reports::ExitStatus::Signal(9)
                    }
                    _ => capyctl_protocol::reports::ExitStatus::Code(1),
                },
                observed_at_ms: capyctl_protocol::now_unix_ms(),
            })
            .collect()
    }

    /// Answer one command as the member's agent does.
    pub(super) async fn execute(
        &self,
        command: MemberCommand,
    ) -> Result<pb::MemberExecutionResult, HostError> {
        if !self.group.connected(&self.name) {
            return Err(HostError::Unreachable);
        }
        self.reconcile_journal();
        self.state().received.push(command.clone());
        // The agent receives the wire form: decode it, check its digest and
        // the capabilities it needs.
        let wire = command.to_wire();
        let declared = self.state().capabilities.clone();
        if let Some(missing) = capabilities::required(&wire)
            .into_iter()
            .find(|need| !declared.contains(*need))
        {
            return Err(HostError::CapabilityMissing(missing.into()));
        }
        let decoded = MemberCommand::try_from(pb::ServerToAgent {
            msg: Some(pb::server_to_agent::Msg::ExecuteMember(wire)),
        })
        .map_err(|_| HostError::Malformed)?;
        if decoded.verify_digest().is_err() || decoded.identity.member.host_id != self.name {
            return Err(HostError::Malformed);
        }
        // SPEC §13.1: the agent takes commands from its own controller only.
        if decoded.identity.controller_id != GROUP_CONTROLLER {
            return Err(HostError::Unauthorized);
        }
        let result = match &decoded.action {
            MemberAction::Prepare(plan) => self.prepare(&decoded, plan),
            MemberAction::Launch { plan, member } => {
                let (observer, witness) = {
                    let state = self.state();
                    (state.launch_observer.clone(), state.witness.clone())
                };
                for observe in [observer, witness].into_iter().flatten() {
                    observe(&decoded);
                }
                let result = self.launch(&decoded, plan, member).await?;
                // The host carried the Launch out; its reply is lost.
                if self.state().lose_launch_replies {
                    return Err(HostError::Unreachable);
                }
                result
            }
            MemberAction::Terminate {
                owned_handle,
                recorded,
            } => self.terminate(&decoded, owned_handle, recorded)?,
            // ADR 0028 §9 (R30): the head answers a completion probe on its
            // retained launch; a readiness probe of a group launch, and any
            // probe of a worker, is refused as the agent refuses it.
            MemberAction::Probe {
                owned_handle,
                max_tokens: Some(max_tokens),
            } => self.probe(&decoded, owned_handle, *max_tokens)?,
            // ADR 0028 §9, §12: the agent admits Park, Restore and a readiness
            // Probe for single-rank launches only, so far.
            MemberAction::Park { .. }
            | MemberAction::Restore { .. }
            | MemberAction::Probe { .. } => return Err(HostError::Unauthorized),
            MemberAction::DigestCheckpoint(_) => self.digest(&decoded),
            MemberAction::LaunchSingle(_) => return Err(HostError::NotScripted("LaunchSingle")),
            MemberAction::Inspect => return Err(HostError::NotScripted("Inspect")),
            MemberAction::CloseIngress => return Err(HostError::NotScripted("CloseIngress")),
            MemberAction::MaterializeSource(_) => {
                return Err(HostError::NotScripted("MaterializeSource"))
            }
        };
        capyctl_protocol::execution::validate_result(&decoded, &result)
            .expect("the scripted host answers as an agent does");
        Ok(result)
    }

    fn reply(command: &MemberCommand) -> pb::MemberExecutionResult {
        pb::MemberExecutionResult {
            identity: command.to_wire().identity,
            state: "completed".into(),
            observed_at_unix_ms: capyctl_protocol::now_unix_ms(),
            ..Default::default()
        }
    }

    /// ADR 0028 §7: a Prepare is effect-free; `refused` carries the closed code.
    fn prepare(&self, command: &MemberCommand, plan: &GroupPlan) -> pb::MemberExecutionResult {
        let scripted = self.state().prepare_refusal.clone();
        pb::MemberExecutionResult {
            refused: scripted.unwrap_or_else(|| self.check(plan).err().unwrap_or_default()),
            ..Self::reply(command)
        }
    }

    /// ADR 0014 §7, ADR 0028 §6: this host measures its own copy.
    fn digest(&self, command: &MemberCommand) -> pb::MemberExecutionResult {
        let state = self.state();
        let digest = state.digests.get(&state.model_path).cloned();
        pb::MemberExecutionResult {
            checkpoint: Some(match digest {
                Some(digest) => pb::CheckpointDigestEvidence {
                    state: "computed".into(),
                    digest,
                    weights_bytes: 1 << 30,
                    total_bytes: 1 << 30,
                    file_count: 1,
                    ..Default::default()
                },
                None => pb::CheckpointDigestEvidence {
                    state: "refused".into(),
                    reason: "checkpoint_unreadable".into(),
                    ..Default::default()
                },
            }),
            ..Self::reply(command)
        }
    }

    /// ADR 0028 §9 (R30): one completion through the head's retained launch,
    /// at most `max_tokens` token ids, on loopback with the launch's key.
    fn probe(
        &self,
        command: &MemberCommand,
        owned_handle: &str,
        max_tokens: u32,
    ) -> Result<pb::MemberExecutionResult, HostError> {
        let state = self.state();
        let Some(launch) = state.journal.get(owned_handle) else {
            return Err(HostError::Unauthorized);
        };
        if launch.rank != 0
            || command.identity.expected_state != "ready"
            || launch.command.identity.deployment_id != command.identity.deployment_id
        {
            return Err(HostError::Unauthorized);
        }
        let tokens: Vec<u32> = if state.probe_fails {
            Vec::new()
        } else {
            self.group
                .complete("probe")
                .into_iter()
                .take(max_tokens as usize)
                .collect()
        };
        // As the agent, the probe reports on the binding of the launch it
        // names.
        let (binding_id, incarnation) = match &launch.command.action {
            MemberAction::Launch { member, .. } => {
                (member.binding_id.clone(), member.incarnation.clone())
            }
            _ => Default::default(),
        };
        Ok(pb::MemberExecutionResult {
            owned_handle: owned_handle.into(),
            processes: self.observed(&state, &launch.identities),
            claim_retained: true,
            probe_tokens: tokens,
            binding_id,
            incarnation,
            ..Self::reply(command)
        })
    }

    /// ADR 0028 §8 (R7, R23, R29): one member launch.
    async fn launch(
        &self,
        command: &MemberCommand,
        plan: &GroupPlan,
        member: &SingleLaunchPlan,
    ) -> Result<pb::MemberExecutionResult, HostError> {
        let id = &command.identity;
        // ADR 0028 §8 (R23): the identical command again replays the recorded
        // launch; the same id for another command is a journal conflict.
        let replay = {
            let state = self.state();
            match state.journal.get(&id.command_id) {
                Some(recorded) if recorded.command.identity.payload_digest != id.payload_digest => {
                    return Err(HostError::Conflict)
                }
                Some(recorded) => Some(recorded.reply.clone()),
                None => {
                    // Another command for a member this host still claims
                    // is refused uncertain and starts nothing.
                    if state.journal.values().any(|r| {
                        r.claim_retained
                            && r.command.identity.deployment_id == id.deployment_id
                            && r.command.identity.instance_index == id.instance_index
                            && r.command.identity.generation == id.generation
                            && r.command.identity.member == id.member
                    }) {
                        return Err(HostError::Uncertain);
                    }
                    None
                }
            }
        };
        if let Some(Some(reply)) = replay {
            return Ok(reply);
        }
        if replay.is_none() {
            // ADR 0028 §8: a Launch is authorized in the reserved state only.
            if id.expected_state != "reserved" {
                return Err(HostError::Unauthorized);
            }
            // R7: the member checks run again; a failure is a refusal that
            // journals and starts nothing.
            if let Err(code) = self.check(plan) {
                return Ok(pb::MemberExecutionResult {
                    owned_handle: id.command_id.clone(),
                    refused: code,
                    ..Self::reply(command)
                });
            }
            self.spawn(command, plan);
            // The engine dies during its initialization (a bad flag, an OOM).
            if self.state().launch_fails {
                let rank = self.group.rank_of(&self.name).unwrap();
                self.group.exit_rank(rank);
            }
            // ADR 0028 §2.1, §10: the engine environment this member
            // renders: the deployment's approved variables, the same on every
            // host, and the member's own address under the engine's name.
            let document: serde_json::Value = serde_json::from_str(&member.deployment_config)
                .map_err(|_| HostError::Malformed)?;
            let mut env: BTreeMap<String, String> = document["engine_config"]["env"]
                .as_object()
                .map(|env| {
                    env.iter()
                        .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
                        .collect()
                })
                .unwrap_or_default();
            let address = match plan.engine() {
                GroupEngine::Vllm => Some("VLLM_HOST_IP"),
                GroupEngine::Sglang => Some("SGLANG_HOST_IP"),
                GroupEngine::Tensorfold => None,
            };
            if let Some(name) = address {
                env.insert(name.into(), self.peer.to_string());
            }
            self.state().launch_env = Some(env);
        }
        let rank = plan
            .members()
            .iter()
            .find(|m| m.member.host_id == self.name)
            .map(|m| m.rank)
            .ok_or(HostError::Malformed)?;
        if rank == 0 {
            self.await_head_ready(id.deadline_ms).await;
        }
        let mut state = self.state();
        let recorded = &state.journal[&id.command_id];
        let alive = self.observed(&state, &recorded.identities);
        // ADR 0028 §9: only the head serves, and only once the whole group
        // formed and initialized.
        let usable = rank == 0
            && self.group.head_ready()
            && self.group.launch_completed()
            && alive.iter().all(|p| p.presence == "alive");
        let reply = pb::MemberExecutionResult {
            state: "launched".into(),
            owned_handle: id.command_id.clone(),
            processes: alive,
            claim_retained: true,
            model_usable: usable,
            binding_id: member.binding_id.clone(),
            incarnation: member.incarnation.clone(),
            ..Self::reply(command)
        };
        // A worker's reply, and the head's once it proved readiness, is
        // final and replayed; a head that timed out is asked again.
        if rank != 0 || usable {
            state.journal.get_mut(&id.command_id).unwrap().reply = Some(reply.clone());
        }
        Ok(reply)
    }

    /// Journal the launch first, then start the rank and record its tree.
    fn spawn(&self, command: &MemberCommand, plan: &GroupPlan) {
        let rank = self
            .group
            .rank_of(&self.name)
            .expect("the scripted host runs a rank of its group");
        assert_eq!(
            plan.members()[rank as usize].member.host_id,
            self.name,
            "the plan ranks the hosts as the fake group does"
        );
        let mut state = self.state();
        state.journal.insert(
            command.identity.command_id.clone(),
            Recorded {
                command: command.clone(),
                rank,
                identities: Vec::new(),
                claim_retained: true,
                reply: None,
            },
        );
        self.group.launch(rank);
        // ADR 0028 §4, §8: a worker's leader is reported as `worker-<r>` and
        // its children under it; the head keeps the engine's own roles.
        let roles = if rank == 0 {
            vec!["api".to_owned(), "worker-0".to_owned()]
        } else {
            vec![format!("worker-{rank}"), format!("worker-{rank}/worker-0")]
        };
        let pid = state.next_pid;
        state.next_pid += roles.len() as u32;
        let identities: Vec<ProcessIdentity> = roles
            .into_iter()
            .enumerate()
            .map(|(offset, role)| ProcessIdentity {
                role,
                pid: pid + offset as u32,
                boot_id: format!("boot-{}", self.name),
                start_ticks: u64::from(pid) * 10 + offset as u64 + 1,
            })
            .collect();
        state.spawned.push(Spawned {
            rank,
            identities: identities.clone(),
        });
        state
            .journal
            .get_mut(&command.identity.command_id)
            .unwrap()
            .identities = identities;
    }

    /// ADR 0028 §9: the head waits for the whole group to form and the engine
    /// to initialize, bounded by the command's deadline. A rank that ends
    /// hangs it until then, as a real head hangs in its collective.
    async fn await_head_ready(&self, deadline_ms: i64) {
        let left = (deadline_ms - capyctl_protocol::now_unix_ms()).max(0) as u64;
        let mut changes = self.group.subscribe();
        let _ = tokio::time::timeout(Duration::from_millis(left), async {
            while !(self.group.head_ready() && self.group.launch_completed()) {
                if changes.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        })
        .await;
    }

    /// ADR 0028 §11, ADR 0016: terminate one recorded launch, or observe the
    /// server's recorded identities of one this host no longer knows.
    fn terminate(
        &self,
        command: &MemberCommand,
        owned_handle: &str,
        recorded: &[ProcessIdentity],
    ) -> Result<pb::MemberExecutionResult, HostError> {
        let id = &command.identity;
        // ADR 0028 §11: a Terminate is authorized against a retained launch only.
        if id.expected_state != "retained" {
            return Err(HostError::Unauthorized);
        }
        let mut state = self.state();
        let Some(launch) = state.journal.get(owned_handle) else {
            // A lost journal signals nothing: it reports what it observes of
            // the identities the server recorded, never claiming the launch.
            let processes = self.observed(&state, recorded);
            return Ok(pb::MemberExecutionResult {
                owned_handle: owned_handle.into(),
                processes,
                ..Self::reply(command)
            });
        };
        if launch.command.identity.deployment_id != id.deployment_id
            || launch.command.identity.member != id.member
        {
            return Err(HostError::Unauthorized);
        }
        let (rank, identities) = (launch.rank, launch.identities.clone());
        // Only the newest tree of the rank still runs; an older one is gone.
        let current = state
            .spawned
            .last()
            .is_some_and(|spawned| spawned.identities == identities);
        let escalated = current && self.group.terminate_rank(rank);
        state.journal.get_mut(owned_handle).unwrap().claim_retained = false;
        let processes = self.observed(&state, &identities);
        Ok(pb::MemberExecutionResult {
            owned_handle: owned_handle.into(),
            processes,
            escalated,
            ..Self::reply(command)
        })
    }
}

/// One fake group over `names`, in rank order, and a scripted agent per host.
fn scripted(names: &[&str]) -> (FakeGroup, Vec<Arc<GroupHost>>) {
    let group = FakeGroup::new(names);
    let hosts = names
        .iter()
        .enumerate()
        .map(|(index, name)| Arc::new(GroupHost::new(name, peer_address(index), group.clone())))
        .collect();
    (group, hosts)
}

/// ADR 0028 §8: the coordinator's transport to the world's scripted hosts:
/// every command goes to [`GroupHost::execute`], whose errors become the
/// transport's "no answer".
struct WorldHosts {
    hosts: Vec<Arc<GroupHost>>,
    owner: SharedCoordinatorState,
    /// What each host publishes: its peer address (`None` without one) and
    /// its policy fingerprint.
    published: Mutex<BTreeMap<String, (Option<IpAddr>, String)>>,
    concluded: Mutex<BTreeMap<String, Result<GroupActivation, GroupActivationError>>>,
    ready: Mutex<Vec<(String, String)>>,
}

impl WorldHosts {
    fn host(&self, name: &str) -> Option<&Arc<GroupHost>> {
        self.hosts.iter().find(|h| h.name == name)
    }
}

impl crate::group_activation::GroupHosts for WorldHosts {
    fn controller_id(&self) -> String {
        GROUP_CONTROLLER.into()
    }

    fn host(&self, host_id: &str) -> Result<crate::group_activation::MemberHost, String> {
        let published = self.published.lock().unwrap();
        let (peer, fingerprint) = published
            .get(host_id)
            .cloned()
            .ok_or_else(|| format!("{host_id} publishes nothing"))?;
        Ok(crate::group_activation::MemberHost {
            policy_fingerprint: fingerprint,
            groups: GroupsPolicy {
                peer_address: peer,
                ..GroupsPolicy::default()
            },
        })
    }

    fn preflight(&self, host_id: &str, needs: &[&str]) -> Result<(), String> {
        let host = WorldHosts::host(self, host_id).ok_or("unauthorized")?;
        let declared = host.state().capabilities.clone();
        match needs.iter().find(|need| !declared.contains(**need)) {
            Some(need) => Err(capabilities::missing(need)),
            None => Ok(()),
        }
    }

    fn supports(&self, host_id: &str, capability: &str) -> bool {
        WorldHosts::host(self, host_id).is_some_and(|h| h.state().capabilities.contains(capability))
    }

    fn execute(
        &self,
        command: MemberCommand,
    ) -> crate::group_activation::HostFuture<'_, Result<pb::MemberExecutionResult, String>> {
        Box::pin(async move {
            let host = WorldHosts::host(self, &command.identity.member.host_id)
                .ok_or_else(|| "no such host".to_owned())?;
            host.execute(command).await.map_err(|e| format!("{e:?}"))
        })
    }

    fn provision_head<'a>(
        &'a self,
        command: &'a MemberCommand,
    ) -> crate::group_activation::HostFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let MemberAction::Launch { member, .. } = &command.action else {
                return Err("not a group launch".into());
            };
            // SPEC §§6, 15: as production, the group's only replica is the
            // head's ingress, frozen with the head's binding.
            let owner = self.owner.lock().unwrap();
            owner
                .store()
                .bind_remote_ingress(
                    &member.binding_id,
                    &command.identity.member.host_id,
                    WORLD_INGRESS,
                )
                .map_err(|e| e.to_string())
        })
    }

    fn head_ready(&self, binding_id: &str, host_id: &str) {
        self.ready
            .lock()
            .unwrap()
            .push((binding_id.into(), host_id.into()));
    }

    fn concluded(
        &self,
        deployment_id: &str,
        outcome: Result<&GroupActivation, &GroupActivationError>,
    ) {
        self.concluded
            .lock()
            .unwrap()
            .insert(deployment_id.into(), outcome.cloned().map_err(Clone::clone));
    }
}

/// The world's bindings: the scripted group hosts, and a Fake engine for a
/// single-rank deployment beside the groups ([`GroupWorld::with_single_rank`]).
struct WorldBindings(Arc<WorldHosts>);

impl ExecutionBindings for WorldBindings {
    fn resolve(&self, work: &InitializeWork) -> Result<ExecutionBinding, CoordinatorError> {
        // ADR 0028 §5: a group never launches as a single-rank engine.
        if work.group().is_some() {
            return Err(CoordinatorError::Service(
                "a group never launches as a single-rank engine".into(),
            ));
        }
        let engine = Arc::new(FakeEngine::with_lifecycle_clock(Arc::new(|| {
            Ok(capyctl_protocol::now_unix_ms())
        })));
        let cleanup = engine.clone();
        Ok(ExecutionBinding::remote(
            engine,
            Arc::new(move |context: CleanupExecutionContext| {
                let engine = cleanup.clone();
                Box::pin(async move {
                    engine
                        .lifecycle_cleanup_observed(
                            &context.binding_id,
                            &context.incarnation,
                            &context.identities,
                        )
                        .map_err(|e| CoordinatorError::Service(e.to_string()))
                })
            }),
        ))
    }
    fn groups(&self) -> Option<Arc<dyn crate::group_activation::GroupHosts>> {
        Some(self.0.clone())
    }
}

/// Each host's own domains, observed fresh and empty; any other host (the
/// fixture's embedded one) as the fixture observed it.
struct WorldObservations {
    domains: Arc<Mutex<BTreeMap<String, Vec<String>>>>,
    fallback: Vec<MemoryObservation>,
    /// The hosts eligible for a start; `None` while the world names none.
    eligible: Arc<Mutex<Option<BTreeSet<String>>>>,
}

impl ServiceObservation for WorldObservations {
    fn eligible_hosts(&self) -> Option<BTreeSet<String>> {
        self.eligible.lock().unwrap().clone()
    }
    fn observe(&self, host: String) -> ObservationFuture {
        let now = capyctl_protocol::now_unix_ms();
        let observed = match self.domains.lock().unwrap().get(&host) {
            Some(domains) => domains
                .iter()
                .map(|domain| MemoryObservation {
                    domain: domain.clone(),
                    capacity_bytes: 1 << 50,
                    available_bytes: 1 << 50,
                    sampled_at_ms: now,
                })
                .collect(),
            None => self
                .fallback
                .iter()
                .cloned()
                .map(|o| MemoryObservation {
                    sampled_at_ms: now,
                    ..o
                })
                .collect(),
        };
        Box::pin(async move { Ok(observed) })
    }
}

/// Why a deploy (or its activation) was refused, led by its closed code.
#[derive(Debug)]
pub(super) struct DeployError(String);

impl DeployError {
    /// The closed code (spec §16) the refusal leads with.
    pub(super) fn code(&self) -> &str {
        let text = self.0.as_str();
        let end = text.find(": ").unwrap_or(text.len());
        &text[..end]
    }
}

impl std::fmt::Display for DeployError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// An instance's status as an operator reads it.
pub(super) struct GroupStatus {
    last_error: Option<String>,
    members: Vec<MemberStatus>,
}

/// One member of a group instance as status reads it (ADR 0028 §15).
pub(super) struct MemberStatus {
    pub(super) state: String,
}

impl GroupStatus {
    pub(super) fn last_error(&self) -> &str {
        self.last_error.as_deref().unwrap_or("")
    }

    /// The member at `rank` of the instance's newest plan.
    pub(super) fn member(&self, rank: u32) -> &MemberStatus {
        &self.members[rank as usize]
    }
}

/// The coordinator and its store, with a scripted agent per member host,
/// every host running its rank of one [`FakeGroup`].
pub(super) struct GroupWorld {
    pub(super) group: FakeGroup,
    hosts: Vec<Arc<GroupHost>>,
    transport: Arc<WorldHosts>,
    domains: Arc<Mutex<BTreeMap<String, Vec<String>>>>,
    owner: SharedCoordinatorState,
    worker: OwnedCoordinator,
    /// Deployment ids by name.
    deployed: Mutex<BTreeMap<String, DeploymentFence>>,
    enrolled: Mutex<bool>,
    engine: String,
    approved: BTreeMap<String, Vec<String>>,
    builds: BTreeMap<String, String>,
    without_peer: BTreeSet<String>,
    model_stores: BTreeMap<String, String>,
    /// `recovery` of every group this world deploys.
    recovery: String,
    /// Every group Launch the hosts received: its generation, and whether the
    /// instance's previous plan had fully settled when it arrived.
    witnessed: Arc<Mutex<Vec<(i64, bool)>>>,
    /// The hosts the coordinator's observations call eligible.
    eligible: Arc<Mutex<Option<BTreeSet<String>>>>,
    /// The coordinator's observations, kept across a restart.
    observations: Arc<WorldObservations>,
    /// The current worker's handler of the hosts' exit reports.
    exits: Arc<Mutex<crate::engine_exit::EngineExits>>,
    _dir: tempfile::TempDir,
}

impl GroupWorld {
    /// A world of `names`, which are the group's hosts in rank order: the
    /// first heads it. Each host is enrolled under its own name and has its
    /// own documentation peer address.
    pub(super) fn hosts(names: &[&str]) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let (group, hosts) = scripted(names);
        let fixture = capyctl_testkit::fixture::fixture();
        let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("srv.sqlite3");
        fixture
            .sql
            .execute("VACUUM INTO ?1", [path.to_str().unwrap()])
            .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        {
            let sql = rusqlite::Connection::open(&path).unwrap();
            for name in names {
                sql.execute(
                    "INSERT INTO enrolled_hosts(host_id,host_name,key_digest,revoked) VALUES(?1,?1,'key',0)",
                    [name],
                )
                .unwrap();
            }
        }
        let owner = Arc::new(Mutex::new(
            crate::ownership::OwnedCoordinatorState::open(dir.path()).unwrap(),
        ));
        let transport = Arc::new(WorldHosts {
            hosts: hosts.clone(),
            owner: owner.clone(),
            published: Mutex::new(BTreeMap::new()),
            concluded: Mutex::new(BTreeMap::new()),
            ready: Mutex::new(Vec::new()),
        });
        let domains = Arc::new(Mutex::new(BTreeMap::new()));
        let eligible = Arc::new(Mutex::new(None));
        let observations = Arc::new(WorldObservations {
            domains: domains.clone(),
            fallback: fixture.observations.clone(),
            eligible: eligible.clone(),
        });
        let worker = spawn_world_worker(&owner, &observations, &transport);
        // SPEC §13.2 (W13): each host's exit watcher reports through the
        // controller's own exit path, as an agent session's `MemberExit` does:
        // the wire report is decoded and validated, then handled by the
        // current worker's exit handler.
        let exits = Arc::new(Mutex::new(crate::engine_exit::EngineExits::new(
            worker.commands(),
        )));
        for host in &hosts {
            let (host, exits) = (host.clone(), exits.clone());
            tokio::spawn(async move {
                let mut changes = host.group.subscribe();
                loop {
                    for exit in host.exits() {
                        let report =
                            capyctl_protocol::reports::MemberExit::try_from(exit.to_wire())
                                .expect("a scripted exit report is valid on the wire");
                        let (exits, name) = (exits.lock().unwrap().clone(), host.name.clone());
                        let _ =
                            tokio::task::spawn_blocking(move || exits.remote(&name, &report)).await;
                    }
                    tokio::select! {
                        changed = changes.changed() => if changed.is_err() { return },
                        _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                    }
                }
            });
        }
        // ADR 0028 §11: what each Launch saw of the plan before it.
        let witnessed = Arc::new(Mutex::new(Vec::new()));
        for host in &hosts {
            let (owner, witnessed) = (owner.clone(), witnessed.clone());
            host.state().witness = Some(Arc::new(move |command: &MemberCommand| {
                let id = &command.identity;
                let o = owner.lock().unwrap();
                let settled = o
                    .store()
                    .group_plan_at(&id.deployment_id, 0, id.generation - 1)
                    .unwrap()
                    .is_some_and(|(_, rows)| rows.iter().all(|r| r.state == MemberState::Settled));
                witnessed.lock().unwrap().push((id.generation, settled));
            }));
        }
        Self {
            group,
            hosts,
            transport,
            domains,
            owner,
            worker,
            deployed: Mutex::new(BTreeMap::new()),
            enrolled: Mutex::new(false),
            engine: "vllm".into(),
            approved: BTreeMap::new(),
            builds: BTreeMap::new(),
            without_peer: BTreeSet::new(),
            model_stores: BTreeMap::new(),
            // Nothing relaunches unless a test asks for `reconcile`.
            recovery: "cold_restart".into(),
            witnessed,
            eligible,
            observations,
            exits,
            _dir: dir,
        }
    }

    /// ADR 0016, ADR 0028 §11 (R38): the controller restarts on the same
    /// store. Its session is retired (the old worker can no longer act, as
    /// after a crash), a new worker adopts what the store holds, and the
    /// hosts' exit reports go to the new worker.
    pub(super) async fn restart(mut self) -> Self {
        self.owner.lock().unwrap().restart_session().unwrap();
        let worker = spawn_world_worker(&self.owner, &self.observations, &self.transport);
        *self.exits.lock().unwrap() = crate::engine_exit::EngineExits::new(worker.commands());
        drop(std::mem::replace(&mut self.worker, worker));
        self
    }
}

/// The world's coordinator worker on `owner`'s current session.
fn spawn_world_worker(
    owner: &SharedCoordinatorState,
    observations: &Arc<WorldObservations>,
    transport: &Arc<WorldHosts>,
) -> OwnedCoordinator {
    OwnedCoordinator::spawn_with_execution_bindings(
        owner.clone(),
        observations.clone(),
        Arc::new(|| Ok(capyctl_protocol::now_unix_ms())),
        CoordinatorOptions {
            retry_cooldown: Duration::from_millis(50),
            ..Default::default()
        },
        Arc::new(WorldBindings(transport.clone())),
    )
    .unwrap()
}

impl GroupWorld {
    /// Every host runs `engine` (`vllm`, `sglang` or `tensorfold`).
    pub(super) fn with_engine(mut self, engine: &str) -> Self {
        self.engine = engine.into();
        self
    }

    /// `host` refuses every Prepare with `code`.
    pub(super) fn prepare_refuses(self, host: &str, code: &str) -> Self {
        self.host(host).state().prepare_refusal = Some(code.into());
        self
    }

    /// `host`'s session does not declare `capability`.
    pub(super) fn without_capability(self, host: &str, capability: &str) -> Self {
        self.host(host).state().capabilities.remove(capability);
        self
    }

    /// `host`'s profile approves engine environment names matching `globs`.
    pub(super) fn approved_env(mut self, host: &str, globs: &[&str]) -> Self {
        self.approved
            .insert(host.into(), globs.iter().map(|g| (*g).to_owned()).collect());
        self
    }

    /// `host` records another build of the profile.
    pub(super) fn profile_fingerprint(mut self, host: &str, build: &str) -> Self {
        self.builds.insert(host.into(), build.into());
        self
    }

    /// `host` publishes no peer address.
    pub(super) fn without_peer_address(mut self, host: &str) -> Self {
        self.without_peer.insert(host.into());
        self
    }

    /// `host`'s model store is `store`: the group's relative model path
    /// resolves there.
    pub(super) fn model_path(mut self, host: &str, store: &str) -> Self {
        self.model_stores.insert(host.into(), store.into());
        let path = format!("{store}/toy");
        let mut state = self.host(host).state();
        state.model_path = path.clone();
        state.digests.insert(path, GROUP_CHECKPOINT_DIGEST.into());
        drop(state);
        self
    }

    /// `host` measures `digest` for its copy of the group's weights.
    pub(super) fn checkpoint_digest(self, host: &str, digest: &str) -> Self {
        let mut state = self.host(host).state();
        let path = state.model_path.clone();
        state.digests.insert(path, digest.into());
        drop(state);
        self
    }

    /// The head's completion probes generate no token.
    pub(super) fn probe_fails(self) -> Self {
        self.hosts[0].state().probe_fails = true;
        self
    }

    /// `host` carries every Launch out but its replies are lost.
    pub(super) fn launch_replies_lost(self, host: &str) -> Self {
        self.host(host).state().lose_launch_replies = true;
        self
    }

    /// `host` starts every Launch's rank, which exits during initialization.
    pub(super) fn launch_fails(self, host: &str) -> Self {
        self.host(host).state().launch_fails = true;
        self
    }

    /// Every group this world deploys runs `recovery: reconcile`.
    pub(super) fn recovery_reconcile(mut self) -> Self {
        self.recovery = "reconcile".into();
        self
    }

    /// A world of `hosts` whose group `name` over all of them is Ready.
    pub(super) async fn ready_group(name: &str, hosts: &[&str]) -> Self {
        Self::hosts(hosts).ready(name).await
    }

    /// Deploy group `name` over every host of this world and wait for READY.
    pub(super) async fn ready(self, name: &str) -> Self {
        let names: Vec<String> = self.names().iter().map(|n| (*n).to_owned()).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let id = self.deploy_group(name, &names).await;
        self.group.launch_completes();
        self.wait_ready(&id).await;
        self
    }

    /// The scripted agent of `name`.
    pub(super) fn host(&self, name: &str) -> &Arc<GroupHost> {
        self.hosts
            .iter()
            .find(|h| h.name == name)
            .unwrap_or_else(|| panic!("{name} is not a host of this world"))
    }

    fn names(&self) -> Vec<&str> {
        self.hosts.iter().map(|h| h.name.as_str()).collect()
    }

    /// Deploy a TP N group named `name` over `hosts` (every host of the world,
    /// in rank order) and start it. Returns the deployment id.
    pub(super) async fn deploy_group(&self, name: &str, hosts: &[&str]) -> String {
        self.try_deploy(name, hosts, &[], None, true).unwrap()
    }

    /// As [`Self::deploy_group`], refused deploys and activations returned.
    pub(super) async fn try_deploy_group(
        &self,
        name: &str,
        hosts: &[&str],
    ) -> Result<String, DeployError> {
        let id = self.try_deploy(name, hosts, &[], None, true)?;
        self.activation_refusal(&id).await.map(|()| id)
    }

    /// As [`Self::deploy_group`], not started.
    pub(super) async fn deploy_group_no_start(&self, name: &str, hosts: &[&str]) -> String {
        self.try_deploy(name, hosts, &[], None, false).unwrap()
    }

    /// As [`Self::try_deploy_group`] with engine environment `env`.
    pub(super) async fn try_deploy_group_with_env(
        &self,
        name: &str,
        hosts: &[&str],
        env: &[(&str, &str)],
    ) -> Result<String, DeployError> {
        let id = self.try_deploy(name, hosts, env, None, true)?;
        self.activation_refusal(&id).await.map(|()| id)
    }

    /// As [`Self::deploy_group`] with engine environment `env`.
    pub(super) async fn deploy_group_with_env(
        &self,
        name: &str,
        hosts: &[&str],
        env: &[(&str, &str)],
    ) -> String {
        self.try_deploy(name, hosts, env, None, true).unwrap()
    }

    /// As [`Self::deploy_group`] with `residency`.
    pub(super) async fn deploy_group_with_residency(
        &self,
        name: &str,
        hosts: &[&str],
        residency: &str,
    ) -> String {
        self.try_deploy(name, hosts, &[], Some(residency), true)
            .unwrap()
    }

    /// As [`Self::try_deploy_group`] with `residency`.
    pub(super) async fn try_deploy_group_with_residency(
        &self,
        name: &str,
        hosts: &[&str],
        residency: &str,
    ) -> Result<String, DeployError> {
        let id = self.try_deploy(name, hosts, &[], Some(residency), true)?;
        self.activation_refusal(&id).await.map(|()| id)
    }

    /// Deploy a TP `tp` × PP `pp` group named `name` over every host of the
    /// world, one rank per host, and start it. Returns the deployment id.
    pub(super) async fn deploy_group_shape(&self, name: &str, tp: u32, pp: u32) -> String {
        assert_eq!(
            tp * pp,
            self.hosts.len() as u32,
            "one rank per host (ADR 0028 §2)"
        );
        self.deploy(name, tp, pp, &[], None, true).unwrap()
    }

    fn try_deploy(
        &self,
        name: &str,
        hosts: &[&str],
        env: &[(&str, &str)],
        residency: Option<&str>,
        start: bool,
    ) -> Result<String, DeployError> {
        assert_eq!(
            hosts,
            self.names().as_slice(),
            "one group over the world's hosts in rank order"
        );
        self.deploy(name, hosts.len() as u32, 1, env, residency, start)
    }

    /// `host`'s document as it publishes it (ADR 0028 §3): the golden host
    /// with this world's engine, approvals, build, model store and peer
    /// address.
    fn host_document(&self, golden: &serde_json::Value, host: &GroupHost) -> serde_json::Value {
        let mut document = golden.clone();
        let profile = &mut document["runtime_profiles"][GROUP_PROFILE];
        match self.engine.as_str() {
            "sglang" => {
                profile["engine"] = serde_json::json!("sglang");
                profile["args"] = serde_json::json!([]);
                profile["security"]["admin_credential_ref"] =
                    serde_json::json!("secret://engine-admin");
            }
            "tensorfold" => {
                profile["engine"] = serde_json::json!("tensorfold");
                profile["executable"] = serde_json::json!("/opt/tf/bin/tensorfold");
                profile["args"] = serde_json::json!([]);
                profile["security"]["deep_park"] = serde_json::json!("disabled");
            }
            _ => {}
        }
        if let Some(globs) = self.approved.get(&host.name) {
            profile["security"]["approved_env"] = serde_json::json!(globs);
        }
        if let Some(build) = self.builds.get(&host.name) {
            profile["build_fingerprint"] = serde_json::json!(build);
        }
        if let Some(store) = self.model_stores.get(&host.name) {
            document["model_store"]["path"] = serde_json::json!(store);
        }
        if !self.without_peer.contains(&host.name) {
            document["resource_policy"]["groups"] =
                serde_json::json!({ "peer_address": host.peer.to_string() });
        }
        document
    }

    /// ADR 0013 §3: each enrolled host's policy imported once (its ledger
    /// keys registered to it), as the registry imports a published host.
    fn enroll(&self, golden: &serde_json::Value) {
        let mut enrolled = self.enrolled.lock().unwrap();
        if *enrolled {
            return;
        }
        let o = self.owner.lock().unwrap();
        for host in &self.hosts {
            let document = self.host_document(golden, host);
            let policy = capyctl_config::effective::normalize_host_policy(&document).unwrap();
            o.store()
                .import_remote_resource_policy(
                    o.session(),
                    &host.name,
                    &policy,
                    &[MemoryObservation {
                        domain: "unified".into(),
                        capacity_bytes: 1 << 50,
                        available_bytes: 1 << 50,
                        sampled_at_ms: 1,
                    }],
                    1,
                )
                .unwrap();
            let controls = o.store().resource_policy(&host.name).unwrap().unwrap();
            self.domains.lock().unwrap().insert(
                host.name.clone(),
                controls.controls.domains.keys().cloned().collect(),
            );
            self.transport.published.lock().unwrap().insert(
                host.name.clone(),
                (
                    (!self.without_peer.contains(&host.name)).then_some(host.peer),
                    capyctl_config::remote_resources::policy_fingerprint(&document),
                ),
            );
        }
        *enrolled = true;
    }

    /// The group deployment `name`, accepted on every member host from that
    /// host's own scoped document, then (with `start`) started through the
    /// coordinator.
    fn deploy(
        &self,
        name: &str,
        tp: u32,
        pp: u32,
        env: &[(&str, &str)],
        residency: Option<&str>,
        start: bool,
    ) -> Result<String, DeployError> {
        let source: serde_json::Value = serde_json::from_str(include_str!(
            "../../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
        ))
        .unwrap();
        let golden = &source["input"]["host"];
        self.enroll(golden);
        let names = self.names();
        let mut deployment = group_document(&source["input"]["deployment"], name, &names, tp, pp);
        deployment.as_object_mut().unwrap().remove("host");
        deployment["recovery"] = serde_json::json!(self.recovery);
        if self.engine == "tensorfold" {
            deployment["residency"] = serde_json::json!("restart_only");
            deployment["engine_config"] = serde_json::json!({"context_length": 8192});
        }
        if let Some(residency) = residency {
            deployment["residency"] = serde_json::json!(residency);
        }
        if !env.is_empty() {
            deployment["engine_config"]["env"] = env
                .iter()
                .map(|(k, v)| ((*k).to_owned(), serde_json::json!(v)))
                .collect::<serde_json::Map<_, _>>()
                .into();
        }
        if !self.model_stores.is_empty() {
            // ADR 0013 §3: a relative path resolves in each host's own store.
            deployment["model"]["path"] = serde_json::json!("toy");
        }
        let hosts: Vec<&GroupHost> = self.hosts.iter().map(|h| h.as_ref()).collect();
        let fence = self.create(name, &deployment, golden, &hosts)?;
        if start {
            self.start_fence(&fence)?;
        }
        Ok(fence.deployment_id)
    }

    /// Deployment `name` from `deployment`, accepted on each of `hosts` from
    /// that host's own scoped document; recorded under its name.
    fn create(
        &self,
        name: &str,
        deployment: &serde_json::Value,
        golden: &serde_json::Value,
        hosts: &[&GroupHost],
    ) -> Result<DeploymentFence, DeployError> {
        let fence = {
            let o = self.owner.lock().unwrap();
            let targets: Vec<_> = hosts
                .iter()
                .map(|host| {
                    let document = self.host_document(golden, host);
                    let trusted = capyctl_config::remote_resources::scope_host_document(
                        &host.name, &document,
                    )
                    .unwrap();
                    let policy = o.store().resource_policy(&host.name).unwrap().unwrap();
                    capyctl_store::managed_configuration::HostTarget {
                        host_id: host.name.clone(),
                        host_name: host.name.clone(),
                        trusted_host: capyctl_config::effective::compose_current_resource_controls(
                            &trusted,
                            &policy.context,
                            &policy.controls,
                        )
                        .unwrap(),
                        scoped: true,
                    }
                })
                .collect();
            let receipt = o
                .store()
                .create_managed_configuration_on_hosts(
                    o.session(),
                    "owner",
                    name,
                    &serde_json::json!({ "config": deployment }).to_string(),
                    &targets,
                    &[],
                    capyctl_protocol::now_unix_ms(),
                )
                .map_err(|e| match e {
                    // The refusal's detail leads with its closed code.
                    capyctl_store::managed_configuration::ManagedConfigurationError::Rejected(
                        e,
                    ) => DeployError(e.detail),
                    e => DeployError(e.to_string()),
                })?;
            DeploymentFence {
                deployment_id: receipt.deployment_id,
                revision: receipt.revision,
                generation: receipt.generation,
            }
        };
        self.deployed
            .lock()
            .unwrap()
            .insert(name.into(), fence.clone());
        Ok(fence)
    }

    fn start_fence(&self, fence: &DeploymentFence) -> Result<(), DeployError> {
        // The start runs on in the coordinator; the world observes it.
        drop(
            self.worker
                .start(fence, capyctl_protocol::now_unix_ms() + 60_000)
                .map_err(|e| DeployError(e.to_string()))?,
        );
        Ok(())
    }

    /// Start deployment `id` at the fence it was accepted with.
    pub(super) async fn start(&self, id: &str) -> Result<(), DeployError> {
        let fence = self
            .deployed
            .lock()
            .unwrap()
            .values()
            .find(|f| f.deployment_id == id)
            .cloned()
            .expect("a deployment of this world");
        self.start_fence(&fence)
    }

    fn id(&self, name: &str) -> String {
        self.deployed.lock().unwrap()[name].deployment_id.clone()
    }

    async fn until<T>(&self, what: &str, mut ready: impl FnMut() -> Option<T>) -> T {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Some(value) = ready() {
                    return value;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "{what} never happened; activations: {:?}; worker: {:?}",
                self.transport.concluded.lock().unwrap(),
                self.worker.status()
            )
        })
    }

    /// The first activation of `id` that concluded: `Err` with its closed code
    /// when it was refused before any member was dispatched.
    async fn activation_refusal(&self, id: &str) -> Result<(), DeployError> {
        let outcome = self
            .until("an activation", || {
                self.transport.concluded.lock().unwrap().get(id).cloned()
            })
            .await;
        outcome
            .map(drop)
            .map_err(|e| DeployError(format!("{}: {e}", e.code())))
    }

    /// How deployment `id`'s activation concluded.
    pub(super) async fn wait_activation(&self, id: &str) -> GroupActivation {
        self.until("an activation", || {
            self.transport
                .concluded
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .map(|o| o.expect("the activation reached its launches"))
        })
        .await
    }

    /// Wait until deployment `id` is observed ready; panics after 30 s.
    pub(super) async fn wait_ready(&self, id: &str) {
        self.until(&format!("deployment {id} ready"), || {
            let o = self.owner.lock().unwrap();
            o.store()
                .get_deployment(id)
                .unwrap()
                .filter(|d| d.observed_state == capyctl_domain::LifecycleState::Ready)
                .map(drop)
        })
        .await
    }

    /// Deployment `id`'s instance as status reads it: its `last_error` and
    /// the members of its newest plan.
    fn read_status(&self, id: &str) -> Option<GroupStatus> {
        let o = self.owner.lock().unwrap();
        let members = o
            .store()
            .group_plan(id, 0)
            .unwrap()
            .map(|(_, rows)| {
                rows.iter()
                    .map(|r| MemberStatus {
                        state: r.state.as_str().into(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let snapshot = o.store().snapshot().unwrap();
        let instance = snapshot
            .deployments
            .iter()
            .find(|d| d.id == id)
            .and_then(|d| d.instances.first().cloned())?;
        Some(GroupStatus {
            last_error: instance.last_error,
            members,
        })
    }

    /// Deployment `name`'s instance as status reads it now.
    pub(super) async fn status(&self, name: &str) -> GroupStatus {
        self.read_status(&self.id(name))
            .expect("a deployed instance")
    }

    /// As [`Self::status`], for deployment `id`.
    pub(super) async fn status_of(&self, id: &str) -> GroupStatus {
        self.read_status(id).expect("a deployed instance")
    }

    /// Wait until deployment `id`'s start gave up with every reservation
    /// released; its status.
    pub(super) async fn wait_settled(&self, id: &str) -> GroupStatus {
        self.until(&format!("deployment {id} settled"), || {
            let status = self.read_status(id)?;
            // Every reservation released, and the instance's status names why.
            (status.members.iter().all(|m| m.state == "settled") && status.last_error.is_some())
                .then_some(status)
        })
        .await
    }

    /// Wait until the plan of deployment `name` at `generation` settled:
    /// every member released on its own host's evidence and the rendezvous
    /// port freed with the last; its instance's status.
    pub(super) async fn wait_settled_generation(&self, name: &str, generation: i64) -> GroupStatus {
        let id = self.id(name);
        self.until(
            &format!("generation {generation} of {name} settled"),
            || {
                let settled = {
                    let o = self.owner.lock().unwrap();
                    o.store()
                        .group_plan_at(&id, 0, generation)
                        .unwrap()
                        .is_some_and(|(_, rows)| {
                            rows.iter().all(|r| r.state == MemberState::Settled)
                        })
                };
                settled.then(|| self.read_status(&id)).flatten()
            },
        )
        .await
    }

    /// Whether no unsettled plan holds `port` on `host` as its rendezvous
    /// port (ADR 0028 §11: released only once every member settled).
    pub(super) fn port_free(&self, host: &str, port: u16) -> bool {
        let o = self.owner.lock().unwrap();
        !o.store().rendezvous_port_held_on(host, port).unwrap()
    }

    /// An operator's Stop of deployment `id`'s Ready group, waited on until
    /// every member either settled or was left uncertain.
    pub(super) async fn stop(&self, id: &str) {
        let fence = self
            .deployed
            .lock()
            .unwrap()
            .values()
            .find(|f| f.deployment_id == id)
            .cloned()
            .expect("a deployment of this world");
        drop(
            self.worker
                .stop(
                    "owner",
                    &fence,
                    "stop",
                    capyctl_protocol::now_unix_ms() + 60_000,
                )
                .unwrap(),
        );
        self.until("every member settled or uncertain", || {
            let status = self.read_status(id)?;
            status
                .members
                .iter()
                .all(|m| m.state == "settled" || m.state == "uncertain")
                .then_some(())
        })
        .await
    }

    /// SPEC §11: time passes far beyond any lease: the store's time-driven
    /// reconciliation runs a day from now, and the worker's own retries run
    /// several times. Nothing of it may release an uncertain member.
    pub(super) async fn advance_past_lease_expiry(&self) {
        {
            let o = self.owner.lock().unwrap();
            o.store()
                .reconcile_instances(
                    o.session(),
                    capyctl_protocol::now_unix_ms() + 24 * 3_600_000,
                    None,
                    false,
                )
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    /// Let the world run for `duration`.
    pub(super) async fn settle_for(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    /// Whether a Launch of deployment `name`'s group at `generation` reached
    /// its head.
    pub(super) fn generation_started(&self, name: &str, generation: i64) -> bool {
        let id = self.id(name);
        self.hosts[0].received().iter().any(|c| {
            matches!(c.action, MemberAction::Launch { .. })
                && c.identity.deployment_id == id
                && c.identity.generation == generation
        })
    }

    /// Wait for `generation` of `name` to start; whether every Launch of it
    /// arrived only after the previous plan had fully settled.
    pub(super) async fn generation_started_after_settlement(
        &self,
        name: &str,
        generation: i64,
    ) -> bool {
        self.until(
            &format!("generation {generation} of {name} started"),
            || self.generation_started(name, generation).then_some(()),
        )
        .await;
        let witnessed = self.witnessed.lock().unwrap();
        let launches: Vec<bool> = witnessed
            .iter()
            .filter(|(g, _)| *g == generation)
            .map(|(_, settled)| *settled)
            .collect();
        !launches.is_empty() && launches.iter().all(|settled| *settled)
    }

    /// Wait until every host received its Launch, while the head is still
    /// waiting for the group to form.
    pub(super) async fn agents_saw_launch_before_head_ready(&self, name: &str) {
        let _ = self.id(name);
        self.until("every Launch", || {
            self.hosts
                .iter()
                .all(|h| {
                    h.received()
                        .iter()
                        .any(|c| matches!(c.action, MemberAction::Launch { .. }))
                })
                .then_some(())
        })
        .await;
        assert!(
            !self.group.launch_completed(),
            "every Launch went out before the engine initialized"
        );
    }

    /// Whether deployment `name` has an instance open for dispatch, and that
    /// it is the head's ingress (ADR 0028 §9: one replica, at the head). The
    /// serving host is the binding's frozen ingress host; a group records no
    /// single-host placement.
    pub(super) fn route_open(&self, name: &str) -> bool {
        let id = self.id(name);
        let o = self.owner.lock().unwrap();
        let serving = o.store().serving_instances(&id).unwrap();
        let open: Vec<_> = serving.iter().filter(|s| s.dispatch_open).collect();
        assert!(open.len() <= 1, "a group is one replica");
        open.first().is_some_and(|instance| {
            assert_eq!(
                instance.remote_host.as_deref(),
                Some(self.hosts[0].name.as_str()),
                "the group serves at the head"
            );
            true
        })
    }

    /// The bytes `owner_id` holds on `host`'s own domains.
    pub(super) fn owner_bytes_on(&self, host: &str, owner_id: &str) -> i64 {
        let o = self.owner.lock().unwrap();
        let ledger = o.store().resource_snapshot().unwrap();
        let prefix = capyctl_config::remote_resources::ledger_key(host, "domain", "");
        ledger.owners.get(owner_id).map_or(0, |footprint| {
            footprint
                .allocations
                .iter()
                .filter(|a| a.domain.starts_with(&prefix))
                .map(|a| a.bytes)
                .sum()
        })
    }

    /// The bytes one Ready member is charged on its host: the golden
    /// deployment's Ready footprint.
    pub(super) fn member_request(&self) -> i64 {
        8 << 30
    }

    /// How many commands `host` received.
    pub(super) fn commands_sent_to(&self, host: &str) -> usize {
        self.host(host).received().len()
    }

    fn probes(&self, host: &str) -> Vec<Option<u32>> {
        self.host(host)
            .received()
            .into_iter()
            .filter_map(|c| match c.action {
                MemberAction::Probe { max_tokens, .. } => Some(max_tokens),
                _ => None,
            })
            .collect()
    }

    /// How many probes `host` received.
    pub(super) fn probes_sent_to(&self, host: &str) -> usize {
        self.probes(host).len()
    }

    /// How many completion probes the group's head was sent.
    pub(super) fn probe_calls(&self) -> usize {
        self.probes(&self.hosts[0].name).len()
    }

    /// The token bound of the last probe the head was sent.
    pub(super) fn last_probe_max_tokens(&self) -> u32 {
        self.probes(&self.hosts[0].name)
            .last()
            .copied()
            .flatten()
            .expect("a completion probe")
    }

    /// The engine environment `host`'s Launch rendered, once it has one.
    pub(super) async fn launch_env(&self, host: &str) -> BTreeMap<String, String> {
        let host = self.host(host).clone();
        self.until("a Launch", || host.state().launch_env.clone())
            .await
    }

    /// The plan and members of deployment `name`'s instance, if any.
    pub(super) fn group_plan(
        &self,
        name: &str,
    ) -> Option<(GroupPlan, Vec<capyctl_store::groups::MemberRow>)> {
        let id = self.id(name);
        let o = self.owner.lock().unwrap();
        o.store().group_plan(&id, 0).unwrap()
    }

    /// The coordinator's state, for scripted observers.
    pub(super) fn owner(&self) -> SharedCoordinatorState {
        self.owner.clone()
    }

    /// How many rank processes the hosts started (a replayed Launch starts
    /// none).
    pub(super) fn launches(&self) -> u32 {
        self.group.launches()
    }
}

/// The golden deployment as a TP `tp` × PP `pp` group named `name` over
/// `hosts` (ADR 0028 §2, §4).
fn group_document(
    golden: &serde_json::Value,
    name: &str,
    hosts: &[&str],
    tp: u32,
    pp: u32,
) -> serde_json::Value {
    let mut deployment = golden.clone();
    deployment["name"] = serde_json::json!(name);
    deployment["routes"] = serde_json::json!([name]);
    deployment["topology"] = serde_json::json!({"tensor_parallel": tp, "pipeline_parallel": pp});
    deployment["placement"] = serde_json::json!({ "hosts": hosts });
    deployment
}

// ---- commands as the server sends them --------------------------------------

/// A canonical ULID-shaped id for test commands.
fn ulid(n: u32) -> String {
    format!("01K{n:023}")
}

fn identity(
    host: &str,
    rank: u32,
    command_id: &str,
    expected_state: &str,
    generation: i64,
) -> CommandIdentity {
    CommandIdentity {
        controller_id: GROUP_CONTROLLER.into(),
        member: MemberKey {
            host_id: host.into(),
            member_id: member_id(rank),
        },
        deployment_id: "deployment".into(),
        operation_id: "operation".into(),
        command_id: command_id.into(),
        step_id: command_id.into(),
        generation,
        revision: 1,
        deadline_ms: capyctl_protocol::now_unix_ms() + 60_000,
        payload_digest: [0; 32],
        expected_state: expected_state.into(),
        profile_fingerprint: GROUP_PROFILE_BUILD.into(),
        instance_index: 0,
    }
}

fn sealed(identity: CommandIdentity, action: MemberAction) -> MemberCommand {
    let mut command = MemberCommand { identity, action };
    command.identity.payload_digest = command.canonical_digest();
    command
}

/// The vLLM plan over `hosts` in rank order: one rank per host at its
/// documentation peer address, the head serving on 8100.
fn plan(hosts: &[&str], rendezvous: u16, generation: i64) -> GroupPlan {
    plan_with(hosts, rendezvous, generation, 1)
}

fn plan_with(hosts: &[&str], rendezvous: u16, generation: i64, local_ranks: u32) -> GroupPlan {
    let members = hosts
        .iter()
        .enumerate()
        .map(|(index, host)| {
            let rank = index as u32;
            MemberPlan {
                member: MemberKey {
                    host_id: (*host).into(),
                    member_id: member_id(rank),
                },
                rank,
                role: if rank == 0 {
                    MemberRole::Head
                } else {
                    MemberRole::Worker
                },
                profile_name: GROUP_PROFILE.into(),
                profile_fingerprint: GROUP_PROFILE_BUILD.into(),
                checkpoint_fingerprint: GROUP_CHECKPOINT_DIGEST.into(),
                model_path: GROUP_MODEL_PATH.into(),
                devices: (0..local_ranks).map(|d| format!("gpu{d}")).collect(),
                peer_address: peer_address(index),
                service_port: (rank == 0).then_some(8100),
                worker_port: None,
            }
        })
        .collect();
    GroupPlan::new(
        GroupEngine::Vllm,
        members,
        GroupTopology {
            tensor_parallel: hosts.len() as u32 * local_ranks,
            pipeline_parallel: 1,
            local_ranks,
        },
        rendezvous,
        generation,
    )
    .unwrap()
}

fn prepare(host: &str, rank: u32, plan: &GroupPlan) -> MemberCommand {
    sealed(
        identity(host, rank, "prepare", "reserved", plan.generation()),
        MemberAction::Prepare(plan.clone()),
    )
}

/// ADR 0028 §8 (R29): host `host`'s Launch of its member of `plan`.
fn launch(host: &str, rank: u32, plan: &GroupPlan, command_id: &str) -> MemberCommand {
    let source: serde_json::Value = serde_json::from_str(include_str!(
        "../../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let hosts: Vec<String> = plan
        .members()
        .iter()
        .map(|m| m.member.host_id.clone())
        .collect();
    let hosts: Vec<&str> = hosts.iter().map(String::as_str).collect();
    let topology = plan.topology();
    let document = group_document(
        &source["input"]["deployment"],
        "g",
        &hosts,
        topology.tensor_parallel,
        topology.pipeline_parallel,
    );
    sealed(
        identity(host, rank, command_id, "reserved", plan.generation()),
        MemberAction::Launch {
            plan: plan.clone(),
            member: SingleLaunchPlan {
                deployment_config: document.to_string(),
                profile_name: GROUP_PROFILE.into(),
                checkpoint_fingerprint: GROUP_CHECKPOINT_DIGEST.into(),
                host_policy_fingerprint: "a".repeat(64),
                binding_id: ulid(rank * 10 + 1),
                incarnation: ulid(rank * 10 + 2),
                grant_id: ulid(rank * 10 + 3),
                service_port: plan.members()[rank as usize].service_port.unwrap_or(0),
                issued_at_ms: capyctl_protocol::now_unix_ms(),
                coordinator_session_id: ulid(99),
                checkpoint_digest: GROUP_CHECKPOINT_DIGEST.into(),
                checkpoint_weights_bytes: None,
                startup_bytes: None,
                checkpoint_state_slot_bytes: None,
            },
        },
    )
}

fn terminate(
    host: &str,
    rank: u32,
    owned_handle: &str,
    recorded: Vec<ProcessIdentity>,
) -> MemberCommand {
    sealed(
        identity(host, rank, &format!("stop-{owned_handle}"), "retained", 1),
        MemberAction::Terminate {
            owned_handle: owned_handle.into(),
            recorded,
        },
    )
}

fn identities(result: &pb::MemberExecutionResult) -> Vec<ProcessIdentity> {
    result
        .processes
        .iter()
        .map(|p| ProcessIdentity {
            role: p.role.clone(),
            pid: p.pid,
            boot_id: p.boot_id.clone(),
            start_ticks: p.start_ticks,
        })
        .collect()
}

fn all_gone(result: &pb::MemberExecutionResult) -> bool {
    !result.processes.is_empty() && result.processes.iter().all(|p| p.presence == "gone")
}

// ---- the scripted hosts' contracts --------------------------------------------

// T30, Review Focus 2: Prepare runs the agent's own member checks and refuses
// with one closed code; a passing Prepare claims nothing.
#[tokio::test]
async fn scripted_prepare_refuses_with_closed_codes() {
    let (_, hosts) = scripted(&["host-a", "host-b"]);
    let (a, b) = (&hosts[0], &hosts[1]);
    let p = plan(&["host-a", "host-b"], 25000, 1);
    for (rank, host) in [a, b].into_iter().enumerate() {
        let passed = host
            .execute(prepare(&host.name, rank as u32, &p))
            .await
            .unwrap();
        assert_eq!(passed.refused, "");
        assert!(!passed.claim_retained && passed.processes.is_empty());
    }
    // A port held outside CapyCTL is named.
    a.hold_port(25000);
    let refused = a.execute(prepare("host-a", 0, &p)).await.unwrap();
    assert_eq!(refused.refused, "rendezvous_port_in_use:25000");
    a.hold_port(8100);
    let p2 = plan(&["host-a", "host-b"], 25001, 1);
    let refused = a.execute(prepare("host-a", 0, &p2)).await.unwrap();
    assert_eq!(refused.refused, "service_port_in_use:8100");
    // A plan whose addresses are swapped names another host's address.
    let swapped = plan(&["host-b", "host-a"], 25001, 1);
    let refused = a.execute(prepare("host-a", 1, &swapped)).await.unwrap();
    assert_eq!(refused.refused, "peer_address_not_local");
    // An unmeasured model path and another build.
    b.state().digests.clear();
    assert_eq!(
        b.execute(prepare("host-b", 1, &p)).await.unwrap().refused,
        "group_checkpoint_mismatch"
    );
    b.state()
        .profiles
        .insert(GROUP_PROFILE.into(), "another-build".into());
    assert_eq!(
        b.execute(prepare("host-b", 1, &p)).await.unwrap().refused,
        "group_profile_mismatch"
    );
    // R35: more than one rank per member.
    let wide = plan_with(&["host-a", "host-b"], 25002, 1, 2);
    assert_eq!(
        b.execute(prepare("host-b", 1, &wide))
            .await
            .unwrap()
            .refused,
        "group_topology_invalid"
    );
    assert_eq!(a.received().len(), 4);
}

// T30 (R23): a Launch journals the member before its rank starts and reports
// its identities; the same command again replays them and starts nothing;
// another command for the claimed member is refused uncertain.
#[tokio::test]
async fn scripted_launch_journals_first_and_replays() {
    let (group, hosts) = scripted(&["host-a", "host-b"]);
    let b = &hosts[1];
    let p = plan(&["host-a", "host-b"], 25000, 1);
    let command = launch("host-b", 1, &p, "launch-b");
    let first = b.execute(command.clone()).await.unwrap();
    assert_eq!(first.state, "launched");
    assert!(first.claim_retained && !first.model_usable);
    assert_eq!(
        first
            .processes
            .iter()
            .map(|p| p.role.as_str())
            .collect::<Vec<_>>(),
        vec!["worker-1", "worker-1/worker-0"]
    );
    assert_eq!(b.recorded("launch-b").unwrap(), identities(&first));
    assert_eq!(group.launches(), 1);
    let again = b.execute(command.clone()).await.unwrap();
    assert_eq!(again, first);
    assert_eq!(group.launches(), 1, "a replay starts nothing");
    let mut changed = command;
    changed.identity.deadline_ms += 1;
    changed.identity.payload_digest = changed.canonical_digest();
    assert_eq!(b.execute(changed).await, Err(HostError::Conflict));
    assert_eq!(
        b.execute(launch("host-b", 1, &p, "launch-b-2")).await,
        Err(HostError::Uncertain)
    );
    assert_eq!(group.launches(), 1);
    // R7: a launch whose checks fail again is refused and journals nothing.
    let (_, hosts) = scripted(&["host-a", "host-b"]);
    hosts[0].hold_port(25000);
    let refused = hosts[0]
        .execute(launch("host-a", 0, &p, "launch-a"))
        .await
        .unwrap();
    assert_eq!(refused.refused, "rendezvous_port_in_use:25000");
    assert!(hosts[0].recorded("launch-a").is_none());
}

// T30: the head answers usable only once every rank launched and the engine
// finished initializing.
#[tokio::test]
async fn scripted_head_is_ready_only_after_every_rank_and_initialization() {
    let (group, hosts) = scripted(&["host-a", "host-b"]);
    let p = plan(&["host-a", "host-b"], 25000, 1);
    let head = {
        let a = hosts[0].clone();
        let command = launch("host-a", 0, &p, "launch-a");
        tokio::spawn(async move { a.execute(command).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!head.is_finished(), "the head waits for every rank");
    hosts[1]
        .execute(launch("host-b", 1, &p, "launch-b"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!head.is_finished(), "the head waits for initialization");
    group.launch_completes();
    let reply = head.await.unwrap().unwrap();
    assert!(reply.model_usable && reply.claim_retained);
    assert_eq!(
        reply
            .processes
            .iter()
            .map(|p| p.role.as_str())
            .collect::<Vec<_>>(),
        vec!["api", "worker-0"]
    );
    assert_eq!(group.launches(), 2);
}

// T31, Review Focus 6: Terminate ends the recorded rank and reports it gone;
// a rank left hanging by another's exit needs escalation.
#[tokio::test]
async fn scripted_terminate_reports_gone_and_escalates_a_hung_rank() {
    let (group, hosts) = scripted(&["host-a", "host-b"]);
    let p = plan(&["host-a", "host-b"], 25000, 1);
    group.launch_completes();
    let worker = hosts[1].clone();
    let launched_b = tokio::spawn({
        let command = launch("host-b", 1, &p, "launch-b");
        async move { worker.execute(command).await }
    });
    let a = hosts[0]
        .execute(launch("host-a", 0, &p, "launch-a"))
        .await
        .unwrap();
    let b = launched_b.await.unwrap().unwrap();
    assert!(a.model_usable);
    group.exit_rank(0);
    let gone = hosts[1]
        .execute(terminate("host-b", 1, "launch-b", identities(&b)))
        .await
        .unwrap();
    assert!(all_gone(&gone) && !gone.claim_retained);
    assert!(gone.escalated, "the worker hung after the head died");
    assert!(!group.alive(1));
    let gone = hosts[0]
        .execute(terminate("host-a", 0, "launch-a", identities(&a)))
        .await
        .unwrap();
    assert!(all_gone(&gone) && !gone.escalated);
}

// T32, Review Focus 3: a disconnected host answers nothing; back with an
// empty journal it never reports its live member gone and signals nothing,
// until the process is really gone.
#[tokio::test]
async fn an_empty_journal_host_never_reports_a_live_member_gone() {
    let (group, hosts) = scripted(&["host-a", "host-b"]);
    let b = &hosts[1];
    let p = plan(&["host-a", "host-b"], 25000, 1);
    let launched = b
        .execute(launch("host-b", 1, &p, "launch-b"))
        .await
        .unwrap();
    let recorded = identities(&launched);
    group.disconnect_host("host-b");
    assert_eq!(
        b.execute(terminate("host-b", 1, "launch-b", recorded.clone()))
            .await,
        Err(HostError::Unreachable)
    );
    group.reconnect_host("host-b", JournalState::Empty);
    let observed = b
        .execute(terminate("host-b", 1, "launch-b", recorded.clone()))
        .await
        .unwrap();
    assert!(!observed.claim_retained);
    assert!(observed.processes.iter().all(|p| p.presence == "alive"));
    assert!(group.alive(1), "a host without a journal signals nothing");
    group.kill_rank_process(1);
    let gone = b
        .execute(terminate("host-b", 1, "launch-b", recorded))
        .await
        .unwrap();
    assert!(all_gone(&gone) && !gone.escalated);
}

// T34: a host without `engine_groups` takes no group command.
#[tokio::test]
async fn a_host_without_engine_groups_takes_no_group_command() {
    let (group, hosts) = scripted(&["host-a", "host-b"]);
    hosts[1]
        .state()
        .capabilities
        .remove(capabilities::ENGINE_GROUPS);
    let p = plan(&["host-a", "host-b"], 25000, 1);
    assert_eq!(
        hosts[1].execute(prepare("host-b", 1, &p)).await,
        Err(HostError::CapabilityMissing(
            capabilities::ENGINE_GROUPS.into()
        ))
    );
    assert_eq!(
        hosts[1].execute(launch("host-b", 1, &p, "launch-b")).await,
        Err(HostError::CapabilityMissing(
            capabilities::ENGINE_GROUPS.into()
        ))
    );
    assert_eq!(group.launches(), 0);
}

// T34: an agent takes commands from its own controller only.
#[tokio::test]
async fn a_command_from_another_controller_is_unauthorized() {
    let (group, hosts) = scripted(&["host-a", "host-b"]);
    let p = plan(&["host-a", "host-b"], 25000, 1);
    let mut foreign = launch("host-b", 1, &p, "launch-b");
    foreign.identity.controller_id = "another-controller".into();
    let foreign = sealed(foreign.identity, foreign.action);
    assert_eq!(
        hosts[1].execute(foreign).await,
        Err(HostError::Unauthorized)
    );
    assert_eq!(group.launches(), 0);
    assert!(hosts[1].recorded("launch-b").is_none());
}

// T31: a Terminate is authorized against a retained launch only; a wrong
// expected state ends nothing.
#[tokio::test]
async fn a_terminate_outside_the_retained_state_is_unauthorized() {
    let (group, hosts) = scripted(&["host-a", "host-b"]);
    let p = plan(&["host-a", "host-b"], 25000, 1);
    let launched = hosts[1]
        .execute(launch("host-b", 1, &p, "launch-b"))
        .await
        .unwrap();
    let stop = terminate("host-b", 1, "launch-b", identities(&launched));
    let mut wrong = stop.identity.clone();
    wrong.expected_state = "reserved".into();
    let wrong = sealed(wrong, stop.action.clone());
    assert_eq!(hosts[1].execute(wrong).await, Err(HostError::Unauthorized));
    assert!(group.alive(1));
    assert!(all_gone(&hosts[1].execute(stop).await.unwrap()));
}

// ---- the world ---------------------------------------------------------------

// T03, T30: a world's hosts are scripted agents at their own documentation
// addresses, ranked as named, sharing one group; the document the world
// deploys is a group over them, head first.
#[tokio::test]
async fn group_world_hosts_run_one_group_in_rank_order() {
    let names = ["h0", "h1", "h2", "h3"];
    let world = GroupWorld::hosts(&names);
    assert_eq!(world.group.hosts(), names.map(String::from).to_vec());
    let p = plan(&names, 25000, 1);
    for (rank, name) in names.iter().enumerate() {
        let host = world.host(name);
        assert_eq!(host.peer, peer_address(rank));
        let answer = host.execute(prepare(name, rank as u32, &p)).await.unwrap();
        assert_eq!(answer.refused, "", "{name}");
    }
    let source: serde_json::Value = serde_json::from_str(include_str!(
        "../../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let document = group_document(&source["input"]["deployment"], "g", &names, 2, 2);
    let group = capyctl_config::instances::parse_instance_spec(&document)
        .unwrap()
        .group
        .expect("a group deployment");
    assert_eq!(group.head(), "h0");
    assert_eq!(group.topology.world_size(), 4);
    assert_eq!(world.launches(), 0);
}

// Harness smoke: a four-host world deploys a TP2 x PP2 group through the coordinator.
// OD4: no shape gate for more than two hosts or PP > 1.
#[tokio::test]
async fn group_world_four_hosts_smoke() {
    let world = GroupWorld::hosts(&["h0", "h1", "h2", "h3"]);
    let id = world.deploy_group_shape("g", 2, 2).await;
    world.group.launch_completes();
    world.wait_ready(&id).await;
    assert_eq!(world.launches(), 4);
}

// ---- Task 16: group activation ------------------------------------------------

use capyctl_store::groups::{member_owner_id, MemberState};

// T30: a clean activation reserves all, launches all concurrently, routes only after head readiness.
#[tokio::test]
async fn group_activates_and_routes_after_head_readiness() {
    for engine in ["vllm", "sglang", "tensorfold"] {
        let world = GroupWorld::hosts(&["host-a", "host-b"]).with_engine(engine);
        let id = world.deploy_group("g", &["host-a", "host-b"]).await;
        world.agents_saw_launch_before_head_ready("g").await;
        assert!(!world.route_open("g"));
        world.group.launch_completes();
        world.wait_ready(&id).await;
        assert!(world.route_open("g"), "{engine}");
        // ADR 0028 §10: each member renders its own peer address under the
        // engine's name; TensorFold reads none.
        let (a, b) = (
            world.launch_env("host-a").await,
            world.launch_env("host-b").await,
        );
        match engine {
            "tensorfold" => assert!(a.is_empty() && b.is_empty()),
            _ => {
                let name = if engine == "vllm" {
                    "VLLM_HOST_IP"
                } else {
                    "SGLANG_HOST_IP"
                };
                assert_eq!(a[name], peer_address(0).to_string());
                assert_eq!(b[name], peer_address(1).to_string());
            }
        }
        assert_eq!(
            world.owner_bytes_on("host-b", &member_owner_id(&id, 0, 1)),
            world.member_request(),
            "{engine}"
        );
        // ADR 0028 §5: the instance owner holds nothing; each member is
        // charged on its own host only.
        assert_eq!(
            world.owner_bytes_on("host-a", &member_owner_id(&id, 0, 1)),
            0
        );
    }
}

// T30: a Prepare refusal on one host releases every member and launches nothing.
#[tokio::test]
async fn prepare_refusal_releases_everything() {
    let world = GroupWorld::hosts(&["host-a", "host-b"])
        .prepare_refuses("host-b", "host_tuning_missing:memlock");
    let id = world.deploy_group("g", &["host-a", "host-b"]).await;
    let status = world.wait_settled(&id).await;
    assert_eq!(status.last_error(), "host_tuning_missing:memlock");
    assert_eq!(world.launches(), 0);
    assert_eq!(
        world.owner_bytes_on("host-a", &member_owner_id(&id, 0, 0)),
        0
    );
    assert_eq!(
        world.owner_bytes_on("host-b", &member_owner_id(&id, 0, 1)),
        0
    );
}

// T15: concurrent activation requests produce one plan and one launch per member.
#[tokio::test]
async fn concurrent_activation_is_single() {
    let world = GroupWorld::hosts(&["host-a", "host-b"]);
    let id = world
        .deploy_group_no_start("g", &["host-a", "host-b"])
        .await;
    let (a, b) = tokio::join!(world.start(&id), world.start(&id));
    assert!(a.is_ok() && b.is_ok());
    world.group.launch_completes();
    world.wait_ready(&id).await;
    assert_eq!(world.launches(), 2);
}

// T34: a named host without engine_groups is refused typed and receives nothing;
// the instance's status names the code.
#[tokio::test]
async fn host_without_capability_is_refused() {
    for missing in ["host-a", "host-b"] {
        let world =
            GroupWorld::hosts(&["host-a", "host-b"]).without_capability(missing, "engine_groups");
        let err = world
            .try_deploy_group("g", &["host-a", "host-b"])
            .await
            .unwrap_err();
        assert_eq!(err.code(), "host_capability_missing:engine_groups");
        assert_eq!(world.commands_sent_to(missing), 0);
        assert!(world.group_plan("g").is_none(), "nothing is reserved");
        assert_eq!(
            world.wait_settled(&world.id("g")).await.last_error(),
            "host_capability_missing:engine_groups"
        );
    }
}

// T14, T37, Review Focus 1: an env name unapproved on one host refuses the group;
// members render equal environments apart from the address variable.
#[tokio::test]
async fn group_engine_env_is_approved_everywhere_and_equal() {
    let world = GroupWorld::hosts(&["host-a", "host-b"])
        .with_engine("sglang")
        .approved_env("host-a", &["SGLANG_ENABLE_*"]);
    let err = world
        .try_deploy_group_with_env("g", &["host-a", "host-b"], &[("SGLANG_ENABLE_X", "1")])
        .await
        .unwrap_err();
    assert_eq!(err.code(), "engine_env_not_approved:SGLANG_ENABLE_X");
    let world = GroupWorld::hosts(&["host-a", "host-b"])
        .with_engine("sglang")
        .approved_env("host-a", &["SGLANG_ENABLE_*"])
        .approved_env("host-b", &["SGLANG_ENABLE_*"]);
    let id = world
        .deploy_group_with_env("g", &["host-a", "host-b"], &[("SGLANG_ENABLE_X", "1")])
        .await;
    world.group.launch_completes();
    world.wait_ready(&id).await;
    // ADR 0028 §2.1, §10: the members' final launch environments are equal
    // apart from each member's own address variable, and carry the value.
    let strip = |mut env: BTreeMap<String, String>| {
        env.remove("SGLANG_HOST_IP");
        env
    };
    let (a, b) = (
        strip(world.launch_env("host-a").await),
        strip(world.launch_env("host-b").await),
    );
    assert_eq!(a.get("SGLANG_ENABLE_X").map(String::as_str), Some("1"));
    assert_eq!(a, b);
}

// T14: one mismatched build or a host without a peer address refuses the deploy.
#[tokio::test]
async fn profile_mismatch_and_missing_peer_address_refuse_deploy() {
    let world = GroupWorld::hosts(&["host-a", "host-b"]).profile_fingerprint("host-b", "other");
    assert_eq!(
        world
            .try_deploy_group("g", &["host-a", "host-b"])
            .await
            .unwrap_err()
            .code(),
        "group_profile_mismatch"
    );
    let world = GroupWorld::hosts(&["host-a", "host-b"]).without_peer_address("host-b");
    assert_eq!(
        world
            .try_deploy_group("g", &["host-a", "host-b"])
            .await
            .unwrap_err()
            .code(),
        "peer_address_missing"
    );
}

// T30 (decided 2026-10-06): a readiness probe that fails is a launch failure; the route never opens.
// Stopping and settling the members after it is Task 17's (`readiness_probe_failure_stops_the_group`).
#[tokio::test]
async fn failing_readiness_probe_fails_the_activation() {
    let world = GroupWorld::hosts(&["host-a", "host-b"]).probe_fails();
    let id = world.deploy_group("g", &["host-a", "host-b"]).await;
    world.group.launch_completes();
    assert!(matches!(
        world.wait_activation(&id).await,
        GroupActivation::Failed { failed_rank: 0, .. }
    ));
    assert!(!world.route_open("g"));
    assert_eq!(world.probe_calls(), 1);
}

// T30 (decided 2026-10-06): a passing 1-token probe through the head opens the route; workers are never probed.
#[tokio::test]
async fn readiness_probe_goes_through_the_head_only() {
    let world = GroupWorld::hosts(&["host-a", "host-b"]);
    let id = world.deploy_group("g", &["host-a", "host-b"]).await;
    world.group.launch_completes();
    world.wait_ready(&id).await;
    assert_eq!(world.probe_calls(), 1);
    assert_eq!(world.last_probe_max_tokens(), 1);
    assert_eq!(world.probes_sent_to("host-b"), 0);
}

// T14 (decided 2026-10-06): a deep SGLang group whose member paths differ is refused before
// the reservation, naming both paths; a restart-only SGLang group with the same differing paths activates.
#[tokio::test]
async fn sglang_deep_group_with_differing_paths_is_refused() {
    let world = GroupWorld::hosts(&["host-a", "host-b"])
        .with_engine("sglang")
        .model_path("host-a", "/models/a")
        .model_path("host-b", "/models/b");
    let err = world
        .try_deploy_group_with_residency("g", &["host-a", "host-b"], "deep")
        .await
        .unwrap_err();
    assert_eq!(err.code(), "group_model_path_mismatch");
    assert!(err.to_string().contains("/models/a") && err.to_string().contains("/models/b"));
    assert!(world.group_plan("g").is_none());
    let id = world.id("g");
    assert_eq!(
        world.wait_settled(&id).await.last_error(),
        "group_model_path_mismatch"
    );
    assert_eq!(
        world.owner_bytes_on("host-b", &member_owner_id(&id, 0, 1)),
        0
    );
    assert_eq!(world.launches(), 0);
    let world = GroupWorld::hosts(&["host-a", "host-b"])
        .with_engine("sglang")
        .model_path("host-a", "/models/a")
        .model_path("host-b", "/models/b");
    let id = world
        .deploy_group_with_residency("g", &["host-a", "host-b"], "restart_only")
        .await;
    world.group.launch_completes();
    world.wait_ready(&id).await;
}

// T14: members whose checkpoint digests differ refuse the group before anything is
// reserved or launched; the instance's status names the code.
#[tokio::test]
async fn differing_checkpoint_digests_refuse_the_group() {
    let world = GroupWorld::hosts(&["host-a", "host-b"]).checkpoint_digest(
        "host-b",
        "sha256:0000000000000000000000000000000000000000000000000000000000000001",
    );
    let err = world
        .try_deploy_group("g", &["host-a", "host-b"])
        .await
        .unwrap_err();
    assert_eq!(err.code(), "group_checkpoint_mismatch");
    assert!(world.group_plan("g").is_none(), "nothing is reserved");
    assert_eq!(world.launches(), 0);
    assert_eq!(
        world.wait_settled(&world.id("g")).await.last_error(),
        "group_checkpoint_mismatch"
    );
}

// T30, T33 (R23): every member is fenced dispatched, durably, before its host
// receives its Launch; after the reply its identities are the reply's.
#[tokio::test]
async fn members_are_fenced_before_launch_and_record_the_reply_identities() {
    let names = ["host-a", "host-b"];
    let world = GroupWorld::hosts(&names);
    let seen = Arc::new(Mutex::new(Vec::new()));
    for (rank, name) in names.iter().enumerate() {
        let (owner, seen) = (world.owner(), seen.clone());
        world.host(name).state().launch_observer =
            Some(Arc::new(move |command: &MemberCommand| {
                let o = owner.lock().unwrap();
                let (_, rows) = o
                    .store()
                    .group_plan(&command.identity.deployment_id, 0)
                    .unwrap()
                    .unwrap();
                let row = &rows[rank];
                seen.lock()
                    .unwrap()
                    .push((row.rank, row.state, row.dispatched));
            }));
    }
    let id = world.deploy_group("g", &names).await;
    world.group.launch_completes();
    world.wait_ready(&id).await;
    let mut seen = seen.lock().unwrap().clone();
    seen.sort_by_key(|(rank, ..)| *rank);
    assert_eq!(
        seen,
        vec![
            (0, MemberState::Dispatching, true),
            (1, MemberState::Dispatching, true)
        ]
    );
    let (plan, rows) = world.group_plan("g").unwrap();
    assert!(rows
        .iter()
        .all(|r| r.state == MemberState::Launched && r.dispatched));
    let o = world.owner();
    for (rank, name) in names.iter().enumerate() {
        let host = world.host(name);
        let launch = host
            .received()
            .into_iter()
            .find(|c| matches!(c.action, MemberAction::Launch { .. }))
            .unwrap();
        let reply = host.recorded(&launch.identity.command_id).unwrap();
        let o = o.lock().unwrap();
        // The same identities again are a retry; any others a conflict.
        o.store()
            .mark_member_launched(&id, 0, plan.generation(), rank as u32, &reply)
            .unwrap();
        let mut other = reply.clone();
        other[0].pid += 1000;
        assert!(o
            .store()
            .mark_member_launched(&id, 0, plan.generation(), rank as u32, &other)
            .is_err());
    }
}

// T30, T33 (R23): a Launch whose reply is lost is resent identically (the host
// replays, never spawns twice); its member stays dispatched, charged and
// uncertain while its host is away. When the host comes back with its journal,
// the stop records the identities that journal names, then settles on them.
#[tokio::test]
async fn a_lost_launch_reply_keeps_the_member_charged() {
    let world = GroupWorld::hosts(&["host-a", "host-b"]).launch_replies_lost("host-b");
    // Host B drops off after its second (resent) Launch.
    let (group, seen) = (world.group.clone(), Arc::new(Mutex::new(0)));
    world.host("host-b").state().launch_observer = Some(Arc::new(move |_: &MemberCommand| {
        let mut seen = seen.lock().unwrap();
        *seen += 1;
        if *seen == 2 {
            group.disconnect_host("host-b");
        }
    }));
    let id = world.deploy_group("g", &["host-a", "host-b"]).await;
    world.group.launch_completes();
    assert!(matches!(
        world.wait_activation(&id).await,
        GroupActivation::Failed { failed_rank: 1, .. }
    ));
    let launches: Vec<_> = world
        .host("host-b")
        .received()
        .into_iter()
        .filter(|c| matches!(c.action, MemberAction::Launch { .. }))
        .collect();
    assert!(launches.len() > 1, "the Launch was resent");
    assert!(launches.iter().all(|c| c == &launches[0]), "identically");
    assert_eq!(world.launches(), 2, "a resend starts nothing");
    assert!(!world.route_open("g"));
    // The head settles on its own host's evidence; host B's member stays.
    world
        .until("the head settled", || {
            let (_, rows) = world.group_plan("g").unwrap();
            (rows[0].state == MemberState::Settled).then_some(())
        })
        .await;
    world.settle_for(Duration::from_millis(300)).await;
    let (_, rows) = world.group_plan("g").unwrap();
    assert!(rows[1].dispatched);
    assert_eq!(rows[1].state, MemberState::Uncertain);
    assert_eq!(rows[1].identities, None);
    // A member whose Launch went unanswered did not fail: it is uncertain.
    assert_eq!(
        world.status("g").await.last_error(),
        "group_member_uncertain"
    );
    assert!(world.owner_bytes_on("host-b", &member_owner_id(&id, 0, 1)) > 0);
    assert!(!world.port_free("host-a", 25000));
    world.group.reconnect_host("host-b", JournalState::Kept);
    let status = world.wait_settled_generation("g", 1).await;
    assert_eq!(status.last_error(), "group_member_failed");
    let (_, rows) = world.group_plan("g").unwrap();
    let journaled = world
        .host("host-b")
        .recorded(&launches[0].identity.command_id)
        .unwrap();
    let mut recorded = rows[1].identities.clone().unwrap();
    let mut expected = journaled.clone();
    recorded.sort_by_key(|p| p.pid);
    expected.sort_by_key(|p| p.pid);
    assert_eq!(recorded, expected, "the identities its host journaled");
    assert_eq!(
        world.owner_bytes_on("host-b", &member_owner_id(&id, 0, 1)),
        0
    );
    assert!(world.port_free("host-a", 25000));
    assert!(!world.group.alive(1));
}

// T27, Review Focus 2 (R15): a head rendezvous port held outside CapyCTL is
// excluded, every reservation released, and the retry draws another port.
#[tokio::test]
async fn a_rendezvous_port_held_outside_capyctl_is_redrawn() {
    let world = GroupWorld::hosts(&["host-a", "host-b"]);
    world.host("host-a").hold_port(25000);
    let id = world.deploy_group("g", &["host-a", "host-b"]).await;
    world.group.launch_completes();
    world.wait_ready(&id).await;
    let (plan, _) = world.group_plan("g").unwrap();
    assert_ne!(plan.rendezvous_port(), 25000);
    let prepared: Vec<u16> = world
        .host("host-a")
        .received()
        .into_iter()
        .filter_map(|c| match c.action {
            MemberAction::Prepare(plan) => Some(plan.rendezvous_port()),
            _ => None,
        })
        .collect();
    assert_eq!(prepared, vec![25000, plan.rendezvous_port()]);
    assert_eq!(world.launches(), 2);
}

// ---- Task 17: stop, failure, settlement, uncertainty and recovery ---------

// T31: a worker exit stops the head; each host releases on its own evidence; the port frees.
// The exit reaches the controller as host B's agent reports it (`MemberExit`).
#[tokio::test]
async fn worker_exit_stops_the_group() {
    let world = GroupWorld::ready_group("g", &["host-a", "host-b"]).await;
    let id = world.id("g");
    world.group.exit_rank(1);
    let status = world.wait_settled_generation("g", 1).await;
    assert_eq!(status.last_error(), "group_member_failed");
    assert!(!world.group.alive(0));
    assert_eq!(
        world.owner_bytes_on("host-a", &member_owner_id(&id, 0, 0)),
        0
    );
    assert_eq!(
        world.owner_bytes_on("host-b", &member_owner_id(&id, 0, 1)),
        0
    );
    assert!(world.port_free("host-a", 25000));
    assert!(!world.route_open("g"));
}

// T31: a head exit terminates the worker before any relaunch; under
// `recovery: reconcile` the group relaunches as a new generation only after
// every member of the old plan settled.
#[tokio::test]
async fn head_exit_terminates_worker_before_any_relaunch() {
    let world = GroupWorld::hosts(&["host-a", "host-b"])
        .recovery_reconcile()
        .ready("g")
        .await;
    world.group.exit_rank(0);
    world.wait_settled_generation("g", 1).await;
    assert!(world.generation_started_after_settlement("g", 2).await);
    world.wait_ready(&world.id("g")).await;
    assert!(world.route_open("g"));
}

// T32: an unreachable worker host keeps its share charged and uncertain; the port stays held.
#[tokio::test]
async fn unreachable_host_keeps_charge_and_port() {
    let world = GroupWorld::ready_group("g", &["host-a", "host-b"]).await;
    let id = world.id("g");
    world.group.disconnect_host("host-b");
    world.stop(&id).await;
    assert_eq!(world.status("g").await.member(1).state, "uncertain");
    assert_eq!(
        world.status("g").await.last_error(),
        "group_member_uncertain"
    );
    assert_ne!(
        world.owner_bytes_on("host-b", &member_owner_id(&id, 0, 1)),
        0
    );
    assert_eq!(
        world.owner_bytes_on("host-a", &member_owner_id(&id, 0, 0)),
        0
    );
    assert!(!world.port_free("host-a", 25000));
    world.advance_past_lease_expiry().await;
    assert_ne!(
        world.owner_bytes_on("host-b", &member_owner_id(&id, 0, 1)),
        0
    );
    world.group.reconnect_host("host-b", JournalState::Kept);
    world.wait_settled_generation("g", 1).await;
    assert!(world.port_free("host-a", 25000));
    assert!(!world.group.alive(1));
}

// Review Focus 3: a worker host back with an empty journal settles only on recorded identities.
#[tokio::test]
async fn empty_journal_reconnect_settles_on_recorded_identities_only() {
    let world = GroupWorld::hosts(&["host-a", "host-b"])
        .recovery_reconcile()
        .ready("g")
        .await;
    world.group.disconnect_host("host-b");
    world.group.exit_rank(0);
    world
        .until("the head settled", || {
            let (_, rows) = world.group_plan("g").unwrap();
            (rows[0].state == MemberState::Settled).then_some(())
        })
        .await;
    world.group.reconnect_host("host-b", JournalState::Empty);
    world.settle_for(Duration::from_secs(5)).await;
    assert_eq!(world.status("g").await.member(1).state, "uncertain");
    assert!(world.group.alive(1), "an empty journal signals nothing");
    assert!(!world.generation_started("g", 2));
    world.group.kill_rank_process(1);
    world.wait_settled_generation("g", 1).await;
    assert!(world.generation_started_after_settlement("g", 2).await);
}

// T30: a launch failure on one member after the other launched is compensated.
#[tokio::test]
async fn launch_failure_is_compensated() {
    let world = GroupWorld::hosts(&["host-a", "host-b"]).launch_fails("host-b");
    let id = world.deploy_group("g", &["host-a", "host-b"]).await;
    world.wait_settled_generation("g", 1).await;
    assert!(!world.group.alive(0));
    assert!(!world.route_open("g"));
    assert_eq!(
        world.status_of(&id).await.last_error(),
        "group_member_failed"
    );
    assert_eq!(
        world.owner_bytes_on("host-a", &member_owner_id(&id, 0, 0)),
        0
    );
    assert_eq!(
        world.owner_bytes_on("host-b", &member_owner_id(&id, 0, 1)),
        0
    );
}

// T30, T31 (decided 2026-10-06): a failed readiness probe stops every member; each host releases on its own evidence.
#[tokio::test]
async fn readiness_probe_failure_stops_the_group() {
    let world = GroupWorld::hosts(&["host-a", "host-b"]).probe_fails();
    let id = world.deploy_group("g", &["host-a", "host-b"]).await;
    world.group.launch_completes();
    world.wait_settled_generation("g", 1).await;
    assert!(!world.group.alive(0) && !world.group.alive(1));
    assert_eq!(
        world.owner_bytes_on("host-a", &member_owner_id(&id, 0, 0)),
        0
    );
    assert_eq!(
        world.owner_bytes_on("host-b", &member_owner_id(&id, 0, 1)),
        0
    );
    assert_eq!(
        world.status_of(&id).await.last_error(),
        "group_member_failed"
    );
}

// T20, T30 (ADR 0028 §9): after the head's host session changes, the group head
// is re-proven with the 1-token completion probe its agent answers (never the
// plain readiness probe it refuses), and only then does dispatch reopen.
#[tokio::test]
async fn group_head_is_reproven_by_a_completion_probe_after_a_session_change() {
    struct Sessions {
        transport: Arc<WorldHosts>,
        session: Mutex<String>,
        changes: tokio::sync::watch::Sender<u64>,
    }
    impl crate::remote_readiness::ReadinessHosts for Sessions {
        fn current_session(&self, _: &str) -> Option<String> {
            Some(self.session.lock().unwrap().clone())
        }
        fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
            self.changes.subscribe()
        }
        fn probe(&self, command: MemberCommand) -> crate::remote_readiness::ProbeFuture {
            let transport = self.transport.clone();
            let session = self.session.lock().unwrap().clone();
            Box::pin(async move {
                let host = WorldHosts::host(&transport, &command.identity.member.host_id)
                    .cloned()
                    .ok_or(())?;
                host.execute(command)
                    .await
                    .map(|r| (session, r))
                    .map_err(drop)
            })
        }
    }
    let world = GroupWorld::ready_group("g", &["host-a", "host-b"]).await;
    assert!(world.route_open("g"));
    let (changes, _) = tokio::sync::watch::channel(0);
    let hosts = Arc::new(Sessions {
        transport: world.transport.clone(),
        session: Mutex::new("session-2".into()),
        changes,
    });
    // The readiness was proven on another session than the head's current one.
    let ledger: crate::remote_execution::ReadinessLedger = Default::default();
    let supervisor = crate::remote_readiness::RemoteReadiness::new(
        world.owner(),
        hosts,
        GROUP_CONTROLLER.into(),
        ledger.clone(),
    );
    let pass = supervisor.clone();
    tokio::task::spawn_blocking(move || pass.pass())
        .await
        .unwrap();
    world
        .until("the head re-proven", || {
            (ledger.lock().unwrap().values().any(|s| s == "session-2") && world.route_open("g"))
                .then_some(())
        })
        .await;
    assert_eq!(world.probe_calls(), 2);
    assert_eq!(world.last_probe_max_tokens(), 1);
    assert_eq!(world.probes_sent_to("host-b"), 0);
}

impl GroupWorld {
    /// Only `hosts` are eligible for a start from now on.
    pub(super) fn eligible_hosts(&self, hosts: &[&str]) {
        *self.eligible.lock().unwrap() = Some(hosts.iter().map(|h| (*h).to_owned()).collect());
    }
}

// T31 (ADR 0013 §7, ADR 0028 §11): a failed group relaunches only while every
// member host is eligible; a settled group whose worker host is not stays
// stopped until it is.
#[tokio::test]
async fn a_failed_group_relaunches_only_on_eligible_hosts() {
    let world = GroupWorld::hosts(&["host-a", "host-b"])
        .recovery_reconcile()
        .ready("g")
        .await;
    world.eligible_hosts(&["host-a"]);
    world.group.exit_rank(0);
    world.wait_settled_generation("g", 1).await;
    world.settle_for(Duration::from_secs(1)).await;
    assert!(!world.generation_started("g", 2));
    world.eligible_hosts(&["host-a", "host-b"]);
    assert!(world.generation_started_after_settlement("g", 2).await);
}

// T32 (ADR 0028 §11, §16): a group whose worker's Launch went unanswered did
// not fail at that member; status names the uncertainty while it is charged.
#[tokio::test]
async fn an_unanswered_launch_reads_uncertain_not_failed() {
    let world = GroupWorld::hosts(&["host-a", "host-b"]).launch_replies_lost("host-b");
    let group = world.group.clone();
    world.host("host-b").state().launch_observer = Some(Arc::new(move |_: &MemberCommand| {
        group.disconnect_host("host-b");
    }));
    let id = world.deploy_group("g", &["host-a", "host-b"]).await;
    world.group.launch_completes();
    world.wait_activation(&id).await;
    world
        .until("the uncertain status", || {
            let status = world.read_status(&id)?;
            (status.last_error() == "group_member_uncertain").then_some(())
        })
        .await;
    assert_ne!(
        world.owner_bytes_on("host-b", &member_owner_id(&id, 0, 1)),
        0
    );
}

/// The readiness supervisor's view of the world's hosts on one session.
struct ReadinessSessions {
    transport: Arc<WorldHosts>,
    session: String,
    changes: tokio::sync::watch::Sender<u64>,
}

impl crate::remote_readiness::ReadinessHosts for ReadinessSessions {
    fn current_session(&self, _: &str) -> Option<String> {
        Some(self.session.clone())
    }
    fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }
    fn probe(&self, command: MemberCommand) -> crate::remote_readiness::ProbeFuture {
        let (transport, session) = (self.transport.clone(), self.session.clone());
        Box::pin(async move {
            let host = WorldHosts::host(&transport, &command.identity.member.host_id)
                .cloned()
                .ok_or(())?;
            host.execute(command)
                .await
                .map(|r| (session, r))
                .map_err(drop)
        })
    }
}

impl GroupWorld {
    /// SPEC §13.2 (G2): the readiness supervisor sees the head's host on a
    /// new session `session` and re-proves the head; waits for the route.
    pub(super) async fn reprove_head(&self, session: &str) {
        let (changes, _) = tokio::sync::watch::channel(0);
        let ledger: crate::remote_execution::ReadinessLedger = Default::default();
        let supervisor = crate::remote_readiness::RemoteReadiness::new(
            self.owner(),
            Arc::new(ReadinessSessions {
                transport: self.transport.clone(),
                session: session.into(),
                changes,
            }),
            GROUP_CONTROLLER.into(),
            ledger.clone(),
        );
        self.until("the head re-proven", || {
            let pass = supervisor.clone();
            pass.pass();
            (ledger.lock().unwrap().values().any(|s| s == session) && self.route_open("g"))
                .then_some(())
        })
        .await;
    }
}

// T33 (R38, ADR 0016): a controller restart with a Ready group adopts it as it
// stands; its head is re-proven by the completion probe and it serves again,
// every member still launched and charged, nothing relaunched.
#[tokio::test]
async fn a_restart_keeps_a_ready_group_serving_after_reproof() {
    let world = GroupWorld::ready_group("g", &["host-a", "host-b"]).await;
    let id = world.id("g");
    let world = world.restart().await;
    world.reprove_head("session-after-restart").await;
    assert!(world.group.alive(0) && world.group.alive(1));
    assert_eq!(world.launches(), 2, "nothing relaunched");
    assert_eq!(world.last_probe_max_tokens(), 1);
    let (_, rows) = world.group_plan("g").unwrap();
    assert!(rows.iter().all(|r| r.state == MemberState::Launched));
    assert_ne!(
        world.owner_bytes_on("host-b", &member_owner_id(&id, 0, 1)),
        0
    );
}

// T32, T33 (R38): a restart while a failed group's worker host is away keeps
// that member uncertain and charged, the port held and nothing relaunched;
// when the host comes back with its journal the adopted stop completes on
// its evidence, and only then does the group relaunch.
#[tokio::test]
async fn a_restart_with_a_member_host_away_keeps_it_charged() {
    let world = GroupWorld::hosts(&["host-a", "host-b"])
        .recovery_reconcile()
        .ready("g")
        .await;
    let id = world.id("g");
    world.group.disconnect_host("host-b");
    world.group.exit_rank(0);
    world
        .until("the head settled", || {
            let (_, rows) = world.group_plan("g").unwrap();
            (rows[0].state == MemberState::Settled).then_some(())
        })
        .await;
    let world = world.restart().await;
    world.settle_for(Duration::from_secs(2)).await;
    assert_eq!(world.status("g").await.member(1).state, "uncertain");
    assert_ne!(
        world.owner_bytes_on("host-b", &member_owner_id(&id, 0, 1)),
        0
    );
    assert!(!world.port_free("host-a", 25000));
    assert!(!world.generation_started("g", 2));
    world.group.reconnect_host("host-b", JournalState::Kept);
    world.wait_settled_generation("g", 1).await;
    assert!(world.generation_started_after_settlement("g", 2).await);
}

// T31, T33 (R38): a restart in the middle of an operator's Stop (one member's
// host away) resumes it; the Stop completes on that host's evidence.
#[tokio::test]
async fn a_restart_mid_stop_completes_the_stop_on_evidence() {
    let world = GroupWorld::ready_group("g", &["host-a", "host-b"]).await;
    let id = world.id("g");
    world.group.disconnect_host("host-b");
    world.stop(&id).await;
    assert_eq!(world.status("g").await.member(1).state, "uncertain");
    let world = world.restart().await;
    world.group.reconnect_host("host-b", JournalState::Kept);
    world.wait_settled_generation("g", 1).await;
    assert!(world.port_free("host-a", 25000));
    assert!(!world.group.alive(0) && !world.group.alive(1));
    assert_eq!(
        world.owner_bytes_on("host-b", &member_owner_id(&id, 0, 1)),
        0
    );
    assert!(!world.route_open("g"));
}

// ---- group eviction across named hosts (ADR 0028 §5, SPEC §11) --------------

impl GroupWorld {
    /// A single-rank deployment `name` on `host` alone, not started: the
    /// golden deployment, restart-only, charged 26 GiB at every phase, which
    /// the host's 32 GiB managed limit holds alone but not beside a group
    /// member (8 GiB Ready, 10 GiB cold).
    pub(super) fn with_single_rank(self, name: &str, host: &str) -> Self {
        self.single_rank(name, host, 26, false);
        self
    }

    /// As [`Self::with_single_rank`], charged `gib` GiB at every phase.
    pub(super) fn with_single_rank_of(self, name: &str, host: &str, gib: u32) -> Self {
        self.single_rank(name, host, gib, false);
        self
    }

    /// As [`Self::with_single_rank`], holding a warm-residency commitment
    /// (SPEC §6.5): it is never a switch victim.
    pub(super) fn with_warm_single_rank(self, name: &str, host: &str) -> Self {
        self.single_rank(name, host, 26, true);
        self
    }

    /// Every host's agent fences per instance (SPEC §§3.1, 7.3): a host
    /// running a launch still takes another, judged by memory alone.
    pub(super) fn with_per_instance_claims(self) -> Self {
        let sql = rusqlite::Connection::open(self._dir.path().join("srv.sqlite3")).unwrap();
        for host in &self.hosts {
            sql.execute(
                "INSERT OR REPLACE INTO host_launch_claims(host_id,mode,recorded_at_ms) VALUES(?1,'per_instance',1)",
                [&host.name],
            )
            .unwrap();
        }
        self
    }

    fn single_rank(&self, name: &str, host: &str, gib: u32, warm: bool) {
        let source: serde_json::Value = serde_json::from_str(include_str!(
            "../../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
        ))
        .unwrap();
        let golden = &source["input"]["host"];
        self.enroll(golden);
        let mut deployment = source["input"]["deployment"].clone();
        deployment.as_object_mut().unwrap().remove("host");
        deployment["name"] = serde_json::json!(name);
        deployment["routes"] = serde_json::json!([name]);
        deployment["recovery"] = serde_json::json!(self.recovery);
        deployment["residency"] = serde_json::json!("restart_only");
        for phase in ["cold", "ready", "parking", "wake"] {
            deployment["resources"][phase]["allocations"][0]["bytes"] =
                serde_json::json!(format!("{gib}GiB"));
        }
        if warm {
            deployment["lifecycle"] = serde_json::json!({ "warm": true });
        }
        let on = self.host(host).clone();
        self.create(name, &deployment, golden, &[on.as_ref()])
            .unwrap();
    }

    /// SPEC §10: a request for deployment `name` while nothing serves it, as
    /// the router hands it to the lifecycle authority: it activates one
    /// instance, making room by switching when it fits nowhere.
    pub(super) async fn try_request(&self, name: &str) -> Result<(), crate::fault::LifecycleFault> {
        use crate::port::LifecyclePort;
        let port = crate::coordinator_port::CoordinatorLifecycle::new(self.worker.commands())
            .with_switch_options(crate::switching::SwitchOptions {
                drain_timeout: Duration::from_secs(5),
                poll: Duration::from_millis(10),
                ..Default::default()
            });
        tokio::time::timeout(
            Duration::from_secs(60),
            port.activate_for_request(&self.id(name)),
        )
        .await
        .unwrap_or_else(|_| panic!("the request for {name} never ended"))
    }

    /// As [`Self::try_request`]; panics unless `name` ends up serving.
    pub(super) async fn request(&self, name: &str) {
        self.try_request(name)
            .await
            .unwrap_or_else(|e| panic!("the request for {name} was refused: {e}"));
    }

    /// Deployment `name`'s instance state as status reads it (`ready`,
    /// `stopped`, `parked`, ...).
    pub(super) async fn state(&self, name: &str) -> String {
        let id = self.id(name);
        let o = self.owner.lock().unwrap();
        let snapshot = o.store().snapshot().unwrap();
        snapshot
            .deployments
            .iter()
            .find(|d| d.id == id)
            .and_then(|d| d.instances.first())
            .map(|i| i.observed_state.clone())
            .expect("a deployed instance")
    }

    /// ADR 0028 §11: every settled plan of deployment `name` released each
    /// member on its own host's evidence: that member's own host was sent a
    /// Terminate for the member's Launch, and no other host was.
    pub(super) fn assert_release_evidence_per_member(&self, name: &str) {
        let id = self.id(name);
        let current = {
            let o = self.owner.lock().unwrap();
            o.store()
                .group_plan(&id, 0)
                .unwrap()
                .map_or(0, |(plan, _)| plan.generation())
        };
        for generation in 1..=current {
            let Some((_, members)) = ({
                let o = self.owner.lock().unwrap();
                o.store().group_plan_at(&id, 0, generation).unwrap()
            }) else {
                continue;
            };
            if members.iter().any(|m| m.state != MemberState::Settled) {
                continue;
            }
            for member in &members {
                let handle = member
                    .launch_handle
                    .as_deref()
                    .expect("a released member was launched");
                for host in &self.hosts {
                    let terminated = host.received().iter().any(|c| {
                        c.identity.deployment_id == id
                            && matches!(&c.action, MemberAction::Terminate { owned_handle, .. }
                                if owned_handle == handle)
                    });
                    assert_eq!(
                        terminated,
                        host.name == member.host_id,
                        "rank {} of generation {generation} is released on {}'s evidence only",
                        member.rank,
                        member.host_id
                    );
                }
            }
        }
    }
}

// T16: A -> B -> A with a group and a single-rank deployment on host A.
// R5: the group parks as the victim, which group park brings.
#[tokio::test]
#[ignore = "enabled by Task 19"]
async fn group_and_single_rank_alternate() {
    let world = GroupWorld::ready_group("g", &["host-a", "host-b"])
        .await
        .with_single_rank("s", "host-a");
    world.request("s").await;
    assert_eq!(world.state("g").await, "parked");
    world.request("g").await;
    assert_eq!(world.state("g").await, "ready");
    world.assert_release_evidence_per_member("g");
}

// T16, T27, T30 (ADR 0028 §5, §11): A -> B -> A with group g on host A and
// host B and a single-rank s on host A, which do not fit together there. The
// request for s evicts g whole, by the group stop: each host is sent one
// Terminate for its own member and each member is released only on its own
// host's evidence, on host B too although only host A needed room. The
// request for g then evicts s and the group starts again on both hosts. Run
// with single-claim agents (each launch occupies its host) and with agents
// fencing per instance (memory alone decides).
#[tokio::test]
async fn a_group_victim_is_stopped_whole_and_released_per_member() {
    for per_instance in [false, true] {
        let world = GroupWorld::hosts(&["host-a", "host-b"]);
        let world = if per_instance {
            world.with_per_instance_claims()
        } else {
            world
        };
        let world = world.ready("g").await.with_single_rank("s", "host-a");
        let g = world.id("g");
        world.request("s").await;
        assert_eq!(world.state("s").await, "ready");
        assert_eq!(world.state("g").await, "stopped");
        world.wait_settled_generation("g", 1).await;
        world.assert_release_evidence_per_member("g");
        assert!(!world.group.alive(0) && !world.group.alive(1));
        assert_eq!(
            world.owner_bytes_on("host-a", &member_owner_id(&g, 0, 0)),
            0
        );
        assert_eq!(
            world.owner_bytes_on("host-b", &member_owner_id(&g, 0, 1)),
            0
        );
        assert!(world.port_free("host-a", 25000));

        world.request("g").await;
        assert_eq!(world.state("g").await, "ready");
        assert_eq!(world.state("s").await, "stopped");
        let (plan, members) = world.group_plan("g").unwrap();
        assert!(plan.generation() > 1, "the group started again");
        assert!(members.iter().all(|m| m.state == MemberState::Launched));
        assert!(world.route_open("g"));
    }
}

// T16, T27 (ADR 0028 §5, SPEC §11): a group request needing room on host A
// and host B evicts on both or neither. Host A could make room by stopping
// s, but host B holds only t, whose warm-residency commitment keeps it from
// being a victim: the request is refused for capacity and s keeps serving.
#[tokio::test]
async fn a_group_request_evicts_nothing_unless_every_named_host_makes_room() {
    let world = GroupWorld::hosts(&["host-a", "host-b"])
        .with_per_instance_claims()
        .ready("g")
        .await
        .with_single_rank("s", "host-a")
        .with_warm_single_rank("t", "host-b");
    world.request("s").await;
    world.request("t").await;
    assert_eq!(world.state("g").await, "stopped");
    let refused = world.try_request("g").await.unwrap_err();
    assert!(
        refused.to_string().contains("insufficient_capacity"),
        "{refused}"
    );
    assert_eq!(world.state("s").await, "ready");
    assert_eq!(world.state("t").await, "ready");
    assert_eq!(world.state("g").await, "stopped");
    assert!(world
        .group_plan("g")
        .is_some_and(|(plan, _)| plan.generation() == 1));
}

impl GroupWorld {
    /// Owner decision 2026-09-25 (`start deployment --evict`): make room for
    /// every instance of deployment `name`; the host of each switch room,
    /// released (and its turn ended) before this returns.
    pub(super) async fn evicting_start(&self, name: &str) -> Vec<String> {
        let port = crate::coordinator_port::CoordinatorLifecycle::new(self.worker.commands())
            .with_switch_options(crate::switching::SwitchOptions {
                drain_timeout: Duration::from_secs(5),
                poll: Duration::from_millis(10),
                ..Default::default()
            });
        let rooms = tokio::time::timeout(
            Duration::from_secs(60),
            port.switcher().make_room_for_start(&self.id(name)),
        )
        .await
        .unwrap_or_else(|_| panic!("the evicting start of {name} never ended"))
        .unwrap_or_else(|e| panic!("the evicting start of {name} was refused: {e}"));
        rooms.iter().map(|room| room.host.clone()).collect()
    }
}

// T16, T27 (ADR 0028 §5, owner decision 2026-09-25): an evicting start of a
// group plans every named host before releasing anything, then releases each
// host's victims in its own switch: s on host A and u on host B.
#[tokio::test]
async fn an_evicting_start_of_a_group_releases_on_every_named_host() {
    let world = GroupWorld::hosts(&["host-a", "host-b"])
        .with_per_instance_claims()
        .ready("g")
        .await
        .with_single_rank("s", "host-a")
        .with_single_rank("u", "host-b");
    world.request("s").await;
    world.request("u").await;
    assert_eq!(world.state("g").await, "stopped");
    assert_eq!(world.evicting_start("g").await, ["host-a", "host-b"]);
    assert_eq!(world.state("s").await, "stopped");
    assert_eq!(world.state("u").await, "stopped");
    world.request("g").await;
    assert_eq!(world.state("g").await, "ready");
}

impl GroupWorld {
    /// An on-demand start of deployment `name`, placed without eviction
    /// (owner decision Q5); the refusal when no allowed host takes it.
    pub(super) fn start_without_eviction(&self, name: &str) -> Result<(), String> {
        let fence = self.deployed.lock().unwrap()[name].clone();
        self.worker
            .commands()
            .start_on_demand(
                "owner",
                &fence.deployment_id,
                fence.revision,
                &format!("start:{name}"),
                capyctl_protocol::now_unix_ms() + 60_000,
            )
            .map(drop)
            .map_err(|e| format!("{e:?}"))
    }
}

// T16, T30 (ADR 0028 §5, §11; SPEC §§3.1, 7.3): host B runs only g's worker
// member. A single-claim agent there takes no other launch: s (10 GiB, which
// host B's memory holds beside the member) is not placed there without
// eviction, and a request for s evicts g whole, each member released on its
// own host's evidence. An agent fencing per instance takes s beside the
// member, by memory alone, and g keeps serving.
#[tokio::test]
async fn a_host_running_only_a_group_worker_member_is_occupied() {
    for per_instance in [false, true] {
        let world = GroupWorld::hosts(&["host-a", "host-b"]);
        let world = if per_instance {
            world.with_per_instance_claims()
        } else {
            world
        };
        let world = world
            .ready("g")
            .await
            .with_single_rank_of("s", "host-b", 10);
        let g = world.id("g");
        if per_instance {
            world.start_without_eviction("s").unwrap();
            world.request("s").await;
            assert_eq!(world.state("s").await, "ready");
            assert_eq!(world.state("g").await, "ready");
            assert!(world.route_open("g"));
            continue;
        }
        let refused = world.start_without_eviction("s").unwrap_err();
        assert!(refused.contains("host host-b: host_occupied"), "{refused}");
        world.request("s").await;
        assert_eq!(world.state("s").await, "ready");
        assert_eq!(world.state("g").await, "stopped");
        world.wait_settled_generation("g", 1).await;
        world.assert_release_evidence_per_member("g");
        assert_eq!(
            world.owner_bytes_on("host-a", &member_owner_id(&g, 0, 0)),
            0
        );
        assert_eq!(
            world.owner_bytes_on("host-b", &member_owner_id(&g, 0, 1)),
            0
        );
    }
}

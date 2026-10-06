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
    /// The agent's authorization refused it (a wrong expected state, another
    /// owner, or a group Park, Restore or Probe, which the agent admits for
    /// single-rank launches only).
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

struct HostState {
    capabilities: BTreeSet<String>,
    /// Profile name to its recorded build.
    profiles: BTreeMap<String, String>,
    /// Model path to the digest this host measured.
    digests: BTreeMap<String, String>,
    /// Ports something outside CapyCTL holds on this host.
    held_ports: BTreeSet<u16>,
    journal: BTreeMap<String, Recorded>,
    spawned: Vec<Spawned>,
    received: Vec<MemberCommand>,
    next_pid: u32,
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
                held_ports: BTreeSet::new(),
                journal: BTreeMap::new(),
                spawned: Vec::new(),
                received: Vec::new(),
                next_pid: 4000,
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

    /// Answer one command as the member's agent does.
    pub(super) async fn execute(
        &self,
        command: MemberCommand,
    ) -> Result<pb::MemberExecutionResult, HostError> {
        if !self.group.connected(&self.name) {
            return Err(HostError::Unreachable);
        }
        // Review Focus 3: a restarted agent with an empty journal knows no
        // launch; the processes it started run on.
        if self.group.take_journal_loss(&self.name) {
            self.state().journal.clear();
        }
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
        let result = match &decoded.action {
            MemberAction::Prepare(plan) => self.prepare(&decoded, plan),
            MemberAction::Launch { plan, member } => self.launch(&decoded, plan, member).await?,
            MemberAction::Terminate {
                owned_handle,
                recorded,
            } => self.terminate(&decoded, owned_handle, recorded)?,
            // ADR 0028 §9, §12: the agent admits Park, Restore and Probe for
            // single-rank launches only, so far.
            MemberAction::Park { .. }
            | MemberAction::Restore { .. }
            | MemberAction::Probe { .. } => return Err(HostError::Unauthorized),
            MemberAction::LaunchSingle(_) => return Err(HostError::NotScripted("LaunchSingle")),
            MemberAction::Inspect => return Err(HostError::NotScripted("Inspect")),
            MemberAction::CloseIngress => return Err(HostError::NotScripted("CloseIngress")),
            MemberAction::DigestCheckpoint(_) => {
                return Err(HostError::NotScripted("DigestCheckpoint"))
            }
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
        pb::MemberExecutionResult {
            refused: self.check(plan).err().unwrap_or_default(),
            ..Self::reply(command)
        }
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

/// The coordinator and its store, with a scripted agent per member host,
/// every host running its rank of one [`FakeGroup`].
pub(super) struct GroupWorld {
    pub(super) group: FakeGroup,
    hosts: Vec<Arc<GroupHost>>,
    owner: SharedCoordinatorState,
    worker: OwnedCoordinator,
    _dir: tempfile::TempDir,
}

impl GroupWorld {
    /// A world of `names`, which are the group's hosts in rank order: the
    /// first heads it. Each host has its own documentation peer address.
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
        let owner = Arc::new(Mutex::new(
            crate::ownership::OwnedCoordinatorState::open(dir.path()).unwrap(),
        ));
        let worker = OwnedCoordinator::spawn_with_execution_bindings(
            owner.clone(),
            Arc::new(Observations(fixture.observations.clone())),
            Arc::new(|| Ok(1900)),
            CoordinatorOptions {
                retry_cooldown: Duration::from_millis(50),
                ..Default::default()
            },
            // ADR 0028 §5: a group never launches as a single-rank engine, so
            // this world resolves no single-host binding for it.
            Arc::new(|_: &InitializeWork| {
                Err(CoordinatorError::Service(
                    "a group world has no single-host engine binding".into(),
                ))
            }),
        )
        .unwrap();
        Self {
            group,
            hosts,
            owner,
            worker,
            _dir: dir,
        }
    }

    /// The scripted agent of `name`.
    pub(super) fn host(&self, name: &str) -> &Arc<GroupHost> {
        self.hosts
            .iter()
            .find(|h| h.name == name)
            .unwrap_or_else(|| panic!("{name} is not a host of this world"))
    }

    /// Deploy a TP N group named `name` over `hosts` (every host of the world,
    /// in rank order) and start it. Returns the deployment id.
    // The core helper the group activation tests deploy with; nothing in
    // this module calls it yet.
    #[allow(dead_code)]
    pub(super) async fn deploy_group(&self, name: &str, hosts: &[&str]) -> String {
        let names: Vec<&str> = self.hosts.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(
            hosts,
            names.as_slice(),
            "one group over the world's hosts in rank order"
        );
        self.deploy(name, hosts.len() as u32, 1)
    }

    /// Deploy a TP `tp` × PP `pp` group named `name` over every host of the
    /// world, one rank per host, and start it. Returns the deployment id.
    pub(super) async fn deploy_group_shape(&self, name: &str, tp: u32, pp: u32) -> String {
        assert_eq!(
            tp * pp,
            self.hosts.len() as u32,
            "one rank per host (ADR 0028 §2)"
        );
        self.deploy(name, tp, pp)
    }

    /// The group deployment `name`, accepted on every member host from that
    /// host's own document, then started through the coordinator.
    fn deploy(&self, name: &str, tp: u32, pp: u32) -> String {
        let fence = {
            let o = self.owner.lock().unwrap();
            let source: serde_json::Value = serde_json::from_str(include_str!(
                "../../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
            ))
            .unwrap();
            let names: Vec<&str> = self.hosts.iter().map(|h| h.name.as_str()).collect();
            let deployment = group_document(&source["input"]["deployment"], name, &names, tp, pp);
            let targets: Vec<_> = self
                .hosts
                .iter()
                .map(|host| capyctl_store::managed_configuration::HostTarget {
                    host_id: host.name.clone(),
                    host_name: host.name.clone(),
                    trusted_host: host_document(&source["input"]["host"], host),
                    scoped: false,
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
                    1700,
                )
                .unwrap();
            DeploymentFence {
                deployment_id: receipt.deployment_id,
                revision: receipt.revision,
                generation: receipt.generation,
            }
        };
        // The start runs on in the coordinator; `wait_ready` observes it.
        drop(self.worker.start(&fence, 60_000).unwrap());
        fence.deployment_id
    }

    /// Wait until deployment `id` is observed ready; panics after 30 s.
    pub(super) async fn wait_ready(&self, id: &str) {
        let ready = || {
            let o = self.owner.lock().unwrap();
            o.store()
                .get_deployment(id)
                .unwrap()
                .is_some_and(|d| d.observed_state == capyctl_domain::LifecycleState::Ready)
        };
        tokio::time::timeout(Duration::from_secs(30), async {
            while !ready() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("deployment {id} never became ready"));
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

/// The golden host document as `host` publishes it: its name and its peer
/// address (ADR 0028 §3).
fn host_document(golden: &serde_json::Value, host: &GroupHost) -> serde_json::Value {
    let mut document = golden.clone();
    document["name"] = serde_json::json!(host.name);
    document["resource_policy"]["groups"] =
        serde_json::json!({ "peer_address": host.peer.to_string() });
    document
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
        controller_id: "controller".into(),
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
#[tokio::test]
#[ignore = "enabled by Task 16"]
async fn group_world_four_hosts_smoke() {
    let world = GroupWorld::hosts(&["h0", "h1", "h2", "h3"]);
    let id = world.deploy_group_shape("g", 2, 2).await;
    world.group.launch_completes();
    world.wait_ready(&id).await;
    assert_eq!(world.launches(), 4);
}

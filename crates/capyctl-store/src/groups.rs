//! ADR 0028 §4, §5, §11: multi-node group plans in the durable store.
//!
//! A group instance is charged as one resource owner per member,
//! `deployment:<id>/instance:<k>/member:<r>`, each on its own host's domains
//! and judged against its own host's admission context. Every member is
//! reserved in one store transaction together with the group plan, the
//! rendezvous port drawn from the head's range and any SGLang worker loopback
//! port leased on its own host. A failure anywhere rolls the whole transaction
//! back, so no host's share is held for a group that did not fit elsewhere.
//! This is atomic accounting in the server's store, not an atomic launch
//! across hosts.
//!
//! Each member settles only on gone evidence from its own host. A member is
//! fenced as dispatched before its Launch is sent; from then on it settles only
//! on recorded identities, never on empty evidence. A member whose host cannot
//! be reached is marked uncertain and keeps its charge. The rendezvous port and
//! worker leases are freed only when every member has settled.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::ops::RangeInclusive;

use capyctl_domain::completion::ProcessIdentity;
use capyctl_domain::group::{
    member_id, validate_local_processes, GroupEngine, GroupIdentityError, GroupPlan, GroupTopology,
    MemberKey, MemberPlan, MemberRole,
};
use capyctl_scheduler::residency::AdmissionContext;
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};

use crate::resource_ledger::{registered_host, GrantRequest, ResourceStoreError};

/// ADR 0028 §5: the resource owner of one member of a group instance. It never
/// equals an instance owner ([`crate::instances::instance_owner_id`]), so a
/// member is charged and released on its own evidence only.
pub fn member_owner_id(deployment_id: &str, instance_index: u32, rank: u32) -> String {
    format!("deployment:{deployment_id}/instance:{instance_index}/member:{rank}")
}

/// The inverse of [`member_owner_id`]: `(deployment, instance, rank)`, or `None`
/// for any owner id not in the member form (instance owners included).
pub fn parse_member_owner_id(owner_id: &str) -> Option<(String, u32, u32)> {
    fn canonical(text: &str) -> Option<u32> {
        text.parse::<u32>().ok().filter(|n| text == n.to_string())
    }
    let rest = owner_id.strip_prefix("deployment:")?;
    let (rest, rank) = rest.rsplit_once("/member:")?;
    let (deployment, instance) = rest.rsplit_once("/instance:")?;
    if deployment.is_empty() {
        return None;
    }
    Some((
        deployment.to_owned(),
        canonical(instance)?,
        canonical(rank)?,
    ))
}

/// ADR 0028 §5: what a group reservation charges and draws. `members` is in
/// rank order, head first, each with its host and the grant charged there.
/// Every key a grant charges must be registered to its member's host. Every
/// grant names the same revision and generation, and the ledger epoch the
/// caller observed before reserving; the store chains the epoch across
/// members inside the one transaction. `worker_ports` maps each SGLang worker
/// host to its `endpoint_port_range` and is empty for other engines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupReservation {
    pub deployment_id: String,
    pub instance_index: u32,
    pub members: Vec<(String, GrantRequest)>,
    pub head_host: String,
    pub port_range: RangeInclusive<u16>,
    pub worker_ports: BTreeMap<String, RangeInclusive<u16>>,
}

#[derive(Debug, thiserror::Error)]
pub enum GroupStoreError {
    /// `rendezvous_ports_exhausted`, or a worker's endpoint range is full.
    #[error("rendezvous_ports_exhausted: no free port in the range")]
    PortsExhausted,
    /// A member did not fit, or the ledger refused it; typed so a planner can
    /// tell "does not fit" from other refusals.
    #[error(transparent)]
    Admission(ResourceStoreError),
    #[error("the group plan is invalid or does not match the reservation")]
    Plan,
    #[error("group conflict: the request clashes with stored group state")]
    Conflict,
}

impl From<rusqlite::Error> for GroupStoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Admission(ResourceStoreError::Sql(error))
    }
}

fn corrupt() -> GroupStoreError {
    GroupStoreError::Admission(ResourceStoreError::Invalid)
}

/// What settling one member left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupSettlement {
    /// These ranks are still reserved, dispatching, launched or uncertain.
    Partial { unsettled: Vec<u32> },
    /// Every member settled; the rendezvous port and worker leases are free.
    Complete,
}

/// ADR 0028 §8, §11: where a member is between reservation and settlement.
/// `Reserved` was never dispatched; `Dispatching` was fenced as dispatched
/// before its Launch was sent and has no identities recorded yet; `Launched`
/// has its identities recorded; `Uncertain` lost its host and keeps its
/// charge, whether it was dispatched or not ([`MemberRow::dispatched`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberState {
    Reserved,
    Dispatching,
    Launched,
    Settled,
    Uncertain,
}

impl MemberState {
    fn parse(value: &str) -> Result<Self, GroupStoreError> {
        Ok(match value {
            "reserved" => Self::Reserved,
            "dispatching" => Self::Dispatching,
            "launched" => Self::Launched,
            "settled" => Self::Settled,
            "uncertain" => Self::Uncertain,
            _ => return Err(corrupt()),
        })
    }

    /// The stored name, which status reports (ADR 0028 §15).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Dispatching => "dispatching",
            Self::Launched => "launched",
            Self::Settled => "settled",
            Self::Uncertain => "uncertain",
        }
    }
}

/// One stored member of a group plan. `dispatched` records durably that its
/// Launch may have been sent; it survives `Uncertain` and is never cleared.
/// `launch_handle` is the command id that Launch carries (its owned handle on
/// the member's host), recorded with the dispatch fence; `identities` are the
/// processes recorded for it, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberRow {
    pub rank: u32,
    pub host_id: String,
    pub owner_id: String,
    pub state: MemberState,
    pub dispatched: bool,
    pub launch_handle: Option<String>,
    pub identities: Option<Vec<ProcessIdentity>>,
}

/// SPEC §11, ADR 0028 §11: gone evidence for one member, reported by that
/// member's own host. `identities` are the processes it proved gone; a member
/// with identities recorded settles only when they are exactly those, a
/// member never dispatched only with none, and a member dispatched without
/// identities recorded not at all until they are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberGone {
    pub member: MemberKey,
    pub identities: Vec<ProcessIdentity>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredMember {
    host_id: String,
    profile_name: String,
    profile_fingerprint: String,
    checkpoint_fingerprint: String,
    model_path: String,
    devices: Vec<String>,
    peer_address: IpAddr,
    service_port: Option<u16>,
    worker_port: Option<u16>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPlan {
    version: u32,
    engine: String,
    tensor_parallel: u32,
    pipeline_parallel: u32,
    local_ranks: u32,
    rendezvous_port: u16,
    generation: i64,
    members: Vec<StoredMember>,
}

fn encode_plan(plan: &GroupPlan) -> Result<String, GroupStoreError> {
    let topology = plan.topology();
    serde_json::to_string(&StoredPlan {
        version: 1,
        engine: plan.engine().as_str().into(),
        tensor_parallel: topology.tensor_parallel,
        pipeline_parallel: topology.pipeline_parallel,
        local_ranks: topology.local_ranks,
        rendezvous_port: plan.rendezvous_port(),
        generation: plan.generation(),
        members: plan
            .members()
            .iter()
            .map(|m| StoredMember {
                host_id: m.member.host_id.clone(),
                profile_name: m.profile_name.clone(),
                profile_fingerprint: m.profile_fingerprint.clone(),
                checkpoint_fingerprint: m.checkpoint_fingerprint.clone(),
                model_path: m.model_path.clone(),
                devices: m.devices.clone(),
                peer_address: m.peer_address,
                service_port: m.service_port,
                worker_port: m.worker_port,
            })
            .collect(),
    })
    .map_err(|e| GroupStoreError::Admission(e.into()))
}

/// Decoded plans are validated again by [`GroupPlan::new`].
fn decode_plan(json: &str) -> Result<GroupPlan, GroupStoreError> {
    let stored: StoredPlan = serde_json::from_str(json).map_err(|_| corrupt())?;
    if stored.version != 1 {
        return Err(corrupt());
    }
    let engine = match stored.engine.as_str() {
        "vllm" => GroupEngine::Vllm,
        "sglang" => GroupEngine::Sglang,
        "tensorfold" => GroupEngine::Tensorfold,
        _ => return Err(corrupt()),
    };
    let members = stored
        .members
        .into_iter()
        .enumerate()
        .map(|(rank, m)| MemberPlan {
            member: MemberKey {
                host_id: m.host_id,
                member_id: member_id(rank as u32),
            },
            rank: rank as u32,
            role: if rank == 0 {
                MemberRole::Head
            } else {
                MemberRole::Worker
            },
            profile_name: m.profile_name,
            profile_fingerprint: m.profile_fingerprint,
            checkpoint_fingerprint: m.checkpoint_fingerprint,
            model_path: m.model_path,
            devices: m.devices,
            peer_address: m.peer_address,
            service_port: m.service_port,
            worker_port: m.worker_port,
        })
        .collect();
    GroupPlan::new(
        engine,
        members,
        GroupTopology {
            tensor_parallel: stored.tensor_parallel,
            pipeline_parallel: stored.pipeline_parallel,
            local_ranks: stored.local_ranks,
        },
        stored.rendezvous_port,
        stored.generation,
    )
    .map_err(|_| corrupt())
}

/// The canonical stored form of a member's identities: sorted, so evidence
/// in any order compares equal.
fn canonical_identities(identities: &[ProcessIdentity]) -> String {
    let mut rows: Vec<(&str, u32, &str, u64)> = identities
        .iter()
        .map(|p| (p.role.as_str(), p.pid, p.boot_id.as_str(), p.start_ticks))
        .collect();
    rows.sort();
    serde_json::to_string(&rows).unwrap_or_default()
}

/// The stored form back to identities.
pub(crate) fn decode_identities(json: &str) -> Result<Vec<ProcessIdentity>, GroupStoreError> {
    let rows: Vec<(String, u32, String, u64)> =
        serde_json::from_str(json).map_err(|_| corrupt())?;
    Ok(rows
        .into_iter()
        .map(|(role, pid, boot_id, start_ticks)| ProcessIdentity {
            role,
            pid,
            boot_id,
            start_ticks,
        })
        .collect())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

/// The loopback host every endpoint lease names (SPEC §3).
const LOOPBACK: &str = "127.0.0.1";

/// ADR 0028 §13: whether an unsettled group plan holds `port` on `host` as its
/// rendezvous port. The torch store binds it on every interface, so no endpoint
/// lease on that host may take it, loopback included.
pub(crate) fn rendezvous_port_held(
    tx: &Transaction<'_>,
    host: &str,
    port: u16,
) -> rusqlite::Result<bool> {
    tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM group_plans WHERE rendezvous_host=?1 AND rendezvous_port=?2 AND state!='settled')",
        params![host, port],
        |r| r.get(0),
    )
}

/// ADR 0028 §5 (R15), SPEC §3: a rendezvous port is free on `head` when no
/// unsettled plan holds it, no unexpired exclusion names it, and no endpoint
/// lease on that host holds it under any address (a host's rendezvous range may
/// overlap its endpoint range).
fn rendezvous_port_free(
    tx: &Transaction<'_>,
    head: &str,
    port: u16,
    now: i64,
) -> Result<bool, GroupStoreError> {
    Ok(!rendezvous_port_held(tx, head, port)?
        && tx.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM group_port_exclusions WHERE host_id=?1 AND port=?2 AND until_ms>?3)
                AND NOT EXISTS(SELECT 1 FROM endpoint_leases WHERE host_id=?1 AND port=?2)",
            params![head, port, now],
            |r| r.get(0),
        )?)
}

fn endpoint_port_free(
    tx: &Transaction<'_>,
    host: &str,
    port: u16,
) -> Result<bool, GroupStoreError> {
    Ok(tx.query_row(
        "SELECT NOT EXISTS(SELECT 1 FROM endpoint_leases WHERE host_id=?1 AND host=?2 AND port=?3)",
        params![host, LOOPBACK, port],
        |r| r.get(0),
    )?)
}

/// SPEC §7: the namespace of the host a member names, by its id or, for the
/// embedded host, by its configured name. None, or more than one, fails closed.
fn member_namespace(tx: &Transaction<'_>, host: &str) -> Result<String, GroupStoreError> {
    let found: Vec<String> = tx
        .prepare("SELECT host_id FROM host_resource_namespaces WHERE host_id=?1 OR policy_key=?1")?
        .query_map([host], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    match found.as_slice() {
        [namespace] => Ok(namespace.clone()),
        _ => Err(GroupStoreError::Plan),
    }
}

fn has_column(tx: &Transaction<'_>, table: &str, column: &str) -> rusqlite::Result<bool> {
    tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name=?2)",
        params![table, column],
        |r| r.get(0),
    )
}

/// Schema v41 data step (ADR 0028 §5, §6). Idempotent: each step checks what is
/// already there, so a store rolled back to an earlier version reapplies it.
pub(crate) fn migrate_v41(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    // ADR 0028 §5: one owner per member. An instance keeps at most one owner of
    // the instance form; member owners carry their rank.
    if !has_column(tx, "resource_owners", "member_rank")? {
        // A store that never ran the v22 data step names instance 0 by the
        // deployment id alone.
        let deployment = if has_column(tx, "resource_owners", "deployment_id")? {
            "deployment_id"
        } else {
            "owner_id"
        };
        let instance = if has_column(tx, "resource_owners", "instance_index")? {
            "instance_index"
        } else {
            "0"
        };
        tx.execute_batch(&format!(
            "CREATE TABLE resource_owners_v41(
               owner_id TEXT PRIMARY KEY,
               footprint_json TEXT NOT NULL,
               deployment_id TEXT NOT NULL REFERENCES deployments(id),
               instance_index INTEGER NOT NULL DEFAULT 0 CHECK(instance_index>=0),
               member_rank INTEGER CHECK(member_rank IS NULL OR member_rank>=0),
               CHECK((member_rank IS NULL
                      AND ((instance_index=0 AND owner_id=deployment_id)
                        OR (instance_index>0 AND owner_id='deployment:'||deployment_id||'/instance:'||instance_index)))
                  OR (member_rank IS NOT NULL
                      AND owner_id='deployment:'||deployment_id||'/instance:'||instance_index||'/member:'||member_rank)));
             INSERT INTO resource_owners_v41(owner_id,footprint_json,deployment_id,instance_index,member_rank)
               SELECT owner_id,footprint_json,{deployment},{instance},NULL FROM resource_owners;
             DROP TABLE resource_owners;
             ALTER TABLE resource_owners_v41 RENAME TO resource_owners;
             CREATE UNIQUE INDEX IF NOT EXISTS one_instance_owner
               ON resource_owners(deployment_id,instance_index) WHERE member_rank IS NULL;"
        ))?;
    }
    // ADR 0028 §5: a SGLang worker's loopback port is leased in the existing
    // endpoint lease table on its own host, held by its member owner rather
    // than a runtime binding.
    if !has_column(tx, "endpoint_leases", "group_owner")? {
        tx.execute_batch(
            "CREATE TABLE endpoint_leases_v41(
               host_id TEXT NOT NULL,
               host TEXT NOT NULL,
               port INTEGER NOT NULL CHECK(port BETWEEN 1 AND 65535),
               binding_id TEXT REFERENCES runtime_bindings(id),
               group_owner TEXT,
               PRIMARY KEY(host_id,host,port),
               CHECK((binding_id IS NULL) <> (group_owner IS NULL)));
             INSERT INTO endpoint_leases_v41(host_id,host,port,binding_id,group_owner)
               SELECT host_id,host,port,binding_id,NULL FROM endpoint_leases;
             DROP TABLE endpoint_leases;
             ALTER TABLE endpoint_leases_v41 RENAME TO endpoint_leases;
             CREATE INDEX IF NOT EXISTS endpoint_leases_binding ON endpoint_leases(binding_id);
             CREATE INDEX IF NOT EXISTS endpoint_leases_group ON endpoint_leases(group_owner)
               WHERE group_owner IS NOT NULL;",
        )?;
    }
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS group_plans(
           deployment_id TEXT NOT NULL REFERENCES deployments(id),
           instance_index INTEGER NOT NULL CHECK(instance_index BETWEEN 0 AND 63),
           generation INTEGER NOT NULL CHECK(generation>0),
           plan_json TEXT NOT NULL,
           rendezvous_host TEXT NOT NULL CHECK(length(rendezvous_host)>0),
           rendezvous_port INTEGER NOT NULL CHECK(rendezvous_port BETWEEN 1 AND 65535),
           state TEXT NOT NULL CHECK(state IN ('active','settled')),
           PRIMARY KEY(deployment_id,instance_index,generation));
         CREATE UNIQUE INDEX IF NOT EXISTS one_unsettled_group_plan
           ON group_plans(deployment_id,instance_index) WHERE state!='settled';
         CREATE UNIQUE INDEX IF NOT EXISTS one_held_rendezvous_port
           ON group_plans(rendezvous_host,rendezvous_port) WHERE state!='settled';
         CREATE TABLE IF NOT EXISTS group_members(
           deployment_id TEXT NOT NULL,
           instance_index INTEGER NOT NULL,
           generation INTEGER NOT NULL,
           rank INTEGER NOT NULL CHECK(rank>=0),
           host_id TEXT NOT NULL CHECK(length(host_id)>0),
           owner_id TEXT NOT NULL
             CHECK(owner_id='deployment:'||deployment_id||'/instance:'||instance_index||'/member:'||rank),
           state TEXT NOT NULL
             CHECK(state IN ('reserved','dispatching','launched','settled','uncertain')),
           dispatched INTEGER NOT NULL DEFAULT 0 CHECK(dispatched IN (0,1)),
           identities_json TEXT,
           CHECK(state!='reserved' OR (dispatched=0 AND identities_json IS NULL)),
           CHECK(state!='dispatching' OR (dispatched=1 AND identities_json IS NULL)),
           CHECK(state!='launched' OR (dispatched=1 AND identities_json IS NOT NULL)),
           CHECK(identities_json IS NULL OR dispatched=1),
           PRIMARY KEY(deployment_id,instance_index,generation,rank),
           UNIQUE(deployment_id,instance_index,generation,host_id),
           FOREIGN KEY(deployment_id,instance_index,generation)
             REFERENCES group_plans(deployment_id,instance_index,generation));
         CREATE TABLE IF NOT EXISTS group_port_exclusions(
           host_id TEXT NOT NULL CHECK(length(host_id)>0),
           port INTEGER NOT NULL CHECK(port BETWEEN 1 AND 65535),
           until_ms INTEGER NOT NULL,
           PRIMARY KEY(host_id,port));
         CREATE TABLE IF NOT EXISTS checkpoint_host_digests(
           deployment_id TEXT NOT NULL REFERENCES deployments(id),
           revision INTEGER NOT NULL CHECK(revision>0),
           host_id TEXT NOT NULL CHECK(length(host_id)>0),
           digest TEXT NOT NULL,
           recorded_at_ms INTEGER NOT NULL CHECK(recorded_at_ms>=0),
           PRIMARY KEY(deployment_id,revision,host_id));
         INSERT OR IGNORE INTO checkpoint_host_digests(deployment_id,revision,host_id,digest,recorded_at_ms)
           SELECT deployment_id,revision,host_id,digest,updated_at_ms FROM checkpoint_digests
            WHERE digest IS NOT NULL AND host_id!='';",
    )?;
    Ok(())
}

/// Schema v42 data step (ADR 0028 §8, §11). Idempotent: each column is added
/// only when missing.
pub(crate) fn migrate_v42(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    if !has_column(tx, "group_members", "launch_handle")? {
        tx.execute_batch("ALTER TABLE group_members ADD COLUMN launch_handle TEXT;")?;
    }
    if !has_column(tx, "group_plans", "failed_rank")? {
        tx.execute_batch(
            "ALTER TABLE group_plans ADD COLUMN failed_rank INTEGER
               CHECK(failed_rank IS NULL OR failed_rank>=0);",
        )?;
    }
    Ok(())
}

/// Schema v43 data step (ADR 0028 §12). Idempotent: each column is added only
/// when missing. `canary_json` is the wake canary's reference recorded at the
/// plan's first readiness; `failure_code` names why a failed group failed
/// when it is not a member's own failure (a wake whose canary differed, or a
/// stalled request whose head probe failed, ADR 0028 §11).
pub(crate) fn migrate_v43(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    if !has_column(tx, "group_plans", "canary_json")? {
        tx.execute_batch("ALTER TABLE group_plans ADD COLUMN canary_json TEXT;")?;
    }
    if !has_column(tx, "group_plans", "failure_code")? {
        tx.execute_batch(
            "ALTER TABLE group_plans ADD COLUMN failure_code TEXT
               CHECK(failure_code IS NULL OR failure_code IN ('group_member_failed','group_wake_mismatch','group_stalled'));",
        )?;
    }
    Ok(())
}

/// ADR 0028 §12 (decided 2026-10-06): the wake canary's reference, recorded
/// at the plan's first readiness: the fixed prompt and the tokens the head
/// generated for it at temperature 0.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredCanary {
    pub prompt: String,
    pub tokens: Vec<u32>,
}

/// The closed codes a group failure may name in status (spec §16).
const FAILURE_CODES: [&str; 3] = [
    "group_member_failed",
    "group_wake_mismatch",
    "group_stalled",
];

/// ADR 0028 §9, §11: a group head's binding with the plan it realizes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundGroup {
    pub deployment_id: String,
    pub instance_index: u32,
    pub plan: GroupPlan,
    pub members: Vec<MemberRow>,
}

/// The plan of an instance at `generation` with its members in rank order.
pub(crate) fn plan_at(
    tx: &Transaction<'_>,
    deployment_id: &str,
    instance_index: u32,
    generation: i64,
) -> Result<Option<(GroupPlan, Vec<MemberRow>)>, GroupStoreError> {
    let json: Option<String> = tx
        .query_row(
            "SELECT plan_json FROM group_plans
              WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3",
            params![deployment_id, instance_index, generation],
            |r| r.get(0),
        )
        .optional()?;
    let Some(json) = json else {
        return Ok(None);
    };
    let plan = decode_plan(&json)?;
    type Row = (
        u32,
        String,
        String,
        String,
        bool,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<Row> = tx
        .prepare(
            "SELECT rank,host_id,owner_id,state,dispatched,launch_handle,identities_json
               FROM group_members
              WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 ORDER BY rank",
        )?
        .query_map(params![deployment_id, instance_index, generation], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let members = rows
        .into_iter()
        .map(
            |(rank, host_id, owner_id, state, dispatched, launch_handle, identities)| {
                Ok(MemberRow {
                    rank,
                    host_id,
                    owner_id,
                    state: MemberState::parse(&state)?,
                    dispatched,
                    launch_handle,
                    identities: identities.as_deref().map(decode_identities).transpose()?,
                })
            },
        )
        .collect::<Result<Vec<_>, GroupStoreError>>()?;
    Ok(Some((plan, members)))
}

/// ADR 0028 §11: keep the first rank a group failed at, and name the failure
/// in the instance's status (`group_member_failed`, spec §16).
pub(crate) fn record_failure(
    tx: &Transaction<'_>,
    deployment_id: &str,
    instance_index: u32,
    generation: i64,
    rank: u32,
) -> Result<(), GroupStoreError> {
    record_failure_code(
        tx,
        deployment_id,
        instance_index,
        generation,
        rank,
        "group_member_failed",
    )
}

/// As [`record_failure`], naming `code` (one of [`FAILURE_CODES`]): the first
/// failure keeps both its rank and its code.
pub(crate) fn record_failure_code(
    tx: &Transaction<'_>,
    deployment_id: &str,
    instance_index: u32,
    generation: i64,
    rank: u32,
    code: &str,
) -> Result<(), GroupStoreError> {
    if !FAILURE_CODES.contains(&code) {
        return Err(GroupStoreError::Plan);
    }
    let members: u32 = tx.query_row(
        "SELECT COUNT(*) FROM group_members
          WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3",
        params![deployment_id, instance_index, generation],
        |r| r.get(0),
    )?;
    if rank >= members {
        return Err(GroupStoreError::Conflict);
    }
    tx.execute(
        "UPDATE group_plans SET failed_rank=?4,failure_code=?5
          WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 AND failed_rank IS NULL",
        params![deployment_id, instance_index, generation, rank, code],
    )?;
    let kept: Option<String> = tx.query_row(
        "SELECT COALESCE(failure_code,'group_member_failed') FROM group_plans
          WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3",
        params![deployment_id, instance_index, generation],
        |r| r.get(0),
    )?;
    record_status(
        tx,
        deployment_id,
        instance_index,
        kept.as_deref().unwrap_or("group_member_failed"),
    )
}

/// SPEC §17, ADR 0028 §16: the closed code a group's failure or stop leaves
/// in its instance's status. Keyed by the instance: its stop moves it to a
/// later generation while its members still settle.
fn record_status(
    tx: &Transaction<'_>,
    deployment_id: &str,
    instance_index: u32,
    code: &str,
) -> Result<(), GroupStoreError> {
    tx.execute(
        "UPDATE deployment_instances SET last_error=?3 WHERE deployment_id=?1 AND instance_index=?2",
        params![deployment_id, instance_index, code],
    )?;
    Ok(())
}

/// ADR 0028 §11: whether an instance's generation ran as a group and, if it
/// did, whether its plan settled (every member released on its own host's
/// evidence, the port and worker leases freed). `None` for a single-host
/// launch, which this never changes (T39).
pub(crate) fn plan_settled(
    tx: &Transaction<'_>,
    deployment_id: &str,
    instance_index: u32,
    generation: i64,
) -> rusqlite::Result<Option<bool>> {
    tx.query_row(
        "SELECT state='settled' FROM group_plans
          WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3",
        params![deployment_id, instance_index, generation],
        |r| r.get(0),
    )
    .optional()
}

/// ADR 0028 §11: once no member of the plan at `generation` is reserved,
/// dispatching, launched or uncertain, the plan settles and frees its
/// rendezvous port, and its worker leases go with it.
fn finish_if_settled(
    tx: &Transaction<'_>,
    deployment_id: &str,
    instance_index: u32,
    generation: i64,
) -> Result<GroupSettlement, GroupStoreError> {
    let unsettled: Vec<u32> = tx
        .prepare(
            "SELECT rank FROM group_members
              WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 AND state!='settled'
              ORDER BY rank",
        )?
        .query_map(params![deployment_id, instance_index, generation], |r| {
            r.get(0)
        })?
        .collect::<rusqlite::Result<_>>()?;
    if !unsettled.is_empty() {
        return Ok(GroupSettlement::Partial { unsettled });
    }
    // ADR 0028 §11: the rendezvous port is released only after every member
    // settles; the worker leases with it.
    tx.execute(
        "UPDATE group_plans SET state='settled'
          WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3",
        params![deployment_id, instance_index, generation],
    )?;
    tx.execute(
        "DELETE FROM endpoint_leases WHERE group_owner IN
           (SELECT owner_id FROM group_members
             WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3)",
        params![deployment_id, instance_index, generation],
    )?;
    Ok(GroupSettlement::Complete)
}

impl crate::Store {
    /// ADR 0028 §5: reserve every member of a group instance, or none.
    ///
    /// `contexts` holds one admission context per member host, keyed by the
    /// host id the member names, built from that host's own limits,
    /// observations, resident floors and `max_parked` (ADR 0028 §12: a host's
    /// parked limit counts the owners on that host only).
    ///
    /// In one immediate transaction: refuse a reservation naming one host
    /// twice, or whose contexts are not exactly one per member host (`Plan`);
    /// refuse a member whose context or footprint names any key the host-scoped
    /// registry does not assign to that member's host (`Plan`); draw the lowest
    /// rendezvous port in `port_range` that no unsettled plan holds on the
    /// head, no unexpired exclusion names and no endpoint lease holds there
    /// (`PortsExhausted`); lease each SGLang worker's loopback port on its own
    /// host, past any rendezvous port held there (`PortsExhausted`); build the
    /// plan with `plan_for(port, workers)` (`Plan` when it fails or does not
    /// match); charge every member to its own owner, judged on its own host;
    /// write the plan and its members. Any error returns before commit, so the
    /// transaction rolls back and nothing is held anywhere.
    ///
    /// An unsettled plan of the instance is a `Conflict`; one an interrupted
    /// activation left with no member dispatched is released first with
    /// [`Self::release_undispatched_group`]. A fully settled plan of the same
    /// generation (a retry that redraws its rendezvous port) is replaced.
    /// Every grant must be new (a fresh grant id per attempt), else `Plan`.
    pub fn reserve_group(
        &self,
        r: &GroupReservation,
        plan_for: impl FnOnce(u16, &BTreeMap<String, u16>) -> Result<GroupPlan, GroupIdentityError>,
        contexts: &BTreeMap<String, AdmissionContext<'_>>,
    ) -> Result<GroupPlan, GroupStoreError> {
        // ADR 0028 §4: distinct hosts, the head first, one fence for all.
        let mut hosts = BTreeSet::new();
        let Some((head, first)) = r.members.first() else {
            return Err(GroupStoreError::Plan);
        };
        if r.members.len() < 2
            || r.deployment_id.is_empty()
            || r.instance_index > capyctl_domain::group::MAX_INSTANCE_INDEX
            || r.members
                .iter()
                .any(|(host, _)| host.trim().is_empty() || !hosts.insert(host.as_str()))
            || &r.head_host != head
            || contexts.len() != r.members.len()
            || contexts.keys().any(|host| !hosts.contains(host.as_str()))
            || *r.port_range.start() == 0
            || r.worker_ports.iter().any(|(host, range)| {
                host == head || !hosts.contains(host.as_str()) || *range.start() == 0
            })
        {
            return Err(GroupStoreError::Plan);
        }
        for (rank, (_, grant)) in r.members.iter().enumerate() {
            if grant.deployment_id != r.deployment_id
                || grant.owner_id
                    != member_owner_id(&r.deployment_id, r.instance_index, rank as u32)
                || grant.revision != first.revision
                || grant.generation != first.generation
                || grant.operation_id != first.operation_id
                || grant.expected_epoch != first.expected_epoch
            {
                return Err(GroupStoreError::Plan);
            }
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        // One unsettled plan per instance, and never beside an instance owner.
        let busy: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM group_plans WHERE deployment_id=?1 AND instance_index=?2 AND state!='settled')
                 OR EXISTS(SELECT 1 FROM resource_owners WHERE deployment_id=?1 AND instance_index=?2 AND member_rank IS NULL)",
            params![r.deployment_id, r.instance_index],
            |row| row.get(0),
        )?;
        if busy {
            return Err(GroupStoreError::Conflict);
        }
        // ADR 0028 §5, §11, SPEC §7: each member is judged on its own host and
        // charged there only. Its context names that host's domains, and every
        // key its footprint charges is registered to that host. An unknown key,
        // or one of another host's, fails closed, so no member can later be
        // released on one host's evidence while its bytes sit on another.
        for (host, grant) in &r.members {
            let namespace = member_namespace(&tx, host)?;
            let context = contexts.get(host).ok_or(GroupStoreError::Plan)?;
            let keys = context
                .limits
                .iter()
                .map(|limit| ("domain", limit.domain.as_str()))
                .chain(
                    grant
                        .next
                        .allocations
                        .iter()
                        .map(|a| ("domain", a.domain.as_str())),
                )
                .chain(
                    grant
                        .next
                        .devices
                        .iter()
                        .map(|d| ("device", d.device.as_str())),
                );
            for (kind, key) in keys {
                let owner = registered_host(&tx, kind, key).map_err(GroupStoreError::Admission)?;
                if owner.as_deref() != Some(namespace.as_str()) {
                    return Err(GroupStoreError::Plan);
                }
            }
        }
        // ADR 0028 §5 (R15), SPEC §3: skip ports held by unsettled plans on
        // this head, ports a Prepare recently found held outside CapyCTL, and
        // ports an endpoint lease holds on the head.
        let now = now_ms();
        let mut rendezvous = None;
        for port in r.port_range.clone() {
            if rendezvous_port_free(&tx, &r.head_host, port, now)? {
                rendezvous = Some(port);
                break;
            }
        }
        let rendezvous = rendezvous.ok_or(GroupStoreError::PortsExhausted)?;
        // ADR 0028 §5, SPEC §3: SGLang worker loopback ports come from each
        // worker host's endpoint range through the endpoint lease table, past
        // any port an unsettled plan holds there as its rendezvous port.
        let mut workers = BTreeMap::new();
        for (rank, (host, _)) in r.members.iter().enumerate() {
            let Some(range) = r.worker_ports.get(host) else {
                continue;
            };
            let mut chosen = None;
            for port in range.clone() {
                if endpoint_port_free(&tx, host, port)? && !rendezvous_port_held(&tx, host, port)? {
                    chosen = Some(port);
                    break;
                }
            }
            let port = chosen.ok_or(GroupStoreError::PortsExhausted)?;
            tx.execute(
                "INSERT INTO endpoint_leases(host_id,host,port,binding_id,group_owner) VALUES(?1,?2,?3,NULL,?4)",
                params![
                    host,
                    LOOPBACK,
                    port,
                    member_owner_id(&r.deployment_id, r.instance_index, rank as u32)
                ],
            )?;
            workers.insert(host.clone(), port);
        }
        // R7: the caller builds the plan from the drawn ports and the paths
        // and digests every host already reported; it must match what is
        // reserved here.
        let plan = plan_for(rendezvous, &workers).map_err(|_| GroupStoreError::Plan)?;
        if plan.rendezvous_port() != rendezvous
            || plan.generation() != first.generation
            || plan.members().len() != r.members.len()
            || plan
                .members()
                .iter()
                .zip(&r.members)
                .any(|(member, (host, _))| {
                    &member.member.host_id != host
                        || member.worker_port != workers.get(host).copied()
                })
        {
            return Err(GroupStoreError::Plan);
        }
        // ADR 0028 §5, SPEC §11: all members or none; atomic accounting, not an atomic launch.
        // ADR 0007: the caller's observed epoch is compared once for the whole
        // group, then chained across the members' grants in this transaction.
        let epoch: i64 = tx.query_row(
            "SELECT epoch FROM resource_ledger_meta WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        if u64::try_from(epoch).ok() != Some(first.expected_epoch) {
            return Err(GroupStoreError::Conflict);
        }
        // ADR 0028 §12, SPEC §7: each grant is admitted against its own host's
        // context, so the ledger it sees and the parked owners it counts are
        // that host's only; the epoch chain stays the whole ledger's.
        for (rank, (host, grant)) in r.members.iter().enumerate() {
            let context = *contexts.get(host).ok_or(GroupStoreError::Plan)?;
            let mut grant = grant.clone();
            grant.expected_epoch = first.expected_epoch + rank as u64;
            let receipt = crate::resource_ledger::reserve_member_increase_in_transaction(
                &tx,
                &grant,
                rank as u32,
                context,
            )
            .map_err(|error| match error {
                ResourceStoreError::Conflict => GroupStoreError::Conflict,
                other => GroupStoreError::Admission(other),
            })?;
            // Every reservation attempt is new grants: one already recorded
            // would leave this member written without a charge.
            if !matches!(receipt, crate::resource_ledger::GrantReceipt::New { .. }) {
                return Err(GroupStoreError::Plan);
            }
        }
        // ADR 0028 §5 (R15): a retry in the same generation (a rendezvous
        // port a `Prepare` found held, redrawn) replaces that generation's
        // plan. The busy check above proved it fully settled, so nothing it
        // charged or leased is still held.
        tx.execute(
            "DELETE FROM group_members WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3",
            params![r.deployment_id, r.instance_index, first.generation],
        )?;
        tx.execute(
            "DELETE FROM group_plans WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 AND state='settled'",
            params![r.deployment_id, r.instance_index, first.generation],
        )?;
        tx.execute(
            "INSERT INTO group_plans(deployment_id,instance_index,generation,plan_json,rendezvous_host,rendezvous_port,state)
             VALUES(?1,?2,?3,?4,?5,?6,'active')",
            params![
                r.deployment_id,
                r.instance_index,
                first.generation,
                encode_plan(&plan)?,
                r.head_host,
                rendezvous
            ],
        )?;
        for (rank, (host, grant)) in r.members.iter().enumerate() {
            tx.execute(
                "INSERT INTO group_members(deployment_id,instance_index,generation,rank,host_id,owner_id,state)
                 VALUES(?1,?2,?3,?4,?5,?6,'reserved')",
                params![
                    r.deployment_id,
                    r.instance_index,
                    first.generation,
                    rank as u32,
                    host,
                    grant.owner_id
                ],
            )?;
        }
        tx.commit()?;
        Ok(plan)
    }

    /// ADR 0028 §8, §11: durably fence one member as dispatched, before its
    /// Launch is sent.
    ///
    /// Contract for the controller (Task 16, group activation, which sends
    /// every Launch; Task 17 relies on it when settling): call this for a
    /// member, and see
    /// it return `Ok`, before sending that member's Launch, and on every
    /// retry of it. A Launch whose reply is lost may have spawned the member,
    /// so from here on it never settles on empty gone evidence, in any later
    /// state, `uncertain` included. It settles only after
    /// [`Self::mark_member_launched`] records its identities (from the Launch
    /// reply, a replay of it, or the host's journal when the host reconnects
    /// or recovers under ADR 0016), on gone evidence for exactly those. There
    /// is no release on revocation alone: a member whose host never reports
    /// its identities keeps its charge, and status must show it as retained.
    ///
    /// `launch_handle` is the command id of the Launch about to be sent (its
    /// owned handle on the member's host); it is recorded with the first fence
    /// so the member's stop reaches that launch from any later session
    /// (ADR 0028 §11). A repeated fence naming the same handle is accepted;
    /// one naming another handle is a `Conflict`, as is fencing a member that
    /// is settled, or uncertain without having been dispatched: nothing is
    /// launched for it.
    pub fn mark_member_dispatching(
        &self,
        deployment_id: &str,
        instance_index: u32,
        generation: i64,
        rank: u32,
        launch_handle: &str,
    ) -> Result<(), GroupStoreError> {
        if launch_handle.trim().is_empty() {
            return Err(GroupStoreError::Plan);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let row: Option<(String, bool, Option<String>)> = tx
            .query_row(
                "SELECT state,dispatched,launch_handle FROM group_members
                  WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 AND rank=?4",
                params![deployment_id, instance_index, generation, rank],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((state, dispatched, handle)) = row else {
            return Err(GroupStoreError::Conflict);
        };
        match (MemberState::parse(&state)?, dispatched) {
            (MemberState::Reserved, false) => {
                tx.execute(
                    "UPDATE group_members SET state='dispatching',dispatched=1,launch_handle=?5
                      WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 AND rank=?4",
                    params![
                        deployment_id,
                        instance_index,
                        generation,
                        rank,
                        launch_handle
                    ],
                )?;
            }
            // A retry of the fence, or of a Launch already fenced: the same
            // Launch, never another one.
            (MemberState::Settled, _) => return Err(GroupStoreError::Conflict),
            (_, true) if handle.as_deref() == Some(launch_handle) => {}
            _ => return Err(GroupStoreError::Conflict),
        }
        tx.commit()?;
        Ok(())
    }

    /// ADR 0028 §8, §11: record the processes a dispatched member launched,
    /// so it later settles only on gone evidence for exactly these identities.
    /// Accepted from `dispatching`, and from `uncertain` once dispatched (the
    /// host reconciled its journal), moving the member to `launched`; a retry
    /// with the same identities changes nothing. A member never fenced by
    /// [`Self::mark_member_dispatching`] is a `Conflict`.
    pub fn mark_member_launched(
        &self,
        deployment_id: &str,
        instance_index: u32,
        generation: i64,
        rank: u32,
        identities: &[ProcessIdentity],
    ) -> Result<(), GroupStoreError> {
        validate_local_processes(identities).map_err(|_| GroupStoreError::Plan)?;
        let recorded = canonical_identities(identities);
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let row: Option<(String, bool, Option<String>)> = tx
            .query_row(
                "SELECT state,dispatched,identities_json FROM group_members
                  WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 AND rank=?4",
                params![deployment_id, instance_index, generation, rank],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((state, dispatched, existing)) = row else {
            return Err(GroupStoreError::Conflict);
        };
        match (MemberState::parse(&state)?, dispatched, existing) {
            (MemberState::Dispatching | MemberState::Uncertain, true, None) => {
                tx.execute(
                    "UPDATE group_members SET state='launched',identities_json=?5
                      WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 AND rank=?4",
                    params![deployment_id, instance_index, generation, rank, recorded],
                )?;
            }
            // A retry with the same identities.
            (MemberState::Launched | MemberState::Uncertain, true, Some(existing))
                if existing == recorded => {}
            _ => return Err(GroupStoreError::Conflict),
        }
        tx.commit()?;
        Ok(())
    }

    /// ADR 0028 §11, SPEC §11: release one member's reservation on gone
    /// evidence from its own host: exactly its recorded identities, or none
    /// for a member never dispatched. A member dispatched without identities
    /// recorded is refused (`Conflict`) and keeps its charge. The rendezvous
    /// port and worker leases are freed only once no member is reserved,
    /// dispatching, launched or uncertain. A retry for a member already
    /// settled reports the group's state again.
    pub fn settle_member(
        &self,
        deployment_id: &str,
        instance_index: u32,
        generation: i64,
        rank: u32,
        evidence: MemberGone,
    ) -> Result<GroupSettlement, GroupStoreError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let row: Option<(String, String, String, bool, Option<String>)> = tx
            .query_row(
                "SELECT host_id,owner_id,state,dispatched,identities_json FROM group_members
                  WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 AND rank=?4",
                params![deployment_id, instance_index, generation, rank],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let Some((host, owner, state, dispatched, recorded)) = row else {
            return Err(GroupStoreError::Conflict);
        };
        // ADR 0028 §11: each member settles on its own host's evidence only.
        let proven = match &recorded {
            // ADR 0028 §8, §11: empty evidence proves only a member that was
            // never dispatched gone. A Launch whose reply was lost may have
            // spawned it, and an empty journal settles only on recorded
            // identities.
            None => !dispatched && evidence.identities.is_empty(),
            Some(recorded) => {
                validate_local_processes(&evidence.identities).is_ok()
                    && &canonical_identities(&evidence.identities) == recorded
            }
        };
        if evidence.member.host_id != host
            || evidence.member.member_id != member_id(rank)
            || !proven
        {
            return Err(GroupStoreError::Conflict);
        }
        if MemberState::parse(&state)? != MemberState::Settled {
            let released = tx.execute("DELETE FROM resource_owners WHERE owner_id=?1", [&owner])?;
            if released != 1 {
                return Err(corrupt());
            }
            crate::resource_ledger::advance_completion_epoch(&tx).map_err(|_| corrupt())?;
            tx.execute(
                "UPDATE group_members SET state='settled'
                  WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 AND rank=?4",
                params![deployment_id, instance_index, generation, rank],
            )?;
        }
        let outcome = finish_if_settled(&tx, deployment_id, instance_index, generation)?;
        tx.commit()?;
        Ok(outcome)
    }

    /// ADR 0028 §8, §11: release a plan whose activation was interrupted, or
    /// whose `Prepare` was refused, before any member was dispatched.
    ///
    /// Contract for the controller (Task 16, group activation): empty
    /// evidence proves only a member never dispatched gone, so this settles
    /// every member of the plan at `generation` only when none was fenced by
    /// [`Self::mark_member_dispatching`]; it releases their charges, their
    /// worker leases and the rendezvous port in one transaction. A plan with
    /// any dispatched member, or no plan at that generation, is a `Conflict`
    /// and changes nothing: such a member settles only on its own host's
    /// evidence ([`Self::settle_member`]). Only the instance's single
    /// activation calls it, for a plan no live activation is using. A plan
    /// already settled reports `Complete` again.
    pub fn release_undispatched_group(
        &self,
        deployment_id: &str,
        instance_index: u32,
        generation: i64,
    ) -> Result<GroupSettlement, GroupStoreError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let rows: Vec<(String, String, bool)> = tx
            .prepare(
                "SELECT owner_id,state,dispatched FROM group_members
                  WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 ORDER BY rank",
            )?
            .query_map(params![deployment_id, instance_index, generation], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        if rows.is_empty() {
            return Err(GroupStoreError::Conflict);
        }
        let mut unsettled = Vec::new();
        for (owner, state, dispatched) in rows {
            if MemberState::parse(&state)? == MemberState::Settled {
                continue;
            }
            // R23: a dispatched member may be running; nothing is released.
            if dispatched {
                return Err(GroupStoreError::Conflict);
            }
            unsettled.push(owner);
        }
        for owner in &unsettled {
            let released = tx.execute("DELETE FROM resource_owners WHERE owner_id=?1", [owner])?;
            if released != 1 {
                return Err(corrupt());
            }
            crate::resource_ledger::advance_completion_epoch(&tx).map_err(|_| corrupt())?;
        }
        tx.execute(
            "UPDATE group_members SET state='settled'
              WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3",
            params![deployment_id, instance_index, generation],
        )?;
        let outcome = finish_if_settled(&tx, deployment_id, instance_index, generation)?;
        tx.commit()?;
        Ok(outcome)
    }

    /// ADR 0028 §11: a member whose host cannot be reached keeps its charge.
    /// Nothing is released here, and lease expiry never frees it. Whether it
    /// was dispatched, and any identities recorded, are kept.
    pub fn mark_member_uncertain(
        &self,
        deployment_id: &str,
        instance_index: u32,
        generation: i64,
        rank: u32,
    ) -> Result<(), GroupStoreError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE group_members SET state='uncertain'
              WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 AND rank=?4
                AND state IN ('reserved','dispatching','launched','uncertain')",
            params![deployment_id, instance_index, generation, rank],
        )?;
        if changed != 1 {
            return Err(GroupStoreError::Conflict);
        }
        tx.commit()?;
        Ok(())
    }

    /// The newest group plan of an instance with its members in rank order,
    /// or `None` when the instance never had one.
    pub fn group_plan(
        &self,
        deployment_id: &str,
        instance_index: u32,
    ) -> Result<Option<(GroupPlan, Vec<MemberRow>)>, GroupStoreError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let generation: Option<i64> = tx
            .query_row(
                "SELECT generation FROM group_plans
                  WHERE deployment_id=?1 AND instance_index=?2 ORDER BY generation DESC LIMIT 1",
                params![deployment_id, instance_index],
                |r| r.get(0),
            )
            .optional()?;
        let Some(generation) = generation else {
            return Ok(None);
        };
        let plan = plan_at(&tx, deployment_id, instance_index, generation)?;
        tx.commit()?;
        Ok(plan)
    }

    /// The group plan of an instance at `generation`, with its members in
    /// rank order, or `None` when that generation had none.
    pub fn group_plan_at(
        &self,
        deployment_id: &str,
        instance_index: u32,
        generation: i64,
    ) -> Result<Option<(GroupPlan, Vec<MemberRow>)>, GroupStoreError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let plan = plan_at(&tx, deployment_id, instance_index, generation)?;
        tx.commit()?;
        Ok(plan)
    }

    /// ADR 0028 §9, §11: the group a runtime binding realizes, when it is a
    /// group head's: the binding's Initialize names its instance and
    /// generation, and a plan exists there. `None` for any other binding.
    pub fn group_of_binding(
        &self,
        binding_id: &str,
    ) -> Result<Option<BoundGroup>, GroupStoreError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let lane: Option<(String, u32, i64)> = tx
            .query_row(
                "SELECT r.deployment_id,r.instance_index,r.generation FROM lifecycle_steps s
                   JOIN operations o ON o.id=s.operation_id
                   JOIN lifecycle_runs r ON r.operation_id=s.operation_id
                  WHERE s.binding_id=?1 AND o.kind='initialize'",
                [binding_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((deployment_id, instance_index, generation)) = lane else {
            return Ok(None);
        };
        let bound =
            plan_at(&tx, &deployment_id, instance_index, generation)?.map(|(plan, members)| {
                BoundGroup {
                    deployment_id,
                    instance_index,
                    plan,
                    members,
                }
            });
        tx.commit()?;
        Ok(bound)
    }

    /// ADR 0028 §11: the group at `generation` failed at `rank` (an exit, a
    /// launch failure or a failed readiness). The first failure is kept; a
    /// later one changes nothing. Nothing is released here.
    pub fn record_group_failure(
        &self,
        deployment_id: &str,
        instance_index: u32,
        generation: i64,
        rank: u32,
    ) -> Result<(), GroupStoreError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        record_failure(&tx, deployment_id, instance_index, generation, rank)?;
        tx.commit()?;
        Ok(())
    }

    /// ADR 0028 §12: as [`Self::record_group_failure`], naming `code`
    /// (`group_member_failed`, `group_wake_mismatch` or `group_stalled`) as
    /// the failure's closed code. The first failure keeps its rank and its
    /// code.
    pub fn record_group_failure_code(
        &self,
        deployment_id: &str,
        instance_index: u32,
        generation: i64,
        rank: u32,
        code: &str,
    ) -> Result<(), GroupStoreError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        record_failure_code(&tx, deployment_id, instance_index, generation, rank, code)?;
        tx.commit()?;
        Ok(())
    }

    /// The closed code of the failure the group at `generation` recorded,
    /// if it failed (`group_member_failed` unless another was named).
    pub fn group_failure_code(
        &self,
        deployment_id: &str,
        instance_index: u32,
        generation: i64,
    ) -> Result<Option<String>, GroupStoreError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let code: Option<Option<String>> = tx
            .query_row(
                "SELECT CASE WHEN failed_rank IS NULL THEN NULL
                             ELSE COALESCE(failure_code,'group_member_failed') END
                   FROM group_plans
                  WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3",
                params![deployment_id, instance_index, generation],
                |r| r.get(0),
            )
            .optional()?;
        tx.commit()?;
        Ok(code.flatten())
    }

    /// ADR 0028 §12 (decided 2026-10-06): record the wake canary's reference
    /// for the active plan at `generation`, once. Returns whether this call
    /// recorded it: an earlier reference is kept (`false`), and a plan that
    /// is not active records nothing (`Conflict`).
    pub fn record_canary_reference(
        &self,
        deployment_id: &str,
        instance_index: u32,
        generation: i64,
        reference: &StoredCanary,
    ) -> Result<bool, GroupStoreError> {
        if reference.tokens.is_empty() {
            return Err(GroupStoreError::Plan);
        }
        let raw = serde_json::to_string(reference).map_err(|_| GroupStoreError::Plan)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM group_plans
              WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 AND state='active')",
            params![deployment_id, instance_index, generation],
            |r| r.get(0),
        )?;
        if !active {
            return Err(GroupStoreError::Conflict);
        }
        let recorded = tx.execute(
            "UPDATE group_plans SET canary_json=?4
              WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 AND canary_json IS NULL",
            params![deployment_id, instance_index, generation, raw],
        )? == 1;
        tx.commit()?;
        Ok(recorded)
    }

    /// The wake canary's reference recorded for the plan at `generation`.
    pub fn canary_reference(
        &self,
        deployment_id: &str,
        instance_index: u32,
        generation: i64,
    ) -> Result<Option<StoredCanary>, GroupStoreError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let raw: Option<Option<String>> = tx
            .query_row(
                "SELECT canary_json FROM group_plans
                  WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3",
                params![deployment_id, instance_index, generation],
                |r| r.get(0),
            )
            .optional()?;
        tx.commit()?;
        raw.flatten()
            .map(|raw| serde_json::from_str(&raw).map_err(|_| corrupt()))
            .transpose()
    }

    /// SPEC §17, ADR 0028 §16: name `code` (`group_member_uncertain` while a
    /// stopped group still has an unreachable member) in the status of the
    /// group's instance. Nothing else changes.
    pub fn record_group_status(
        &self,
        deployment_id: &str,
        instance_index: u32,
        code: &str,
    ) -> Result<(), GroupStoreError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        record_status(&tx, deployment_id, instance_index, code)?;
        tx.commit()?;
        Ok(())
    }

    /// ADR 0028 §16: the instance's status no longer names `code` (the
    /// uncertainty a stop recorded while a host was away, once every member
    /// settled). Any other recorded error is kept.
    pub fn clear_group_status(
        &self,
        deployment_id: &str,
        instance_index: u32,
        code: &str,
    ) -> Result<(), GroupStoreError> {
        self.conn.execute(
            "UPDATE deployment_instances SET last_error=NULL
              WHERE deployment_id=?1 AND instance_index=?2 AND last_error=?3",
            params![deployment_id, instance_index, code],
        )?;
        Ok(())
    }

    /// The rank the group at `generation` failed at, if it failed.
    pub fn group_failure(
        &self,
        deployment_id: &str,
        instance_index: u32,
        generation: i64,
    ) -> Result<Option<u32>, GroupStoreError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let rank: Option<Option<u32>> = tx
            .query_row(
                "SELECT failed_rank FROM group_plans
                  WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3",
                params![deployment_id, instance_index, generation],
                |r| r.get(0),
            )
            .optional()?;
        tx.commit()?;
        Ok(rank.flatten())
    }

    /// ADR 0028 §11, §13: whether an unsettled group plan holds `port` on
    /// `host_id` as its rendezvous port.
    pub fn rendezvous_port_held_on(
        &self,
        host_id: &str,
        port: u16,
    ) -> Result<bool, GroupStoreError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let held = rendezvous_port_held(&tx, host_id, port)?;
        tx.commit()?;
        Ok(held)
    }

    /// ADR 0028 §5, §7 (R15): a `Prepare` found `port` held outside CapyCTL on
    /// `host_id`; the rendezvous draw skips it on that head until `until_ms`
    /// (Unix milliseconds). The latest finding replaces an earlier one.
    pub fn exclude_rendezvous_port(
        &self,
        host_id: &str,
        port: u16,
        until_ms: i64,
    ) -> Result<(), GroupStoreError> {
        if host_id.trim().is_empty() || port == 0 {
            return Err(GroupStoreError::Plan);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO group_port_exclusions(host_id,port,until_ms) VALUES(?1,?2,?3)
             ON CONFLICT(host_id,port) DO UPDATE SET until_ms=excluded.until_ms",
            params![host_id, port, until_ms],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// SPEC §3: whether `port` on `host_id`'s loopback is leased, by a runtime
    /// binding or by a group member.
    pub fn endpoint_port_leased(&self, host_id: &str, port: u16) -> Result<bool, GroupStoreError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let free = endpoint_port_free(&tx, host_id, port)?;
        tx.commit()?;
        Ok(!free)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // T33: a member owner never parses as an instance owner, and back.
    #[test]
    fn member_owner_ids_are_canonical() {
        assert_eq!(
            member_owner_id("d", 2, 1),
            "deployment:d/instance:2/member:1"
        );
        assert_eq!(
            parse_member_owner_id("deployment:d/instance:2/member:1"),
            Some(("d".into(), 2, 1))
        );
        for bad in [
            "d",
            "deployment:d/instance:2",
            "deployment:/instance:0/member:1",
            "deployment:d/instance:02/member:1",
            "deployment:d/instance:2/member:x",
        ] {
            assert_eq!(parse_member_owner_id(bad), None, "{bad}");
        }
        assert_eq!(
            crate::instances::parse_instance_owner_id(&member_owner_id("d", 2, 1)).1,
            0,
            "a member owner is not an instance owner"
        );
    }
}

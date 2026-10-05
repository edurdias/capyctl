//! ADR 0028 §4, §5, §11: multi-node group plans in the durable store.
//!
//! A group instance is charged as one resource owner per member,
//! `deployment:<id>/instance:<k>/member:<r>`, each on its own host's domains.
//! Every member is reserved in one store transaction together with the group
//! plan, the rendezvous port drawn from the head's range and any SGLang worker
//! loopback port leased on its own host. A failure anywhere rolls the whole
//! transaction back, so no host's share is held for a group that did not fit
//! elsewhere. This is atomic accounting in the server's store, not an atomic
//! launch across hosts.
//!
//! Each member settles only on gone evidence from its own host. A member whose
//! host cannot be reached is marked uncertain and keeps its charge. The
//! rendezvous port and worker leases are freed only when every member has
//! settled.

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

use crate::resource_ledger::{GrantRequest, ResourceStoreError};

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
/// Every grant names the same revision and generation, and the ledger epoch
/// the caller observed before reserving; the store chains the epoch across
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
    /// These ranks are still reserved, launched or uncertain.
    Partial { unsettled: Vec<u32> },
    /// Every member settled; the rendezvous port and worker leases are free.
    Complete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberState {
    Reserved,
    Launched,
    Settled,
    Uncertain,
}

impl MemberState {
    fn parse(value: &str) -> Result<Self, GroupStoreError> {
        Ok(match value {
            "reserved" => Self::Reserved,
            "launched" => Self::Launched,
            "settled" => Self::Settled,
            "uncertain" => Self::Uncertain,
            _ => return Err(corrupt()),
        })
    }
}

/// One stored member of a group plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberRow {
    pub rank: u32,
    pub host_id: String,
    pub owner_id: String,
    pub state: MemberState,
}

/// SPEC §11, ADR 0028 §11: gone evidence for one member, reported by that
/// member's own host. `identities` are the processes it proved gone; a member
/// recorded as launched settles only when they are exactly its recorded
/// identities, and a member that never launched only with none.
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

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

/// The loopback host every endpoint lease names (SPEC §3).
const LOOPBACK: &str = "127.0.0.1";

fn rendezvous_port_free(
    tx: &Transaction<'_>,
    head: &str,
    port: u16,
    now: i64,
) -> Result<bool, GroupStoreError> {
    Ok(tx.query_row(
        "SELECT NOT EXISTS(SELECT 1 FROM group_plans WHERE rendezvous_host=?1 AND rendezvous_port=?2 AND state!='settled')
            AND NOT EXISTS(SELECT 1 FROM group_port_exclusions WHERE host_id=?1 AND port=?2 AND until_ms>?3)",
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
           state TEXT NOT NULL CHECK(state IN ('reserved','launched','settled','uncertain')),
           identities_json TEXT,
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
            WHERE digest IS NOT NULL AND host_id!='' AND state IN ('recorded','mismatch');",
    )?;
    Ok(())
}

impl crate::Store {
    /// ADR 0028 §5: reserve every member of a group instance, or none.
    ///
    /// In one immediate transaction: refuse a reservation naming one host
    /// twice (`Plan`); draw the lowest rendezvous port in `port_range` that no
    /// unsettled plan holds on the head and no unexpired exclusion names
    /// (`PortsExhausted`); lease each SGLang worker's loopback port on its own
    /// host (`PortsExhausted`); build the plan with `plan_for(port, workers)`
    /// (`Plan` when it fails or does not match); charge every member to its own
    /// owner; write the plan and its members. Any error returns before commit,
    /// so the transaction rolls back and nothing is held anywhere.
    pub fn reserve_group(
        &self,
        r: &GroupReservation,
        plan_for: impl FnOnce(u16, &BTreeMap<String, u16>) -> Result<GroupPlan, GroupIdentityError>,
        ctx: AdmissionContext<'_>,
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
        // ADR 0028 §5 (R15): skip ports held by unsettled plans on this head
        // and ports a Prepare recently found held outside CapyCTL.
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
        // worker host's endpoint range through the endpoint lease table.
        let mut workers = BTreeMap::new();
        for (rank, (host, _)) in r.members.iter().enumerate() {
            let Some(range) = r.worker_ports.get(host) else {
                continue;
            };
            let mut chosen = None;
            for port in range.clone() {
                if endpoint_port_free(&tx, host, port)? {
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
        for (rank, (_, grant)) in r.members.iter().enumerate() {
            let mut grant = grant.clone();
            grant.expected_epoch = first.expected_epoch + rank as u64;
            crate::resource_ledger::reserve_member_increase_in_transaction(
                &tx,
                &grant,
                rank as u32,
                ctx,
            )
            .map_err(|error| match error {
                ResourceStoreError::Conflict => GroupStoreError::Conflict,
                other => GroupStoreError::Admission(other),
            })?;
        }
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

    /// ADR 0028 §11: record the processes a member launched, so it later
    /// settles only on gone evidence for exactly these identities.
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
        let row: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT state,identities_json FROM group_members
                  WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 AND rank=?4",
                params![deployment_id, instance_index, generation, rank],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            Some((state, None)) if state == "reserved" => {
                tx.execute(
                    "UPDATE group_members SET state='launched',identities_json=?5
                      WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 AND rank=?4",
                    params![deployment_id, instance_index, generation, rank, recorded],
                )?;
            }
            // A retry with the same identities.
            Some((state, Some(existing))) if state == "launched" && existing == recorded => {}
            _ => return Err(GroupStoreError::Conflict),
        }
        tx.commit()?;
        Ok(())
    }

    /// ADR 0028 §11, SPEC §11: release one member's reservation on gone
    /// evidence from its own host. The rendezvous port and worker leases are
    /// freed only once no member is reserved, launched or uncertain. A retry
    /// for a member already settled reports the group's state again.
    pub fn settle_member(
        &self,
        deployment_id: &str,
        instance_index: u32,
        generation: i64,
        rank: u32,
        evidence: MemberGone,
    ) -> Result<GroupSettlement, GroupStoreError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let row: Option<(String, String, String, Option<String>)> = tx
            .query_row(
                "SELECT host_id,owner_id,state,identities_json FROM group_members
                  WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 AND rank=?4",
                params![deployment_id, instance_index, generation, rank],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((host, owner, state, recorded)) = row else {
            return Err(GroupStoreError::Conflict);
        };
        // ADR 0028 §11: each member settles on its own host's evidence only.
        let proven = match &recorded {
            None => evidence.identities.is_empty(),
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
        let outcome = if unsettled.is_empty() {
            // ADR 0028 §11: the rendezvous port is released only after every
            // member settles; the worker leases with it.
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
            GroupSettlement::Complete
        } else {
            GroupSettlement::Partial { unsettled }
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// ADR 0028 §11: a member whose host cannot be reached keeps its charge.
    /// Nothing is released here, and lease expiry never frees it.
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
                AND state IN ('reserved','launched','uncertain')",
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
        let row: Option<(i64, String)> = tx
            .query_row(
                "SELECT generation,plan_json FROM group_plans
                  WHERE deployment_id=?1 AND instance_index=?2 ORDER BY generation DESC LIMIT 1",
                params![deployment_id, instance_index],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((generation, json)) = row else {
            return Ok(None);
        };
        let plan = decode_plan(&json)?;
        let rows: Vec<(u32, String, String, String)> = tx
            .prepare(
                "SELECT rank,host_id,owner_id,state FROM group_members
                  WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 ORDER BY rank",
            )?
            .query_map(params![deployment_id, instance_index, generation], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let members = rows
            .into_iter()
            .map(|(rank, host_id, owner_id, state)| {
                Ok(MemberRow {
                    rank,
                    host_id,
                    owner_id,
                    state: MemberState::parse(&state)?,
                })
            })
            .collect::<Result<Vec<_>, GroupStoreError>>()?;
        tx.commit()?;
        Ok(Some((plan, members)))
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

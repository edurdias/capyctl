//! SPEC §§11 and 13: process identities are local to an assigned host/member.
use crate::completion::ProcessIdentity;
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct MemberKey {
    pub host_id: String,
    pub member_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberProcesses {
    pub member: MemberKey,
    pub processes: Vec<ProcessIdentity>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid or incomplete host-scoped group evidence")]
pub struct GroupIdentityError;

/// Validate local identities without assuming every member has an API process.
/// A headless rank is valid; roles and PIDs are unique only within this host.
pub fn validate_local_processes(ids: &[ProcessIdentity]) -> Result<(), GroupIdentityError> {
    let Some(first) = ids.first() else {
        return Err(GroupIdentityError);
    };
    let mut roles = BTreeSet::new();
    let mut pids = BTreeSet::new();
    if ids.iter().any(|id| {
        id.role.trim().is_empty()
            || id.pid == 0
            || id.start_ticks == 0
            || id.boot_id.trim().is_empty()
            || id.boot_id != first.boot_id
            || !roles.insert(&id.role)
            || !pids.insert(id.pid)
    }) {
        return Err(GroupIdentityError);
    }
    Ok(())
}

/// SPEC §11: every assigned member must provide the exact recorded identities.
/// PID reuse never becomes completion evidence, and missing ranks cannot settle.
pub fn verify_group_processes(
    expected: &[MemberProcesses],
    actual: &[MemberProcesses],
) -> Result<(), GroupIdentityError> {
    fn canonical(
        members: &[MemberProcesses],
    ) -> Result<std::collections::BTreeMap<MemberKey, BTreeSet<ProcessIdentity>>, GroupIdentityError>
    {
        if members.is_empty() {
            return Err(GroupIdentityError);
        }
        let mut result = std::collections::BTreeMap::new();
        let mut processes = BTreeSet::new();
        let mut boots = std::collections::BTreeMap::new();
        for member in members {
            if member.member.host_id.trim().is_empty() || member.member.member_id.trim().is_empty()
            {
                return Err(GroupIdentityError);
            }
            validate_local_processes(&member.processes)?;
            if boots
                .insert(&member.member.host_id, &member.processes[0].boot_id)
                .is_some_and(|boot| boot != &member.processes[0].boot_id)
            {
                return Err(GroupIdentityError);
            }
            for process in &member.processes {
                if !processes.insert((&member.member.host_id, process.pid)) {
                    return Err(GroupIdentityError);
                }
            }
            if result
                .insert(
                    member.member.clone(),
                    member.processes.iter().cloned().collect(),
                )
                .is_some()
            {
                return Err(GroupIdentityError);
            }
        }
        Ok(result)
    }
    if canonical(expected)? != canonical(actual)? {
        return Err(GroupIdentityError);
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberPlan {
    pub member: MemberKey,
    pub rank: u32,
    pub profile_name: String,
    pub profile_fingerprint: String,
    pub checkpoint_fingerprint: String,
    pub devices: Vec<String>,
    pub peer_address: std::net::IpAddr,
    pub service_port: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupPlan {
    members: Vec<MemberPlan>,
    rendezvous_port: u16,
}
impl GroupPlan {
    /// SPEC §11: this contract permits only the reviewed two-host TP2/DP1 shape.
    /// Recipe qualification remains a separate prerequisite; shape grants no launch authority.
    pub fn two_host(
        members: Vec<MemberPlan>,
        rendezvous_port: u16,
    ) -> Result<Self, GroupIdentityError> {
        if members.len() != 2 || rendezvous_port == 0 {
            return Err(GroupIdentityError);
        }
        let mut ranks = BTreeSet::new();
        let mut hosts = BTreeSet::new();
        for member in &members {
            if member.member.host_id.trim().is_empty()
                || member.member.member_id.trim().is_empty()
                || !hosts.insert(&member.member.host_id)
                || member.rank > 1
                || !ranks.insert(member.rank)
                || member.profile_name.trim().is_empty()
                || member.profile_fingerprint.trim().is_empty()
                || member.checkpoint_fingerprint.trim().is_empty()
                || member.service_port == 0
                || member.peer_address.is_unspecified()
                || member.peer_address.is_multicast()
                || member.peer_address.is_loopback()
                || member.devices.len() != 1
                || member.devices[0].trim().is_empty()
            {
                return Err(GroupIdentityError);
            }
        }
        if members[0].profile_fingerprint != members[1].profile_fingerprint
            || members[0].checkpoint_fingerprint != members[1].checkpoint_fingerprint
            || members[0].peer_address == members[1].peer_address
        {
            return Err(GroupIdentityError);
        }
        Ok(Self {
            members,
            rendezvous_port,
        })
    }
    pub fn members(&self) -> &[MemberPlan] {
        &self.members
    }
    pub fn rendezvous_port(&self) -> u16 {
        self.rendezvous_port
    }
}

/// SPEC §13.1: identity is per lifecycle step, not merely per operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandIdentity {
    pub controller_id: String,
    pub member: MemberKey,
    pub deployment_id: String,
    pub operation_id: String,
    pub command_id: String,
    pub step_id: String,
    pub generation: i64,
    pub revision: i64,
    pub deadline_ms: i64,
    pub payload_digest: [u8; 32],
    pub expected_state: String,
    pub profile_fingerprint: String,
    /// ADR 0013 §5: the deployment instance this command belongs to. A host
    /// fences commands per (deployment, instance), so two instances of one
    /// deployment on one host never fence each other. Zero for instance 0 and
    /// for every command written before instances existed.
    pub instance_index: u32,
}
/// ADR 0013 §2: a deployment has at most 64 instances, indexed `0..64`.
pub const MAX_INSTANCE_INDEX: u32 = 63;
impl CommandIdentity {
    pub fn validate(&self) -> Result<(), GroupIdentityError> {
        if [
            &self.controller_id,
            &self.member.host_id,
            &self.member.member_id,
            &self.deployment_id,
            &self.operation_id,
            &self.command_id,
            &self.step_id,
            &self.expected_state,
            &self.profile_fingerprint,
        ]
        .iter()
        .any(|s| s.trim().is_empty() || s.len() > 256)
            || self.generation <= 0
            || self.revision <= 0
            || self.deadline_ms <= 0
            || self.instance_index > MAX_INSTANCE_INDEX
        {
            return Err(GroupIdentityError);
        }
        Ok(())
    }
}

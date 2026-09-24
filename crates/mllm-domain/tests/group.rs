use mllm_domain::completion::ProcessIdentity;
use mllm_domain::group::{verify_group_processes, MemberKey, MemberProcesses};
fn member(host: &str) -> MemberProcesses {
    MemberProcesses {
        member: MemberKey {
            host_id: host.into(),
            member_id: "rank".into(),
        },
        processes: vec![ProcessIdentity {
            role: "worker-0".into(),
            pid: 41,
            boot_id: format!("boot-{host}"),
            start_ticks: 10,
        }],
    }
}
// T24: numeric PIDs are not globally unique across group hosts.
#[test]
fn matching_pids_on_distinct_hosts_are_distinct() {
    let group = vec![member("one"), member("two")];
    assert!(verify_group_processes(&group, &group).is_ok());
}
// T24: uncertain or replaced workers retain accounting.
#[test]
fn absent_and_reused_processes_cannot_settle() {
    let group = vec![member("one"), member("two")];
    assert!(verify_group_processes(&group, &group[..1]).is_err());
    let mut replaced = group.clone();
    replaced[1].processes[0].start_ticks += 1;
    assert!(verify_group_processes(&group, &replaced).is_err());
    let duplicate = vec![member("one"), member("one")];
    assert!(verify_group_processes(&duplicate, &duplicate).is_err());
}

fn plan_member(host: &str, rank: u32) -> mllm_domain::group::MemberPlan {
    mllm_domain::group::MemberPlan {
        member: MemberKey {
            host_id: host.into(),
            member_id: format!("rank-{rank}"),
        },
        rank,
        profile_name: "sglang".into(),
        profile_fingerprint: "pinned".into(),
        checkpoint_fingerprint: "sha256:checkpoint_fingerprint".into(),
        devices: vec!["gpu0".into()],
        peer_address: format!("10.0.0.{}", rank + 1).parse().unwrap(),
        service_port: 30000,
    }
}
// T24: local device labels may repeat across hosts, never across ranks on one host.
#[test]
fn plan_rejects_duplicate_ranks_hosts_and_unsupported_topology() {
    use mllm_domain::group::GroupPlan;
    let members = vec![plan_member("one", 0), plan_member("two", 1)];
    assert!(GroupPlan::two_host(members.clone(), 29500).is_ok());
    let mut invalid = members.clone();
    invalid[1].rank = 0;
    assert!(GroupPlan::two_host(invalid, 29500).is_err());
    let mut invalid = members.clone();
    invalid[1].member.host_id = "one".into();
    assert!(GroupPlan::two_host(invalid, 29500).is_err());
    let mut invalid = members.clone();
    invalid[1].devices.push("gpu0".into());
    assert!(GroupPlan::two_host(invalid, 29500).is_err());
    assert!(GroupPlan::two_host(members[..1].to_vec(), 29500).is_err());
}

// T24: one host cannot contribute evidence from two boots to the same group.
#[test]
fn host_evidence_cannot_mix_boots_or_duplicate_local_pids() {
    let first = member("one");
    let mut second = first.clone();
    second.member.member_id = "second".into();
    second.processes[0].pid += 1;
    let valid = vec![first.clone(), second.clone()];
    assert!(verify_group_processes(&valid, &valid).is_ok());
    second.processes[0].boot_id = "another-boot".into();
    let mixed = vec![first.clone(), second.clone()];
    assert!(verify_group_processes(&mixed, &mixed).is_err());
    second.processes[0].boot_id = first.processes[0].boot_id.clone();
    second.processes[0].pid = first.processes[0].pid;
    let duplicate = vec![first, second];
    assert!(verify_group_processes(&duplicate, &duplicate).is_err());
}

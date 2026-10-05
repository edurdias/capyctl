use capyctl_domain::completion::ProcessIdentity;
use capyctl_domain::group::*;
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

fn plan_member(host: &str, rank: u32, engine: GroupEngine) -> MemberPlan {
    MemberPlan {
        member: MemberKey {
            host_id: host.into(),
            member_id: member_id(rank),
        },
        rank,
        role: if rank == 0 {
            MemberRole::Head
        } else {
            MemberRole::Worker
        },
        profile_name: "sglang".into(),
        profile_fingerprint: "pinned".into(),
        checkpoint_fingerprint: "sha256:c".into(),
        model_path: "/models/m".into(),
        devices: vec!["gpu0".into()],
        peer_address: format!("192.0.2.{}", rank + 10).parse().unwrap(),
        service_port: (rank == 0).then_some(8100),
        worker_port: (rank > 0 && engine == GroupEngine::Sglang).then_some(8101),
    }
}
fn topo(tp: u32, pp: u32) -> GroupTopology {
    GroupTopology {
        tensor_parallel: tp,
        pipeline_parallel: pp,
        local_ranks: 1,
    }
}
fn members(n: u32, engine: GroupEngine) -> Vec<MemberPlan> {
    (0..n)
        .map(|r| plan_member(&format!("h{r}"), r, engine))
        .collect()
}

// T27: N-member plans validate in rank order with one head.
#[test]
fn four_member_plan_is_valid() {
    let plan = GroupPlan::new(
        GroupEngine::Vllm,
        members(4, GroupEngine::Vllm),
        topo(2, 2),
        25000,
        7,
    )
    .unwrap();
    assert_eq!(plan.head().member.host_id, "h0");
    assert_eq!(plan.generation(), 7);
    assert_eq!(plan.members()[3].member.member_id, "worker-3");
    assert_eq!(plan.engine().as_str(), "vllm");
    assert_eq!(plan.topology(), topo(2, 2));
    assert_eq!(plan.rendezvous_port(), 25000);
    assert_eq!(GroupEngine::Sglang.as_str(), "sglang");
    assert_eq!(GroupEngine::Tensorfold.as_str(), "tensorfold");
}

// T27: every shape violation is refused.
#[test]
fn invalid_plans_are_refused() {
    let e = GroupEngine::Vllm;
    let base = members(2, e);
    let mutate = |f: &dyn Fn(&mut Vec<MemberPlan>)| {
        let mut m = base.clone();
        f(&mut m);
        GroupPlan::new(e, m, topo(2, 1), 25000, 1)
    };
    assert!(mutate(&|_| {}).is_ok());
    assert!(mutate(&|m| m[1].member.host_id = "h0".into()).is_err());
    assert!(mutate(&|m| m.swap(0, 1)).is_err());
    assert!(mutate(&|m| m[1].role = MemberRole::Head).is_err());
    assert!(mutate(&|m| m[1].service_port = Some(8101)).is_err());
    assert!(mutate(&|m| m[0].service_port = None).is_err());
    assert!(mutate(&|m| m[1].worker_port = Some(8101)).is_err()); // vLLM workers listen on nothing
    assert!(mutate(&|m| m[1].profile_fingerprint = "other".into()).is_err());
    assert!(mutate(&|m| m[1].checkpoint_fingerprint = "sha256:d".into()).is_err());
    assert!(mutate(&|m| m[1].peer_address = m[0].peer_address).is_err());
    assert!(mutate(&|m| m[1].peer_address = "127.0.0.1".parse().unwrap()).is_err());
    assert!(mutate(&|m| m[1].model_path.clear()).is_err());
    assert!(mutate(&|m| m[1].devices.push("gpu1".into())).is_err());
    assert!(GroupPlan::new(e, base.clone(), topo(4, 1), 25000, 1).is_err());
    assert!(GroupPlan::new(e, base.clone(), topo(2, 1), 0, 1).is_err());
    assert!(GroupPlan::new(e, base.clone(), topo(2, 1), 25000, 0).is_err());
    assert!(GroupPlan::new(e, base[..1].to_vec(), topo(1, 1), 25000, 1).is_err());
}

// T22: a SGLang worker has a loopback port; TensorFold is two members only.
#[test]
fn engine_specific_member_rules() {
    let s = GroupEngine::Sglang;
    assert!(GroupPlan::new(s, members(2, s), topo(2, 1), 25000, 1).is_ok());
    let mut no_port = members(2, s);
    no_port[1].worker_port = None;
    assert!(GroupPlan::new(s, no_port, topo(2, 1), 25000, 1).is_err());
    let t = GroupEngine::Tensorfold;
    assert!(GroupPlan::new(t, members(2, t), topo(2, 1), 25000, 1).is_ok());
    assert!(GroupPlan::new(t, members(4, t), topo(4, 1), 25000, 1).is_err());
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

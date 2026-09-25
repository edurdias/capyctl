# Multi-host Engine Groups Implementation Plan

**Execution:** implement task by task in order; each task ends green on its own tests and is committed before the next starts. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run one engine instance across N named hosts (tensor parallel × pipeline parallel, one device per host in this version) through mllm, starting with vLLM native multi-node, with per-rank reservations, concurrent fan-out launch, head-only readiness, group-wide deep park with per-rank evidence, whole-group stop on any rank failure, and a live reproduction of the published two-host Qwen3.8-Flash-Next run through the router.

**Architecture:** A deployment gains `topology` and an exact, rank-ordered `placement.hosts`. The server writes a durable N-member group plan and reserves every member (one resource owner per member, on its own host) plus a rendezvous port from the head's declared range in one store transaction. Each host prepares (host checks, digest, port), then every host launches its member concurrently; the head runs the API server on loopback and workers run headless. The head's agent is the lead for readiness and for sleep/wake; worker agents supply process identities, residency and stop evidence. Any member failure stops the whole group, and each member settles only on its own host's evidence. All protocol additions sit behind a new ADR 0017 capability, `engine_groups`.

**Tech Stack:** Rust 2021 workspace (tokio, tonic/prost, axum, rusqlite, clap, serde/serde_json, saphyr strict YAML), Python 3 runtime helpers under `runtime/` (unittest), bash live harness under `scripts/live/matrix/`.

**Spec:** `docs/specs/2026-09-25-multi-host-groups-design.md`. Read it with this plan. Governing documents: `docs/SPEC.md` §6, §7, §9.1, §11, §13, §16.4, §20; ADR 0007, 0011, 0012, 0013, 0014, 0016, 0017, 0018; `AGENTS.md`.

## Owner decisions

The thirteen binding decisions of 2026-09-25 are listed in the spec ("Decisions"). The six "Owner check" items at the top of the spec must be answered before Task 1. This plan assumes each recommendation is accepted; where an answer differs, the named task changes:

1. Recipe environment: host-approved `security.approved_env` (Task 4).
2. mllm renders `--master-addr` and `VLLM_HOST_IP`, no `NCCL_*`/`GLOO_*` (Tasks 9 and 10).
3. Host checks warn by default; `groups.require_rdma: true` refuses (Tasks 3 and 8).
4. Group eviction is all-or-nothing across named hosts (Task 15).
5. `instances: 1`, one device per host (Task 2).
6. Post-wake canary for groups (Task 16).

## Global Constraints

- This milestone follows 0.1.0 and must not block it: work on a branch cut after the 0.1.0 tag; a deployment without `topology` (or with world size 1) behaves byte-identically to 0.1.0, including its command identity, effective configuration and stored digests.
- Cite the governing requirement inline where behaviour is spec-driven, e.g. `// ADR 0020 §4: ...`, `// SPEC §11: ...` (AGENTS.md "Code conventions").
- Tag every new test with its acceptance-matrix ID. IDs used: T03, T14 (configuration), T15, T16, T20, T27, T30, T31, T32 (lifecycle and accounting), T21, T37 (security), T34 (capability gating).
- Uncertainty keeps accounting: a member is released only on its own host's gone-evidence; an unreachable host's member stays charged and uncertain; the rendezvous port is released only after every member settles.
- Additive protocol only: no field renumbered, `PROTOCOL_VERSION` stays `"2"`, command encoding version stays `"1"`. New capability: `engine_groups`.
- mllm renders no `NCCL_*` or `GLOO_*` variable for a group and inherits none; it renders `--master-addr` (head peer address) and `VLLM_HOST_IP` (own peer address). The API server and every control endpoint stay on loopback with the per-launch keys and the key-guard (ADR 0012).
- mllm never changes sysctls, limits, device permissions or firewalls; it reads them.
- Rendezvous port range default: `25000`–`25099` inclusive.
- Closed codes (spec §15): `group_placement_required`, `group_topology_invalid`, `group_shape_unsupported`, `group_instances_unsupported`, `group_engine_unsupported:<engine>`, `group_profile_mismatch`, `group_checkpoint_mismatch`, `group_model_path_mismatch`, `peer_address_missing`, `peer_address_not_local`, `rendezvous_ports_exhausted`, `host_tuning_warning:<item>`, `host_tuning_missing:<item>`, `rendezvous_port_in_use:<port>`, `service_port_in_use:<port>`, `group_member_failed`, `group_member_uncertain`, `group_wake_mismatch`, `host_capability_missing:engine_groups`. Items: `compaction`, `memlock`, `infiniband`. Exits: 2 for deploy-time shape errors, 5 for `group_shape_unsupported`, `group_instances_unsupported`, `group_engine_unsupported`, 4 for `rendezvous_ports_exhausted`. No new exit number.
- In tracked files, commits and the PR: no machine names, addresses, or home paths. The hosts are "host A" (head) and "host B" (worker); the link is "the direct link". Addresses in examples and tests use the documentation ranges `192.0.2.0/24` and `198.51.100.0/24`. Live addresses come only from the untracked `scripts/live/matrix/hosts.local.env`.
- CPU and Fake-engine tests are not qualification; the live rows MH1–MH9 are. Say so in every status claim.
- Verification before every commit: `cargo fmt --all --check`; the core suite `cargo test -p mllm-adapters -p mllm-store -p mllm-controller -p mllm-management -p harness --all-targets --no-fail-fast --locked -- --test-threads=4`; `cargo test --workspace --all-targets --locked`; `cargo clippy --workspace --all-targets --locked -- -D warnings`; Python helpers: `python3 -m unittest discover -s runtime/tests`.

## Review Focus

1. **A host listed twice under different spellings, or a host renamed after deploy.** Placement uses host ids; a re-enrolled or recovered host (ADR 0016) must keep its member's owner and never produce two members on one physical host. Pinned in Task 2 (duplicate refusal after normalization) and Task 7 (reservation refuses two members with one host id).
2. **The head's rendezvous port is already taken by something outside mllm** (a stale engine from before a crash, another tool). Prepare must refuse with the port named and release every reservation, not launch and hang for the whole initialization timeout. Pinned in Task 8.
3. **A worker host reconnects mid-launch with a restarted agent and an empty journal.** Its member must not be declared gone because the journal is empty; it settles only on recorded identities (ADR 0016), and the group must not relaunch while it is unsettled. Pinned in Task 14.
4. **Park succeeds on the head's HTTP call but one rank never frees memory** (the documented TP>1 sleep bug class). The group must keep that member's full charge and stop the group, not report PARKED. Pinned in Task 16.
5. **Two group deployments sharing host B activate at once.** Neither may hold host B's share while waiting for host A; both reservations must be all-or-nothing and one must be refused or queued cleanly. Pinned in Task 7.

---

## File map

| File | Responsibility | Tasks |
|---|---|---|
| `docs/design/adr/0020-multi-host-engine-groups.md` (new), `docs/design/adr/0012-deep-park-default-on.md`, `docs/SPEC.md` | decision record and amendments | 1 |
| `crates/mllm-config/src/topology.rs` (new), `crates/mllm-config/src/instances.rs`, `crates/mllm-config/src/schema.rs` | `topology`, group placement rules, refusal codes | 2 |
| `crates/mllm-config/src/groups_policy.rs` (new), `crates/mllm-config/src/schema.rs` | host `resource_policy.groups` | 3 |
| `crates/mllm-config/src/engine_policy.rs`, `crates/mllm-config/src/schema.rs` | `security.approved_env` | 4 |
| `crates/mllm-domain/src/group.rs` | N-member `GroupPlan`, `MemberRole` | 5 |
| `crates/mllm-protocol/proto/mllm/management/v1/management.proto`, `crates/mllm-protocol/src/execution.rs`, `crates/mllm-protocol/src/capabilities.rs` | wire fields, `engine_groups` | 6 |
| `crates/mllm-store/src/groups.rs` (new), `crates/mllm-store/src/instances.rs`, `crates/mllm-store/src/migrations.rs`, `crates/mllm-store/src/resource_ledger.rs` | group plans, member owners, all-or-nothing reservation, ports | 7 |
| `crates/mllm-agent/src/host_checks.rs` (new), `crates/mllm-agent/src/native_execution.rs` | host tuning checks, peer address, Prepare | 8 |
| `crates/mllm-adapters/src/vllm/args.rs`, `crates/mllm-adapters/src/vllm/frozen.rs` | multi-node rendering | 9 |
| `runtime/vllm_entry.py`, `runtime/loopback_rendezvous.py`, `runtime/tests/test_vllm_entry.py` | group mode in the protected entry | 10 |
| `crates/mllm-agent/src/native_execution.rs`, `crates/mllm-agent/src/native_execution/vllm.rs`, `crates/mllm-agent/src/journal.rs` | member launch, identities, member terminate | 11 |
| `crates/mllm-testkit/src/fake_group.rs` (new) | multi-member Fake engine | 12 |
| `crates/mllm-controller/src/group_sources.rs` (new) | parallel weights, digest agreement | 13 |
| `crates/mllm-controller/src/group_activation.rs` (new), `crates/mllm-controller/src/remote_execution.rs`, `crates/mllm-controller/src/remote_readiness.rs` | reserve, prepare, fan-out, head readiness | 13 |
| `crates/mllm-controller/src/group_settlement.rs` (new) | stop, failure, compensation, uncertainty, recovery | 14 |
| `crates/mllm-controller/src/switching.rs`, `crates/mllm-scheduler/src/placement.rs` | group eviction across named hosts | 15 |
| `crates/mllm-controller/src/group_residency.rs` (new) | group park/wake, per-rank evidence, canary | 16 |
| `crates/mllm-cli/src/output.rs`, `crates/mllm-management/src/status.rs` | status rows, codes, exits | 17 |
| `scripts/live/matrix/rows/MH*.sh`, `scripts/live/matrix/rdma_counters.py` (new), `docs/runbooks/f2-current-status.md` | live rows | 18, 19 |
| `crates/mllm-adapters/src/sglang/*`, `runtime/sglang_server_args.py` | SGLang group, restart-only | 20 |

Task 13 is split in two (13a weights, 13b activation) because a reviewer can accept one without the other.

---

### Task 1: ADR 0020, the ADR 0012 amendment and SPEC amendments

**Files:**
- Create: `docs/design/adr/0020-multi-host-engine-groups.md`
- Modify: `docs/design/adr/0012-deep-park-default-on.md` (append "Amendment 2026-09-25: peer exposure of engine groups")
- Modify: `docs/design/adr/0013-deployment-instances-and-placement.md` (decision 1: point the multi-host refusal at ADR 0020)
- Modify: `docs/SPEC.md` §11, §16.4 (replace `placement.head` with "first host is the head"; delete "Multi-host group placement is not yet specified"), §20 (add live rows MH1–MH8 below the table)
- Modify: `docs/specs/2026-09-25-discrete-gpu-and-network-endpoint-design.md` §7 (note that `tensor_parallel > 1` with a multi-host topology is governed by ADR 0020)
- Modify: `AGENTS.md` and its mirrored working-agreement file, hard constraints (one bullet: group peer transport is unauthenticated and on every interface during a run, per ADR 0012 amendment)

**Interfaces:**
- Produces: the section numbers code cites: ADR 0020 §1 vocabulary, §2 configuration, §3 group plan, §4 reservations, §5 preparation and weights, §6 fan-out, §7 readiness, §8 rendering, §9 stop and failure, §10 park and wake, §11 host checks, §12 ports and exposure, §13 capability. They mirror the spec's §1–§13 one to one.

- [ ] **Step 1: Write ADR 0020.** Status "Accepted (owner decisions 2026-09-25)". Sections: Context (spec "Problem"), Decision (the thirteen owner decisions verbatim, then §1–§13 condensed from the spec, each a short paragraph), Honest scope ("CPU and Fake-engine tests are not qualification; MH1–MH8 are"), Consequences (ADR 0013 refusal lifted only for named-host groups; `multi_gpu_unsupported` unchanged for one host; profile `approved_env`; new capability).

- [ ] **Step 2: Write the ADR 0012 amendment.** Text to append:

```markdown
## Amendment 2026-09-25 — peer exposure of engine groups (ADR 0020)

Owner decision 3 of 2026-09-25: during a multi-host group run, the engine's
rendezvous store, the head's broadcast queue and NCCL open unauthenticated
listeners on every interface of every member host, including any wireless or
overlay network. mllm renders the direct-link addresses where the engine takes
them (`--master-addr`, `VLLM_HOST_IP`), performs no firewall check, and leaves
the network to the operator. This is a recorded known risk, not a protection.
Every protection in decision 5 above is unchanged: the API server and every
control endpoint stay on loopback with the per-launch keys and the guard, and
no control path reaches host ingress or the router. Status marks a group
instance `peer_transport: unauthenticated`.
```

- [ ] **Step 3: Amend SPEC §11, §16.4, §20 and the discrete-GPU design as listed above.** In §20, add below the matrix: "Multi-host live rows MH1–MH8 (ADR 0020) supplement T16, T20, T30–T32 on two hosts."

- [ ] **Step 4: Check links and names.** Run: `grep -n "0020" docs/SPEC.md docs/design/adr/*.md AGENTS.md` — expected: every amended file cites ADR 0020. Run: `git diff --stat` — docs only.

- [ ] **Step 5: Commit**

```bash
git add -u && git add docs/design/adr/0020-multi-host-engine-groups.md
git commit -m "docs: ADR 0020 multi-host engine groups and ADR 0012 peer-exposure amendment"
```

---

### Task 2: Deployment `topology` and group placement rules

**Files:**
- Create: `crates/mllm-config/src/topology.rs`
- Modify: `crates/mllm-config/src/lib.rs` (export), `crates/mllm-config/src/instances.rs` (`InstanceSpec` gains `topology`), `crates/mllm-config/src/schema.rs` (deployment kind accepts `topology: {tensor_parallel, pipeline_parallel}`)
- Test: `crates/mllm-config/tests/topology.rs`

**Interfaces:**
- Consumes: `parse_instance_spec(&Value) -> Result<InstanceSpec, ConfigError>` (existing).
- Produces:
  - `pub struct Topology { pub tensor_parallel: u32, pub pipeline_parallel: u32 }` with `fn world_size(&self) -> u32`.
  - `pub struct GroupShape { pub hosts: Vec<String>, pub topology: Topology, pub local_ranks: u32 }` with `fn head(&self) -> &str`.
  - `pub enum GroupRefusal { PlacementRequired, TopologyInvalid, ShapeUnsupported, InstancesUnsupported }` with `fn code(&self) -> &'static str` returning the closed codes.
  - `pub fn parse_group_shape(deployment: &Value, spec: &InstanceSpec) -> Result<Option<GroupShape>, ConfigError>` — `None` for a single-host deployment. A refusal is `ConfigError::new(UnsupportedCombination, path, format!("{code}: {detail}"))`.
  - `InstanceSpec.group: Option<GroupShape>` with `#[serde(default, skip_serializing_if = "Option::is_none")]`, so a single-host command identity is unchanged.

- [ ] **Step 1: Write the failing tests**

```rust
// crates/mllm-config/tests/topology.rs
use mllm_config::instances::parse_instance_spec;
use serde_json::json;

fn doc(extra: serde_json::Value) -> serde_json::Value {
    let mut d = json!({"schema_version": 1, "kind": "deployment", "name": "g"});
    d.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    d
}
fn code(err: mllm_config::ConfigError) -> String {
    err.to_string()
}

// T03: a two-host TP2 group parses; the first host is the head.
#[test]
fn two_host_tp2_parses_with_head_first() {
    let spec = parse_instance_spec(&doc(json!({
        "topology": {"tensor_parallel": 2, "pipeline_parallel": 1},
        "placement": {"hosts": ["host-b", "host-a"]}
    }))).unwrap();
    let group = spec.group.unwrap();
    assert_eq!(group.head(), "host-b");
    assert_eq!(group.topology.world_size(), 2);
    assert_eq!(group.local_ranks, 1);
}

// T03: TP2 x PP2 over four hosts is one rank per host.
#[test]
fn tp2_pp2_over_four_hosts_parses() {
    let spec = parse_instance_spec(&doc(json!({
        "topology": {"tensor_parallel": 2, "pipeline_parallel": 2},
        "placement": {"hosts": ["a", "b", "c", "d"]}
    }))).unwrap();
    assert_eq!(spec.group.unwrap().topology.world_size(), 4);
}

// T03, T14: every deploy-time refusal carries its closed code.
#[test]
fn group_refusals_carry_codes() {
    let cases = [
        (json!({"topology": {"tensor_parallel": 2}}), "group_placement_required"),
        (json!({"topology": {"tensor_parallel": 2}, "host": "a"}), "group_placement_required"),
        (json!({"topology": {"tensor_parallel": 2},
                "placement": {"hosts": ["a", "b"], "strategy": "spread"}}), "group_placement_required"),
        (json!({"topology": {"tensor_parallel": 3},
                "placement": {"hosts": ["a", "b"]}}), "group_topology_invalid"),
        (json!({"topology": {"tensor_parallel": 4},
                "placement": {"hosts": ["a", "b"]}}), "group_shape_unsupported"),
        (json!({"topology": {"tensor_parallel": 2}, "instances": 2,
                "placement": {"hosts": ["a", "b"]}}), "group_instances_unsupported"),
        (json!({"topology": {"tensor_parallel": 0},
                "placement": {"hosts": ["a", "b"]}}), "group_topology_invalid"),
    ];
    for (extra, expected) in cases {
        let err = parse_instance_spec(&doc(extra.clone())).unwrap_err();
        assert!(code(err).contains(expected), "{extra} -> {expected}");
    }
}

// T03: world size 1 is a single-host deployment with an unchanged identity.
#[test]
fn world_size_one_is_single_host_and_identity_is_unchanged() {
    let plain = parse_instance_spec(&doc(json!({}))).unwrap();
    let tp1 = parse_instance_spec(&doc(json!({
        "topology": {"tensor_parallel": 1, "pipeline_parallel": 1}
    }))).unwrap();
    assert!(tp1.group.is_none());
    assert_eq!(plain.command_identity(), tp1.command_identity());
}

// Review Focus 1: a host repeated with surrounding whitespace is still a repeat.
#[test]
fn repeated_host_after_trim_is_refused() {
    let err = parse_instance_spec(&doc(json!({
        "topology": {"tensor_parallel": 2},
        "placement": {"hosts": ["host-a", " host-a"]}
    }))).unwrap_err();
    assert!(code(err).contains("placement.hosts"));
}
```

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p mllm-config --test topology` — expected: compile error, `group` field and `head` not found.

- [ ] **Step 3: Implement `topology.rs`**

```rust
//! ADR 0020 §2: a deployment's multi-host topology. A world size above one
//! requires an exact, rank-ordered host list; the first host is the head.
use crate::{instances::InstanceSpec, ConfigError, ConfigErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Topology {
    pub tensor_parallel: u32,
    pub pipeline_parallel: u32,
}
impl Topology {
    pub fn world_size(&self) -> u32 {
        self.tensor_parallel * self.pipeline_parallel
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupShape {
    pub hosts: Vec<String>,
    pub topology: Topology,
    pub local_ranks: u32,
}
impl GroupShape {
    pub fn head(&self) -> &str {
        &self.hosts[0]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupRefusal {
    PlacementRequired,
    TopologyInvalid,
    ShapeUnsupported,
    InstancesUnsupported,
}
impl GroupRefusal {
    pub fn code(&self) -> &'static str {
        match self {
            Self::PlacementRequired => "group_placement_required",
            Self::TopologyInvalid => "group_topology_invalid",
            Self::ShapeUnsupported => "group_shape_unsupported",
            Self::InstancesUnsupported => "group_instances_unsupported",
        }
    }
    fn at(self, path: &str, detail: &str) -> ConfigError {
        ConfigError::new(
            ConfigErrorCode::UnsupportedCombination,
            path,
            format!("{}: {detail}", self.code()),
        )
    }
}

/// Bound on one topology dimension: far above any hardware in hand, low enough
/// that `world_size` never overflows.
const MAX_DIMENSION: u64 = 64;

fn dimension(raw: &Value, key: &str) -> Result<u32, ConfigError> {
    match raw.get(key) {
        None | Some(Value::Null) => Ok(1),
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_DIMENSION).contains(n))
            .map(|n| n as u32)
            .ok_or_else(|| {
                GroupRefusal::TopologyInvalid.at(&format!("topology.{key}"), "must be 1..=64")
            }),
    }
}

pub fn parse_group_shape(
    deployment: &Value,
    spec: &InstanceSpec,
) -> Result<Option<GroupShape>, ConfigError> {
    let Some(raw) = deployment.get("topology").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let object = raw
        .as_object()
        .ok_or_else(|| GroupRefusal::TopologyInvalid.at("topology", "must be a mapping"))?;
    for key in object.keys() {
        if !matches!(key.as_str(), "tensor_parallel" | "pipeline_parallel") {
            return Err(ConfigError::new(
                ConfigErrorCode::UnknownField,
                format!("topology.{key}"),
                "unknown topology field",
            ));
        }
    }
    let topology = Topology {
        tensor_parallel: dimension(raw, "tensor_parallel")?,
        pipeline_parallel: dimension(raw, "pipeline_parallel")?,
    };
    if topology.world_size() == 1 {
        return Ok(None);
    }
    // ADR 0020 §2 (owner decision 8): named hosts only, head first.
    let placement = &deployment["placement"];
    if deployment.get("host").is_some_and(|v| !v.is_null())
        || ["selector", "strategy", "max_per_host"]
            .iter()
            .any(|k| placement.get(*k).is_some_and(|v| !v.is_null()))
    {
        return Err(GroupRefusal::PlacementRequired
            .at("placement", "a group lists exact hosts only, head first"));
    }
    let Some(hosts) = spec.placement.hosts.clone().filter(|h| h.len() >= 2) else {
        return Err(GroupRefusal::PlacementRequired
            .at("placement.hosts", "a group names at least two hosts, head first"));
    };
    if spec.instances != 1 {
        return Err(GroupRefusal::InstancesUnsupported
            .at("instances", "a group deployment has one instance in this version"));
    }
    let world = topology.world_size();
    let n = hosts.len() as u32;
    if world % n != 0 {
        return Err(GroupRefusal::TopologyInvalid
            .at("topology", "the host count must divide tensor_parallel x pipeline_parallel"));
    }
    let local_ranks = world / n;
    if local_ranks != 1 {
        return Err(GroupRefusal::ShapeUnsupported
            .at("topology", "one device per host in this version"));
    }
    Ok(Some(GroupShape { hosts, topology, local_ranks }))
}
```

In `instances.rs`: trim host names before the duplicate check (`h.trim()` in the `names` map; refuse an entry whose trimmed form differs from the original with "host names have no surrounding space"), add `pub group: Option<GroupShape>` (serde default, skip when `None`) to `InstanceSpec` and `Default`, and at the end of `parse_instance_spec` set `spec.group = crate::topology::parse_group_shape(deployment, &spec)?;` before the `placeable_on` check, skipping `placeable_on` when `group` is `Some`. In `schema.rs`, add `("topology", FieldSpec::Struct(&[("tensor_parallel", SCALAR), ("pipeline_parallel", SCALAR)]))` to the deployment kind.

- [ ] **Step 4: Run the tests.** Run: `cargo test -p mllm-config` — expected: PASS, including every existing instances test.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-config
git commit -m "feat(config): deployment topology and named-host group placement (ADR 0020 §2)"
```

---

### Task 3: Host `groups` policy

**Files:**
- Create: `crates/mllm-config/src/groups_policy.rs`
- Modify: `crates/mllm-config/src/schema.rs` (`F2_RESOURCE_POLICY` gains `groups`), `crates/mllm-config/src/lib.rs`
- Test: `crates/mllm-config/tests/groups_policy.rs`

**Interfaces:**
- Produces:
  - `pub struct GroupsPolicy { pub peer_address: Option<IpAddr>, pub rendezvous_ports: RangeInclusive<u16>, pub require_rdma: bool }` (`Default`: no address, `25000..=25099`, false).
  - `pub fn host_groups_policy(host: &Value) -> Result<GroupsPolicy, ConfigError>`.
  - `pub const DEFAULT_RENDEZVOUS_PORTS: RangeInclusive<u16> = 25000..=25099;`

- [ ] **Step 1: Write the failing tests**

```rust
use mllm_config::groups_policy::{host_groups_policy, DEFAULT_RENDEZVOUS_PORTS};
use serde_json::json;

// T03: an absent block is the default and names no peer address.
#[test]
fn absent_block_is_default() {
    let p = host_groups_policy(&json!({"resource_policy": {}})).unwrap();
    assert_eq!(p.peer_address, None);
    assert_eq!(p.rendezvous_ports, DEFAULT_RENDEZVOUS_PORTS);
    assert!(!p.require_rdma);
}

// T03: a declared block parses.
#[test]
fn declared_block_parses() {
    let p = host_groups_policy(&json!({"resource_policy": {"groups": {
        "peer_address": "192.0.2.10",
        "rendezvous_port_range": {"start": 26000, "end": 26009},
        "require_rdma": true}}})).unwrap();
    assert_eq!(p.peer_address, Some("192.0.2.10".parse().unwrap()));
    assert_eq!(p.rendezvous_ports, 26000..=26009);
    assert!(p.require_rdma);
}

// T03, T37: loopback, unspecified, multicast, reversed and privileged ranges are refused.
#[test]
fn unsafe_values_are_refused() {
    for groups in [
        json!({"peer_address": "127.0.0.1"}),
        json!({"peer_address": "0.0.0.0"}),
        json!({"peer_address": "224.0.0.1"}),
        json!({"peer_address": "not-an-ip"}),
        json!({"rendezvous_port_range": {"start": 26009, "end": 26000}}),
        json!({"rendezvous_port_range": {"start": 80, "end": 90}}),
        json!({"unknown": 1}),
    ] {
        let host = json!({"resource_policy": {"groups": groups.clone()}});
        assert!(host_groups_policy(&host).is_err(), "{groups}");
    }
}
```

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p mllm-config --test groups_policy` — expected: unresolved module.

- [ ] **Step 3: Implement.** Parse `resource_policy.groups` strictly (unknown field → `UnknownField`); `peer_address` via `str::parse::<IpAddr>`, refused when loopback, unspecified or multicast (`UnsupportedCombination`, detail names the address rule); port range requires `1024 <= start <= end`; `require_rdma` boolean. Add the schema entry:

```rust
(
    "groups",
    FieldSpec::Struct(&[
        ("peer_address", SCALAR),
        ("rendezvous_port_range", FieldSpec::Struct(&[("start", SCALAR), ("end", SCALAR)])),
        ("require_rdma", SCALAR),
    ]),
),
```

A host document without `groups` must serialize and digest exactly as before; add to the test file:

```rust
// T39: a host without the block keeps its stored policy digest.
#[test]
fn host_without_groups_keeps_its_digest() {
    let host = json!({"resource_policy": {"max_parked": 2}});
    let before = mllm_config::effective::host_policy_digest(&host).unwrap();
    let _ = host_groups_policy(&host).unwrap();
    assert_eq!(before, mllm_config::effective::host_policy_digest(&host).unwrap());
}
```

If `host_policy_digest` has a different name in `effective.rs`, use the function the store calls to compute the published policy digest (search `digest` in `crates/mllm-config/src/effective.rs`) and keep the assertion.

- [ ] **Step 4: Run the tests.** Run: `cargo test -p mllm-config` — expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-config
git commit -m "feat(config): host groups policy: peer address, rendezvous range, require_rdma (ADR 0020 §2, §11)"
```

---

### Task 4: Host-approved recipe environment (`security.approved_env`)

**Files:**
- Modify: `crates/mllm-config/src/engine_policy.rs` (`validate_profile_env` takes the approved list), `crates/mllm-config/src/schema.rs` (`SECURITY` gains `approved_env`), `crates/mllm-config/src/effective/core.rs` (pass the list)
- Test: `crates/mllm-config/src/engine_policy.rs` unit tests

**Interfaces:**
- Produces: `pub fn validate_profile_env(env: &BTreeMap<String, String>, approved: &[String]) -> Result<(), String>`; `pub const NEVER_APPROVABLE_PREFIXES: &[&str] = &["NCCL_", "GLOO_", "MASTER_", "MLLM_"];` and `pub const NEVER_APPROVABLE: &[&str] = &["VLLM_HOST_IP", "PATH", "LD_PRELOAD", "LD_LIBRARY_PATH", "PYTHONPATH", "CUDA_VISIBLE_DEVICES", "CUDA_HOME"];`

- [ ] **Step 1: Write the failing tests**

```rust
// T37: a profile may set a variable only when the host approved its name.
#[test]
fn approved_env_admits_only_listed_names() {
    let env = BTreeMap::from([("MBX_FUSED_DRAFT".to_owned(), "1".to_owned())]);
    assert!(validate_profile_env(&env, &[]).is_err());
    assert!(validate_profile_env(&env, &["MBX_FUSED_DRAFT".to_owned()]).is_ok());
}

// T21, T37: transport, rendezvous and loader variables can never be approved.
#[test]
fn transport_and_loader_names_are_never_approvable() {
    for name in ["NCCL_IB_HCA", "GLOO_SOCKET_IFNAME", "MASTER_ADDR", "VLLM_HOST_IP",
                 "LD_PRELOAD", "PATH", "MLLM_ENGINE_KEY"] {
        let env = BTreeMap::from([(name.to_owned(), "x".to_owned())]);
        assert!(validate_profile_env(&env, &[name.to_owned()]).is_err(), "{name}");
    }
}
```

Also add a host-document test in `crates/mllm-config/tests/effective.rs`: a profile with `security.approved_env: ["NCCL_DEBUG"]` fails resolution naming `security.approved_env`.

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p mllm-config engine_policy` — expected: arity error.

- [ ] **Step 3: Implement.** `validate_profile_env` admits a name when it is in `SAFE_ENV` or in `approved`; `approved` itself is validated at resolution (upper-case identifier, ≤ 64 entries, not in `NEVER_APPROVABLE`, no `NEVER_APPROVABLE_PREFIXES` prefix). `COUNT_ENV` still applies. Record `approved_env` in the effective profile so the recipe fingerprint changes only when the list is non-empty (`skip_serializing_if = "Vec::is_empty"`).

- [ ] **Step 4: Run the tests.** Run: `cargo test -p mllm-config` — expected: PASS; existing profiles' fingerprints unchanged.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-config
git commit -m "feat(config): host-approved recipe environment names, transport names never approvable (ADR 0020 owner check 1)"
```

---

### Task 5: N-member `GroupPlan`

**Files:**
- Modify: `crates/mllm-domain/src/group.rs`
- Test: `crates/mllm-domain/tests/group.rs`

**Interfaces:**
- Produces:
  - `pub enum MemberRole { Head, Worker }`.
  - `MemberPlan` gains `pub role: MemberRole`, `pub model_path: String`; `service_port: Option<u16>` (head `Some`, worker `None`).
  - `pub struct GroupTopology { pub tensor_parallel: u32, pub pipeline_parallel: u32, pub local_ranks: u32 }`.
  - `GroupPlan::new(members: Vec<MemberPlan>, topology: GroupTopology, rendezvous_port: u16, generation: i64) -> Result<GroupPlan, GroupIdentityError>`; accessors `members()`, `head()`, `topology()`, `rendezvous_port()`, `generation()`.
  - `pub fn member_id(rank: u32) -> String` → `"head"` for 0, `"worker-<r>"` otherwise.
  - `GroupPlan::two_host` is removed; its callers (protocol tests) move to `new`.

- [ ] **Step 1: Write the failing tests** (replace `plan_member` and the `two_host` test)

```rust
use mllm_domain::group::{member_id, GroupPlan, GroupTopology, MemberKey, MemberPlan, MemberRole};

fn plan_member(host: &str, rank: u32) -> MemberPlan {
    MemberPlan {
        member: MemberKey { host_id: host.into(), member_id: member_id(rank) },
        rank,
        role: if rank == 0 { MemberRole::Head } else { MemberRole::Worker },
        profile_name: "vllm-030".into(),
        profile_fingerprint: "pinned".into(),
        checkpoint_fingerprint: "sha256:c".into(),
        model_path: "/models/m".into(),
        devices: vec!["gpu0".into()],
        peer_address: format!("192.0.2.{}", rank + 10).parse().unwrap(),
        service_port: (rank == 0).then_some(30000),
    }
}
fn topo(tp: u32, pp: u32) -> GroupTopology {
    GroupTopology { tensor_parallel: tp, pipeline_parallel: pp, local_ranks: 1 }
}

// T27: N-member plans validate in rank order with one head.
#[test]
fn four_member_plan_is_valid() {
    let members = (0..4).map(|r| plan_member(&format!("h{r}"), r)).collect();
    let plan = GroupPlan::new(members, topo(2, 2), 25000, 7).unwrap();
    assert_eq!(plan.head().member.host_id, "h0");
    assert_eq!(plan.generation(), 7);
}

// T27: every shape violation is refused.
#[test]
fn invalid_plans_are_refused() {
    let base: Vec<_> = (0..2).map(|r| plan_member(&format!("h{r}"), r)).collect();
    let mutate = |f: &dyn Fn(&mut Vec<MemberPlan>)| {
        let mut m = base.clone();
        f(&mut m);
        GroupPlan::new(m, topo(2, 1), 25000, 1)
    };
    assert!(mutate(&|m| m[1].member.host_id = "h0".into()).is_err()); // one host twice
    assert!(mutate(&|m| m.swap(0, 1)).is_err()); // not in rank order
    assert!(mutate(&|m| m[1].role = MemberRole::Head).is_err()); // two heads
    assert!(mutate(&|m| m[1].service_port = Some(30001)).is_err()); // worker with a port
    assert!(mutate(&|m| m[0].service_port = None).is_err()); // head without a port
    assert!(mutate(&|m| m[1].profile_fingerprint = "other".into()).is_err());
    assert!(mutate(&|m| m[1].checkpoint_fingerprint = "sha256:d".into()).is_err());
    assert!(mutate(&|m| m[1].peer_address = m[0].peer_address).is_err());
    assert!(mutate(&|m| m[1].peer_address = "127.0.0.1".parse().unwrap()).is_err());
    assert!(mutate(&|m| m[1].model_path.clear()).is_err());
    assert!(GroupPlan::new(base.clone(), topo(4, 1), 25000, 1).is_err()); // world != N x local
    assert!(GroupPlan::new(base.clone(), topo(2, 1), 0, 1).is_err());
    assert!(GroupPlan::new(base.clone(), topo(2, 1), 25000, 0).is_err());
    assert!(GroupPlan::new(base[..1].to_vec(), topo(1, 1), 25000, 1).is_err()); // one member
}
```

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p mllm-domain --test group` — expected: compile errors.

- [ ] **Step 3: Implement `GroupPlan::new`.** Rules, all returning `GroupIdentityError`: `members.len() >= 2`; `rendezvous_port != 0`; `generation > 0`; `members[i].rank == i`; `role == Head` iff rank 0; `member.member_id == member_id(rank)`; distinct host ids; non-empty profile name, fingerprints, model path; `devices.len() == local_ranks as usize` with non-empty labels; head `service_port` is `Some(p)` with `p != 0`, workers `None`; peer addresses distinct and not unspecified, multicast or loopback; all profile and checkpoint fingerprints equal; `tensor_parallel * pipeline_parallel == members.len() as u32 * local_ranks`. Doc comment cites `// ADR 0020 §3`.

- [ ] **Step 4: Run the tests.** Run: `cargo test -p mllm-domain && cargo build --workspace --all-targets` — expected: PASS; fix compile sites that used `two_host` (protocol only).

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-domain crates/mllm-protocol
git commit -m "feat(domain): N-member group plan with head and headless workers (ADR 0020 §3)"
```

---

### Task 6: Wire fields and the `engine_groups` capability

**Files:**
- Modify: `crates/mllm-protocol/proto/mllm/management/v1/management.proto` (`GroupMemberPlan`, `GroupLaunchPlan`), `crates/mllm-protocol/src/execution.rs` (conversion), `crates/mllm-protocol/src/capabilities.rs`
- Modify: `crates/mllm-protocol/proto/...` `HostInventory` (or the inventory message the agent sends at Connect) gains `GroupInventory { string peer_address = 1; repeated string findings = 2; }`
- Test: `crates/mllm-protocol/tests/execution.rs`, `crates/mllm-protocol/src/capabilities.rs` tests

**Interfaces:**
- Consumes: `GroupPlan::new`, `MemberRole` (Task 5).
- Produces: `pub const ENGINE_GROUPS: &str = "engine_groups";` in `CATALOGUE` as `ServerToHost`; `MemberAction::Prepare(GroupPlan)` / `Launch(GroupPlan)` round-trip every new field; `pub fn group_refusal(host_capabilities: &BTreeSet<String>) -> Option<String>` returning `Some(missing(ENGINE_GROUPS))` when absent.

- [ ] **Step 1: Write the failing tests**

```rust
// T34: group actions round-trip every member field, and the digest binds them.
#[test]
fn group_launch_round_trips_new_fields() {
    let plan = sample_group_plan(); // head + worker, as Task 5's plan_member
    let action = MemberAction::Launch(plan.clone());
    let wire = action.to_wire();
    assert_eq!(MemberAction::from_wire(&wire).unwrap(), action);
    let mut other = plan.clone_with_rendezvous_port(25001);
    assert_ne!(payload_digest(&MemberAction::Launch(other)), payload_digest(&action));
}

// T34: a host without engine_groups is refused typed; one with it is not.
#[test]
fn engine_groups_gates_group_placement() {
    let none = BTreeSet::new();
    assert_eq!(group_refusal(&none).as_deref(), Some("host_capability_missing:engine_groups"));
    let with = BTreeSet::from([ENGINE_GROUPS.to_owned()]);
    assert_eq!(group_refusal(&with), None);
    assert!(agent_capabilities().contains(&ENGINE_GROUPS.to_owned()));
}

// T34: drain-only hosts may still terminate a group member but never prepare or launch one.
#[test]
fn drain_only_refuses_group_prepare_and_launch() {
    assert!(!drain_only_permits(&command_with(MemberAction::Prepare(sample_group_plan()))));
    assert!(!drain_only_permits(&command_with(MemberAction::Launch(sample_group_plan()))));
}
```

Use the file's existing helpers for `to_wire`/`from_wire`/`payload_digest`/`command_with` (search `fn round_trip` in `crates/mllm-protocol/tests/execution.rs`); add `sample_group_plan()` there. `clone_with_rendezvous_port` is a test-only helper that rebuilds the plan through `GroupPlan::new` with another port.

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p mllm-protocol` — expected: missing fields and constant.

- [ ] **Step 3: Implement.** Add fields with new numbers only:

```proto
message GroupMemberPlan {
  // existing fields 1..8 unchanged; service_port 0 now means "none" (worker)
  string role = 9;            // "head" | "worker"
  string model_path = 10;
}
message GroupLaunchPlan {
  // existing fields unchanged
  uint32 tensor_parallel = 3;
  uint32 pipeline_parallel = 4;
  uint32 local_ranks = 5;
  int64 generation = 6;
}
```

(Use the next free numbers in the actual file; the comment in each message records "ADR 0020 §13".) Conversion validates through `GroupPlan::new`, so a malformed wire plan never becomes a domain plan. Add `ENGINE_GROUPS` to `CATALOGUE` and `group_refusal`.

- [ ] **Step 4: Run the tests.** Run: `cargo test -p mllm-protocol` — expected: PASS; `PROTOCOL_VERSION` still `"2"` (existing assertion).

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-protocol
git commit -m "feat(protocol): group plan member fields and the engine_groups capability (ADR 0020 §13)"
```

---

### Task 7: Store: group plans, member owners, all-or-nothing reservation, rendezvous ports

**Files:**
- Create: `crates/mllm-store/src/groups.rs`, `crates/mllm-store/tests/groups.rs`
- Modify: `crates/mllm-store/src/instances.rs` (member owner ids), `crates/mllm-store/src/migrations.rs` (new tables), `crates/mllm-store/src/lib.rs`

**Interfaces:**
- Consumes: `GrantRequest`, `reserve_increase_in_transaction(&Transaction, &GrantRequest, AdmissionContext)` (existing, `resource_ledger.rs`), `GroupPlan` (Task 5), `GroupsPolicy` (Task 3).
- Produces:
  - `pub fn member_owner_id(deployment_id: &str, instance_index: u32, rank: u32) -> String` → `deployment:<id>/instance:<k>/member:<r>`; `pub fn parse_member_owner_id(&str) -> Option<(String, u32, u32)>`.
  - Tables: `group_plans(deployment_id, instance_index, generation, plan_json, rendezvous_host, rendezvous_port, state, PRIMARY KEY(deployment_id, instance_index, generation))`; `group_members(deployment_id, instance_index, generation, rank, host_id, owner_id, state CHECK(state IN ('reserved','launched','settled','uncertain')))`.
  - `pub struct GroupReservation { pub deployment_id: String, pub instance_index: u32, pub members: Vec<(String /*host*/, GrantRequest)>, pub head_host: String, pub port_range: RangeInclusive<u16> }`.
  - `impl ResourceStore { pub fn reserve_group(&self, r: &GroupReservation, plan_for: impl FnOnce(u16) -> Result<GroupPlan, GroupIdentityError>, ctx: AdmissionContext<'_>) -> Result<GroupPlan, GroupStoreError>; pub fn settle_member(&self, deployment_id: &str, instance_index: u32, generation: i64, rank: u32, evidence: MemberGone) -> Result<GroupSettlement, GroupStoreError>; pub fn mark_member_uncertain(&self, ..., rank: u32) -> Result<(), GroupStoreError>; pub fn group_plan(&self, deployment_id: &str, instance_index: u32) -> Result<Option<(GroupPlan, Vec<MemberRow>)>, GroupStoreError>; }`.
  - `pub enum GroupSettlement { Partial { unsettled: Vec<u32> }, Complete }` — `Complete` also frees the rendezvous port.
  - `pub enum GroupStoreError { PortsExhausted, Admission(ResourceStoreError), Plan, Conflict }`; `PortsExhausted` maps to `rendezvous_ports_exhausted`.
  - `MemberGone` is the existing verified gone-evidence type the single-instance settlement consumes (search `fn settle` in `crates/mllm-store/src/instances.rs`); reuse it, do not invent a weaker one.

- [ ] **Step 1: Write the failing tests**

```rust
// crates/mllm-store/tests/groups.rs
// T27: all members reserve in one transaction or none do.
#[test]
fn group_reservation_is_all_or_nothing() {
    let store = two_host_store(/*host_a_free*/ gib(100), /*host_b_free*/ gib(10));
    let r = reservation("g", &[("host-a", gib(80)), ("host-b", gib(80))]);
    assert!(matches!(store.reserve_group(&r, plan_for("g"), ctx()), Err(GroupStoreError::Admission(_))));
    assert_eq!(store.owner_bytes(&member_owner_id("g", 0, 0)), 0);
    assert_eq!(store.owner_bytes(&member_owner_id("g", 0, 1)), 0);
    assert!(store.group_plan("g", 0).unwrap().is_none());
}

// T27, Review Focus 5: two groups sharing host B never hold host B's share while failing on A.
#[test]
fn concurrent_groups_do_not_deadlock_or_leak() {
    let store = shared_store_three_hosts(gib(100), gib(100), gib(100)); // a, b, c
    let one = reservation("g1", &[("host-a", gib(80)), ("host-b", gib(80))]);
    let two = reservation("g2", &[("host-c", gib(80)), ("host-b", gib(80))]);
    let (x, y) = std::thread::scope(|s| {
        let h1 = s.spawn(|| store.clone().reserve_group(&one, plan_for("g1"), ctx()));
        let h2 = s.spawn(|| store.clone().reserve_group(&two, plan_for("g2"), ctx()));
        (h1.join().unwrap(), h2.join().unwrap())
    });
    assert!(x.is_ok() ^ y.is_ok(), "exactly one fits on host B");
    let loser = if x.is_ok() { "g2" } else { "g1" };
    assert_eq!(store.owner_bytes(&member_owner_id(loser, 0, 0)), 0);
}

// T27: the rendezvous port comes from the head's range and is not reused while unsettled.
#[test]
fn rendezvous_ports_are_allocated_and_held_until_complete() {
    let store = two_host_store(gib(400), gib(400));
    let mut r = reservation("g1", &[("host-a", gib(10)), ("host-b", gib(10))]);
    r.port_range = 25000..=25001;
    let p1 = store.reserve_group(&r, plan_for("g1"), ctx()).unwrap().rendezvous_port();
    let r2 = GroupReservation { deployment_id: "g2".into(), ..r.clone() };
    let p2 = store.reserve_group(&r2, plan_for("g2"), ctx()).unwrap().rendezvous_port();
    assert_ne!(p1, p2);
    let r3 = GroupReservation { deployment_id: "g3".into(), ..r.clone() };
    assert!(matches!(store.reserve_group(&r3, plan_for("g3"), ctx()), Err(GroupStoreError::PortsExhausted)));
    // settle one member of g1: the port stays held
    store.settle_member("g1", 0, 1, 1, gone()).unwrap();
    assert!(matches!(store.reserve_group(&r3, plan_for("g3"), ctx()), Err(GroupStoreError::PortsExhausted)));
    assert!(matches!(store.settle_member("g1", 0, 1, 0, gone()).unwrap(), GroupSettlement::Complete));
    assert_eq!(store.reserve_group(&r3, plan_for("g3"), ctx()).unwrap().rendezvous_port(), p1);
}

// T32: an uncertain member keeps its charge; settling others does not free it.
#[test]
fn uncertain_member_keeps_its_charge() {
    let store = two_host_store(gib(100), gib(100));
    store.reserve_group(&reservation("g", &[("host-a", gib(50)), ("host-b", gib(50))]), plan_for("g"), ctx()).unwrap();
    store.mark_member_uncertain("g", 0, 1, 1).unwrap();
    assert!(matches!(store.settle_member("g", 0, 1, 0, gone()).unwrap(), GroupSettlement::Partial { .. }));
    assert_eq!(store.owner_bytes(&member_owner_id("g", 0, 1)), gib(50));
}

// Review Focus 1: two members naming one host id are refused before any write.
#[test]
fn one_host_twice_is_refused() {
    let store = two_host_store(gib(400), gib(400));
    let r = reservation("g", &[("host-a", gib(10)), ("host-a", gib(10))]);
    assert!(matches!(store.reserve_group(&r, plan_for("g"), ctx()), Err(GroupStoreError::Plan)));
}

// T33: owner ids round-trip and never collide with instance owners.
#[test]
fn member_owner_ids_round_trip() {
    let id = member_owner_id("d", 0, 1);
    assert_eq!(parse_member_owner_id(&id), Some(("d".into(), 0, 1)));
    assert_eq!(parse_member_owner_id(&instance_owner_id("d", 2)), None);
}
```

Write the fixtures (`two_host_store`, `shared_store_three_hosts`, `reservation`, `plan_for`, `ctx`, `gone`, `gib`, `owner_bytes`) at the top of the test file on top of the store's existing test fixtures for published host policies (search `fn published_policy` in `crates/mllm-store/tests/`). `plan_for(d)` returns a closure building a two- or three-member plan through `GroupPlan::new` with the given port.

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p mllm-store --test groups` — expected: unresolved items.

- [ ] **Step 3: Implement `reserve_group`.** One `TransactionBehavior::Immediate` transaction: refuse duplicate host ids (`Plan`); pick the lowest port in `port_range` not held by any `group_plans` row with `state != 'settled'` and `rendezvous_host = head_host` (`PortsExhausted`); build the plan with `plan_for(port)` (`Plan` on error); call `reserve_increase_in_transaction` for every member's `GrantRequest` (owner id from `member_owner_id`), returning on the first error so the transaction rolls back; insert `group_plans` and `group_members` rows; commit. Comment: `// ADR 0020 §4, SPEC §11: all members or none; atomic accounting, not an atomic launch`. `settle_member` releases one member's owner through the existing release path only with `MemberGone`; when no member remains unsettled or uncertain, mark the plan settled (frees the port). `mark_member_uncertain` never releases.

- [ ] **Step 4: Run the tests.** Run: `cargo test -p mllm-store` — expected: PASS, including the migration tests (add the new tables to the migration fixture).

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-store
git commit -m "feat(store): group plans, per-member owners, all-or-nothing reservation and rendezvous ports (ADR 0020 §4)"
```

---

### Task 8: Host checks, peer address and `Prepare`

**Files:**
- Create: `crates/mllm-agent/src/host_checks.rs`
- Modify: `crates/mllm-agent/src/native_execution.rs` (handle `MemberAction::Prepare`), `crates/mllm-agent/src/lib.rs`, the inventory builder (search `fn inventory` in `native_execution.rs`)
- Test: unit tests in `host_checks.rs`; `crates/mllm-agent/tests/group_prepare.rs`

**Interfaces:**
- Consumes: `GroupsPolicy` (Task 3), `GroupPlan` (Task 5), `ENGINE_GROUPS` (Task 6).
- Produces:
  - `pub struct HostFacts { pub compaction_proactiveness: Option<u64>, pub memlock_soft: Option<u64> /* None = unlimited */, pub infiniband: InfinibandAccess, pub local_addresses: Vec<IpAddr> }`; `pub enum InfinibandAccess { Absent, NoAccess, ReadWrite }`.
  - `pub fn read_host_facts(root: &Path) -> HostFacts` (reads `<root>/proc/sys/vm/compaction_proactiveness`, `getrlimit(RLIMIT_MEMLOCK)`, `<root>/dev/infiniband/uverbs*` with `access(R_OK|W_OK)`, local addresses via `getifaddrs`); `root` is `/` in production and a temp dir in tests.
  - `pub enum Finding { Compaction, Memlock, Infiniband }` with `fn item(&self) -> &'static str` (`compaction`, `memlock`, `infiniband`).
  - `pub struct CheckVerdict { pub warnings: Vec<String>, pub refusal: Option<String> }`; `pub fn evaluate(facts: &HostFacts, policy: &GroupsPolicy) -> CheckVerdict`.
  - `pub fn prepare_member(plan: &GroupPlan, host_id: &str, facts: &HostFacts, policy: &GroupsPolicy, port_free: impl Fn(IpAddr, u16) -> bool, digest_of: impl Fn(&str) -> Option<String>, profile_fingerprint: impl Fn(&str) -> Option<String>) -> Result<CheckVerdict, String>`; the `Err` string is a closed code.

- [ ] **Step 1: Write the failing tests**

```rust
// T29: every finding warns by default and refuses only under require_rdma (compaction never refuses).
#[test]
fn findings_warn_by_default_and_refuse_under_require_rdma() {
    let facts = HostFacts {
        compaction_proactiveness: Some(20),
        memlock_soft: Some(8 << 20),
        infiniband: InfinibandAccess::NoAccess,
        local_addresses: vec!["192.0.2.10".parse().unwrap()],
    };
    let lax = evaluate(&facts, &GroupsPolicy::default());
    assert_eq!(lax.warnings, ["host_tuning_warning:compaction", "host_tuning_warning:memlock", "host_tuning_warning:infiniband"]);
    assert_eq!(lax.refusal, None);
    let strict = evaluate(&facts, &GroupsPolicy { require_rdma: true, ..Default::default() });
    assert_eq!(strict.refusal.as_deref(), Some("host_tuning_missing:memlock"));
}

// T29: facts are read from files and never written.
#[test]
fn facts_are_read_only() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("proc/sys/vm")).unwrap();
    let path = root.path().join("proc/sys/vm/compaction_proactiveness");
    std::fs::write(&path, "0\n").unwrap();
    let before = std::fs::metadata(&path).unwrap().modified().unwrap();
    let facts = read_host_facts(root.path());
    assert_eq!(facts.compaction_proactiveness, Some(0));
    assert_eq!(facts.infiniband, InfinibandAccess::Absent);
    assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), before);
}

// Review Focus 2: an occupied rendezvous port on the head refuses Prepare, naming the port.
#[test]
fn occupied_rendezvous_port_refuses_prepare() {
    let plan = sample_plan(); // Task 5 shape, head on "host-a", port 25000
    let err = prepare_member(&plan, "host-a", &good_facts(), &policy_with("192.0.2.10"),
        |_, port| port != 25000, |_| Some("sha256:c".into()), |_| Some("pinned".into())).unwrap_err();
    assert_eq!(err, "rendezvous_port_in_use:25000");
}

// T30: a peer address not on this host, a digest mismatch and a profile mismatch are refused.
#[test]
fn prepare_refuses_address_digest_and_profile_mismatch() {
    let plan = sample_plan();
    let ok_port = |_, _| true;
    assert_eq!(prepare_member(&plan, "host-b", &good_facts(), &policy_with("192.0.2.99"), ok_port,
        |_| Some("sha256:c".into()), |_| Some("pinned".into())).unwrap_err(), "peer_address_not_local");
    assert_eq!(prepare_member(&plan, "host-b", &good_facts_for("192.0.2.11"), &policy_with("192.0.2.11"), ok_port,
        |_| Some("sha256:x".into()), |_| Some("pinned".into())).unwrap_err(), "group_checkpoint_mismatch");
    assert_eq!(prepare_member(&plan, "host-b", &good_facts_for("192.0.2.11"), &policy_with("192.0.2.11"), ok_port,
        |_| Some("sha256:c".into()), |_| Some("other".into())).unwrap_err(), "group_profile_mismatch");
}
```

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p mllm-agent host_checks` — expected: unresolved module.

- [ ] **Step 3: Implement.** `evaluate`: compaction nonzero → warning only; memlock not unlimited → warning, or refusal `host_tuning_missing:memlock` under `require_rdma`; infiniband not `ReadWrite` → warning, or `host_tuning_missing:infiniband` under `require_rdma`; the first refusal in the order memlock, infiniband wins. `prepare_member`: find this host's member (else `group_profile_mismatch`), its `peer_address` must equal the policy's and be in `facts.local_addresses` (`peer_address_not_local`), profile fingerprint equal (`group_profile_mismatch`), digest of `model_path` equal (`group_checkpoint_mismatch`), on the head `port_free(peer, rendezvous_port)` and `port_free(127.0.0.1, service_port)` (`rendezvous_port_in_use:<p>`, `service_port_in_use:<p>`), then `evaluate`. The native path's `Prepare` handler calls `prepare_member` with a real `TcpListener::bind` probe (bind and drop), the checkpoint digest cache and the profile table; it journals nothing and spawns nothing. The inventory reports `GroupInventory { peer_address, findings }`. Comments cite `// ADR 0020 §5, §11 (owner decision 10): read, never change`.

- [ ] **Step 4: Run the tests.** Run: `cargo test -p mllm-agent` — expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-agent
git commit -m "feat(agent): group Prepare with read-only host checks and peer address verification (ADR 0020 §5, §11)"
```

---

### Task 9: vLLM multi-node rendering

**Files:**
- Modify: `crates/mllm-adapters/src/vllm/args.rs`, `crates/mllm-adapters/src/vllm/frozen.rs`
- Test: `crates/mllm-adapters/src/vllm/args.rs` tests (or `crates/mllm-adapters/tests/vllm_group_args.rs`)

**Interfaces:**
- Consumes: `PlanInputVllm`, `render_command` (existing).
- Produces:
  - `pub struct VllmGroupArgs { pub nnodes: u32, pub node_rank: u32, pub master_addr: IpAddr, pub master_port: u16, pub own_peer_address: IpAddr }`.
  - `PlanInputVllm.group: Option<VllmGroupArgs>` (default `None`).
  - `RenderedCommand.env` gains `VLLM_HOST_IP` and `MLLM_GROUP_MODE=1` for a group; `MLLM_GROUP_EXPECTED` carries a JSON object of the rendered multi-node destinations for the entry (Task 10).
  - Worker (`node_rank > 0`) omits `--host`, `--port`, `--served-model-name`, the API key and the middleware, and adds `--headless`.

- [ ] **Step 1: Write the failing tests**

```rust
fn group_input(rank: u32) -> PlanInputVllm {
    PlanInputVllm {
        engine_bin: "/opt/venv/bin/vllm".into(),
        model_path: "/models/m".into(),
        port: 30000,
        served_model_name: "m".into(),
        tensor_parallel_size: 2,
        pipeline_parallel_size: 1,
        group: Some(VllmGroupArgs {
            nnodes: 2,
            node_rank: rank,
            master_addr: "192.0.2.10".parse().unwrap(),
            master_port: 25000,
            own_peer_address: format!("192.0.2.{}", 10 + rank).parse().unwrap(),
        }),
        ..PlanInputVllm::default()
    }
}
fn has_pair(argv: &[String], flag: &str, value: &str) -> bool {
    argv.windows(2).any(|w| w[0] == flag && w[1] == value)
}

// T14: the head renders every multi-node flag and keeps its loopback API.
#[test]
fn head_renders_multinode_flags() {
    let cmd = render_command(&group_input(0)).unwrap();
    for (f, v) in [("--tensor-parallel-size", "2"), ("--pipeline-parallel-size", "1"),
                   ("--nnodes", "2"), ("--node-rank", "0"), ("--master-addr", "192.0.2.10"),
                   ("--master-port", "25000"), ("--distributed-executor-backend", "mp"),
                   ("--host", "127.0.0.1"), ("--port", "30000")] {
        assert!(has_pair(&cmd.argv, f, v), "{f} {v}");
    }
    assert!(!cmd.argv.iter().any(|a| a == "--headless"));
    assert_eq!(cmd.env.get("VLLM_HOST_IP").map(String::as_str), Some("192.0.2.10"));
}

// T14, T21: a worker is headless with no API listener, key or middleware.
#[test]
fn worker_is_headless() {
    let cmd = render_command(&group_input(1)).unwrap();
    assert!(cmd.argv.iter().any(|a| a == "--headless"));
    assert!(has_pair(&cmd.argv, "--node-rank", "1"));
    for absent in ["--host", "--port", "--api-key", "--middleware", "--served-model-name"] {
        assert!(!cmd.argv.iter().any(|a| a == absent), "{absent}");
    }
    assert_eq!(cmd.env.get("VLLM_HOST_IP").map(String::as_str), Some("192.0.2.11"));
}

// T21, T37 (owner decision 4): no NCCL or GLOO variable is ever rendered.
#[test]
fn no_transport_variables_are_rendered() {
    for rank in [0, 1] {
        let cmd = render_command(&group_input(rank)).unwrap();
        assert!(!cmd.env.keys().any(|k| k.starts_with("NCCL_") || k.starts_with("GLOO_")));
    }
}

// T14: user args still cannot state a multi-node flag.
#[test]
fn user_multinode_flags_stay_reserved() {
    let mut input = group_input(0);
    input.extra_args = vec!["--nnodes".into(), "4".into()];
    assert!(matches!(render_command(&input), Err(ArgsError::ReservedConflict(_))));
}

// T39: a single-rank launch renders byte-identically to before.
#[test]
fn single_rank_rendering_is_unchanged() {
    let mut input = group_input(0);
    input.group = None;
    input.tensor_parallel_size = 1;
    let cmd = render_command(&input).unwrap();
    assert!(!cmd.argv.iter().any(|a| a == "--nnodes" || a == "--headless"));
    assert!(!cmd.env.contains_key("VLLM_HOST_IP"));
}
```

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p mllm-adapters vllm` — expected: `group` field missing.

- [ ] **Step 3: Implement.** In `render_command`, after typed args: when `group` is `Some`, push the multi-node pairs, and for `node_rank > 0` skip the listener/served-name/key/middleware block and push `--headless`. Set env `VLLM_HOST_IP`, `MLLM_GROUP_MODE=1`, `MLLM_GROUP_EXPECTED={"nnodes":2,"node_rank":r,"master_addr":"…","master_port":p,"headless":bool,"distributed_executor_backend":"mp","tensor_parallel_size":tp,"pipeline_parallel_size":pp}`. In `frozen.rs`, replace the `tensor_parallel_size: 1` pin with the plan's values. Comment: `// ADR 0020 §8: mllm renders every multi-node flag; the user states none`.

- [ ] **Step 4: Run the tests.** Run: `cargo test -p mllm-adapters` — expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-adapters
git commit -m "feat(adapters): render vLLM native multi-node head and headless worker (ADR 0020 §8)"
```

---

### Task 10: Group mode in the protected vLLM entry

**Files:**
- Modify: `runtime/vllm_entry.py`, `runtime/loopback_rendezvous.py`
- Test: `runtime/tests/test_vllm_entry.py`, `runtime/tests/test_loopback_rendezvous.py`

**Interfaces:**
- Consumes: `MLLM_GROUP_MODE`, `MLLM_GROUP_EXPECTED`, `VLLM_HOST_IP` (Task 9).
- Produces: `loopback_rendezvous.pin_group(env)` — removes `MASTER_ADDR`, `MASTER_PORT`, `HOST_IP` and every `NCCL_*`/`GLOO_*` present, keeps `VLLM_HOST_IP`; `verify_group(env, expected_host_ip)`; `vllm_entry.check_group(expected, namespace)` raising `LaunchError("group_drift:<dest>")`.

- [ ] **Step 1: Write the failing tests**

```python
class GroupModeTests(unittest.TestCase):
    # T14, T21: every rendered multi-node destination must match the parse.
    def test_group_drift_is_refused(self):
        expected = {"nnodes": 2, "node_rank": 1, "master_addr": "192.0.2.10",
                    "master_port": 25000, "headless": True,
                    "distributed_executor_backend": "mp",
                    "tensor_parallel_size": 2, "pipeline_parallel_size": 1}
        ns = argparse.Namespace(**expected)
        vllm_entry.check_group(expected, ns)
        for dest, bad in [("node_rank", 0), ("master_port", 25001), ("headless", False)]:
            drifted = argparse.Namespace(**{**expected, dest: bad})
            with self.assertRaises(vllm_entry.LaunchError) as ctx:
                vllm_entry.check_group(expected, drifted)
            self.assertEqual(ctx.exception.code, "group_drift:" + dest)

    # T21 (owner decision 4): group mode pins no interface and strips inherited transport.
    def test_pin_group_strips_transport_and_keeps_host_ip(self):
        env = {"NCCL_IB_HCA": "x", "GLOO_SOCKET_IFNAME": "lo", "MASTER_PORT": "1",
               "VLLM_HOST_IP": "192.0.2.11"}
        loopback_rendezvous.pin_group(env)
        self.assertEqual(env, {"VLLM_HOST_IP": "192.0.2.11"})
        loopback_rendezvous.verify_group(env, "192.0.2.11")
        env["NCCL_SOCKET_IFNAME"] = "eth0"
        with self.assertRaises(Exception):
            loopback_rendezvous.verify_group(env, "192.0.2.11")

    # T39: without MLLM_GROUP_MODE the single-rank pin is applied unchanged.
    def test_single_rank_keeps_loopback_pin(self):
        env = {}
        loopback_rendezvous.pin(env)
        self.assertEqual(env["NCCL_SOCKET_IFNAME"], "lo")
```

Adapt `pin(env)` to the module's existing signature (read it first); the assertion is what matters.

- [ ] **Step 2: Run to verify failure.** Run: `python3 -m unittest runtime.tests.test_vllm_entry runtime.tests.test_loopback_rendezvous` (or `discover -s runtime/tests`) — expected: AttributeError.

- [ ] **Step 3: Implement.** In `main`, when `os.environ.get("MLLM_GROUP_MODE") == "1"`: call `pin_group` instead of `pin`, move `nnodes, node_rank, master_addr, master_port, headless, distributed_executor_backend, tensor_parallel_size, pipeline_parallel_size` out of the reserved "must equal the single-rank value" comparison into `check_group(json.loads(MLLM_GROUP_EXPECTED), namespace)`, and call `verify_group` immediately before handing control to vLLM. The single-rank path is unchanged. Module docstring gains: "ADR 0020 §8, §12: group mode opens non-loopback rendezvous listeners by design (ADR 0012 amendment)".

- [ ] **Step 4: Run the tests.** Run: `python3 -m unittest discover -s runtime/tests` — expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add runtime
git commit -m "feat(runtime): group mode in the vLLM entry: rendered multi-node destinations, no transport pin (ADR 0020 §8)"
```

---

### Task 11: Agent member launch and member terminate

**Files:**
- Modify: `crates/mllm-agent/src/native_execution.rs` (accept `MemberAction::Launch(GroupPlan)` for vLLM; remove the `Unauthorized` refusal for it; select this host's member), `crates/mllm-agent/src/native_execution/vllm.rs` (build `PlanInputVllm.group`), `crates/mllm-agent/src/journal.rs` (journal a member launch keyed by `(deployment, instance, generation, rank)`)
- Test: `crates/mllm-agent/tests/group_launch.rs`

**Interfaces:**
- Consumes: `GroupPlan`, `member_id` (Task 5); `VllmGroupArgs` (Task 9); `prepare_member` (Task 8).
- Produces:
  - `fn member_of<'a>(plan: &'a GroupPlan, host_id: &str) -> Option<&'a MemberPlan>` in `native_execution.rs`.
  - Launch outcome for a worker reports `ProcessIdentity` entries with roles `worker-<r>` and its children, never an ingress endpoint.
  - `Terminate` on a member handle behaves exactly as for a single launch (journal-owned tree, ADR 0016 recorded identities).
  - SGLang group launches still refuse: `group_engine_unsupported:sglang`.

- [ ] **Step 1: Write the failing tests** (Fake launcher from `mllm-testkit`, no real engine)

```rust
// T30: a worker member launches headless, journals first, reports identities, opens no ingress.
#[tokio::test]
async fn worker_member_launches_headless_and_journals_first() {
    let host = FakeHost::new("host-b").with_groups_policy("192.0.2.11");
    let plan = two_member_plan(); // head host-a, worker host-b
    let out = host.execute(MemberAction::Launch(plan.clone())).await.unwrap();
    assert!(host.journal().has_launch_for(&plan, 1));
    assert!(out.ingress.is_none());
    assert!(out.processes.iter().all(|p| p.role.starts_with("worker-1")));
    assert!(host.last_argv().contains(&"--headless".to_owned()));
}

// T30: the head launches with its loopback API and ingress.
#[tokio::test]
async fn head_member_launches_with_ingress() {
    let host = FakeHost::new("host-a").with_groups_policy("192.0.2.10");
    let out = host.execute(MemberAction::Launch(two_member_plan())).await.unwrap();
    assert!(out.ingress.is_some());
}

// T30: a plan that does not name this host is refused with no effect.
#[tokio::test]
async fn plan_without_this_host_is_refused() {
    let host = FakeHost::new("host-c").with_groups_policy("192.0.2.12");
    assert!(host.execute(MemberAction::Launch(two_member_plan())).await.is_err());
    assert!(host.journal().is_empty());
}

// T22: SGLang group launches stay refused until Task 20.
#[tokio::test]
async fn sglang_group_is_refused() {
    let host = FakeHost::new("host-a").with_groups_policy("192.0.2.10").with_engine("sglang");
    let err = host.execute(MemberAction::Launch(two_member_plan())).await.unwrap_err();
    assert_eq!(err.code(), "group_engine_unsupported:sglang");
}

// T31, T33: a worker member terminates its own recorded tree only.
#[tokio::test]
async fn worker_member_terminates_its_recorded_tree() {
    let host = FakeHost::new("host-b").with_groups_policy("192.0.2.11");
    let out = host.execute(MemberAction::Launch(two_member_plan())).await.unwrap();
    let gone = host.terminate(&out.owned_handle, &out.processes).await.unwrap();
    assert!(gone.all_gone());
}
```

`FakeHost` is the agent test fixture over `NativeExecution` with the Fake launcher; extend the existing fixture in `crates/mllm-agent/tests/` (search `struct Fake` there) rather than writing a new one, adding `with_groups_policy`, `with_engine` and `last_argv`.

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p mllm-agent --test group_launch` — expected: the native path returns `Unauthorized`.

- [ ] **Step 3: Implement.** In `render_launch`, accept `Launch(GroupPlan)` when the profile's engine is vLLM: re-run `prepare_member` (a Launch without a passing Prepare-equivalent never spawns), take `member_of`, build `PlanInputVllm` from the member (its `model_path`, devices → `CUDA_VISIBLE_DEVICES` as today, `group: Some(VllmGroupArgs { nnodes: plan.members().len(), node_rank: rank, master_addr: head.peer_address, master_port: plan.rendezvous_port(), own_peer_address: member.peer_address })`), then take the existing durable-spawn path. Replace the `native_execution.rs:394` "exactly one device" rule by "exactly `local_ranks` devices". Worker launches skip ingress creation and the readiness wait; they return once identities are recorded. Comments cite `// ADR 0020 §6`.

- [ ] **Step 4: Run the tests.** Run: `cargo test -p mllm-agent` — expected: PASS; every single-rank launch test unchanged.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-agent
git commit -m "feat(agent): launch and terminate a vLLM group member; workers run headless (ADR 0020 §6)"
```

---

### Task 12: Multi-member Fake engine

**Files:**
- Create: `crates/mllm-testkit/src/fake_group.rs`
- Modify: `crates/mllm-testkit/src/lib.rs`
- Test: unit tests in `fake_group.rs`

**Interfaces:**
- Produces:
  - `pub struct FakeGroup` — N fake hosts sharing one fake engine group; `FakeGroup::new(hosts: &[&str]) -> Self`.
  - `fn launch(&self, rank: u32)`; the head becomes ready only after every rank has launched (`fn head_ready(&self) -> bool`).
  - Fault injection: `fn exit_rank(&self, rank: u32)`, `fn disconnect_host(&self, host: &str)`, `fn reconnect_host(&self, host: &str, journal: JournalState /* Kept | Empty */)`, `fn sleep_leaves_rank_resident(&self, rank: u32)`, `fn wake_output(&self, tokens: Vec<u32>)`.
  - Observations: `fn resident_bytes(&self, rank: u32) -> u64`, `fn alive(&self, rank: u32) -> bool`, `fn sleep_calls(&self) -> u32` (counts collectives the head received).
  - When any rank exits, the others hang (alive, not serving), as NCCL does; the head's readiness fails.

- [ ] **Step 1: Write the failing tests**

```rust
// T30: the head is not ready until every rank has launched.
#[test]
fn head_waits_for_every_rank() {
    let g = FakeGroup::new(&["a", "b"]);
    g.launch(0);
    assert!(!g.head_ready());
    g.launch(1);
    assert!(g.head_ready());
}

// T31: a rank exit leaves the others alive but not serving.
#[test]
fn rank_exit_hangs_the_rest() {
    let g = FakeGroup::new(&["a", "b"]);
    g.launch(0);
    g.launch(1);
    g.exit_rank(1);
    assert!(g.alive(0));
    assert!(!g.head_ready());
}

// T20: a faulty sleep leaves one rank resident while the head reports success.
#[test]
fn faulty_sleep_leaves_a_rank_resident() {
    let g = FakeGroup::new(&["a", "b"]);
    g.launch(0);
    g.launch(1);
    g.sleep_leaves_rank_resident(1);
    assert!(g.sleep().is_ok());
    assert_eq!(g.sleep_calls(), 1);
    assert!(g.resident_bytes(1) > 0);
    assert_eq!(g.resident_bytes(0), g.parked_bytes());
}
```

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p mllm-testkit fake_group` — expected: unresolved module.

- [ ] **Step 3: Implement** with `Arc<Mutex<State>>` holding per-rank `{launched, alive, resident, host_connected}`; `sleep()`/`wake()` act on every rank except the injected faulty one; the canary returns `wake_output` when set, else the reference tokens `[1, 2, 3, 4]`. Plug it into `FakeLauncher` so the agent fixture of Task 11 and the controller tests of Tasks 13–16 share it.

- [ ] **Step 4: Run the tests.** Run: `cargo test -p mllm-testkit` — expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-testkit
git commit -m "test(testkit): multi-member Fake engine with rank faults and per-rank residency"
```

---

### Task 13a: Parallel weights and cross-host digest agreement

**Files:**
- Create: `crates/mllm-controller/src/group_sources.rs`
- Modify: `crates/mllm-controller/src/lib.rs`, `crates/mllm-controller/src/model_sources.rs` (take a member id instead of `"head"`)
- Test: `crates/mllm-controller/src/group_sources.rs` tests

**Interfaces:**
- Consumes: the existing per-host materialization (`model_sources.rs`) and digest request (`checkpoint_digests.rs`).
- Produces:
  - `pub async fn materialize_on_all(hosts: &[String], source: &ModelSource, driver: &impl SourceDriver) -> Result<BTreeMap<String, Materialized>, GroupSourceError>` — starts every host at once and waits for all.
  - `pub fn agree_digests(per_host: &BTreeMap<String, String>) -> Result<String, GroupSourceError>`.
  - `pub enum GroupSourceError { Space { host: String }, Failed { host: String, reason: String }, Mismatch { digests: BTreeMap<String, String> } }` with `code()` → `insufficient_space`, the host's own reason, `group_checkpoint_mismatch`.
  - `SourceDriver` is the trait the controller already uses to send `MaterializeSource` and read digests; if it is a concrete type today, add this trait with one production impl delegating to it.

- [ ] **Step 1: Write the failing tests**

```rust
// T07: every host materializes concurrently; one host short of space fails the group before launch.
#[tokio::test]
async fn materialization_is_parallel_and_space_is_checked() {
    let driver = FakeSourceDriver::new()
        .host("host-a", Ok(mat("/m", "sha256:c")))
        .host("host-b", Err(SourceFailure::InsufficientSpace));
    let err = materialize_on_all(&hosts(), &hf_source(), &driver).await.unwrap_err();
    assert_eq!(err.code(), "insufficient_space");
    assert_eq!(driver.max_in_flight(), 2);
}

// T14: digests must agree across hosts; a mismatch names every host.
#[test]
fn digests_must_agree() {
    let same = BTreeMap::from([("host-a".into(), "sha256:c".into()), ("host-b".into(), "sha256:c".into())]);
    assert_eq!(agree_digests(&same).unwrap(), "sha256:c");
    let diff = BTreeMap::from([("host-a".into(), "sha256:c".into()), ("host-b".into(), "sha256:d".into())]);
    let err = agree_digests(&diff).unwrap_err();
    assert_eq!(err.code(), "group_checkpoint_mismatch");
    assert!(err.to_string().contains("host-b"));
}
```

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p mllm-controller group_sources` — expected: unresolved.

- [ ] **Step 3: Implement** with `futures::future::join_all` over hosts (no new dependency: use what `mllm-controller` already depends on), collecting every host's outcome before deciding, so the error lists all failing hosts. Comment `// ADR 0020 §5 (owner decision 11)`.

- [ ] **Step 4: Run the tests.** Run: `cargo test -p mllm-controller` — expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-controller
git commit -m "feat(controller): materialize group weights on every host in parallel and compare digests (ADR 0020 §5)"
```

---

### Task 13b: Group activation: reserve, prepare, fan-out, head readiness

**Files:**
- Create: `crates/mllm-controller/src/group_activation.rs`
- Modify: `crates/mllm-controller/src/coordinator.rs` (route a deployment with `InstanceSpec.group` to group activation), `crates/mllm-controller/src/remote_execution.rs` and `remote_readiness.rs` (member id from the plan, not `"head"`), `crates/mllm-management/src/configuration.rs` (deploy resolves on **every** named host; `group_profile_mismatch`, `peer_address_missing`, `group_engine_unsupported:<engine>`, `host_capability_missing:engine_groups`)
- Test: `crates/mllm-controller/src/coordinator/tests_groups.rs` (new, registered in `coordinator.rs`'s test modules)

**Interfaces:**
- Consumes: `reserve_group` (Task 7), `group_refusal` (Task 6), `materialize_on_all`, `agree_digests` (Task 13a), `FakeGroup` (Task 12).
- Produces:
  - `pub async fn activate_group(ctx: &CoordinatorCtx, deployment: &DeploymentRecord, shape: &GroupShape) -> Result<GroupActivation, GroupActivationError>`.
  - `pub enum GroupActivation { Ready { plan: GroupPlan }, Failed { plan: GroupPlan, failed_rank: u32, reason: String } }`; `Failed` hands off to Task 14's `stop_group`.
  - Order, each step cited in code: sources (13a) → `reserve_group` → `Prepare` to all members concurrently → on any refusal release all reservations (no process exists) → `Launch` to all members concurrently → wait for head readiness while watching every member's exit reports → open the route only on `Ready`.

- [ ] **Step 1: Write the failing tests**

```rust
// T30: a clean two-host activation reserves both, launches both concurrently, routes only after head readiness.
#[tokio::test]
async fn group_activates_and_routes_after_head_readiness() {
    let world = GroupWorld::two_hosts(); // coordinator + two fake agents + FakeGroup
    let id = world.deploy_group("g", &["host-a", "host-b"]).await;
    world.agents_saw_launch_before_head_ready("g").await; // worker launched without waiting for the head
    assert!(!world.route_open("g"));
    world.group.launch_completes();
    world.wait_ready(&id).await;
    assert!(world.route_open("g"));
    assert_eq!(world.owner_bytes_on("host-b", &member_owner_id("g", 0, 1)), world.member_request());
}

// T30: a Prepare refusal on one host releases every member and launches nothing.
#[tokio::test]
async fn prepare_refusal_releases_everything() {
    let world = GroupWorld::two_hosts().prepare_refuses("host-b", "host_tuning_missing:memlock");
    let id = world.deploy_group("g", &["host-a", "host-b"]).await;
    let status = world.wait_settled(&id).await;
    assert_eq!(status.last_error(), "host_tuning_missing:memlock");
    assert_eq!(world.launches(), 0);
    assert_eq!(world.owner_bytes_on("host-a", &member_owner_id("g", 0, 0)), 0);
}

// T15: concurrent activation requests produce one plan and one launch per member.
#[tokio::test]
async fn concurrent_activation_is_single() {
    let world = GroupWorld::two_hosts();
    let id = world.deploy_group_no_start("g", &["host-a", "host-b"]).await;
    let (a, b) = tokio::join!(world.start(&id), world.start(&id));
    assert!(a.is_ok() && b.is_ok());
    assert_eq!(world.launches(), 2);
}

// T34: a named host without engine_groups is refused typed and receives nothing.
#[tokio::test]
async fn host_without_capability_is_refused() {
    let world = GroupWorld::two_hosts().without_capability("host-b", "engine_groups");
    let err = world.try_deploy_group("g", &["host-a", "host-b"]).await.unwrap_err();
    assert_eq!(err.code(), "host_capability_missing:engine_groups");
    assert_eq!(world.commands_sent_to("host-b"), 0);
}

// T14: deploy resolves on every named host; one mismatched build refuses the group.
#[tokio::test]
async fn profile_mismatch_refuses_deploy() {
    let world = GroupWorld::two_hosts().profile_fingerprint("host-b", "other");
    let err = world.try_deploy_group("g", &["host-a", "host-b"]).await.unwrap_err();
    assert_eq!(err.code(), "group_profile_mismatch");
}
```

`GroupWorld` extends the coordinator's remote-test harness (search `tests_remote.rs` for its world type) with `FakeGroup` and two fake agents; add its helpers in `tests_groups.rs`.

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p mllm-controller tests_groups` — expected: unresolved.

- [ ] **Step 3: Implement `activate_group`** as ordered above; `Prepare` and `Launch` fan-out use `join_all` over the per-host command send (each with the plan's generation and the member's `CommandIdentity { member: MemberKey { host_id, member_id: member_id(rank) }, instance_index: 0, .. }`); readiness uses the existing native readiness check against the head only (`// ADR 0020 §7, owner decision 5`), bounded by the deployment's initialize timeout, returning `Failed` on any member exit report. The lifecycle claim spans the whole activation.

- [ ] **Step 4: Run the tests.** Run the core suite — expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-controller crates/mllm-management
git commit -m "feat(controller): group activation: reserve all, prepare, concurrent launch, head readiness (ADR 0020 §4-§7)"
```

---

### Task 14: Stop, failure, compensation, uncertainty and recovery

**Files:**
- Create: `crates/mllm-controller/src/group_settlement.rs`
- Modify: `crates/mllm-controller/src/coordinator.rs` (stop, member exit handling), `crates/mllm-controller/src/engine_exit.rs` (a member exit maps to its group)
- Test: `crates/mllm-controller/src/coordinator/tests_groups.rs`

**Interfaces:**
- Consumes: `settle_member`, `mark_member_uncertain`, `group_plan` (Task 7); `GroupActivation::Failed` (Task 13b).
- Produces:
  - `pub async fn stop_group(ctx: &CoordinatorCtx, plan: &GroupPlan, reason: StopReason) -> GroupSettlement` — close head ingress, drain, `Terminate` every member concurrently, settle each on its own gone-evidence, mark unreachable members uncertain.
  - `pub async fn on_member_exit(ctx, deployment_id, generation, rank)` → `stop_group(.., StopReason::MemberFailed { rank })`, status `group_member_failed`.
  - Recovery under `recovery: reconcile` relaunches (new generation, new plan) only after `GroupSettlement::Complete`.

- [ ] **Step 1: Write the failing tests**

```rust
// T31: a worker exit stops the head; each host releases on its own evidence.
#[tokio::test]
async fn worker_exit_stops_the_group() {
    let world = GroupWorld::ready_two_host_group("g").await;
    world.group.exit_rank(1);
    let status = world.wait_settled_generation("g", 1).await;
    assert_eq!(status.last_error(), "group_member_failed");
    assert!(!world.group.alive(0));
    assert_eq!(world.owner_bytes_on("host-a", &member_owner_id("g", 0, 0)), 0);
    assert_eq!(world.owner_bytes_on("host-b", &member_owner_id("g", 0, 1)), 0);
    assert!(world.port_free("host-a", 25000));
}

// T31: a head exit with a surviving worker: the worker is terminated, no competing activation meanwhile.
#[tokio::test]
async fn head_exit_terminates_worker_before_any_relaunch() {
    let world = GroupWorld::ready_two_host_group("g").await.recovery_reconcile();
    world.group.exit_rank(0);
    world.wait_settled_generation("g", 1).await;
    assert!(!world.group.alive(1));
    assert!(world.generation_started_after_settlement("g", 2));
}

// T32: an unreachable worker host keeps its share charged and uncertain; the port stays held.
#[tokio::test]
async fn unreachable_host_keeps_charge_and_port() {
    let world = GroupWorld::ready_two_host_group("g").await;
    world.group.disconnect_host("host-b");
    world.stop(&world.id("g")).await;
    let status = world.status("g").await;
    assert_eq!(status.member(1).state, "uncertain");
    assert_ne!(world.owner_bytes_on("host-b", &member_owner_id("g", 0, 1)), 0);
    assert_eq!(world.owner_bytes_on("host-a", &member_owner_id("g", 0, 0)), 0);
    assert!(!world.port_free("host-a", 25000));
    world.advance_past_lease_expiry().await;
    assert_ne!(world.owner_bytes_on("host-b", &member_owner_id("g", 0, 1)), 0);
    world.group.reconnect_host("host-b", JournalState::Kept);
    world.wait_settled_generation("g", 1).await;
    assert!(world.port_free("host-a", 25000));
}

// Review Focus 3: a worker host back with an empty journal settles only on recorded identities.
#[tokio::test]
async fn empty_journal_reconnect_settles_on_recorded_identities_only() {
    let world = GroupWorld::ready_two_host_group("g").await.recovery_reconcile();
    world.group.disconnect_host("host-b");
    world.group.exit_rank(0);
    world.group.reconnect_host("host-b", JournalState::Empty); // rank 1 process still alive
    world.settle_for(std::time::Duration::from_secs(5)).await;
    assert_eq!(world.status("g").await.member(1).state, "uncertain");
    assert!(!world.generation_started("g", 2));
    world.group.kill_rank_process(1);
    world.wait_settled_generation("g", 1).await;
    assert!(world.generation_started("g", 2));
}

// T30: a Launch failure on one member after the other launched is compensated.
#[tokio::test]
async fn launch_failure_is_compensated() {
    let world = GroupWorld::two_hosts().launch_fails("host-b");
    let id = world.deploy_group("g", &["host-a", "host-b"]).await;
    world.wait_settled_generation("g", 1).await;
    assert!(!world.group.alive(0));
    assert!(!world.route_open("g"));
    assert_eq!(world.status_of(&id).await.last_error(), "group_member_failed");
}
```

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p mllm-controller tests_groups` — expected: new tests fail.

- [ ] **Step 3: Implement** `stop_group` and `on_member_exit`. Every release goes through `settle_member` with the host's own `MemberGone`; lease expiry and timeouts only ever call `mark_member_uncertain`. Relaunch checks `GroupSettlement::Complete` inside the same store transaction that draws the new generation. Comments cite `// ADR 0020 §9, owner decision 6; SPEC §11: lease expiry never frees memory`.

- [ ] **Step 4: Run the tests.** Run the core suite — expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-controller
git commit -m "feat(controller): whole-group stop on rank failure, per-host settlement, uncertain members (ADR 0020 §9)"
```

---

### Task 15: Group eviction across named hosts

**Files:**
- Modify: `crates/mllm-controller/src/switching.rs`, `crates/mllm-scheduler/src/placement.rs` (a group has fixed hosts: placement only validates fit)
- Test: `crates/mllm-controller/src/coordinator/tests_groups.rs`, `crates/mllm-scheduler` unit tests

**Interfaces:**
- Consumes: the per-host victim chooser (ADR 0013 decision 8, search `choose_victims` in `crates/mllm-scheduler`).
- Produces: `pub fn plan_group_eviction(per_host: &BTreeMap<String, HostCandidateView>, need: &BTreeMap<String, Bytes>) -> Option<BTreeMap<String, Vec<Victim>>>` — `None` when any host cannot make room; a group as a victim is evicted whole (its members on all hosts).

- [ ] **Step 1: Write the failing tests**

```rust
// T16, T27 (owner check 4): if one named host cannot make room, nothing is evicted anywhere.
#[test]
fn eviction_is_all_or_nothing_across_hosts() {
    let views = views(&[("host-a", free(10), &[victim("x", 80)]), ("host-b", free(10), &[])]);
    let need = need(&[("host-a", 80), ("host-b", 80)]);
    assert!(plan_group_eviction(&views, &need).is_none());
}

// T16: each host evicts only what it needs.
#[test]
fn each_host_evicts_its_minimum() {
    let views = views(&[("host-a", free(10), &[victim("x", 80), victim("y", 10)]),
                        ("host-b", free(90), &[victim("z", 50)])]);
    let plan = plan_group_eviction(&views, &need(&[("host-a", 80), ("host-b", 80)])).unwrap();
    assert_eq!(names(&plan["host-a"]), ["x"]);
    assert!(plan["host-b"].is_empty());
}

// T16: A -> B -> A with a group as A and a single-rank deployment on host A as B.
#[tokio::test]
async fn group_and_single_rank_alternate() {
    let world = GroupWorld::ready_two_host_group("g").await.with_single_rank("s", "host-a");
    world.request("s").await;
    assert_eq!(world.state("g").await, "parked"); // whole group, both hosts
    world.request("g").await;
    assert_eq!(world.state("g").await, "ready");
    world.assert_release_evidence_per_member("g");
}
```

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p mllm-scheduler && cargo test -p mllm-controller tests_groups` — expected: unresolved.

- [ ] **Step 3: Implement.** Compute every named host's victim set first; act only when all are `Some`; a group victim appears on each of its hosts and is parked or stopped as one unit through Task 16 or Task 14. Comment `// ADR 0020 §4, SPEC §11: validate all hosts before evicting`.

- [ ] **Step 4: Run the tests.** Run the core suite — expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-scheduler crates/mllm-controller
git commit -m "feat(switching): all-or-nothing group eviction across named hosts (ADR 0020 §4)"
```

---

### Task 16: Group park and wake with per-rank evidence and the canary

**Files:**
- Create: `crates/mllm-controller/src/group_residency.rs`
- Modify: `crates/mllm-controller/src/coordinator.rs`, `crates/mllm-agent/src/process_residency.rs` (report residency per member handle), `crates/mllm-store/src/groups.rs` (member parked charge, canary reference)
- Test: `crates/mllm-controller/src/coordinator/tests_groups.rs`

**Interfaces:**
- Consumes: `FakeGroup` faults (Task 12), `stop_group` (Task 14).
- Produces:
  - `pub async fn park_group(ctx, plan: &GroupPlan) -> Result<(), GroupResidencyError>` — head `/sleep` once, then wait for every member's residency report at or below its parked budget within the park deadline.
  - `pub async fn wake_group(ctx, plan: &GroupPlan) -> Result<(), GroupResidencyError>` — head `/wake_up` once, every member resident again, head readiness, then the canary.
  - `pub struct CanaryReference { pub prompt: String, pub tokens: Vec<u32> }` recorded at first readiness with `temperature: 0`, `max_tokens: 8`; the store keeps it per generation.
  - `GroupResidencyError::{MemberSilent { rank }, MemberResident { rank }, CanaryMismatch}` → codes `group_member_uncertain`, `group_member_failed`, `group_wake_mismatch`; each triggers `stop_group`.

- [ ] **Step 1: Write the failing tests**

```rust
// T20: park settles only when every rank reports its memory released; one collective only.
#[tokio::test]
async fn park_needs_every_rank() {
    let world = GroupWorld::ready_two_host_group("g").await;
    world.park("g").await.unwrap();
    assert_eq!(world.group.sleep_calls(), 1);
    assert_eq!(world.state("g").await, "parked");
    assert_eq!(world.owner_bytes_on("host-b", &member_owner_id("g", 0, 1)), world.member_parked_budget());
}

// Review Focus 4, T20: the head's sleep succeeds but rank 1 stays resident: full charge kept, group stopped.
#[tokio::test]
async fn resident_rank_after_sleep_stops_the_group() {
    let world = GroupWorld::ready_two_host_group("g").await;
    world.group.sleep_leaves_rank_resident(1);
    let err = world.park("g").await.unwrap_err();
    assert_eq!(err.code(), "group_member_failed");
    assert_eq!(world.group.sleep_calls(), 1); // never repeated
    assert_ne!(world.state("g").await, "parked");
    world.wait_settled_generation("g", 1).await;
}

// T20: a member that never reports keeps its full charge and is uncertain.
#[tokio::test]
async fn silent_member_keeps_full_charge() {
    let world = GroupWorld::ready_two_host_group("g").await;
    world.group.disconnect_host("host-b");
    let _ = world.park("g").await;
    assert_eq!(world.owner_bytes_on("host-b", &member_owner_id("g", 0, 1)), world.member_request());
    assert_eq!(world.status("g").await.member(1).state, "uncertain");
}

// T20 (owner check 6): a wake whose canary differs stops the group.
#[tokio::test]
async fn wake_canary_mismatch_stops_the_group() {
    let world = GroupWorld::ready_two_host_group("g").await;
    world.park("g").await.unwrap();
    world.group.wake_output(vec![9, 9, 9]);
    let err = world.wake("g").await.unwrap_err();
    assert_eq!(err.code(), "group_wake_mismatch");
}

// T20: five clean cycles keep the canary identical.
#[tokio::test]
async fn repeated_cycles_are_clean() {
    let world = GroupWorld::ready_two_host_group("g").await;
    for _ in 0..5 {
        world.park("g").await.unwrap();
        world.wake("g").await.unwrap();
    }
    assert_eq!(world.group.sleep_calls(), 5);
}
```

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p mllm-controller tests_groups` — expected: unresolved.

- [ ] **Step 3: Implement.** Only the head's agent receives `Park`/`Restore` (existing actions, member id `head`); worker agents receive nothing new — they already report `process_residency` for their member handle, now keyed by member. Parked charge per member moves to its parked budget only on that member's report. `max_parked` counts the group once on each host. Comments cite `// ADR 0020 §10, SPEC §11: the lead agent invokes each collective once`.

- [ ] **Step 4: Run the tests.** Run the core suite — expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-controller crates/mllm-agent crates/mllm-store
git commit -m "feat(controller): group deep park and wake with per-rank evidence and a wake canary (ADR 0020 §10)"
```

---

### Task 17: Status, codes and exits

**Files:**
- Modify: `crates/mllm-management/src/status.rs` (group instance rows), `crates/mllm-cli/src/output.rs` (codes → exits), `docs/operations/install.md` (a "Multi-host groups" section: host `groups` block, root prerequisites mllm checks, the known risk)
- Test: `crates/mllm-cli/tests/output_codes.rs` (or the existing codes test), `crates/mllm-management` status tests

**Interfaces:**
- Produces: status JSON for a group instance: `{"topology": {"tensor_parallel":2,"pipeline_parallel":1}, "rendezvous_port": 25000, "peer_transport": "unauthenticated", "members": [{"host","node_rank","role","state","processes","reservation","residency","last_error","warnings"}]}`.

- [ ] **Step 1: Write the failing tests**

```rust
// T14: every closed group code maps to its exit.
#[test]
fn group_codes_map_to_exits() {
    for (code, exit) in [
        ("group_placement_required", 2), ("group_topology_invalid", 2), ("peer_address_missing", 2),
        ("group_shape_unsupported", 5), ("group_instances_unsupported", 5),
        ("group_engine_unsupported:sglang", 5), ("rendezvous_ports_exhausted", 4),
    ] {
        assert_eq!(exit_for_code(code).0, exit, "{code}");
    }
}

// T21: a group instance is marked unauthenticated and lists every member.
#[test]
fn status_marks_peer_transport_and_members() {
    let s = render_status(&two_member_group_status());
    assert_eq!(s["instances"][0]["peer_transport"], "unauthenticated");
    assert_eq!(s["instances"][0]["members"].as_array().unwrap().len(), 2);
    assert_eq!(s["instances"][0]["members"][1]["role"], "worker");
}
```

Use the real function names in `output.rs` (the match at `output.rs:96` and `:149`) in place of `exit_for_code`.

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p mllm-cli && cargo test -p mllm-management` — expected: fail.

- [ ] **Step 3: Implement** the mappings (prefix match for `group_engine_unsupported:`, `host_tuning_*:`), the status rows and the install section.

- [ ] **Step 4: Run the tests.** Run: `cargo test --workspace --all-targets --locked` — expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-cli crates/mllm-management docs/operations/install.md
git commit -m "feat(cli): group status rows, peer-transport marker and exit codes (ADR 0020 §14, §15)"
```

---

### Task 18: Live rows MH1–MH6 on two hosts with a stock build

**Files:**
- Create: `scripts/live/matrix/rows/MH1.sh` … `MH6.sh`, `scripts/live/matrix/rdma_counters.py`
- Modify: `scripts/live/matrix/hosts.example.env` (document `HOST_A_DIRECT`, `HOST_B_DIRECT`, `GROUP_MODEL`), `scripts/live/matrix/gen_deployment.py` (topology), `scripts/live/matrix/gen_host_doc.py` (`groups` block), `docs/runbooks/f2-current-status.md`

**Interfaces:**
- Consumes: the whole feature; `rowlib.sh` helpers.
- Produces: `rdma_counters.py <host>` prints the summed `port_xmit_data`/`port_rcv_data` of `/sys/class/infiniband/*/ports/*/counters/` as JSON; read-only.

Prerequisites (owner, as root, before MH1; mllm checks and never changes them): `vm.compaction_proactiveness=0`, unlimited memlock for the service user, read-write `/dev/infiniband/uverbs*` for the service user, the direct-link addresses declared in each host's `groups.peer_address`. A stock vLLM 0.30 venv registered on both hosts with the same `--name` and build fingerprint. A model stock vLLM loads at TP=2, present on both hosts with equal digests.

- [ ] **Step 1: Write the counter helper and its unit test** (`python3 -m unittest` against a temp sysfs tree): sums every port's counters; a missing tree prints `{"present": false}`.

- [ ] **Step 2: Write MH1.** Deploy the group through the server with `placement.hosts: [host A, host B]`, `topology.tensor_parallel: 2`, `residency: deep`; wait READY; read counters on both hosts; send 20 completions through the router; read counters again. Pass: READY, 20/20 completions, counters rose on both hosts (records `transport: rdma`), else record `transport: socket` and still pass bring-up but flag it in the runbook.

- [ ] **Step 3: Write MH2–MH6** per spec §16: MH2 five park/wake cycles with residency per host and the canary; MH3 `signal_owned.py` kills host B's engine; MH4 kills host A's engine; MH5 stops host B's agent service through the CLI (no firewall change), checks `uncertain` and the charge, restarts it, checks settlement; MH6 alternates the group with a single-rank deployment on host A. Every row cleans up on failure (`rowlib.sh` trap) and prints evidence lines only.

- [ ] **Step 4: Run MH1–MH6 on host A and host B.** Run: `scripts/live/matrix/run_row.sh MH1` … `MH6`. Expected: pass. On a failure, debug before continuing; record the fix in the commit.

- [ ] **Step 5: Record and commit.** Add one section to `docs/runbooks/f2-current-status.md` with the commit range, each row's result and the transport observed, and the sentence "CPU and Fake-engine tests are not qualification; these rows are."

```bash
git add scripts/live/matrix docs/runbooks/f2-current-status.md
git commit -m "test(live): multi-host group rows MH1-MH6 on two hosts with a stock build"
```

---

### Task 19: Patched vLLM 0.30 and the Flash-Next benchmark (MH7, MH8)

**Files:**
- Create: `scripts/live/matrix/rows/MH7.sh`, `MH8.sh`
- Modify: `docs/benchmarks/` (new `2026-MM-DD-two-host-flash-next.md` on the day it runs), `docs/runbooks/f2-current-status.md`

Prerequisites: the patched vLLM 0.30 rebuilt as a new venv on each host from the published recipe's patch set (decision 1; no existing environment changes), registered with `mllm engine add <env> --name vllm-030-patched` on both hosts with equal build fingerprints; the recipe's non-transport variables in `security.approved_env` and `env` (Task 4); the hibrid48 checkpoint downloaded on both hosts through mllm (Task 13a; about 105 GB each).

- [ ] **Step 1: Write MH7.** Deploy with the recipe's flags as `engine_config`/`extra_args` (`--speculative-config` MTP K=5, `--block-size 1632`, `--kv-cache-memory`, `--max-num-seqs 64`, `--max-num-batched-tokens 8192`, `--moe-backend marlin`, `--load-format fastsafetensors`, `--async-scheduling`, `--enable-prefix-caching`, the parsers), `timeouts.initialize: 1800s`. Run `bench.py` through the router at concurrency 1, 2, 4, 8, 16, 32, 64, three runs each, reporting average and peak tok/s and TTFT, plus the RDMA counters.

- [ ] **Step 2: Run MH7.** Pass: average ≥ 95 tok/s at 1 and ≥ 735 tok/s at 64 (decision 13). A miss is recorded with the transport evidence, not tuned by setting NCCL variables (decision 4); it goes to the owner.

- [ ] **Step 3: Write and run MH8.** One deep park and wake of the Flash-Next group with per-rank evidence and the canary; then one c=1 run to check no throughput loss after wake.

- [ ] **Step 4: Record.** Benchmark document (method, pinned build fingerprint, numbers, peaks) and the status runbook section.

- [ ] **Step 5: Commit**

```bash
git add scripts/live/matrix docs/benchmarks docs/runbooks/f2-current-status.md
git commit -m "test(live): two-host Flash-Next TP2 MTP benchmark through the router (MH7, MH8)"
```

---

### Task 20: SGLang groups, restart-only

**Files:**
- Modify: `crates/mllm-adapters/src/sglang/args.rs` (render `--tp`, `--nnodes`, `--node-rank`, `--dist-init-addr`), `runtime/sglang_server_args.py` (group mode: compare rendered values instead of pinning `tp_size=1`), `runtime/sglang_entry.py` (no file rendezvous in group mode), `crates/mllm-agent/src/native_execution.rs` (lift `group_engine_unsupported:sglang`), `crates/mllm-config` (a SGLang group with `residency: deep` refused `capability_missing:deep_park`)
- Test: `crates/mllm-adapters` SGLang args tests, `runtime/tests/test_sglang_server_args.py`, `runtime/tests/test_sglang_entry.py`, a new live row `MH9.sh`

- [ ] **Step 1: Write the failing tests**

```rust
// T22: SGLang group rendering; worker health is never used.
#[test]
fn sglang_group_renders_node_rank_and_dist_init() {
    let cmd = render_sglang(&sglang_group_input(1)).unwrap();
    for (f, v) in [("--tp", "2"), ("--nnodes", "2"), ("--node-rank", "1"),
                   ("--dist-init-addr", "192.0.2.10:25000")] {
        assert!(has_pair(&cmd.argv, f, v), "{f} {v}");
    }
}

// T22 (owner decision 12): a SGLang group is restart_only until release/resume across ranks is proven.
#[test]
fn sglang_group_deep_is_refused() {
    let err = resolve_group_deployment("sglang", "deep").unwrap_err();
    assert!(err.to_string().contains("capability_missing:deep_park"));
}
```

```python
# T22: group mode compares rendered tp/nnodes/node_rank/dist_init_addr; single-rank pin unchanged.
def test_group_mode_compares_rendered_values(self):
    ...  # build a namespace from rendered args with MLLM_GROUP_MODE=1; drift in node_rank raises group_drift:node_rank
```

(Write the Python test body in the style of the existing `test_sglang_server_args.py` cases, asserting `group_drift:node_rank` for a drifted namespace and the unchanged pin without `MLLM_GROUP_MODE`.)

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p mllm-adapters sglang && python3 -m unittest discover -s runtime/tests` — expected: fail.

- [ ] **Step 3: Implement** the rendering and group mode. Readiness remains head-only; the nonzero-rank dummy health server is never probed (`// ADR 0020 §8: its health always passes`).

- [ ] **Step 4: Run tests, then MH9 live.** MH9: a SGLang TP=2 group with a small model on host A and host B, READY through the router, worker kill stops the group (as MH3). Not Flash-Next.

- [ ] **Step 5: Commit**

```bash
git add crates runtime scripts/live/matrix docs/runbooks/f2-current-status.md
git commit -m "feat(sglang): restart-only multi-host groups (ADR 0020 §8, decision 12)"
```

---

## Self-review notes

- Spec coverage: §2 config → Tasks 2, 3, 4; §3 plan → 5; §4 reservations and ports → 7, 15; §5 preparation and weights → 8, 13a; §6 fan-out → 11, 13b; §7 readiness → 13b; §8 rendering → 9, 10, 20; §9 stop and failure → 14; §10 park and wake → 16; §11 host checks → 8; §12 exposure → 1, 9, 10, 17; §13 capability → 6; §14 status → 17; §15 codes → 2, 7, 8, 13a, 13b, 16, 17; §16 testing → every task plus 18, 19, 20; ADR 0020 and the ADR 0012 amendment → 1.
- Added during review: `rendezvous_port_in_use:<port>` and `service_port_in_use:<port>` (Prepare refusals for Review Focus 2) are in the spec's §15 table.
- Type names used across tasks: `GroupShape`/`Topology` (config, Task 2) are distinct from `GroupTopology` (domain, Task 5); Task 13b converts one to the other. `member_owner_id`, `GroupReservation`, `GroupSettlement`, `MemberGone` (7) are used by 13b, 14, 16. `VllmGroupArgs` (9) is used by 11.
- CPU and Fake-engine tests are not qualification; MH1–MH9 are.

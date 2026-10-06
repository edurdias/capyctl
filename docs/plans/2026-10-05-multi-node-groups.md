# Multi-node Engine Groups Implementation Plan

**Execution:** implement task by task in order; each task ends green on its own tests and is committed before the next starts. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run one engine instance across N named hosts (tensor parallel × pipeline parallel, one device per host in this version) for vLLM 0.30.0, SGLang 0.5.21 and TensorFold 0.6.5 through one group mechanism: per-rank reservations, a server-allocated rendezvous port, parallel weights with cross-host digest agreement, concurrent fan-out launch, head-only readiness, group park and wake with per-rank evidence, whole-group stop on any rank failure, and live proof on two GB10 hosts through the router.

**Architecture:** A deployment gains `topology` and an exact, rank-ordered `placement.hosts`. The server writes a durable N-member group plan and reserves every member (one owner per member, on its own host) plus a rendezvous port from the head's declared range in one store transaction. Every host prepares (host checks, digest, ports), then launches its member concurrently; the head serves HTTP on loopback behind the existing keys, workers serve nothing. The controller, store, agent and protocol see only the plan; each adapter turns one member into its engine's command through one shared `GroupMemberArgs`. The head's agent leads readiness and park/wake; worker agents supply identities, residency and stop evidence. All protocol additions sit behind a new ADR 0017 capability, `engine_groups`.

**Tech Stack:** Rust workspace (tokio, tonic/prost, axum, rusqlite, clap, serde/serde_json, strict YAML), Python 3 runtime helpers under `runtime/` (unittest), bash live harness under `scripts/live/matrix/`.

**Spec:** `docs/specs/2026-10-05-multi-node-groups-design.md`. Read it with this plan. Governing documents: `docs/SPEC.md` §6, §7, §9, §11, §13, §15, §16.4, §20; ADR 0007, 0011, 0012, 0013, 0014, 0016, 0017, 0018, 0023, 0025; `AGENTS.md`.

## Global Constraints

- A deployment without `topology` (or with world size 1) behaves byte-identically to today: command identity, effective configuration, recipe fingerprint and stored digests unchanged. A host document without `resource_policy.groups` keeps its policy digest. A profile without `env`/`approved_env` is written exactly as before.
- Cite the governing requirement inline where behaviour is spec-driven, e.g. `// ADR 0028 §5: ...`, `// SPEC §11: ...`. ADR 0028 sections mirror the spec's §1–§14 one to one.
- Tag every new test with its acceptance-matrix ID: T03, T14 (configuration), T15, T16, T20, T27, T30, T31, T32 (lifecycle and accounting), T21, T37 (security), T22 (SGLang conformance), T34 (capability gating), T39 (unchanged single-host behaviour).
- Uncertainty keeps accounting: a member is released only on its own host's gone-evidence; an unreachable host's member stays charged and `uncertain`; lease expiry never frees it; the rendezvous port is released only after every member settles.
- Additive protocol only: no field renumbered, `PROTOCOL_VERSION` stays `"2"`, `COMMAND_ENCODING_VERSION` stays `"1"`. New capability: `engine_groups`.
- CapyCTL-owned environment names, never settable by profile or deployment, by YAML or by flag: prefixes `NCCL_`, `GLOO_`, `MASTER_`, `CAPYCTL_`, `LD_`; names `VLLM_HOST_IP`, `SGLANG_HOST_IP`, `HOST_IP`, `SGLANG_LOCAL_IP_NIC`, `SGLANG_DISTRIBUTED_INIT_METHOD_OVERRIDE`, `TF_COMM_BACKEND`, `TF_NCCL_LIB`, `PATH`, `PYTHONPATH`, `CUDA_HOME`, `CUDA_VISIBLE_DEVICES`.
- A group launch renders no `NCCL_*` or `GLOO_*` variable and inherits none. It renders the head's peer address as the engine's rendezvous address and the member's own peer address as `VLLM_HOST_IP` (vLLM) or `SGLANG_HOST_IP` (SGLang). The API server and every control endpoint stay on loopback with the per-launch keys and the key guard (ADR 0012).
- CapyCTL never changes sysctls, limits, device permissions or firewalls; it reads them.
- Every new setting comes three ways with one precedence, flag > env > YAML > default: `--peer-address`/`CAPYCTL_PEER_ADDRESS`/`resource_policy.groups.peer_address`; `--rendezvous-ports`/`CAPYCTL_RENDEZVOUS_PORTS`/`resource_policy.groups.rendezvous_port_range` (default `25000-25099` inclusive); `--require-rdma`/`CAPYCTL_REQUIRE_RDMA`/`resource_policy.groups.require_rdma` (default `false`).
- Closed codes (spec §16), with exits: 2 for `group_placement_required`, `group_topology_invalid`, `group_profile_mismatch`, `group_checkpoint_mismatch`, `group_model_path_mismatch`, `peer_address_missing`, `peer_address_not_local`, `engine_env_reserved:<name>`, `engine_env_not_approved:<name>`, `engine_env_conflict:<name>`; 4 for `rendezvous_ports_exhausted`, `rendezvous_port_in_use:<port>`, `service_port_in_use:<port>`, `host_tuning_missing:<item>`; 5 for `group_shape_unsupported`, `group_shape_unsupported:<engine>`, `group_instances_unsupported`, `group_drift:<field>`, `host_capability_missing:engine_groups`; no exit for `host_tuning_warning:<item>`, `group_member_failed`, `group_member_uncertain`, `group_wake_mismatch`. Items: `compaction`, `memlock`, `infiniband`. No new exit number.
- Engine group support (spec §2): vLLM and SGLang any N dividing TP × PP, deep park; TensorFold TP 2, PP 1, exactly 2 hosts, restart-only.
- In tracked files, commits and the PR: no machine names, addresses or home paths. The hosts are "host A" (head) and "host B" (worker); the link is "the direct link". Example and test addresses use `192.0.2.0/24` and `198.51.100.0/24`. Live addresses come only from the untracked `scripts/live/matrix/hosts.local.env`.
- CPU and Fake-engine tests are not qualification; the live rows MN1–MN9 are. Say so in every status claim.
- Verification before every commit: `scripts/ci-local.sh`; before merge, `scripts/ci-local.sh --deep` with its result in the PR body.

## Review Focus

1. **An approval glob written too wide (`S*`, `V*`, `T*`) or an engine that reads an address under another name.** A glob must never admit a CapyCTL-owned name, and group members must render equal environments apart from the one address variable. Pinned in Task 2 (`owned_names_are_never_settable` with wide globs, including `SGLANG_HOST_IP` and `TF_COMM_BACKEND`) and Task 16 (member environments compared).
2. **The head's rendezvous port, or a SGLang worker's loopback port, is already taken by something outside CapyCTL** (a stale engine from before a crash). Prepare must refuse with the port named and release every reservation, not launch and hang for the whole initialization timeout. Pinned in Task 9.
3. **A worker host reconnects mid-run with a restarted agent and an empty journal.** Its member must not be declared gone because the journal is empty; it settles only on recorded identities, and the group must not relaunch while it is unsettled. Pinned in Task 17.
4. **Park succeeds on the head's call but one rank never frees memory** (the TP > 1 sleep bug class). The group must keep that member's full charge and stop the group, not report parked. Pinned in Task 19.
5. **Two group deployments sharing host B activate at once.** Neither may hold host B's share while failing on its other host; reservations are all-or-nothing. Pinned in Task 8.
6. **A SGLang worker ignores SIGTERM after the head died.** Stop must escalate and settle, never wait forever. Pinned in Task 13.

---

## File map

| File | Responsibility | Tasks |
|---|---|---|
| `docs/design/adr/0028-multi-node-engine-groups.md` (new), ADR 0012, 0013, 0023, `docs/SPEC.md`, `AGENTS.md` | decision record and amendments | 1 |
| `crates/capyctl-config/src/engine_env.rs` (new), `engine_policy.rs`, `schema.rs`, `effective/*`, `crates/capyctl-adapters/src/engine_env.rs` | engine environment, approvals, provenance, launch env | 2 |
| `crates/capyctl-cli/src/grammar.rs`, `engine.rs`, `client.rs`, `crates/capyctl-config/src/registration.rs` | `--env`, `--approve-env`, `--engine-env` | 3 |
| `crates/capyctl-config/src/topology.rs` (new), `group_support.rs` (new), `instances.rs`, `schema.rs` | `topology`, group placement rules, per-engine shape support | 4 |
| `crates/capyctl-config/src/groups_policy.rs` (new), `schema.rs`, `crates/capyctl-cli/src/grammar.rs`, role settings resolution | host `groups` policy, three ways | 5 |
| `crates/capyctl-domain/src/group.rs` | N-member `GroupPlan`, `MemberRole`, `GroupEngine` | 6 |
| `crates/capyctl-protocol/proto/capyctl/management/v1/management.proto`, `src/execution.rs`, `src/capabilities.rs` | wire fields, `engine_groups` | 7 |
| `crates/capyctl-store/src/groups.rs` (new), `instances.rs`, `schema.rs`/migrations, `checkpoint_digests.rs` | group plans, member owners, reservation, ports, per-host digests | 8 |
| `crates/capyctl-agent/src/host_checks.rs` (new), `native_execution.rs` | host checks, peer address, Prepare | 9 |
| `crates/capyctl-adapters/src/group.rs` (new), `vllm/args.rs`, `vllm/frozen.rs`, `runtime/vllm_entry.py`, `runtime/loopback_rendezvous.py` | shared member args; vLLM rendering and entry group mode | 10 |
| `crates/capyctl-adapters/src/sglang/*`, `runtime/sglang_server_args.py`, `runtime/sglang_entry.py` | SGLang rendering and group mode | 11 |
| `crates/capyctl-adapters/src/tensorfold/args.rs` | TensorFold two-rank rendering | 12 |
| `crates/capyctl-agent/src/native_execution.rs`, `journal.rs` | member launch and terminate for three engines | 13 |
| `crates/capyctl-testkit/src/fake_group.rs` (new), `crates/capyctl-controller/src/coordinator/tests_groups.rs` (new, `GroupWorld`) | fake multi-host harness | 14 |
| `crates/capyctl-controller/src/group_sources.rs` (new), `model_sources.rs`, `checkpoint_digests.rs` | parallel weights, digest agreement | 15 |
| `crates/capyctl-controller/src/group_activation.rs` (new), `remote_execution.rs`, `remote_readiness.rs`, `crates/capyctl-management/src/configuration.rs`, `crates/capyctl-router/src/balance.rs` | reserve, prepare, fan-out, head readiness, route | 16 |
| `crates/capyctl-controller/src/group_settlement.rs` (new) | stop, failure, settlement, uncertainty, recovery | 17 |
| `crates/capyctl-scheduler/src/placement.rs`, `crates/capyctl-store/src/ordinary_lifecycle/switching.rs` | group eviction across named hosts | 18 |
| `crates/capyctl-controller/src/group_residency.rs` (new), agent residency per member | park/wake, per-rank evidence, canary, restart-only | 19 |
| `crates/capyctl-management/src/status.rs`, `crates/capyctl-cli/src/output.rs`, docs | status, codes, exits, user docs | 20 |
| `scripts/live/matrix/rows/MN*.sh`, `scripts/live/matrix/rdma_counters.py` (new), status runbook | live rows | 21, 22 |

---

### Task 1: ADR 0028 and the amendments

**Files:**
- Create: `docs/design/adr/0028-multi-node-engine-groups.md`
- Modify: `docs/design/adr/0012-deep-park-default-on.md` (append the peer-exposure amendment), `docs/design/adr/0013-deployment-instances-and-placement.md` (decision 1 and open issue 7 point at ADR 0028), `docs/design/adr/0023-tensorfold-engine.md` (multi-rank: in scope under ADR 0028), `docs/SPEC.md` §11, §15, §16.4, §20, `AGENTS.md` (one hard-constraint line)

**Interfaces:**
- Produces: the section numbers code cites, ADR 0028 §1 vocabulary, §2 configuration (§2.1 engine environment), §3 host policy, §4 group plan, §5 reservations, §6 weights, §7 prepare and host checks, §8 fan-out, §9 readiness and router, §10 adapters, §11 stop and failure, §12 park and wake, §13 ports and exposure, §14 protocol.

- [ ] **Step 1: Write ADR 0028.** Format of ADR 0023: `# ADR 0028 — Multi-node engine groups`, `**Status:** Accepted (owner decisions 2026-09-25 and 2026-10-05).`, `**Amends:** SPEC §11, §15, §16.4, §20; ADR 0012; ADR 0013 decision 1; ADR 0023.`, `**Related:** ADR 0007, 0011, 0016, 0017; docs/specs/2026-10-05-multi-node-groups-design.md`. Sections: Context (spec "Problem"), Decision (the thirteen owner decisions and the engine-environment rule verbatim, then §1–§14 condensed from the spec, each a short paragraph), Honest scope ("CPU and Fake-engine tests are not qualification; MN1–MN9 are"), Consequences.

- [ ] **Step 2: Append the ADR 0012 amendment.**

```markdown
## Amendment 2026-10-05 — peer exposure of engine groups (ADR 0028)

Owner decision 3 of 2026-09-25: during a multi-node group run, the engine's
rendezvous store, vLLM's broadcast queue, TensorFold's extra serving port and
NCCL open unauthenticated listeners on every interface of every member host,
including any wireless or overlay network. Anyone who can reach them can disturb
or crash the group. CapyCTL renders the direct-link addresses where the engine
takes them, performs no firewall check, and leaves the network to the operator.
This is a recorded known risk, not a protection. Every protection in decision 5
above is unchanged: the API server and every control endpoint stay on loopback
with the per-launch keys and the guard, and no control path reaches host ingress
or the router. Status marks a group instance `peer transport: unauthenticated`.
```

- [ ] **Step 3: Amend ADR 0013, ADR 0023, SPEC and AGENTS.md.** SPEC §16.4: replace `head: host-a` with a comment "the first host is the head"; replace "Multi-host group placement is not yet specified." with "Group placement: ADR 0028." SPEC §20: below the matrix, "Multi-node live rows MN1–MN9 (ADR 0028) supplement T16, T20, T22, T30–T32 on two hosts." AGENTS.md hard constraints: "A multi-node group run opens unauthenticated peer listeners on every interface of its hosts (ADR 0012 amendment, ADR 0028 §13)."

- [ ] **Step 4: Check.** Run: `grep -n "0028" docs/SPEC.md docs/design/adr/*.md AGENTS.md` — expected: every amended file cites ADR 0028. Run: `git diff --stat` — docs only. Run: `scripts/check-name.sh` — expected: pass.

- [ ] **Step 5: Commit**

```bash
git add -u && git add docs/design/adr/0028-multi-node-engine-groups.md
git commit -m "docs: ADR 0028 multi-node engine groups and the peer-exposure amendment"
```

---

### Task 2: Engine environment in configuration

**Files:**
- Create: `crates/capyctl-config/src/engine_env.rs`
- Modify: `crates/capyctl-config/src/engine_policy.rs` (`validate_profile_env` delegates), `crates/capyctl-config/src/schema.rs` (profile `security` gains `approved_env: Seq(SCALAR)`; deployment `engine_config` gains `env: MapOf(SCALAR)`; top-level deployment `env` stays `Struct(NO_FIELDS)`), `crates/capyctl-config/src/effective/core.rs` (merge, provenance, fingerprint), `crates/capyctl-adapters/src/engine_env.rs` (launch environment takes the resolved engine env), the vLLM and TensorFold env allowlists (`vllm/initialize.rs`, `tensorfold/args.rs`) admit resolved names, `crates/capyctl-config/src/lib.rs`
- Test: `crates/capyctl-config/tests/engine_env.rs`; unit test in `crates/capyctl-adapters/src/engine_env.rs`

**Interfaces:**
- Produces (in `capyctl_config::engine_env`):
  - `pub const OWNED_PREFIXES: &[&str] = &["NCCL_", "GLOO_", "MASTER_", "CAPYCTL_", "LD_"];`
  - `pub const OWNED_NAMES: &[&str] = &["VLLM_HOST_IP", "SGLANG_HOST_IP", "HOST_IP", "SGLANG_LOCAL_IP_NIC", "SGLANG_DISTRIBUTED_INIT_METHOD_OVERRIDE", "TF_COMM_BACKEND", "TF_NCCL_LIB", "PATH", "PYTHONPATH", "CUDA_HOME", "CUDA_VISIBLE_DEVICES"];`
  - `pub fn is_owned(name: &str) -> bool`.
  - `pub struct ApprovedEnv(Vec<String>)`; `ApprovedEnv::parse(entries: &[String]) -> Result<ApprovedEnv, EnvRefusal>`; `fn admits(&self, name: &str) -> bool`.
  - `pub enum EnvSource { Profile, Deployment }`.
  - `pub struct ResolvedEnv { pub vars: BTreeMap<String, (String, EnvSource)> }` with `fn values(&self) -> BTreeMap<String, String>`.
  - `pub fn resolve_engine_env(profile_env: &BTreeMap<String, String>, approved: &ApprovedEnv, deployment_env: &BTreeMap<String, String>) -> Result<ResolvedEnv, EnvRefusal>`.
  - `pub enum EnvRefusal { Reserved(String), NotApproved(String), Conflict(String), Invalid(String) }` with `fn code(&self) -> String` → `engine_env_reserved:<name>`, `engine_env_not_approved:<name>`, `engine_env_conflict:<name>`, and for `Invalid` the detail naming the entry.

- [ ] **Step 1: Write the failing tests**

```rust
// crates/capyctl-config/tests/engine_env.rs
use capyctl_config::engine_env::*;
use std::collections::BTreeMap;

fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}
fn approved(entries: &[&str]) -> ApprovedEnv {
    ApprovedEnv::parse(&entries.iter().map(|e| e.to_string()).collect::<Vec<_>>()).unwrap()
}

// T37: a deployment name must match an approval; globs match by prefix.
#[test]
fn deployment_names_need_an_approval() {
    let a = approved(&["SGLANG_ENABLE_*", "VLLM_MARLIN_USE_ATOMIC_ADD"]);
    assert!(resolve_engine_env(&map(&[]), &a, &map(&[("SGLANG_ENABLE_JIT_DEEPGEMM", "0")])).is_ok());
    assert!(resolve_engine_env(&map(&[]), &a, &map(&[("VLLM_MARLIN_USE_ATOMIC_ADD", "1")])).is_ok());
    let err = resolve_engine_env(&map(&[]), &a, &map(&[("TORCH_NCCL_HEARTBEAT_TIMEOUT_SEC", "180")])).unwrap_err();
    assert_eq!(err.code(), "engine_env_not_approved:TORCH_NCCL_HEARTBEAT_TIMEOUT_SEC");
}

// T37: the profile's own env is host-authored and needs no approval.
#[test]
fn profile_env_needs_no_approval() {
    let r = resolve_engine_env(&map(&[("TORCH_NCCL_ASYNC_ERROR_HANDLING", "1")]), &approved(&[]), &map(&[])).unwrap();
    assert_eq!(r.vars["TORCH_NCCL_ASYNC_ERROR_HANDLING"].1, EnvSource::Profile);
}

// T21, T37, Review Focus 1: owned names are refused at both levels, even through wide globs.
#[test]
fn owned_names_are_never_settable() {
    let a = approved(&["N*", "G*", "M*", "V*", "S*", "T*", "H*"]);
    for name in ["NCCL_IB_HCA", "GLOO_SOCKET_IFNAME", "MASTER_ADDR", "VLLM_HOST_IP",
                 "SGLANG_HOST_IP", "HOST_IP", "SGLANG_LOCAL_IP_NIC",
                 "SGLANG_DISTRIBUTED_INIT_METHOD_OVERRIDE", "TF_COMM_BACKEND", "TF_NCCL_LIB",
                 "CAPYCTL_ENGINE_KEY", "LD_PRELOAD", "PATH", "CUDA_VISIBLE_DEVICES"] {
        let e = resolve_engine_env(&map(&[]), &a, &map(&[(name, "x")])).unwrap_err();
        assert_eq!(e.code(), format!("engine_env_reserved:{name}"));
        let e = resolve_engine_env(&map(&[(name, "x")]), &a, &map(&[])).unwrap_err();
        assert_eq!(e.code(), format!("engine_env_reserved:{name}"));
    }
}

// T03: approval entries are validated; a bare glob and owned-only entries are refused.
#[test]
fn approval_entries_are_validated() {
    for bad in ["*", "mbx_*", "MBX_**", "A*B", "NCCL_*", "VLLM_HOST_IP", "TF_COMM_BACKEND", ""] {
        assert!(ApprovedEnv::parse(&[bad.to_string()]).is_err(), "{bad}");
    }
    assert!(ApprovedEnv::parse(&(0..65).map(|i| format!("V{i}")).collect::<Vec<_>>()).is_err());
}

// T14: the deployment overrides the profile for one name, with provenance.
#[test]
fn deployment_overrides_profile_with_provenance() {
    let r = resolve_engine_env(&map(&[("SGLANG_ENABLE_X", "1")]), &approved(&["SGLANG_ENABLE_*"]),
        &map(&[("SGLANG_ENABLE_X", "0")])).unwrap();
    assert_eq!(r.vars["SGLANG_ENABLE_X"], ("0".to_string(), EnvSource::Deployment));
}

// T03: values are bounded and single-line; the safe names keep their rules.
#[test]
fn values_are_bounded_and_safe_names_keep_rules() {
    let a = approved(&["MBX_*"]);
    assert!(resolve_engine_env(&map(&[]), &a, &map(&[("MBX_X", "a\nb")])).is_err());
    assert!(resolve_engine_env(&map(&[]), &a, &map(&[("MBX_X", &"x".repeat(4097))])).is_err());
    assert!(resolve_engine_env(&map(&[]), &a, &map(&[("MAX_JOBS", "4")])).is_ok());
    assert!(resolve_engine_env(&map(&[]), &a, &map(&[("MAX_JOBS", "0")])).is_err());
}
```

Add to `crates/capyctl-config/tests/effective.rs` (use the file's real fixture names; search `fn resolve` and the recipe-fingerprint accessor in `crates/capyctl-config/src/effective/`):

```rust
// T14, T39: no env keeps the fingerprint; a deployment env shows with its source.
#[test]
fn engine_env_is_in_the_effective_configuration() {
    let (host, deployment) = lab_host_and_deployment();
    let before = resolve_effective(&deployment, &host).unwrap();
    let host = with_profile_security(&host, json!({"approved_env": ["MBX_*"]}));
    let deployment = with_engine_config(&deployment, json!({"env": {"MBX_FUSED_DRAFT": "1"}}));
    let after = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(after.engine_env()["MBX_FUSED_DRAFT"], ("1".into(), EnvSource::Deployment));
    assert_ne!(before.recipe_fingerprint(), after.recipe_fingerprint());
    let plain_again = resolve_effective(&lab_host_and_deployment().1, &lab_host_and_deployment().0).unwrap();
    assert_eq!(before.recipe_fingerprint(), plain_again.recipe_fingerprint());
}
```

Add to `crates/capyctl-adapters/src/engine_env.rs` tests:

```rust
// T37: the launch environment carries the resolved engine env; CapyCTL's own values win.
#[test]
fn launch_environment_includes_resolved_engine_env() {
    let resolved = BTreeMap::from([("MBX_FUSED_DRAFT".to_owned(), "1".to_owned())]);
    let env = launch_environment(&resolved, Some("/opt/venv/bin/vllm"), None, 8 << 30, 4);
    assert_eq!(env["MBX_FUSED_DRAFT"], "1");
    assert!(env["PATH"].starts_with("/opt/venv/bin"));
}
```

`launch_environment(resolved, engine_bin, cuda_home, available_bytes, cpus)` is extracted from the existing `toolchain_environment` call sites so the engine env and CapyCTL's values are assembled in one place.

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p capyctl-config --test engine_env && cargo test -p capyctl-adapters engine_env` — expected: unresolved module and function.

- [ ] **Step 3: Implement.** `engine_env.rs` holds spec §2.1 rules 1–4 and 7. `validate_profile_env` becomes a thin wrapper over the profile half of `resolve_engine_env`. Resolution reads the profile's `security.approved_env` and the deployment's `engine_config.env`, stores `ResolvedEnv` in the effective configuration (serialized only when non-empty, so existing fingerprints are byte-identical), and hands `values()` to the adapters, whose final env allowlists (`vllm/initialize.rs` retain, `tensorfold/args.rs` retain) admit exactly the resolved names on top of their fixed lists. Comments cite `// ADR 0028 §2.1`.

- [ ] **Step 4: Run the tests.** Run: `cargo test -p capyctl-config && cargo test -p capyctl-adapters` — expected: PASS; existing fingerprint tests unchanged.

- [ ] **Step 5: Commit**

```bash
git add crates/capyctl-config crates/capyctl-adapters
git commit -m "feat(config): engine environment at profile and deployment level with host approvals"
```

---

### Task 3: Engine environment by CLI flag

**Files:**
- Modify: `crates/capyctl-cli/src/grammar.rs` (`engine add` gains repeatable `--env K=V` and `--approve-env GLOB`; `deploy` gains repeatable `--engine-env K=V`), `crates/capyctl-config/src/registration.rs` (`ProfileSpec` gains `env`, `approved_env`; `profile_document` writes them), `crates/capyctl-cli/src/engine.rs`, `crates/capyctl-cli/src/client.rs`
- Test: `crates/capyctl-cli/tests/engine_env_flags.rs`; unit tests in `registration.rs`

**Interfaces:**
- Consumes: `resolve_engine_env`, `ApprovedEnv::parse`, `is_owned`, `EnvRefusal` (Task 2).
- Produces:
  - `pub fn parse_env_flag(raw: &str) -> Result<(String, String), String>` in `grammar.rs` (split on the first `=`; empty name or no `=` is a usage error).
  - `pub fn merge_engine_env(document: &mut Value, flags: &[(String, String)]) -> Result<(), EnvRefusal>` in `client.rs`.
  - `ProfileSpec { .., env: BTreeMap<String, String>, approved_env: Vec<String> }`.

- [ ] **Step 1: Write the failing tests**

```rust
// crates/capyctl-cli/tests/engine_env_flags.rs
use capyctl_cli::client::merge_engine_env;
use serde_json::json;

// T14: --engine-env is saved with the deployment under engine_config.env.
#[test]
fn engine_env_flags_merge_into_the_document() {
    let mut doc = json!({"kind": "deployment", "engine_config": {"context_length": 8192}});
    merge_engine_env(&mut doc, &[("SGLANG_ENABLE_X".into(), "1".into())]).unwrap();
    assert_eq!(doc["engine_config"]["env"]["SGLANG_ENABLE_X"], "1");
    assert_eq!(doc["engine_config"]["context_length"], 8192);
}

// T03: a name in both the file and a flag, or twice by flag, is a conflict.
#[test]
fn file_and_flag_conflict() {
    let mut doc = json!({"engine_config": {"env": {"MBX_FUSED_DRAFT": "0"}}});
    let err = merge_engine_env(&mut doc, &[("MBX_FUSED_DRAFT".into(), "1".into())]).unwrap_err();
    assert_eq!(err.code(), "engine_env_conflict:MBX_FUSED_DRAFT");
    let mut doc = json!({});
    let twice = [("A_B".into(), "1".into()), ("A_B".into(), "2".into())];
    assert_eq!(merge_engine_env(&mut doc, &twice).unwrap_err().code(), "engine_env_conflict:A_B");
}

// T21: an owned name by flag is refused before anything is sent.
#[test]
fn owned_name_by_flag_is_refused_locally() {
    let mut doc = json!({});
    let err = merge_engine_env(&mut doc, &[("SGLANG_HOST_IP".into(), "x".into())]).unwrap_err();
    assert_eq!(err.code(), "engine_env_reserved:SGLANG_HOST_IP");
}

// T01: the flags parse as K=V and repeat.
#[test]
fn flags_parse() {
    assert_eq!(capyctl_cli::grammar::parse_env_flag("MBX_B=x=y").unwrap(), ("MBX_B".into(), "x=y".into()));
    assert!(capyctl_cli::grammar::parse_env_flag("NOEQUALS").is_err());
    assert!(capyctl_cli::grammar::parse_env_flag("=v").is_err());
}
```

In `registration.rs` tests (`sample_spec()` is the existing fixture):

```rust
// T14: engine add --env/--approve-env are saved in the profile and checked like YAML.
#[test]
fn profile_document_saves_env_and_approvals() {
    let mut spec = sample_spec();
    spec.env = BTreeMap::from([("TORCH_NCCL_ASYNC_ERROR_HANDLING".into(), "1".into())]);
    spec.approved_env = vec!["MBX_*".into()];
    let doc = profile_document(&spec);
    assert_eq!(doc["env"]["TORCH_NCCL_ASYNC_ERROR_HANDLING"], "1");
    assert_eq!(doc["security"]["approved_env"], json!(["MBX_*"]));
    assert!(check_profile("p", &doc).is_ok());
    spec.env.insert("NCCL_DEBUG".into(), "INFO".into());
    assert!(check_profile("p", &profile_document(&spec)).unwrap_err().to_string().contains("engine_env_reserved:NCCL_DEBUG"));
}

// T39: a profile added without the flags is written exactly as before.
#[test]
fn profile_without_flags_is_unchanged() {
    let doc = profile_document(&sample_spec());
    assert_eq!(doc, profile_document_before_env(&sample_spec()));
}
```

`profile_document_before_env` is a test helper holding a frozen copy of today's output for `sample_spec()` (paste the current JSON literal in the test).

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p capyctl-cli --test engine_env_flags && cargo test -p capyctl-config registration` — expected: unresolved.

- [ ] **Step 3: Implement.** Grammar: `#[arg(long = "engine-env", value_name = "K=V", value_parser = parse_env_flag)] engine_env: Vec<(String, String)>` on `deploy` (merged before submission, so an env change with `--revision` is a new revision); `--env` and `--approve-env` on `engine add`. `engine add` refuses a name the role document's profile already sets (`engine_env_conflict:<name>`). Help text: "shown in status; not secret storage". Comments cite `// ADR 0028 §2.1`.

- [ ] **Step 4: Run the tests.** Run: `cargo test -p capyctl-cli && cargo test -p capyctl-config` — expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/capyctl-cli crates/capyctl-config
git commit -m "feat(cli): engine environment by flag on engine add and deploy"
```

---

### Task 4: Deployment `topology`, group placement and engine shape support

**Files:**
- Create: `crates/capyctl-config/src/topology.rs`, `crates/capyctl-config/src/group_support.rs`
- Modify: `crates/capyctl-config/src/lib.rs`, `crates/capyctl-config/src/instances.rs` (`InstanceSpec.group`), `crates/capyctl-config/src/schema.rs` (deployment accepts `topology`), the deployment resolution that knows the profile's engine (search `fn resolve` in `crates/capyctl-config/src/effective/core.rs`) for the shape and residency check, the standalone resolution (refuse a topology)
- Test: `crates/capyctl-config/tests/topology.rs`

**Interfaces:**
- Produces:
  - `pub struct Topology { pub tensor_parallel: u32, pub pipeline_parallel: u32 }`, `fn world_size(&self) -> u32`.
  - `pub struct GroupShape { pub hosts: Vec<String>, pub topology: Topology, pub local_ranks: u32 }`, `fn head(&self) -> &str`.
  - `pub enum GroupRefusal { PlacementRequired, TopologyInvalid, ShapeUnsupported, InstancesUnsupported }`, `fn code(&self) -> &'static str`.
  - `pub fn parse_group_shape(deployment: &Value, spec: &InstanceSpec) -> Result<Option<GroupShape>, ConfigError>`.
  - `InstanceSpec.group: Option<GroupShape>` (`#[serde(default, skip_serializing_if = "Option::is_none")]`).
  - In `group_support.rs`: `pub struct GroupSupport { pub max_tensor_parallel: Option<u32>, pub pipeline: bool, pub exact_hosts: Option<u32>, pub deep_park: bool, pub worker_listens: bool }`; `pub fn group_support(engine: &str) -> Option<GroupSupport>`; `pub fn check_engine_shape(engine: &str, shape: &GroupShape, residency: &str) -> Result<(), ConfigError>` (codes `group_shape_unsupported:<engine>`, `capability_missing:deep_park`).

- [ ] **Step 1: Write the failing tests**

```rust
// crates/capyctl-config/tests/topology.rs
use capyctl_config::group_support::{check_engine_shape, group_support};
use capyctl_config::instances::parse_instance_spec;
use serde_json::json;

fn doc(extra: serde_json::Value) -> serde_json::Value {
    let mut d = json!({"schema_version": 1, "kind": "deployment", "name": "g"});
    d.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    d
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
        (json!({"topology": {"tensor_parallel": 2},
                "placement": {"hosts": ["host-a", " host-a"]}}), "placement.hosts"),
    ];
    for (extra, expected) in cases {
        let err = parse_instance_spec(&doc(extra.clone())).unwrap_err();
        assert!(err.to_string().contains(expected), "{extra} -> {expected}");
    }
}

// T39: world size 1 is a single-host deployment with an unchanged identity.
#[test]
fn world_size_one_is_single_host_and_identity_is_unchanged() {
    let plain = parse_instance_spec(&doc(json!({}))).unwrap();
    let tp1 = parse_instance_spec(&doc(json!({
        "topology": {"tensor_parallel": 1, "pipeline_parallel": 1}
    }))).unwrap();
    assert!(tp1.group.is_none());
    assert_eq!(plain.command_identity(), tp1.command_identity());
}

// T03, T22: engine shape support: TensorFold is TP 2 on 2 hosts and restart-only;
// vLLM and SGLang take PP and deep park.
#[test]
fn engine_shape_support() {
    let shape = |tp, pp, hosts: &[&str]| parse_instance_spec(&doc(json!({
        "topology": {"tensor_parallel": tp, "pipeline_parallel": pp},
        "placement": {"hosts": hosts}
    }))).unwrap().group.unwrap();
    let two = shape(2, 1, &["a", "b"]);
    let pp4 = shape(2, 2, &["a", "b", "c", "d"]);
    for engine in ["vllm", "sglang"] {
        assert!(check_engine_shape(engine, &two, "deep").is_ok());
        assert!(check_engine_shape(engine, &pp4, "deep").is_ok());
    }
    assert!(check_engine_shape("tensorfold", &two, "restart_only").is_ok());
    let err = check_engine_shape("tensorfold", &pp4, "restart_only").unwrap_err();
    assert!(err.to_string().contains("group_shape_unsupported:tensorfold"));
    let err = check_engine_shape("tensorfold", &two, "deep").unwrap_err();
    assert!(err.to_string().contains("capability_missing:deep_park"));
    assert!(group_support("sglang").unwrap().worker_listens);
    assert!(!group_support("vllm").unwrap().worker_listens);
}
```

Add one test in the standalone configuration tests (search `fn standalone` in `crates/capyctl-cli/src/standalone_config/tests.rs`): a standalone deployment with `topology.tensor_parallel: 2` is refused `group_placement_required`.

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p capyctl-config --test topology` — expected: compile errors.

- [ ] **Step 3: Implement `topology.rs`**

```rust
//! ADR 0028 §2: a deployment's multi-node topology. A world size above one
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
    pub(crate) fn at(self, path: &str, detail: &str) -> ConfigError {
        ConfigError::new(
            ConfigErrorCode::UnsupportedCombination,
            path,
            format!("{}: {detail}", self.code()),
        )
    }
}

/// Far above any hardware in hand, low enough that `world_size` never overflows.
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
    let topology = Topology {
        tensor_parallel: dimension(raw, "tensor_parallel")?,
        pipeline_parallel: dimension(raw, "pipeline_parallel")?,
    };
    if topology.world_size() == 1 {
        return Ok(None);
    }
    // ADR 0028 §2 (owner decision 8): named hosts only, head first.
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
    let n = hosts.len() as u32;
    if topology.world_size() % n != 0 {
        return Err(GroupRefusal::TopologyInvalid
            .at("topology", "the host count must divide tensor_parallel x pipeline_parallel"));
    }
    let local_ranks = topology.world_size() / n;
    if local_ranks != 1 {
        return Err(GroupRefusal::ShapeUnsupported
            .at("topology", "one device per host in this version"));
    }
    Ok(Some(GroupShape { hosts, topology, local_ranks }))
}
```

`group_support.rs`:

```rust
//! ADR 0028 §2, §10: what each engine's multi-node mode can run.
use crate::{topology::GroupShape, ConfigError, ConfigErrorCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupSupport {
    pub max_tensor_parallel: Option<u32>,
    pub pipeline: bool,
    pub exact_hosts: Option<u32>,
    pub deep_park: bool,
    pub worker_listens: bool,
}

pub fn group_support(engine: &str) -> Option<GroupSupport> {
    match engine {
        "vllm" => Some(GroupSupport { max_tensor_parallel: None, pipeline: true, exact_hosts: None, deep_park: true, worker_listens: false }),
        "sglang" => Some(GroupSupport { max_tensor_parallel: None, pipeline: true, exact_hosts: None, deep_park: true, worker_listens: true }),
        // TensorFold 0.6.5: `--tp {1,2}`, one GPU per machine, no sleep.
        "tensorfold" => Some(GroupSupport { max_tensor_parallel: Some(2), pipeline: false, exact_hosts: Some(2), deep_park: false, worker_listens: false }),
        _ => None,
    }
}

pub fn check_engine_shape(engine: &str, shape: &GroupShape, residency: &str) -> Result<(), ConfigError> {
    let unsupported = |detail: &str| ConfigError::new(
        ConfigErrorCode::UnsupportedCombination, "topology",
        format!("group_shape_unsupported:{engine}: {detail}"));
    let Some(s) = group_support(engine) else { return Err(unsupported("no multi-node mode")) };
    if s.max_tensor_parallel.is_some_and(|m| shape.topology.tensor_parallel > m) {
        return Err(unsupported("tensor_parallel above the engine's limit"));
    }
    if !s.pipeline && shape.topology.pipeline_parallel > 1 {
        return Err(unsupported("no pipeline parallel"));
    }
    if s.exact_hosts.is_some_and(|n| shape.hosts.len() as u32 != n) {
        return Err(unsupported("host count"));
    }
    if residency == "deep" && !s.deep_park {
        return Err(ConfigError::new(ConfigErrorCode::UnsupportedCombination, "residency",
            "capability_missing:deep_park"));
    }
    Ok(())
}
```

In `instances.rs`: trim host names before the duplicate check and refuse an entry whose trimmed form differs (`placement.hosts: host names have no surrounding space`); set `spec.group = crate::topology::parse_group_shape(deployment, &spec)?;` before the `placeable_on` check, skipping `placeable_on` for a group. Schema: `("topology", FieldSpec::Struct(&[("tensor_parallel", SCALAR), ("pipeline_parallel", SCALAR)]))`. The effective resolution calls `check_engine_shape` with the profile's engine and the resolved residency (a group whose residency defaults to `deep` on TensorFold resolves to `restart_only`, as a TensorFold single-host deployment does today). Standalone refuses any group shape with `group_placement_required` (one host).

- [ ] **Step 4: Run the tests.** Run: `cargo test -p capyctl-config && cargo test -p capyctl-cli standalone` — expected: PASS, every existing instances test unchanged.

- [ ] **Step 5: Commit**

```bash
git add crates/capyctl-config crates/capyctl-cli
git commit -m "feat(config): deployment topology, named-host group placement and engine shape support"
```

---

### Task 5: Host `groups` policy, three ways

**Files:**
- Create: `crates/capyctl-config/src/groups_policy.rs`
- Modify: `crates/capyctl-config/src/schema.rs` (`F2_RESOURCE_POLICY` gains `groups`), `crates/capyctl-config/src/lib.rs`, `crates/capyctl-cli/src/grammar.rs` (`start host` gains `--peer-address`, `--rendezvous-ports`, `--require-rdma`), the host role settings resolution that applies flag > env > YAML (search `CAPYCTL_ENGINE_PORTS` in `crates/capyctl-cli/src/` for the pattern to follow)
- Test: `crates/capyctl-config/tests/groups_policy.rs`; a settings precedence test beside the existing `--engine-ports` precedence test

**Interfaces:**
- Produces:
  - `pub struct GroupsPolicy { pub peer_address: Option<IpAddr>, pub rendezvous_ports: RangeInclusive<u16>, pub require_rdma: bool }` (`Default`: none, `25000..=25099`, false).
  - `pub const DEFAULT_RENDEZVOUS_PORTS: RangeInclusive<u16> = 25000..=25099;`
  - `pub fn host_groups_policy(host: &Value) -> Result<GroupsPolicy, ConfigError>`.

- [ ] **Step 1: Write the failing tests**

```rust
use capyctl_config::groups_policy::{host_groups_policy, DEFAULT_RENDEZVOUS_PORTS};
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

// T39: a host without the block keeps its policy digest.
#[test]
fn host_without_groups_keeps_its_digest() {
    let host = json!({"resource_policy": {"max_parked": 2}});
    let before = capyctl_config::effective::host_policy_digest(&host).unwrap();
    let _ = host_groups_policy(&host).unwrap();
    assert_eq!(before, capyctl_config::effective::host_policy_digest(&host).unwrap());
}
```

Use the function the store calls for the published policy digest in place of `host_policy_digest` if its name differs (search `digest` in `crates/capyctl-config/src/effective/`). Precedence test, beside the existing engine-ports one:

```rust
// T14: flag > env > YAML > default for every groups setting.
#[test]
fn groups_settings_precedence() {
    let yaml = json!({"resource_policy": {"groups": {"peer_address": "192.0.2.10"}}});
    let env = [("CAPYCTL_PEER_ADDRESS", "192.0.2.11"), ("CAPYCTL_RENDEZVOUS_PORTS", "26000-26009")];
    let flags = ["--peer-address", "192.0.2.12"];
    let host = resolve_host_settings(&yaml, &env, &flags).unwrap();
    assert_eq!(host["resource_policy"]["groups"]["peer_address"], "192.0.2.12");
    assert_eq!(host["resource_policy"]["groups"]["rendezvous_port_range"], json!({"start": 26000, "end": 26009}));
    let host = resolve_host_settings(&yaml, &[], &[]).unwrap();
    assert_eq!(host["resource_policy"]["groups"]["peer_address"], "192.0.2.10");
}
```

`resolve_host_settings` stands for the existing function that overlays host flags and env on the host document; use its real name and signature.

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p capyctl-config --test groups_policy` — expected: unresolved module.

- [ ] **Step 3: Implement.** Strict parse of `resource_policy.groups` (unknown field → `UnknownField`); `peer_address` via `IpAddr` parse, refused when loopback, unspecified or multicast; `1024 <= start <= end`; boolean `require_rdma`. Schema:

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

Flags `--peer-address <ip>`, `--rendezvous-ports <start-end>`, `--require-rdma`; env `CAPYCTL_PEER_ADDRESS`, `CAPYCTL_RENDEZVOUS_PORTS`, `CAPYCTL_REQUIRE_RDMA` (`true`/`false`). Comments cite `// ADR 0028 §3`.

- [ ] **Step 4: Run the tests.** Run: `cargo test -p capyctl-config && cargo test -p capyctl-cli` — expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/capyctl-config crates/capyctl-cli
git commit -m "feat(config): host groups policy: peer address, rendezvous ports, require_rdma, three ways"
```

---

### Task 6: N-member `GroupPlan`

**Files:**
- Modify: `crates/capyctl-domain/src/group.rs`, `crates/capyctl-protocol/src/execution.rs` (compile sites), `crates/capyctl-agent/tests/journal.rs` (fixture)
- Test: `crates/capyctl-domain/tests/group.rs`

**Interfaces:**
- Produces:
  - `pub enum MemberRole { Head, Worker }`; `pub enum GroupEngine { Vllm, Sglang, Tensorfold }` with `fn as_str(&self) -> &'static str`.
  - `MemberPlan` gains `pub role: MemberRole`, `pub model_path: String`, `pub worker_port: Option<u16>`; `service_port` becomes `Option<u16>`.
  - `pub struct GroupTopology { pub tensor_parallel: u32, pub pipeline_parallel: u32, pub local_ranks: u32 }`.
  - `GroupPlan::new(engine: GroupEngine, members: Vec<MemberPlan>, topology: GroupTopology, rendezvous_port: u16, generation: i64) -> Result<GroupPlan, GroupIdentityError>`; accessors `engine()`, `members()`, `head()`, `topology()`, `rendezvous_port()`, `generation()`.
  - `pub fn member_id(rank: u32) -> String` → `"head"` for 0, `"worker-<r>"` otherwise.
  - `GroupPlan::two_host` is removed.

- [ ] **Step 1: Write the failing tests** (replace the `two_host` tests)

```rust
use capyctl_domain::group::*;

fn plan_member(host: &str, rank: u32, engine: GroupEngine) -> MemberPlan {
    MemberPlan {
        member: MemberKey { host_id: host.into(), member_id: member_id(rank) },
        rank,
        role: if rank == 0 { MemberRole::Head } else { MemberRole::Worker },
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
    GroupTopology { tensor_parallel: tp, pipeline_parallel: pp, local_ranks: 1 }
}
fn members(n: u32, engine: GroupEngine) -> Vec<MemberPlan> {
    (0..n).map(|r| plan_member(&format!("h{r}"), r, engine)).collect()
}

// T27: N-member plans validate in rank order with one head.
#[test]
fn four_member_plan_is_valid() {
    let plan = GroupPlan::new(GroupEngine::Vllm, members(4, GroupEngine::Vllm), topo(2, 2), 25000, 7).unwrap();
    assert_eq!(plan.head().member.host_id, "h0");
    assert_eq!(plan.generation(), 7);
    assert_eq!(plan.members()[3].member.member_id, "worker-3");
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
```

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p capyctl-domain --test group` — expected: compile errors.

- [ ] **Step 3: Implement `GroupPlan::new`.** All failures are `GroupIdentityError`: `members.len() >= 2`; `rendezvous_port != 0`; `generation > 0`; `members[i].rank == i`; `role == Head` iff rank 0; `member_id == member_id(rank)`; distinct host ids; non-empty profile name, fingerprints and model path; `devices.len() == local_ranks` with non-empty labels; head `service_port == Some(p != 0)` and `worker_port == None`; workers `service_port == None`; workers `worker_port` is `Some(p != 0)` for `Sglang` and `None` otherwise; distinct peer addresses, none unspecified, multicast or loopback; equal profile and checkpoint fingerprints; `tensor_parallel * pipeline_parallel == members.len() * local_ranks`; for `Tensorfold`, exactly two members, TP 2, PP 1. Doc comment cites `// ADR 0028 §4`. Move the protocol and journal test fixtures to `new`.

- [ ] **Step 4: Run the tests.** Run: `cargo test -p capyctl-domain && cargo build --workspace --all-targets` — expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/capyctl-domain crates/capyctl-protocol crates/capyctl-agent
git commit -m "feat(domain): N-member group plan for three engines"
```

---

### Task 7: Wire fields and the `engine_groups` capability

**Files:**
- Modify: `crates/capyctl-protocol/proto/capyctl/management/v1/management.proto` (`GroupMemberPlan`, `GroupLaunchPlan`, the host inventory message), `crates/capyctl-protocol/src/execution.rs`, `crates/capyctl-protocol/src/capabilities.rs`
- Test: `crates/capyctl-protocol/tests/execution.rs`, `capabilities.rs` unit tests

**Interfaces:**
- Consumes: `GroupPlan::new`, `MemberRole`, `GroupEngine` (Task 6).
- Produces: `pub const ENGINE_GROUPS: &str = "engine_groups";` in `CATALOGUE` (server to host) and in `agent_capabilities()`; `required()` returns `[ENGINE_GROUPS]` for `Prepare` and `Launch`; `pub fn group_refusal(host_capabilities: &BTreeSet<String>) -> Option<String>`; `GroupInventory { peer_address, findings }` on the inventory.

- [ ] **Step 1: Write the failing tests**

```rust
// T34: group actions round-trip every member field, and the digest binds them.
#[test]
fn group_launch_round_trips_new_fields() {
    let plan = sample_group_plan(GroupEngine::Sglang); // head + worker with worker_port
    let action = MemberAction::Launch(plan.clone());
    let wire = action.to_wire();
    assert_eq!(MemberAction::from_wire(&wire).unwrap(), action);
    let other = sample_group_plan_with_port(GroupEngine::Sglang, 25001);
    assert_ne!(payload_digest(&MemberAction::Launch(other)), payload_digest(&action));
}

// T34: a malformed wire plan never becomes a domain plan.
#[test]
fn malformed_wire_plan_is_refused() {
    let mut wire = MemberAction::Launch(sample_group_plan(GroupEngine::Vllm)).to_wire();
    corrupt_second_member_rank(&mut wire, 0);
    assert!(MemberAction::from_wire(&wire).is_err());
}

// T34: a host without engine_groups is refused typed; one with it is not.
#[test]
fn engine_groups_gates_group_placement() {
    let none = BTreeSet::new();
    assert_eq!(group_refusal(&none).as_deref(), Some("host_capability_missing:engine_groups"));
    let with = BTreeSet::from([ENGINE_GROUPS.to_owned()]);
    assert_eq!(group_refusal(&with), None);
    assert!(agent_capabilities().contains(&ENGINE_GROUPS.to_owned()));
    assert_eq!(required(&command_with(MemberAction::Launch(sample_group_plan(GroupEngine::Vllm)))), [ENGINE_GROUPS]);
}

// T34: drain-only hosts may terminate a member but never prepare or launch one.
#[test]
fn drain_only_refuses_group_prepare_and_launch() {
    assert!(!drain_only_permits(&command_with(MemberAction::Prepare(sample_group_plan(GroupEngine::Vllm)))));
    assert!(!drain_only_permits(&command_with(MemberAction::Launch(sample_group_plan(GroupEngine::Vllm)))));
}
```

Use the file's existing helpers for `to_wire`/`from_wire`/`payload_digest`/`command_with` (search `fn round_trip` in the test file); add `sample_group_plan`, `sample_group_plan_with_port` and `corrupt_second_member_rank` there.

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p capyctl-protocol` — expected: missing fields and constant.

- [ ] **Step 3: Implement.** New fields on the next free numbers only, each message commented `// ADR 0028 §14`:

```proto
message GroupMemberPlan {
  // existing fields unchanged; service_port 0 now means "none" (worker)
  string role = 10;            // "head" | "worker"
  string model_path = 11;
  uint32 worker_port = 12;     // 0 = none; SGLang workers only
}
message GroupLaunchPlan {
  // existing fields unchanged
  string engine = 3;           // "vllm" | "sglang" | "tensorfold"
  uint32 tensor_parallel = 4;
  uint32 pipeline_parallel = 5;
  uint32 local_ranks = 6;
  int64 generation = 7;
}
message GroupInventory {
  string peer_address = 1;
  repeated string findings = 2;
}
```

Conversion validates through `GroupPlan::new`. Add `ENGINE_GROUPS` to `CATALOGUE`, `agent_capabilities` and the `required` match.

- [ ] **Step 4: Run the tests.** Run: `cargo test -p capyctl-protocol` — expected: PASS; the existing `PROTOCOL_VERSION == "2"` assertion still passes.

- [ ] **Step 5: Commit**

```bash
git add crates/capyctl-protocol
git commit -m "feat(protocol): group plan member fields and the engine_groups capability"
```

---

### Task 8: Store: group plans, member owners, all-or-nothing reservation, ports, per-host digests

**Files:**
- Create: `crates/capyctl-store/src/groups.rs`, `crates/capyctl-store/tests/groups.rs`
- Modify: `crates/capyctl-store/src/instances.rs` (member owner ids), the schema/migration module (new tables, digest table per host), `crates/capyctl-store/src/checkpoint_digests.rs`, `crates/capyctl-store/src/lib.rs`

**Interfaces:**
- Consumes: `GrantRequest`, `reserve_increase_in_transaction` (existing, `resource_ledger.rs`), `GroupPlan` (Task 6).
- Produces:
  - `pub fn member_owner_id(deployment_id: &str, instance_index: u32, rank: u32) -> String` → `deployment:<id>/instance:<k>/member:<r>`; `pub fn parse_member_owner_id(&str) -> Option<(String, u32, u32)>`.
  - Tables `group_plans(deployment_id, instance_index, generation, plan_json, rendezvous_host, rendezvous_port, state)` and `group_members(deployment_id, instance_index, generation, rank, host_id, owner_id, state CHECK(state IN ('reserved','dispatching','launched','settled','uncertain')), dispatched, identities_json)`.
  - `pub struct GroupReservation { pub deployment_id: String, pub instance_index: u32, pub members: Vec<(String, GrantRequest)>, pub head_host: String, pub port_range: RangeInclusive<u16>, pub worker_ports: BTreeMap<String, RangeInclusive<u16>> }` (`worker_ports` empty unless SGLang).
  - `impl ResourceStore { pub fn reserve_group(&self, r: &GroupReservation, plan_for: impl FnOnce(u16, &BTreeMap<String, u16>) -> Result<GroupPlan, GroupIdentityError>, contexts: &BTreeMap<String, AdmissionContext<'_>>) -> Result<GroupPlan, GroupStoreError>; pub fn mark_member_dispatching(&self, deployment_id: &str, instance_index: u32, generation: i64, rank: u32) -> Result<(), GroupStoreError>; pub fn mark_member_launched(&self, deployment_id: &str, instance_index: u32, generation: i64, rank: u32, identities: &[ProcessIdentity]) -> Result<(), GroupStoreError>; pub fn settle_member(&self, deployment_id: &str, instance_index: u32, generation: i64, rank: u32, evidence: MemberGone) -> Result<GroupSettlement, GroupStoreError>; pub fn mark_member_uncertain(&self, deployment_id: &str, instance_index: u32, generation: i64, rank: u32) -> Result<(), GroupStoreError>; pub fn group_plan(&self, deployment_id: &str, instance_index: u32) -> Result<Option<(GroupPlan, Vec<MemberRow>)>, GroupStoreError>; }`. `contexts` holds exactly one admission context per member host, built from that host's own publication, so limits and `max_parked` are judged per host (ADR 0028 §12); every footprint key must belong to its member's host or the reservation is refused `Plan`.
  - Dispatch fence (ruling R23): a member settles on empty `MemberGone` only if it was never dispatched. Once `mark_member_dispatching` commits, it settles only on gone evidence for the identities `mark_member_launched` recorded; revocation alone releases nothing.
  - `pub enum GroupSettlement { Partial { unsettled: Vec<u32> }, Complete }`; `Complete` frees the rendezvous port and worker endpoint leases.
  - `pub enum GroupStoreError { PortsExhausted, Admission(ResourceStoreError), Plan, Conflict }`.
  - Checkpoint digests: `record_digest(deployment, revision, host_id, digest)` and `digests_for(deployment, revision) -> BTreeMap<String, String>`; single-host callers read their host's row, unchanged in behaviour.
  - `MemberGone { member: MemberKey, identities: Vec<ProcessIdentity> }` (new; the single-instance path settles on binding-bound `CleanupEvidence`, which a member does not have). The key must match the row's host and rank.

- [ ] **Step 1: Write the failing tests**

```rust
// crates/capyctl-store/tests/groups.rs
// T27: all members reserve in one transaction or none do.
#[test]
fn group_reservation_is_all_or_nothing() {
    let store = two_host_store(gib(100), gib(10));
    let r = reservation("g", &[("host-a", gib(80)), ("host-b", gib(80))]);
    assert!(matches!(store.reserve_group(&r, plan_for("g"), ctx()), Err(GroupStoreError::Admission(_))));
    assert_eq!(store.owner_bytes(&member_owner_id("g", 0, 0)), 0);
    assert_eq!(store.owner_bytes(&member_owner_id("g", 0, 1)), 0);
    assert!(store.group_plan("g", 0).unwrap().is_none());
}

// T27, Review Focus 5: two groups sharing host B never hold host B's share while failing elsewhere.
#[test]
fn concurrent_groups_do_not_deadlock_or_leak() {
    let store = shared_store_three_hosts(gib(100), gib(100), gib(100));
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

// T27: the rendezvous port comes from the head's range and is held until every member settles.
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
    store.settle_member("g1", 0, 1, 1, gone()).unwrap();
    assert!(matches!(store.reserve_group(&r3, plan_for("g3"), ctx()), Err(GroupStoreError::PortsExhausted)));
    assert!(matches!(store.settle_member("g1", 0, 1, 0, gone()).unwrap(), GroupSettlement::Complete));
    assert_eq!(store.reserve_group(&r3, plan_for("g3"), ctx()).unwrap().rendezvous_port(), p1);
}

// T22: a SGLang worker's loopback port is leased on its own host and released on Complete.
#[test]
fn sglang_worker_ports_are_leased_per_host() {
    let store = two_host_store(gib(400), gib(400));
    let mut r = reservation("g", &[("host-a", gib(10)), ("host-b", gib(10))]);
    r.worker_ports = BTreeMap::from([("host-b".into(), 8100..=8100)]);
    let plan = store.reserve_group(&r, sglang_plan_for("g"), ctx()).unwrap();
    assert_eq!(plan.members()[1].worker_port, Some(8100));
    assert!(store.endpoint_leased("host-b", 8100));
    store.settle_member("g", 0, 1, 0, gone()).unwrap();
    store.settle_member("g", 0, 1, 1, gone()).unwrap();
    assert!(!store.endpoint_leased("host-b", 8100));
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

// T27: two members naming one host id are refused before any write.
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

// T14: digests are recorded per host; a single-host deployment reads its own row as before.
#[test]
fn digests_are_per_host() {
    let store = two_host_store(gib(10), gib(10));
    store.record_digest("g", 1, "host-a", "sha256:c").unwrap();
    store.record_digest("g", 1, "host-b", "sha256:d").unwrap();
    let all = store.digests_for("g", 1).unwrap();
    assert_eq!(all["host-a"], "sha256:c");
    assert_eq!(all["host-b"], "sha256:d");
}
```

Write the fixtures (`two_host_store`, `shared_store_three_hosts`, `reservation`, `plan_for`, `sglang_plan_for`, `ctx`, `gone`, `gib`, `owner_bytes`, `endpoint_leased`) at the top of the file on the store's existing published-policy fixtures (search `fn published_policy` in `crates/capyctl-store/tests/`).

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p capyctl-store --test groups` — expected: unresolved items.

- [ ] **Step 3: Implement `reserve_group`.** One `TransactionBehavior::Immediate` transaction: refuse duplicate host ids (`Plan`); pick the lowest rendezvous port in `port_range` not held by a `group_plans` row with `state != 'settled'` on `head_host` (`PortsExhausted`); lease each SGLang worker port through the existing `endpoint_leases` table on that worker's host (`PortsExhausted` when its range is full); build the plan with `plan_for(port, &worker_ports)` (`Plan` on error); `reserve_increase_in_transaction` for every member (owner from `member_owner_id`), returning on the first error so the transaction rolls back; insert the rows; commit. Comment: `// ADR 0028 §5, SPEC §11: all members or none; atomic accounting, not an atomic launch`. `settle_member` releases one member through the existing release path with `MemberGone` only; when nothing is unsettled or uncertain, mark the plan settled and release worker leases. `mark_member_uncertain` never releases. Migrate `checkpoint_digests` to one row per `(deployment_id, revision, host_id)`, copying existing rows.

- [ ] **Step 4: Run the tests.** Run: `cargo test -p capyctl-store` — expected: PASS, including migration tests (add the new tables to the migration fixture).

- [ ] **Step 5: Commit**

```bash
git add crates/capyctl-store
git commit -m "feat(store): group plans, per-member owners, all-or-nothing reservation, ports and per-host digests"
```

---

### Task 9: Host checks, peer address and `Prepare`

**Files:**
- Create: `crates/capyctl-agent/src/host_checks.rs`
- Modify: `crates/capyctl-agent/src/native_execution.rs` (`authorize` admits `Prepare`; handler), `crates/capyctl-agent/src/lib.rs`, the inventory builder (search `fn inventory`)
- Test: unit tests in `host_checks.rs`

**Interfaces:**
- Consumes: `GroupsPolicy` (Task 5), `GroupPlan` (Task 6).
- Produces:
  - `pub struct HostFacts { pub compaction_proactiveness: Option<u64>, pub memlock_soft: Option<u64>, pub infiniband: InfinibandAccess, pub local_addresses: Vec<IpAddr> }` (`memlock_soft: None` = unlimited); `pub enum InfinibandAccess { Absent, NoAccess, ReadWrite }`.
  - `pub fn read_host_facts(root: &Path) -> HostFacts`.
  - `pub struct CheckVerdict { pub warnings: Vec<String>, pub refusal: Option<String> }`; `pub fn evaluate(facts: &HostFacts, policy: &GroupsPolicy) -> CheckVerdict`.
  - `pub fn prepare_member(plan: &GroupPlan, host_id: &str, facts: &HostFacts, policy: &GroupsPolicy, port_free: impl Fn(IpAddr, u16) -> bool, digest_of: impl Fn(&str) -> Option<String>, profile_fingerprint: impl Fn(&str) -> Option<String>) -> Result<CheckVerdict, String>`; the `Err` string is a closed code.

- [ ] **Step 1: Write the failing tests**

```rust
// T29: findings warn by default and refuse only under require_rdma (compaction never refuses).
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
    let plan = sample_plan(GroupEngine::Vllm); // head host-a 192.0.2.10, port 25000
    let err = prepare_member(&plan, "host-a", &facts_for("192.0.2.10"), &policy_with("192.0.2.10"),
        |_, port| port != 25000, |_| Some("sha256:c".into()), |_| Some("pinned".into())).unwrap_err();
    assert_eq!(err, "rendezvous_port_in_use:25000");
}

// Review Focus 2: an occupied SGLang worker loopback port refuses Prepare on the worker.
#[test]
fn occupied_worker_port_refuses_prepare() {
    let plan = sample_plan(GroupEngine::Sglang); // worker host-b 192.0.2.11, worker_port 8101
    let err = prepare_member(&plan, "host-b", &facts_for("192.0.2.11"), &policy_with("192.0.2.11"),
        |_, port| port != 8101, |_| Some("sha256:c".into()), |_| Some("pinned".into())).unwrap_err();
    assert_eq!(err, "service_port_in_use:8101");
}

// T30: a peer address not on this host, a digest mismatch and a profile mismatch are refused.
#[test]
fn prepare_refuses_address_digest_and_profile_mismatch() {
    let plan = sample_plan(GroupEngine::Vllm);
    let ok = |_, _| true;
    assert_eq!(prepare_member(&plan, "host-b", &facts_for("192.0.2.99"), &policy_with("192.0.2.11"), ok,
        |_| Some("sha256:c".into()), |_| Some("pinned".into())).unwrap_err(), "peer_address_not_local");
    assert_eq!(prepare_member(&plan, "host-b", &facts_for("192.0.2.11"), &policy_with("192.0.2.11"), ok,
        |_| Some("sha256:x".into()), |_| Some("pinned".into())).unwrap_err(), "group_checkpoint_mismatch");
    assert_eq!(prepare_member(&plan, "host-b", &facts_for("192.0.2.11"), &policy_with("192.0.2.11"), ok,
        |_| Some("sha256:c".into()), |_| Some("other".into())).unwrap_err(), "group_profile_mismatch");
    assert!(prepare_member(&plan, "host-c", &facts_for("192.0.2.12"), &policy_with("192.0.2.12"), ok,
        |_| Some("sha256:c".into()), |_| Some("pinned".into())).is_err());
}
```

`sample_plan`, `facts_for` (good facts with that local address) and `policy_with` are helpers at the top of the test module.

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p capyctl-agent host_checks` — expected: unresolved module.

- [ ] **Step 3: Implement.** `evaluate`: compaction nonzero → warning only; memlock not unlimited → warning, or `host_tuning_missing:memlock` under `require_rdma`; infiniband not `ReadWrite` → warning, or `host_tuning_missing:infiniband`; first refusal in the order memlock, infiniband. `read_host_facts` reads `<root>/proc/sys/vm/compaction_proactiveness`, `getrlimit(RLIMIT_MEMLOCK)`, `<root>/dev/infiniband/uverbs*` with `access(R_OK|W_OK)`, local addresses via `getifaddrs` (`root` is `/` in production). `prepare_member`: this host's member (else `group_profile_mismatch`); its peer address equals the policy's and is local (`peer_address_not_local`); profile fingerprint (`group_profile_mismatch`); digest of `model_path` (`group_checkpoint_mismatch`); on the head `port_free(peer, rendezvous_port)` (`rendezvous_port_in_use:<p>`) and `port_free(127.0.0.1, service_port)` (`service_port_in_use:<p>`); on a worker with `worker_port`, `port_free(127.0.0.1, p)`; then `evaluate`. The native handler for `Prepare` uses a bind-and-drop `TcpListener` probe, the checkpoint digest cache and the profile table; it journals nothing and spawns nothing. The inventory reports `GroupInventory`. Comments cite `// ADR 0028 §7 (owner decision 10): read, never change`.

- [ ] **Step 4: Run the tests.** Run: `cargo test -p capyctl-agent` — expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/capyctl-agent
git commit -m "feat(agent): group Prepare with read-only host checks and peer address verification"
```

---

### Task 10: Shared member arguments and vLLM multi-node rendering

**Files:**
- Create: `crates/capyctl-adapters/src/group.rs`
- Modify: `crates/capyctl-adapters/src/lib.rs`, `crates/capyctl-adapters/src/vllm/args.rs`, `crates/capyctl-adapters/src/vllm/frozen.rs`, `runtime/vllm_entry.py`, `runtime/loopback_rendezvous.py`
- Test: `crates/capyctl-adapters/tests/vllm_group_args.rs`, `runtime/tests/test_vllm_entry.py`, `runtime/tests/test_loopback_rendezvous.py`

**Interfaces:**
- Consumes: `GroupPlan`, `MemberPlan` (Task 6).
- Produces:
  - In `group.rs`: `pub struct GroupMemberArgs { pub tensor_parallel: u32, pub pipeline_parallel: u32, pub nnodes: u32, pub node_rank: u32, pub head_address: IpAddr, pub rendezvous_port: u16, pub own_address: IpAddr, pub worker_port: Option<u16> }`; `pub fn member_args(plan: &GroupPlan, host_id: &str) -> Option<GroupMemberArgs>`; `fn is_head(&self) -> bool`; `pub fn expected_json(&self, fields: serde_json::Value) -> String` (the `CAPYCTL_GROUP_EXPECTED` payload).
  - `PlanInputVllm.group: Option<GroupMemberArgs>` (default `None`).
  - Rendered env for a group: `VLLM_HOST_IP=<own>`, `CAPYCTL_GROUP_MODE=1`, `CAPYCTL_GROUP_EXPECTED=<json>`.
  - Python: `loopback_rendezvous.pin_group(env, address_var)`, `verify_group(env, address_var, expected_address)`; `vllm_entry.check_group(expected, namespace)` raising `LaunchError("group_drift:<dest>")`.

- [ ] **Step 1: Write the failing tests**

```rust
// crates/capyctl-adapters/tests/vllm_group_args.rs
use capyctl_adapters::group::GroupMemberArgs;
use capyctl_adapters::vllm::args::{render_command, ArgsError, PlanInputVllm};

fn group_input(rank: u32) -> PlanInputVllm {
    PlanInputVllm {
        engine_bin: "/opt/venv/bin/vllm".into(),
        model_path: "/models/m".into(),
        port: 8100,
        served_model_name: "m".into(),
        tensor_parallel_size: 2,
        pipeline_parallel_size: 1,
        group: Some(GroupMemberArgs {
            tensor_parallel: 2, pipeline_parallel: 1, nnodes: 2, node_rank: rank,
            head_address: "192.0.2.10".parse().unwrap(), rendezvous_port: 25000,
            own_address: format!("192.0.2.{}", 10 + rank).parse().unwrap(), worker_port: None,
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
                   ("--host", "127.0.0.1"), ("--port", "8100")] {
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

// T20: deep park needs sleep mode on every rank.
#[test]
fn sleep_mode_on_every_rank_when_deep() {
    for rank in [0, 1] {
        let mut input = group_input(rank);
        input.sleep_mode = true;
        assert!(render_command(&input).unwrap().argv.iter().any(|a| a == "--enable-sleep-mode"));
    }
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
    assert_eq!(cmd, render_command(&single_rank_reference_input()).unwrap());
    assert!(!cmd.argv.iter().any(|a| a == "--nnodes" || a == "--headless"));
}
```

`sleep_mode` stands for the existing `PlanInputVllm` field that renders `--enable-sleep-mode` (search `enable-sleep-mode` in `vllm/args.rs`); `single_rank_reference_input()` builds today's single-rank input with the same values. Python:

```python
class GroupModeTests(unittest.TestCase):
    EXPECTED = {"nnodes": 2, "node_rank": 1, "master_addr": "192.0.2.10",
                "master_port": 25000, "headless": True,
                "distributed_executor_backend": "mp",
                "tensor_parallel_size": 2, "pipeline_parallel_size": 1}

    # T14, T21: every rendered multi-node destination must match the parse.
    def test_group_drift_is_refused(self):
        vllm_entry.check_group(self.EXPECTED, argparse.Namespace(**self.EXPECTED))
        for dest, bad in [("node_rank", 0), ("master_port", 25001), ("headless", False)]:
            drifted = argparse.Namespace(**{**self.EXPECTED, dest: bad})
            with self.assertRaises(vllm_entry.LaunchError) as ctx:
                vllm_entry.check_group(self.EXPECTED, drifted)
            self.assertEqual(ctx.exception.code, "group_drift:" + dest)

    # T21 (owner decision 4): group mode pins no interface and strips inherited transport.
    def test_pin_group_strips_transport_and_keeps_host_ip(self):
        env = {"NCCL_IB_HCA": "x", "GLOO_SOCKET_IFNAME": "lo", "MASTER_PORT": "1",
               "VLLM_HOST_IP": "192.0.2.11"}
        loopback_rendezvous.pin_group(env, "VLLM_HOST_IP")
        self.assertEqual(env, {"VLLM_HOST_IP": "192.0.2.11"})
        loopback_rendezvous.verify_group(env, "VLLM_HOST_IP", "192.0.2.11")
        env["NCCL_SOCKET_IFNAME"] = "eth0"
        with self.assertRaises(loopback_rendezvous.RendezvousError):
            loopback_rendezvous.verify_group(env, "VLLM_HOST_IP", "192.0.2.11")

    # T39: without CAPYCTL_GROUP_MODE the single-rank pin is unchanged.
    def test_single_rank_keeps_loopback_pin(self):
        env = {}
        loopback_rendezvous.pin(env)
        self.assertEqual(env["NCCL_SOCKET_IFNAME"], "lo")
```

Adapt `pin(env)` and the error class to the module's real signature (read it first); the assertions are what matter.

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p capyctl-adapters --test vllm_group_args && python3 -m unittest discover -s runtime/tests` — expected: `group` missing; AttributeError.

- [ ] **Step 3: Implement.** `group.rs`: `member_args` finds the host's member and fills the struct from the plan. In `render_command`, when `group` is `Some`: push `--tensor-parallel-size`, `--pipeline-parallel-size`, `--distributed-executor-backend mp`, `--nnodes`, `--node-rank`, `--master-addr <head>`, `--master-port <port>`; for a worker skip the listener, served-name, key and middleware block and push `--headless`; env `VLLM_HOST_IP`, `CAPYCTL_GROUP_MODE=1`, `CAPYCTL_GROUP_EXPECTED`. `frozen.rs` takes TP and PP from the plan instead of the `1` pin. `vllm_entry.main`: in group mode call `pin_group(env, "VLLM_HOST_IP")` instead of `pin`, move the eight destinations from the single-rank constant comparison into `check_group`, and call `verify_group` immediately before handing control to vLLM. Comments cite `// ADR 0028 §10`.

- [ ] **Step 4: Run the tests.** Run: `cargo test -p capyctl-adapters && python3 -m unittest discover -s runtime/tests` — expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/capyctl-adapters runtime
git commit -m "feat(adapters): shared group member args and vLLM multi-node head and headless worker"
```

---

### Task 11: SGLang multi-node rendering and group mode

**Files:**
- Modify: `crates/capyctl-adapters/src/sglang/args.rs` and `frozen.rs` (a `group` object in the public settings), `runtime/sglang_server_args.py` (`_RESERVED_CONSTANT` values for `tp_size`, `pp_size`, `nnodes`, `node_rank`, `dist_init_addr` come from the group in group mode), `runtime/sglang_entry.py` (no file rendezvous in group mode; `pin_group(env, "SGLANG_HOST_IP")`)
- Test: `crates/capyctl-adapters/tests/sglang_group_args.rs`, `runtime/tests/test_sglang_server_args.py`, `runtime/tests/test_sglang_entry.py`

**Interfaces:**
- Consumes: `GroupMemberArgs` (Task 10), `pin_group`/`verify_group` (Task 10).
- Produces:
  - The frozen SGLang launch carries `group: Option<GroupMemberArgs>`; `SglangLaunch::from_frozen` writes `"group": {"tp_size", "pp_size", "nnodes", "node_rank", "dist_init_addr": "<head>:<port>", "host_ip": "<own>"}` into the public descriptor; for a worker, the descriptor's `endpoint` is `127.0.0.1:<worker_port>`.
  - Env `SGLANG_HOST_IP=<own>`, `CAPYCTL_GROUP_MODE=1`.
  - Python: `construct_server_args` in group mode sets the five reserved fields from `public["group"]` and refuses any other value (`group_drift:<field>`); `--enable-memory-saver` on every rank when the residency is deep.

- [ ] **Step 1: Write the failing tests**

```rust
// T22: the public descriptor carries the group; a worker's endpoint is its loopback port.
#[test]
fn sglang_group_descriptor() {
    let head = SglangLaunch::from_frozen(&frozen_sglang_group(0)).unwrap();
    let g = &head.public_metadata()["settings"]["group"];
    assert_eq!(g["tp_size"], 2);
    assert_eq!(g["nnodes"], 2);
    assert_eq!(g["node_rank"], 0);
    assert_eq!(g["dist_init_addr"], "192.0.2.10:25000");
    let worker = SglangLaunch::from_frozen(&frozen_sglang_group(1)).unwrap();
    assert_eq!(worker.public_metadata()["endpoint"], "127.0.0.1:8101");
    assert_eq!(worker.public_metadata()["settings"]["group"]["host_ip"], "192.0.2.11");
}

// T21, T37: no NCCL/GLOO variable; SGLANG_HOST_IP is the member's own address.
#[test]
fn sglang_group_env() {
    let env = sglang_launch_env(&frozen_sglang_group(1)).unwrap();
    assert_eq!(env["SGLANG_HOST_IP"], "192.0.2.11");
    assert!(!env.keys().any(|k| k.starts_with("NCCL_") || k.starts_with("GLOO_")));
    assert!(!env.contains_key("SGLANG_DISTRIBUTED_INIT_METHOD_OVERRIDE"));
}

// T39: a single-rank descriptor has no group and is byte-identical.
#[test]
fn sglang_single_rank_descriptor_unchanged() {
    let single = SglangLaunch::from_frozen(&frozen_sglang_single()).unwrap();
    assert!(single.public_metadata()["settings"].get("group").is_none());
    assert_eq!(single.public_metadata(), &single_reference_descriptor());
}
```

`frozen_sglang_group(rank)` and `frozen_sglang_single()` build `NativeLaunch` fixtures on the existing SGLang frozen-launch test fixture (search `fn frozen` in `crates/capyctl-adapters/src/sglang/`); `sglang_launch_env` is the function the agent uses to build the SGLang launch environment (search `SGLANG_DISTRIBUTED_INIT_METHOD_OVERRIDE` callers); `single_reference_descriptor()` is a frozen JSON literal of today's output. Python, in the style of the existing `test_sglang_server_args.py` cases:

```python
class GroupModeTests(unittest.TestCase):
    # T22: group mode takes the five reserved fields from the descriptor; drift is refused.
    def test_group_fields_come_from_descriptor(self):
        public = sample_public(group={"tp_size": 2, "pp_size": 1, "nnodes": 2, "node_rank": 1,
                                      "dist_init_addr": "192.0.2.10:25000", "host_ip": "192.0.2.11"})
        args = construct_for_test(public, extra_args=[])
        self.assertEqual((args.tp_size, args.nnodes, args.node_rank, args.dist_init_addr),
                         (2, 2, 1, "192.0.2.10:25000"))
        with self.assertRaises(sglang_server_args.LaunchRefused) as ctx:
            construct_for_test(public, resolved_override={"node_rank": 0})
        self.assertEqual(ctx.exception.code, "group_drift:node_rank")

    # T39: without a group the single-rank constants hold.
    def test_single_rank_constants_hold(self):
        args = construct_for_test(sample_public(group=None), extra_args=[])
        self.assertEqual((args.tp_size, args.nnodes, args.node_rank), (1, 1, 0))
```

```python
class EntryGroupModeTests(unittest.TestCase):
    # T21: group mode uses no file rendezvous and keeps only SGLANG_HOST_IP.
    def test_entry_group_mode_env(self):
        env = {"SGLANG_HOST_IP": "192.0.2.11", "GLOO_SOCKET_IFNAME": "lo"}
        sglang_entry.prepare_group_environment(env)
        self.assertEqual(env, {"SGLANG_HOST_IP": "192.0.2.11"})
```

`sample_public`, `construct_for_test` and the error class follow the existing test helpers; use their real names.

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p capyctl-adapters --test sglang_group_args && python3 -m unittest discover -s runtime/tests` — expected: fail.

- [ ] **Step 3: Implement.** Rust: carry the group through the frozen launch into the descriptor; worker endpoint from `worker_port`; env `SGLANG_HOST_IP` and `CAPYCTL_GROUP_MODE=1`; no file-store variable in group mode. Python: `construct_server_args` replaces the five constants with the group's values when `public["settings"]["group"]` is present and compares the resolved record against them; `sglang_entry` calls `prepare_group_environment` (strips transport names, keeps `SGLANG_HOST_IP`) instead of the loopback pin and `verify_group` before `launch_server`. Readiness stays head-only; the rank > 0 health server is never probed (`# ADR 0028 §9: its health always passes`).

- [ ] **Step 4: Run the tests.** Run: `cargo test -p capyctl-adapters && python3 -m unittest discover -s runtime/tests` — expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/capyctl-adapters runtime
git commit -m "feat(adapters): SGLang multi-node rendering and group mode in the protected entry"
```

---

### Task 12: TensorFold two-rank rendering

**Files:**
- Modify: `crates/capyctl-adapters/src/tensorfold/args.rs`
- Test: `crates/capyctl-adapters/tests/tensorfold_args.rs`

**Interfaces:**
- Consumes: `GroupMemberArgs` (Task 10).
- Produces: `PlanInputTensorfold.group: Option<GroupMemberArgs>`; head argv gains `--tp 2 --rank 0 --master <head> --master-port <port>` and keeps `--host 127.0.0.1 --port <p> --name <served>`; rank 1 argv is `serve <model> --tp 2 --rank 1 --master <head> --master-port <port>` plus the same typed args (`--context`, `--parallel`, `--kv-dtype`, drafter) and no host, port or name; `TensorfoldArgsError::GroupShape` for anything but TP 2, PP 1, two members.

- [ ] **Step 1: Write the failing tests**

```rust
fn tf_group(rank: u32) -> PlanInputTensorfold {
    PlanInputTensorfold {
        engine_bin: "/opt/tf/bin/tensorfold".into(),
        model_path: "/models/m".into(),
        served_model_name: "m".into(),
        port: 8100,
        context_length: 32768,
        parallel: Some(4),
        group: Some(GroupMemberArgs {
            tensor_parallel: 2, pipeline_parallel: 1, nnodes: 2, node_rank: rank,
            head_address: "192.0.2.10".parse().unwrap(), rendezvous_port: 25000,
            own_address: format!("192.0.2.{}", 10 + rank).parse().unwrap(), worker_port: None,
        }),
        ..PlanInputTensorfold::default()
    }
}

// T14: both ranks render the two-rank flags; only rank 0 serves.
#[test]
fn two_rank_flags() {
    let head = render_command(&tf_group(0)).unwrap();
    let follower = render_command(&tf_group(1)).unwrap();
    for (cmd, rank) in [(&head, "0"), (&follower, "1")] {
        for (f, v) in [("--tp", "2"), ("--rank", rank), ("--master", "192.0.2.10"), ("--master-port", "25000")] {
            assert!(has_pair(&cmd.argv, f, v), "{f} {v}");
        }
    }
    assert!(has_pair(&head.argv, "--host", "127.0.0.1"));
    for absent in ["--host", "--port", "--name"] {
        assert!(!follower.argv.iter().any(|a| a == absent), "{absent}");
    }
}

// T14: both ranks carry equal context and --parallel (TensorFold requires agreement).
#[test]
fn ranks_agree_on_context_and_parallel() {
    let typed = |c: &RenderedCommand| c.argv.iter().skip_while(|a| *a != "--context").take(4).cloned().collect::<Vec<_>>();
    assert_eq!(typed(&render_command(&tf_group(0)).unwrap()), typed(&render_command(&tf_group(1)).unwrap()));
}

// T03: any other shape is refused; the user still cannot pass --tp/--rank/--master.
#[test]
fn other_shapes_and_user_flags_are_refused() {
    let mut pp = tf_group(0);
    pp.group.as_mut().unwrap().pipeline_parallel = 2;
    assert!(matches!(render_command(&pp), Err(TensorfoldArgsError::GroupShape)));
    let mut user = tf_group(0);
    user.extra_args = vec!["--rank".into(), "1".into()];
    assert!(matches!(render_command(&user), Err(TensorfoldArgsError::Reserved(_))));
}

// T21: no transport or TF_COMM variable is rendered.
#[test]
fn no_transport_variables() {
    for rank in [0, 1] {
        let cmd = render_command(&tf_group(rank)).unwrap();
        assert!(!cmd.env.keys().any(|k| k.starts_with("NCCL_") || k.starts_with("GLOO_") || k.starts_with("TF_COMM")));
    }
}
```

`has_pair` as in Task 10; if the typed-args order differs, compare the `--context` and `--parallel` values directly.

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p capyctl-adapters --test tensorfold_args` — expected: `group` missing.

- [ ] **Step 3: Implement.** In `render_command`: refuse a group that is not TP 2, PP 1, `nnodes` 2 (`GroupShape`); push `--tp 2 --rank r --master <head> --master-port <port>` after the model path; rank 1 omits `--name`, `--host`, `--port`. Comments cite `// ADR 0028 §10: rank 0 serves HTTP, rank 1 follows it`.

- [ ] **Step 4: Run the tests.** Run: `cargo test -p capyctl-adapters` — expected: PASS; existing TensorFold tests unchanged.

- [ ] **Step 5: Commit**

```bash
git add crates/capyctl-adapters
git commit -m "feat(adapters): TensorFold two-rank rendering"
```

---

### Task 13: Agent member launch and member terminate

**Files:**
- Modify: `crates/capyctl-agent/src/native_execution.rs` (`authorize` admits `Launch(GroupPlan)`; `render_launch` dispatches per engine through `member_args`; the "exactly one device" rule becomes "exactly `local_ranks` devices"), `crates/capyctl-agent/src/journal.rs` (journal a member launch keyed by deployment, instance, generation, rank), `crates/capyctl-agent/src/rendezvous.rs` (no file rendezvous root for a group member)
- Test: `crates/capyctl-agent/tests/group_launch.rs`

**Interfaces:**
- Consumes: `GroupPlan`, `member_id` (Task 6); `member_args` (Task 10); the three renderers (Tasks 10–12); `prepare_member` (Task 9).
- Produces: `fn member_of<'a>(plan: &'a GroupPlan, host_id: &str) -> Option<&'a MemberPlan>`; a worker launch reports `ProcessIdentity` roles `worker-<r>` and children and no ingress; `Terminate` on a member handle is the journal-owned SIGTERM, bounded wait, SIGKILL path, and the gone report carries `escalated: bool`.

- [ ] **Step 1: Write the failing tests** (Fake launcher, no real engine)

```rust
// T30: a worker launches, journals first, reports identities, opens no ingress (each engine).
#[tokio::test]
async fn worker_member_launches_and_journals_first() {
    for engine in [GroupEngine::Vllm, GroupEngine::Sglang, GroupEngine::Tensorfold] {
        let host = FakeHost::new("host-b").with_groups_policy("192.0.2.11").with_engine(engine.as_str());
        let plan = two_member_plan(engine);
        let out = host.execute(MemberAction::Launch(plan.clone())).await.unwrap();
        assert!(host.journal().has_launch_for(&plan, 1), "{engine:?}");
        assert!(out.ingress.is_none());
        assert!(out.processes.iter().all(|p| p.role.starts_with("worker-1")));
    }
}

// T30: the head launches with its loopback API and ingress.
#[tokio::test]
async fn head_member_launches_with_ingress() {
    let host = FakeHost::new("host-a").with_groups_policy("192.0.2.10").with_engine("sglang");
    let out = host.execute(MemberAction::Launch(two_member_plan(GroupEngine::Sglang))).await.unwrap();
    assert!(out.ingress.is_some());
    assert!(host.last_argv().windows(2).any(|w| w[0] == "--node-rank" || w[0] == "--rank"));
}

// T30: a plan that does not name this host, or whose Prepare checks fail now, spawns nothing.
#[tokio::test]
async fn plan_without_this_host_is_refused() {
    let host = FakeHost::new("host-c").with_groups_policy("192.0.2.12");
    assert!(host.execute(MemberAction::Launch(two_member_plan(GroupEngine::Vllm))).await.is_err());
    assert!(host.journal().is_empty());
}

// Review Focus 6, T31: a SGLang worker that ignores SIGTERM is killed and settles, escalation recorded.
#[tokio::test]
async fn sglang_worker_ignoring_sigterm_is_escalated() {
    let host = FakeHost::new("host-b").with_groups_policy("192.0.2.11").with_engine("sglang")
        .with_fake_ignoring_sigterm();
    let out = host.execute(MemberAction::Launch(two_member_plan(GroupEngine::Sglang))).await.unwrap();
    let gone = host.terminate(&out.owned_handle, &out.processes).await.unwrap();
    assert!(gone.all_gone());
    assert!(gone.escalated);
}

// T31, T33: a worker terminates its own recorded tree only.
#[tokio::test]
async fn worker_member_terminates_its_recorded_tree() {
    let host = FakeHost::new("host-b").with_groups_policy("192.0.2.11");
    let out = host.execute(MemberAction::Launch(two_member_plan(GroupEngine::Vllm))).await.unwrap();
    let gone = host.terminate(&out.owned_handle, &out.processes).await.unwrap();
    assert!(gone.all_gone());
    assert!(!gone.escalated);
}
```

`FakeHost` extends the agent's existing Fake-launcher fixture (search `struct Fake` in `crates/capyctl-agent/tests/`) with `with_groups_policy`, `with_engine`, `with_fake_ignoring_sigterm` (the fake child traps SIGTERM) and `last_argv`.

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p capyctl-agent --test group_launch` — expected: the native path returns `Unauthorized`.

- [ ] **Step 3: Implement.** `render_launch` accepts `Launch(GroupPlan)`: re-run `prepare_member` (a Launch without passing checks never spawns), take `member_of`, build the engine's input from the member (its `model_path`, devices → `CUDA_VISIBLE_DEVICES` as today, `group: member_args(plan, host_id)`), then the existing durable-spawn path. Workers skip ingress and the readiness wait; they return once identities are recorded. The terminate path records whether SIGKILL was needed. Comments cite `// ADR 0028 §8, §11`.

- [ ] **Step 4: Run the tests.** Run: `cargo test -p capyctl-agent` — expected: PASS; every single-rank launch test unchanged.

- [ ] **Step 5: Commit**

```bash
git add crates/capyctl-agent
git commit -m "feat(agent): launch and terminate group members for vLLM, SGLang and TensorFold"
```

---

### Task 14: Fake multi-host harness

**Files:**
- Create: `crates/capyctl-testkit/src/fake_group.rs`, `crates/capyctl-controller/src/coordinator/tests_groups.rs` (registered in the coordinator's test modules)
- Modify: `crates/capyctl-testkit/src/lib.rs`

**Interfaces:**
- Produces:
  - `pub struct FakeGroup`; `FakeGroup::new(hosts: &[&str]) -> Self` (clonable handle, shared state).
  - `fn launch(&self, rank: u32)`; `fn head_ready(&self) -> bool` (true only after every rank launched and none exited).
  - Faults: `exit_rank(rank)`, `disconnect_host(host)`, `reconnect_host(host, JournalState)` (`JournalState::{Kept, Empty}`), `sleep_leaves_rank_resident(rank)`, `wake_output(tokens: Vec<u32>)`, `kill_rank_process(rank)`.
  - Observations: `resident_bytes(rank) -> u64`, `parked_bytes() -> u64`, `alive(rank) -> bool`, `sleep_calls() -> u32`, `sleep() -> Result<(), String>`, `wake() -> Result<(), String>`, `complete(prompt) -> Vec<u32>`.
  - `GroupWorld` in `tests_groups.rs`: the coordinator and store with N `ScriptedHost` agents (the existing remote-test stub in `coordinator/tests_remote.rs`) wired to one `FakeGroup`; constructors `GroupWorld::hosts(&["host-a", "host-b"])`, `GroupWorld::ready_group(name, hosts)`; helpers used by Tasks 15–19: `deploy_group`, `try_deploy_group`, `deploy_group_with_env`, `start`, `stop`, `park`, `wake`, `request`, `wait_ready`, `wait_settled_generation`, `status`, `route_open`, `owner_bytes_on`, `port_free`, `launches`, `commands_sent_to`, `launch_env`, builders `prepare_refuses`, `launch_fails`, `without_capability`, `approved_env`, `profile_fingerprint`, `recovery_reconcile`, `with_single_rank`, `advance_past_lease_expiry`.

- [ ] **Step 1: Write the failing tests**

```rust
// T30: the head is not ready until every rank has launched.
#[test]
fn head_waits_for_every_rank() {
    let g = FakeGroup::new(&["a", "b", "c", "d"]);
    for r in 0..3 { g.launch(r); }
    assert!(!g.head_ready());
    g.launch(3);
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
    assert!(g.resident_bytes(1) > g.parked_bytes());
    assert_eq!(g.resident_bytes(0), g.parked_bytes());
}

// Harness smoke: a four-host world deploys a TP2 x PP2 group through the coordinator.
#[tokio::test]
async fn group_world_four_hosts_smoke() {
    let world = GroupWorld::hosts(&["h0", "h1", "h2", "h3"]);
    let id = world.deploy_group_shape("g", 2, 2).await;
    world.group.launch_completes();
    world.wait_ready(&id).await;
    assert_eq!(world.launches(), 4);
}
```

The smoke test fails until Task 16; mark it `#[ignore = "enabled by Task 16"]` here and remove the attribute in Task 16.

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p capyctl-testkit fake_group` — expected: unresolved module.

- [ ] **Step 3: Implement** `FakeGroup` over `Arc<Mutex<State>>` (per-rank launched, exited, resident bytes, host connectivity, sleep count, wake output) and `GroupWorld` on top of the scripted hosts: each scripted host answers `Prepare`, `Launch`, `Terminate`, `Park`, `Restore` and residency queries from the shared `FakeGroup`, and fakes host facts with documentation addresses.

- [ ] **Step 4: Run the tests.** Run: `cargo test -p capyctl-testkit && cargo test -p capyctl-controller tests_groups` — expected: PASS (smoke ignored).

- [ ] **Step 5: Commit**

```bash
git add crates/capyctl-testkit crates/capyctl-controller
git commit -m "test(testkit): fake multi-host group and coordinator group world"
```

---

### Task 15: Parallel weights and cross-host digest agreement

**Files:**
- Create: `crates/capyctl-controller/src/group_sources.rs`
- Modify: `crates/capyctl-controller/src/lib.rs`, `crates/capyctl-controller/src/model_sources.rs` and `checkpoint_digests.rs` (member id and host taken as arguments instead of `"head"`; digests recorded per host)
- Test: unit tests in `group_sources.rs`

**Interfaces:**
- Consumes: per-host materialization (`model_sources.rs`), digest request (`checkpoint_digests.rs`), `record_digest`/`digests_for` (Task 8).
- Produces:
  - `pub async fn materialize_on_all(hosts: &[String], source: &ModelSource, driver: &impl SourceDriver) -> Result<BTreeMap<String, Materialized>, GroupSourceError>`.
  - `pub fn agree_digests(per_host: &BTreeMap<String, String>) -> Result<String, GroupSourceError>`.
  - `pub enum GroupSourceError { Space { hosts: Vec<String> }, Failed { host: String, reason: String }, Mismatch { digests: BTreeMap<String, String> } }` with `code()` → `insufficient_space`, the host's reason, `group_checkpoint_mismatch`.
  - `SourceDriver` trait with one production impl delegating to the existing sender.

- [ ] **Step 1: Write the failing tests**

```rust
// T07: every host materializes concurrently; hosts short of space fail the group before launch.
#[tokio::test]
async fn materialization_is_parallel_and_space_is_checked() {
    let driver = FakeSourceDriver::new()
        .host("host-a", Ok(mat("/m", "sha256:c")))
        .host("host-b", Err(SourceFailure::InsufficientSpace))
        .host("host-c", Err(SourceFailure::InsufficientSpace));
    let err = materialize_on_all(&hosts(&["host-a", "host-b", "host-c"]), &hf_source(), &driver).await.unwrap_err();
    assert_eq!(err.code(), "insufficient_space");
    assert!(err.to_string().contains("host-b") && err.to_string().contains("host-c"));
    assert_eq!(driver.max_in_flight(), 3);
}

// T14: digests must agree across hosts; a mismatch names every host.
#[test]
fn digests_must_agree() {
    let same = BTreeMap::from([("host-a".into(), "sha256:c".into()), ("host-b".into(), "sha256:c".into())]);
    assert_eq!(agree_digests(&same).unwrap(), "sha256:c");
    let diff = BTreeMap::from([("host-a".into(), "sha256:c".into()), ("host-b".into(), "sha256:d".into())]);
    let err = agree_digests(&diff).unwrap_err();
    assert_eq!(err.code(), "group_checkpoint_mismatch");
    assert!(err.to_string().contains("host-a") && err.to_string().contains("host-b"));
}
```

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p capyctl-controller group_sources` — expected: unresolved.

- [ ] **Step 3: Implement** with `futures::future::join_all` (already a dependency; add none), collecting every host's outcome before deciding. Comment `// ADR 0028 §6 (owner decision 11)`.

- [ ] **Step 4: Run the tests.** Run: `cargo test -p capyctl-controller` — expected: PASS; single-host digest tests unchanged.

- [ ] **Step 5: Commit**

```bash
git add crates/capyctl-controller
git commit -m "feat(controller): group weights on every host in parallel and cross-host digest agreement"
```

---

### Task 16: Group activation: reserve, prepare, fan-out, head readiness, route

**Files:**
- Create: `crates/capyctl-controller/src/group_activation.rs`
- Modify: `crates/capyctl-controller/src/coordinator/worker.rs` and `scheduler.rs` (a deployment with `InstanceSpec.group` goes to group activation), `remote_execution.rs` and `remote_readiness.rs` (member id from the plan), `crates/capyctl-management/src/configuration.rs` (resolve on every named host; `group_profile_mismatch`, `peer_address_missing`, `engine_env_not_approved` on any host, `host_capability_missing:engine_groups`), `crates/capyctl-router/src/balance.rs` (a group is one replica at the head's ingress)
- Test: `crates/capyctl-controller/src/coordinator/tests_groups.rs`

**Interfaces:**
- Consumes: `reserve_group` (Task 8), `group_refusal` (Task 7), `materialize_on_all`, `agree_digests` (Task 15), `GroupWorld` (Task 14), `check_engine_shape` (Task 4).
- Produces:
  - `pub async fn activate_group(ctx: &CoordinatorCtx, deployment: &DeploymentRecord, shape: &GroupShape) -> Result<GroupActivation, GroupActivationError>`.
  - `pub enum GroupActivation { Ready { plan: GroupPlan }, Failed { plan: GroupPlan, failed_rank: u32, reason: String } }`; `Failed` hands off to Task 17's `stop_group`.
  - Order: sources → build one `AdmissionContext` per member host → `reserve_group` → `Prepare` to all concurrently → on any refusal release all reservations → for each member, `mark_member_dispatching` must commit before its `Launch` is sent (and before every retry) → `Launch` to all concurrently → `mark_member_launched` with the identities from each Launch reply → head readiness while watching every member's exit reports → open the route on `Ready`.

- [ ] **Step 1: Write the failing tests**

```rust
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
        assert_eq!(world.owner_bytes_on("host-b", &member_owner_id("g", 0, 1)), world.member_request());
    }
}

// T30: a Prepare refusal on one host releases every member and launches nothing.
#[tokio::test]
async fn prepare_refusal_releases_everything() {
    let world = GroupWorld::hosts(&["host-a", "host-b"]).prepare_refuses("host-b", "host_tuning_missing:memlock");
    let id = world.deploy_group("g", &["host-a", "host-b"]).await;
    let status = world.wait_settled(&id).await;
    assert_eq!(status.last_error(), "host_tuning_missing:memlock");
    assert_eq!(world.launches(), 0);
    assert_eq!(world.owner_bytes_on("host-a", &member_owner_id("g", 0, 0)), 0);
}

// T15: concurrent activation requests produce one plan and one launch per member.
#[tokio::test]
async fn concurrent_activation_is_single() {
    let world = GroupWorld::hosts(&["host-a", "host-b"]);
    let id = world.deploy_group_no_start("g", &["host-a", "host-b"]).await;
    let (a, b) = tokio::join!(world.start(&id), world.start(&id));
    assert!(a.is_ok() && b.is_ok());
    assert_eq!(world.launches(), 2);
}

// T34: a named host without engine_groups is refused typed and receives nothing.
#[tokio::test]
async fn host_without_capability_is_refused() {
    let world = GroupWorld::hosts(&["host-a", "host-b"]).without_capability("host-b", "engine_groups");
    let err = world.try_deploy_group("g", &["host-a", "host-b"]).await.unwrap_err();
    assert_eq!(err.code(), "host_capability_missing:engine_groups");
    assert_eq!(world.commands_sent_to("host-b"), 0);
}

// T14, T37, Review Focus 1: an env name unapproved on one host refuses the group;
// members render equal environments apart from the address variable.
#[tokio::test]
async fn group_engine_env_is_approved_everywhere_and_equal() {
    let world = GroupWorld::hosts(&["host-a", "host-b"]).with_engine("sglang").approved_env("host-a", &["SGLANG_ENABLE_*"]);
    let err = world.try_deploy_group_with_env("g", &["host-a", "host-b"], &[("SGLANG_ENABLE_X", "1")]).await.unwrap_err();
    assert_eq!(err.code(), "engine_env_not_approved:SGLANG_ENABLE_X");
    let world = GroupWorld::hosts(&["host-a", "host-b"]).with_engine("sglang")
        .approved_env("host-a", &["SGLANG_ENABLE_*"]).approved_env("host-b", &["SGLANG_ENABLE_*"]);
    world.deploy_group_with_env("g", &["host-a", "host-b"], &[("SGLANG_ENABLE_X", "1")]).await;
    let strip = |mut e: std::collections::BTreeMap<String, String>| { e.remove("SGLANG_HOST_IP"); e };
    assert_eq!(strip(world.launch_env("host-a")), strip(world.launch_env("host-b")));
}

// T14: one mismatched build or a host without a peer address refuses the deploy.
#[tokio::test]
async fn profile_mismatch_and_missing_peer_address_refuse_deploy() {
    let world = GroupWorld::hosts(&["host-a", "host-b"]).profile_fingerprint("host-b", "other");
    assert_eq!(world.try_deploy_group("g", &["host-a", "host-b"]).await.unwrap_err().code(), "group_profile_mismatch");
    let world = GroupWorld::hosts(&["host-a", "host-b"]).without_peer_address("host-b");
    assert_eq!(world.try_deploy_group("g", &["host-a", "host-b"]).await.unwrap_err().code(), "peer_address_missing");
}
```

Remove the `#[ignore]` from Task 14's four-host smoke test.

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p capyctl-controller tests_groups` — expected: fail.

- [ ] **Step 3: Implement `activate_group`** in the order above. `Prepare` and `Launch` fan-out use `join_all` over per-host sends, each command carrying the plan's generation and `CommandIdentity { member: MemberKey { host_id, member_id: member_id(rank) }, instance_index: 0, .. }`. Readiness uses the engine's existing native readiness check against the head only (`// ADR 0028 §9, owner decision 5`), bounded by `timeouts.initialize`, returning `Failed` on any member exit. The lifecycle claim spans the whole activation. The router registers the head's ingress as the only replica.

- [ ] **Step 4: Run the tests.** Run: `scripts/ci-local.sh --deep --only core` (or the core suite command from `AGENTS.md`) — expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/capyctl-controller crates/capyctl-management crates/capyctl-router
git commit -m "feat(controller): group activation: reserve all, prepare, concurrent launch, head readiness"
```

---

### Task 17: Stop, failure, settlement, uncertainty and recovery

**Files:**
- Create: `crates/capyctl-controller/src/group_settlement.rs`
- Modify: `crates/capyctl-controller/src/coordinator/worker.rs` (stop, member exit), the engine-exit handling (a member exit maps to its group)
- Test: `crates/capyctl-controller/src/coordinator/tests_groups.rs`

**Interfaces:**
- Consumes: `settle_member`, `mark_member_uncertain`, `group_plan` (Task 8); `GroupActivation::Failed` (Task 16).
- Produces:
  - `pub async fn stop_group(ctx: &CoordinatorCtx, plan: &GroupPlan, reason: StopReason) -> GroupSettlement` — close head ingress, drain, `Terminate` every member concurrently, settle each on its own gone-evidence, mark unreachable members uncertain.
  - `pub async fn on_member_exit(ctx: &CoordinatorCtx, deployment_id: &str, generation: i64, rank: u32)` → `stop_group(.., StopReason::MemberFailed { rank })`, status `group_member_failed`.
  - Relaunch under `recovery: reconcile` only after `GroupSettlement::Complete`, checked in the transaction that draws the new generation.

- [ ] **Step 1: Write the failing tests**

```rust
// T31: a worker exit stops the head; each host releases on its own evidence; the port frees.
#[tokio::test]
async fn worker_exit_stops_the_group() {
    let world = GroupWorld::ready_group("g", &["host-a", "host-b"]).await;
    world.group.exit_rank(1);
    let status = world.wait_settled_generation("g", 1).await;
    assert_eq!(status.last_error(), "group_member_failed");
    assert!(!world.group.alive(0));
    assert_eq!(world.owner_bytes_on("host-a", &member_owner_id("g", 0, 0)), 0);
    assert_eq!(world.owner_bytes_on("host-b", &member_owner_id("g", 0, 1)), 0);
    assert!(world.port_free("host-a", 25000));
}

// T31: a head exit terminates the worker before any relaunch.
#[tokio::test]
async fn head_exit_terminates_worker_before_any_relaunch() {
    let world = GroupWorld::ready_group("g", &["host-a", "host-b"]).await.recovery_reconcile();
    world.group.exit_rank(0);
    world.wait_settled_generation("g", 1).await;
    assert!(!world.group.alive(1));
    assert!(world.generation_started_after_settlement("g", 2));
}

// T32: an unreachable worker host keeps its share charged and uncertain; the port stays held.
#[tokio::test]
async fn unreachable_host_keeps_charge_and_port() {
    let world = GroupWorld::ready_group("g", &["host-a", "host-b"]).await;
    world.group.disconnect_host("host-b");
    world.stop(&world.id("g")).await;
    assert_eq!(world.status("g").await.member(1).state, "uncertain");
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
    let world = GroupWorld::ready_group("g", &["host-a", "host-b"]).await.recovery_reconcile();
    world.group.disconnect_host("host-b");
    world.group.exit_rank(0);
    world.group.reconnect_host("host-b", JournalState::Empty);
    world.settle_for(std::time::Duration::from_secs(5)).await;
    assert_eq!(world.status("g").await.member(1).state, "uncertain");
    assert!(!world.generation_started("g", 2));
    world.group.kill_rank_process(1);
    world.wait_settled_generation("g", 1).await;
    assert!(world.generation_started("g", 2));
}

// T30: a launch failure on one member after the other launched is compensated.
#[tokio::test]
async fn launch_failure_is_compensated() {
    let world = GroupWorld::hosts(&["host-a", "host-b"]).launch_fails("host-b");
    let id = world.deploy_group("g", &["host-a", "host-b"]).await;
    world.wait_settled_generation("g", 1).await;
    assert!(!world.group.alive(0));
    assert!(!world.route_open("g"));
    assert_eq!(world.status_of(&id).await.last_error(), "group_member_failed");
}
```

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p capyctl-controller tests_groups` — expected: new tests fail.

- [ ] **Step 3: Implement** `stop_group` and `on_member_exit`. Every release goes through `settle_member` with the host's own `MemberGone`; lease expiry and timeouts only call `mark_member_uncertain`. Comments cite `// ADR 0028 §11, owner decision 6; SPEC §11: lease expiry never frees memory`.

- [ ] **Step 4: Run the tests.** Run the core suite — expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/capyctl-controller
git commit -m "feat(controller): whole-group stop on rank failure, per-host settlement, uncertain members"
```

---

### Task 18: Group eviction across named hosts

**Files:**
- Modify: `crates/capyctl-scheduler/src/placement.rs` (a group has fixed hosts: placement validates fit only; new `plan_group_eviction`), `crates/capyctl-store/src/ordinary_lifecycle/switching.rs` (a group victim is evicted whole; a group request computes victims on every named host)
- Test: `crates/capyctl-scheduler` unit tests, `crates/capyctl-controller/src/coordinator/tests_groups.rs`

**Interfaces:**
- Consumes: the per-host victim chooser (search `victim` in `crates/capyctl-scheduler/src/`).
- Produces: `pub fn plan_group_eviction(per_host: &BTreeMap<String, HostCandidateView>, need: &BTreeMap<String, u64>) -> Option<BTreeMap<String, Vec<Victim>>>` — `None` when any host cannot make room.

- [ ] **Step 1: Write the failing tests**

```rust
// T16, T27: if one named host cannot make room, nothing is evicted anywhere.
#[test]
fn eviction_is_all_or_nothing_across_hosts() {
    let views = views(&[("host-a", free(10), &[victim("x", 80)]), ("host-b", free(10), &[])]);
    assert!(plan_group_eviction(&views, &need(&[("host-a", 80), ("host-b", 80)])).is_none());
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

// T16: A -> B -> A with a group and a single-rank deployment on host A.
#[tokio::test]
async fn group_and_single_rank_alternate() {
    let world = GroupWorld::ready_group("g", &["host-a", "host-b"]).await.with_single_rank("s", "host-a");
    world.request("s").await;
    assert_eq!(world.state("g").await, "parked");
    world.request("g").await;
    assert_eq!(world.state("g").await, "ready");
    world.assert_release_evidence_per_member("g");
}
```

`views`, `free`, `victim`, `need` and `names` are small test builders over `HostCandidateView`.

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p capyctl-scheduler && cargo test -p capyctl-controller tests_groups` — expected: unresolved.

- [ ] **Step 3: Implement.** Compute every named host's victim set first; act only when all are `Some`; a group victim appears on each of its hosts and is parked or stopped as one unit (Tasks 19, 17). Comment `// ADR 0028 §5, SPEC §11: validate all hosts before evicting`.

- [ ] **Step 4: Run the tests.** Run the core suite — expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/capyctl-scheduler crates/capyctl-store crates/capyctl-controller
git commit -m "feat(switching): all-or-nothing group eviction across named hosts"
```

---

### Task 19: Group park and wake with per-rank evidence, canary and restart-only groups

**Files:**
- Create: `crates/capyctl-controller/src/group_residency.rs`
- Modify: `crates/capyctl-controller/src/coordinator/worker.rs` (park, wake for groups), the agent's process residency report (keyed by member), `crates/capyctl-store/src/groups.rs` (member parked charge, canary reference per generation)
- Test: `crates/capyctl-controller/src/coordinator/tests_groups.rs`

**Interfaces:**
- Consumes: `FakeGroup` faults (Task 14), `stop_group` (Task 17), `group_support` (Task 4).
- Produces:
  - `pub async fn park_group(ctx: &CoordinatorCtx, plan: &GroupPlan) -> Result<(), GroupResidencyError>` — deep: head collective once, then every member's residency at or below its parked budget within the park deadline; restart-only engines: `stop_group`.
  - `pub async fn wake_group(ctx: &CoordinatorCtx, plan: &GroupPlan) -> Result<(), GroupResidencyError>` — deep: head collective once, every member resident, head readiness, canary; restart-only: a new activation.
  - `pub struct CanaryReference { pub prompt: String, pub tokens: Vec<u32> }` recorded at first readiness (`temperature: 0`, `max_tokens: 8`).
  - `GroupResidencyError::{MemberSilent { rank }, MemberResident { rank }, CanaryMismatch}` → codes `group_member_uncertain`, `group_member_failed`, `group_wake_mismatch`; each triggers `stop_group`.

- [ ] **Step 1: Write the failing tests**

```rust
// T20: park settles only when every rank reports its memory released; one collective only.
#[tokio::test]
async fn park_needs_every_rank() {
    for engine in ["vllm", "sglang"] {
        let world = GroupWorld::ready_group_with("g", &["host-a", "host-b"], engine).await;
        world.park("g").await.unwrap();
        assert_eq!(world.group.sleep_calls(), 1);
        assert_eq!(world.state("g").await, "parked");
        assert_eq!(world.owner_bytes_on("host-b", &member_owner_id("g", 0, 1)), world.member_parked_budget());
    }
}

// Review Focus 4, T20: the head's call succeeds but rank 1 stays resident: charge kept, group stopped.
#[tokio::test]
async fn resident_rank_after_sleep_stops_the_group() {
    let world = GroupWorld::ready_group("g", &["host-a", "host-b"]).await;
    world.group.sleep_leaves_rank_resident(1);
    let err = world.park("g").await.unwrap_err();
    assert_eq!(err.code(), "group_member_failed");
    assert_eq!(world.group.sleep_calls(), 1);
    assert_ne!(world.state("g").await, "parked");
    world.wait_settled_generation("g", 1).await;
}

// T20: a member that never reports keeps its full charge and is uncertain.
#[tokio::test]
async fn silent_member_keeps_full_charge() {
    let world = GroupWorld::ready_group("g", &["host-a", "host-b"]).await;
    world.group.disconnect_host("host-b");
    let _ = world.park("g").await;
    assert_eq!(world.owner_bytes_on("host-b", &member_owner_id("g", 0, 1)), world.member_request());
    assert_eq!(world.status("g").await.member(1).state, "uncertain");
}

// T20: a wake whose canary differs stops the group.
#[tokio::test]
async fn wake_canary_mismatch_stops_the_group() {
    let world = GroupWorld::ready_group("g", &["host-a", "host-b"]).await;
    world.park("g").await.unwrap();
    world.group.wake_output(vec![9, 9, 9]);
    assert_eq!(world.wake("g").await.unwrap_err().code(), "group_wake_mismatch");
}

// T20: five clean cycles keep the canary identical.
#[tokio::test]
async fn repeated_cycles_are_clean() {
    let world = GroupWorld::ready_group("g", &["host-a", "host-b"]).await;
    for _ in 0..5 {
        world.park("g").await.unwrap();
        world.wake("g").await.unwrap();
    }
    assert_eq!(world.group.sleep_calls(), 5);
}

// T22: a TensorFold group parks by stopping both ranks and wakes by relaunching.
#[tokio::test]
async fn tensorfold_group_is_restart_only() {
    let world = GroupWorld::ready_group_with("g", &["host-a", "host-b"], "tensorfold").await;
    world.park("g").await.unwrap();
    assert_eq!(world.group.sleep_calls(), 0);
    assert!(!world.group.alive(0) && !world.group.alive(1));
    assert_eq!(world.owner_bytes_on("host-b", &member_owner_id("g", 0, 1)), 0);
    world.wake("g").await.unwrap();
    assert!(world.generation_started("g", 2));
}
```

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p capyctl-controller tests_groups` — expected: unresolved.

- [ ] **Step 3: Implement.** Only the head's agent receives `Park`/`Restore` (existing actions, member id `head`); worker agents report `process_residency` for their member handle, now keyed by member. A member's charge moves to its parked budget only on its own report. `max_parked` counts the group once on each host. Restart-only groups (TensorFold; SGLang if the owner chooses the fallback) take the stop and activation paths. Comments cite `// ADR 0028 §12, SPEC §11: the lead agent invokes each collective once`.

- [ ] **Step 4: Run the tests.** Run the core suite — expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/capyctl-controller crates/capyctl-agent crates/capyctl-store
git commit -m "feat(controller): group park and wake with per-rank evidence and a wake canary"
```

---

### Task 20: Status, codes, exits and user docs

**Files:**
- Modify: `crates/capyctl-management/src/status.rs` (group rows), `crates/capyctl-cli/src/output.rs` (text view, codes → exits), `docs/guide/several-machines.md` ("One model across machines"), `docs/operations/configuration.md` (three group settings, engine environment settings), `docs/operations/network-access.md` (risk), `docs/guide/engines.md` (`--env`, `--approve-env`, group support table), `docs/examples/deployment-multinode.yaml` (real two-host TP 2 group) and `docs/examples/deployment-spread.yaml` (the current spread example, moved)
- Test: `crates/capyctl-cli` output tests, `crates/capyctl-management` status tests, the example-validation test (search `deployment-multinode.yaml` in tests)

**Interfaces:**
- Produces: status JSON for a group instance: `{"engine", "topology": {"tensor_parallel", "pipeline_parallel"}, "rendezvous": "<head>:<port>", "peer_transport": "unauthenticated", "members": [{"host", "node_rank", "role", "state", "processes", "reservation", "residency", "last_error", "warnings"}]}`; text view as in spec §15.

- [ ] **Step 1: Write the failing tests**

```rust
// T14: every closed group code maps to its exit.
#[test]
fn group_codes_map_to_exits() {
    for (code, exit) in [
        ("group_placement_required", 2), ("group_topology_invalid", 2), ("peer_address_missing", 2),
        ("group_profile_mismatch", 2), ("engine_env_not_approved:X", 2),
        ("group_shape_unsupported", 5), ("group_shape_unsupported:tensorfold", 5),
        ("group_instances_unsupported", 5), ("group_drift:node_rank", 5),
        ("host_capability_missing:engine_groups", 5),
        ("rendezvous_ports_exhausted", 4), ("rendezvous_port_in_use:25000", 4),
        ("host_tuning_missing:memlock", 4),
    ] {
        assert_eq!(exit_for_code(code).0, exit, "{code}");
    }
}

// T21: a group instance is marked unauthenticated and lists every member.
#[test]
fn status_marks_peer_transport_and_members() {
    let s = render_status_json(&two_member_group_status());
    assert_eq!(s["instances"][0]["peer_transport"], "unauthenticated");
    assert_eq!(s["instances"][0]["members"].as_array().unwrap().len(), 2);
    assert_eq!(s["instances"][0]["members"][1]["role"], "worker");
    let text = render_status_text(&two_member_group_status());
    assert!(text.contains("peer transport unauthenticated"));
    assert!(text.contains("RANK  HOST"));
}

// T03: the multinode example validates against two host documents.
#[test]
fn multinode_example_validates() {
    validate_example("docs/examples/deployment-multinode.yaml", &["docs/examples/host.yaml", "docs/examples/host-b.yaml"]).unwrap();
}
```

Use the real function names in `output.rs` for `exit_for_code`, `render_status_json` and `render_status_text`; add `docs/examples/host-b.yaml` (a second host with `groups.peer_address: 192.0.2.11`) and a `groups.peer_address: 192.0.2.10` to `host.yaml`.

- [ ] **Step 2: Run to verify failure.** Run: `cargo test -p capyctl-cli && cargo test -p capyctl-management` — expected: fail.

- [ ] **Step 3: Implement** the mappings (prefix match for `group_shape_unsupported:`, `group_drift:`, `host_tuning_*:`, `rendezvous_port_in_use:`, `service_port_in_use:`), the status rows, the docs (short, matching real output exactly; the guide section shows the deployment, the status view and the risk in plain words), and the examples.

- [ ] **Step 4: Run the tests.** Run: `scripts/ci-local.sh` — expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/capyctl-cli crates/capyctl-management docs
git commit -m "feat(cli): group status, codes and exits; docs for one model across machines"
```

---

### Task 21: Live bring-up rows MN1–MN8 on two GB10 hosts

**Files:**
- Create: `scripts/live/matrix/rows/MN1.sh` … `MN8.sh`, `scripts/live/matrix/rdma_counters.py`, `scripts/live/matrix/tests/test_rdma_counters.py`
- Modify: `scripts/live/matrix/hosts.example.env` (`HOST_A_DIRECT`, `HOST_B_DIRECT`, `GROUP_MODEL_VLLM_SGLANG`, `GROUP_MODEL_TENSORFOLD`), `scripts/live/matrix/gen_deployment.py` (topology), `scripts/live/matrix/gen_host_doc.py` (`groups` block), `docs/runbooks/f2-current-status.md`

**Interfaces:**
- Produces: `rdma_counters.py` prints the summed `port_xmit_data`/`port_rcv_data` per device of `/sys/class/infiniband/*/ports/*/counters/` as JSON; read-only; `{"present": false}` without the tree.

Prerequisites (owner, as root, before MN1; CapyCTL checks and never changes them): `vm.compaction_proactiveness=0`, unlimited memlock for the service user, read-write `/dev/infiniband/uverbs*`, each host's direct-link address as its peer address. vLLM 0.30.0, SGLang 0.5.21 and TensorFold 0.6.5 registered on both hosts under the same names with equal build fingerprints. Bring-up checkpoints downloaded through CapyCTL on both hosts: Qwen3-30B-A3B (vLLM, SGLang) and Nemotron 3.5 Lightning 30B-A3B MLX 4-bit (TensorFold).

- [ ] **Step 1: Write the counter helper and its unit test.**

```python
# scripts/live/matrix/tests/test_rdma_counters.py
class CounterTests(unittest.TestCase):
    def test_sums_every_port(self):
        with tempfile.TemporaryDirectory() as root:
            for dev, port, tx in [("mlx5_0", "1", "10"), ("mlx5_1", "1", "5")]:
                d = os.path.join(root, dev, "ports", port, "counters")
                os.makedirs(d)
                open(os.path.join(d, "port_xmit_data"), "w").write(tx)
                open(os.path.join(d, "port_rcv_data"), "w").write("1")
            out = rdma_counters.read(root)
            self.assertEqual(out["devices"]["mlx5_0"]["xmit"], 10)
            self.assertEqual(out["total"]["xmit"], 15)

    def test_missing_tree(self):
        self.assertEqual(rdma_counters.read("/nonexistent"), {"present": False})
```

Run: `python3 -m unittest discover -s scripts/live/matrix/tests` — expected: FAIL, then implement `read(root="/sys/class/infiniband")` and the CLI, then PASS.

- [ ] **Step 2: Write MN1–MN3.** One script per engine: deploy the group through the server with `placement.hosts: [host A, host B]`, `topology.tensor_parallel: 2`; wait READY; read counters on both hosts; send 20 completions through the router; read counters again. Pass: READY, 20/20, counters rose on both hosts (records `transport: rdma` and which devices carried traffic), else records `transport: socket`, passes bring-up and flags it in the runbook.

- [ ] **Step 3: Write MN4–MN8** per spec §17.2: MN4 five park/wake cycles (vLLM, SGLang) with per-host residency and the canary; MN5 `signal_owned.py` kills host B's engine (each engine); MN6 kills host A's engine (each engine); MN7 stops host B's agent service through the CLI (no firewall change), checks `uncertain` and the charge, restarts it, checks settlement; MN8 alternates the group with a single-rank deployment on host A. Every row cleans up on failure (`rowlib.sh` trap) and prints evidence lines only.

- [ ] **Step 4: Run MN1–MN8 on host A and host B.** Run: `scripts/live/matrix/run_row.sh MN1` … `MN8`. Expected: pass. On a failure, debug before continuing; record the fix in its commit. If MN4 fails for SGLang after debugging, stop and take spec open question 2 to the owner.

- [ ] **Step 5: Record and commit.** One status runbook section: commit range, each row's result per engine, the transport observed, and "CPU and Fake-engine tests are not qualification; these rows are."

```bash
git add scripts/live/matrix docs/runbooks/f2-current-status.md
git commit -m "test(live): multi-node group rows MN1-MN8 on two hosts for three engines"
```

---

### Task 22: Flash-Next benchmark through the router (MN9)

**Files:**
- Create: `scripts/live/matrix/rows/MN9.sh`
- Modify: `docs/benchmarks/` (new `<date>-two-host-flash-next.md` on the day it runs), `docs/runbooks/f2-current-status.md`

Prerequisites: Qwen3.8-Flash-Next downloaded through CapyCTL on both hosts (RadixArk NVFP4 for SGLang and vLLM; the MLX 4-bit export for TensorFold two-rank), with equal digests; recipe-only environment variables declared through `engine add --env`/`--approve-env` or `engine_config.env` (Tasks 2, 3); owner approval of downloads per spec open question 3.

- [ ] **Step 1: Write MN9.** Per engine: deploy with the reference recipe's non-transport flags as `engine_config`/approved extra args and `timeouts.initialize: 1800s`; run `bench.py` through the router at concurrency 1, 2, 4, 8, 16, 32, 64, three runs each; report average and peak tok/s, TTFT and the RDMA counters. A build that cannot load the model is recorded as such (vLLM stock 0.30.0 may not), not patched in this task.

- [ ] **Step 2: Run MN9.** Pass per engine: averages within about 10% of that engine's reference (vLLM published two-Spark recipe about 95 tok/s at 1 and 735 at 64; SGLang cookbook two-Spark TP 2 cells; TensorFold two-rank Flash Next recipe numbers). A miss is recorded with the transport evidence, not tuned with NCCL variables (decision 4); it goes to the owner (spec open question 1).

- [ ] **Step 3: One deep park and wake** of the SGLang or vLLM Flash-Next group with per-rank evidence and the canary, then one concurrency-1 run to check no loss after wake.

- [ ] **Step 4: Record.** Benchmark document (method, pinned build fingerprints, numbers, peaks, transport) and the status runbook section.

- [ ] **Step 5: Commit**

```bash
git add scripts/live/matrix docs/benchmarks docs/runbooks/f2-current-status.md
git commit -m "test(live): two-host Flash-Next benchmark through the router on three engines (MN9)"
```

---

## Self-review notes

- Spec coverage: §2 configuration → Tasks 4 (topology, shape support, standalone refusal); §2.1 engine environment → 2, 3; §3 host policy three ways → 5; §4 plan → 6; §5 reservations, memory per rank, ports, eviction → 8, 18; §6 weights → 8 (per-host digests), 15; §7 prepare and host checks → 9; §8 fan-out → 13, 16; §9 readiness and router → 16; §10 adapters → 10, 11, 12; §11 stop and failure → 13 (escalation), 17; §12 park and wake → 19; §13 exposure → 1, 10, 11, 12, 20; §14 protocol → 7; §15 status → 20; §16 codes → 4, 8, 9, 15, 16, 19, 20; §17 testing → every task, 14 (harness), 21, 22; §18 docs → 20; §19 ADR and amendments → 1.
- Type names across tasks: `GroupShape`/`Topology` (config, Task 4) are distinct from `GroupTopology` (domain, Task 6); Task 16 converts one to the other. `GroupEngine` (6) is used by 7, 9, 13. `GroupMemberArgs` and `member_args` (10) are used by 11, 12, 13. `member_owner_id`, `GroupReservation`, `GroupSettlement`, `MemberGone` (8) are used by 16, 17, 19. `resolve_engine_env`, `ApprovedEnv`, `EnvRefusal` (2) are used by 3 and 16. `group_support`, `check_engine_shape` (4) are used by 16 and 19.
- Review Focus pins: 1 → Tasks 2, 16; 2 → 9; 3 → 17; 4 → 19; 5 → 8; 6 → 13.
- CPU and Fake-engine tests are not qualification; MN1–MN9 are.

# Discrete GPU and Network Endpoint Implementation Plan

**Execution:** implement task by task in order; each task ends green on its own tests and is committed before the next starts. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Account each discrete NVIDIA GPU as its own device-memory domain (standalone and remote hosts, one GPU per model, mllm picks the GPU), with deep and host-RAM parking and switching that work on a 16–32 GB card, and serve the inference endpoint on `0.0.0.0:8443` by default, migrating existing loopback listeners, with an API key required unless explicitly opted out.

**Architecture:** A bounded `nvidia-smi` collector in `mllm-agent` classifies the host (`NoGpu`, `Unified`, `Discrete`) and observes each discrete GPU. The host policy gains a `device` domain kind per GPU; derived deployment budgets charge the device domain for weights and KV and the system domain for engine host overhead and, for the `host_backed` tier, the pinned weights copy. The existing per-domain admission and switch planner then see VRAM; placement additionally chooses a GPU per instance, and the launch check reads every domain. vLLM sleep level 1 and SGLang's weights CPU backup implement the `host_backed` park. Remote hosts report device domains under a new ADR 0017 capability. The inference listener reads its bind from the role document (default `0.0.0.0:8443`), a one-time migration moves the old loopback default, `--listen` overrides it per run, and `authentication: none` is an explicit opt-out with a loud warning on a non-loopback bind.

**Tech Stack:** Rust 2021 workspace (tokio, tonic/prost, axum, rusqlite, clap, serde/serde_json, saphyr strict YAML), Python 3 runtime helpers under `runtime/` (unittest), bash live harness under `scripts/live/matrix/`.

**Spec:** `docs/specs/2026-09-25-discrete-gpu-and-network-endpoint-design.md`. Read it with this plan. Governing documents: `docs/SPEC.md` §6.2, §7, §13.3, §15, §16, §20; ADR 0007, 0010, 0012, 0013, 0014, 0017; `AGENTS.md`.

## Owner decisions (owner-decided on PR #38, 2026-09-25)

1. Existing loopback inference listeners migrate to `0.0.0.0:8443` on upgrade (server and standalone), once, with a notice; the key stays required. Tasks 14 and 15.
2. The host-RAM park tier (`host_backed`) is in 0.1.0 and is the default on a discrete-GPU host; `deep` stays. Tasks 6, 8, 11 and 12.
3. mllm picks the GPU on a multi-GPU host; `devices: [{id: gpuN}]` pins it. Task 7.
4. `AGENTS.md` authorizes live work on the maintainers' local machines, a discrete-GPU laptop included, with no driver, CUDA or system-package changes and engine virtual environments only in the home directory. Task 19.
5. The server's generated inference listener is `0.0.0.0:8443` too. Task 14.

## Global Constraints

- Cite the governing requirement inline where behaviour is spec-driven, e.g. `// SPEC §7.2: ...`, `// ADR 0019: ...` (AGENTS.md "Code conventions").
- Tag every new test with its acceptance-matrix ID (`// T26`). IDs used: T02, T03 (configuration), T16, T20, T23, T26, T27 (accounting, parking and switching), T21, T37 (security), T29 (unknown pressure), T34 (capability gating).
- Uncertainty keeps accounting: an unobserved device closes admission on its domain and releases nothing.
- No new crate dependency. GPU memory is read with `nvidia-smi` only (`/usr/bin/nvidia-smi`, then `/bin/nvidia-smi`), cleared environment, 3 s bound, 64 KiB output cap.
- Unified hosts are unchanged: the published standalone document, stored policies and their digests stay byte-identical on a unified or no-GPU host, and `host_backed` stays refused there.
- Engines stay loopback-only with per-launch keys and the key-guard middleware (ADR 0012). The management listener stays loopback-only.
- Additive protocol only: no field renumbered, `PROTOCOL_VERSION` stays `"2"`, command encoding version stays `"1"`. New capability: `device_memory_domains` (device observations, split residents, chosen device).
- Closed codes (spec §11): `insufficient_device_memory`, `device_unobserved`, `device_policy_mismatch`, `unsupported_gpu_topology`, `missing_system_allocation`, `multi_gpu_unsupported`, `host_backed_unavailable`, `config_migration_failed` (warning only). Exits: 4 for the first two, 5 for `multi_gpu_unsupported`, `unsupported_gpu_topology`, `host_backed_unavailable`, 2 for `device_policy_mismatch` and `missing_system_allocation`. No new exit number.
- Placeholders (spec §3): `ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES = 4 GiB`, `PARKED_DEVICE_RESIDUE_PLACEHOLDER_BYTES = 1 GiB`. Device reserve `max(1 GiB, 8 % of total)`; device parked limit `min(2 GiB × max_parked, 25 % of total)`. vLLM on a discrete device: `--gpu-memory-utilization` = request ÷ device total, rounded up to 0.01, at least 0.75.
- Default inference bind `0.0.0.0:8443`; default management bind `127.0.0.1:7443` (unchanged). Migration marker `<state_dir>/migrations/inference-bind-v1`; backup `<document>.pre-0.1.0`.
- In tracked files, commits and the PR: no machine names, addresses, vendor product names of the maintainers' machines, or home paths. The live box is "the 16 GB discrete-GPU laptop host"; its venv paths come only from the untracked `hosts.local.env`.
- CPU and Fake-engine tests are not qualification; the live rows DG1–DG7 are. The multi-GPU picker has CPU/Fake tests only in this plan. Say so in every status claim.
- Verification before every commit: `cargo fmt --all --check`; the core suite `cargo test -p mllm-adapters -p mllm-store -p mllm-controller -p mllm-management -p harness --all-targets --no-fail-fast --locked -- --test-threads=4`; `cargo test --workspace --all-targets --locked`; `cargo clippy --workspace --all-targets --locked -- -D warnings`; Python helpers: `python3 -m unittest discover -s runtime/tests`.

## Review Focus

1. **A laptop GPU already partly used by the desktop.** `memory.used` of 1–2 GiB at boot must lower availability, not the managed limit's honesty: the first deploy must still fit when it fits, and be refused with numbers when it does not. Pinned in Task 3 (policy from a fixture with 1.5 GiB used) and Task 9 (launch check with that availability).
2. **`nvidia-smi` hangs or disappears after boot** (driver reload, suspend and resume on a laptop). Admission must close on the device domain with `device_unobserved` and keep every reservation; nothing may be launched on a stale reading. Pinned in Task 1 (timeout) and Task 4 (unknown observation).
3. **The migration meets a hand-edited document** (comments, the address written twice, a read-only file). It must never corrupt the file, never loop on later starts, and still serve. Pinned in Task 15.
4. **Host RAM fills with parked copies.** A third `host_backed` park whose copy exceeds the system `parked_limit` must become a stop, not an overcommit and not a hang. Pinned in Task 11.
5. **Two parked engines' CUDA contexts plus a waking one** exceed a small card even though each alone fits. The device `parked_limit` and the parked residue must make the planner evict or stop rather than let the wake fail. Pinned in Task 9.

---

## File map

| File | Responsibility | Tasks |
|---|---|---|
| `crates/mllm-agent/src/gpu_memory.rs` (new) | bounded `nvidia-smi --query-gpu` collector, `HostShape` | 1 |
| `crates/mllm-config/src/effective.rs`, `effective/core.rs` | `DomainMemory::Device`, `device` field, host-policy rules | 2 |
| `crates/mllm-store/src/resource_policy.rs` | stored `device` field, identity unchanged | 2 |
| `crates/mllm-cli/src/standalone_config.rs`, `device_inventory.rs` | discrete standalone policy, UUID per device, template | 3, 8 |
| `crates/mllm-cli/src/host_observation.rs`, `crates/mllm-agent/src/native_execution.rs` (`inventory()`), `crates/mllm-cli/src/remote_roles.rs` | shape-aware observation | 4, 13 |
| `crates/mllm-domain/src/resources.rs`, `crates/mllm-agent/src/process_residency.rs`, `crates/mllm-store/src/resident_floors.rs` | per-domain resident credit | 5 |
| `crates/mllm-config/src/effective/engine_config.rs`, `effective/core.rs` | derived budgets for three tiers, resolution refusals | 6 |
| `crates/mllm-scheduler/src/device_choice.rs` (new), `placement.rs`, coordinator placement and launch plan | GPU picker | 7 |
| `crates/mllm-agent/src/native_execution/refusal.rs` | multi-domain launch check | 9 |
| `crates/mllm-adapters/src/vllm/args.rs`, `crates/mllm-adapters/src/sglang/args.rs`, `runtime/sglang_server_args.py` | engine sizing on a discrete device | 10 |
| `crates/mllm-scheduler/src/switching.rs`, `crates/mllm-adapters/src/vllm/residency.rs`, `crates/mllm-agent/src/native_execution.rs` | park-or-stop, vLLM level 1 | 11 |
| `crates/mllm-adapters/src/sglang/*`, `runtime/sglang_saver_*.py` | SGLang weights CPU backup park | 12 |
| `crates/mllm-protocol/*`, `crates/mllm-controller/src/agent_sessions.rs` | capability and remote reporting | 13 |
| `crates/mllm-config/src/standalone.rs`, `remote_roles.rs`, `defaults.rs`, `crates/mllm-cli/src/grammar.rs`, `main.rs`, `roles.rs` | bind default, `--listen` | 14 |
| `crates/mllm-config/src/listener_migration.rs` (new) | one-time loopback migration | 15 |
| `crates/mllm-cli/src/exposure.rs` (new), router wiring | auth opt-out, warning | 16 |
| `docs/design/adr/0019-discrete-gpu-and-network-endpoint.md` (new), `docs/SPEC.md` | ADR and amendments | 17 |
| `docs/examples/host-discrete.yaml` (new), `docs/operations/*` | operator docs, release notes draft | 18 |
| `scripts/live/matrix/discrete_gpu.sh` (new), `AGENTS.md`, `docs/runbooks/f2-current-status.md` | live rows DG1–DG7, live-work rule | 19 |

---

### Task 1: GPU memory collector and host shape

**Files:**
- Create: `crates/mllm-agent/src/gpu_memory.rs`
- Modify: `crates/mllm-agent/src/lib.rs` (add `pub mod gpu_memory;`)
- Test: in-module `#[cfg(test)] mod tests` of `gpu_memory.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces:
  - `pub struct GpuDevice { pub index: u32, pub uuid: String, pub pci_bus_id: String, pub name: String, pub memory: Option<GpuMemory> }` (`None` = integrated)
  - `pub struct GpuMemory { pub total_bytes: i64, pub used_bytes: i64, pub free_bytes: i64 }`
  - `pub struct GpuSample { pub devices: Vec<GpuDevice>, pub sampled_at_ms: i64 }`
  - `pub enum HostShape { NoGpu, Unified, Discrete(Vec<GpuDevice>) }`
  - `pub enum GpuShapeError { MixedTopology }` (code `unsupported_gpu_topology`)
  - `pub fn parse_query_gpu(text: &str, sampled_at_ms: i64) -> Option<GpuSample>`
  - `pub fn shape(sample: Option<&GpuSample>) -> Result<HostShape, GpuShapeError>`
  - `pub fn sample() -> Option<GpuSample>` (runs `nvidia-smi`, bounded)
  - `pub type GpuSampler = dyn Fn() -> Option<GpuSample> + Send + Sync;`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const MIB: i64 = 1 << 20;

    // T26: a discrete card reports its own memory in MiB.
    #[test]
    fn a_discrete_row_is_parsed_in_bytes() {
        let text = "0, GPU-11111111-2222-3333-4444-555555555555, 00000000:01:00.0, NVIDIA Discrete GPU, 16376, 1500, 14876\n";
        let sample = parse_query_gpu(text, 1_000).expect("valid row");
        let device = &sample.devices[0];
        assert_eq!(device.index, 0);
        assert_eq!(device.pci_bus_id, "00000000:01:00.0");
        let memory = device.memory.as_ref().expect("discrete");
        assert_eq!(memory.total_bytes, 16376 * MIB);
        assert_eq!(memory.used_bytes, 1500 * MIB);
        assert_eq!(memory.free_bytes, 14876 * MIB);
        assert_eq!(shape(Some(&sample)).unwrap(), HostShape::Discrete(sample.devices.clone()));
    }

    // T26: an integrated device (GB10) has no memory of its own.
    #[test]
    fn an_integrated_row_is_unified() {
        let text = "0, GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee, 0000000F:01:00.0, NVIDIA GB10, [N/A], [N/A], [N/A]\n";
        let sample = parse_query_gpu(text, 1).expect("valid row");
        assert!(sample.devices[0].memory.is_none());
        assert_eq!(shape(Some(&sample)).unwrap(), HostShape::Unified);
        let unsupported = text.replace("[N/A]", "[Not Supported]");
        assert!(parse_query_gpu(&unsupported, 1).unwrap().devices[0].memory.is_none());
    }

    #[test]
    fn no_sample_or_no_device_is_no_gpu() {
        assert_eq!(shape(None).unwrap(), HostShape::NoGpu);
        assert_eq!(shape(Some(&parse_query_gpu("", 1).unwrap())).unwrap(), HostShape::NoGpu);
    }

    // T26: mixed integrated and discrete devices are refused, never guessed.
    #[test]
    fn mixed_topology_is_refused() {
        let text = "0, GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee, 0000000F:01:00.0, NVIDIA GB10, [N/A], [N/A], [N/A]\n\
                    1, GPU-11111111-2222-3333-4444-555555555555, 00000000:01:00.0, RTX, 16376, 0, 16376\n";
        let sample = parse_query_gpu(text, 1).unwrap();
        assert_eq!(shape(Some(&sample)), Err(GpuShapeError::MixedTopology));
    }

    // T29: anything malformed invalidates the whole sample.
    #[test]
    fn malformed_samples_are_refused() {
        let good = "0, GPU-11111111-2222-3333-4444-555555555555, 00000000:01:00.0, RTX, 16376, 0, 16376\n";
        for bad in [
            "0, GPU-x, 00000000:01:00.0, RTX, 16376, 0\n".to_string(),          // 6 fields
            good.replace("16376, 0, 16376", "16376, 9000, 9000"),               // used+free > total
            good.replace("0, GPU", "x, GPU"),                                    // index
            good.replace("16376, 0", "-1, 0"),                                   // negative
            format!("{good}{good}"),                                             // duplicate
            "a".repeat(70_000),                                                  // oversized
        ] {
            assert!(parse_query_gpu(&bad, 1).is_none(), "refused: {bad:.60}");
        }
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p mllm-agent --lib gpu_memory`
Expected: FAIL to compile, `gpu_memory` not found.

- [ ] **Step 3: Write the implementation**

```rust
//! SPEC §7.2 / ADR 0019: the memory of each NVIDIA GPU on this host.
//!
//! A discrete GPU's VRAM is a physical domain distinct from host RAM; an
//! integrated device (GB10) reports `[N/A]` memory because it has none of its
//! own, which is how a unified host is recognised — never from a product name.
//! Sampling runs `nvidia-smi` from a fixed path with a cleared environment,
//! bounded in time and output, exactly like `process_residency`. Every parse
//! failure invalidates the whole sample: an unknown device closes admission
//! on its domain (SPEC §7.2), it is never filled in.

use std::collections::BTreeSet;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const BOUND: Duration = Duration::from_secs(3);
pub const MAX_OUTPUT: usize = 64 * 1024;
const MIB: i64 = 1 << 20;
/// Driver rounding between `used + free` and `total`.
const SLACK: i64 = 64 * MIB;
const QUERY: &str =
    "--query-gpu=index,uuid,pci.bus_id,name,memory.total,memory.used,memory.free";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuMemory {
    pub total_bytes: i64,
    pub used_bytes: i64,
    pub free_bytes: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuDevice {
    pub index: u32,
    pub uuid: String,
    pub pci_bus_id: String,
    pub name: String,
    /// `None` for an integrated device sharing host memory.
    pub memory: Option<GpuMemory>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuSample {
    pub devices: Vec<GpuDevice>,
    pub sampled_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostShape {
    NoGpu,
    Unified,
    Discrete(Vec<GpuDevice>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GpuShapeError {
    #[error("unsupported_gpu_topology: integrated and discrete GPUs on one host")]
    MixedTopology,
}

pub type GpuSampler = dyn Fn() -> Option<GpuSample> + Send + Sync;

fn mib(field: &str) -> Result<Option<i64>, ()> {
    match field {
        "[N/A]" | "[Not Supported]" => Ok(None),
        text => {
            let value: i64 = text.parse().map_err(|_| ())?;
            if value < 0 {
                return Err(());
            }
            value.checked_mul(MIB).map(Some).ok_or(())
        }
    }
}

pub fn parse_query_gpu(text: &str, sampled_at_ms: i64) -> Option<GpuSample> {
    if text.len() > MAX_OUTPUT || sampled_at_ms < 0 {
        return None;
    }
    let mut devices = Vec::new();
    let (mut indexes, mut uuids) = (BTreeSet::new(), BTreeSet::new());
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let fields: Vec<&str> = line.split(',').map(str::trim).collect();
        let [index, uuid, bus, name, total, used, free] = fields.as_slice() else {
            return None;
        };
        let index: u32 = index.parse().ok()?;
        if !uuid.starts_with("GPU-") || uuid.len() > 64 || bus.is_empty() || name.is_empty() {
            return None;
        }
        if !indexes.insert(index) || !uuids.insert(uuid.to_string()) {
            return None;
        }
        let memory = match (mib(total).ok()?, mib(used).ok()?, mib(free).ok()?) {
            (None, None, None) => None,
            (Some(total_bytes), Some(used_bytes), Some(free_bytes)) => {
                if total_bytes == 0 || used_bytes.checked_add(free_bytes)? > total_bytes + SLACK {
                    return None;
                }
                Some(GpuMemory { total_bytes, used_bytes, free_bytes })
            }
            _ => return None,
        };
        devices.push(GpuDevice {
            index,
            uuid: uuid.to_string(),
            pci_bus_id: bus.to_string(),
            name: name.to_string(),
            memory,
        });
    }
    devices.sort_by_key(|d| d.index);
    Some(GpuSample { devices, sampled_at_ms })
}

pub fn shape(sample: Option<&GpuSample>) -> Result<HostShape, GpuShapeError> {
    let Some(sample) = sample.filter(|s| !s.devices.is_empty()) else {
        return Ok(HostShape::NoGpu);
    };
    let integrated = sample.devices.iter().filter(|d| d.memory.is_none()).count();
    match integrated {
        0 => Ok(HostShape::Discrete(sample.devices.clone())),
        n if n == sample.devices.len() => Ok(HostShape::Unified),
        _ => Err(GpuShapeError::MixedTopology),
    }
}

/// The live sample; `None` on any failure (no binary, timeout, bad output).
pub fn sample() -> Option<GpuSample> {
    let program = ["/usr/bin/nvidia-smi", "/bin/nvidia-smi"]
        .into_iter()
        .find(|p| std::path::Path::new(p).is_file())?;
    let mut child = Command::new(program)
        .arg(QUERY)
        .arg("--format=csv,noheader,nounits")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + BOUND;
    let status = loop {
        match child.try_wait().ok()? {
            Some(status) => break status,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let mut text = String::new();
    child
        .stdout
        .take()?
        .take(MAX_OUTPUT as u64 + 1)
        .read_to_string(&mut text)
        .ok()?;
    if !status.success() {
        return None;
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_millis();
    parse_query_gpu(&text, i64::try_from(now).ok()?)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p mllm-agent --lib gpu_memory`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-agent/src/gpu_memory.rs crates/mllm-agent/src/lib.rs
git commit -m "feat: observe discrete GPU memory with a bounded nvidia-smi collector"
```

---

### Task 2: Device memory domain in the host policy

**Files:**
- Modify: `crates/mllm-config/src/effective.rs` (`DomainMemory`, `RawDomain`, `DomainPolicy`)
- Modify: `crates/mllm-config/src/effective/core.rs:259-269` (domain parsing and host-policy rules)
- Modify: `crates/mllm-store/src/resource_policy.rs` (`StoredDomain`, `domain_memory`, `parse_domain_memory`)
- Modify: `crates/mllm-config/src/error.rs` (codes `device_policy_mismatch`, `unsupported_gpu_topology` if `ConfigErrorCode` is closed)
- Test: `crates/mllm-config/tests/effective.rs`, `crates/mllm-store/src/resource_policy.rs` tests

**Interfaces:**
- Produces:
  - `DomainMemory::Device` (serialized `"device"`)
  - `DomainPolicy { ..., pub device: Option<String> }` — `Some(id)` exactly when `memory == Device`
  - host-policy rules, refused as `ConfigErrorCode::UnsupportedCombination` at the stated path with detail prefix `unsupported_gpu_topology:` or `device_policy_mismatch:`
- Consumes: nothing from Task 1 (config stays free of `mllm-agent`).

Rules (spec §2): a `device` domain names one device in `resource_policy.devices`, that device's `domain` is this domain, no other device maps to it, and it has no `host_kv_limit`. At most one `unified` domain; a policy with a `unified` domain has no `device` domain. `memory: unified | distinct` domains must not carry `device`.

- [ ] **Step 1: Write the failing tests** (in `crates/mllm-config/tests/effective.rs`, using its existing `host()` fixture helper that returns the example unified host as `serde_json::Value`)

```rust
fn discrete_host() -> serde_json::Value {
    let mut h = host();
    h["resource_policy"]["domains"] = serde_json::json!({
        "system": {"memory": "distinct", "managed_limit": "24GiB", "free_reserve": "8GiB",
                   "parked_limit": "12GiB", "host_kv_limit": "4GiB"},
        "gpu0": {"memory": "device", "device": "gpu0", "managed_limit": "14848MiB",
                 "free_reserve": "1536MiB", "parked_limit": "2GiB"}
    });
    h["resource_policy"]["devices"] = serde_json::json!({"gpu0": {"domain": "gpu0", "sharing": "shared"}});
    h
}

// T26: a discrete host declares a system domain and a device domain.
#[test]
fn a_discrete_host_policy_resolves() {
    let policy = resolve_host(&discrete_host()).expect("valid");
    let gpu = &policy.domains["gpu0"];
    assert_eq!(gpu.memory, DomainMemory::Device);
    assert_eq!(gpu.device.as_deref(), Some("gpu0"));
    assert_eq!(policy.domains["system"].device, None);
}

// T26: every broken shape is refused with its path.
#[test]
fn broken_device_domains_are_refused() {
    let cases: Vec<(&str, Box<dyn Fn(&mut serde_json::Value)>)> = vec![
        ("device domain without device", Box::new(|h| { h["resource_policy"]["domains"]["gpu0"].as_object_mut().unwrap().remove("device"); })),
        ("unknown device", Box::new(|h| h["resource_policy"]["domains"]["gpu0"]["device"] = "gpu9".into())),
        ("device maps elsewhere", Box::new(|h| h["resource_policy"]["devices"]["gpu0"]["domain"] = "system".into())),
        ("host kv on device", Box::new(|h| h["resource_policy"]["domains"]["gpu0"]["host_kv_limit"] = "1GiB".into())),
        ("device on system domain", Box::new(|h| h["resource_policy"]["domains"]["system"]["device"] = "gpu0".into())),
        ("unified mixed with device", Box::new(|h| h["resource_policy"]["domains"]["system"]["memory"] = "unified".into())),
    ];
    for (name, mutate) in cases {
        let mut h = discrete_host();
        mutate(&mut h);
        let error = resolve_host(&h).expect_err(name);
        assert!(error.to_string().contains("resource_policy"), "{name}: {error}");
    }
}

// T26: two GPUs, each its own device domain (owner decision 3).
#[test]
fn two_device_domains_resolve() {
    let mut h = discrete_host();
    h["resource_policy"]["domains"]["gpu1"] = serde_json::json!({"memory": "device", "device": "gpu1",
        "managed_limit": "22GiB", "free_reserve": "2GiB", "parked_limit": "2GiB"});
    h["resource_policy"]["devices"]["gpu1"] = serde_json::json!({"domain": "gpu1", "sharing": "shared"});
    assert_eq!(resolve_host(&h).unwrap().domains.len(), 3);
}
```

In `resource_policy.rs` tests:

```rust
// T26: an existing unified stored policy keeps its bytes (no `device` key).
#[test]
fn a_unified_stored_domain_serializes_as_before() {
    let stored = StoredDomain { managed_limit: 1, free_reserve: 1, host_kv_limit: None,
        parked_limit: None, memory: "unified".into(), device: None };
    assert_eq!(serde_json::to_string(&stored).unwrap(),
        r#"{"managed_limit":1,"free_reserve":1,"host_kv_limit":null,"parked_limit":null,"memory":"unified"}"#);
    let device = StoredDomain { memory: "device".into(), device: Some("gpu0".into()), ..stored };
    assert!(serde_json::to_string(&device).unwrap().ends_with(r#""memory":"device","device":"gpu0"}"#));
}
```

(`resolve_host` is the existing test helper in `tests/effective.rs` that parses a host document into `HostPolicy`; if it is named differently there, use that name — do not add a second helper.)

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p mllm-config --test effective device && cargo test -p mllm-store --lib resource_policy::tests::a_unified_stored`
Expected: FAIL (`Device` variant and `device` field missing).

- [ ] **Step 3: Implement**

In `effective.rs`:

```rust
pub enum DomainMemory {
    Unified,
    Distinct,
    /// ADR 0019: a discrete GPU's own memory; the domain names its device.
    Device,
}
// RawDomain gains:
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device: Option<String>,
// DomainPolicy gains:
    pub device: Option<String>,
```

In `core.rs`, after the domain loop builds `domains` (and after `devices` are parsed), call a new function:

```rust
/// ADR 0019, SPEC §7.2: a discrete GPU's memory is its own domain.
fn check_domain_shape(
    domains: &BTreeMap<String, DomainPolicy>,
    devices: &BTreeMap<String, DevicePolicy>,
) -> Result<(), ConfigError> {
    let path = "resource_policy.domains";
    let unified = domains.values().filter(|d| d.memory == DomainMemory::Unified).count();
    let device = domains.values().filter(|d| d.memory == DomainMemory::Device).count();
    if unified > 1 || (unified == 1 && device > 0) {
        return Err(invalid(path, "unsupported_gpu_topology: a unified domain cannot be combined with another unified or a device domain"));
    }
    for (name, domain) in domains {
        let here = format!("{path}.{name}");
        match (domain.memory, domain.device.as_deref()) {
            (DomainMemory::Device, Some(id)) => {
                if domain.host_kv_limit.is_some() {
                    return Err(invalid(&here, "device_policy_mismatch: host_kv_limit belongs to the system domain"));
                }
                if devices.get(id).map(|d| d.domain.as_str()) != Some(name.as_str()) {
                    return Err(invalid(&here, "device_policy_mismatch: the named device must map to this domain"));
                }
                if devices.iter().any(|(other, d)| other != id && d.domain == *name) {
                    return Err(invalid(&here, "device_policy_mismatch: only its own device may map to a device domain"));
                }
            }
            (DomainMemory::Device, None) => {
                return Err(invalid(&here, "device_policy_mismatch: a device domain names its device"))
            }
            (_, Some(_)) => {
                return Err(invalid(&here, "device_policy_mismatch: only a device domain names a device"))
            }
            (_, None) => {}
        }
    }
    Ok(())
}
```

(`DevicePolicy` is the existing resolved type of `host.devices` values; `invalid` is the module's existing helper. Set `device: raw.device` in the domain loop.)

In `resource_policy.rs`: add `#[serde(default, skip_serializing_if = "Option::is_none")] device: Option<String>` to `StoredDomain`; map `DomainMemory::Device => "device"` and `"device" => Ok(DomainMemory::Device)`; copy `device` in both directions in `StoredControls::from_public` / `to_public`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p mllm-config -p mllm-store --all-targets --locked`
Expected: PASS, including every existing unified test unchanged.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-config crates/mllm-store/src/resource_policy.rs
git commit -m "feat: add a device memory domain to the host resource policy"
```

---

### Task 3: Standalone publishes a discrete host policy

**Files:**
- Modify: `crates/mllm-cli/src/standalone_config.rs` (`host_policy`)
- Modify: `crates/mllm-cli/src/device_inventory.rs` (`InventoryPublication`: UUID per device)
- Modify: `crates/mllm-cli/src/roles.rs` (boot: sample GPUs once, pass the shape; refuse `MixedTopology`)
- Test: `crates/mllm-cli/src/standalone_config/tests.rs`, `crates/mllm-cli/src/device_inventory.rs` tests

**Interfaces:**
- Consumes: `mllm_agent::gpu_memory::{HostShape, GpuDevice, GpuMemory}` (Task 1); `DomainMemory::Device` (Task 2).
- Produces:
  - `pub fn host_policy(installations, environment_fingerprint, capacity_bytes, inventory, shape: &HostShape) -> Value`
  - `pub fn device_limits(memory: &GpuMemory, max_parked: i64) -> DeviceLimits` with `pub struct DeviceLimits { pub managed_limit: i64, pub free_reserve: i64, pub parked_limit: i64 }`
  - `InventoryPublication.physical_gpu_uuids: BTreeMap<u32, String>` (index → UUID) replacing `physical_gpu_uuid: Option<String>`; the single-device case keeps publishing `gpu0.physical_gpu_uuid` identically.
  - `StartError::GpuTopology` (message starts `unsupported_gpu_topology`)

Device ids are `gpu{index}`, domains are named after their device.

- [ ] **Step 1: Write the failing tests**

```rust
use mllm_agent::gpu_memory::{GpuDevice, GpuMemory, HostShape};
const GIB: i64 = 1 << 30;
const MIB: i64 = 1 << 20;

fn rtx(index: u32, total_mib: i64, used_mib: i64) -> GpuDevice {
    GpuDevice { index, uuid: format!("GPU-{index:08}-2222-3333-4444-555555555555"),
        pci_bus_id: format!("00000000:0{index}:00.0"), name: "RTX".into(),
        memory: Some(GpuMemory { total_bytes: total_mib * MIB, used_bytes: used_mib * MIB,
                                 free_bytes: (total_mib - used_mib) * MIB }) }
}

// T26: a 16 GB card with 1.5 GiB of desktop use; 61 GiB of RAM.
#[test]
fn a_discrete_standalone_host_has_system_and_device_domains() {
    let shape = HostShape::Discrete(vec![rtx(0, 16376, 1536)]);
    let doc = host_policy(&installations(), "env", 61 * GIB, None, &shape);
    let domains = &doc["resource_policy"]["domains"];
    assert!(domains.get("unified").is_none());
    assert_eq!(domains["system"]["memory"], "distinct");
    assert_eq!(domains["gpu0"]["memory"], "device");
    assert_eq!(domains["gpu0"]["device"], "gpu0");
    let reserve = (16376 * MIB / 100 * 8).max(GIB);
    assert_eq!(domains["gpu0"]["free_reserve"], format!("{reserve}B"));
    assert_eq!(domains["gpu0"]["managed_limit"], format!("{}B", 16376 * MIB - reserve));
    assert!(domains["gpu0"].get("host_kv_limit").is_none());
    assert_eq!(doc["resource_policy"]["devices"]["gpu0"]["domain"], "gpu0");
}

// T26: the unified document is byte-identical to before.
#[test]
fn a_unified_standalone_host_is_unchanged() {
    let before = include_str!("fixtures/unified_host_policy.json"); // captured from main before this task
    let doc = host_policy(&installations(), "env", 128 * GIB, None, &HostShape::Unified);
    assert_eq!(serde_json::to_string_pretty(&doc).unwrap(), before.trim_end());
    let no_gpu = host_policy(&installations(), "env", 128 * GIB, None, &HostShape::NoGpu);
    assert_eq!(no_gpu, doc);
}

// T26 (owner decision 3): two GPUs publish two devices and two device domains.
#[test]
fn two_gpus_publish_two_device_domains() {
    let shape = HostShape::Discrete(vec![rtx(0, 24576, 0), rtx(1, 32768, 0)]);
    let doc = host_policy(&installations(), "env", 64 * GIB, None, &shape);
    assert_eq!(doc["resource_policy"]["devices"]["gpu1"]["domain"], "gpu1");
    assert_eq!(doc["resource_policy"]["domains"]["gpu1"]["device"], "gpu1");
}

#[test]
fn device_limits_follow_the_spec_table() {
    let limits = device_limits(&rtx(0, 16376, 0).memory.unwrap(), 4);
    assert_eq!(limits.free_reserve, GIB.max(16376 * MIB / 100 * 8));
    assert_eq!(limits.parked_limit, (2 * GIB * 4).min(16376 * MIB / 100 * 25));
}
```

Create the fixture first: run `host_policy` on `main` with the unified inputs and save the output as `crates/mllm-cli/src/standalone_config/fixtures/unified_host_policy.json` (commit it with this task). In `device_inventory.rs` tests, add a case with two devices asserting both UUIDs are published and the digest is unchanged.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p mllm-cli --lib standalone_config device_inventory`
Expected: FAIL (`host_policy` takes four arguments; `device_limits` missing).

- [ ] **Step 3: Implement**

```rust
/// ADR 0019: the device reserve absorbs a display server's use.
pub struct DeviceLimits { pub managed_limit: i64, pub free_reserve: i64, pub parked_limit: i64 }

pub fn device_limits(memory: &GpuMemory, max_parked: i64) -> DeviceLimits {
    const GIB: i64 = 1 << 30;
    let total = memory.total_bytes;
    let free_reserve = (total / 100 * 8).max(GIB);
    DeviceLimits {
        managed_limit: total - free_reserve,
        free_reserve,
        parked_limit: (2 * GIB * max_parked).min(total / 100 * 25),
    }
}
```

In `host_policy`, keep the current body for `HostShape::Unified | HostShape::NoGpu`. For `HostShape::Discrete(devices)` build:

```rust
let mut domains = serde_json::Map::new();
domains.insert("system".into(), json!({
    "managed_limit": share(MANAGED_FRACTION), "free_reserve": share(FREE_RESERVE_FRACTION),
    "parked_limit": share(PARKED_FRACTION), "host_kv_limit": share(HOST_KV_FRACTION),
    // ADR 0019: host RAM only; the GPU has its own domain.
    "memory": "distinct"
}));
let mut device_table = serde_json::Map::new();
for device in devices {
    let id = format!("gpu{}", device.index);
    let memory = device.memory.as_ref().expect("discrete shape has memory");
    let limits = device_limits(memory, MAX_PARKED);
    domains.insert(id.clone(), json!({
        "memory": "device", "device": id,
        "managed_limit": format!("{}B", limits.managed_limit),
        "free_reserve": format!("{}B", limits.free_reserve),
        "parked_limit": format!("{}B", limits.parked_limit)
    }));
    let mut entry = json!({"domain": id, "sharing": "shared"});
    if let Some(uuid) = inventory.and_then(|p| p.physical_gpu_uuids.get(&device.index)) {
        entry["physical_gpu_uuid"] = json!(uuid);
    }
    device_table.insert(id, entry);
}
```

and place them under `resource_policy.domains` / `resource_policy.devices`. Introduce `const MAX_PARKED: i64 = 4;` and use it for the existing `"max_parked": 4`. In `device_inventory.rs`, parse every device's UUID into `physical_gpu_uuids` keyed by the collector's index; the UUID from `runtime/sglang_device.py` must equal the `nvidia-smi` UUID for the same PCI bus id, else publish no UUIDs (fail closed). In `roles.rs` boot, call `mllm_agent::gpu_memory::sample()` once, compute `shape(...)`, map `MixedTopology` to `StartError::GpuTopology`, and pass the shape to `host_policy`. Keep the shape on `App` (`pub gpu_shape: HostShape`) for Tasks 4 and 8.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p mllm-cli --all-targets --locked`
Expected: PASS; existing standalone tests unchanged.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-cli
git commit -m "feat: publish a system and a device memory domain on a discrete standalone host"
```

---

### Task 4: Shape-aware memory observation

**Files:**
- Modify: `crates/mllm-cli/src/host_observation.rs` (`HostMemoryObservation`)
- Modify: `crates/mllm-agent/src/native_execution.rs` (`inventory()` refresh near line 1470)
- Test: `crates/mllm-cli/src/host_observation.rs` tests; `crates/mllm-agent/tests/` inventory test beside the existing one

**Interfaces:**
- Consumes: `GpuSampler`, `GpuSample` (Task 1); `DomainPolicy.device` (Task 2).
- Produces:
  - `HostMemoryObservation::with_domains(domains: Vec<ObservedDomain>) -> Self` where `pub enum ObservedDomain { Host(String), Device { domain: String, index: u32 } }`
  - `HostMemoryObservation::with_gpu_sampler(self, sampler: Arc<GpuSampler>) -> Self`
  - `pub fn observe_domains(domains: &[ObservedDomain], host: &HostMemorySample, gpu: Option<&GpuSample>) -> Vec<MemoryObservation>` — a device absent from `gpu` yields **no** observation for its domain (the coordinator treats a missing observation as unknown and closes admission there, SPEC §7.2).
  - The existing `HostMemoryObservation::new(names)` keeps working and maps every name to `ObservedDomain::Host` (unified hosts unchanged).

- [ ] **Step 1: Write the failing tests**

```rust
// T26: host RAM for the system domain, VRAM for the device domain.
#[test]
fn a_device_domain_is_observed_from_the_gpu() {
    let host = HostMemorySample { memory: MemoryObservation { domain: "x".into(),
        capacity_bytes: 61 << 30, available_bytes: 50 << 30, sampled_at_ms: 10 }, swap_used_bytes: 0 };
    let gpu = mllm_agent::gpu_memory::parse_query_gpu(
        "0, GPU-11111111-2222-3333-4444-555555555555, 00000000:01:00.0, RTX, 16376, 1536, 14840\n", 11).unwrap();
    let domains = [ObservedDomain::Host("system".into()),
                   ObservedDomain::Device { domain: "gpu0".into(), index: 0 }];
    let observed = observe_domains(&domains, &host, Some(&gpu));
    assert_eq!(observed[0].capacity_bytes, 61 << 30);
    assert_eq!(observed[1].domain, "gpu0");
    assert_eq!(observed[1].capacity_bytes, 16376 << 20);
    assert_eq!(observed[1].available_bytes, 14840 << 20);
    assert_eq!(observed[1].sampled_at_ms, 11);
}

// T29: a GPU sample that failed reports nothing for the device domain.
#[test]
fn an_unobserved_device_has_no_observation() {
    let host = HostMemorySample { memory: MemoryObservation { domain: "x".into(),
        capacity_bytes: 1, available_bytes: 1, sampled_at_ms: 1 }, swap_used_bytes: 0 };
    let domains = [ObservedDomain::Host("system".into()),
                   ObservedDomain::Device { domain: "gpu0".into(), index: 0 }];
    let observed = observe_domains(&domains, &host, None);
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].domain, "system");
}
```

Add a coordinator-level test in `crates/mllm-controller/tests/` (next to the existing admission-with-observation tests) that imports a discrete policy, feeds only the system observation, and asserts a start is blocked with `device_unobserved` while existing reservations stay charged (T29). If the coordinator today reports a missing observation as `stale_observation`, map it to `device_unobserved` only when the missing domain's `memory` is `Device`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p mllm-cli --lib host_observation && cargo test -p mllm-controller --test <file> device_unobserved`
Expected: FAIL (types missing).

- [ ] **Step 3: Implement**

```rust
pub enum ObservedDomain { Host(String), Device { domain: String, index: u32 } }

pub fn observe_domains(domains: &[ObservedDomain], host: &HostMemorySample,
                       gpu: Option<&GpuSample>) -> Vec<MemoryObservation> {
    domains.iter().filter_map(|d| match d {
        ObservedDomain::Host(domain) => Some(MemoryObservation { domain: domain.clone(), ..host.memory.clone() }),
        // SPEC §7.2: VRAM is read from the device, never substituted with RAM.
        ObservedDomain::Device { domain, index } => {
            let sample = gpu?;
            let memory = sample.devices.iter().find(|g| g.index == *index)?.memory.as_ref()?;
            Some(MemoryObservation { domain: domain.clone(), capacity_bytes: memory.total_bytes,
                available_bytes: memory.free_bytes, sampled_at_ms: sample.sampled_at_ms })
        }
    }).collect()
}
```

`observe()` reads `/proc/meminfo` as today, runs the GPU sampler (off the async thread via `spawn_blocking` because it spawns a process) only when some domain is a `Device`, and returns `observe_domains(..)`. The standalone boot in `roles.rs` builds the domain list from the published policy: `memory: device` → `Device { index }` from its `device` id `gpuN`, everything else `Host`. In the agent's `inventory()`, replace the "exactly one domain" early return: refresh each `system`/`unified` domain from meminfo and each device domain from the GPU sample; a device missing from the sample is published with `capacity_bytes = available_bytes = -1` (the existing unknown encoding).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p mllm-cli -p mllm-agent -p mllm-controller --all-targets --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-cli crates/mllm-agent crates/mllm-controller
git commit -m "feat: observe device memory domains from the GPU and close admission when unobserved"
```

---

### Task 5: Resident credit per domain

**Files:**
- Modify: `crates/mllm-domain/src/resources.rs` (`ProcessResident`)
- Modify: `crates/mllm-agent/src/process_residency.rs` (`sample_now`)
- Modify: `crates/mllm-store/src/resident_floors.rs` (`resident_floors`)
- Modify: `crates/mllm-controller/src/agent_sessions.rs:119` (proto mapping; new fields default 0 when absent)
- Modify: `crates/mllm-protocol/proto/*.proto` resident message (field numbers assigned in Task 13; this task only uses the in-process type — remote hosts send the new fields after Task 13)
- Test: `crates/mllm-store/src/resident_floors.rs` tests, `process_residency.rs` tests

**Interfaces:**
- Produces: `ProcessResident { pid, boot_id, start_ticks, bytes, pub device_bytes: i64, pub host_bytes: i64 }` where `bytes = device_bytes + host_bytes` (kept for unified credit and old peers).
- `resident_floors` credits, per owner, one floor per allocation: a `device` domain gets `min(device_bytes sum, allocation.bytes)`, a `system` domain gets `min(host_bytes sum, allocation.bytes)`, a `unified` domain gets `min(bytes sum, allocation.bytes)` (unchanged). It needs the domain kinds: add a parameter `kinds: &BTreeMap<String, DomainMemory>`.

- [ ] **Step 1: Write the failing test**

```rust
// T26 / ADR 0007 (M33 regression on discrete hosts): a READY engine with two
// allocations is credited on both domains, not skipped.
#[test]
fn a_two_domain_owner_is_credited_per_domain() {
    let (conn, scoped) = ready_owner_fixture(vec![
        allocation("gpu0", 10 << 30), allocation("system", 4 << 30)]); // existing fixture builder, extended to take allocations
    let observations = vec![observation("gpu0"), observation("system")];
    let residents = vec![ProcessResident { pid: 7, boot_id: "b".into(), start_ticks: 1,
        bytes: (9 << 30) + (3 << 30), device_bytes: 9 << 30, host_bytes: 3 << 30 }];
    let kinds = BTreeMap::from([("gpu0".to_string(), DomainMemory::Device),
                                ("system".to_string(), DomainMemory::Distinct)]);
    let floors = resident_floors(&conn, &scoped, "candidate", &observations, &residents, &kinds).unwrap();
    let by_domain: BTreeMap<_, _> = floors.iter().map(|f| (f.domain.as_str(), f.bytes)).collect();
    assert_eq!(by_domain["gpu0"], 9 << 30);
    assert_eq!(by_domain["system"], 3 << 30);
}
```

Keep the existing single-allocation unified test green unchanged.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p mllm-store --lib resident_floors`
Expected: FAIL (fields and parameter missing).

- [ ] **Step 3: Implement**

In `resident_floors`, replace `let [allocation] = footprint.allocations.as_slice() else { continue; };` with a loop over `footprint.allocations`, and select the sampled figure by kind:

```rust
let pick = |p: &ProcessResident| match kinds.get(&allocation.domain) {
    Some(DomainMemory::Device) => p.device_bytes,
    Some(DomainMemory::Distinct) => p.host_bytes,
    _ => p.bytes, // unified: one pool (ADR 0007)
};
```

`sampled` becomes a map from identity to `&ProcessResident`. In `process_residency::sample_now`, set `device_bytes = gpu_bytes`, `host_bytes = anonymous`, `bytes = sum`. Callers of `resident_floors` pass the kinds from the current host policy (`ResourceControls.domains`).

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p mllm-store -p mllm-agent -p mllm-controller --all-targets --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-domain crates/mllm-agent crates/mllm-store crates/mllm-controller
git commit -m "fix: credit resident GPU and host memory to their own domains"
```

---

### Task 6: Derived budgets for the three tiers, and resolution refusals

**Files:**
- Modify: `crates/mllm-config/src/effective/engine_config.rs` (`derive_resources`, constants)
- Modify: `crates/mllm-config/src/effective/core.rs:400-425` (explicit-resource checks, residency checks)
- Test: `crates/mllm-config/tests/effective.rs`

**Interfaces:**
- Produces:
  - `pub const ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES: i64 = 4 << 30;`
  - `pub const PARKED_DEVICE_RESIDUE_PLACEHOLDER_BYTES: i64 = 1 << 30;`
  - `pub fn derive_resources(request, startup, residency, devices, host, weights_bytes: Option<i64>, engine: Engine) -> Result<RecipeFootprints, ConfigError>` — the two new parameters feed the `host_backed` copy. When the selected device's domain is `Device`, every phase has two allocations `[device, system]` (spec §3). `host_backed`: the system allocation of the parked phase adds `weights_bytes` with `Category`-equivalent parked-residue tagging (the system allocation's `host_kv_bytes` stays 0; the parked phase is what `fits` counts against `parked_limit`); for `Engine::Sglang` the copy is added to every phase's system allocation (the CPU backup lives for the engine's life).
  - Refusals (all `ConfigErrorCode::UnsupportedCombination`, detail prefixed by the code): `missing_system_allocation` (explicit `resources:` naming a device domain but not the system domain in a non-zero phase, or a discrete host without exactly one `distinct` domain), `multi_gpu_unsupported` (more than one device claim, or `topology.tensor_parallel > 1`, on a host with a device domain), `host_backed_unavailable` (`residency: host_backed` on a unified domain — the existing ADR 0010 refusal with its code renamed — or with `weights_bytes` unknown).
- Consumes: `DomainPolicy.device`, `DomainMemory::Device` (Task 2).

Measured replacements: the ADR 0014 measured-peak store already replaces the cold figure; the placeholders stay until a follow-up records host overhead and parked residue (note it in the status runbook, Task 19).

- [ ] **Step 1: Write the failing tests**

```rust
const GIB: i64 = 1 << 30;

fn phase(p: &PhaseFootprint) -> Vec<(String, i64)> {
    p.allocations.iter().map(|a| (a.domain.clone(), a.bytes)).collect()
}

// T26/T23: deep on a discrete host: device and system per phase.
#[test]
fn deep_budgets_charge_device_and_system() {
    let d = deployment_with("deep", "vllm", "10GiB"); // fixture: devices [gpu0], weights 8 GiB
    let r = resolve(&d, &discrete_host()).unwrap().resources;
    assert_eq!(phase(&r.ready), vec![("gpu0".into(), 10 * GIB), ("system".into(), ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES)]);
    assert_eq!(phase(&r.parked), vec![("gpu0".into(), PARKED_DEVICE_RESIDUE_PLACEHOLDER_BYTES), ("system".into(), ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES)]);
}

// T26/T23: vLLM host_backed charges the weights copy only while parked.
#[test]
fn vllm_host_backed_charges_the_copy_when_parked() {
    let r = resolve(&deployment_with("host_backed", "vllm", "10GiB"), &discrete_host()).unwrap().resources;
    assert_eq!(phase(&r.ready)[1].1, ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES);
    assert_eq!(phase(&r.parked)[1].1, ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES + 8 * GIB);
    assert_eq!(phase(&r.parked)[0].1, PARKED_DEVICE_RESIDUE_PLACEHOLDER_BYTES);
}

// T26: SGLang's CPU backup lives for the engine's life.
#[test]
fn sglang_host_backed_charges_the_copy_in_every_phase() {
    let r = resolve(&deployment_with("host_backed", "sglang", "10GiB"), &discrete_host()).unwrap().resources;
    for p in [&r.cold, &r.ready, &r.parking, &r.parked, &r.wake] {
        assert_eq!(phase(p)[1].1, ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES + 8 * GIB);
    }
}

// T26: restart_only parks nothing; unified unchanged.
#[test]
fn restart_only_and_unified_are_unchanged() {
    let r = resolve(&deployment_with("restart_only", "vllm", "10GiB"), &discrete_host()).unwrap().resources;
    assert!(r.parked.allocations.iter().all(|a| a.bytes == 0));
    let u = resolve(&deployment_with("deep", "vllm", "10GiB"), &host()).unwrap().resources;
    assert_eq!(u.ready.allocations.len(), 1);
    assert_eq!(u.parked.allocations[0].bytes, PARKED_RESIDUAL_PLACEHOLDER_BYTES);
}

#[test]
fn discrete_refusals_are_typed() {
    let err = |d: &serde_json::Value, h: &serde_json::Value| resolve(d, h).unwrap_err().to_string();
    assert!(err(&deployment_with_resources("gpu0"), &discrete_host()).contains("missing_system_allocation"));
    let mut two = deployment_with("deep", "vllm", "10GiB");
    two["devices"] = serde_json::json!([{"id": "gpu0", "sharing": "shared"}, {"id": "gpu1", "sharing": "shared"}]);
    assert!(err(&two, &two_gpu_host()).contains("multi_gpu_unsupported"));
    assert!(err(&deployment_with("host_backed", "vllm", "10GiB"), &host()).contains("host_backed_unavailable"));
}
```

`deployment_with(residency, engine, request)` and `deployment_with_resources(domain)` are fixture builders added at the top of the test file from the existing `deployment()` fixture; `discrete_host()` and `two_gpu_host()` come from Task 2. Update the two existing host-backed tests: `a_host_backed_park_is_refused_on_a_unified_domain` now expects the code `host_backed_unavailable`; `a_host_backed_park_resolves_on_a_distinct_domain` switches its host to `discrete_host()`.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mllm-config --test effective`
Expected: FAIL on the new tests.

- [ ] **Step 3: Implement**

In `derive_resources`, after the domain is found:

```rust
let policy = &host.domains[&domain];
if policy.memory == DomainMemory::Device {
    // ADR 0019: VRAM in the device domain, host overhead (and the
    // host_backed weights copy) in host RAM.
    let systems: Vec<&String> = host.domains.iter()
        .filter(|(_, d)| d.memory == DomainMemory::Distinct).map(|(n, _)| n).collect();
    let [system] = systems.as_slice() else {
        return Err(invalid("resource_policy.domains",
            "missing_system_allocation: a discrete host declares one distinct system domain"));
    };
    let copy = match residency {
        Residency::HostBacked => weights_bytes.ok_or_else(|| invalid("residency",
            "host_backed_unavailable: the checkpoint's weight size is unknown"))?,
        _ => 0,
    };
    // SGLang's --enable-weights-cpu-backup holds the copy for the engine's life;
    // vLLM level 1 allocates it only while asleep (spec §3).
    let always = if engine == Engine::Sglang { copy } else { 0 };
    let overhead = ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES;
    let two = |device: i64, host_bytes: i64, devices: Vec<DeviceClaim>| PhaseFootprint {
        allocations: vec![
            Allocation { domain: domain.clone(), bytes: device, host_kv_bytes: 0 },
            Allocation { domain: (*system).clone(), bytes: host_bytes, host_kv_bytes: 0 },
        ],
        devices,
    };
    let active = |device: i64| two(device, overhead + always, devices.to_vec());
    let parked = match residency {
        Residency::RestartOnly => two(0, 0, vec![]),
        Residency::Deep => two(PARKED_DEVICE_RESIDUE_PLACEHOLDER_BYTES.min(request), overhead, vec![]),
        Residency::HostBacked => two(PARKED_DEVICE_RESIDUE_PLACEHOLDER_BYTES.min(request), overhead + copy, vec![]),
    };
    let cold = startup.map_or(request, |peak| peak.max(request));
    return Ok(RecipeFootprints {
        cold: active(cold),
        ready: active(request),
        parking: active(request),
        parked,
        wake: active(request),
    });
}
```

(Match the exact `RecipeFootprints` fields and the parked-phase device list the unified branch uses; if the unified branch keeps devices on the parked phase, do the same.) Callers pass `facts.weights_bytes` and the profile's engine. In `core.rs`: replace the unified host-backed error text with the `host_backed_unavailable:` prefix, and add `missing_system_allocation` (explicit resources) and `multi_gpu_unsupported` before phase resolution.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mllm-config -p mllm-store -p mllm-controller --all-targets --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-config
git commit -m "feat: derive device and system budgets for every residency tier on a discrete host"
```

---

### Task 7: mllm picks the GPU

**Files:**
- Create: `crates/mllm-scheduler/src/device_choice.rs`
- Modify: `crates/mllm-scheduler/src/lib.rs`, `crates/mllm-scheduler/src/placement.rs` (`HostCandidate.device_options`, `Placement.device`)
- Modify: the coordinator's placement caller and launch resource plan (find with `grep -rn "placement::place(" crates/mllm-controller/src`), the instance record (store: `deployment_instances.device TEXT NULL`, forward-only migration to the next schema version)
- Modify: `crates/mllm-agent/src/native_execution.rs` (launch sets `CUDA_VISIBLE_DEVICES` from the plan's device UUID)
- Test: `crates/mllm-scheduler/src/device_choice.rs` tests, `crates/mllm-controller/tests/` placement test with Fake devices, `crates/mllm-agent/tests/` launch environment test

**Interfaces:**
- Consumes: `fits`, `choose_victims`, `order_victims` (existing); per-device footprints from Task 6 (resolve once per device of the host by setting the deployment's device claim to that device).
- Produces:
  - `pub struct DeviceOption { pub device: String, pub footprint: PhaseFootprint }`
  - `pub fn choose_device(ledger: &LedgerSnapshot, owner: &str, options: &[DeviceOption], limits: &[MemoryLimit], max_parked: usize, preferred: Option<&str>) -> Result<(String, i64), HostRefusal>` — fits without eviction; most headroom; ties by device index; `preferred` wins when it fits.
  - `pub fn choose_device_with_eviction(ledger, owner, options, limits, max_parked, victims: &[VictimCandidate]) -> Result<(String, Vec<String>), HostRefusal>` — per device, the victims are only owners with an allocation on that device's domain; picks the smallest victim set, then the least recently used set (sum of `last_used_ms`), then the lower device index.
  - `HostCandidate.device_options: Vec<DeviceOption>` (empty on unified hosts: today's single-footprint path is unchanged); `Placement.device: Option<String>`.
  - Launch plan field `device` (the chosen device id); the agent maps it to the device's `physical_gpu_uuid` from its own policy (never from the server's description).

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    const GIB: i64 = 1 << 30;

    fn limits2() -> Vec<MemoryLimit> {
        let l = |d: &str, m| MemoryLimit { domain: d.into(), managed_bytes: m, free_reserve_bytes: GIB,
                                           host_kv_bytes: None, parked_bytes: Some(2 * GIB) };
        vec![l("gpu0", 22 * GIB), l("gpu1", 30 * GIB), l("system", 60 * GIB)]
    }
    fn option(device: &str, bytes: i64) -> DeviceOption {
        DeviceOption { device: device.into(), footprint: footprint(&[(device, bytes), ("system", 4 * GIB)]) }
    }
    fn options(bytes: i64) -> Vec<DeviceOption> { vec![option("gpu0", bytes), option("gpu1", bytes)] }

    // T27: the instance lands on the GPU with room.
    #[test]
    fn the_gpu_with_room_is_chosen() {
        let ledger = ledger_with([("deployment:x/instance:0", footprint(&[("gpu1", 25 * GIB), ("system", 4 * GIB)]))]);
        let (device, _) = choose_device(&ledger, "deployment:y/instance:0", &options(12 * GIB), &limits2(), 4, None).unwrap();
        assert_eq!(device, "gpu0");
    }

    // T27: with both empty, the most headroom wins; a pin is honoured.
    #[test]
    fn headroom_then_pin() {
        let empty = ledger_with([]);
        assert_eq!(choose_device(&empty, "o", &options(12 * GIB), &limits2(), 4, None).unwrap().0, "gpu1");
        assert_eq!(choose_device(&empty, "o", &[option("gpu0", 12 * GIB)], &limits2(), 4, None).unwrap().0, "gpu0");
        assert_eq!(choose_device(&empty, "o", &options(12 * GIB), &limits2(), 4, Some("gpu0")).unwrap().0, "gpu0");
    }

    // T27/T16: both full; the device needing fewer evictions is chosen, and only its owners are victims.
    #[test]
    fn eviction_is_per_device() {
        let ledger = ledger_with([
            ("a", footprint(&[("gpu0", 12 * GIB), ("system", 4 * GIB)])),
            ("b", footprint(&[("gpu1", 12 * GIB), ("system", 4 * GIB)])),
            ("c", footprint(&[("gpu1", 12 * GIB), ("system", 4 * GIB)])),
        ]);
        let victims = [victim("a", 3), victim("b", 1), victim("c", 2)];
        let (device, chosen) = choose_device_with_eviction(&ledger, "n", &options(16 * GIB), &limits2(), 4, &victims).unwrap();
        assert_eq!(device, "gpu0");
        assert_eq!(chosen, vec!["a".to_string()]);
    }

    // No device can ever fit: the host's reason is returned.
    #[test]
    fn nothing_fits() {
        assert!(choose_device_with_eviction(&ledger_with([]), "n", &options(40 * GIB), &limits2(), 4, &[]).is_err());
    }
}
```

(`footprint`, `ledger_with` and `victim(owner, last_used_ms)` are small helpers at the top of the test module building `PhaseFootprint`, `LedgerSnapshot` and `VictimCandidate` with `ResourcePhase::Ready` and no device claims.)

Controller test (Fake devices, T27): a Fake host publishing `gpu0` and `gpu1` device domains; deploy two instances of 12 GiB each on 22 and 30 GiB cards; the first lands on `gpu1`, the second on `gpu0`; the recorded launch plans name those devices; after a restart the stopped instance prefers its last device. Agent test: a launch plan with device `gpu1` sets the child's `CUDA_VISIBLE_DEVICES` to `gpu1`'s published UUID; a device not in the host's own policy refuses the launch `unauthorized`.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mllm-scheduler --lib device_choice`
Expected: FAIL (module missing).

- [ ] **Step 3: Implement**

```rust
//! ADR 0019 (owner decision 3): on a multi-GPU host mllm picks the GPU.
//! Each GPU is its own device-memory domain; the choice is the device where
//! the instance fits with the most headroom, or needs the fewest evictions.

use mllm_domain::resources::{LedgerSnapshot, MemoryLimit, PhaseFootprint};

use crate::placement::{fits, HostRefusal};
use crate::switching::{choose_victims, VictimCandidate};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceOption {
    pub device: String,
    pub footprint: PhaseFootprint,
}

fn index(device: &str) -> u32 {
    device.strip_prefix("gpu").and_then(|n| n.parse().ok()).unwrap_or(u32::MAX)
}

pub fn choose_device(
    ledger: &LedgerSnapshot,
    owner: &str,
    options: &[DeviceOption],
    limits: &[MemoryLimit],
    max_parked: usize,
    preferred: Option<&str>,
) -> Result<(String, i64), HostRefusal> {
    let mut fitting = Vec::new();
    let mut last = HostRefusal::Insufficient;
    for option in options {
        match fits(ledger, owner, &option.footprint, limits, max_parked) {
            Ok(headroom) => fitting.push((option.device.clone(), headroom)),
            Err(refusal) => last = refusal,
        }
    }
    if let Some(found) = preferred.and_then(|p| fitting.iter().find(|(d, _)| d == p)) {
        return Ok(found.clone());
    }
    fitting.sort_by(|(a, ra), (b, rb)| rb.cmp(ra).then(index(a).cmp(&index(b))));
    fitting.into_iter().next().ok_or(last)
}

pub fn choose_device_with_eviction(
    ledger: &LedgerSnapshot,
    owner: &str,
    options: &[DeviceOption],
    limits: &[MemoryLimit],
    max_parked: usize,
    victims: &[VictimCandidate],
) -> Result<(String, Vec<String>), HostRefusal> {
    let mut best: Option<(String, Vec<String>, i64)> = None;
    let mut last = HostRefusal::Insufficient;
    for option in options {
        // Only owners charged on this GPU can make room on it.
        let here: Vec<VictimCandidate> = victims.iter().filter(|v| {
            ledger.owners.get(&v.owner).is_some_and(|f| f.allocations.iter()
                .any(|a| a.domain == option.device && a.bytes > 0))
        }).cloned().collect();
        match choose_victims(ledger, owner, &option.footprint, limits, max_parked, &here) {
            Ok(chosen) => {
                let recency: i64 = here.iter().filter(|v| chosen.contains(&v.owner))
                    .map(|v| v.last_used_ms).sum();
                let better = best.as_ref().is_none_or(|(d, c, r)| {
                    (chosen.len(), recency, index(&option.device)) < (c.len(), *r, index(d))
                });
                if better {
                    best = Some((option.device.clone(), chosen, recency));
                }
            }
            Err(refusal) => last = refusal,
        }
    }
    best.map(|(device, chosen, _)| (device, chosen)).ok_or(last)
}
```

This assumes device domains are named after their device (Task 2/3 rule: domain `gpuN` for device `gpuN`); for a remote host whose domain names differ, map through the policy's `devices.<id>.domain` before filtering. In `placement::place`, when `candidate.device_options` is non-empty, call `choose_device` instead of `fits` and put the device in `Placement.device`; the switching caller uses `choose_device_with_eviction` for such hosts. Order `victims` with `order_victims` before calling. Persist the chosen device with the instance and pass it as `preferred` next time (ADR 0013 §4).

- [ ] **Step 4: Run to verify they pass**

Run: the core suite from Global Constraints plus `cargo test -p mllm-scheduler -p mllm-agent --all-targets --locked`.
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-scheduler crates/mllm-controller crates/mllm-store crates/mllm-agent
git commit -m "feat: place each instance on the GPU with room on a multi-GPU host"
```

---

### Task 8: Standalone deployment template on a discrete host (request and default tier)

**Files:**
- Modify: `crates/mllm-cli/src/standalone_config.rs` (`deployment_document`)
- Modify: the standalone deploy path in `crates/mllm-cli/src/roles.rs` (`App::deploy…`, where the template is filled)
- Test: `crates/mllm-cli/src/standalone_config/tests.rs`, `crates/mllm-cli/tests/standalone_start.rs`

**Interfaces:**
- Consumes: `HostShape` on `App` (Task 3), `device_limits` (Task 3), derived budgets for three tiers (Task 6), checkpoint weights bytes (the ADR 0014 `CheckpointFacts.weights_bytes` the deploy path already reads).
- Produces:
  - `pub enum TemplateMemory { Unified { capacity_bytes: i64 }, Device { managed_limit: i64, device_total: i64, weights_bytes: i64, system_parked_limit: i64 } }` — no device id: the picker chooses it (Task 7).
  - `deployment_document(name, route, source, engine, memory: &TemplateMemory, request_deadline, deep_park, profile) -> Result<Value, TemplateError>` with `TemplateError::InsufficientDeviceMemory { request: i64, limit: i64 }` (code `insufficient_device_memory`, exit 4).
  - `pub fn device_request(engine: Engine, weights_bytes: i64, managed_limit: i64, device_total: i64) -> (i64 /*request*/, i64 /*kv*/)`: `kv = min(4 GiB, managed_limit / 4)`, `request = weights × 110 / 100 + kv`, and for vLLM at least `device_total × 75 / 100` (spec §3: vLLM 0.29 with CUDA graphs needs `--gpu-memory-utilization ≥ 0.75` for a 4B model on a 16 GB card).
  - `pub fn default_residency(deep_park: bool, discrete: Option<(i64 /*weights*/, i64 /*system parked_limit*/)>) -> &'static str`: `restart_only` when deep parking is off; on a discrete host `host_backed` when the weights fit the system parked limit, else `deep`; `deep` on a unified host (owner decision 2).

- [ ] **Step 1: Write the failing tests**

```rust
// T26/T23: a 4B bf16 model (~8 GiB) on a 16 GB card: request, no fixed shares.
#[test]
fn a_discrete_template_states_a_request_and_derives_phases() {
    let memory = TemplateMemory::Device { managed_limit: 15 << 30, device_total: 16376 << 20,
                                          weights_bytes: 8 << 30, system_parked_limit: 15 << 30 };
    let doc = deployment_document("a", "a", &source(), Engine::Sglang, &memory, DEFAULT_REQUEST_DEADLINE, true, "local").unwrap();
    assert!(doc.get("resources").is_none());
    assert!(doc.get("devices").is_none(), "the picker chooses the GPU");
    let (request, kv) = device_request(Engine::Sglang, 8 << 30, 15 << 30, 16376 << 20);
    assert_eq!(kv, (4i64 << 30).min((15i64 << 30) / 4)); // min(4 GiB, 3.75 GiB)
    assert_eq!(doc["engine_config"]["memory"]["request"], format!("{request}B"));
    assert_eq!(doc["engine_config"]["memory"]["kv_cache"], format!("{kv}B"));
    assert_eq!(doc["residency"], "host_backed");
}

// T26: vLLM's request never falls below 0.75 of the card.
#[test]
fn the_vllm_request_has_a_floor() {
    let (small, _) = device_request(Engine::Vllm, 2 << 30, 15 << 30, 16376 << 20);
    assert!(small >= (16376i64 << 20) / 100 * 75);
}

// Owner decision 2: host_backed is the discrete default when the copy fits.
#[test]
fn the_default_tier_follows_the_host() {
    assert_eq!(default_residency(true, Some((8 << 30, 15 << 30))), "host_backed");
    assert_eq!(default_residency(true, Some((20 << 30, 15 << 30))), "deep");
    assert_eq!(default_residency(true, None), "deep");
    assert_eq!(default_residency(false, Some((8 << 30, 15 << 30))), "restart_only");
}

// Review focus 1: a model that cannot fit is refused at deploy with numbers.
#[test]
fn a_model_larger_than_the_device_is_refused() {
    let memory = TemplateMemory::Device { managed_limit: 15 << 30, device_total: 16376 << 20,
                                          weights_bytes: 16 << 30, system_parked_limit: 15 << 30 };
    let error = deployment_document("a", "a", &source(), Engine::Vllm, &memory, DEFAULT_REQUEST_DEADLINE, true, "local").unwrap_err();
    assert!(matches!(error, TemplateError::InsufficientDeviceMemory { .. }));
    assert!(error.to_string().starts_with("insufficient_device_memory"));
}

// T26: the unified template is byte-identical to before.
#[test]
fn the_unified_template_is_unchanged() {
    let before = include_str!("fixtures/unified_deployment.json"); // captured on main
    let doc = deployment_document("a", "a", &source(), Engine::Vllm,
        &TemplateMemory::Unified { capacity_bytes: 128 << 30 }, DEFAULT_REQUEST_DEADLINE, true, "local").unwrap();
    assert_eq!(serde_json::to_string_pretty(&doc).unwrap(), before.trim_end());
}
```

Write the unified test concretely: capture `deployment_document(...)` output on `main` into `fixtures/unified_deployment.json` and assert equality with `TemplateMemory::Unified { capacity_bytes: 128 << 30 }`.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mllm-cli --lib standalone_config`
Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
pub fn device_request(engine: Engine, weights_bytes: i64, managed_limit: i64, device_total: i64) -> (i64, i64) {
    const GIB: i64 = 1 << 30;
    let kv = (4 * GIB).min(managed_limit / 4);
    let request = weights_bytes / 100 * 110 + kv;
    // Spec §3: vLLM 0.29 with CUDA graphs starts a 4B model on a 16 GB card
    // only at --gpu-memory-utilization >= 0.75.
    let floor = if engine == Engine::Vllm { device_total / 100 * 75 } else { 0 };
    (request.max(floor), kv)
}

pub fn default_residency(deep_park: bool, discrete: Option<(i64, i64)>) -> &'static str {
    match (deep_park, discrete) {
        (false, _) => "restart_only",
        // ADR 0019 (owner decision 2): a wake from pinned host RAM is the
        // discrete default when the copy fits the host's parked limit.
        (true, Some((weights, parked_limit))) if weights <= parked_limit => "host_backed",
        (true, _) => "deep",
    }
}
```

In `deployment_document`, for `TemplateMemory::Device`, compute the request; if `request > managed_limit` return the error; otherwise emit the same document as today minus `resources` and `devices` (the picker chooses the GPU, Task 7), with `"residency": default_residency(deep_park, Some((weights_bytes, system_parked_limit)))` and `"engine_config": {"memory": {"request": "<n>B", "kv_cache": "<kv>B"}}`. The deploy path picks `Device { managed_limit, device_total }` of the largest GPU and the system domain's `parked_limit` when `App.gpu_shape` is `Discrete`, maps the error to the management error code `insufficient_device_memory` (HTTP 409, CLI exit 4) and stores nothing.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mllm-cli --all-targets --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-cli
git commit -m "feat: size standalone deployments from the checkpoint and park to host RAM by default on a discrete GPU"
```

---

### Task 9: Multi-domain launch check, and planner agreement

**Files:**
- Modify: `crates/mllm-agent/src/native_execution/refusal.rs` (`admit_memory`)
- Test: `crates/mllm-agent/src/native_execution/refusal.rs` tests (or its sibling test module), `crates/mllm-scheduler/src/switching.rs` tests, `crates/mllm-agent/tests/discrete_switch.rs` (new)

**Interfaces:**
- Consumes: `observe_domains`/`GpuSampler` (Task 4), two-allocation footprints (Task 6). A `host_backed` SGLang launch's copy is part of its cold system allocation, so the system-domain check covers it.
- Produces: `fn admit_memory_with(effective: &EffectiveDeployment, host: &HostMemorySample, gpu: Option<&GpuSample>) -> Result<(), LaunchVerdict>` (pure; `admit_memory` samples and calls it). Refusal reasons: `insufficient_memory` (system/unified), `insufficient_device_memory` (device), `unauthorized` (undeclared domain or limit above capacity), `LaunchVerdict::Uncertain` (device unobserved).

- [ ] **Step 1: Write the failing tests**

```rust
// T26: each cold allocation is checked against its own domain.
#[test]
fn the_launch_check_reads_the_device() {
    let effective = discrete_effective(/*device cold*/ 10 << 30, /*system cold*/ 4 << 30);
    let host = ram(61 << 30, 50 << 30);
    assert!(admit_memory_with(&effective, &host, Some(&gpu(16376, 2000))).is_ok());
    // 10 GiB + 1.25 GiB reserve > 5 GiB free on the card
    assert_eq!(admit_memory_with(&effective, &host, Some(&gpu(16376, 11_256))),
               Err(LaunchVerdict::Refused("insufficient_device_memory")));
    assert_eq!(admit_memory_with(&effective, &host, None), Err(LaunchVerdict::Uncertain));
    assert_eq!(admit_memory_with(&effective, &ram(61 << 30, 10 << 30), Some(&gpu(16376, 0))),
               Err(LaunchVerdict::Refused("insufficient_memory")));
}

// T26: the unified single-pool behaviour is unchanged.
#[test]
fn the_unified_launch_check_is_unchanged() {
    let effective = unified_effective(48 << 30); // one `unified` allocation, managed 96 GiB, reserve 12 GiB
    assert!(admit_memory_with(&effective, &ram(128 << 30, 70 << 30), None).is_ok());
    assert_eq!(admit_memory_with(&effective, &ram(128 << 30, 50 << 30), None),
               Err(LaunchVerdict::Refused("insufficient_memory")));
}
```

In `crates/mllm-agent/tests/discrete_switch.rs` (T16/T27, and review focus 5), one fixture drives both sides:

```rust
// Two 8 GiB-weight models on a 16 GiB card, 61 GiB RAM. The planner evicts A
// for B; the launch check refuses B before A's release and admits it after.
#[test]
fn planner_and_launch_check_agree_on_a_small_card() {
    let limits = discrete_limits();             // gpu0 managed 15 GiB / reserve 1.25 GiB; system 30 GiB
    let a = ready_footprint(9 << 30, 4 << 30);  // device, system
    let b_cold = cold_footprint(9 << 30, 4 << 30);
    let ledger = ledger_with([("deployment:a/instance:0", a)]);
    let victims = choose_victims(&ledger, "deployment:b/instance:0", &b_cold, &limits, 4,
        &[victim("deployment:a/instance:0")]).unwrap();
    assert_eq!(victims, vec!["deployment:a/instance:0".to_string()]);
    let effective_b = discrete_effective(9 << 30, 4 << 30);
    let before = gpu(16376, 9 * 1024 + 300);   // A holds 9 GiB + context
    let after = gpu(16376, 1024);              // A deep-parked: 1 GiB residue
    assert!(admit_memory_with(&effective_b, &ram(61 << 30, 50 << 30), Some(&before)).is_err());
    assert!(admit_memory_with(&effective_b, &ram(61 << 30, 50 << 30), Some(&after)).is_ok());
}

// Review focus 5: three parked contexts plus a wake exceed the device parked limit.
#[test]
fn parked_contexts_count_against_the_device() {
    let limits = discrete_limits();             // gpu0 parked_limit 2 GiB
    let parked = |n| (format!("deployment:p{n}/instance:0"), parked_footprint(1 << 30, 4 << 30));
    let ledger = ledger_with([parked(1), parked(2)]);
    let third = parked_footprint(1 << 30, 4 << 30);
    assert!(fits(&ledger, "deployment:p3/instance:0", &third, &limits, 4).is_err());
}
```

(The helpers `discrete_limits`, `unified_effective`, `ready_footprint`, `cold_footprint`, `parked_footprint`, `ledger_with`, `victim`, `gpu`, `ram`, `discrete_effective` are defined at the top of the test file with the literal numbers above; `gpu(total_mib, used_mib)` builds a `GpuSample` through `parse_query_gpu`.)

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mllm-agent --test discrete_switch && cargo test -p mllm-agent --lib refusal`
Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
fn admit_memory_with(
    effective: &EffectiveDeployment,
    host: &HostMemorySample,
    gpu: Option<&GpuSample>,
) -> Result<(), LaunchVerdict> {
    let refused = LaunchVerdict::Refused;
    let allocations = &effective.resources.cold.allocations;
    if allocations.is_empty() {
        return Err(refused("unauthorized"));
    }
    for allocation in allocations {
        let limit = effective.host.domains.get(&allocation.domain).ok_or(refused("unauthorized"))?;
        // SPEC §7.2: each domain is read from its own source, never substituted.
        let (capacity, available, short) = match (limit.memory, limit.device.as_deref()) {
            (DomainMemory::Device, Some(id)) => {
                let index: u32 = id.strip_prefix("gpu").and_then(|n| n.parse().ok())
                    .ok_or(refused("unauthorized"))?;
                let memory = gpu.and_then(|s| s.devices.iter().find(|d| d.index == index))
                    .and_then(|d| d.memory.as_ref()).ok_or(LaunchVerdict::Uncertain)?;
                (memory.total_bytes, memory.free_bytes, "insufficient_device_memory")
            }
            (DomainMemory::Device, None) => return Err(refused("unauthorized")),
            _ => (host.memory.capacity_bytes, host.memory.available_bytes, "insufficient_memory"),
        };
        if allocation.bytes > limit.managed_limit || limit.managed_limit > capacity {
            return Err(refused("unauthorized"));
        }
        if allocation.bytes.checked_add(limit.free_reserve).is_none_or(|need| need > available) {
            return Err(refused(short));
        }
    }
    Ok(())
}
```

`admit_memory` reads meminfo as today, samples the GPU only if some allocation's domain is a device, and calls `admit_memory_with`. Add `insufficient_device_memory` to the refusal-reason list the journal and the coordinator accept (search for `"insufficient_memory"` in `mllm-agent`, `mllm-controller` and `mllm-management` and add the sibling beside each occurrence).

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mllm-agent -p mllm-scheduler -p mllm-controller -p mllm-management --all-targets --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-agent crates/mllm-scheduler crates/mllm-controller crates/mllm-management
git commit -m "feat: check every memory domain at launch, including the GPU"
```

---

### Task 10: Engine sizing on a discrete device

**Files:**
- Modify: `crates/mllm-adapters/src/vllm/args.rs` and the grant resolution that fills `gpu_utilization_pct` (`--gpu-memory-utilization` from the device request on a device domain)
- Modify: `crates/mllm-adapters/src/sglang/args.rs` (launch spec gains `device_total_bytes`)
- Modify: `runtime/sglang_launch_spec.py`, `runtime/sglang_server_args.py` (`static_fraction` baseline)
- Test: `crates/mllm-adapters/tests/vllm_args.rs`, `crates/mllm-adapters/tests/sglang_args.rs`, `runtime/tests/test_sglang_server_args.py`

**Interfaces:**
- Consumes: `DomainPolicy.device` (Task 2), device total from the GPU sample (Task 4) — the agent fills `device_total_bytes` in the launch input from the same sample the launch check used (Task 9).
- Produces: `pub fn device_utilization_pct(request: i64, device_total: i64) -> u8` = `ceil(request × 100 / device_total)` clamped to `75..=99`; the vLLM grant sets `gpu_utilization_pct` from it on a device domain. `SglangLaunchSettings.memory.device_total_bytes: Option<i64>` rendered as `"device_total_bytes"` in the entry's closed settings only when `Some`; Python `available_bytes_for(spec) -> int` returning `spec["device_total_bytes"]` when present, else `available_memory_bytes()`.

- [ ] **Step 1: Write the failing tests**

```python
# runtime/tests/test_sglang_server_args.py
class DiscreteBaselineTest(unittest.TestCase):
    # T26 / ADR 0014 open issue 2: on a discrete device the fraction is of the card.
    def test_device_total_is_the_baseline(self):
        spec = {"device_total_bytes": 16376 * 2**20}
        self.assertEqual(sglang_server_args.available_bytes_for(spec), 16376 * 2**20)
        self.assertEqual(sglang_server_args.static_fraction(8 * 2**30, 16 * 2**30), 0.5)

    def test_unified_keeps_memavailable(self):
        with mock.patch.object(sglang_server_args, "available_memory_bytes", return_value=100):
            self.assertEqual(sglang_server_args.available_bytes_for({}), 100)

    def test_bad_device_total_is_refused(self):
        for bad in (0, -1, "16", 2**63):
            with self.assertRaises(sglang_server_args.ServerArgsError):
                sglang_server_args.available_bytes_for({"device_total_bytes": bad})
```

```rust
// crates/mllm-adapters/tests/vllm_args.rs — T26: KV bytes and the card fraction on a discrete device.
#[test]
fn a_discrete_launch_renders_kv_bytes_and_utilization() {
    assert_eq!(device_utilization_pct(12 << 30, 16376 << 20), 76);
    assert_eq!(device_utilization_pct(2 << 30, 16376 << 20), 75);
    assert_eq!(device_utilization_pct(20 << 30, 16376 << 20), 99);
    let mut input = launch_input(); // existing builder
    input.granted.gpu_utilization_pct = Some(76);
    input.granted.kv_cache_bytes = Some(4 << 30);
    let argv = render(&input).unwrap().argv;
    assert!(argv.windows(2).any(|w| w == ["--kv-cache-memory-bytes", &(4i64 << 30).to_string()]));
    assert!(argv.windows(2).any(|w| w == ["--gpu-memory-utilization", "0.76"]));
}
```

```rust
// crates/mllm-adapters/tests/sglang_args.rs — T26
#[test]
fn a_device_total_rides_the_settings_only_when_known() {
    let mut s = settings();
    assert!(public_settings_json(&s)["memory"].get("device_total_bytes").is_none());
    s.memory.device_total_bytes = Some(16376 << 20);
    assert_eq!(public_settings_json(&s)["memory"]["device_total_bytes"], 16376i64 << 20);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `python3 -m unittest runtime.tests.test_sglang_server_args -v; cargo test -p mllm-adapters --test sglang_args --test vllm_args`
Expected: FAIL.

- [ ] **Step 3: Implement**

```python
def available_bytes_for(spec):
    """ADR 0019: a discrete device sizes against its own total; unified keeps MemAvailable."""
    total = spec.get("device_total_bytes")
    if total is None:
        return available_memory_bytes()
    if type(total) is not int or not 0 < total < 2**62:
        raise ServerArgsError("memory_grant_unavailable")
    return total
```

Replace the `available_memory_bytes()` call feeding `static_fraction` (near line 347) with `available_bytes_for(spec)`, and allow the key in `sglang_launch_spec.py`'s closed schema (int, optional). In Rust, add the optional field to the settings struct and serialize it only when `Some`. For vLLM:

```rust
/// ADR 0019: vLLM checks that this fraction of the card is free at start;
/// the planner and the launch check already made that room.
pub fn device_utilization_pct(request: i64, device_total: i64) -> u8 {
    let pct = (request.saturating_mul(100) + device_total - 1) / device_total.max(1);
    pct.clamp(75, 99) as u8
}
```

and the grant resolution sets `gpu_utilization_pct = Some(device_utilization_pct(ready_device_bytes, device_total))` when the allocation's domain is a device domain; the unified path is unchanged.

- [ ] **Step 4: Run to verify they pass**

Run: `python3 -m unittest discover -s runtime/tests; cargo test -p mllm-adapters --all-targets --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add runtime crates/mllm-adapters crates/mllm-agent
git commit -m "feat: size vLLM and SGLang against the device total on a discrete GPU"
```

---

### Task 11: Host-backed park: park-or-stop in the planner, and vLLM level 1

**Files:**
- Modify: `crates/mllm-scheduler/src/switching.rs` (`choose_victims` returns a release kind per victim)
- Modify: the coordinator's switch execution (find with `grep -rn "choose_victims" crates/mllm-controller/src`) to park or stop per victim
- Modify: `crates/mllm-agent/src/native_execution.rs:1296-1305` (Park admitted for `Residency::HostBacked` too)
- Modify: `crates/mllm-adapters/src/vllm/residency.rs`, `crates/mllm-adapters/src/vllm/adapter.rs` (level from the binding's residency)
- Test: `crates/mllm-scheduler/src/switching.rs` tests, `crates/mllm-adapters/tests/vllm_residency.rs` (Fake engine), `crates/mllm-controller/src/coordinator/tests_residency.rs`

**Interfaces:**
- Consumes: parked footprints from Task 6 (the `host_backed` copy on the system domain).
- Produces:
  - `pub enum Release { Park, Stop }` and `pub struct Victim { pub owner: String, pub release: Release }`; `VictimCandidate` gains `pub parked: Option<PhaseFootprint>` (`None` for `restart_only`); `choose_victims(...) -> Result<Vec<Victim>, HostRefusal>`. A victim is released by `Park` when, with every chosen victim applied, the ledger with its parked footprint still fits the waiting footprint; otherwise `Stop`. Parking is tried for victims in preference order; a victim is switched to `Stop` when keeping it parked makes the fit fail.
  - `ParkLevel { Deep = 2, HostBacked = 1 }` in `vllm/residency.rs`, taken from the adapter's launch settings (`residency`), never from the command.
  - The vLLM `ReloadWeights` step under `HostBacked` makes no engine call and reports `Milestone::WeightsUsable`, so `worker.rs`'s persisted sequence (Restore, ReloadWeights, InvalidateCache, Probe) is unchanged.

- [ ] **Step 1: Write the failing tests**

```rust
// switching.rs tests — T27 and review focus 4
#[test]
fn a_copy_that_does_not_fit_is_stopped_not_parked() {
    const GIB: i64 = 1 << 30;
    // system parked_limit 12 GiB, one 8 GiB copy already parked.
    let limits = discrete_limits_with_system_parked(12 * GIB);
    let ledger = ledger_with([
        ("p", parked_footprint(1 * GIB, 4 * GIB + 8 * GIB)),
        ("a", ready_footprint(12 * GIB, 4 * GIB)),
    ]);
    let a_parked = parked_footprint(1 * GIB, 4 * GIB + 8 * GIB); // another 8 GiB copy
    let victims = [VictimCandidate { owner: "a".into(), serves_elsewhere: false, last_used_ms: 1,
                                     parked: Some(a_parked) }];
    let chosen = choose_victims(&ledger, "b", &cold_footprint(12 * GIB, 4 * GIB), &limits, 4, &victims).unwrap();
    assert_eq!(chosen, vec![Victim { owner: "a".into(), release: Release::Stop }]);
}

#[test]
fn a_copy_that_fits_is_parked() {
    const GIB: i64 = 1 << 30;
    let limits = discrete_limits_with_system_parked(24 * GIB);
    let ledger = ledger_with([("a", ready_footprint(12 * GIB, 4 * GIB))]);
    let victims = [VictimCandidate { owner: "a".into(), serves_elsewhere: false, last_used_ms: 1,
                                     parked: Some(parked_footprint(1 * GIB, 12 * GIB)) }];
    let chosen = choose_victims(&ledger, "b", &cold_footprint(12 * GIB, 4 * GIB), &limits, 4, &victims).unwrap();
    assert_eq!(chosen[0].release, Release::Park);
}
```

```rust
// crates/mllm-adapters/tests/vllm_residency.rs — T20/T16 against the Fake vLLM HTTP server
#[tokio::test]
async fn host_backed_parks_at_level_one_and_never_reloads() {
    let fake = FakeVllm::start().await;                  // existing Fake engine used by the deep tests
    let adapter = adapter_for(&fake, Residency::HostBacked);
    run(&adapter, RuntimeAction::Park).await.unwrap();
    assert_eq!(fake.calls(), ["POST /sleep?level=1", "GET /is_sleeping"]);
    fake.clear();
    for action in [RuntimeAction::Restore, RuntimeAction::ReloadWeights, RuntimeAction::InvalidateCache] {
        run(&adapter, action).await.unwrap();
    }
    assert_eq!(fake.calls(), ["POST /wake_up?tags=weights", "POST /wake_up?tags=kv_cache",
                              "POST /reset_prefix_cache", "GET /is_sleeping"]);
    assert!(!fake.calls().iter().any(|c| c.contains("collective_rpc")));
}

#[tokio::test]
async fn deep_still_parks_at_level_two_and_reloads() {
    let fake = FakeVllm::start().await;
    let adapter = adapter_for(&fake, Residency::Deep);
    run(&adapter, RuntimeAction::Park).await.unwrap();
    assert_eq!(fake.calls()[0], "POST /sleep?level=2");
    run(&adapter, RuntimeAction::ReloadWeights).await.unwrap();
    assert!(fake.calls().iter().any(|c| c.contains("collective_rpc")));
}
```

Agent test: a `host_backed` owner's Park command is admitted (today it is refused `unauthorized`), a `restart_only` one is still refused.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mllm-scheduler --lib switching && cargo test -p mllm-adapters --test vllm_residency`
Expected: FAIL.

- [ ] **Step 3: Implement**

In `vllm/residency.rs` replace `const DEEP_LEVEL: u8 = 2;` with:

```rust
/// ADR 0010, ADR 0019: the park level follows the deployment's declared
/// residency, fixed at launch; it is never chosen at park time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParkLevel { HostBacked = 1, Deep = 2 }
```

`Park` calls `http.sleep(adapter.park_level() as u8)`. `ReloadWeights`:

```rust
RuntimeAction::ReloadWeights => {
    if adapter.park_level() == ParkLevel::HostBacked {
        // Level 1 kept the weights in pinned host RAM and the weights wake
        // copied them back: there is nothing to reload (vLLM sleep mode docs).
        Milestone::WeightsUsable
    } else {
        http.collective_rpc().await.map_err(|e| uncertain("reload_weights", e))?;
        Milestone::WeightsUsable
    }
}
```

`VllmAdapter` gets `park_level: ParkLevel` from its launch settings (`Residency::HostBacked` → `HostBacked`, otherwise `Deep`). In `native_execution.rs`, admit `Residency::HostBacked | Residency::Deep` for Park and Restore. In `choose_victims`, after the minimal set is found, decide the release per victim:

```rust
let mut releases: Vec<Victim> = chosen.iter()
    .map(|o| Victim { owner: o.clone(), release: Release::Park }).collect();
for i in 0..releases.len() {
    let parked = ordered.iter().find(|c| c.owner == releases[i].owner).and_then(|c| c.parked.clone());
    let trial = |rs: &[Victim]| {
        let mut state = ledger.clone();
        for v in rs {
            match (&v.release, ordered.iter().find(|c| c.owner == v.owner).and_then(|c| c.parked.clone())) {
                (Release::Park, Some(p)) => { state.owners.insert(v.owner.clone(), p); }
                _ => { state.owners.remove(&v.owner); }
            }
        }
        fits(&state, owner, footprint, limits, max_parked).is_ok()
    };
    if parked.is_none() || !trial(&releases) {
        releases[i].release = Release::Stop;
    }
}
Ok(releases)
```

The coordinator executes `Release::Park` through the existing park path and `Release::Stop` through the existing stop path, and the status line names which one happened (`released: parked` / `released: stopped (host RAM full)`).

- [ ] **Step 4: Run to verify they pass**

Run: the core suite plus `cargo test -p mllm-scheduler -p mllm-agent --all-targets --locked`.
Expected: PASS; existing deep tests unchanged.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-scheduler crates/mllm-controller crates/mllm-agent crates/mllm-adapters
git commit -m "feat: park vLLM to host RAM at sleep level 1, and stop a victim whose copy does not fit"
```

---

### Task 12: Host-backed park for SGLang

**Files:**
- Modify: `crates/mllm-adapters/src/sglang/http.rs:240-260` and `crates/mllm-adapters/src/sglang/adapter.rs` (reload step under `HostBacked`)
- Modify: `runtime/sglang_saver_binding.py:125-131`, `runtime/sglang_saver_residency.py:115-125,175-185` (accept the weights CPU backup for a `host_backed` launch only)
- Modify: `runtime/engine_capabilities.py` (the deep probe already reads `enable_weights_cpu_backup`; a build without it answers `host_backed_unavailable`)
- Test: `crates/mllm-adapters/tests/sglang_residency.rs` (Fake engine), `runtime/tests/test_sglang_saver_binding.py`, `runtime/tests/test_sglang_saver_residency.py`

**Interfaces:**
- Consumes: `cpu_weight_backup` in the launch settings (already rendered from `Residency::HostBacked`, `effective/engine_config.rs:634`).
- Produces: SGLang `ReloadWeights` under `HostBacked` makes no engine call and reports `WeightsUsable`; the saver binding's check becomes `values.get("enable_weights_cpu_backup") is not expected_backup`, where `expected_backup` comes from the launch spec's `weight_restore == "cpu_backup"`; `enable_draft_weights_cpu_backup` stays required `False`.

- [ ] **Step 1: Write the failing tests**

```python
# runtime/tests/test_sglang_saver_binding.py
class HostBackedBindingTest(unittest.TestCase):
    # T22 / ADR 0019: the weights backup is accepted only when the launch asked for it.
    def test_backup_accepted_for_host_backed(self):
        values = dict(BASE_VALUES, enable_weights_cpu_backup=True)
        sglang_saver_binding.check_values(values, weight_restore="cpu_backup")

    def test_backup_refused_for_deep(self):
        values = dict(BASE_VALUES, enable_weights_cpu_backup=True)
        with self.assertRaises(sglang_saver_binding.BindingError):
            sglang_saver_binding.check_values(values, weight_restore="disk_reload")

    def test_missing_backup_refused_for_host_backed(self):
        with self.assertRaises(sglang_saver_binding.BindingError):
            sglang_saver_binding.check_values(dict(BASE_VALUES), weight_restore="cpu_backup")

    def test_draft_backup_always_refused(self):
        values = dict(BASE_VALUES, enable_weights_cpu_backup=True, enable_draft_weights_cpu_backup=True)
        with self.assertRaises(sglang_saver_binding.BindingError):
            sglang_saver_binding.check_values(values, weight_restore="cpu_backup")
```

(`BASE_VALUES` is the module's existing valid-values fixture; if the check is not yet a function named `check_values`, extract it into one in Step 3 and keep the existing call site.) Mirror the four cases in `test_sglang_saver_residency.py` for the region key's `cpu_backup` flag of the `weights` tag (the `kv_cache` tag keeps `cpu_backup is False`).

```rust
// crates/mllm-adapters/tests/sglang_residency.rs — T20/T22
#[tokio::test]
async fn host_backed_restores_from_host_ram_without_a_disk_reload() {
    let fake = FakeSglang::start().await;
    let adapter = adapter_for(&fake, Residency::HostBacked);
    for action in [RuntimeAction::Park, RuntimeAction::Restore, RuntimeAction::ReloadWeights,
                   RuntimeAction::InvalidateCache] {
        run(&adapter, action).await.unwrap();
    }
    assert_eq!(fake.paths(), ["/release_memory_occupation", "/resume_memory_occupation", "/flush_cache"]);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `python3 -m unittest runtime.tests.test_sglang_saver_binding runtime.tests.test_sglang_saver_residency -v; cargo test -p mllm-adapters --test sglang_residency`
Expected: FAIL.

- [ ] **Step 3: Implement**

```python
def check_values(values, weight_restore):
    """ADR 0019: the weights CPU backup is the host_backed tier's mechanism.

    Present exactly when the launch declared `cpu_backup`; the draft-model
    backup is never used by mllm.
    """
    expected = weight_restore == "cpu_backup"
    if (values.get("enable_weights_cpu_backup") is not expected
            or values.get("enable_draft_weights_cpu_backup") is not False):
        raise BindingError()
```

In the residency observer, accept `cpu_backup is True` for the `weights` tag only when the bound launch declared `cpu_backup`. In the adapter:

```rust
RuntimeAction::ReloadWeights if self.cpu_weight_backup => {
    // SGLang restored the weights from its pinned host copy on resume.
    return Ok(Milestone::WeightsUsable);
}
```

placed before the HTTP call, with `cpu_weight_backup` taken from the adapter's launch settings.

- [ ] **Step 4: Run to verify they pass**

Run: `python3 -m unittest discover -s runtime/tests; cargo test -p mllm-adapters --all-targets --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add runtime crates/mllm-adapters
git commit -m "feat: park SGLang to host RAM with its weights CPU backup"
```

---

### Task 13: Remote hosts report device domains under a capability

**Files:**
- Modify: `crates/mllm-protocol/src/capabilities.rs` (`DEVICE_MEMORY_DOMAINS`)
- Modify: the proto file holding `DomainObservation` and the resident message (under `crates/mllm-protocol/`; find with `grep -rn "message DomainObservation" crates/mllm-protocol`)
- Modify: `crates/mllm-cli/src/remote_roles.rs:725-765` (connect report), host start policy check
- Modify: `crates/mllm-controller/src/agent_sessions.rs` (map device observations and resident fields)
- Modify: `crates/mllm-scheduler/src/placement.rs` or the coordinator's host-eligibility filter (capability gate)
- Test: `crates/mllm-protocol` capability test, `crates/mllm-controller/tests/` session test, `crates/mllm-cli/tests/` host start test

**Interfaces:**
- Produces:
  - `pub const DEVICE_MEMORY_DOMAINS: &str = "device_memory_domains";` added to the host's declared list.
  - `DomainObservation.device_id` (string, next free field number) and `kind: "device"` for device domains.
  - Resident message gains `device_bytes` and `host_bytes` (next free numbers); absent means 0 and the server falls back to `bytes` on unified domains only.
  - Placement refuses a footprint naming a `Device` domain on a host without the capability: `host_capability_missing:device_memory_domains` (existing typed-reason mechanism).
  - Host start: `pub fn check_device_policy(policy: &HostPolicy, shape: &HostShape) -> Result<(), String>` returning a `device_policy_mismatch: …` message naming the domain and observed total.
- Consumes: Tasks 1, 2, 4, 5, 7.

The same capability gates the chosen device in the launch plan (Task 7) and the split residents (Task 5). Record the chosen field numbers in the ADR (Task 17).

- [ ] **Step 1: Write the failing tests**

```rust
// T34: a host that did not declare the capability is refused typed; nothing is sent.
#[tokio::test]
async fn device_domains_need_the_capability() {
    let server = test_server().await; // existing session harness
    let host = server.connect_host(discrete_policy(), &[/* no device_memory_domains */]).await;
    let refusal = server.place(deployment_on(&host, "gpu0")).await.unwrap_err();
    assert_eq!(refusal.reason, "host_capability_missing:device_memory_domains");
    assert!(host.sent_commands().is_empty());
}

// T26: a capable host's device observation reaches the coordinator.
#[tokio::test]
async fn a_device_observation_is_accepted() {
    let server = test_server().await;
    let host = server.connect_host(discrete_policy(), &[DEVICE_MEMORY_DOMAINS]).await;
    host.report_inventory(vec![system_obs(), device_obs("gpu0", 16376 << 20, 14000 << 20)]).await;
    assert_eq!(server.observed(&host, "gpu0").await.capacity_bytes, 16376 << 20);
}

// T03: a declared device domain that the GPU cannot back refuses the start.
#[test]
fn a_mismatched_device_policy_refuses_start() {
    let shape = HostShape::Discrete(vec![rtx(0, 16376, 0)]);
    let mut policy = discrete_policy_resolved();
    policy.domains.get_mut("gpu0").unwrap().managed_limit = 20 << 30;
    let error = check_device_policy(&policy, &shape).unwrap_err();
    assert!(error.starts_with("device_policy_mismatch: gpu0"));
    assert!(check_device_policy(&discrete_policy_resolved(), &HostShape::NoGpu).is_err());
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mllm-controller -p mllm-cli --all-targets device`
Expected: FAIL.

- [ ] **Step 3: Implement**

- `check_device_policy`: for each `Device` domain, the device `gpuN` must be in `shape` with memory, and `managed_limit + free_reserve <= total_bytes`; a `Device` domain on a `NoGpu`/`Unified` shape is a mismatch. Called at `mllm start host` after config load, before connecting; failure exits 2 with the message.
- Connect report (`remote_roles.rs`): replace the `supported = declared.len() == 1 && unified` rule with the shape-aware builder: system/unified domains from meminfo (a single `distinct` or `unified` system domain is supported), device domains from the GPU sample with `kind: "device"` and `device_id`, unknown encoded `-1`.
- Declare `DEVICE_MEMORY_DOMAINS` in the host's capability list only when the build supports it (always, for this release).
- Server: accept `kind: "device"` only from a host that declared the capability; gate placement as above.

- [ ] **Step 4: Run to verify they pass**

Run: the core suite from Global Constraints.
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-protocol crates/mllm-cli crates/mllm-controller crates/mllm-scheduler
git commit -m "feat: report device memory domains from remote hosts behind a capability"
```

---

### Task 14: Inference listener bind: default and `--listen`

Owner decision 5: the server's generated default moves too. Existing documents are migrated by Task 15.

**Files:**
- Modify: `crates/mllm-config/src/standalone.rs` (`LISTENERS`, bind check, template in tests)
- Modify: `crates/mllm-config/src/defaults.rs:262-265` (generated standalone document)
- Modify: `crates/mllm-config/src/remote_roles.rs:343,393` (server inference listener not loopback-forced; template)
- Modify: `crates/mllm-cli/src/grammar.rs` (`--listen` on `start standalone` and `start server`; `Invocation.listen`)
- Modify: `crates/mllm-cli/src/roles.rs:220-252` (`standalone_inference_address`), `crates/mllm-cli/src/main.rs:292`, `crates/mllm-cli/src/remote_roles.rs:557`
- Test: `crates/mllm-config/src/standalone.rs` tests, `crates/mllm-config/tests/remote_roles.rs`, `crates/mllm-cli/tests/grammar.rs`, `crates/mllm-cli/tests/standalone_start.rs`

**Interfaces:**
- Produces:
  - `pub const DEFAULT_INFERENCE_BIND: &str = "0.0.0.0:8443";` in `mllm-config::standalone`.
  - `pub fn inference_bind(document: &Value) -> Result<SocketAddr, ConfigError>`: the document's `server.listeners.inference.bind` if stated, else `DEFAULT_INFERENCE_BIND`; refuses port 0 and multicast.
  - `Invocation.listen: Option<SocketAddr>`.
  - `pub fn effective_inference_address(document_bind: SocketAddr, listen: Option<SocketAddr>) -> Result<SocketAddr, StartError>`: precedence `--listen` > `MLLM_STANDALONE_INFERENCE_ADDR` > document; env and flag may be any non-multicast address with a non-zero port.
  - The management listener rule is unchanged (loopback-only, `MLLM_STANDALONE_MANAGEMENT_ADDR`).

- [ ] **Step 1: Write the failing tests**

```rust
// T02: a new standalone document binds inference on all interfaces.
#[test]
fn the_generated_document_binds_all_interfaces() {
    let doc = generated_standalone_document(Path::new("/s")); // defaults.rs generator
    assert_eq!(doc["server"]["listeners"]["inference"]["bind"], "0.0.0.0:8443");
    assert_eq!(doc["server"]["listeners"]["management"]["bind"], "127.0.0.1:7443");
}

// T03: the old loopback shape still validates (Task 15 migrates it before the bind is read).
#[test]
fn the_old_loopback_document_still_validates() {
    let doc = generated("/s"); // the old shape with 127.0.0.1:8443
    assert!(validate(&doc).is_ok());
    assert_eq!(inference_bind(&doc).unwrap().to_string(), "127.0.0.1:8443");
}

// T03: the inference bind accepts any unicast address; management stays loopback.
#[test]
fn bind_rules() {
    for ok in ["0.0.0.0:8443", "100.64.0.5:8443", "[::]:8443", "127.0.0.1:9000"] {
        let mut d = generated("/s");
        d["server"]["listeners"]["inference"]["bind"] = ok.into();
        assert!(validate(&d).is_ok(), "{ok}");
    }
    for bad in ["0.0.0.0:0", "224.0.0.1:8443", "nonsense"] {
        let mut d = generated("/s");
        d["server"]["listeners"]["inference"]["bind"] = bad.into();
        assert!(validate(&d).is_err(), "{bad}");
    }
    let mut d = generated("/s");
    d["server"]["listeners"]["management"]["bind"] = "0.0.0.0:7443".into();
    assert!(validate(&d).is_err());
}
```

```rust
// crates/mllm-cli/tests/grammar.rs — T01
#[test]
fn listen_is_parsed_on_start_standalone_and_server() {
    let i = parse_invocation(["mllm", "start", "standalone", "--listen", "100.64.0.5:8443"]).unwrap();
    assert_eq!(i.listen, Some("100.64.0.5:8443".parse().unwrap()));
    assert!(parse_invocation(["mllm", "start", "server", "--listen", "0.0.0.0:9443"]).unwrap().listen.is_some());
    assert!(parse_invocation(["mllm", "start", "standalone", "--listen", "bad"]).is_err());
    assert!(parse_invocation(["mllm", "status", "--listen", "0.0.0.0:1"]).is_err());
}

// roles tests — precedence
#[test]
fn listen_beats_environment_beats_document() {
    let doc: SocketAddr = "0.0.0.0:8443".parse().unwrap();
    std::env::remove_var(INFERENCE_ADDR_ENV);
    assert_eq!(effective_inference_address(doc, None).unwrap(), doc);
    std::env::set_var(INFERENCE_ADDR_ENV, "100.64.0.5:8443");
    assert_eq!(effective_inference_address(doc, None).unwrap().to_string(), "100.64.0.5:8443");
    assert_eq!(effective_inference_address(doc, Some("127.0.0.1:1".parse().unwrap())).unwrap().to_string(), "127.0.0.1:1");
    std::env::remove_var(INFERENCE_ADDR_ENV);
}
```

For the server: `ServerConfig::parse` accepts `listeners.inference.bind: "0.0.0.0:8443"` and the template states it; `management` stays loopback-forced.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mllm-config -p mllm-cli --all-targets listen bind generated`
Expected: FAIL.

- [ ] **Step 3: Implement**

- `standalone.rs`: split the bind check — `management` must equal its loopback default as today; `inference` goes through `inference_bind`. Update the module doc comment (the "loopback" wording) and the refusal message.
- `defaults.rs`: the generated `inference.bind` becomes `"0.0.0.0:8443"`.
- `remote_roles.rs`: `listener(&v, "inference", "api_key", false)` and the template's inference bind `"0.0.0.0:8443"`; update the authentication check in Task 16.
- `grammar.rs`: `#[arg(long, value_name = "ADDR:PORT")] listen: Option<SocketAddr>` on `StartTarget::Standalone` and `StartTarget::Server` (turn `Server` into a struct variant), copied into `Invocation.listen`.
- `roles.rs`: replace `standalone_inference_address()` with `effective_inference_address(document_bind, listen)`; the env var check drops the loopback filter for inference only. `main.rs` and `remote_roles.rs` bind the effective address and print it in the start banner.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mllm-config -p mllm-cli --all-targets --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-config crates/mllm-cli
git commit -m "feat: serve inference on all interfaces by default with --listen to narrow it"
```

---

### Task 15: One-time migration of loopback inference listeners

**Files:**
- Create: `crates/mllm-config/src/listener_migration.rs`
- Modify: `crates/mllm-config/src/lib.rs` (`pub mod listener_migration;`)
- Modify: `crates/mllm-cli/src/roles.rs` (standalone start) and `crates/mllm-cli/src/remote_roles.rs` (server start): run the migration after the document is read and before it is parsed for listeners
- Test: in-module tests of `listener_migration.rs`, `crates/mllm-cli/tests/standalone_start.rs`

**Interfaces:**
- Consumes: `DEFAULT_INFERENCE_BIND` (Task 14).
- Produces:
  - `pub const OLD_DEFAULT: &str = "127.0.0.1:8443";`
  - `pub const MARKER: &str = "migrations/inference-bind-v1";`
  - `pub enum Migration { NotNeeded, Rewritten { backup: PathBuf }, BindOnly { reason: String } }`
  - `pub fn migrate(document: &Path, state_dir: &Path, parsed_bind: Option<&str>) -> Migration` — pure file logic; `parsed_bind` is the document's `listeners.inference.bind` (server) or `server.listeners.inference.bind` (standalone) as parsed by the caller.
  - `pub fn notice(document: &Path, outcome: &Migration) -> Option<String>` — the spec §9 text, or the `BindOnly` variant naming the line to edit.
  - Callers: on `Rewritten`, re-read the document; on `BindOnly`, override the inference bind to `0.0.0.0:8443` for this run and log `config_migration_failed`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const DOC: &str = "{\n  \"listeners\": {\n    \"management\": {\"bind\": \"127.0.0.1:7443\"},\n    \"inference\": {\"bind\": \"127.0.0.1:8443\", \"authentication\": \"api_key\"}\n  }\n}\n";

    fn setup(text: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let doc = dir.path().join("server.yaml");
        fs::write(&doc, text).unwrap();
        (dir, doc)
    }

    // T02 / owner decision 1: the old default is rewritten once, with a backup and a marker.
    #[test]
    fn the_old_default_is_migrated_once() {
        let (dir, doc) = setup(DOC);
        let outcome = migrate(&doc, dir.path(), Some(OLD_DEFAULT));
        let Migration::Rewritten { backup } = &outcome else { panic!("{outcome:?}") };
        assert_eq!(fs::read_to_string(backup).unwrap(), DOC);
        let after = fs::read_to_string(&doc).unwrap();
        assert_eq!(after, DOC.replace("127.0.0.1:8443", "0.0.0.0:8443"));
        assert!(after.contains("\"api_key\""));
        assert!(dir.path().join(MARKER).exists());
        assert!(notice(&doc, &outcome).unwrap().starts_with("NOTICE: mllm 0.1.0 serves inference on all interfaces"));
        // A second start, even after the operator sets loopback back, does nothing.
        fs::write(&doc, DOC).unwrap();
        assert!(matches!(migrate(&doc, dir.path(), Some(OLD_DEFAULT)), Migration::NotNeeded));
        assert_eq!(fs::read_to_string(&doc).unwrap(), DOC);
    }

    // T03: an operator's own address is never migrated.
    #[test]
    fn other_addresses_are_left_alone() {
        for bind in ["127.0.0.1:9000", "100.64.0.5:8443", "0.0.0.0:8443"] {
            let (dir, doc) = setup(&DOC.replace("127.0.0.1:8443", bind));
            assert!(matches!(migrate(&doc, dir.path(), Some(bind)), Migration::NotNeeded), "{bind}");
        }
    }

    // Review focus 3: ambiguous text is not rewritten; the run still serves on 0.0.0.0.
    #[test]
    fn ambiguous_text_binds_only() {
        let text = format!("# was 127.0.0.1:8443\n{DOC}");
        let (dir, doc) = setup(&text);
        assert!(matches!(migrate(&doc, dir.path(), Some(OLD_DEFAULT)), Migration::BindOnly { .. }));
        assert_eq!(fs::read_to_string(&doc).unwrap(), text);
    }

    // Review focus 3: an unwritable directory never corrupts the document.
    #[test]
    fn an_unwritable_document_binds_only() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, doc) = setup(DOC);
        let config = dir.path().join("ro");
        fs::create_dir(&config).unwrap();
        let ro_doc = config.join("server.yaml");
        fs::write(&ro_doc, DOC).unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o500)).unwrap();
        let outcome = migrate(&ro_doc, dir.path(), Some(OLD_DEFAULT));
        fs::set_permissions(&config, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(outcome, Migration::BindOnly { .. }));
        assert_eq!(fs::read_to_string(&ro_doc).unwrap(), DOC);
        let _ = doc;
    }
}
```

In `standalone_start.rs` (T02): a state root created with the previous generator's document starts, prints the notice once on stderr, binds `0.0.0.0` (checked with `getsockname` on the listener), and a second start prints nothing.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mllm-config --lib listener_migration`
Expected: FAIL (module missing).

- [ ] **Step 3: Implement**

```rust
//! ADR 0019 (owner decision 1): the old loopback inference default moves to
//! all interfaces once, on upgrade. The only sanctioned rewrite of an
//! administrator document (SPEC §15.1 as amended): one value, atomically,
//! with the original kept beside it, and never twice.
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub const OLD_DEFAULT: &str = "127.0.0.1:8443";
pub const NEW_DEFAULT: &str = "0.0.0.0:8443";
pub const MARKER: &str = "migrations/inference-bind-v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Migration {
    NotNeeded,
    Rewritten { backup: PathBuf },
    BindOnly { reason: String },
}

pub fn migrate(document: &Path, state_dir: &Path, parsed_bind: Option<&str>) -> Migration {
    let marker = state_dir.join(MARKER);
    if marker.exists() || parsed_bind != Some(OLD_DEFAULT) {
        return Migration::NotNeeded;
    }
    let outcome = rewrite(document).unwrap_or_else(|reason| Migration::BindOnly { reason });
    // The marker is written whatever the outcome: the notice is one-time, and
    // a BindOnly run already serves on the new default.
    if let Some(parent) = marker.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&marker, b"inference bind migrated to 0.0.0.0:8443\n");
    outcome
}

fn rewrite(document: &Path) -> Result<Migration, String> {
    let text = fs::read_to_string(document).map_err(|e| format!("cannot read: {e}"))?;
    if text.matches(OLD_DEFAULT).count() != 1 {
        return Err(format!("{OLD_DEFAULT} does not occur exactly once in the document"));
    }
    let mode = fs::metadata(document).map_err(|e| e.to_string())?.permissions().mode();
    let backup = PathBuf::from(format!("{}.pre-0.1.0", document.display()));
    let dir = document.parent().ok_or("no parent directory")?;
    let temporary = dir.join(format!(".{}.migrating", document.file_name().unwrap().to_string_lossy()));
    let write = |path: &Path, body: &str| -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(path)?;
        file.write_all(body.as_bytes())?;
        file.sync_all()
    };
    use std::os::unix::fs::OpenOptionsExt;
    if !backup.exists() {
        write(&backup, &text).map_err(|e| format!("cannot keep a backup: {e}"))?;
    }
    let _ = fs::remove_file(&temporary);
    write(&temporary, &text.replacen(OLD_DEFAULT, NEW_DEFAULT, 1))
        .map_err(|e| format!("cannot write: {e}"))?;
    fs::rename(&temporary, document).map_err(|e| {
        let _ = fs::remove_file(&temporary);
        format!("cannot replace: {e}")
    })?;
    Ok(Migration::Rewritten { backup })
}

pub fn notice(document: &Path, outcome: &Migration) -> Option<String> {
    let path = document.display();
    match outcome {
        Migration::NotNeeded => None,
        Migration::Rewritten { backup } => Some(format!(
            "NOTICE: mllm 0.1.0 serves inference on all interfaces: {NEW_DEFAULT} (was {OLD_DEFAULT}).\n\
             The API key is still required. Configuration updated: {path} (previous copy: {}).\n\
             To keep inference local, start with --listen {OLD_DEFAULT} or set listeners.inference.bind.",
            backup.display()
        )),
        Migration::BindOnly { reason } => Some(format!(
            "NOTICE: mllm 0.1.0 serves inference on all interfaces: {NEW_DEFAULT} (was {OLD_DEFAULT}).\n\
             The API key is still required. {path} was not changed ({reason}); edit listeners.inference.bind there.\n\
             To keep inference local, start with --listen {OLD_DEFAULT}."
        )),
    }
}
```

(Move the `use std::os::unix::fs::OpenOptionsExt;` to the top imports.) The callers print the notice with `eprintln!` and `tracing::warn!`, and for `BindOnly` also log `config_migration_failed`. A standalone document that the role generated in its state directory and a server document passed with `--config` go through the same function.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mllm-config -p mllm-cli --all-targets --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-config crates/mllm-cli
git commit -m "feat: migrate the old loopback inference listener to all interfaces once, with a notice"
```

---

### Task 16: Inference authentication opt-out and warning

**Files:**
- Modify: `crates/mllm-config/src/standalone.rs` (`inference` accepts `api_key | none`)
- Modify: `crates/mllm-config/src/remote_roles.rs` (`listener` accepts `none` for inference)
- Modify: `crates/mllm-cli/src/grammar.rs` (`--no-inference-auth` on both starts; `Invocation.no_inference_auth`)
- Modify: `crates/mllm-cli/src/roles.rs:945-948` (remove the `mllm-local` fallback), `:1206` (`api_key: None` when auth is off), `crates/mllm-cli/src/remote_roles.rs:519`
- Create: `crates/mllm-cli/src/exposure.rs` (warning text and decision)
- Modify: `crates/mllm-cli/src/client.rs` status rendering (`inference: unauthenticated on <addr>`)
- Test: `crates/mllm-cli/src/exposure.rs` tests, `crates/mllm-router/tests/` auth test, `crates/mllm-cli/tests/standalone_start.rs`

**Interfaces:**
- Produces:
  - `pub enum InferenceAuth { ApiKey, None }`; `pub fn inference_auth(document: &Value, no_auth_flag: bool) -> Result<InferenceAuth, ConfigError>`
  - `pub fn exposure_warning(bind: SocketAddr, auth: InferenceAuth) -> Option<String>` in `exposure.rs`: `Some(text)` exactly when `!bind.ip().is_loopback() && auth == InferenceAuth::None`.
  - Router: `RouterDeps.api_key: None` only when auth is `None` (the existing `None => true` path).
- Consumes: `effective_inference_address` (Task 14).

- [ ] **Step 1: Write the failing tests**

```rust
// T37: the warning fires only for an unauthenticated non-loopback bind.
#[test]
fn the_warning_is_loud_and_precise() {
    let open: SocketAddr = "0.0.0.0:8443".parse().unwrap();
    let text = exposure_warning(open, InferenceAuth::None).expect("warns");
    assert!(text.starts_with("WARNING: the inference endpoint on 0.0.0.0:8443 accepts requests without an API key."));
    assert!(text.contains("Anyone who can reach this address can use your models and GPU."));
    assert!(text.contains("--listen"));
    assert!(exposure_warning(open, InferenceAuth::ApiKey).is_none());
    assert!(exposure_warning("127.0.0.1:8443".parse().unwrap(), InferenceAuth::None).is_none());
    assert!(exposure_warning("[::1]:8443".parse().unwrap(), InferenceAuth::None).is_none());
}

// T37 / review focus 4: a keyed router on any bind refuses a missing or wrong key on every route.
#[tokio::test]
async fn every_inference_route_needs_the_key() {
    let app = serve_router(deps_with_key("k"));
    for (method, path) in [("GET", "/v1/models"), ("POST", "/v1/chat/completions")] {
        for header in [None, Some("Bearer wrong"), Some("Basic k")] {
            let status = call(&app, method, path, header).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path} {header:?}");
        }
        assert_ne!(call(&app, method, path, Some("Bearer k")).await, StatusCode::UNAUTHORIZED);
    }
}

// T37: no constant key; unreadable fresh credentials are a start failure.
#[test]
fn fresh_credentials_must_be_readable() {
    let dir = tempfile::tempdir().unwrap();
    // create_standalone_credentials then corrupt the file to drop the api_key line
    let error = start_with_unreadable_key(dir.path()).unwrap_err();
    assert!(matches!(error, StartError::MissingCredentials));
}

// T03: `authentication: none` is accepted for inference only.
#[test]
fn authentication_none_is_inference_only() {
    let mut d = generated("/s");
    d["server"]["listeners"]["inference"]["authentication"] = "none".into();
    assert_eq!(inference_auth(&d, false).unwrap(), InferenceAuth::None);
    assert_eq!(inference_auth(&generated("/s"), true).unwrap(), InferenceAuth::None);
    d["server"]["listeners"]["management"]["authentication"] = "none".into();
    assert!(validate(&d).is_err());
}
```

In `standalone_start.rs`: start with `--listen 0.0.0.0:<free port>` and `--no-inference-auth`; the captured stderr contains the warning once, before the "listening" line; `mllm status` shows `inference: unauthenticated on 0.0.0.0:<port>`.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mllm-cli -p mllm-router -p mllm-config --all-targets auth warning`
Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
//! SPEC §13.3 / ADR 0019: exposing the inference endpoint without a key is
//! the operator's explicit choice, and it is said out loud.
use std::net::SocketAddr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InferenceAuth { ApiKey, None }

pub fn exposure_warning(bind: SocketAddr, auth: InferenceAuth) -> Option<String> {
    if bind.ip().is_loopback() || auth == InferenceAuth::ApiKey {
        return None;
    }
    Some(format!(
        "WARNING: the inference endpoint on {bind} accepts requests without an API key.\n\
         Anyone who can reach this address can use your models and GPU.\n\
         Set listeners.inference.authentication: api_key, or bind to 127.0.0.1 or a Tailscale address with --listen."
    ))
}
```

Print it with `eprintln!` and `tracing::warn!` before `serve` in `main.rs` (standalone) and `remote_roles.rs` (server). Remove `None if created_this_boot => "mllm-local".to_string(),`. Put `InferenceAuth` in `mllm-config` (so `inference_auth` can return it) and re-export it from `exposure.rs`. Status: add the listener's effective bind and auth to the management status payload (additive field `inference_listener: {bind, authenticated}`) and render the line when `authenticated` is false.

- [ ] **Step 4: Run to verify they pass**

Run: the core suite plus `cargo test -p mllm-cli -p mllm-router --all-targets --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-config crates/mllm-cli crates/mllm-router crates/mllm-management
git commit -m "feat: require the inference key by default and warn loudly when it is turned off"
```

---

### Task 17: ADR 0019 and SPEC amendments

**Files:**
- Create: `docs/design/adr/0019-discrete-gpu-and-network-endpoint.md`
- Modify: `docs/SPEC.md` §6.2, §7.2, §13.3, §15.1, §15.2, §16.2, §16.5, §20 (T26, T37), §21 revision history
- Modify: `AGENTS.md` "Authoritative documents" item 2 (ADR list mentions 0019)

**Interfaces:** none (documentation). Content is taken from the spec; it must state the proto field numbers chosen in Task 13.

- [ ] **Step 1: Write the ADR**

Sections, in the style of ADR 0017/0018: Status (`Accepted (owner decisions A and B, 2026-09-25, and the owner's decisions on PR #38 the same day)`), Amends (the SPEC sections above), Related (ADR 0007, 0010, 0012, 0013, 0014, 0017), Context (spec "Problem" condensed), Decision (numbered: 1 detection via `nvidia-smi`; 2 `memory: device` domain and its rules; 3 two-domain derived budgets and placeholders; 4 observation, residents, launch check; 5 three tiers on a discrete host — `host_backed` (vLLM sleep level 1, SGLang weights CPU backup; the copy charged on the system domain; park-or-stop when it does not fit; the discrete default), `deep`, `restart_only` — and `host_backed_unavailable` on unified hosts; 6 one GPU per model, mllm picks the GPU, optional pin, `multi_gpu_unsupported`; 7 capability `device_memory_domains` with field numbers; 8 inference bind `0.0.0.0:8443`, `--listen`, the one-time migration of the old loopback default with backup, marker and notice; 9 `authentication: none` / `--no-inference-auth` with the warning; 10 management and engines unchanged; 11 error codes table), Consequences, Open issues (moving an instance between GPUs; measured host overhead and parked residue).

- [ ] **Step 2: Amend SPEC**

Add under each amended section a line `> **Amended by [ADR 0019](design/adr/0019-discrete-gpu-and-network-endpoint.md)** (owner decision 2026-09-25).` followed by the new normative text:
- §7.2: "On a discrete-GPU host each GPU's memory is a `device` domain observed from the device; host RAM is a `distinct` system domain. A deployment's derived budget charges both."
- §6.2: "`host_backed` is supported on hosts whose device memory is distinct from host RAM; it is the default there." 
- §15.1: "One sanctioned rewrite: the one-time migration of the old loopback inference default (ADR 0019)."
- §15.2 Defaults: "The inference listener binds all interfaces (`0.0.0.0:8443`) and requires the API key; `authentication: none` is an explicit opt-out that warns at start when the bind is not loopback. Management listeners stay loopback-only."
- §16.2: a discrete-host variant of the `resource_policy` block (the spec §2 example).
- §16.5: `inference.bind: "0.0.0.0:8443"` in the generated shape; the sentence "standalone listeners serve plain HTTP on loopback" becomes "serve plain HTTP; use a private network or a TLS reverse proxy".
- §20 T26 evidence: "…and device-domain observation, derived device and system budgets, and a switch on a small card"; T37: "…and a non-loopback inference bind without a key warns; no constant key".
- §21: one revision line.

- [ ] **Step 3: Check**

Run: `grep -n "ADR 0019" docs/SPEC.md AGENTS.md docs/design/adr/0019-*.md | wc -l` (expect ≥ 7) and `cargo test -p harness --all-targets --locked` (the harness checks SPEC §20 tags).

- [ ] **Step 4: Commit**

```bash
git add docs/design/adr/0019-discrete-gpu-and-network-endpoint.md docs/SPEC.md AGENTS.md
git commit -m "docs: ADR 0019 for discrete GPUs and the network inference endpoint"
```

---

### Task 18: Operator documentation

**Files:**
- Create: `docs/examples/host-discrete.yaml` (parseable host document for one 24 GB card)
- Create: `docs/operations/network-access.md`
- Create: `docs/operations/release-notes-0.1.0.md` (draft text the owner copies into the release; mllm never publishes a release)
- Modify: `docs/operations/install.md` (discrete GPU section, upgrading section, link to network access), `docs/README.md` (index)
- Test: `mllm validate config --file docs/examples/host-discrete.yaml` inside the existing examples test (find it with `grep -rn "docs/examples" crates/*/tests`)

- [ ] **Step 1: Add the example to the examples test** (it must fail until the file exists)

```rust
// T03: every shipped example validates.
#[test]
fn the_discrete_host_example_validates() {
    validate_example("docs/examples/host-discrete.yaml");
}
```

- [ ] **Step 2: Write `host-discrete.yaml`** as a copy of `docs/examples/host.yaml` with the header "for a discrete-GPU host (one 24 GB NVIDIA card; device memory is its own domain)" and this `resource_policy` block:

```yaml
resource_policy:
  domains:
    system:
      memory: distinct
      managed_limit: "32GiB"
      free_reserve: "12GiB"
      parked_limit: "16GiB"
      host_kv_limit: "8GiB"
    gpu0:
      memory: device
      device: gpu0
      managed_limit: "22GiB"
      free_reserve: "2GiB"
      parked_limit: "2GiB"
  # host_backed parks keep weights here: size parked_limit on `system` for them.
  devices:
    gpu0:
      domain: gpu0
      sharing: shared
```

- [ ] **Step 3: Write `network-access.md`** with these sections and commands:
  - "Default": inference on `0.0.0.0:8443`, key required; where the key is (`grep '^api_key:' <state_dir>/identity/credentials`, owner-only file); client example `curl -H "Authorization: Bearer $KEY" http://<host>:8443/v1/models`.
  - "Narrow to a Tailscale address": `mllm start standalone --listen "$(tailscale ip -4):8443"`; the same as `listeners.inference.bind` in the document; Tailscale ACL snippet allowing only your devices to port 8443.
  - "Loopback only": `--listen 127.0.0.1:8443`.
  - "Turning the key off": `authentication: none` or `--no-inference-auth`, the warning text, and why not to do it beyond loopback.
  - "Internet exposure through a TLS reverse proxy": keep mllm on `127.0.0.1:8443` (or the tailnet), put Caddy in front:

    ```
    models.example.com {
        reverse_proxy 127.0.0.1:8443 {
            flush_interval -1
        }
    }
    ```

    (`flush_interval -1` keeps streaming responses streaming); keep the API key on.
  - "What is never exposed": management (loopback, admin token) and engines (loopback, per-launch keys, ADR 0012).

- [ ] **Step 4: Update `install.md`** with a "Discrete NVIDIA GPU" section: requirements (`nvidia-smi` on the path), what standalone detects, the two domains, one GPU per model, mllm picks the GPU and `devices: [{id: gpu1}]` pins one, the three residency tiers and why `host_backed` is the default there, what `insufficient_device_memory` means; and an "Upgrading to 0.1.0" section with the migration notice and how to narrow the address.

- [ ] **Step 4b: Write `release-notes-0.1.0.md`** with a "Network access" entry:

```markdown
### Inference is reachable from other machines

The inference endpoint now listens on all interfaces, `0.0.0.0:8443`, and still
requires the API key. An existing configuration that used the old default
(`127.0.0.1:8443`) is updated once at the first start, with a copy of the old file
kept as `<file>.pre-0.1.0` and a notice printed. To keep inference on this
machine only, start with `--listen 127.0.0.1:8443` or set
`listeners.inference.bind` in the configuration; to limit it to your tailnet, use
the machine's Tailscale address. See docs/operations/network-access.md.
```

and a "Discrete NVIDIA GPUs" entry (device memory accounted, host-RAM parking, mllm picks the GPU).

- [ ] **Step 5: Run and commit**

Run: `cargo test --workspace --all-targets --locked the_discrete_host_example_validates`
Expected: PASS.

```bash
git add docs/examples/host-discrete.yaml docs/operations docs/README.md crates
git commit -m "docs: discrete GPU hosts and network access to the inference endpoint"
```

---

### Task 19: Live-work rule and live rows DG1–DG7 on the 16 GB discrete-GPU laptop host

CPU and Fake-engine tests are not qualification; these rows are. The multi-GPU picker (Task 7) has no live row here.

**Files:**
- Modify: `AGENTS.md` "Hard constraints"
- Create: `scripts/live/matrix/discrete_gpu.sh`
- Modify: `scripts/live/matrix/hosts.example.env` (placeholders `DGPU_HOST`, `DGPU_VLLM_VENV`, `DGPU_SGLANG_VENV`, `DGPU_MODEL_A`, `DGPU_MODEL_B`), `scripts/live/matrix/README.md`
- Modify: `docs/runbooks/f2-current-status.md` (results, in place)

**Interfaces:**
- Consumes: every earlier task; host name, venv paths and model paths only from the untracked `hosts.local.env`.

- [ ] **Step 1: Amend `AGENTS.md`** — add to "Hard constraints", after the lab-host bullet:

```markdown
- Live work is also authorized on the maintainers' local machines, a discrete-GPU
  laptop included. On a local machine: no driver, CUDA or system-package changes,
  and engine virtual environments only in the home directory. The lab-host rules
  above (no new environments beyond the listed exceptions) still apply to the lab
  hosts.
```

- [ ] **Step 2: Prepare the host** (operator step): the vLLM 0.29 and SGLang 0.5.20 environments already exist in that host's home directory (paths in `hosts.local.env`). Register them: `mllm engine add "$DGPU_VLLM_VENV"` and `mllm engine add "$DGPU_SGLANG_VENV" --arg --attention-backend --arg triton` (that host's SGLang has no flashinfer). Put a 4B and a 3B instruct model (bf16) in the model store. The vLLM request floor of 0.75 of the card (Task 10) is what lets vLLM start a 4B model with CUDA graphs on 16 GB.

- [ ] **Step 3: Write `discrete_gpu.sh`** following the existing matrix scripts (source `hosts.local.env`, `set -euo pipefail`, a trap that stops only mllm-owned deployments, one function per row, results to an untracked local log). Rows:
  - `dg1_vllm_host_backed`: deploy A and B on vLLM with `residency: host_backed`; request A; request B (A parked, `status` shows A's copy on `system` and residue on `gpu0`; `nvidia-smi` shows A's process at ≤ 1.5 GiB); request A (B parked, A woken); record wake time.
  - `dg2_vllm_deep`: the same with `residency: deep`; record wake time for comparison.
  - `dg3_sglang`: both tiers on SGLang.
  - `dg4_mixed`: A on vLLM, B on SGLang, switching both ways.
  - `dg5_refusal`: deploy a model whose derived request exceeds the device limit; expect exit 4 and `insufficient_device_memory` within 30 s and no engine process.
  - `dg6_network_migration`: install the previous release, create a standalone install, upgrade to this build, start: the notice appears once and the document holds `0.0.0.0:8443` with the backup beside it; from another machine on the tailnet `/v1/models` with the key is 200 and without it 401; restart with `--listen 127.0.0.1:8443`: the peer cannot connect; restart with `--no-inference-auth` on `0.0.0.0`: the warning is in the log.
  - `dg7_remote_host` (optional for 0.1.0): run `mllm start host` on the laptop host against a server on the same machine; `mllm list hosts` shows `gpu0`; deploy and switch A/B.
  - Regression: rerun the existing unified switching row on one GB10 lab host.

- [ ] **Step 4: Run and record**

Run: `scripts/live/matrix/discrete_gpu.sh all`
Expected: DG1–DG6 pass; DG7 passes or is recorded pending. Update `docs/runbooks/f2-current-status.md` in place: rows, commit range, wake times per tier, measured parked residue and engine host RSS (to replace the placeholders later), and the statements that CPU/Fake tests are not qualification and that the multi-GPU picker has no live evidence in the repository.

- [ ] **Step 5: Commit**

```bash
git add AGENTS.md scripts/live/matrix docs/runbooks/f2-current-status.md
git commit -m "test: live discrete-GPU parking, switching, migration and network rows"
```

---

## Self-review notes

- Spec coverage: §1 → T1; §2 → T2, T3; §3 → T6, T8; §4 → T4, T5, T9; §5 → T6, T11, T12; §6 → T10; §7 → T7; §8 → T13; §9 → T14, T15, T16, T18; §10 → T16, T17; §11 → T6, T8, T9, T13, T15, T16; §12 → tests in every task, live T19; ADR → T17; docs and release notes → T18; owner decision 4 → T19.
- Names used across tasks: `HostShape`, `GpuSample`, `GpuDevice`, `GpuMemory`, `parse_query_gpu`, `DomainMemory::Device`, `DomainPolicy.device`, `device_limits`, `ObservedDomain`, `observe_domains`, `ProcessResident.device_bytes/host_bytes`, `ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES`, `PARKED_DEVICE_RESIDUE_PLACEHOLDER_BYTES`, `DeviceOption`, `choose_device`, `choose_device_with_eviction`, `Placement.device`, `TemplateMemory`, `device_request`, `default_residency`, `admit_memory_with`, `device_utilization_pct`, `Release`, `Victim`, `ParkLevel`, `check_values`, `DEVICE_MEMORY_DOMAINS`, `check_device_policy`, `inference_bind`, `effective_inference_address`, `Migration`, `migrate`, `notice`, `InferenceAuth`, `exposure_warning`.
- Interaction to watch: Task 11 changes `choose_victims`' return type, and Task 7's `choose_device_with_eviction` consumes it; if Task 11 lands after Task 7, update `choose_device_with_eviction` to take `Vec<Victim>` and compare by length as written.

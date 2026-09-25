# Discrete GPU and Network Endpoint Implementation Plan

**Execution:** implement task by task in order; each task ends green on its own tests and is committed before the next starts. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Account one discrete NVIDIA GPU as its own device-memory domain (standalone and remote hosts, one GPU per model) so eviction, deep parking and switching work on a 16–32 GB card, and serve the inference endpoint on `0.0.0.0:8443` by default with an API key required unless explicitly opted out.

**Architecture:** A bounded `nvidia-smi` collector in `mllm-agent` classifies the host (`NoGpu`, `Unified`, `Discrete`) and observes each discrete GPU's memory. The host policy gains a `device` domain kind mapped from each GPU; derived deployment budgets charge the device domain for weights and KV and the system domain for engine host overhead, so the existing per-domain admission, switch planner and (generalized) launch check all see VRAM. Remote hosts report device domains under a new ADR 0017 capability. The inference listener reads its bind from the role document (default `0.0.0.0:8443`), `--listen` overrides it per run, and `authentication: none` is an explicit opt-out with a loud warning on a non-loopback bind.

**Tech Stack:** Rust 2021 workspace (tokio, tonic/prost, axum, rusqlite, clap, serde/serde_json, saphyr strict YAML), Python 3 runtime helpers under `runtime/` (unittest), bash live harness under `scripts/live/matrix/`.

**Spec:** `docs/specs/2026-09-25-discrete-gpu-and-network-endpoint-design.md`. Read it with this plan. Governing documents: `docs/SPEC.md` §7, §13.3, §15, §16, §20; ADR 0007, 0010, 0012, 0013, 0014, 0017; `AGENTS.md`.

## Owner checks

The spec's "Owner checks" section lists five points. The tasks that depend on them: owner check 1 (existing documents keep loopback) is Task 11; owner check 2 (host-backed refused) is Task 6; owner check 3 (explicit device selection only) is Tasks 2 and 3; owner check 4 (live work and new venvs on the 16 GB discrete-GPU laptop host) is Task 15; owner check 5 (server default moves too) is Task 11. Do not start a dependent task before its check is answered; the other tasks do not wait.

## Global Constraints

- Cite the governing requirement inline where behaviour is spec-driven, e.g. `// SPEC §7.2: ...`, `// ADR 0019: ...` (AGENTS.md "Code conventions").
- Tag every new test with its acceptance-matrix ID (`// T26`). IDs used: T02, T03 (configuration), T16, T23, T26, T27 (accounting and switching), T21, T37 (security), T29 (unknown pressure), T34 (capability gating).
- Uncertainty keeps accounting: an unobserved device closes admission on its domain and releases nothing.
- No new crate dependency. GPU memory is read with `nvidia-smi` only (`/usr/bin/nvidia-smi`, then `/bin/nvidia-smi`), cleared environment, 3 s bound, 64 KiB output cap.
- Unified hosts are unchanged: the published standalone document, stored policies and their digests stay byte-identical on a unified or no-GPU host.
- Engines stay loopback-only with per-launch keys and the key-guard middleware (ADR 0012). The management listener stays loopback-only.
- Additive protocol only: no field renumbered, `PROTOCOL_VERSION` stays `"2"`, command encoding version stays `"1"`. New capability: `device_memory_domains`.
- New closed codes (spec §11): `insufficient_device_memory`, `device_unobserved`, `device_policy_mismatch`, `unsupported_gpu_topology`, `missing_system_allocation`, `multi_gpu_unsupported`, `residency_unsupported:host_backed`. Exits: 4 for the first two, 5 for `multi_gpu_unsupported`, `unsupported_gpu_topology`, `residency_unsupported:host_backed`, 2 for the rest. No new exit number.
- Placeholders (spec §3): `ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES = 4 GiB`, `PARKED_DEVICE_RESIDUE_PLACEHOLDER_BYTES = 1 GiB`. Device reserve `max(1 GiB, 8 % of total)`; device parked limit `min(2 GiB × max_parked, 25 % of total)`.
- Default inference bind `0.0.0.0:8443`; default management bind `127.0.0.1:7443` (unchanged).
- In tracked files, commits and the PR: no machine names, addresses or home paths. The live box is "a 16 GB discrete-GPU laptop host".
- CPU and Fake-engine tests are not qualification; the live rows DG1–DG6 are. Say so in every status claim.
- Verification before every commit: `cargo fmt --all --check`; the core suite `cargo test -p mllm-adapters -p mllm-store -p mllm-controller -p mllm-management -p harness --all-targets --no-fail-fast --locked -- --test-threads=4`; `cargo test --workspace --all-targets --locked`; `cargo clippy --workspace --all-targets --locked -- -D warnings`; Python helpers: `python3 -m unittest discover -s runtime/tests`.

## Review Focus

1. **A laptop GPU already partly used by the desktop.** `memory.used` of 1–2 GiB at boot must lower availability, not the managed limit's honesty: the first deploy must still fit when it fits, and be refused with numbers when it does not. Pinned in Task 3 (policy from a fixture with 1.5 GiB used) and Task 8 (launch check with that availability).
2. **`nvidia-smi` hangs or disappears after boot** (driver reload, suspend/resume on a laptop). Admission must close on the device domain with `device_unobserved` and keep every reservation; nothing may be launched on a stale reading. Pinned in Task 1 (timeout) and Task 4 (unknown observation).
3. **Upgrade of an existing standalone install.** The old generated document states `127.0.0.1:8443`; it must keep starting, stay on loopback, and print the `--listen` hint — never widen silently. Pinned in Task 11.
4. **A client with a wrong or missing key on a `0.0.0.0` bind** must get 401 on every inference route, including `/v1/models`. Pinned in Task 12.
5. **Two parked engines' CUDA contexts plus a waking one** exceed a small card even though each alone fits. The device `parked_limit` and the parked residue must make the planner evict or stop rather than let the wake fail. Pinned in Task 8.

---

## File map

| File | Responsibility | Tasks |
|---|---|---|
| `crates/mllm-agent/src/gpu_memory.rs` (new) | bounded `nvidia-smi --query-gpu` collector, `HostShape` | 1 |
| `crates/mllm-config/src/effective.rs`, `effective/core.rs` | `DomainMemory::Device`, `device` field, host-policy rules | 2 |
| `crates/mllm-store/src/resource_policy.rs` | stored `device` field, identity unchanged | 2 |
| `crates/mllm-cli/src/standalone_config.rs`, `device_inventory.rs` | discrete standalone policy, UUID per device | 3, 7 |
| `crates/mllm-cli/src/host_observation.rs`, `crates/mllm-agent/src/native_execution.rs` (`inventory()`), `crates/mllm-cli/src/remote_roles.rs` | shape-aware observation | 4, 10 |
| `crates/mllm-domain/src/resources.rs`, `crates/mllm-agent/src/process_residency.rs`, `crates/mllm-store/src/resident_floors.rs` | per-domain resident credit | 5 |
| `crates/mllm-config/src/effective/engine_config.rs`, `effective/core.rs` | two-domain derived budgets, resolution refusals | 6 |
| `crates/mllm-agent/src/native_execution/refusal.rs` | multi-domain launch check | 8 |
| `crates/mllm-adapters/src/vllm/args.rs`, `crates/mllm-adapters/src/sglang/args.rs`, `runtime/sglang_server_args.py` | engine sizing on a discrete device | 9 |
| `crates/mllm-protocol/*`, `crates/mllm-controller/src/agent_sessions.rs`, `crates/mllm-scheduler/src/placement.rs` | capability and remote reporting | 10 |
| `crates/mllm-config/src/standalone.rs`, `remote_roles.rs`, `defaults.rs`, `crates/mllm-cli/src/grammar.rs`, `main.rs`, `roles.rs` | bind default, `--listen`, auth opt-out, warning | 11, 12 |
| `docs/design/adr/0019-discrete-gpu-and-network-endpoint.md` (new), `docs/SPEC.md` | ADR and amendments | 13 |
| `docs/examples/host-discrete.yaml` (new), `docs/operations/install.md`, `docs/operations/network-access.md` (new) | operator docs | 14 |
| `scripts/live/matrix/discrete_gpu.sh` (new), `AGENTS.md`, `docs/runbooks/f2-current-status.md` | live rows DG1–DG6 | 15 |

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
        let text = "0, GPU-11111111-2222-3333-4444-555555555555, 00000000:01:00.0, NVIDIA GeForce RTX 4090 Laptop GPU, 16376, 1500, 14876\n";
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

// T26: two GPUs, each its own device domain (owner check 3).
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

// T26 (owner check 3): two GPUs publish two devices and two device domains.
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

and place them under `resource_policy.domains` / `resource_policy.devices`. Introduce `const MAX_PARKED: i64 = 4;` and use it for the existing `"max_parked": 4`. In `device_inventory.rs`, parse every device's UUID into `physical_gpu_uuids` keyed by the collector's index; the UUID from `runtime/sglang_device.py` must equal the `nvidia-smi` UUID for the same PCI bus id, else publish no UUIDs (fail closed). In `roles.rs` boot, call `mllm_agent::gpu_memory::sample()` once, compute `shape(...)`, map `MixedTopology` to `StartError::GpuTopology`, and pass the shape to `host_policy`. Keep the shape on `App` (`pub gpu_shape: HostShape`) for Tasks 4 and 7.

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
- Modify: `crates/mllm-protocol/proto/*.proto` resident message (field numbers assigned in Task 10; this task only uses the in-process type — remote hosts send the new fields after Task 10)
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

### Task 6: Two-domain derived budgets and resolution refusals

**Files:**
- Modify: `crates/mllm-config/src/effective/engine_config.rs` (`derive_resources`, constants)
- Modify: `crates/mllm-config/src/effective/core.rs:400-425` (explicit-resource checks, residency checks)
- Test: `crates/mllm-config/tests/effective.rs`

**Interfaces:**
- Produces:
  - `pub const ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES: i64 = 4 << 30;`
  - `pub const PARKED_DEVICE_RESIDUE_PLACEHOLDER_BYTES: i64 = 1 << 30;`
  - `derive_resources` returns, when the selected device's domain is `Device`, two allocations per phase: `[device, system]` (spec §3 table). The system domain is the host's single `distinct` domain; none or several is `missing_system_allocation`.
  - Refusals (all `ConfigErrorCode::UnsupportedCombination`, detail prefixed by the code): `missing_system_allocation` (explicit `resources:` naming a device domain but not the system domain, any phase that is not zero), `multi_gpu_unsupported` (more than one device claim, or `topology.tensor_parallel > 1`, on any host with a device domain), `residency_unsupported:host_backed` (`residency: host_backed` on every host — owner check 2).
- Consumes: `DomainPolicy.device`, `DomainMemory::Device` (Task 2).

Measured replacements: the ADR 0014 measured-peak store already replaces the cold figure; extend its record with `host_overhead_bytes` and `parked_device_bytes` only if the store already has a free-form measurement JSON — otherwise the placeholders stay and a follow-up records measurement (note it in the status runbook, Task 15).

- [ ] **Step 1: Write the failing tests**

```rust
// T26/T23: a discrete host derives a device and a system allocation per phase.
#[test]
fn derived_budgets_charge_device_and_system() {
    let mut d = deployment(); // existing fixture with engine_config.memory.request "10GiB", residency deep, devices [gpu0]
    let effective = resolve(&d, &discrete_host()).expect("resolves");
    let domains = |phase: &PhaseFootprint| phase.allocations.iter()
        .map(|a| (a.domain.clone(), a.bytes)).collect::<Vec<_>>();
    assert_eq!(domains(&effective.resources.ready),
        vec![("gpu0".into(), 10 << 30), ("system".into(), ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES)]);
    assert_eq!(domains(&effective.resources.parked),
        vec![("gpu0".into(), PARKED_DEVICE_RESIDUE_PLACEHOLDER_BYTES), ("system".into(), ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES)]);
    d["residency"] = "restart_only".into();
    let stopped = resolve(&d, &discrete_host()).unwrap();
    assert!(stopped.resources.parked.allocations.iter().all(|a| a.bytes == 0));
}

// T26: a unified host is unchanged (one allocation, 2 GiB parked residue).
#[test]
fn unified_derivation_is_unchanged() {
    let effective = resolve(&deployment(), &host()).unwrap();
    assert_eq!(effective.resources.ready.allocations.len(), 1);
    assert_eq!(effective.resources.parked.allocations[0].bytes, PARKED_RESIDUAL_PLACEHOLDER_BYTES);
}

#[test]
fn discrete_refusals_are_typed() {
    let mut explicit = deployment_with_resources("gpu0"); // resources naming only gpu0
    assert!(resolve(&explicit, &discrete_host()).unwrap_err().to_string().contains("missing_system_allocation"));
    explicit = deployment();
    explicit["devices"] = serde_json::json!([{"id": "gpu0", "sharing": "shared"}, {"id": "gpu1", "sharing": "shared"}]);
    assert!(resolve(&explicit, &two_gpu_host()).unwrap_err().to_string().contains("multi_gpu_unsupported"));
    let mut backed = deployment();
    backed["residency"] = "host_backed".into();
    assert!(resolve(&backed, &discrete_host()).unwrap_err().to_string().contains("residency_unsupported:host_backed"));
}
```

Update the two existing tests `a_host_backed_park_resolves_on_a_distinct_domain` and the `("host_backed", true, true, "cpu_backup")` row: they now expect `residency_unsupported:host_backed` (owner check 2). Leave the adapter's `cpu_backup` rendering in place; it becomes unreachable from resolution and is kept for the later slice.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mllm-config --test effective`
Expected: FAIL on the new tests.

- [ ] **Step 3: Implement**

In `derive_resources`, after the domain is found:

```rust
let policy = &host.domains[&domain];
if policy.memory == DomainMemory::Device {
    // ADR 0019: VRAM in the device domain, engine host overhead in host RAM.
    let systems: Vec<&String> = host.domains.iter()
        .filter(|(_, d)| d.memory == DomainMemory::Distinct).map(|(n, _)| n).collect();
    let [system] = systems.as_slice() else {
        return Err(invalid("resource_policy.domains", "missing_system_allocation: a discrete host declares one distinct system domain"));
    };
    let two = |device: i64, host_bytes: i64| PhaseFootprint {
        allocations: vec![
            Allocation { domain: domain.clone(), bytes: device, host_kv_bytes: 0 },
            Allocation { domain: (*system).clone(), bytes: host_bytes, host_kv_bytes: 0 },
        ],
        devices: devices.to_vec(),
    };
    let overhead = ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES;
    let (parked_device, parked_host) = if residency.parks() {
        (PARKED_DEVICE_RESIDUE_PLACEHOLDER_BYTES.min(request), overhead)
    } else {
        (0, 0)
    };
    let cold = startup.map_or(request, |peak| peak.max(request));
    return Ok(RecipeFootprints {
        cold: two(cold, overhead),
        ready: two(request, overhead),
        parking: two(request, overhead),
        parked: PhaseFootprint { devices: vec![], ..two(parked_device, parked_host) },
        wake: two(request, overhead),
    });
}
```

(Match the exact `RecipeFootprints` field set and parked-phase device list used by the unified branch below it.) In `core.rs`, add the three refusal checks before phase resolution; the host-backed check replaces the current unified-only check.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mllm-config -p mllm-store -p mllm-controller --all-targets --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-config
git commit -m "feat: derive device and system budgets on a discrete host"
```

---

### Task 7: Standalone deployment template on a discrete host

**Files:**
- Modify: `crates/mllm-cli/src/standalone_config.rs` (`deployment_document`)
- Modify: the standalone deploy path in `crates/mllm-cli/src/roles.rs` (`App::deploy…`, where the template is filled)
- Test: `crates/mllm-cli/src/standalone_config/tests.rs`, `crates/mllm-cli/tests/standalone_start.rs`

**Interfaces:**
- Consumes: `HostShape` on `App` (Task 3), `device_limits` (Task 3), derived budgets (Task 6), checkpoint weights bytes (the ADR 0014 `CheckpointFacts.weights_bytes` the deploy path already reads).
- Produces:
  - `pub enum TemplateMemory { Unified { capacity_bytes: i64 }, Device { device: String, managed_limit: i64, weights_bytes: i64 } }`
  - `deployment_document(name, route, source, engine, memory: &TemplateMemory, request_deadline, deep_park, profile) -> Result<Value, TemplateError>` with `TemplateError::InsufficientDeviceMemory { request: i64, limit: i64 }` (code `insufficient_device_memory`, exit 4).
  - `pub fn device_request(weights_bytes: i64, managed_limit: i64) -> (i64 /*request*/, i64 /*kv*/)`: `kv = min(4 GiB, managed_limit / 4)`, `request = weights × 110 / 100 + kv`.

- [ ] **Step 1: Write the failing tests**

```rust
// T26/T23: a 4B bf16 model (~8 GiB) on a 16 GB card: request, no fixed shares.
#[test]
fn a_discrete_template_states_a_request_and_derives_phases() {
    let memory = TemplateMemory::Device { device: "gpu0".into(), managed_limit: 15 << 30, weights_bytes: 8 << 30 };
    let doc = deployment_document("a", "a", &source(), Engine::Vllm, &memory, DEFAULT_REQUEST_DEADLINE, true, "local").unwrap();
    assert!(doc.get("resources").is_none());
    let (request, kv) = device_request(8 << 30, 15 << 30);
    assert_eq!(kv, (4i64 << 30).min((15i64 << 30) / 4)); // min(4 GiB, 3.75 GiB)
    assert_eq!(doc["engine_config"]["memory"]["request"], format!("{request}B"));
    assert_eq!(doc["engine_config"]["memory"]["kv_cache"], format!("{kv}B"));
    assert_eq!(doc["devices"][0]["id"], "gpu0");
}

// Review focus 1: a model that cannot fit is refused at deploy with numbers.
#[test]
fn a_model_larger_than_the_device_is_refused() {
    let memory = TemplateMemory::Device { device: "gpu0".into(), managed_limit: 15 << 30, weights_bytes: 16 << 30 };
    let error = deployment_document("a", "a", &source(), Engine::Vllm, &memory, DEFAULT_REQUEST_DEADLINE, true, "local").unwrap_err();
    assert!(matches!(error, TemplateError::InsufficientDeviceMemory { .. }));
    assert!(error.to_string().starts_with("insufficient_device_memory"));
}

// T26: the unified template is byte-identical to before.
#[test]
fn the_unified_template_is_unchanged() { /* compare with a fixture captured on main, as in Task 3 */ }
```

Write the unified test concretely: capture `deployment_document(...)` output on `main` into `fixtures/unified_deployment.json` and assert equality with `TemplateMemory::Unified { capacity_bytes: 128 << 30 }`.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mllm-cli --lib standalone_config`
Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
pub fn device_request(weights_bytes: i64, managed_limit: i64) -> (i64, i64) {
    const GIB: i64 = 1 << 30;
    let kv = (4 * GIB).min(managed_limit / 4);
    (weights_bytes / 100 * 110 + kv, kv)
}
```

In `deployment_document`, for `TemplateMemory::Device`, compute the request; if `request > managed_limit` return the error; otherwise emit the same document as today minus `resources`, with `"devices": [{"id": device, "sharing": "shared"}]` and `"engine_config": {"memory": {"request": "<n>B", "kv_cache": "<kv>B"}}`. The deploy path picks `Device { device: "gpu0", managed_limit: <gpu0 domain limit>, weights_bytes }` when `App.gpu_shape` is `Discrete`, maps the error to the management error code `insufficient_device_memory` (HTTP 409, CLI exit 4) and stores nothing.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p mllm-cli --all-targets --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-cli
git commit -m "feat: size standalone deployments from the checkpoint on a discrete GPU"
```

---

### Task 8: Multi-domain launch check, and planner agreement

**Files:**
- Modify: `crates/mllm-agent/src/native_execution/refusal.rs` (`admit_memory`)
- Test: `crates/mllm-agent/src/native_execution/refusal.rs` tests (or its sibling test module), `crates/mllm-scheduler/src/switching.rs` tests, `crates/mllm-agent/tests/discrete_switch.rs` (new)

**Interfaces:**
- Consumes: `observe_domains`/`GpuSampler` (Task 4), two-allocation footprints (Task 6).
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
fn the_unified_launch_check_is_unchanged() { /* existing assertions, now through admit_memory_with with gpu = None */ }
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

(The helpers `discrete_limits`, `ready_footprint`, `cold_footprint`, `parked_footprint`, `ledger_with`, `victim`, `gpu`, `ram`, `discrete_effective` are defined at the top of the test file with the literal numbers above; `gpu(total_mib, used_mib)` builds a `GpuSample` through `parse_query_gpu`.)

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

### Task 9: Engine sizing on a discrete device

**Files:**
- Modify: `crates/mllm-adapters/src/vllm/args.rs` (`--gpu-memory-utilization` not rendered on a device domain)
- Modify: `crates/mllm-adapters/src/sglang/args.rs` (launch spec gains `device_total_bytes`)
- Modify: `runtime/sglang_launch_spec.py`, `runtime/sglang_server_args.py` (`static_fraction` baseline)
- Test: `crates/mllm-adapters/tests/vllm_args.rs`, `crates/mllm-adapters/tests/sglang_args.rs`, `runtime/tests/test_sglang_server_args.py`

**Interfaces:**
- Consumes: `DomainPolicy.device` (Task 2), device total from the GPU sample (Task 4) — the agent fills `device_total_bytes` in the launch input from the same sample the launch check used (Task 8).
- Produces: `SglangLaunchSettings.memory.device_total_bytes: Option<i64>` rendered as `"device_total_bytes"` in the entry's closed settings only when `Some`; Python `available_bytes_for(spec) -> int` returning `spec["device_total_bytes"]` when present, else `available_memory_bytes()`.

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
// crates/mllm-adapters/tests/vllm_args.rs — T26: explicit KV bytes only on a discrete device.
#[test]
fn a_discrete_launch_has_kv_bytes_and_no_utilization() {
    let mut input = launch_input(); // existing builder
    input.granted.gpu_utilization_pct = None;
    input.granted.kv_cache_bytes = Some(4 << 30);
    let argv = render(&input).unwrap().argv;
    assert!(argv.windows(2).any(|w| w == ["--kv-cache-memory-bytes", &(4i64 << 30).to_string()]));
    assert!(!argv.iter().any(|a| a == "--gpu-memory-utilization"));
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

Replace the `available_memory_bytes()` call feeding `static_fraction` (near line 347) with `available_bytes_for(spec)`, and allow the key in `sglang_launch_spec.py`'s closed schema (int, optional). In Rust, add the optional field to the settings struct and serialize it only when `Some`. For vLLM, the grant resolution (where `gpu_utilization_pct` is set) leaves it `None` when the allocation's domain is a device domain; check that it is already `None` whenever `kv_cache_bytes` is set, and only add a test if so.

- [ ] **Step 4: Run to verify they pass**

Run: `python3 -m unittest discover -s runtime/tests; cargo test -p mllm-adapters --all-targets --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add runtime crates/mllm-adapters crates/mllm-agent
git commit -m "feat: size SGLang against the device total on a discrete GPU"
```

---

### Task 10: Remote hosts report device domains under a capability

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
- Consumes: Tasks 1, 2, 4, 5.

Record the chosen field numbers in the ADR (Task 13).

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

### Task 11: Inference listener bind: default, `--listen`, existing documents

Depends on owner checks 1 and 5.

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

// T03 / owner check 1: an existing loopback document keeps loopback.
#[test]
fn an_existing_loopback_document_stays_on_loopback() {
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

In `standalone_start.rs`, a start with an old loopback document prints the hint `inference is on 127.0.0.1:8443 (loopback only); use --listen 0.0.0.0:8443 to serve other machines` once. For the server: `ServerConfig::parse` accepts `listeners.inference.bind: "0.0.0.0:8443"` and the template states it; `management` stays loopback-forced.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mllm-config -p mllm-cli --all-targets listen bind generated`
Expected: FAIL.

- [ ] **Step 3: Implement**

- `standalone.rs`: split the bind check — `management` must equal its loopback default as today; `inference` goes through `inference_bind`. Update the module doc comment (the "loopback" wording) and the refusal message.
- `defaults.rs`: the generated `inference.bind` becomes `"0.0.0.0:8443"`.
- `remote_roles.rs`: `listener(&v, "inference", "api_key", false)` and the template's inference bind `"0.0.0.0:8443"`; update the authentication check in Task 12.
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

### Task 12: Inference authentication opt-out and warning

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
- Consumes: `effective_inference_address` (Task 11).

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

### Task 13: ADR 0019 and SPEC amendments

**Files:**
- Create: `docs/design/adr/0019-discrete-gpu-and-network-endpoint.md`
- Modify: `docs/SPEC.md` §7.2, §13.3, §15.2, §16.2, §16.5, §20 (T26, T37), §21 revision history
- Modify: `AGENTS.md` "Authoritative documents" item 2 (ADR list mentions 0019)

**Interfaces:** none (documentation). Content is taken from the spec; it must state the proto field numbers chosen in Task 10.

- [ ] **Step 1: Write the ADR**

Sections, in the style of ADR 0017/0018: Status (`Accepted (owner decisions A and B, 2026-09-25)`), Amends (the SPEC sections above), Related (ADR 0007, 0010, 0012, 0013, 0014, 0017), Context (spec "Problem" condensed), Decision (numbered: 1 detection via `nvidia-smi`; 2 `memory: device` domain and its rules; 3 two-domain derived budgets and placeholders; 4 observation, residents, launch check; 5 deep park frees device memory, host-backed refused `residency_unsupported:host_backed`; 6 one GPU per model, explicit device id, `multi_gpu_unsupported`; 7 capability `device_memory_domains` with field numbers; 8 inference bind `0.0.0.0:8443`, `--listen`, existing documents unchanged; 9 `authentication: none` / `--no-inference-auth` with the warning; 10 management and engines unchanged; 11 error codes table), Consequences, Open issues (automatic GPU choice; host-backed tier; measured host overhead).

- [ ] **Step 2: Amend SPEC**

Add under each amended section a line `> **Amended by [ADR 0019](design/adr/0019-discrete-gpu-and-network-endpoint.md)** (owner decision 2026-09-25).` followed by the new normative text:
- §7.2: "On a discrete-GPU host each GPU's memory is a `device` domain observed from the device; host RAM is a `distinct` system domain. A deployment's derived budget charges both."
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

### Task 14: Operator documentation

**Files:**
- Create: `docs/examples/host-discrete.yaml` (parseable host document for one 24 GB card)
- Create: `docs/operations/network-access.md`
- Modify: `docs/operations/install.md` (discrete GPU section, link to network access), `docs/README.md` (index)
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

- [ ] **Step 4: Update `install.md`** with a "Discrete NVIDIA GPU" section: requirements (`nvidia-smi` on the path), what standalone detects, the two domains, one GPU per model, `devices: [{id: gpu1}]` for a second card, what `insufficient_device_memory` means.

- [ ] **Step 5: Run and commit**

Run: `cargo test --workspace --all-targets --locked the_discrete_host_example_validates`
Expected: PASS.

```bash
git add docs/examples/host-discrete.yaml docs/operations docs/README.md crates
git commit -m "docs: discrete GPU hosts and network access to the inference endpoint"
```

---

### Task 15: Live rows DG1–DG6 on the 16 GB discrete-GPU laptop host

Depends on owner check 4. CPU and Fake-engine tests are not qualification; these rows are.

**Files:**
- Create: `scripts/live/matrix/discrete_gpu.sh`
- Modify: `scripts/live/matrix/hosts.example.env` (a `DGPU_HOST` placeholder), `scripts/live/matrix/README.md`
- Modify: `AGENTS.md` "Hard constraints" (the owner's exception, wording confirmed by the owner)
- Modify: `docs/runbooks/f2-current-status.md` (results, in place)

**Interfaces:**
- Consumes: every earlier task; the host name and venv paths only from the untracked `hosts.local.env`.

- [ ] **Step 1: Record the exception in `AGENTS.md`** (after owner confirmation):

```markdown
- Owner exception (2026-09-25): live discrete-GPU work is authorized on a 16 GB
  discrete-GPU laptop host (named only in `hosts.local.env`), including creating
  one vLLM and one SGLang virtual environment there (x86_64 wheels, versions
  matching the lab hosts). No other environment or driver changes.
```

- [ ] **Step 2: Prepare the host** (operator step, from `hosts.local.env`): create the two venvs at the paths named there (`python3 -m venv "$DGPU_VLLM_VENV" && "$DGPU_VLLM_VENV/bin/pip" install vllm==0.29.0`; the same for `sglang[all]==0.5.20` in `$DGPU_SGLANG_VENV`); download the two models into the model store (a 4B instruct model, e.g. Qwen3-4B-Instruct, and a 3B instruct model, bf16). Register both engines: `mllm engine add "$DGPU_VLLM_VENV"` and `mllm engine add "$DGPU_SGLANG_VENV"`.

- [ ] **Step 3: Write `discrete_gpu.sh`** following the existing matrix scripts' structure (source `hosts.local.env`, `set -euo pipefail`, trap cleanup that stops only mllm-owned deployments, one function per row, results appended to a local log that is not tracked). Rows:
  - `dg1_vllm_switch`: deploy A and B on vLLM; request A; request B (expect A parked in `status`, `nvidia-smi` process for A ≤ 1.5 GiB); request A (expect B parked); check `mllm status` shows `gpu0` charges.
  - `dg2_sglang_switch`: the same on SGLang.
  - `dg3_mixed_switch`: A on vLLM, B on SGLang, both directions.
  - `dg4_refusal`: deploy a model whose derived request exceeds the device limit; expect exit 4 and `insufficient_device_memory`, no engine process started, within 30 s.
  - `dg5_network`: from a lab host over the tailnet: `/v1/models` with the key → 200, without → 401; restart with `--listen 127.0.0.1:8443` → connection refused from the peer; restart with `--no-inference-auth` on `0.0.0.0` → warning present in the log.
  - `dg6_remote_host` (optional): run `mllm start host` on the laptop host against the control-plane server; `mllm list hosts` shows `gpu0`; deploy and switch A/B.
  - Regression: rerun the existing unified switching row on one GB10 lab host.

- [ ] **Step 4: Run and record**

Run: `scripts/live/matrix/discrete_gpu.sh all`
Expected: DG1–DG5 pass; DG6 passes or is recorded pending. Update `docs/runbooks/f2-current-status.md` in place: rows, commit range, measured parked residue and engine host RSS (to replace the placeholders later), and the explicit statement that CPU/Fake tests are not qualification.

- [ ] **Step 5: Commit**

```bash
git add scripts/live/matrix AGENTS.md docs/runbooks/f2-current-status.md
git commit -m "test: live discrete-GPU switching and network access rows"
```

---

## Self-review notes

- Spec coverage: §1 → T1; §2 → T2, T3; §3 → T6, T7; §4 → T4, T5, T8; §5 → T6 (host-backed), T8 (parked contexts); §6 → T9; §7 → T2, T3, T6; §8 → T10; §9 → T11, T12; §10 → T12, T13; §11 → T6, T7, T8, T10, T12; §12 → tests in every task, live T15; ADR → T13; docs → T14.
- Names used across tasks: `HostShape`, `GpuSample`, `GpuDevice`, `GpuMemory`, `parse_query_gpu`, `DomainMemory::Device`, `DomainPolicy.device`, `device_limits`, `ObservedDomain`, `observe_domains`, `ProcessResident.device_bytes/host_bytes`, `ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES`, `PARKED_DEVICE_RESIDUE_PLACEHOLDER_BYTES`, `TemplateMemory`, `device_request`, `admit_memory_with`, `DEVICE_MEMORY_DOMAINS`, `check_device_policy`, `inference_bind`, `effective_inference_address`, `InferenceAuth`, `exposure_warning`.

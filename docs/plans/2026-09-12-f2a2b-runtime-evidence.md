# F2A2b Runtime Observation and Completion Evidence Implementation Plan

**Goal:** Provide real Linux host memory observations and an engine-neutral validator for completion evidence, so runtime coordination cannot confuse an acknowledgement, memory sample, or surviving API server with a verified warm transition.

**Architecture:** Add a strict procfs reader to the agent and a pure completion validator directly to the domain crate. Controller re-exports the shared contract. Bind evidence to the exact deployment revision, lifecycle generation, step, recipe qualification, and complete retained process set. The later coordinator transaction consumes validated completion only after rechecking current durable ownership; these helpers do not change reservations or readiness themselves.

**Tech Stack:** Existing Rust 2021 workspace, standard library, existing thiserror and domain/scheduler dependencies. No engine installation, new GPU dependency, or public service.

**Spec:** [Approved F2 design](../design/milestones/f2-sglang-design.md), §§3–6 and Q2/Q5/Q6/Q8/Q11. Dependencies: [F2A1](2026-09-12-f2a1-resource-contracts-and-admission.md) and [F2A2a](2026-09-12-f2a2a-durable-reservation-transactions.md).

## Execution status

Implemented through `0f5a576`. Task reviews, whole-branch review, and scoped fix
review complete. Fresh full check at `60fd33a`: 194 CPU tests and scoped Clippy
pass. Formatting-only fix then passes focused memory tests and direct rustfmt
check. CLI integration targets excluded. No production cutover or live qualification.

## Global Constraints

- “Only host-a is authorized for subsequent live work.” No live work occurs in this plan.
- “Retaining only the API server PID is insufficient.”
- “Warm residency preserves initialized runtime processes, not necessarily weight contents or KV contents.”
- “A low usage sample does not authorize shrinking a qualified peak reservation.”
- “Unknown engine work does not qualify as safe quiescence for parking.”
- “No blind repetition of a possibly applied engine operation is allowed.”
- “Only verified release/cleanup evidence permits reuse.”
- Preserve the unrelated `crates/mllm-cli/tests/live_interactive.rs`; do not edit, stage, or execute it.
- This plan does not connect new helpers to the existing synthetic production admission path. That cutover must be atomic with runtime coordination.

---

## 1. Source findings and frozen protocol decisions

### Host memory

Read `/proc/meminfo` once per sample. `MemTotal` is usable physical RAM, not
marketing-installed capacity; `MemAvailable` estimates headroom for applications
without swapping. Do not add GPU-labelled bytes as another domain on the selected
unified-memory topology. A discrete GPU requires its own observer and physical
domain; this reader does not manufacture one. [Linux procfs documentation](https://docs.kernel.org/filesystems/proc.html)

Treat the sample as an independent pressure check, not owner attribution or proof
of release. Preserve reservations derived from qualified recipes. Report observed
usage separately and label partial counters honestly. Unknown or contradictory
samples block increasing transitions. This parser has no fallback from missing
`MemAvailable` to `MemTotal` or `MemFree`.

### Pinned vLLM reference

The repository's F1 records identify vLLM 0.29.0. For source inspection, its upstream
tag resolves to commit `98dff2a81d747d1dba01a47f939f48c3526d4206`. This is a source
reference, not proof that an installed wheel matches it; qualification must compare
the installed build and recipe fingerprints.

- The sleep route defaults to `mode=abort`, accepts `mode`, and returns an empty
  successful response after the engine call completes. F2's selected non-aborting
  protocol must explicitly use `POST /sleep?level=2&mode=wait`.
  [Pinned sleep router](https://github.com/vllm-project/vllm/blob/98dff2a81d747d1dba01a47f939f48c3526d4206/vllm/entrypoints/serve/dev/sleep/api_router.py)
- The process-based engine's wait mode drains before executing sleep. The in-process
  variant rejects wait mode, so engine topology is part of qualification.
  `is_sleeping` also includes scheduler pause; it does not identify a completed deep
  release. Do not resolve an uncertain deep-park call from that boolean alone.
  [Pinned engine core](https://github.com/vllm-project/vllm/blob/98dff2a81d747d1dba01a47f939f48c3526d4206/vllm/v1/engine/core.py)
- Worker sleep saves some buffers on CPU and logs changes in device-wide free memory.
  Those logs are not a complete per-deployment retained-byte report, especially with
  another active engine. Remove use of the adapter's simulator value of 256 bytes
  from any production release decision.
  [Pinned worker](https://github.com/vllm-project/vllm/blob/98dff2a81d747d1dba01a47f939f48c3526d4206/vllm/v1/worker/gpu_worker.py)
- A successful wake allocation is not proof of usable weights. Preserve the existing
  repository's reload and cache-reset regression, then require a qualified model
  probe before readiness. These are completion milestones, not universal SGLang
  endpoint names or vLLM sleep levels in the common interface.

### Completion evidence

For a normal, acknowledged transition, combine the exact qualified protocol's
successful control milestones with unchanged verified runtime identities and its
qualified retained/serving bounds. A bound is an estimate backed by qualification,
not a claim that memory was measured exactly at completion. Fresh host observations
remain required before the next allocation increase.

An uncertain or missing control acknowledgement retains the transition reservation
and closes admission. It does not become success merely because the endpoint listens,
the adapter remembers a flag, the engine reports sleeping, or free memory increases.
The reconciliation implementation must use a separately qualified observation protocol
or report a bounded failure with resources retained. It cannot silently repeat sleep,
reload, or restore. Administrative cleanup remains a separate authorized operation.

The validator below checks **trusted collector output**. It does not authenticate
engine responses or prove that arbitrary caller-created evidence is true. It is not
a management API deserializer. Production integration must keep its producer inside
the qualified adapter/host path and revalidate its token under the store transaction.

## 2. File map and boundaries

| File | Responsibility |
|---|---|
| Create `crates/mllm-agent/src/memory.rs` | Strict host observation parser and bounded procfs read |
| Modify `crates/mllm-agent/src/lib.rs` | Export memory module |
| Modify `crates/mllm-agent/Cargo.toml` | Add existing workspace domain dependency |
| Create `crates/mllm-agent/tests/memory.rs` | Missing/invalid/overflow/contradictory sample cases |
| Create `crates/mllm-domain/src/completion.rs` | Typed transition identity, completion milestones, pure evidence validation |
| Modify `crates/mllm-domain/src/lib.rs` | Export completion module |
| Modify `crates/mllm-controller/src/lib.rs` | Re-export `mllm_domain::completion` |
| Create `crates/mllm-domain/tests/completion.rs` | Token, runtime identity, deadline and milestone regressions |

No resource capacity defaults change. The coordinator enforces the F2 production
2-second observation TTL and resamples before each increasing step; the parser does
not choose TTL. Clock rollback/future timestamps must fail at the admission gate.
The small millisecond TTL values in pure completion fixtures are test inputs,
not production defaults. Native meminfo supplies no per-owner `ResidentFloor`:
unknown attribution gives zero credit. Global free-memory changes never establish
owner residency. Qualified floors require verified ownership and the same coherent
availability observation. Conservative admission rejection is safer than invented
credit; synthetic forecast floors never enter real admission.
Restricted containers/cgroups require an additional effective-limit observer before
their samples can authorize work; this native-host parser is not such an observer.

### Task 1: Read available memory without inventing capacity

**Interfaces:** Produces `HostMemorySample`, `MemoryReadError`,
`parse_meminfo(input: &str, sampled_at_ms: i64) -> Result<HostMemorySample, MemoryReadError>`,
and `read_host_memory() -> Result<HostMemorySample, MemoryReadError>`.
`sample.memory` has the F2A1 `MemoryObservation` type. Swap usage is separate telemetry,
never extra managed capacity.

**Files:** Agent files from the file map.

- [ ] Create the integration test below and run
  `cargo test -p mllm-agent --test memory available_is_not_total`.
  Expected RED: unresolved `memory` module.

```rust
use mllm_agent::memory::parse_meminfo;

#[test]
fn available_is_not_total() {
    let sample = parse_meminfo(
        "MemTotal: 128 kB\nMemAvailable: 40 kB\nSwapTotal: 8 kB\nSwapFree: 6 kB\n", 100).unwrap();
    assert_eq!(sample.memory.domain, "system");
    assert_eq!(sample.memory.capacity_bytes, 128 * 1024);
    assert_eq!(sample.memory.available_bytes, 40 * 1024);
    assert_eq!(sample.memory.sampled_at_ms, 100);
    assert_eq!(sample.swap_used_bytes, 2 * 1024);
}
```

- [ ] Add `mllm-domain = { workspace = true }` to the agent's dependencies and
  `pub mod memory;` to its library module exports. Create `memory.rs`:

```rust
use std::collections::BTreeMap;
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};
use mllm_domain::resources::MemoryObservation;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostMemorySample {
    pub memory: MemoryObservation,
    pub swap_used_bytes: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum MemoryReadError {
    #[error("invalid or incomplete host memory observation")]
    Invalid,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub fn parse_meminfo(input: &str, sampled_at_ms: i64)
    -> Result<HostMemorySample, MemoryReadError> {
    if sampled_at_ms < 0 || input.len() > 65_536 { return Err(MemoryReadError::Invalid); }
    let mut fields = BTreeMap::new();
    for line in input.lines() {
        let Some((name, value)) = line.split_once(':') else { continue; };
        if !matches!(name, "MemTotal" | "MemAvailable" | "SwapTotal" | "SwapFree") { continue; }
        let words: Vec<_> = value.split_whitespace().collect();
        if words.len() != 2 || words[1] != "kB" { return Err(MemoryReadError::Invalid); }
        let kib: i64 = words[0].parse().map_err(|_| MemoryReadError::Invalid)?;
        if kib < 0 { return Err(MemoryReadError::Invalid); }
        let bytes = kib.checked_mul(1024).ok_or(MemoryReadError::Invalid)?;
        if fields.insert(name, bytes).is_some() { return Err(MemoryReadError::Invalid); }
    }
    let get = |name| fields.get(name).copied().ok_or(MemoryReadError::Invalid);
    let total = get("MemTotal")?;
    let available = get("MemAvailable")?;
    let swap_total = get("SwapTotal")?;
    let swap_free = get("SwapFree")?;
    if total == 0 || available > total || swap_free > swap_total { return Err(MemoryReadError::Invalid); }
    Ok(HostMemorySample {
        memory: MemoryObservation { domain: "system".into(), capacity_bytes: total,
            available_bytes: available, sampled_at_ms },
        swap_used_bytes: swap_total - swap_free,
    })
}

pub fn read_host_memory() -> Result<HostMemorySample, MemoryReadError> {
    // Timestamp before reading: a delayed read cannot make old data look newer.
    let sampled_at_ms = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)
        .map_err(|_| MemoryReadError::Invalid)?.as_millis()).map_err(|_| MemoryReadError::Invalid)?;
    let mut input = String::new();
    std::fs::File::open("/proc/meminfo")?.take(65_537).read_to_string(&mut input)?;
    parse_meminfo(&input, sampled_at_ms)
}
```

- [ ] Add the following boundary test. It reads fixture strings only, not the host.

```rust
#[test]
fn bad_samples_fail_closed() {
    let valid = "MemTotal: 128 kB\nMemAvailable: 40 kB\nSwapTotal: 8 kB\nSwapFree: 6 kB\n";
    for invalid in [
        valid.replace("MemAvailable: 40 kB\n", ""),
        valid.replace("MemAvailable: 40", "MemAvailable: 129"),
        valid.replace("MemTotal: 128", "MemTotal: 0"),
        valid.replace("MemAvailable: 40", "MemAvailable: -1"),
        valid.replace("MemAvailable: 40 kB", "MemAvailable: 40 MB"),
        valid.replace("MemTotal: 128", "MemTotal: 9223372036854775807"),
        valid.replace("SwapFree: 6", "SwapFree: 9"),
        format!("{valid}MemTotal: 128 kB\n"),
        "x".repeat(65_537),
    ] {
        assert!(parse_meminfo(&invalid, 100).is_err(), "accepted {invalid:?}");
    }
    assert!(parse_meminfo(valid, -1).is_err());
}
```

- [ ] Run `cargo test -p mllm-agent`. Expect PASS.
- [ ] Commit only the task files:

```bash
git add crates/mllm-agent/src/memory.rs crates/mllm-agent/src/lib.rs crates/mllm-agent/Cargo.toml crates/mllm-agent/tests/memory.rs
git commit -m "feat(agent): report usable and available host memory separately"
```

### Task 2: Bind completion to a qualified step and the full runtime identity

**Interfaces:** Produces the types and `verify_completion` function below. A
`VerifiedCompletion` is only a checked value; it cannot directly change readiness or
the ledger. Its constructor remains private. The next coordinator integration will
consume it under revision/generation/step/qualification checks in one transaction.

**Files:** Domain completion files and controller re-export from the file map.

- [ ] Create `tests/completion.rs` with this fixture and failing test.

```rust
use mllm_domain::completion::*;
use mllm_domain::resources::*;

fn fixture() -> (CompletionExpectation, CompletionEvidence) {
    let token = TransitionToken { deployment_id: "a".into(), revision: 1,
        generation: 7, operation_id: "op-a".into(), step_id: "park-a-1".into(),
        qualification_id: "qualified-recipe-a".into() };
    let identities = vec![
        ProcessIdentity { role: "api".into(), pid: 100, boot_id: "boot-a".into(), start_ticks: 30 },
        ProcessIdentity { role: "worker-0".into(), pid: 101, boot_id: "boot-a".into(), start_ticks: 31 },
    ];
    let target = PhaseFootprint { phase: ResourcePhase::Parked,
        allocations: vec![Allocation { domain: "system".into(), bytes: 8, host_kv_bytes: 0 }],
        devices: vec![] };
    let expected = CompletionExpectation { token: token.clone(), identities: identities.clone(),
        target, issued_at_ms: 100, deadline_ms: 200 };
    let evidence = CompletionEvidence { token, identities, observed_at_ms: 150,
        control_receipt: Some("ack-park-a-1".into()),
        milestones: vec![Milestone::Quiesced, Milestone::MemoryReleased] };
    (expected, evidence)
}

#[test]
fn qualified_ack_and_unchanged_workers_can_complete_parking() {
    let (expected, evidence) = fixture();
    let verified = verify_completion(&expected, &evidence, 151, 60).unwrap();
    assert_eq!(verified.token(), &expected.token);
    assert_eq!(verified.target(), &expected.target);
}
```

- [ ] Run `cargo test -p mllm-domain --test completion qualified_ack`.
  Expected RED: unresolved completion module.
- [ ] Export `pub mod completion;` from domain and `pub use mllm_domain::completion;`
  from controller. Create domain `completion.rs`; no scheduler dependency:

```rust
use std::collections::BTreeSet;
use crate::resources::{validate_footprint, PhaseFootprint, ResourcePhase};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionToken {
    pub deployment_id: String,
    pub revision: i64,
    pub generation: i64,
    pub operation_id: String,
    pub step_id: String,
    pub qualification_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProcessIdentity {
    pub role: String,
    pub pid: u32,
    pub boot_id: String,
    pub start_ticks: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Milestone {
    Quiesced,
    MemoryReleased,
    AllocationsRestored,
    WeightsUsable,
    CacheValid,
    ModelUsable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionExpectation {
    pub token: TransitionToken,
    pub identities: Vec<ProcessIdentity>,
    pub target: PhaseFootprint,
    pub issued_at_ms: i64,
    pub deadline_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionEvidence {
    pub token: TransitionToken,
    pub identities: Vec<ProcessIdentity>,
    pub observed_at_ms: i64,
    pub control_receipt: Option<String>,
    pub milestones: Vec<Milestone>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedCompletion {
    token: TransitionToken,
    target: PhaseFootprint,
    evidence: CompletionEvidence,
    observed_at_ms: i64,
    valid_until_ms: i64,
}
impl VerifiedCompletion {
    pub fn token(&self) -> &TransitionToken { &self.token }
    pub fn target(&self) -> &PhaseFootprint { &self.target }
    pub fn evidence(&self) -> &CompletionEvidence { &self.evidence }
    pub fn observed_at_ms(&self) -> i64 { self.observed_at_ms }
    pub fn valid_until_ms(&self) -> i64 { self.valid_until_ms }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CompletionError {
    #[error("invalid completion expectation")]
    Invalid,
    #[error("completion token is stale or mismatched")]
    StaleToken,
    #[error("runtime process identity changed or is incomplete")]
    RuntimeChanged,
    #[error("completion evidence is outside its time bounds")]
    Expired,
    #[error("completion milestones or control acknowledgement are missing")]
    Incomplete,
}

fn identities_valid(identities: &[ProcessIdentity]) -> bool {
    let mut roles = BTreeSet::new();
    let mut processes = BTreeSet::new();
    identities.iter().any(|identity| identity.role == "api")
        && identities.iter().any(|identity| identity.role.starts_with("worker-") && identity.role.len() > 7)
        && identities.iter().all(|identity| {
        !identity.role.is_empty() && identity.pid > 0 && !identity.boot_id.is_empty()
            && identity.boot_id == identities[0].boot_id
            && roles.insert(identity.role.as_str())
            && processes.insert((identity.boot_id.as_str(), identity.pid))
    })
}

pub fn verify_completion(expected: &CompletionExpectation, evidence: &CompletionEvidence,
    now_ms: i64, ttl_ms: i64) -> Result<VerifiedCompletion, CompletionError> {
    let token = &expected.token;
    if token.deployment_id.is_empty() || token.operation_id.is_empty()
        || token.step_id.is_empty() || token.qualification_id.is_empty()
        || token.revision < 1 || token.generation < 1 || expected.issued_at_ms < 0
        || expected.deadline_ms < expected.issued_at_ms || ttl_ms <= 0
        || validate_footprint(&expected.target).is_err() {
        return Err(CompletionError::Invalid);
    }
    if token != &evidence.token { return Err(CompletionError::StaleToken); }
    if !identities_valid(&expected.identities) || !identities_valid(&evidence.identities)
        || expected.identities.iter().collect::<BTreeSet<_>>()
            != evidence.identities.iter().collect::<BTreeSet<_>>() {
        return Err(CompletionError::RuntimeChanged);
    }
    let expiry = evidence.observed_at_ms.checked_add(ttl_ms)
        .ok_or(CompletionError::Expired)?.min(expected.deadline_ms);
    if evidence.observed_at_ms < expected.issued_at_ms || now_ms < evidence.observed_at_ms
        || now_ms > expiry { return Err(CompletionError::Expired); }
    if evidence.control_receipt.as_ref().is_none_or(|receipt| receipt.is_empty()) {
        return Err(CompletionError::Incomplete);
    }
    let required: &[Milestone] = match expected.target.phase {
        ResourcePhase::Parked => &[Milestone::Quiesced, Milestone::MemoryReleased],
        ResourcePhase::Ready => &[Milestone::AllocationsRestored, Milestone::WeightsUsable,
            Milestone::CacheValid, Milestone::ModelUsable],
        _ => return Err(CompletionError::Invalid),
    };
    if evidence.milestones.as_slice() != required {
        return Err(CompletionError::Incomplete);
    }
    Ok(VerifiedCompletion { token: token.clone(), target: expected.target.clone(), evidence: evidence.clone(),
        observed_at_ms: evidence.observed_at_ms, valid_until_ms: expiry })
}
```

For cold initialization, `AllocationsRestored` means that the requested allocation
state is established, not that a warm wake occurred. Record cold versus warm mode in
the durable step, not by inferring it from these shared completion milestones.
Cold completion uses process identities established and recorded after spawning;
warm completion uses the retained identities from before the transition.
The collector labels the API process `api` and retained workers `worker-<rank>`;
additional owned helper processes have distinct roles. This selected multi-process
contract rejects API-only evidence. Milestones must appear in the required order;
an adapter can map one qualified composite acknowledgement to several ordered
milestones, but cannot treat a probe performed before cache restoration as final proof.

- [ ] Run `cargo test -p mllm-domain --test completion`. Expect PASS.
- [ ] Commit the task files only:

```bash
git add crates/mllm-domain/src/completion.rs crates/mllm-domain/src/lib.rs crates/mllm-domain/tests/completion.rs crates/mllm-controller/src/lib.rs
git commit -m "feat(domain): validate generation-bound runtime completion evidence"
```

### Task 3: Reject stale acknowledgements, worker replacement, and incomplete restore

**Interfaces:** No new production API. These tests define the rejection boundary
the durable coordinator must preserve.

**Files:** `crates/mllm-domain/tests/completion.rs`.

- [ ] Append these tests and run `cargo test -p mllm-domain --test completion`.
  Expected PASS; these add boundary coverage, not a claimed RED/GREEN cycle.

```rust
#[test]
fn stale_tokens_do_not_release_resources() {
    let (expected, evidence) = fixture();
    let mut invalid = Vec::new();
    let mut changed = evidence.clone(); changed.token.revision += 1; invalid.push(changed);
    let mut changed = evidence.clone(); changed.token.generation += 1; invalid.push(changed);
    let mut changed = evidence.clone(); changed.token.step_id.push('x'); invalid.push(changed);
    let mut changed = evidence.clone(); changed.token.operation_id.push('x'); invalid.push(changed);
    let mut changed = evidence.clone(); changed.token.deployment_id.push('x'); invalid.push(changed);
    let mut changed = evidence; changed.token.qualification_id.push('x'); invalid.push(changed);
    for changed in invalid {
        assert_eq!(verify_completion(&expected, &changed, 151, 60), Err(CompletionError::StaleToken));
    }
}

#[test]
fn api_survival_does_not_hide_worker_loss_or_pid_reuse() {
    let (expected, evidence) = fixture();
    let mut missing = evidence.clone(); missing.identities.pop();
    assert_eq!(verify_completion(&expected, &missing, 151, 60), Err(CompletionError::RuntimeChanged));
    let mut reused = evidence.clone(); reused.identities[1].start_ticks += 1;
    assert_eq!(verify_completion(&expected, &reused, 151, 60), Err(CompletionError::RuntimeChanged));
    let mut rebooted = evidence.clone(); rebooted.identities[1].boot_id.push('x');
    assert_eq!(verify_completion(&expected, &rebooted, 151, 60), Err(CompletionError::RuntimeChanged));
    let mut reordered = evidence.clone(); reordered.identities.reverse();
    assert!(verify_completion(&expected, &reordered, 151, 60).is_ok());
    let mut duplicate = evidence; duplicate.identities.push(duplicate.identities[0].clone());
    assert_eq!(verify_completion(&expected, &duplicate, 151, 60), Err(CompletionError::RuntimeChanged));
    let mut api_only_expected = expected.clone(); api_only_expected.identities.truncate(1);
    duplicate.identities.truncate(1);
    assert_eq!(verify_completion(&api_only_expected, &duplicate, 151, 60), Err(CompletionError::RuntimeChanged));
}

#[test]
fn lost_ack_and_expired_evidence_stay_uncertain() {
    let (expected, evidence) = fixture();
    let mut lost = evidence.clone(); lost.control_receipt = None;
    assert_eq!(verify_completion(&expected, &lost, 151, 60), Err(CompletionError::Incomplete));
    assert_eq!(verify_completion(&expected, &evidence, 149, 60), Err(CompletionError::Expired));
    assert_eq!(verify_completion(&expected, &evidence, 201, 60), Err(CompletionError::Expired));
    assert_eq!(verify_completion(&expected, &evidence, 161, 10), Err(CompletionError::Expired));
    let mut old = evidence; old.observed_at_ms = 99;
    assert_eq!(verify_completion(&expected, &old, 151, 60), Err(CompletionError::Expired));
}

#[test]
fn every_restore_milestone_is_required_before_ready() {
    let (mut expected, mut evidence) = fixture();
    expected.target.phase = ResourcePhase::Ready;
    evidence.milestones = vec![Milestone::AllocationsRestored, Milestone::WeightsUsable,
        Milestone::CacheValid, Milestone::ModelUsable];
    assert!(verify_completion(&expected, &evidence, 151, 60).is_ok());
    for index in 0..evidence.milestones.len() {
        let mut missing = evidence.clone(); missing.milestones.remove(index);
        assert_eq!(verify_completion(&expected, &missing, 151, 60), Err(CompletionError::Incomplete));
    }
    let mut duplicate = evidence; duplicate.milestones.push(Milestone::ModelUsable);
    assert_eq!(verify_completion(&expected, &duplicate, 151, 60), Err(CompletionError::Incomplete));
    duplicate.milestones.pop();
    duplicate.milestones.swap(2, 3);
    assert_eq!(verify_completion(&expected, &duplicate, 151, 60), Err(CompletionError::Incomplete));
}
```

- [ ] Run `cargo test -p mllm-agent -p mllm-domain -p mllm-controller`.
- [ ] Run `cargo clippy -p mllm-agent -p mllm-domain -p mllm-controller --all-targets -- -D warnings`.
- [ ] Run `git diff --check`. Confirm no engine call, reservation reduction,
  readiness mutation, remote command, or production database migration occurred.
- [ ] Commit only the task file:

```bash
git add crates/mllm-domain/tests/completion.rs
git commit -m "test(domain): reject incomplete and stale runtime completion"
```

## 3. Remaining runtime-coordination integration

This plan is an executable prerequisite, not the complete F2A2 coordinator. The next
plan must implement the durable step machine and routing integration using these
interfaces. Freeze the following requirements there before implementation:

The [F2A2c dispatch-ownership plan](2026-09-12-f2a2c-durable-dispatch-ownership.md)
now defines atomic dispatch closure, request leases, and coordinator-session fences.
The [planning index](2026-09-12-f2-planning-index.md) records the remaining cutover boundaries.

1. A single controller authority accepts administrative and routed activation.
   Join requests by deployment and current revision/generation; do not hold a global
   lock during inference or an engine HTTP wait.
2. Close dispatch and register in-flight work under the same generation gate.
   Disconnect, task cancellation, and guard destruction do not establish backend
   completion. Unknown work keeps admission closed until qualified drain evidence.
3. Persist the affected deployment claims, resource grant, planned action, and step
   identity before issuing an engine control. An uncertain dispatched step is never
   automatically replayed after crash or lost acknowledgement.
4. Persist complete process/worker ownership and private endpoint allocation. Retain
   endpoint ownership while parked or uncertain. Use verified boot/start identities,
   not only a process tree snapshot or API PID.
5. Validate completion against the current durable step and the pinned target
   footprint; never accept a caller-selected smaller footprint. Recheck evidence
   expiry, qualification, revision, generation, and ownership in the same transaction
   that replaces the peak reservation and changes lifecycle state.
6. Recheck fresh host observations and ledger epoch before every increasing action.
   Credit releases only after step completion commits. A stale grant receipt is
   historical evidence, not permission to execute again.
7. Preserve retained runtimes during policy-driven preparation and switching. Report
   partial preparation and impossible wake paths. Do not reclaim by cold stop unless
   an explicit policy or administrative action authorizes it.
8. On local restart, close gates and reconcile surviving runtimes, unknown work,
   endpoints, and legacy reservations before accepting new work. No automatic
   reservation/endpoint release from expired leases or failed operation state.

Qualification must implement real collectors for every milestone and establish the
recipe's retained-memory bound. The tests above validate supplied evidence; they do
not qualify vLLM or SGLang, measure memory release, or demonstrate live warm switching.

# F2A1 Resource Contracts and Admission Kernel Implementation Plan

**Execution status:** CPU-only slice implemented and reviewed through `5c2cbbe`.
Fresh verification: 174 tests pass across the workspace excluding CLI integration
and the CLI library; scoped Clippy passes. This does not qualify live engines or
close F2. Task instructions below remain the implementation contract.

**Goal:** Build a deterministic, engine-neutral resource kernel that can prove whether a cold start, parking transition, or warm wake fits alongside retained deployments, and can produce a revision-bound reservation proposal.

**Architecture:** Add typed physical-domain and phase-footprint contracts to the domain crate and pure admission/sequence evaluation to the scheduler crate. Keep physical observations separate from reservations. Return proposals bound to the input ledger epoch; the subsequent durable-coordination plan must commit them atomically before any engine action.

**Tech Stack:** Existing Rust 2021 workspace, standard collections, existing `thiserror` and `proptest` dependencies. No new runtime dependency, engine package, service, or GPU library.

**Spec:** [Approved F2 design](../../design/milestones/f2-sglang-design.md), especially §§3–5 and Q2/Q6. Read it and this plan before execution.

## Global Constraints

- “Only host-a is authorized for subsequent live work.” This plan is entirely CPU-only and authorizes no live work.
- “Ordinary switching must not require a cold process restart.”
- “Warm residency preserves initialized runtime processes, not necessarily weight contents or KV contents.”
- “Unknown recipes require an explicit conservative estimate or controlled qualification under a safe reservation; they are never launched without a bound to discover their size.”
- “On unified-memory hardware, CPU/GPU allocations share one physical domain.”
- “A low usage sample does not authorize shrinking a qualified peak reservation.”
- “Overlap is allowed only when the participating assignments permit sharing and aggregate budgets fit.”
- “Resource observations and operation generations are revalidated before each increasing step; a changed condition blocks or replans safely.”
- “Do not silently stop a parked deployment, shrink model settings, or exceed headroom.”
- “Client disconnect is not proof of engine cancellation.”
- Preserve the untracked `crates/mllm-cli/tests/live_interactive.rs`; do not stage, edit, or run it.
- No implicit downloads, engine startup, checkpoint changes, production database migrations, or public listeners.

---

## 1. Plan boundaries and delivery order

F2 remains three stages: shared single-host foundation, SGLang integration, mixed-engine qualification. The foundation is divided into bounded plans:

| Plan | Deliverable | Dependency |
|---|---|---|
| **F2A1 — this document** | Pure resource contracts, admission and sequence evaluator, revision-bound proposals, deterministic tests | Approved F2 spec |
| F2A2 — durable runtime coordination | Real host observations, persistent contracts and reservation CAS, per-deployment runtimes/ports/credentials, verified release, lifecycle arbitration, warm preparation/switching, recovery, router integration | F2A1 interfaces and tests |
| F2A3 — management and observability | Strict runnable configuration, API/CLI, updates, attachment, snapshots/events, timings, supported actions, operator tests | F2A2 operations |
| F2B — SGLang | Pinned engine qualification and adapter using the same controller and resource contracts | Shared foundation |
| F2C — mixed-engine proof | Live concurrent serving, pressure-driven warm switching, bursts, streams and recovery on host-a | Both adapters and product API/CLI |

Only F2A1 is implementation-ready in this document. The other plans must be written
against the resulting interfaces before their execution. This is not a complete F2
implementation plan or permission to omit those deliverables.

The persistence portion of F2A2 is now specified separately in
[F2A2a durable reservation transactions](2026-09-12-f2a2a-durable-reservation-transactions.md).
The [F2A2b runtime-evidence plan](2026-09-12-f2a2b-runtime-evidence.md) defines the
initial observation and completion-validation interfaces. The
[F2A2d coordinator plan](2026-09-12-f2a2d-coordinator-integration.md) owns durable
coordination and routing cutover.

F2A1 deliberately does **not** connect the new kernel to engine execution. The current
controller's synthetic admission and the old reservation writer must be removed
together with durable coordination, not partly replaced by an in-memory check.
No live-capability claim follows from this plan's passing tests.

## 2. Baseline and file map

Baseline code is `0efd718`; `b1faeb0` added the design without implementation changes.
Existing scheduler files implement the F0/F1 behavior: `admission.rs` conflates capacity
and available observations, treats device overlap as exclusive, and accepts caller-
supplied reservations. `ledger.rs` has activation/ready/parked states only. Preserve
those exports for existing callers while adding the new, complete kernel. F2A2 must
switch production callers and remove the legacy path; do not advertise both as
independent scheduling authorities.

| File | Responsibility |
|---|---|
| Create `crates/mllm-domain/src/resources.rs` | Physical-domain values, resident lower bounds, phase footprints, validation, active device claims |
| Modify `crates/mllm-domain/src/lib.rs` | Export `resources` |
| Create `crates/mllm-scheduler/src/residency.rs` | Validation reexports, phase replacement admission, and revision-bound proposal |
| Create `crates/mllm-scheduler/src/sequence.rs` | Evaluate an ordered sequence without applying or crediting real releases |
| Modify `crates/mllm-scheduler/src/lib.rs` | Export the two new modules |
| Modify `crates/mllm-scheduler/Cargo.toml` | Add existing workspace `proptest` as a dev dependency |
| Create `crates/mllm-scheduler/tests/resource_admission.rs` | Table-driven admission and policy tests |
| Create `crates/mllm-scheduler/tests/resource_sequences.rs` | Intermediate peaks and warm-arrangement feasibility |
| Create `crates/mllm-scheduler/tests/resource_properties.rs` | Monotonicity, checked arithmetic, no double charge, epoch races |
| Create `docs/design/adr/0007-phase-aware-resource-admission.md` | Accounting meanings and integration prerequisites, including headroom and sharing |

Use test-local helpers only where shown. Do not refactor the large controller file,
add a generic plugin framework, change numerical defaults, or build a second store
in this plan.

## 3. Frozen accounting semantics

- All byte fields are signed `i64` at the boundary to match the existing SQLite/Rust
  contract; negative values are invalid. Checked arithmetic overflow blocks admission.
- `capacity_bytes` is usable physical capacity. `available_bytes` is a conservative
  observation of allocatable capacity at the observation time. They are different
  values and cannot substitute for one another.
- Every allocation has one owner and one physical domain. Host-KV bytes are a subset
  of its total bytes, not another physical charge. Duplicate owner/domain rows are
  rejected; shared service owners must be normalized by the caller before evaluation.
- Limits and observations must name identical known domains. Unknown, future-dated,
  stale, or inconsistent observations block before capacity diagnostics.
- Candidate phase bytes replace that owner's existing bytes on each domain. All
  other owners remain charged. A transition footprint includes the retained bytes
  that physically overlap it.
- For domain `d`, require `after[d] <= managed_limit[d]` and
  `after[d] <= capacity[d] - free_reserve[d]`. The second inequality is not another
  subtraction from the managed ceiling.
- Physical allocation credit is a verified resident lower bound, never a reservation
  ceiling. Without qualified owner attribution, credit is zero. For each domain,
  require available memory minus the candidate's unallocated commitment and every
  other owner's unallocated commitment to remain above protected headroom.
  Each commitment is `max(reservation_after - resident_floor, 0)`. Resident floors
  must be reflected in the same availability observation and bound to the current
  runtime identity by the collector. Matching timestamps alone do not establish this.
- This kernel cannot infer precise external usage or detect a deployment exceeding
  its reservation from host availability alone. F2A2 must reject degraded/unreconciled
  snapshots before calling it and must compare measured owner usage to reservations.
- Device claims are active leases, not a declaration that a parked process has no GPU
  context. Parked footprints retain memory but hold no active compute lease. Ready,
  cold, park-transition and wake footprints carry the applicable claims. A lease may
  be removed only after verified parking/cleanup in the durable coordinator.
- A shared active claim conflicts with an overlapping exclusive active claim;
  two shared claims do not conflict by identity alone. Other physical limits still apply.
- A proposal is not authorization to launch. It names the expected epoch, full owner
  replacement, phase, and observation expiry. F2A2 must compare-and-swap that epoch and
  validate deployment revision/generation and current observations under its durable
  transition authority before accepting it. No automatic retry using stale evidence.
- Sequence evaluation is a **forecast**. It may forecast released memory to assess
  feasibility, but it never updates observations, reservations, or permissions on a
  host. Runtime execution rechecks every step and credits only verified releases.

### Task 1: Define physical-domain and phase contracts

**Files:** Create `crates/mllm-domain/src/resources.rs`; modify `crates/mllm-domain/src/lib.rs`.

**Interfaces:** Produces `MemoryObservation`, `MemoryLimit`, `Sharing`, `DeviceClaim`,
`ResidentFloor`, `Allocation`, `ResourcePhase`, `PhaseFootprint`, `RecipeFootprints`, and
`LedgerSnapshot` below. Existing identity/lifecycle types remain unchanged.

- [ ] Add this failing unit test to the new module, export the module, and run
  `cargo test -p mllm-domain phase_names_include_transient_parking`.
  Expected RED: missing `ResourcePhase` type, not an unrelated workspace failure.

```rust
#[test]
fn phase_names_include_transient_parking() {
    assert_ne!(ResourcePhase::Parking, ResourcePhase::Parked);
    assert_ne!(ResourcePhase::Cold, ResourcePhase::Wake);
}
```

- [ ] Add the contract types. These are internal typed interfaces, not a promise
  that the public YAML/API schemas already accept these field names.

```rust
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryObservation {
    pub domain: String,
    pub capacity_bytes: i64,
    pub available_bytes: i64,
    pub sampled_at_ms: i64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResidentFloor {
    pub owner: String,
    pub domain: String,
    pub bytes: i64,
    pub sampled_at_ms: i64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryLimit {
    pub domain: String,
    pub managed_bytes: i64,
    pub free_reserve_bytes: i64,
    pub host_kv_bytes: Option<i64>,
    pub parked_bytes: Option<i64>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sharing { Shared, Exclusive }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceClaim { pub device: String, pub sharing: Sharing }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allocation {
    pub domain: String,
    pub bytes: i64,
    pub host_kv_bytes: i64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourcePhase { Cold, Ready, Parking, Parked, Wake }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseFootprint {
    pub phase: ResourcePhase,
    pub allocations: Vec<Allocation>,
    pub devices: Vec<DeviceClaim>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeFootprints {
    pub cold: PhaseFootprint,
    pub ready: PhaseFootprint,
    pub parking: PhaseFootprint,
    pub parked: PhaseFootprint,
    pub wake: PhaseFootprint,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerSnapshot {
    pub epoch: u64,
    pub owners: BTreeMap<String, PhaseFootprint>,
}
```

- [ ] Run `cargo test -p mllm-domain`; expect all tests PASS.
- [ ] Commit only the two named files with
  `git commit -m "feat(domain): distinguish resource phases and physical observations"`.

### Task 2: Validate budgets and active device sharing

**Files:** Create `crates/mllm-scheduler/src/residency.rs` and
`crates/mllm-scheduler/tests/resource_admission.rs`; modify `crates/mllm-scheduler/src/lib.rs`
and `crates/mllm-domain/src/resources.rs`.

**Interfaces:** Consumes Task 1 types. Produces the following error and functions:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ResourceError {
    #[error("invalid resource contract")] Invalid,
    #[error("unknown physical domain")] UnknownDomain,
    #[error("stale resource observation")] StaleObservation,
    #[error("device assignment conflict")] DeviceConflict,
    #[error("insufficient resources")] Insufficient,
    #[error("resource category limit exceeded")] CategoryLimit,
    #[error("stale ledger epoch")] StaleEpoch,
}
// Definitions use the imported mllm_domain::resources types.
pub fn validate_footprint(f: &PhaseFootprint) -> Result<(), ResourceError>;
pub fn validate_recipe(r: &RecipeFootprints) -> Result<(), ResourceError>;
pub fn claims_conflict(a: &[DeviceClaim], b: &[DeviceClaim]) -> bool;
```

- [ ] Write and run this failing integration test:
  `cargo test -p mllm-scheduler --test resource_admission shared_claims_can_overlap`.
  Expected RED: unresolved new module/function.

```rust
use mllm_domain::resources::*;
use mllm_scheduler::residency::*;

#[test]
fn shared_claims_can_overlap() {
    let shared = DeviceClaim { device: "gpu:0".into(), sharing: Sharing::Shared };
    let exclusive = DeviceClaim { device: "gpu:0".into(), sharing: Sharing::Exclusive };
    assert!(!claims_conflict(std::slice::from_ref(&shared), std::slice::from_ref(&shared)));
    assert!(claims_conflict(std::slice::from_ref(&shared), std::slice::from_ref(&exclusive)));
    assert!(claims_conflict(&[exclusive], &[shared]));
}
```

- [ ] Define `ResourceError`, validation, and conflict detection in domain's
  `resources.rs`, importing `std::collections::BTreeSet`. Scheduler's `residency.rs`
  reexports those four names. Add imports only as callers need them. Completion validation
  can then depend on domain alone; domain must never depend on scheduler.

```rust
pub fn validate_footprint(f: &PhaseFootprint) -> Result<(), ResourceError> {
    let mut domains = BTreeSet::new();
    let mut devices = BTreeSet::new();
    if f.allocations.is_empty() { return Err(ResourceError::Invalid); }
    for a in &f.allocations {
        if a.domain.is_empty() || !domains.insert(&a.domain)
            || a.bytes < 0 || a.host_kv_bytes < 0 || a.host_kv_bytes > a.bytes {
            return Err(ResourceError::Invalid);
        }
    }
    for d in &f.devices {
        if d.device.is_empty() || !devices.insert(&d.device) {
            return Err(ResourceError::Invalid);
        }
    }
    if f.phase == ResourcePhase::Parked && !f.devices.is_empty() {
        return Err(ResourceError::Invalid);
    }
    Ok(())
}

pub fn claims_conflict(a: &[DeviceClaim], b: &[DeviceClaim]) -> bool {
    a.iter().any(|x| b.iter().any(|y| x.device == y.device &&
        (x.sharing == Sharing::Exclusive || y.sharing == Sharing::Exclusive)))
}

pub fn validate_recipe(r: &RecipeFootprints) -> Result<(), ResourceError> {
    for (f, expected) in [(&r.cold, ResourcePhase::Cold),
        (&r.ready, ResourcePhase::Ready), (&r.parking, ResourcePhase::Parking),
        (&r.parked, ResourcePhase::Parked), (&r.wake, ResourcePhase::Wake)] {
        validate_footprint(f)?;
        if f.phase != expected { return Err(ResourceError::Invalid); }
    }
    let domain_set = |f: &PhaseFootprint| f.allocations.iter()
        .map(|a| a.domain.clone()).collect::<BTreeSet<_>>();
    let domains = domain_set(&r.ready);
    for f in [&r.cold, &r.parking, &r.parked, &r.wake] {
        if domain_set(f) != domains { return Err(ResourceError::Invalid); }
    }
    for (peak, base) in [(&r.cold, &r.ready), (&r.parking, &r.ready),
        (&r.parking, &r.parked), (&r.wake, &r.parked), (&r.wake, &r.ready)] {
        for b in &base.allocations {
            let p = peak.allocations.iter().find(|p| p.domain == b.domain)
                .ok_or(ResourceError::Invalid)?;
            if p.bytes < b.bytes || p.host_kv_bytes < b.host_kv_bytes {
                return Err(ResourceError::Invalid);
            }
        }
    }
    Ok(())
}
```

- [ ] Add this negative-input test before adjusting any failed rule, then rerun
  `cargo test -p mllm-scheduler --test resource_admission`.

```rust
#[test]
fn invalid_and_duplicate_allocations_are_rejected() {
    let a = Allocation { domain: "system".into(), bytes: 10, host_kv_bytes: 11 };
    let mut f = PhaseFootprint {
        phase: ResourcePhase::Ready, allocations: vec![a], devices: vec![],
    };
    assert_eq!(validate_footprint(&f), Err(ResourceError::Invalid));
    f.allocations[0].host_kv_bytes = 0;
    f.allocations.push(f.allocations[0].clone());
    assert_eq!(validate_footprint(&f), Err(ResourceError::Invalid));
    f.allocations.pop();
    f.allocations[0].bytes = -1;
    assert_eq!(validate_footprint(&f), Err(ResourceError::Invalid));
}
```

- [ ] Run `cargo test -p mllm-scheduler`; expect PASS without altering old tests.
- [ ] Keep imports warning-free at this task boundary; later tasks expand the
  integration test imports when adding admission. Commit the four named files with
  `git commit -m "feat(scheduler): validate phase budgets and shared device claims"`.

### Task 3: Admit complete owner replacements against fresh observations

**Files:** Modify `crates/mllm-scheduler/src/residency.rs` and
`crates/mllm-scheduler/tests/resource_admission.rs`.

**Interfaces:** Consumes Task 1/2 types. Produces `admit_phase`, with all inputs
explicit and no reads, writes, environment lookup, or engine calls:

```rust
#[derive(Debug, Clone, Copy)]
pub struct AdmissionContext<'a> {
    pub observations: &'a [MemoryObservation],
    pub resident_floors: &'a [ResidentFloor],
    pub limits: &'a [MemoryLimit],
    pub now_ms: i64,
    pub ttl_ms: i64,
    pub max_parked: usize,
}
impl<'a> AdmissionContext<'a> {
    pub fn new(observations: &'a [MemoryObservation], limits: &'a [MemoryLimit],
        now_ms: i64, ttl_ms: i64, max_parked: usize) -> Self {
        Self { observations, resident_floors: &[], limits, now_ms, ttl_ms, max_parked }
    }
    pub fn with_resident_floors(mut self, floors: &'a [ResidentFloor]) -> Self {
        self.resident_floors = floors;
        self
    }
}
pub fn admit_phase(
    snapshot: &LedgerSnapshot, owner: &str, next: &PhaseFootprint,
    context: AdmissionContext<'_>,
) -> Result<(), ResourceError>;
```

- [ ] Add this failing test and run
  `cargo test -p mllm-scheduler --test resource_admission wake_replaces_parked_residue`.

```rust
#[test]
fn wake_replaces_parked_residue() {
    let footprint = |phase, bytes| PhaseFootprint {
        phase, allocations: vec![Allocation {
            domain: "system".into(), bytes, host_kv_bytes: 0,
        }], devices: vec![],
    };
    let snapshot = LedgerSnapshot { epoch: 7, owners: [
        ("a".into(), footprint(ResourcePhase::Ready, 50)),
        ("b".into(), footprint(ResourcePhase::Parked, 10)),
    ].into() };
    let observations = [MemoryObservation {
        domain: "system".into(), capacity_bytes: 128, available_bytes: 60,
        sampled_at_ms: 1_000,
    }];
    let limits = [MemoryLimit {
        domain: "system".into(), managed_bytes: 96, free_reserve_bytes: 12,
        host_kv_bytes: None, parked_bytes: None,
    }];
    let floors = [("a", 50), ("b", 10)].map(|(owner, bytes)| ResidentFloor {
        owner: owner.into(), domain: "system".into(), bytes, sampled_at_ms: 1_000,
    });
    assert_eq!(admit_phase(&snapshot, "b", &footprint(ResourcePhase::Wake, 46),
        AdmissionContext::new(&observations, &limits, 1_001, 2_000, 4).with_resident_floors(&floors)), Ok(()));
    assert_eq!(admit_phase(&snapshot, "b", &footprint(ResourcePhase::Wake, 47),
        AdmissionContext::new(&observations, &limits, 1_001, 2_000, 4).with_resident_floors(&floors)), Err(ResourceError::Insufficient));
}
```

- [ ] Add the `AdmissionContext` definition and constructor from the Interfaces block,
  then implement the complete admission calculation below. It intentionally makes
  no decision about whom to park, no inference-readiness decision, and no physical
  release claim. Namespace helper functions privately inside this module.

```rust
fn add(a: i64, b: i64) -> Result<i64, ResourceError> {
    a.checked_add(b).ok_or(ResourceError::Invalid)
}
fn amount(f: &PhaseFootprint, domain: &str) -> (i64, i64) {
    f.allocations.iter().find(|a| a.domain == domain)
        .map(|a| (a.bytes, a.host_kv_bytes)).unwrap_or((0, 0))
}

pub fn admit_phase(
    snapshot: &LedgerSnapshot, owner: &str, next: &PhaseFootprint,
    context: AdmissionContext<'_>,
) -> Result<(), ResourceError> {
    let AdmissionContext { observations, resident_floors, limits, now_ms, ttl_ms, max_parked } = context;
    if owner.is_empty() || ttl_ms <= 0 || now_ms < 0 || limits.is_empty() {
        return Err(ResourceError::Invalid);
    }
    validate_footprint(next)?;
    for (id, f) in &snapshot.owners {
        if id.is_empty() { return Err(ResourceError::Invalid); }
        validate_footprint(f)?;
    }
    let mut known = BTreeSet::new();
    for l in limits {
        if l.domain.is_empty() || !known.insert(l.domain.as_str())
            || l.managed_bytes < 0 || l.free_reserve_bytes < 0
            || l.host_kv_bytes.is_some_and(|x| x < 0)
            || l.parked_bytes.is_some_and(|x| x < 0) {
            return Err(ResourceError::Invalid);
        }
    }
    let mut observed = BTreeSet::new();
    for o in observations {
        if !observed.insert(o.domain.as_str()) { return Err(ResourceError::Invalid); }
        if !known.contains(o.domain.as_str()) { return Err(ResourceError::UnknownDomain); }
        if o.capacity_bytes < 0 || o.available_bytes < 0
            || o.available_bytes > o.capacity_bytes || o.sampled_at_ms < 0 {
            return Err(ResourceError::Invalid);
        }
        let age = now_ms.checked_sub(o.sampled_at_ms).ok_or(ResourceError::Invalid)?;
        if age < 0 || age > ttl_ms { return Err(ResourceError::StaleObservation); }
    }
    if known != observed { return Err(ResourceError::UnknownDomain); }
    let mut credited = BTreeSet::new();
    for floor in resident_floors {
        let existing = snapshot.owners.get(&floor.owner).ok_or(ResourceError::Invalid)?;
        let observation = observations.iter().find(|o| o.domain == floor.domain)
            .ok_or(ResourceError::UnknownDomain)?;
        if !credited.insert((&floor.owner, &floor.domain)) || floor.bytes < 0
            || floor.bytes > amount(existing, &floor.domain).0
            || floor.sampled_at_ms != observation.sampled_at_ms {
            return Err(ResourceError::Invalid);
        }
    }
    for f in snapshot.owners.values().chain(std::iter::once(next)) {
        if f.allocations.iter().any(|a| !known.contains(a.domain.as_str())) {
            return Err(ResourceError::UnknownDomain);
        }
    }
    let others = snapshot.owners.iter().filter(|(id, _)| id.as_str() != owner);
    if others.clone().any(|(_, f)| claims_conflict(&f.devices, &next.devices)) {
        return Err(ResourceError::DeviceConflict);
    }
    let parked_count = others.clone().filter(|(_, f)| f.phase == ResourcePhase::Parked)
        .count() + usize::from(next.phase == ResourcePhase::Parked);
    if parked_count > max_parked { return Err(ResourceError::CategoryLimit); }
    for l in limits {
        let o = observations.iter().find(|o| o.domain == l.domain)
            .ok_or(ResourceError::UnknownDomain)?;
        let (candidate, candidate_kv) = amount(next, &l.domain);
        let floor = |id: &str| resident_floors.iter()
            .find(|f| f.owner == id && f.domain == l.domain).map(|f| f.bytes).unwrap_or(0);
        let resident_total = resident_floors.iter().filter(|f| f.domain == l.domain)
            .try_fold(0, |sum, f| add(sum, f.bytes))?;
        if resident_total > o.capacity_bytes - o.available_bytes {
            return Err(ResourceError::Invalid);
        }
        let mut remaining = candidate.checked_sub(floor(owner)).ok_or(ResourceError::Invalid)?.max(0);
        let mut total = candidate;
        let mut kv = candidate_kv;
        let mut parked = if next.phase == ResourcePhase::Parked { candidate } else { 0 };
        for (id, f) in others.clone() {
            let (bytes, host_kv) = amount(f, &l.domain);
            remaining = add(remaining, bytes.checked_sub(floor(id)).ok_or(ResourceError::Invalid)?.max(0))?;
            total = add(total, bytes)?;
            kv = add(kv, host_kv)?;
            if f.phase == ResourcePhase::Parked { parked = add(parked, bytes)?; }
        }
        let ceiling = o.capacity_bytes.checked_sub(l.free_reserve_bytes)
            .ok_or(ResourceError::Invalid)?;
        if total > l.managed_bytes || total > ceiling {
            return Err(ResourceError::Insufficient);
        }
        if l.host_kv_bytes.is_some_and(|x| kv > x)
            || l.parked_bytes.is_some_and(|x| parked > x) {
            return Err(ResourceError::CategoryLimit);
        }
        if o.available_bytes.checked_sub(remaining).ok_or(ResourceError::Invalid)?
            < l.free_reserve_bytes {
            return Err(ResourceError::Insufficient);
        }
    }
    Ok(())
}
```

- [ ] Add and run the following observation regression, then the full integration test.

- [ ] Add these reservation-credit regressions. Equal reservation ceilings cannot
  authorize a wake when actual parked residency is small. Other owners' unused
  reservations remain outstanding commitments. Invalid attribution blocks admission.

```rust
#[test]
fn reservation_slack_is_not_physical_credit() {
    let f = |phase, bytes| PhaseFootprint { phase,
        allocations: vec![Allocation { domain: "system".into(), bytes, host_kv_bytes: 0 }],
        devices: vec![] };
    let state = LedgerSnapshot { epoch: 1, owners: [
        ("a".into(), f(ResourcePhase::Parked, 48)),
    ].into() };
    let obs = [MemoryObservation { domain: "system".into(), capacity_bytes: 128,
        available_bytes: 32, sampled_at_ms: 100 }];
    let limits = [MemoryLimit { domain: "system".into(), managed_bytes: 96,
        free_reserve_bytes: 16, host_kv_bytes: None, parked_bytes: None }];
    let floors = [ResidentFloor { owner: "a".into(), domain: "system".into(),
        bytes: 8, sampled_at_ms: 100 }];
    for context in [AdmissionContext::new(&obs, &limits, 101, 60, 4),
        AdmissionContext::new(&obs, &limits, 101, 60, 4).with_resident_floors(&floors)] {
        assert_eq!(admit_phase(&state, "a", &f(ResourcePhase::Wake, 48), context),
            Err(ResourceError::Insufficient));
        assert_eq!(admit_phase(&state, "b", &f(ResourcePhase::Cold, 1), context),
            Err(ResourceError::Insufficient));
    }
    for bad in [ResidentFloor { sampled_at_ms: 99, ..floors[0].clone() },
        ResidentFloor { bytes: 49, ..floors[0].clone() },
        ResidentFloor { bytes: -1, ..floors[0].clone() }] {
        assert_eq!(admit_phase(&state, "a", &f(ResourcePhase::Wake, 48),
            AdmissionContext::new(&obs, &limits, 101, 60, 4)
                .with_resident_floors(&[bad])), Err(ResourceError::Invalid));
    }
    assert_eq!(admit_phase(&state, "a", &f(ResourcePhase::Wake, 48),
        AdmissionContext::new(&obs, &limits, 101, 60, 4)
            .with_resident_floors(&[floors[0].clone(), floors[0].clone()])),
        Err(ResourceError::Invalid));
}
```

```rust
#[test]
fn installed_capacity_is_not_available_memory() {
    let next = PhaseFootprint { phase: ResourcePhase::Cold,
        allocations: vec![Allocation { domain: "system".into(), bytes: 30, host_kv_bytes: 0 }],
        devices: vec![] };
    let snapshot = LedgerSnapshot { epoch: 0, owners: Default::default() };
    let mut obs = [MemoryObservation { domain: "system".into(), capacity_bytes: 128,
        available_bytes: 20, sampled_at_ms: 100 }];
    let limits = [MemoryLimit { domain: "system".into(), managed_bytes: 96,
        free_reserve_bytes: 12, host_kv_bytes: None, parked_bytes: None }];
    assert_eq!(admit_phase(&snapshot, "a", &next, AdmissionContext::new(&obs, &limits, 101, 60, 4)),
        Err(ResourceError::Insufficient));
    obs[0].sampled_at_ms = 0;
    assert_eq!(admit_phase(&snapshot, "a", &next, AdmissionContext::new(&obs, &limits, 101, 60, 4)),
        Err(ResourceError::StaleObservation));
}
```

- [ ] Run `cargo test -p mllm-scheduler`; expect PASS.
- [ ] Commit the two files with
  `git commit -m "feat(scheduler): account for retained owners and transition growth"`.

### Task 4: Bind reservation proposals to ledger epoch and observation expiry

**Files:** Modify `crates/mllm-scheduler/src/residency.rs` and
`crates/mllm-scheduler/tests/resource_admission.rs`.

**Interfaces:** Produces `ReservationProposal`, `propose_phase`, and
`apply_proposal_to_snapshot`. The last function is pure and is not a durable commit.
All proposal fields are private; consumers inspect through getters. Construction
must pass Task 3 admission first.

- [ ] Add the epoch race test below and run
  `cargo test -p mllm-scheduler --test resource_admission two_proposals_cannot_spend_one_epoch`.
  Expected RED: new functions unresolved.

```rust
#[test]
fn two_proposals_cannot_spend_one_epoch() {
    let snapshot = LedgerSnapshot { epoch: 4, owners: Default::default() };
    let next = PhaseFootprint { phase: ResourcePhase::Cold,
        allocations: vec![Allocation { domain: "system".into(), bytes: 60, host_kv_bytes: 0 }],
        devices: vec![] };
    let obs = [MemoryObservation { domain: "system".into(), capacity_bytes: 128,
        available_bytes: 128, sampled_at_ms: 100 }];
    let limits = [MemoryLimit { domain: "system".into(), managed_bytes: 96,
        free_reserve_bytes: 12, host_kv_bytes: None, parked_bytes: None }];
    let a = propose_phase(&snapshot, "a", &next, AdmissionContext::new(&obs, &limits, 101, 60, 4)).unwrap();
    let b = propose_phase(&snapshot, "b", &next, AdmissionContext::new(&obs, &limits, 101, 60, 4)).unwrap();
    let updated = apply_proposal_to_snapshot(&snapshot, &a, 102).unwrap();
    assert_eq!(apply_proposal_to_snapshot(&updated, &b, 102), Err(ResourceError::StaleEpoch));
    assert_eq!(apply_proposal_to_snapshot(&snapshot, &a, 161), Err(ResourceError::StaleObservation));
}
```

- [ ] Implement the proposal, preserving all owner allocations in its replacement.
  F2A2 must validate policy/revision/ownership in addition to these epoch checks.

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationProposal {
    expected_epoch: u64,
    owner: String,
    replacement: PhaseFootprint,
    valid_until_ms: i64,
}
impl ReservationProposal {
    pub fn expected_epoch(&self) -> u64 { self.expected_epoch }
    pub fn owner(&self) -> &str { &self.owner }
    pub fn replacement(&self) -> &PhaseFootprint { &self.replacement }
    pub fn valid_until_ms(&self) -> i64 { self.valid_until_ms }
}
pub fn propose_phase(
    snapshot: &LedgerSnapshot, owner: &str, next: &PhaseFootprint,
    context: AdmissionContext<'_>,
) -> Result<ReservationProposal, ResourceError> {
    admit_phase(snapshot, owner, next, context)?;
    let earliest = context.observations.iter().map(|o| o.sampled_at_ms).min()
        .ok_or(ResourceError::UnknownDomain)?;
    let valid_until_ms = earliest.checked_add(context.ttl_ms).ok_or(ResourceError::Invalid)?;
    Ok(ReservationProposal { expected_epoch: snapshot.epoch, owner: owner.into(),
        replacement: next.clone(), valid_until_ms })
}
pub fn apply_proposal_to_snapshot(
    snapshot: &LedgerSnapshot, proposal: &ReservationProposal, now_ms: i64,
) -> Result<LedgerSnapshot, ResourceError> {
    if snapshot.epoch != proposal.expected_epoch { return Err(ResourceError::StaleEpoch); }
    if now_ms < 0 || now_ms > proposal.valid_until_ms {
        return Err(ResourceError::StaleObservation);
    }
    let mut next = snapshot.clone();
    next.epoch = next.epoch.checked_add(1).ok_or(ResourceError::Invalid)?;
    next.owners.insert(proposal.owner.clone(), proposal.replacement.clone());
    Ok(next)
}
```

- [ ] Run `cargo test -p mllm-scheduler --test resource_admission`; expect PASS.
- [ ] Commit these two files with
  `git commit -m "feat(scheduler): bind resource proposals to ledger epochs"`.

### Task 5: Forecast complete cold/park/wake sequences

**Files:** Create `crates/mllm-scheduler/src/sequence.rs` and
`crates/mllm-scheduler/tests/resource_sequences.rs`; modify `crates/mllm-scheduler/src/lib.rs`.

**Interfaces:** Produces `ForecastStep { owner: String, footprint: PhaseFootprint }`,
`SequenceFailure { step: usize, reason: ResourceError }`, and `forecast_sequence` below.
The caller supplies an ordered sequence; this kernel does not choose victims or
perform a combinatorial search. F2A2 owns policy-constrained sequence construction.

- [ ] Write this failing test. Run
  `cargo test -p mllm-scheduler --test resource_sequences park_before_wake_fits_without_cold_restart`.

```rust
use mllm_domain::resources::*;
use mllm_scheduler::{residency::{AdmissionContext, ResourceError}, sequence::*};

fn f(phase: ResourcePhase, bytes: i64) -> PhaseFootprint {
    PhaseFootprint { phase, allocations: vec![Allocation {
        domain: "system".into(), bytes, host_kv_bytes: 0,
    }], devices: vec![] }
}
#[test]
fn park_before_wake_fits_without_cold_restart() {
    let initial = LedgerSnapshot { epoch: 1, owners: [
        ("a".into(), f(ResourcePhase::Ready, 60)),
        ("b".into(), f(ResourcePhase::Parked, 10)),
    ].into() };
    let obs = [MemoryObservation { domain: "system".into(), capacity_bytes: 128,
        available_bytes: 58, sampled_at_ms: 100 }];
    let limits = [MemoryLimit { domain: "system".into(), managed_bytes: 96,
        free_reserve_bytes: 12, host_kv_bytes: None, parked_bytes: None }];
    let wake = ForecastStep { owner: "b".into(), footprint: f(ResourcePhase::Wake, 80) };
    let floors = [("a", 60), ("b", 10)].map(|(owner, bytes)| ResidentFloor {
        owner: owner.into(), domain: "system".into(), bytes, sampled_at_ms: 100,
    });
    let context = AdmissionContext::new(&obs, &limits, 101, 60, 4).with_resident_floors(&floors);
    assert_eq!(forecast_sequence(&initial, std::slice::from_ref(&wake), context)
        .unwrap_err().reason, ResourceError::Insufficient);
    let steps = [
        ForecastStep { owner: "a".into(), footprint: f(ResourcePhase::Parking, 65) },
        ForecastStep { owner: "a".into(), footprint: f(ResourcePhase::Parked, 8) },
        wake,
        ForecastStep { owner: "b".into(), footprint: f(ResourcePhase::Ready, 60) },
    ];
    let result = forecast_sequence(&initial, &steps, context).unwrap();
    assert_eq!(result.owners["a"].phase, ResourcePhase::Parked);
    assert_eq!(result.owners["b"].phase, ResourcePhase::Ready);
    assert_eq!(initial.epoch, 1); // Forecast cannot mutate the source ledger.
    let mut impossible = steps.clone();
    impossible[0].footprint = f(ResourcePhase::Parking, 100);
    assert_eq!(forecast_sequence(&initial, &impossible, context)
        .unwrap_err().step, 0); // A parking peak itself can be infeasible.
}
```

- [ ] Implement the pure forecast. Forecasted free memory is explicitly separate
  from observed free memory. The durable coordinator must never feed forecast values
  to a real admission check as if the host had measured them.
  Forecast floors are synthetic accounting credits for bytes already charged in
  that forecast. They are not measured residency or evidence of a physical release.

```rust
use mllm_domain::resources::*;
use crate::residency::{admit_phase, AdmissionContext, ResourceError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForecastStep { pub owner: String, pub footprint: PhaseFootprint }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceFailure { pub step: usize, pub reason: ResourceError }

pub fn forecast_sequence(
    initial: &LedgerSnapshot, steps: &[ForecastStep],
    context: AdmissionContext<'_>,
) -> Result<LedgerSnapshot, SequenceFailure> {
    let mut state = initial.clone();
    let mut forecast = context.observations.to_vec();
    let mut floors = context.resident_floors.to_vec();
    for (index, step) in steps.iter().enumerate() {
        let fail = |reason| SequenceFailure { step: index, reason };
        admit_phase(&state, &step.owner, &step.footprint,
            AdmissionContext { observations: &forecast, resident_floors: &floors, ..context }).map_err(fail)?;
        for observation in &mut forecast {
            let get = |f: &PhaseFootprint| f.allocations.iter()
                .find(|a| a.domain == observation.domain).map(|a| a.bytes).unwrap_or(0);
            let before = floors.iter().find(|f| f.owner == step.owner && f.domain == observation.domain)
                .map(|f| f.bytes).unwrap_or(0);
            let after = get(&step.footprint);
            observation.available_bytes = observation.available_bytes
                .checked_add(before).and_then(|x| x.checked_sub(after))
                .ok_or_else(|| fail(ResourceError::Invalid))?
                .min(observation.capacity_bytes);
            floors.retain(|f| !(f.owner == step.owner && f.domain == observation.domain));
            floors.push(ResidentFloor { owner: step.owner.clone(), domain: observation.domain.clone(),
                bytes: after, sampled_at_ms: observation.sampled_at_ms });
        }
        state.owners.insert(step.owner.clone(), step.footprint.clone());
    }
    // Do not increment a real ledger epoch or return an executable proposal.
    Ok(state)
}
```

- [ ] Add a both-parked arrangement test, including a failed wake path; run the
  integration test again and verify both tests pass.

```rust
#[test]
fn both_parked_is_not_enough_without_each_wake_path() {
    let initial = LedgerSnapshot { epoch: 9, owners: [
        ("a".into(), f(ResourcePhase::Parked, 8)),
        ("b".into(), f(ResourcePhase::Parked, 10)),
    ].into() };
    let obs = [MemoryObservation { domain: "system".into(), capacity_bytes: 128,
        available_bytes: 110, sampled_at_ms: 100 }];
    let limits = [MemoryLimit { domain: "system".into(), managed_bytes: 96,
        free_reserve_bytes: 12, host_kv_bytes: None, parked_bytes: None }];
    for (owner, peak, expected) in [("a", 70, true), ("b", 95, false)] {
        let steps = [ForecastStep { owner: owner.into(), footprint: f(ResourcePhase::Wake, peak) }];
        assert_eq!(forecast_sequence(&initial, &steps, AdmissionContext::new(&obs, &limits, 101, 60, 4)).is_ok(), expected);
    }
}
```

- [ ] Run `cargo test -p mllm-scheduler`; expect PASS.
- [ ] Commit the three named files with
  `git commit -m "feat(scheduler): forecast intermediate residency peaks"`.

### Task 6: Lock down accounting invariants and record the integration boundary

**Files:** Create `crates/mllm-scheduler/tests/resource_properties.rs` and
`docs/design/adr/0007-phase-aware-resource-admission.md`; modify
`crates/mllm-scheduler/Cargo.toml`.

**Interfaces:** Consumes `admit_phase` and Task 1 contracts. No new public functions.

- [ ] Add `[dev-dependencies] proptest = { workspace = true }` to the scheduler
  manifest and write the property test below. First run
  `cargo test -p mllm-scheduler --test resource_properties --no-run` before adding
  the dependency to observe the expected unresolved `proptest` import, then add it.
  Do not manufacture a logic failure if an already-correct implementation passes.

```rust
use mllm_domain::resources::*;
use mllm_scheduler::residency::*;
use proptest::prelude::*;

proptest! {
    #[test]
    fn replacing_owner_charges_peak_once(old in 0i64..50, peak in 50i64..100) {
        let f = |phase, bytes| PhaseFootprint { phase,
            allocations: vec![Allocation { domain: "system".into(), bytes, host_kv_bytes: 0 }],
            devices: vec![] };
        let state = LedgerSnapshot { epoch: 1, owners: [
            ("a".into(), f(ResourcePhase::Parked, old)),
        ].into() };
        let obs = [MemoryObservation { domain: "system".into(), capacity_bytes: 128,
            available_bytes: 128 - old, sampled_at_ms: 100 }];
        let limits = [MemoryLimit { domain: "system".into(), managed_bytes: peak,
            free_reserve_bytes: 12, host_kv_bytes: None, parked_bytes: None }];
        let floors = [ResidentFloor { owner: "a".into(), domain: "system".into(),
            bytes: old, sampled_at_ms: 100 }];
        prop_assert_eq!(admit_phase(&state, "a", &f(ResourcePhase::Wake, peak),
            AdmissionContext::new(&obs, &limits, 101, 60, 4).with_resident_floors(&floors)), Ok(()));
        let too_large = f(ResourcePhase::Wake, peak + 1);
        prop_assert_eq!(admit_phase(&state, "a", &too_large,
            AdmissionContext::new(&obs, &limits, 101, 60, 4)), Err(ResourceError::Insufficient));
    }
}
```

- [ ] Add this table test for physical-domain isolation, host-KV subset limits,
  and checked arithmetic. It must pass without increasing a capacity or changing
  a host limit to make a failing case disappear.

```rust
#[test]
fn categories_and_domains_do_not_create_capacity() {
    let f = |domain: &str, bytes, kv| PhaseFootprint { phase: ResourcePhase::Ready,
        allocations: vec![Allocation { domain: domain.into(), bytes, host_kv_bytes: kv }],
        devices: vec![] };
    let state = LedgerSnapshot { epoch: 1, owners: [
        ("a".into(), f("system", 40, 9)),
    ].into() };
    let obs = [MemoryObservation { domain: "system".into(), capacity_bytes: 128,
        available_bytes: 88, sampled_at_ms: 100 }];
    let limits = [MemoryLimit { domain: "system".into(), managed_bytes: 96,
        free_reserve_bytes: 12, host_kv_bytes: Some(16), parked_bytes: None }];
    for (next, error) in [
        (f("system", 40, 8), ResourceError::CategoryLimit),
        (f("unmapped-vram", 1, 0), ResourceError::UnknownDomain),
        (f("system", i64::MAX, 0), ResourceError::Invalid),
    ] {
        assert_eq!(admit_phase(&state, "b", &next, AdmissionContext::new(&obs, &limits, 101, 60, 4)), Err(error));
    }
}
```

- [ ] Write ADR 0007 using §3's accounting semantics as the decision text. Include
  the following exact integration checklist in its Consequences section:

```text
Production cutover requires one durable ledger authority, atomic epoch CAS,
fresh host observations, deployment revision/generation validation, and exclusive
lifecycle claims. A forecast is not evidence and a proposal is not a launch grant.
Lowering a reservation or dropping a device lease requires verified release.
Existing synthetic controller admission remains unqualified until cutover.
Legacy unknown reservations must reconcile; migration must not discard or invent
their resource ownership. No engine effect occurs inside a database transaction.
```

- [ ] Add these boundary regressions to `resource_properties.rs` and run
  `cargo test -p mllm-scheduler --test resource_properties`. Each assertion names
  a distinct contract boundary; do not weaken the tests to accommodate a bad grant.

```rust
#[test]
fn recipe_peaks_cover_each_adjacent_phase() {
    let f = |phase, bytes| PhaseFootprint { phase,
        allocations: vec![Allocation { domain: "system".into(), bytes, host_kv_bytes: 0 }],
        devices: vec![] };
    let mut recipe = RecipeFootprints {
        cold: f(ResourcePhase::Cold, 80), ready: f(ResourcePhase::Ready, 60),
        parking: f(ResourcePhase::Parking, 65), parked: f(ResourcePhase::Parked, 8),
        wake: f(ResourcePhase::Wake, 70),
    };
    assert_eq!(validate_recipe(&recipe), Ok(()));
    recipe.parking.allocations[0].bytes = 59;
    assert_eq!(validate_recipe(&recipe), Err(ResourceError::Invalid));
    recipe.parking.allocations[0].bytes = 65;
    recipe.wake.allocations[0].domain = "wrong-domain".into();
    assert_eq!(validate_recipe(&recipe), Err(ResourceError::Invalid));
}

#[test]
fn parked_and_timestamp_limits_are_enforced() {
    let state = LedgerSnapshot { epoch: 1, owners: Default::default() };
    let next = PhaseFootprint { phase: ResourcePhase::Parked,
        allocations: vec![Allocation { domain: "system".into(), bytes: 10, host_kv_bytes: 0 }],
        devices: vec![] };
    let mut obs = [MemoryObservation { domain: "system".into(), capacity_bytes: 128,
        available_bytes: 128, sampled_at_ms: 100 }];
    let mut limits = [MemoryLimit { domain: "system".into(), managed_bytes: 96,
        free_reserve_bytes: 12, host_kv_bytes: None, parked_bytes: Some(9) }];
    assert_eq!(admit_phase(&state, "a", &next,
        AdmissionContext::new(&obs, &limits, 101, 60, 4)), Err(ResourceError::CategoryLimit));
    limits[0].parked_bytes = Some(10);
    assert_eq!(admit_phase(&state, "a", &next,
        AdmissionContext::new(&obs, &limits, 101, 60, 0)), Err(ResourceError::CategoryLimit));
    obs[0].sampled_at_ms = 102;
    assert_eq!(admit_phase(&state, "a", &next,
        AdmissionContext::new(&obs, &limits, 101, 60, 4)), Err(ResourceError::StaleObservation));
}

#[test]
fn spare_system_memory_cannot_cover_a_full_discrete_gpu() {
    let state = LedgerSnapshot { epoch: 0, owners: Default::default() };
    let next = PhaseFootprint { phase: ResourcePhase::Cold, allocations: vec![
        Allocation { domain: "system".into(), bytes: 5, host_kv_bytes: 0 },
        Allocation { domain: "gpu-memory:0".into(), bytes: 50, host_kv_bytes: 0 },
    ], devices: vec![] };
    let obs = [
        MemoryObservation { domain: "system".into(), capacity_bytes: 128,
            available_bytes: 128, sampled_at_ms: 100 },
        MemoryObservation { domain: "gpu-memory:0".into(), capacity_bytes: 48,
            available_bytes: 48, sampled_at_ms: 100 },
    ];
    let limits = [
        MemoryLimit { domain: "system".into(), managed_bytes: 96,
            free_reserve_bytes: 12, host_kv_bytes: None, parked_bytes: None },
        MemoryLimit { domain: "gpu-memory:0".into(), managed_bytes: 40,
            free_reserve_bytes: 4, host_kv_bytes: None, parked_bytes: None },
    ];
    assert_eq!(admit_phase(&state, "a", &next,
        AdmissionContext::new(&obs, &limits, 101, 60, 4)), Err(ResourceError::Insufficient));
}
```

- [ ] Run `cargo test -p mllm-domain -p mllm-scheduler` and then
  `cargo clippy -p mllm-domain -p mllm-scheduler --all-targets -- -D warnings`.
  Expect PASS. Use the `AdmissionContext` from Task 3 consistently; do not hide
  inputs in ambient state or suppress warnings globally.
- [ ] Run `cargo test --workspace --exclude mllm-cli`, `cargo test -p mllm-cli --lib`,
  `cargo clippy --workspace --exclude mllm-cli --all-targets -- -D warnings`, and
  `cargo clippy -p mllm-cli --lib --bin mllm -- -D warnings`; record actual counts and
  skipped live tests without treating skips as qualification.
- [ ] Commit the three named files with
  `git commit -m "test(scheduler): lock down phase-aware admission invariants"`.

## 4. Self-review and completion checks

- [ ] Every new function and type used by another task is defined above; public
  exports and test imports compile without unexplained fixture helpers.
- [ ] Verify exact-boundary fits, one-byte overflow, negative quantities, stale and
  future timestamps, unknown domains, duplicate allocations, active exclusivity,
  host-KV sub-limits, parked count/bytes, and total arithmetic overflow.
- [ ] Read the changed files and verify no code path launches engines, releases
  memory, mutates real observations, silently picks a victim, or commits a forecast.
- [ ] Confirm the source ledger stays unchanged after forecasts and unsuccessful
  proposals. Confirm two proposals from one epoch cannot both apply to the same
  evolving snapshot.
- [ ] Check `git diff --check` and the workspace tests; preserve unrelated files.
- [ ] Review the implementation before integration. Passing this plan qualifies
  only the deterministic kernel, not concurrent serving, durable admission, parking,
  SGLang, or the F2 milestone.

## 5. Spec coverage and remaining required plans

| Approved spec requirement | This plan | Required continuation |
|---|---|---|
| Per-phase budgets, unified-domain accounting, sharing constraints | Tasks 1–3, 6 | F2A2 observes and enforces the real host; F2A3 resolves configuration |
| Cold/park/wake intermediate peaks; both-parked feasibility | Task 5 | F2A2 constructs policy-valid sequences and verifies each physical step |
| Atomic reservation decisions | Task 4 defines the epoch-bound proposal contract | F2A2 persists and applies it transactionally before effects |
| Safe release, lifecycle ownership, routing and recovery | Deliberately no side effects here | F2A2 |
| Usable config, API/CLI, revisions, attachment, events and timings | Not implemented by this kernel | F2A3 |
| Qualified SGLang parking and engine-specific protocols | Not implemented by this kernel | F2B |
| Q1–Q11 full conformance and live evidence | Deterministic portion of Q2/Q6 only | F2A2/F2A3/F2B tests plus F2C live gates |

Before writing F2A2, resolve the concrete host measurement sources and release-evidence
protocol against pinned runtimes. Before any live run, fix numeric safety guardrails,
test repetitions, checkpoint/profile fingerprints, and cleanup ownership. Those are
requirements of the later plans, not hidden approvals in this CPU-only task set.

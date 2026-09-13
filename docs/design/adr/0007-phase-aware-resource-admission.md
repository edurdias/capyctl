# ADR 0007 — Phase-aware resource admission

**Status:** Accepted (2026-09-12)

## Context

Resource admission needs one deterministic accounting contract for physical memory,
phase transitions, device claims, observations, forecasts, and reservation proposals.

## Decision

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

## Consequences

Production cutover requires one durable ledger authority, atomic epoch CAS,
fresh host observations, deployment revision/generation validation, and exclusive
lifecycle claims. A forecast is not evidence and a proposal is not a launch grant.
Lowering a reservation or dropping a device lease requires verified release.
Existing synthetic controller admission remains unqualified until cutover.
Legacy unknown reservations must reconcile; migration must not discard or invent
their resource ownership. No engine effect occurs inside a database transaction.

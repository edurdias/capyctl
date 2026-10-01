# Formal models (Lean 4)

Machine-checked models of CapyCTL's safety-critical logic. They use Lean
4.34.1 and the core library only (no Mathlib):

```bash
cd formal && lake build
```

A clean build checks every theorem, and there is no `sorry`. The safety
theorems depend only on Lean's standard axioms (`propext`,
`Classical.choice`, `Quot.sound`); the protocol checks depend on none. CI
builds this directory in the `formal` job.

The models are hand transcriptions of the Rust, not extracted code. Each
module names the source it mirrors and states why each simplification is
sound, so a change to that source should update the module in the same
change. A proof here is evidence about the modelled logic, never qualification
of an engine recipe.

## Modules

| Module | Mirrors | What is proved |
|---|---|---|
| `Lifecycle` | `capyctl-domain/src/lifecycle.rs`, SPEC §6.1 | The legal table has no dead or trap states, and FAILED is left only through RECONCILING. Reconciliation can never conclude PARKED. The store's `observed_state` writes are multi-step paths of the table, never legal single steps; the table is enforced only by the F0 controller. |
| `Admission` | `capyctl-scheduler/src/residency.rs::admit_phase` | **Soundness.** An `Ok` result implies the post-grant ledger meets, on every declared domain, the managed limit, capacity minus free reserve, the host-KV and parked sub-limits and the free-reserve rule; the parked-count bound holds; and there is no device conflict. |
| `Grant` | `capyctl-store/src/resource_ledger.rs` (`ensure_increasing`, `allocates_more`, the M27 `Insufficient` bypass) | The bypass never increases any aggregate, even without well-formedness of the next footprint. Device exclusivity is preserved by every admitted grant and every release. |
| `Completion` | start completion in `ordinary_lifecycle.rs`; restore completion in `park.rs`; `validate_recipe` | A completion writes Ready without admission. Counterexample: without device coverage, a second exclusive owner enters the ledger. With the coverage rule `validate_recipe` now enforces, exclusivity is preserved. |
| `Protocols.Lease` | `capyctl-controller/src/request_leases.rs` | Exhaustive state check. The old hand-off leaks exactly one state: the reply was sent and the caller was dropped before polling. The repaired hand-off (undelivered-ticket guard) has no unaccountable state. |
| `Protocols.Gate` | `dispatch.rs`, `local_recovery.rs`, `park.rs::reopen`, `switching.rs` (switch closures and release) | Exhaustive state check. The old code reaches restart, adopt, park, deadline cancel, ending with dispatch open over a retired lease. With the repair (a park reopens only a gate it closed or a switch handed it), every reachable state is safe, and a switch victim whose park is refused still serves again. |
| `AdmitF0` | `capyctl-scheduler/src/admission.rs::admit` | Latent counterexample: a claimed `parked_budget` larger than the held Parked reservation under-charges the transition. Crediting the held bytes is proved exact. |

## Findings from the first review (2026-10-01, base `868c2a4`)

Each finding below was reproduced against the Rust before it was fixed.

| Finding | Status |
|---|---|
| A cancelled or refused park reopened a gate a restart had closed (SPEC §6.1, §10). | Fixed; store test `a_cancelled_park_does_not_reopen_a_gate_a_restart_closed`. |
| A Ready phase could claim a device its cold, parking or wake phase lacked, so a completion bypassed device exclusivity (SPEC §7.3). | Fixed in `validate_recipe`; scheduler and config tests. |
| A granted request lease leaked when its caller was dropped after the reply was sent (SPEC §10). | Fixed; controller test `a_grant_sent_but_never_taken_is_closed_as_not_accepted`. |
| F0 `admit` credits a claimed parked budget. | Open, latent: its only production caller passes an empty ledger. |
| The lifecycle table is not the production state machine, and reconciliation cannot conclude PARKED. | Open: decide whether the table documents the store's coarse projection or is retired. |

## Coverage

What the models cover, by capability, and what they do not yet cover. A
capability marked *partial* has one path modelled, not all of them.

| Capability (SPEC / ADR) | Coverage |
|---|---|
| Lifecycle states and transitions (§6.1) | Covered: the table. Not covered: the store's step and run states, which carry the in-flight phases. |
| Resource admission and aggregate bounds (§7.2, §7.3, ADR 0007) | Covered: `admit_phase`, the store grant and its bypass. |
| Device exclusivity and sharing (§7.3, ADR 0019) | Covered: grants, releases, completions. |
| Request leases and dispatch gates (§10) | Partial: lease hand-off; gate across restart for a local park. |
| Recovery and adoption (§13.2) | Partial: the local adoption gate only. |
| Releases on evidence, ledger epoch fencing (§6.1, §7.3, ADR 0009) | **Not covered.** |
| Switching and eviction planner (§10) | **Not covered.** |
| Placement and device choice (ADR 0013, ADR 0019) | **Not covered**, beyond the device checks `fits` shares with admission. |
| Idle policy, warm residency, bounded parked sets, LRU reclamation (§6.5) | **Not covered.** |
| Router queueing, fairness, outstanding bounds (§10) | **Not covered.** |
| Durable acceptance, idempotency, revision fences, `delete --stop` (§6.3, §6.4) | **Not covered.** |
| Multi-instance lifecycle and resize (ADR 0013, 0015) | **Not covered.** |
| Remote execution and agent session protocol (§13.1, ADR 0003) | **Not covered.** |
| Deep-park security gate: loopback listener, per-launch key, key guard (§9.1, T21, ADR 0012) | **Not covered.** |
| Version skew and capability gating (ADR 0017) | **Not covered.** |
| Configuration resolution, derived budgets, startup budget (§15, ADR 0014) | Partial: `startup_bytes ≥ total(ready)` was checked by reading the code, not modelled. |
| Residency tiers and the host-backed copy (§6.2) | **Not covered.** |
| KV cache and shared services (§7.4, §12); multi-node groups (§11, parked) | **Not covered.** |

### Suggested order for the next passes

1. **Releases and epoch fencing.** "Never release a reservation, advance an
   epoch, or replay a dispatch without verified evidence" is the project's
   central invariant, and no model covers it yet. Leads from the first review:
   - `release_failed_launch` lacks the `vacuous_release` guard that
     `complete_cleanup` has;
   - the start completion advances the epoch even when Ready equals cold.
2. **Switching and eviction planner.** A bounded search over victims
   (`scheduler/switching.rs`, `planner_max_states`); prove that it never
   evicts warm or active work it must keep.
3. **Dispatch gate, remaining paths.** `reverify_remote_dispatch` does not
   re-check open park runs: a possible TOCTOU against an accepted park.
4. **Router queue and fairness.** Bounds and the waiting-window rules.
5. **Deep-park security gate.** A configuration-level model: protections stay
   mandatory whenever deep parking is on.

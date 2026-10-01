/-!
# Lifecycle state machine

Faithful transcription of `crates/capyctl-domain/src/lifecycle.rs`
(`legal_transitions`) checked against SPEC §6.1.

Every theorem here is closed by `decide`: the state space is finite, so the
kernel enumerates it exhaustively.
-/
namespace Capy.Lifecycle

inductive S
  | stopped | starting | ready | draining | parking
  | parked | waking | stopping | reconciling | failed
  deriving DecidableEq, Repr, Inhabited

open S

def all : List S :=
  [stopped, starting, ready, draining, parking, parked, waking, stopping, reconciling, failed]

theorem all_complete : ∀ s : S, s ∈ all := by intro s; cases s <;> decide

/-- `legal_transitions()` verbatim, in source order. -/
def table : List (S × S) :=
  [ (stopped, starting), (starting, ready), (ready, draining), (draining, parking),
    (parking, parked), (parked, waking), (waking, ready),
    (draining, stopping), (stopping, stopped), (parked, stopping),
    (stopped, reconciling), (starting, reconciling), (ready, reconciling),
    (draining, reconciling), (parking, reconciling), (parked, reconciling),
    (waking, reconciling), (stopping, reconciling), (failed, reconciling),
    (reconciling, stopped), (reconciling, ready), (reconciling, failed) ]

def step (a b : S) : Bool := table.contains (a, b)

/-- One round of forward closure. -/
def succs (xs : List S) : List S :=
  (xs ++ all.filter (fun b => xs.any (fun a => step a b))).eraseDups

/-- States reachable from `xs` in at most `n` steps. Ten states ⇒ `n = 10` is a fixpoint. -/
def reach (n : Nat) (xs : List S) : List S :=
  match n with
  | 0 => xs
  | n + 1 => reach n (succs xs)

def reachable (a b : S) : Bool := (reach 10 [a]).contains b

/-- Reachability in a graph with some state `avoid` deleted (used for "every path visits"). -/
def stepAvoid (avoid : S) (a b : S) : Bool := step a b && a != avoid && b != avoid

def succsAvoid (avoid : S) (xs : List S) : List S :=
  (xs ++ all.filter (fun b => xs.any (fun a => stepAvoid avoid a b))).eraseDups

def reachAvoid (avoid : S) : Nat → List S → List S
  | 0, xs => xs
  | n + 1, xs => reachAvoid avoid n (succsAvoid avoid xs)

/-- `b` is reachable from `a` without ever entering `avoid`. -/
def reachableAvoiding (avoid a b : S) : Bool := (reachAvoid avoid 10 [a]).contains b

/-! ## Conformance with the SPEC §6.1 diagram -/

/-- Every "normal transition" in SPEC §6.1 is in the table. -/
theorem spec_normal_paths_legal :
    [ (stopped, starting), (starting, ready), (ready, draining), (draining, parking),
      (parking, parked), (parked, waking), (waking, ready), (draining, stopping),
      (stopping, stopped), (parked, stopping) ].all (fun p => step p.1 p.2) = true := by
  decide

/-- Every state except RECONCILING may enter RECONCILING ("any uncertain state"). -/
theorem every_state_may_reconcile :
    ∀ s : S, s ≠ reconciling → step s reconciling = true := by
  intro s; cases s <;> decide

/-- No self-loops: a state write that does not change state is never "legal". -/
theorem no_self_loops : ∀ s : S, step s s = false := by
  intro s; cases s <;> decide

/-- Every state is reachable from STOPPED (no dead states). -/
theorem all_reachable_from_stopped : ∀ s : S, reachable stopped s = true := by
  intro s; cases s <;> decide

/-- STOPPED is reachable from every state (no traps; FAILED recovers through RECONCILING). -/
theorem stopped_reachable_from_all : ∀ s : S, reachable s stopped = true := by
  intro s; cases s <;> decide

/-- FAILED is left only through RECONCILING (SPEC: "Closed except bounded recovery wait"). -/
theorem failed_exits_only_to_reconciling :
    ∀ t : S, step failed t = true → t = reconciling := by
  intro t; cases t <;> decide

/-- READY is entered only from STARTING, WAKING or RECONCILING. -/
theorem ready_entries :
    ∀ s : S, step s ready = true → s = starting ∨ s = waking ∨ s = reconciling := by
  intro s; cases s <;> decide

/-- No transition skips the drain: READY never goes directly to PARKING/PARKED/STOPPING. -/
theorem ready_must_drain :
    step ready parking = false ∧ step ready parked = false ∧ step ready stopping = false := by
  decide

/-! ## Findings

### F-L1: reconciliation can never conclude PARKED

SPEC §6.1: "Any uncertain state -> RECONCILING -> verified state or FAILED".
The table's reconciliation outcomes are only STOPPED, READY and FAILED. A
deployment whose PARKED state became uncertain therefore cannot be reconciled
back to PARKED: every route from RECONCILING to PARKED passes through READY
(a full wake, which needs a wake-sized reservation) — or through STOPPED,
which releases the engine. -/

theorem reconciling_outcomes :
    ∀ t : S, step reconciling t = true ↔ (t = stopped ∨ t = ready ∨ t = failed) := by
  intro t; cases t <;> decide

theorem reconcile_cannot_conclude_parked : step reconciling parked = false := by decide

/-- Every path from RECONCILING to PARKED visits READY. -/
theorem parked_after_reconcile_requires_ready :
    reachableAvoiding ready reconciling parked = false := by decide

/-- …and PARKED is still reachable once READY is allowed (so the theorem above is not vacuous). -/
theorem parked_after_reconcile_reachable : reachable reconciling parked = true := by decide

/-- Likewise STARTING/WAKING/PARKING/DRAINING/STOPPING are never reconciliation outcomes:
    an in-flight transition found ambiguous must be redone from a settled state. -/
theorem in_flight_not_outcomes :
    [starting, draining, parking, waking, stopping].all (fun t => !step reconciling t) = true := by
  decide

/-! ### F-L2: no direct cancellation of an in-flight transition

STARTING and WAKING cannot go to STOPPING or FAILED directly; aborting a cold
start or a wake is only expressible through RECONCILING. -/
theorem no_direct_abort :
    step starting stopping = false ∧ step waking stopping = false ∧
    step starting failed = false ∧ step waking failed = false ∧
    step parking failed = false ∧ step stopping failed = false := by decide

/-- FAILED is only entered from RECONCILING. -/
theorem failed_entries : ∀ s : S, step s failed = true → s = reconciling := by
  intro s; cases s <;> decide

end Capy.Lifecycle

namespace Capy.Lifecycle
open S

/-! ### F-L3: the production store does not follow the table

`LifecycleState::can_transition_to` is consulted only by `validate_chain`
(`operations.rs:187`), i.e. by the F0 `Controller`, which nothing outside tests
constructs. The coordinator's store persists a coarse `observed_state` and
writes these transitions directly (in-flight phases live in step/run rows): -/
def storeWrites : List (S × S) :=
  [ (stopped, ready)    -- ordinary_lifecycle.rs:1210 (start completion)
  , (ready, parked)     -- park.rs:1906 (park completion)
  , (parked, ready)     -- park.rs:1915 (restore completion)
  , (ready, stopped)    -- cleanup.rs:1480 (stop completion)
  , (parked, stopped) ] -- cleanup.rs:1480

/-- None of the store's state writes is a legal single step of the table. -/
theorem store_writes_all_illegal : storeWrites.all (fun p => !step p.1 p.2) = true := by decide

/-- Each is, however, a legal multi-step path, so the store is a *coarsening*
    of the table, not a contradiction of it (the missing states are the step
    rows' `armed`/`uncertain` phases). -/
theorem store_writes_are_paths : storeWrites.all (fun p => reachable p.1 p.2) = true := by decide

end Capy.Lifecycle

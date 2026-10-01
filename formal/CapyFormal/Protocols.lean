/-!
# Protocol models (exhaustive finite-state checks)

Two small concurrent protocols, each modelled as a transition system whose
reachable set is computed by the kernel. `decide` then checks an invariant over
**every** reachable state — a bounded-but-complete model check, since each
state space is finite and the closure reaches a fixpoint.

1. **Request-lease hand-off** (`crates/capyctl-controller/src/request_leases.rs`):
   the group-commit writer grants a lease and replies over a oneshot. Before
   the 2026-10-01 fix a caller that went away was covered only if
   `reply.send` *failed*; now an untaken ticket queues its own close.
2. **Dispatch gate across a restart** (`crates/capyctl-store/src/dispatch.rs`,
   `ordinary_lifecycle/{local_recovery,park}.rs`): restart closes every gate
   without a closure row; adoption moves the launch to the new session;
   `reverify_local_dispatch` reopens only after `require_settled`. Before the
   2026-10-01 fix a cancelled park `reopen`ed subject only to closure rows; now
   it reopens only a gate it closed (`reopens_dispatch`).

In each model `next false` is the previous code and `next true` the repair.
-/
namespace Capy.Protocols

/-- Generic reachability by iterated closure over a finite successor function. -/
def closure {σ} [DecidableEq σ] (next : σ → List σ) : Nat → List σ → List σ
  | 0, xs => xs
  | n + 1, xs => closure next n ((xs ++ xs.flatMap next).eraseDups)

/-! ## 1. Request-lease hand-off -/

namespace Lease

inductive Caller | waiting | holding | gone
  deriving DecidableEq, Repr
inductive Chan | empty | full | dropped   -- `dropped`: receiver gone
  deriving DecidableEq, Repr

structure St where
  row : Bool          -- lease row is `inflight` in the ledger
  committed : Bool    -- writer has applied the grant batch
  replied : Bool      -- writer has run `reply.send`
  orphan : Bool       -- a `Finish` is queued in the writer's orphan list
  caller : Caller
  chan : Chan
  deriving DecidableEq, Repr

def init : St := ⟨false, false, false, false, .waiting, .empty⟩

/-- Moves. `fixed` selects the repaired protocol (an undelivered ticket in a
    dropped channel is turned into an orphan close, e.g. by a `Drop` guard). -/
def next (fixed : Bool) (s : St) : List St :=
  -- the writer applies the batch: the row becomes inflight
  (if !s.committed then [{ s with committed := true, row := true }] else []) ++
  -- the writer replies: `send` succeeds iff the receiver is alive
  (if s.committed && !s.replied then
     [if s.chan == .dropped then { s with replied := true, orphan := true }
      else { s with replied := true, chan := .full }] else []) ++
  -- the caller polls and takes the ticket
  (if s.caller == .waiting && s.chan == .full then
     [{ s with caller := .holding, chan := .empty }] else []) ++
  -- the caller's future is dropped while waiting (client disconnect)
  (if s.caller == .waiting then
     [{ s with caller := .gone, chan := .dropped,
               orphan := s.orphan || (fixed && s.chan == .full) }] else []) ++
  -- the holder closes on evidence
  (if s.caller == .holding && s.row then [{ s with row := false, caller := .gone }] else []) ++
  -- the writer flushes an orphan close in its next batch
  (if s.orphan && s.row then [{ s with row := false, orphan := false }] else [])

def reachable (fixed : Bool) : List St := closure (next fixed) 8 [init]

/-- Someone can still close an inflight lease. -/
def accountable (s : St) : Bool :=
  !s.row || s.orphan || s.caller == .holding ||
  (s.caller == .waiting) ||
  (s.committed && !s.replied) ||            -- the writer's pending send will orphan it                 -- the caller will receive or be orphaned
  (s.chan == .full && s.caller != .gone)

/-- The closure has converged (the 8-step bound is a fixpoint). -/
theorem converged (b : Bool) :
    (closure (next b) 9 [init]).length = (reachable b).length := by
  cases b <;> decide

/-- **Previous protocol: a lease could be leaked.** A reachable state holds an
    inflight row that no party will ever close (reproduced by a probe test). -/
theorem previous_leaks : (reachable false).any (fun s => !accountable s) = true := by decide

/-- The leaked state is exactly: committed, replied into a live channel, caller
    dropped before polling, no orphan queued. -/
theorem leak_witness :
    (⟨true, true, true, false, .gone, .dropped⟩ : St) ∈ reachable false := by decide

/-- **Repaired protocol: every reachable inflight lease is accountable.** -/
theorem fixed_no_leak : (reachable true).all accountable = true := by decide

/-- In the previous protocol the leak was the *only* unaccountable state. -/
theorem previous_only_leak :
    (reachable false).filter (fun s => !accountable s) =
      [⟨true, true, true, false, .gone, .dropped⟩] := by decide

end Lease

/-! ## 2. Dispatch gate across a restart -/

namespace Gate

structure St where
  open_ : Bool          -- `dispatch_enabled`
  proven : Bool         -- readiness re-proven in the *current* session
  retired : Bool        -- a retired session's lease is still recorded
  adopted : Bool        -- launch belongs to the current session
  parkPlanned : Bool    -- a park is accepted and not yet armed past drain
  parkClosedOpen : Bool -- the park closed a gate that was open (fix bookkeeping)
  deriving DecidableEq, Repr

/-- Serving, in a session that launched it, with one request in flight. -/
def init : St := ⟨true, true, false, true, false, false⟩

def next (fixed : Bool) (s : St) : List St :=
  -- controller restart: every gate closes, leases become retired, launch unadopted
  [{ s with open_ := false, proven := false, retired := s.retired || s.open_,
            adopted := false, parkPlanned := false, parkClosedOpen := false }] ++
  -- `adopt_retired_local_launch`
  (if !s.adopted then [{ s with adopted := true }] else []) ++
  -- `reverify_local_dispatch`: fresh evidence, `require_settled`, no open park
  (if s.adopted && !s.retired && !s.parkPlanned then
     [{ s with open_ := true, proven := true }] else []) ++
  -- `abandon_retired_request_leases` on quiescence evidence
  (if s.retired then [{ s with retired := false }] else []) ++
  -- `accept_instance` (park): needs an adopted Ready launch; closes the gate
  (if s.adopted && !s.parkPlanned then
     [{ s with parkPlanned := true, parkClosedOpen := s.open_, open_ := false }] else []) ++
  -- park cancelled at its deadline: `reopen`
  (if s.parkPlanned then
     [{ s with parkPlanned := false,
               open_ := if fixed then s.parkClosedOpen else true }] else [])

def reachable (fixed : Bool) : List St := closure (next fixed) 10 [init]

/-- SPEC §6.1/§10: dispatch is open only on readiness proven in this session and
    with no retired session's request possibly still running. -/
def safe (s : St) : Bool := !s.open_ || (s.proven && !s.retired)

theorem converged (b : Bool) :
    (closure (next b) 11 [init]).length = (reachable b).length := by
  cases b <;> decide

theorem init_safe : safe init = true := by decide

/-- **Previous code: unsafe state reachable** (reproduced against the Rust):
    restart → adopt → park accepted → deadline cancel → gate open over a retired lease. -/
theorem previous_unsafe : (reachable false).any (fun s => !safe s) = true := by decide

theorem unsafe_witness :
    (⟨true, false, true, true, false, false⟩ : St) ∈ reachable false := by decide

/-- **Repaired: a cancelled park reopens only a gate it closed itself.** -/
theorem fixed_safe : (reachable true).all safe = true := by decide

end Gate
end Capy.Protocols

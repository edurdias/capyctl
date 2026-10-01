/-!
# Resource admission kernel (`admit_phase`)

Model of `crates/capyctl-scheduler/src/residency.rs::admit_phase` and the
footprint helpers in `crates/capyctl-domain/src/resources.rs`.

## Modelling choices (and why they are sound)

* Byte counts are unbounded `Int`. Rust uses `i64` with `checked_add` /
  `checked_sub`, every overflow mapping to `Err(Invalid)`. A Rust `Ok` therefore
  means no overflow happened and every intermediate equals its `Int` value; so a
  property of the model's `Ok` results holds for Rust's `Ok` results. We keep the
  checked-add *structure* (an `Except` fold) so the loop shape matches, but do
  not need a 64-bit bound to prove safety.
* The ledger (`BTreeMap<String, PhaseFootprint>`) is a list of pairs with
  distinct keys; the post-grant ledger is the others plus `(owner, next)`,
  exactly what `apply_proposal_to_snapshot` / the store's upsert produce.
* Validation that only *rejects* (`validate_context`, `validate_domains`, …) is
  modelled where a proof needs it and otherwise elided: elision can only add
  `Ok` results to the model, so safety proved for the model still holds.
-/
namespace Capy.Admission

inductive Phase | cold | ready | parking | parked | wake
  deriving DecidableEq, Repr

inductive Sharing | shared | exclusive
  deriving DecidableEq, Repr

structure Claim where
  device : String
  sharing : Sharing
  deriving DecidableEq, Repr

structure Alloc where
  domain : String
  bytes : Int
  hostKv : Int
  deriving DecidableEq, Repr

structure Fp where
  phase : Phase
  allocs : List Alloc
  devices : List Claim
  deriving DecidableEq, Repr

structure Limit where
  domain : String
  managed : Int
  freeReserve : Int
  hostKv : Option Int
  parked : Option Int

structure Obs where
  domain : String
  capacity : Int
  available : Int

structure Floor where
  owner : String
  domain : String
  bytes : Int

inductive Err | invalid | unknownDomain | deviceConflict | insufficient | categoryLimit
  deriving DecidableEq, Repr

abbrev Ledger := List (String × Fp)

/-- `amount(f, domain)`: first allocation on the domain, else `(0, 0)`. -/
def amount (f : Fp) (d : String) : Int × Int :=
  match f.allocs.find? (·.domain == d) with
  | some a => (a.bytes, a.hostKv)
  | none => (0, 0)

/-- `claims_conflict` from `resources.rs`. -/
def conflict (a b : List Claim) : Bool :=
  a.any fun x => b.any fun y =>
    x.device == y.device && (x.sharing == .exclusive || y.sharing == .exclusive)

def floorOf (floors : List Floor) (id d : String) : Int :=
  match floors.find? (fun f => f.owner == id && f.domain == d) with
  | some f => f.bytes
  | none => 0

/-- `limit.is_some_and(|x| v > x)`. -/
def over (lim : Option Int) (v : Int) : Bool :=
  match lim with
  | some x => decide (v > x)
  | none => false

/-- Accumulator of the per-domain loop: `(remaining, total, kv, parked)`. -/
structure Acc where
  remaining : Int
  total : Int
  kv : Int
  parked : Int

/-- One iteration of the `for (id, f) in others` loop (checked adds elided; see header). -/
def accStep (floors : List Floor) (d : String) (acc : Acc) (e : String × Fp) : Acc :=
  let (bytes, hostKv) := amount e.2 d
  { remaining := acc.remaining + max (bytes - floorOf floors e.1 d) 0
    total := acc.total + bytes
    kv := acc.kv + hostKv
    parked := acc.parked + (if e.2.phase = .parked then bytes else 0) }

/-- `for x in xs { f(x)?; }` — stops at the first error. -/
def forEach {α ε} (f : α → Except ε Unit) : List α → Except ε Unit
  | [] => .ok ()
  | x :: xs => match f x with
    | .error e => .error e
    | .ok () => forEach f xs

/-- Body of `for l in limits` in `admit_phase`, checks in source order. -/
def checkLimit (owner : String) (next : Fp) (others : Ledger) (obs : List Obs)
    (floors : List Floor) (l : Limit) : Except Err Unit :=
  match obs.find? (·.domain == l.domain) with
  | none => .error .unknownDomain
  | some o =>
    let cand := (amount next l.domain).1
    let candKv := (amount next l.domain).2
    let own := max (cand - floorOf floors owner l.domain) 0
    let init : Acc := { remaining := own, total := cand, kv := candKv,
                        parked := if next.phase = .parked then cand else 0 }
    let acc := others.foldl (accStep floors l.domain) init
    let ceiling := o.capacity - l.freeReserve
    if acc.total > l.managed ∨ acc.total > ceiling then .error .insufficient
    else if over l.hostKv acc.kv || over l.parked acc.parked then .error .categoryLimit
    else if own > 0 ∧ o.available - acc.remaining < l.freeReserve then .error .insufficient
    else .ok ()

def others (ledger : Ledger) (owner : String) : Ledger := ledger.filter (·.1 != owner)

def admitPhase (ledger : Ledger) (owner : String) (next : Fp) (limits : List Limit)
    (obs : List Obs) (floors : List Floor) (maxParked : Nat) : Except Err Unit :=
  let os := others ledger owner
  if os.any (fun e => conflict e.2.devices next.devices) then .error .deviceConflict
  else if (os.filter (·.2.phase = .parked)).length + (if next.phase = .parked then 1 else 0)
      > maxParked then .error .categoryLimit
  else forEach (checkLimit owner next os obs floors) limits

/-- The ledger after the grant is applied (store upsert / `apply_proposal_to_snapshot`). -/
def post (ledger : Ledger) (owner : String) (next : Fp) : Ledger :=
  others ledger owner ++ [(owner, next)]

/-! ## Aggregates over a ledger, stated independently of the implementation -/

def total (L : Ledger) (d : String) : Int := (L.map fun e => (amount e.2 d).1).sum
def kvTotal (L : Ledger) (d : String) : Int := (L.map fun e => (amount e.2 d).2).sum
def parkedBytes (L : Ledger) (d : String) : Int :=
  (L.map fun e => if e.2.phase = .parked then (amount e.2 d).1 else 0).sum
def parkedCount (L : Ledger) : Nat := (L.filter (·.2.phase = .parked)).length

/-- Unresolved charge: reserved bytes not yet credited as resident. -/
def unresolved (floors : List Floor) (L : Ledger) (d : String) : Int :=
  (L.map fun e => max ((amount e.2 d).1 - floorOf floors e.1 d) 0).sum

/-- Pairwise device exclusivity across the whole ledger. -/
def exclusive (L : Ledger) : Prop :=
  ∀ i j, (hi : i < L.length) → (hj : j < L.length) → i ≠ j →
    conflict L[i].2.devices L[j].2.devices = false

/-! ## Lemmas -/

theorem ite3_ok {A B C : Prop} [Decidable A] [Decidable B] [Decidable C] {e1 e2 e3 : Err}
    (h : (if A then .error e1 else if B then .error e2 else if C then .error e3 else .ok ())
      = (Except.ok () : Except Err Unit)) : ¬A ∧ ¬B ∧ ¬C := by
  by_cases a : A <;> by_cases b : B <;> by_cases c : C <;> simp_all <;> cases h

theorem ite2_ok {A B : Prop} [Decidable A] [Decidable B] {e1 e2 : Err} {r : Except Err Unit}
    (h : (if A then .error e1 else if B then .error e2 else r) = .ok ()) :
    ¬A ∧ ¬B ∧ r = .ok () := by
  by_cases a : A <;> by_cases b : B <;> simp_all <;> cases h

theorem forEach_ok {α ε} {f : α → Except ε Unit} :
    ∀ {xs : List α}, forEach f xs = .ok () → ∀ x ∈ xs, f x = .ok ()
  | [], _, x, hx => by cases hx
  | y :: ys, h, x, hx => by
    unfold forEach at h
    split at h
    · cases h
    · rename_i hy
      cases List.mem_cons.mp hx with
      | inl hxy => subst hxy; exact hy
      | inr hxs => exact forEach_ok h x hxs

/-- The loop computes the four aggregates over `others` on top of its initial value. -/
theorem foldl_acc (floors : List Floor) (d : String) :
    ∀ (xs : Ledger) (a : Acc), (xs.foldl (accStep floors d) a) =
      { remaining := a.remaining + unresolved floors xs d
        total := a.total + total xs d
        kv := a.kv + kvTotal xs d
        parked := a.parked + parkedBytes xs d }
  | [], a => by simp [unresolved, total, kvTotal, parkedBytes]
  | e :: xs, a => by
    rw [List.foldl_cons, foldl_acc floors d xs]
    simp only [accStep, unresolved, total, kvTotal, parkedBytes, List.map_cons, List.sum_cons,
      Acc.mk.injEq]
    omega

theorem total_post (L : Ledger) (o : String) (n : Fp) (d : String) :
    total (post L o n) d = (amount n d).1 + total (others L o) d := by
  simp [post, total, List.sum_append]; omega

theorem kv_post (L : Ledger) (o : String) (n : Fp) (d : String) :
    kvTotal (post L o n) d = (amount n d).2 + kvTotal (others L o) d := by
  simp [post, kvTotal, List.sum_append]; omega

theorem parked_post (L : Ledger) (o : String) (n : Fp) (d : String) :
    parkedBytes (post L o n) d =
      (if n.phase = .parked then (amount n d).1 else 0) + parkedBytes (others L o) d := by
  simp [post, parkedBytes, List.sum_append]; omega

theorem unresolved_post (floors : List Floor) (L : Ledger) (o : String) (n : Fp) (d : String) :
    unresolved floors (post L o n) d =
      max ((amount n d).1 - floorOf floors o d) 0 + unresolved floors (others L o) d := by
  simp [post, unresolved, List.sum_append]; omega

theorem parkedCount_post (L : Ledger) (o : String) (n : Fp) :
    parkedCount (post L o n) =
      parkedCount (others L o) + (if n.phase = .parked then 1 else 0) := by
  by_cases h : n.phase = .parked <;> simp [post, parkedCount, List.filter_append, h]

/-! ## Safety theorems -/

/-- Everything `admit_phase` guarantees about one declared domain, phrased over the
    **post-grant ledger** rather than over the implementation's loop variables. -/
structure DomainSafe (L : Ledger) (owner : String) (next : Fp) (obs : List Obs)
    (floors : List Floor) (l : Limit) : Prop where
  observed : ∃ o, obs.find? (·.domain == l.domain) = some o ∧
    total L l.domain ≤ o.capacity - l.freeReserve ∧
    (max ((amount next l.domain).1 - floorOf floors owner l.domain) 0 > 0 →
      o.available - unresolved floors L l.domain ≥ l.freeReserve)
  managed : total L l.domain ≤ l.managed
  hostKv : over l.hostKv (kvTotal L l.domain) = false
  parked : over l.parked (parkedBytes L l.domain) = false

theorem checkLimit_sound {owner next L obs floors l}
    (h : checkLimit owner next (others L owner) obs floors l = .ok ()) :
    DomainSafe (post L owner next) owner next obs floors l := by
  unfold checkLimit at h
  split at h
  · cases h
  · rename_i o hfind
    obtain ⟨h1, h2, h3⟩ := ite3_ok h
    simp only [foldl_acc, Bool.or_eq_true, not_or, Bool.not_eq_true] at h1 h2 h3
    have hu := unresolved_post floors L owner next l.domain
    have hp := parked_post L owner next l.domain
    refine ⟨⟨o, hfind, ?_, ?_⟩, ?_, ?_, ?_⟩
    · rw [total_post]; omega
    · intro hpos; rw [hu]; omega
    · rw [total_post]; omega
    · rw [kv_post]; simpa [Int.add_comm] using h2.1
    · rw [hp]
      by_cases hn : next.phase = .parked <;> simp only [hn, ite_true, ite_false] at h2 ⊢ <;>
        simpa [Int.add_comm] using h2.2

/-- **Admission soundness.** If `admit_phase` returns `Ok`, the ledger after the
    grant satisfies, on every declared domain: total ≤ managed limit, total ≤
    capacity − free reserve, host-KV and parked sub-limits, and the free-reserve
    check on unresolved charge whenever the candidate adds memory; the parked
    count bound holds; and the candidate conflicts with no other owner's devices. -/
theorem admit_sound {L owner next limits obs floors maxParked}
    (h : admitPhase L owner next limits obs floors maxParked = .ok ()) :
    (∀ l ∈ limits, DomainSafe (post L owner next) owner next obs floors l) ∧
    parkedCount (post L owner next) ≤ maxParked ∧
    (∀ e ∈ others L owner, conflict e.2.devices next.devices = false) := by
  unfold admitPhase at h
  dsimp only at h
  obtain ⟨hdev, hpark, h⟩ := ite2_ok h
  refine ⟨fun l hl => checkLimit_sound (forEach_ok h l hl), ?_, ?_⟩
  · rw [parkedCount_post]; unfold parkedCount; omega
  · intro e he
    simp only [List.any_eq_true, not_exists, not_and, Bool.not_eq_true] at hdev
    exact hdev e he

end Capy.Admission

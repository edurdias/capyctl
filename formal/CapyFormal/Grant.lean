import CapyFormal.Admission
/-!
# Store grant path (`reserve_in_transaction`)

Models `crates/capyctl-store/src/resource_ledger.rs`:

* `ensure_increasing` — a grant may only move Ready→Parking, Parked→Wake, or
  create a Cold owner, and may never shrink an allocation or drop a device;
* `allocates_more`;
* the **Insufficient bypass** (“Found live 2026-09-23 (matrix M27)”): an
  `Err(Insufficient)` from `admit_phase` is overridden to `Ok` when the next
  footprint allocates nothing beyond the current one.

The bypass swallows an error raised *before* the category checks of the same
domain and before every later domain, so it is only sound if it cannot make any
aggregate worse. `bypass_sound` proves exactly that.
-/
namespace Capy.Grant
open Capy.Admission

/-- `validate_footprint`'s structural part: distinct domains, non-negative bytes,
    `0 ≤ host_kv ≤ bytes`. -/
structure WF (f : Fp) : Prop where
  nodup : (f.allocs.map (·.domain)).Nodup
  nonneg : ∀ a ∈ f.allocs, 0 ≤ a.hostKv ∧ a.hostKv ≤ a.bytes

/-- The phase/monotonicity part of `ensure_increasing` for an existing owner. -/
def ensureIncreasing (old next : Fp) : Prop :=
  (next.phase = .cold ∨ next.phase = .parking ∨ next.phase = .wake) ∧
  ((old.phase = .ready ∧ next.phase = .parking) ∨ (old.phase = .parked ∧ next.phase = .wake)) ∧
  (∀ p ∈ old.allocs, ∃ c, next.allocs.find? (·.domain == p.domain) = some c ∧
      c.bytes ≥ p.bytes ∧ c.hostKv ≥ p.hostKv) ∧
  (∀ c ∈ old.devices, c ∈ next.devices)

/-- `allocates_more(current, next)` as a proposition. -/
def allocatesMore (cur next : Fp) : Prop :=
  (∃ w ∈ next.allocs, ¬ ∃ h ∈ cur.allocs,
      h.domain = w.domain ∧ h.bytes ≥ w.bytes ∧ h.hostKv ≥ w.hostKv) ∨
  (∃ d ∈ next.devices, d ∉ cur.devices)

theorem not_more_allocs {cur next : Fp} (h : ¬ allocatesMore cur next) :
    ∀ w ∈ next.allocs, ∃ x ∈ cur.allocs,
      x.domain = w.domain ∧ x.bytes ≥ w.bytes ∧ x.hostKv ≥ w.hostKv :=
  fun w hw => Classical.byContradiction fun hc => h (Or.inl ⟨w, hw, hc⟩)

theorem not_more_devices {cur next : Fp} (h : ¬ allocatesMore cur next) :
    ∀ d ∈ next.devices, d ∈ cur.devices :=
  fun d hd => Classical.byContradiction fun hc => h (Or.inr ⟨d, hd, hc⟩)

/-! ### `find?` on a list with distinct keys -/

theorem find_of_mem {xs : List Alloc} (hnd : (xs.map (·.domain)).Nodup) {a : Alloc}
    (ha : a ∈ xs) : xs.find? (·.domain == a.domain) = some a := by
  induction xs with
  | nil => cases ha
  | cons y ys ih =>
    simp only [List.map_cons, List.nodup_cons, List.mem_map] at hnd
    rw [List.find?_cons]
    cases List.mem_cons.mp ha with
    | inl h => subst h; simp
    | inr h =>
      have hne : (y.domain == a.domain) = false := by
        simp only [beq_eq_false_iff_ne, ne_eq]
        intro he; exact hnd.1 ⟨a, h, he.symm⟩
      rw [hne]; exact ih hnd.2 h

theorem find_none {xs : List Alloc} {d : String} (h : ∀ a ∈ xs, a.domain ≠ d) :
    xs.find? (·.domain == d) = none := by
  rw [List.find?_eq_none]; intro a ha; simpa using h a ha

/-- Under `ensure_increasing` and `¬allocates_more`, the next footprint charges
    exactly what the current one does, on every domain. -/
theorem amount_eq {old next : Fp} (wo : WF old) (wn : WF next)
    (hinc : ensureIncreasing old next) (hmore : ¬ allocatesMore old next) (d : String) :
    amount next d = amount old d := by
  obtain ⟨-, -, hcover, -⟩ := hinc
  have hle := not_more_allocs hmore
  unfold amount
  by_cases hd : ∃ p ∈ old.allocs, p.domain = d
  · obtain ⟨p, hp, rfl⟩ := hd
    obtain ⟨c, hc, hcb, hck⟩ := hcover p hp
    have hcm : c ∈ next.allocs := List.mem_of_find?_eq_some hc
    have hcd : c.domain = p.domain := by
      have := List.find?_some hc; simpa using this
    obtain ⟨h, hh, hhd, hhb, hhk⟩ := hle c hcm
    have : h = p := by
      have e1 := find_of_mem wo.nodup hh
      have e2 := find_of_mem wo.nodup hp
      rw [hhd, hcd] at e1; rw [e1] at e2; exact Option.some.inj e2
    subst this
    rw [hc, find_of_mem wo.nodup hp]
    simp only [Prod.mk.injEq]; omega
  · have hn : ∀ a ∈ next.allocs, a.domain ≠ d := by
      intro a ha had
      obtain ⟨h, hh, hhd, -⟩ := hle a ha
      exact hd ⟨h, hh, by rw [hhd, had]⟩
    have ho : ∀ a ∈ old.allocs, a.domain ≠ d := fun a ha had => hd ⟨a, ha, had⟩
    rw [find_none hn, find_none ho]

theorem amount_nonneg {f : Fp} (w : WF f) (d : String) : 0 ≤ (amount f d).1 := by
  unfold amount
  cases h : f.allocs.find? (·.domain == d) with
  | none => simp
  | some a =>
    have := w.nonneg a (List.mem_of_find?_eq_some h)
    simp; omega

theorem conflict_mono {a a' b : List Claim} (hsub : ∀ x ∈ a, x ∈ a')
    (h : conflict a' b = false) : conflict a b = false := by
  simp only [conflict, List.any_eq_false, Bool.not_eq_true] at h ⊢
  intro x hx; exact h x (hsub x hx)

/-- **Bypass soundness.** Comparing the ledger after the grant (`post … next`)
    with the ledger holding the owner's current footprint (`post … old`, i.e. the
    current ledger up to order): every domain total and host-KV total is
    unchanged, parked bytes and parked count do not increase, and the candidate
    conflicts with nobody it did not already conflict with. So overriding
    `Insufficient` never makes a limit that held before fail afterwards. -/
theorem bypass_sound {L : Ledger} {o : String} {old next : Fp}
    (wo : WF old) (wn : WF next)
    (hinc : ensureIncreasing old next) (hmore : ¬ allocatesMore old next) :
    (∀ d, total (post L o next) d = total (post L o old) d) ∧
    (∀ d, kvTotal (post L o next) d = kvTotal (post L o old) d) ∧
    (∀ d, parkedBytes (post L o next) d ≤ parkedBytes (post L o old) d) ∧
    parkedCount (post L o next) ≤ parkedCount (post L o old) ∧
    (∀ e ∈ others L o, conflict old.devices e.2.devices = false →
        conflict next.devices e.2.devices = false) := by
  have heq := amount_eq wo wn hinc hmore
  have hnp : next.phase ≠ .parked := by
    rcases hinc.1 with h | h | h <;> rw [h] <;> decide
  refine ⟨?_, ?_, ?_, ?_, ?_⟩
  · intro d; rw [total_post, total_post, heq]
  · intro d; rw [kv_post, kv_post, heq]
  · intro d
    rw [parked_post, parked_post, ite_cond_eq_false _ _ (eq_false hnp)]
    have := amount_nonneg wo d
    split <;> omega
  · rw [parkedCount_post, parkedCount_post, ite_cond_eq_false _ _ (eq_false hnp)]; omega
  · intro e _ h
    -- `¬allocates_more` ⇒ next's devices ⊆ old's devices.
    exact conflict_mono (not_more_devices hmore) h

/-! ### Device exclusivity is an inductive invariant of grants -/

theorem conflict_symm (a b : List Claim) : conflict a b = conflict b a := by
  unfold conflict
  apply Bool.eq_iff_iff.mpr
  simp only [List.any_eq_true, Bool.and_eq_true, beq_iff_eq, Bool.or_eq_true]
  constructor
  · rintro ⟨x, hx, y, hy, hd, hs⟩; exact ⟨y, hy, x, hx, hd.symm, hs.symm⟩
  · rintro ⟨x, hx, y, hy, hd, hs⟩; exact ⟨y, hy, x, hx, hd.symm, hs.symm⟩

/-- No two distinct ledger entries hold conflicting device claims. -/
def Exclusive (L : Ledger) : Prop :=
  L.Pairwise fun a b => conflict a.2.devices b.2.devices = false

/-- If the ledger was exclusive and `admit_phase` admitted the grant, the
    post-grant ledger is exclusive. (Releases only remove owners, and a
    sublist of a pairwise list is pairwise, so releases preserve it too.) -/
theorem admit_preserves_exclusive {L owner next limits obs floors maxParked}
    (hL : Exclusive L)
    (h : admitPhase L owner next limits obs floors maxParked = .ok ()) :
    Exclusive (post L owner next) := by
  obtain ⟨-, -, hdev⟩ := admit_sound h
  unfold Exclusive post others
  rw [List.pairwise_append]
  refine ⟨hL.sublist List.filter_sublist, List.pairwise_singleton _ _, ?_⟩
  intro a ha b hb
  simp only [List.mem_singleton] at hb; subst hb
  exact hdev a ha

theorem release_preserves_exclusive {L : Ledger} (hL : Exclusive L) (o : String) :
    Exclusive (others L o) := hL.sublist List.filter_sublist

end Capy.Grant

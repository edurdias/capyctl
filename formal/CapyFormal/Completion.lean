import CapyFormal.Grant
/-!
# Completions that rewrite a footprint without admission

Two store paths replace an owner's footprint with the recipe's `ready` phase
**without** running `admit_phase`:

* `ordinary_lifecycle.rs` `complete` (start: cold → ready), and
* `park.rs` residency `complete` for a restore (wake → ready).

That is sound only if `ready` never needs anything the held phase did not
already have admitted. For bytes and host-KV, `validate_recipe` checks exactly
that (`cold ≥ ready`, `wake ≥ ready`). Until the 2026-10-01 fix it **checked
nothing for device claims**, and config validation only required each phase's
claims to be among the deployment's selected devices.

Below: the concrete counterexample in which a completion broke device
exclusivity, and a proof that the coverage rule `validate_recipe` now
enforces (`covers`) preserves it.
-/
namespace Capy.Completion
open Capy.Admission Capy.Grant

/-- The completion write: the owner's footprint becomes `ready`, nothing checked. -/
def complete (L : Ledger) (o : String) (ready : Fp) : Ledger := post L o ready

def gpu0x : Claim := ⟨"gpu0", .exclusive⟩
def alloc (b : Int) : Alloc := ⟨"unified", b, 0⟩

/-- B serves on gpu0 exclusively. -/
def B : String × Fp := ("B", ⟨.ready, [alloc 8], [gpu0x]⟩)
/-- A's declared recipe: cold claims no device, ready claims gpu0 exclusively
    (accepted by `resolve_effective` — reproduced with a probe test). -/
def coldA : Fp := ⟨.cold, [alloc 10], []⟩
def readyA : Fp := ⟨.ready, [alloc 8], [gpu0x]⟩

def before : Ledger := [B, ("A", coldA)]
def after : Ledger := complete before "A" readyA

/-- The cold reservation passes the device check that `admit_phase` / `fits` run
    (there is nothing to conflict with)… -/
theorem cold_admissible_on_devices :
    (others [B] "A").any (fun e => conflict e.2.devices coldA.devices) = false := by decide

/-- …and the ledger is exclusive before completion… -/
theorem before_exclusive : Exclusive before := by
  unfold Exclusive before; decide

/-- …but after the unchecked completion two owners hold gpu0, one exclusively. -/
theorem after_not_exclusive : ¬ Exclusive after := by
  unfold Exclusive after complete post others before; decide

/-! ## The fix: device monotonicity in `validate_recipe` -/

theorem pairwise_mem {α} {R : α → α → Prop} (symm : ∀ x y, R x y → R y x) :
    ∀ {L : List α} {a b : α}, L.Pairwise R → a ∈ L → b ∈ L → a ≠ b → R a b
  | [], _, _, _, ha, _, _ => by cases ha
  | x :: xs, a, b, hp, ha, hb, hne => by
    rw [List.pairwise_cons] at hp
    rcases List.mem_cons.mp ha with rfl | ha' <;> rcases List.mem_cons.mp hb with rfl | hb'
    · exact absurd rfl hne
    · exact hp.1 _ hb'
    · exact symm _ _ (hp.1 _ ha')
    · exact pairwise_mem symm hp.2 ha' hb' hne

/-- The rule `validate_recipe` enforces for each (peak, base) phase pair: every
    base claim is held by the peak on the same device, equally or exclusively. -/
def covers (held ready : Fp) : Prop :=
  ∀ c ∈ ready.devices, ∃ h ∈ held.devices,
    h.device = c.device ∧ (h.sharing = c.sharing ∨ h.sharing = .exclusive)

/-- A covered footprint conflicts with nobody its cover did not conflict with. -/
theorem conflict_of_covers {held ready : Fp} (hc : covers held ready) (x : List Claim)
    (h : conflict held.devices x = false) : conflict ready.devices x = false := by
  simp only [conflict, List.any_eq_false, Bool.not_eq_true, Bool.and_eq_false_iff,
    List.any_eq_true, not_exists, not_and, Bool.and_eq_true, beq_iff_eq,
    Bool.or_eq_true] at h ⊢
  intro c hcm y hy hdev hsh
  obtain ⟨g, hg, hgd, hgs⟩ := hc c hcm
  refine h g hg y hy (by rw [hgd]; exact hdev) ?_
  rcases hsh with hce | hye
  · rcases hgs with hgs | hgs
    · left; rw [hgs]; simpa using hce
    · left; simpa using hgs
  · right; exact hye

/-- If the ready phase is covered by the held phase, an unchecked completion
    preserves exclusivity. -/
theorem complete_preserves_exclusive {L : Ledger} {o : String} {held ready : Fp}
    (hL : Exclusive L) (hheld : (o, held) ∈ L) (hcov : covers held ready) :
    Exclusive (complete L o ready) := by
  unfold complete Exclusive post others
  rw [List.pairwise_append]
  refine ⟨hL.sublist List.filter_sublist, List.pairwise_singleton _ _, ?_⟩
  intro a ha b hb
  simp only [List.mem_singleton] at hb; subst hb
  have hane : a.1 ≠ o := by
    have := (List.mem_filter.mp ha).2; simpa using this
  have haL : a ∈ L := (List.mem_filter.mp ha).1
  have hpair : conflict a.2.devices held.devices = false :=
    pairwise_mem (R := fun x y : String × Fp => conflict x.2.devices y.2.devices = false)
      (fun x y h => by rw [conflict_symm]; exact h) hL haL hheld
      (fun h => hane (by simp [h]))
  rw [conflict_symm]
  exact conflict_of_covers hcov _ (by rw [conflict_symm]; exact hpair)

/-- The counterexample recipe is exactly what the rule rejects. -/
theorem counterexample_not_covered : ¬ covers coldA readyA := by
  intro h
  obtain ⟨g, hg, -⟩ := h gpu0x (by simp [readyA])
  simp [coldA] at hg

end Capy.Completion

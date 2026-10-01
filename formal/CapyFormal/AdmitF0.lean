/-!
# F0 `admit` (`crates/capyctl-scheduler/src/admission.rs`)

The capacity arithmetic of the older `admit`, which the live store no longer
uses (its one production caller, `operations.rs:1017`, passes an empty ledger).
Replace-don't-stack subtracts the candidate's **claimed** `parked_budget`, not
the bytes of the Parked reservation it actually holds, so a claim larger than
the held reservation under-charges the transition (SPEC §7.3: "validate the
transition's true peak").
-/
namespace Capy.AdmitF0

structure Res where
  owner : String
  bytes : Int
  parked : Bool

/-- `charged_bytes` + `holds_parked` + `delta` + the managed-limit check. -/
def capacityOk (ledger : List Res) (owner : String) (peak : Int) (parkedBudget : Option Int)
    (managed : Int) : Bool :=
  let charged := (ledger.map (·.bytes)).sum
  let holdsParked := ledger.any (fun r => r.owner == owner && r.parked)
  -- transition-peak check: `activation_peak < budget` rejects
  let peakOk := match parkedBudget with | some b => decide (peak ≥ b) | none => true
  let delta := peak - (if holdsParked then parkedBudget.getD 0 else 0)
  peakOk && decide (charged + delta ≤ managed)

/-- The ledger after the transition: the owner's reservations replaced by `peak`. -/
def truePeak (ledger : List Res) (owner : String) (peak : Int) : Int :=
  ((ledger.filter (·.owner != owner)).map (·.bytes)).sum + peak

def G : Int := 1024 * 1024 * 1024
def ledger : List Res := [⟨"X", 90 * G, false⟩, ⟨"C", 2 * G, true⟩]

/-- Admitted (reproduced by a probe test against the Rust function)… -/
theorem admitted : capacityOk ledger "C" (50 * G) (some (50 * G)) (100 * G) = true := by decide
/-- …although the true post-transition charge is 140 GiB against a 100 GiB limit. -/
theorem overcommits : truePeak ledger "C" (50 * G) = 140 * G ∧ 140 * G > 100 * G := by decide

/-- Using the held Parked bytes instead of the claim is sound: whenever the
    owner's only reservation is one Parked entry of `held` bytes,
    `charged + peak - held` *is* the true post-transition charge. -/
theorem held_credit_exact (others : List Res) (o : String) (held peak : Int)
    (h : ∀ r ∈ others, r.owner ≠ o) :
    let L : List Res := others ++ [(⟨o, held, true⟩ : Res)]
    (L.map (·.bytes)).sum + (peak - held) = truePeak L o peak := by
  intro L
  have hf : L.filter (·.owner != o) = others := by
    simp only [L, List.filter_append, List.filter_cons, List.filter_nil, bne_self_eq_false,
      Bool.false_eq_true, ite_false, List.append_nil]
    exact List.filter_eq_self.mpr (fun r hr => by
      have := h r hr; simpa [bne_iff_ne] using this)
  unfold truePeak
  rw [hf]; simp [L, List.sum_append]; omega

end Capy.AdmitF0

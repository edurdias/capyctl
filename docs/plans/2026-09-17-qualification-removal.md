# Qualification Removal Implementation Plan

**Goal:** Remove the candidate and qualification subsystem from mllm, keep the park contract and the other machine guards it contained, drop its schema, retire its vocabulary, and finish ADR 0011's retry budget.

**Architecture:** mllm guards the host; the user owns the recipe. Every piece of the subsystem is sorted by one test: does it stop mllm blowing the machine, or does it judge the recipe? Guards move to where the ordinary lifecycle can reach them; judgements are deleted. Work is sequenced so every commit leaves a compiling, green tree: commit the pending worker change, set document authority, move what survives, delete consumers before producers, drop the schema, retire the vocabulary, then build retry on the surviving loop.

**Tech Stack:** Rust workspace (`cargo`, `clippy -D warnings`), SQLite through `rusqlite` with forward-only migrations, `tokio` coordinator worker, `axum` management API, `serde` DTOs.

**Spec:** `docs/specs/2026-09-17-qualification-removal-design.md`. Read it first. Where this plan deviates from the spec it says so in the task and the docs task records the amendment.

## Global Constraints

- Only host `host-a` is authorized for live work. Never access `host-b`. No task in this plan touches a live host.
- Do not change engine environments, drivers, or reboot hosts. The Python files under `runtime/` are not edited by this plan.
- Native entrypoint denials stay closed. Moving the native launch handoff opens nothing; `ProfileBindings` keeps refusing SGLang.
- Excluded files, never read, edited, formatted, tested or staged: `crates/mllm-cli/tests/live_interactive.rs` and a local Task 2 implementation report. The second is modified in the worktree; do not stage it in any commit.
- `AGENTS.md` is untracked and not this plan's. Leave it alone.
- One status document: `docs/runbooks/f2-current-status.md`. No progress or continuation files anywhere under `docs/`.
- Prose in documents and commit messages is normal English.
- Cite the governing requirement inline where behavior is spec-driven, e.g. `// ADR 0011 decision 4`. Tag tests with acceptance-matrix identifiers, e.g. `// T20`.
- Uncertainty retains accounting. No task releases a reservation, advances an epoch, or replays a dispatch without verified evidence. The `Uncertain` pause in the worker is preserved verbatim throughout.
- Migrations are forward-only. Never edit `SCHEMA_V1` through `SCHEMA_V12`.
- Verification is scoped per task to the crates touched. The core suite runs once, in the last task. CPU and Fake-engine tests are not verification of any native recipe; every status claim says so.
- Commits end with the attribution lines the session provides.
- The controller library suite currently takes about 40 minutes because 49 tests fail on one fixture assertion. Do not run `cargo test -p mllm-controller` unscoped before Task 7 removes that fixture; run named tests or `--test`/`--lib` targets as each task says.

## Deviations from the spec, recorded here and in Task 2

1. **Park contract location.** The spec puts it at `crates/mllm-store/src/ordinary_lifecycle/park.rs`. The rules in `warm.rs` are inseparable from candidate SQL types, so they are re-expressed as pure functions over decoded evidence and live in `crates/mllm-domain/src/park.rs`, where ADR 0009 puts rules that must be testable with no database. Nothing in the store references it until the park ADR.
2. **The identity string.** `qualification_id` is not only a token field. It is a host YAML key (`runtime_profiles.<name>.qualification_id`), an effective-config field, a binding DTO field on disk and a `RuntimeBinding` field. Task 9 retires all of it. The YAML key is removed from the strict schema, so a host file carrying it is refused with a clear message. Standalone generates its own host file and needs no operator action.
3. **The Fake engine.** `crates/mllm-adapters/src/fake/qualification.rs` is the Fake's lifecycle simulation (persisted effects, cleanup, parked status) under a qualification name, and production adapter resolution uses it. It is kept, renamed, and stripped of its candidate-only cases in Task 10.
4. **Retry policy fields.** ADR 0011 decision 5 says the budget and cooldown are host policy fields. Host policy shape is owned by `mllm-config` under "one writer for the policy shape" (commit `7566336`) and is not touched here. Task 12 puts them on `CoordinatorOptions` with the ADR's defaults; the docs task amends decision 5 to say the host-policy plumbing follows when remote hosts publish policy (F3).
5. **Wire strings left alone.** The private descriptor `"kind": "sglang_candidate_private_launch"` is a contract with `runtime/sglang_entry.py`, and the SGLang served-name rule `candidate-{binding_id}` in `crates/mllm-adapters/src/sglang/args.rs` belongs to the undesigned ordinary native launch. Both stay and are listed in the runbook as leftovers of the native launch design.

---

### Task 1: Commit the pending worker change

The worktree holds ADR 0011 Task 3, complete and uncommitted: a failed deployment closes its own admission and the loop continues. Commit it before anything else touches `worker.rs`.

**Files:**
- Already modified: `crates/mllm-controller/src/coordinator/worker.rs` (the initialize failure branch around line 1560), `crates/mllm-store/src/deployments.rs` (`Store::set_admission_enabled`), `crates/mllm-controller/src/coordinator/tests.rs` (`a_failed_deployment_does_not_stop_the_others` and three relaxed assertions), `crates/mllm-controller/src/coordinator/tests_cleanup.rs`.
- Not in this commit: `docs/design/adr/0011-the-state-machine-owns-recovery.md` (Task 2), the excluded report, `AGENTS.md`.

**Interfaces:**
- Produces: `Store::set_admission_enabled(&self, id: &str, enabled: bool) -> Result<(), StoreError>`; a worker loop that survives a `Failed` or `Blocked` step. Task 12 builds retry on it.

- [ ] **Step 1: Read the diff**

Run: `git diff crates/mllm-controller/src/coordinator/worker.rs crates/mllm-store/src/deployments.rs crates/mllm-controller/src/coordinator/tests.rs crates/mllm-controller/src/coordinator/tests_cleanup.rs`

Confirm the failure branch matches ADR 0011 decision 4: only `!shared.accepting` or `*stop.borrow()` returns; `Uncertain` pauses exactly as before; any other outcome calls `set_admission_enabled(id, false)`, sets initializing back on, and continues.

- [ ] **Step 2: Run the new test and the coordinator unit tests**

Run: `cargo test --offline -p mllm-controller --lib -- coordinator::tests::a_failed_deployment_does_not_stop_the_others coordinator::tests_cleanup --test-threads=4`
Expected: pass. If a test in `tests.rs` or `tests_cleanup.rs` other than the 49 fixture-driven failures fails, it is asserting the old close-everything behaviour. Read it, decide whether the failure it drives is one deployment's or process-wide (poisoned mutex, corrupt store), and change only the per-deployment ones. State which in the commit message.

- [ ] **Step 3: Commit**

```bash
git add crates/mllm-controller/src/coordinator/worker.rs crates/mllm-store/src/deployments.rs crates/mllm-controller/src/coordinator/tests.rs crates/mllm-controller/src/coordinator/tests_cleanup.rs
git commit -m "feat(controller): a failed deployment stops itself, not the host

The worker returned from its loop on any outcome that was not uncertain, and the
task then closed admission for every deployment on the host. One deployment failing
to initialize stopped everything, which is a blast radius nobody chose.

A failure now closes that deployment's own admission through the new
Store::set_admission_enabled and the loop continues. An uncertain outcome is
unchanged: it still holds the retained binding and waits for an explicit Stop,
because nobody knows whether the effect landed."
```

---

### Task 2: Documents set the authority

Docs first, so code moves against an amended SPEC and ADR rather than ahead of them.

**Files:**
- Modify: `docs/design/adr/0011-the-state-machine-owns-recovery.md` (already partly edited in the worktree), `docs/SPEC.md`, `docs/design/adr/0009-proof-carrying-reconciliation.md`, `docs/design/milestones/f2-sglang-design.md`, `docs/plans/2026-09-12-f2-planning-index.md`, `docs/README.md`, `docs/specs/2026-09-17-qualification-removal-design.md`
- Delete: `docs/plans/2026-09-12-f2c-mixed-engine-qualification.md`, `docs/plans/2026-09-14-f2-native-candidate-handoff.md`, `docs/plans/2026-09-16-state-machine-owns-recovery.md`, `docs/runbooks/f2-mixed-engine-qualification.md`, `docs/runbooks/f2-sglang-qualification.md`, `docs/AGENT_HANDOFF.md`

- [ ] **Step 1: ADR 0011**

Keep the worktree edit of decision 2 and its consequences. Then:

Replace the first paragraph of decision 2 with:

```markdown
**2. Qualification is not an mllm concept and is removed.** mllm guards the host;
the user owns the recipe. The concept exists in another product and was carried into
this one by mistake. Nothing is relocated and no interface to another product is
designed. Before launch mllm checks recipe shape (SPEC §8.2) and host fit
(ADR 0007); it then launches and watches readiness. A recipe that does not work
surfaces as failed attempts and, after the budget in decision 5, a terminal
`Failed` deployment.
```

Add after the salvage-types paragraph:

```markdown
What survives is what guards the machine: the park contract in
`candidate_creation/warm.rs`, which is the only definition of "parked" in the
repository (no allocations, weights or cache; quiesced; no unknown work; activity
counters unmoved; no outstanding request lease; identities equal to the owned
association; then milestone verification), the Fake engine's lifecycle simulation,
and the native launch handoff. The design at
`docs/specs/2026-09-17-qualification-removal-design.md` sorts every piece.
```

In decision 5, replace "Both are host policy fields with these defaults, not constants, because a host with slower storage may need a longer cooldown." with:

```markdown
Both are policy, not constants, because a host with slower storage may need a longer
cooldown. They are coordinator options with these defaults today; the host-policy
plumbing follows when remote hosts publish policy (F3), since the policy shape has
one writer in `mllm-config`.
```

Under "What this does not decide", add:

```markdown
**The ordinary native launch is not designed here.** The handoff that renders a
protected SGLang launch survives as `NativeLaunchHandoff`, sourced through a trait
the application implements. Nothing implements it yet, `ProfileBindings` still
refuses SGLang, and the private descriptor's `sglang_candidate_private_launch` tag
and the served-name rule `candidate-{binding_id}` stay until that design.
```

- [ ] **Step 2: SPEC edits, each citing ADR 0011**

Make these edits in `docs/SPEC.md`. Line numbers are as of `9687205`; find the text, not the line.

- §1.1 R08 (line 46): "where a complete release/restore path is qualified" becomes "where the declared residency tier can be delivered (ADR 0010, ADR 0011)".
- §6.2 (line 213): "backends without qualified memory-release APIs" becomes "backends without a verified memory-release API".
- §6.3 rows (lines 223 and 224): "require qualified parking" becomes "park at the declared tier (ADR 0010)"; "perform qualified parking" becomes "park at the declared tier".
- §6.4 (line 235): remove ", qualification" from the status field list.
- §7.5 (line 306): "Prefer fixed qualified allocations initially" becomes "Prefer fixed, verified allocations initially".
- §8.1 last paragraph (line 322): "reuse old qualification results" becomes "reuse a superseded binding identity". Append to that paragraph: "mllm validates a recipe's shape and the host's capacity to hold it; whether the recipe works is the user's responsibility (ADR 0011)."
- §8.4 (lines 346 to 351): replace heading and body with:

```markdown
### 8.4 Withdrawn

Capability qualification was withdrawn by ADR 0011. The section number is kept so
that references in §20 remain stable. Recipe ownership is stated in §8.1.
```

- §9.2 (line 364): "Qualify its actual release, retained-copy, and restoration behavior" becomes "Verify its actual release, retained-copy, and restoration behavior on authorized hardware". Line 366: "when deep parking is not qualified" becomes "when the declared tier is `restart_only`". Lines 368 to 370: "the owner requires qualified parking and restoration for a selected recipe of each engine" becomes "the owner requires parking and restoration verified live on authorized hardware for a selected recipe of each engine".
- §13.2 (line 452): "Repeated failures disable the profile's parking capability until requalification." becomes "Repeated failures exhaust the deployment's attempt budget and leave it terminal `Failed` with its own admission closed (ADR 0011 decision 5)."
- §14 (line 484): delete the line `mllm qualify deployment dep_example`. Line 509: remove ", qualification" from the required operations list.
- §15.1 (line 523): "reservations and qualification results" becomes "reservations and operational evidence".
- §17 (line 789): remove ", qualification reasons".
- §18 F2 row (line 805): "mandatory qualified parking" becomes "declared-tier parking"; "selected-recipe live qualification" becomes "live verification of the selected recipes on authorized hardware". F4 row (line 807): "Distributed and cache qualification" becomes "Distributed and cache verification".
- §19 (line 832): "production-qualified host recipes" becomes "production-verified host recipes"; "Qualification and the development-endpoint security issue are release gates" becomes "Live verification on authorized hardware and the development-endpoint security issue are release gates".
- §20 T14 (line 853): remove "; old qualification invalidated" and append "; a superseded binding identity is not reused". T36 (line 875): "qualified fallback" becomes "declared fallback". Line 881: "a qualified experimental deep-park path where permitted" becomes "a live-verified deep-park path where permitted".
- §21 (line 905): "retain qualification evidence" becomes "retain operational evidence".

Then run: `grep -n -i "qualif" docs/SPEC.md`
Expected: only the §8.4 withdrawal text and the revision-history line 6 remain. Anything else is a miss; fix it.

- [ ] **Step 3: ADR 0009 note**

After the paragraph beginning "Qualification becomes an authority on the one lifecycle", add:

```markdown
> Superseded by ADR 0011 (2026-09-17): qualification is not an mllm concept.
> `candidate_creation` is deleted rather than collapsed; consequence 4 below is
> discharged by that deletion.
```

- [ ] **Step 4: F2 design**

In `docs/design/milestones/f2-sglang-design.md` §6, retitle to "## 6. Failure, recovery, and engine verification" and replace the last paragraph ("Advertise capabilities as qualified, unknown, unsupported, or disabled ... missing qualified park path.") with:

```markdown
Capability qualification was withdrawn by ADR 0011: mllm guards the host and the
user owns the recipe. Security permission remains a separate check. F2 closes on
declared-tier parking verified live on authorized hardware, not on a proof step per
deployment.
```

In §8, row Q11 becomes "| Q11 — Engine verification | Pinned recipe for each engine passes readiness, parking, restoration, cache-correctness and security gates live on authorized hardware; unknown combinations remain unsupported |". Replace "Larger model recipes are qualified separately" with "Larger model recipes are verified separately" and "qualifying one recipe does not qualify every model" with "verifying one recipe does not verify every model".

- [ ] **Step 5: Deletions and index**

```bash
git rm docs/plans/2026-09-12-f2c-mixed-engine-qualification.md \
       docs/plans/2026-09-14-f2-native-candidate-handoff.md \
       docs/plans/2026-09-16-state-machine-owns-recovery.md \
       docs/runbooks/f2-mixed-engine-qualification.md \
       docs/runbooks/f2-sglang-qualification.md \
       docs/AGENT_HANDOFF.md
```

In `docs/plans/2026-09-12-f2-planning-index.md`, delete the F2C row (line 18). In `docs/README.md`, delete the sentence "Start with **`AGENT_HANDOFF.md`**, then read **`SPEC.md`** before implementation planning." and replace with "Start with `AGENTS.md`, then `SPEC.md`."; change "Live mixed-engine qualification remains pending." to "Live mixed-engine verification remains pending." and "hardware qualification claim" to "hardware verification claim".

Run: `grep -rn "AGENT_HANDOFF\|f2c-mixed-engine-qualification\|f2-native-candidate-handoff\|state-machine-owns-recovery.md\|f2-sglang-qualification\|f2-mixed-engine-qualification" docs AGENTS.md`
Expected: no hits outside `docs/runbooks/f2-current-status.md` (fixed in Task 13) and the spec's own §5 list.

- [ ] **Step 6: Amend the design doc**

In `docs/specs/2026-09-17-qualification-removal-design.md`, add a section "## 9. Amendments during planning" listing the five deviations from the top of this plan, one sentence each.

- [ ] **Step 7: Commit**

```bash
git add docs
git commit -m "docs: qualification is not an mllm concept

ADR 0011 decision 2 now states the principle: mllm guards the host and the user
owns the recipe. SPEC §8.4 is withdrawn and every other qualification reference is
reworded to declared tiers or live verification. ADR 0009's candidate-authority
paragraph is marked superseded, the F2 design's qualification gate points at ADR
0011, and the plans and runbooks whose subject was the deleted code are removed."
```

---

### Task 3: The park contract as a pure domain module

Re-express what `crates/mllm-store/src/candidate_creation/warm.rs` knows about parking as pure functions with no SQL, in the domain crate, with tests. This is additive; nothing calls it yet.

**Files:**
- Create: `crates/mllm-domain/src/park.rs`
- Modify: `crates/mllm-domain/src/lib.rs` (add `pub mod park;`)
- Reference while writing: `crates/mllm-store/src/candidate_creation/warm.rs` lines 32 to 70 (`facts`, `milestone`, `kinds`), 134 to 165 (`predecessors`), 186 to 240 (`join`), 940 to 1016 (the parked predicate); `crates/mllm-store/src/candidate_creation/progression.rs` line 39 (`PersistedEffectKind`) and line 50 (`Fact`).

**Interfaces:**
- Produces: the types and functions below. The park ADR wires them; Task 10's Fake `parked_status` returns the observation type from `completion.rs`, not from here.

- [ ] **Step 1: Write the failing tests**

Create `crates/mllm-domain/src/park.rs` with the tests first:

```rust
//! What "parked" means, and the order in which a park or a restore is performed.
//!
//! ADR 0011: parking is an ordinary transition and needs no qualification, but the
//! machine must not be trusted to have released memory because an engine said so.
//! The rules here are the guard: a park is complete only when a fresh local
//! observation shows nothing resident, nothing running and the same owned
//! processes, and a restore may only be armed on the strength of a park whose
//! evidence committed. Nothing here touches a database; the store applies these
//! rules inside its transactions.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::{Milestone, ProcessIdentity};

    fn identity() -> ProcessIdentity {
        ProcessIdentity { role: "api".into(), pid: 7, boot_id: "boot".into(), start_ticks: 1 }
    }

    fn parked() -> ParkedStatus {
        ParkedStatus {
            identities: vec![identity()],
            observed_at_ms: 2_000,
            receipt: "status".into(),
            allocations: false,
            weights: false,
            cache: false,
            quiesced: true,
            unknown_work: false,
            activity_before: (1, 1, 1),
            activity_after: (1, 1, 1),
        }
    }

    fn owned() -> ParkedExpectation {
        ParkedExpectation {
            owned: vec![identity()],
            park_committed_at_ms: 1_500,
            outstanding_request_leases: 0,
        }
    }

    /// The park effect sequence is fixed: drain, then park. A restore reloads
    /// weights and invalidates cache before it probes.
    #[test]
    fn effects_are_ordered_and_each_yields_its_facts() {
        assert_eq!(effects(ParkAction::Park), [Effect::Drain, Effect::Park]);
        assert_eq!(
            effects(ParkAction::Restore),
            [Effect::Restore, Effect::ReloadWeights, Effect::InvalidateCache, Effect::Probe]
        );
        assert_eq!(facts(Effect::Drain), Ok(vec![Milestone::Quiesced]));
        assert_eq!(facts(Effect::Park), Ok(vec![Milestone::MemoryReleased]));
        assert_eq!(facts(Effect::Restore), Ok(vec![Milestone::AllocationsRestored]));
        assert_eq!(facts(Effect::ReloadWeights), Ok(vec![Milestone::WeightsUsable]));
        assert_eq!(facts(Effect::InvalidateCache), Ok(vec![Milestone::CacheValid]));
        assert_eq!(facts(Effect::Probe), Err(ParkError::ProbeCarriesNoFacts));
    }

    /// A later effect is armed only if every earlier effect committed the exact
    /// facts expected, in time order, at strictly increasing epochs. T20
    #[test]
    fn predecessors_must_have_committed_in_order() {
        let drained = CommittedEffect {
            effect: Effect::Drain,
            facts: vec![Milestone::Quiesced],
            observed_at_ms: 1_000,
            committed_epoch: 5,
        };
        let parked = CommittedEffect {
            effect: Effect::Park,
            facts: vec![Milestone::MemoryReleased],
            observed_at_ms: 1_200,
            committed_epoch: 6,
        };
        assert_eq!(
            validate_predecessors(ParkAction::Park, 2, &[drained.clone()], 900, 1_300),
            Ok(vec![Milestone::Quiesced])
        );
        // Epoch did not advance.
        let stale = CommittedEffect { committed_epoch: 5, ..parked.clone() };
        assert_eq!(
            validate_predecessors(ParkAction::Restore, 2, &[drained.clone(), stale], 900, 1_300)
                .unwrap_err(),
            ParkError::PredecessorMismatch
        );
        // Observed after the effect being armed.
        assert_eq!(
            validate_predecessors(ParkAction::Park, 2, &[drained.clone()], 900, 999).unwrap_err(),
            ParkError::PredecessorMismatch
        );
        // Wrong facts for the effect.
        let wrong = CommittedEffect { facts: vec![Milestone::MemoryReleased], ..drained };
        assert_eq!(
            validate_predecessors(ParkAction::Park, 2, &[wrong], 900, 1_300).unwrap_err(),
            ParkError::PredecessorMismatch
        );
    }

    /// A footprint join never shrinks the retained reservation and never drops a
    /// domain the owner already holds.
    #[test]
    fn join_only_grows() {
        use crate::resources::{Allocation, PhaseFootprint, ResourcePhase};
        let mut base = PhaseFootprint {
            phase: ResourcePhase::Ready,
            allocations: vec![Allocation { domain: "unified".into(), bytes: 10, host_kv_bytes: 2 }],
            devices: vec![],
        };
        let peak = PhaseFootprint {
            phase: ResourcePhase::Parking,
            allocations: vec![Allocation { domain: "unified".into(), bytes: 4, host_kv_bytes: 8 }],
            devices: vec![],
        };
        join(&mut base, &peak).unwrap();
        assert_eq!(base.allocations[0].bytes, 10);
        assert_eq!(base.allocations[0].host_kv_bytes, 8);
        let foreign = PhaseFootprint {
            phase: ResourcePhase::Parking,
            allocations: vec![Allocation { domain: "other".into(), bytes: 1, host_kv_bytes: 0 }],
            devices: vec![],
        };
        assert_eq!(join(&mut base, &foreign).unwrap_err(), ParkError::UnknownDomain);
    }

    /// The parked predicate: nothing resident, nothing running, same processes,
    /// no work leased, observed after the park committed. Any one failing means
    /// the deployment is not parked, whatever the engine reported. T20
    #[test]
    fn parked_requires_every_condition() {
        assert_eq!(verify_parked(&parked(), &owned()), Ok(()));
        let cases: Vec<(&str, ParkedStatus, ParkedExpectation)> = vec![
            ("allocations", ParkedStatus { allocations: true, ..parked() }, owned()),
            ("weights", ParkedStatus { weights: true, ..parked() }, owned()),
            ("cache", ParkedStatus { cache: true, ..parked() }, owned()),
            ("quiesced", ParkedStatus { quiesced: false, ..parked() }, owned()),
            ("unknown work", ParkedStatus { unknown_work: true, ..parked() }, owned()),
            ("activity", ParkedStatus { activity_after: (2, 1, 1), ..parked() }, owned()),
            ("receipt", ParkedStatus { receipt: String::new(), ..parked() }, owned()),
            ("stale", ParkedStatus { observed_at_ms: 1_000, ..parked() }, owned()),
            (
                "identities",
                ParkedStatus { identities: vec![], ..parked() },
                owned(),
            ),
            (
                "leases",
                parked(),
                ParkedExpectation { outstanding_request_leases: 1, ..owned() },
            ),
        ];
        for (name, status, expectation) in cases {
            assert!(verify_parked(&status, &expectation).is_err(), "{name} must refuse");
        }
    }
}
```

- [ ] **Step 2: Run the tests and watch them fail to compile**

Run: `cargo test --offline -p mllm-domain park`
Expected: compile errors, the types do not exist yet.

- [ ] **Step 3: Implement the module above the tests**

```rust
use crate::completion::{Milestone, ProcessIdentity};
use crate::resources::PhaseFootprint;

/// The two park-family actions the ordinary lifecycle will perform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParkAction {
    Park,
    Restore,
}

/// One engine effect. Each is its own persisted step with its own evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Drain,
    Park,
    Restore,
    ReloadWeights,
    InvalidateCache,
    Probe,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParkError {
    /// A probe is a read; it proves usability through the completion path, not facts.
    ProbeCarriesNoFacts,
    /// An earlier effect is missing, out of order, wrong, or did not advance the epoch.
    PredecessorMismatch,
    /// A phase peak names a domain the owner does not hold.
    UnknownDomain,
    /// The observation does not show a parked runtime.
    NotParked(&'static str),
}

/// The fixed order of effects for an action.
pub fn effects(action: ParkAction) -> &'static [Effect] {
    match action {
        ParkAction::Park => &[Effect::Drain, Effect::Park],
        ParkAction::Restore => &[
            Effect::Restore,
            Effect::ReloadWeights,
            Effect::InvalidateCache,
            Effect::Probe,
        ],
    }
}

/// The facts one committed effect establishes.
pub fn facts(effect: Effect) -> Result<Vec<Milestone>, ParkError> {
    Ok(match effect {
        Effect::Drain => vec![Milestone::Quiesced],
        Effect::Park => vec![Milestone::MemoryReleased],
        Effect::Restore => vec![Milestone::AllocationsRestored],
        Effect::ReloadWeights => vec![Milestone::WeightsUsable],
        Effect::InvalidateCache => vec![Milestone::CacheValid],
        Effect::Probe => return Err(ParkError::ProbeCarriesNoFacts),
    })
}

/// What the store reads back for an earlier effect of the same action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommittedEffect {
    pub effect: Effect,
    pub facts: Vec<Milestone>,
    pub observed_at_ms: i64,
    pub committed_epoch: u64,
}

/// Before arming effect number `ordinal` (1-based) of `action`, every earlier
/// effect must have committed with exactly its facts, observed no earlier than the
/// action's acceptance and no later than `observed_at_ms`, at strictly increasing
/// epochs. Returns the accumulated facts on success.
pub fn validate_predecessors(
    action: ParkAction,
    ordinal: usize,
    committed: &[CommittedEffect],
    accepted_at_ms: i64,
    observed_at_ms: i64,
) -> Result<Vec<Milestone>, ParkError> {
    let expected = effects(action);
    if ordinal == 0 || ordinal > expected.len() || committed.len() != ordinal - 1 {
        return Err(ParkError::PredecessorMismatch);
    }
    let mut collected = Vec::new();
    let mut previous_time = accepted_at_ms;
    let mut previous_epoch = 0;
    for (effect, evidence) in expected.iter().zip(committed) {
        if evidence.effect != *effect
            || evidence.facts != facts(*effect)?
            || evidence.observed_at_ms < previous_time
            || evidence.observed_at_ms > observed_at_ms
            || evidence.committed_epoch <= previous_epoch
        {
            return Err(ParkError::PredecessorMismatch);
        }
        previous_time = evidence.observed_at_ms;
        previous_epoch = evidence.committed_epoch;
        collected.extend(evidence.facts.iter().copied());
    }
    Ok(collected)
}

/// Join a phase peak into the owner's retained footprint. Bytes and host KV take
/// the maximum per domain; device sharing can only escalate to exclusive. The
/// retained reservation never shrinks here, because a smaller number would be a
/// claim about released memory that only a parked-status observation can make.
pub fn join(base: &mut PhaseFootprint, peak: &PhaseFootprint) -> Result<(), ParkError> {
    for next in &peak.allocations {
        let old = base
            .allocations
            .iter_mut()
            .find(|a| a.domain == next.domain)
            .ok_or(ParkError::UnknownDomain)?;
        old.bytes = old.bytes.max(next.bytes);
        old.host_kv_bytes = old.host_kv_bytes.max(next.host_kv_bytes);
    }
    for next in &peak.devices {
        if let Some(old) = base.devices.iter_mut().find(|d| d.device == next.device) {
            if next.sharing == crate::resources::Sharing::Exclusive {
                old.sharing = next.sharing;
            }
        } else {
            base.devices.push(next.clone());
        }
    }
    Ok(())
}

/// A local, read-only observation of a runtime believed parked. No engine command
/// and no inference request is issued to take it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkedStatus {
    pub identities: Vec<ProcessIdentity>,
    pub observed_at_ms: i64,
    pub receipt: String,
    pub allocations: bool,
    pub weights: bool,
    pub cache: bool,
    pub quiesced: bool,
    pub unknown_work: bool,
    pub activity_before: (u64, u64, u64),
    pub activity_after: (u64, u64, u64),
}

/// What the store knows independently of the observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkedExpectation {
    /// The processes recorded in the owned-launch association.
    pub owned: Vec<ProcessIdentity>,
    /// When the Park effect's own evidence was observed.
    pub park_committed_at_ms: i64,
    /// Rows in `request_leases` for this deployment.
    pub outstanding_request_leases: u64,
}

/// SPEC §6.1 PARKED: "release conditions verified". This is the verification.
pub fn verify_parked(status: &ParkedStatus, expected: &ParkedExpectation) -> Result<(), ParkError> {
    let mut observed = status.identities.clone();
    observed.sort();
    let mut owned = expected.owned.clone();
    owned.sort();
    if observed.is_empty() || observed != owned {
        return Err(ParkError::NotParked("identities differ from the owned association"));
    }
    if status.allocations {
        return Err(ParkError::NotParked("allocations still resident"));
    }
    if status.weights {
        return Err(ParkError::NotParked("weights still resident"));
    }
    if status.cache {
        return Err(ParkError::NotParked("cache still resident"));
    }
    if !status.quiesced {
        return Err(ParkError::NotParked("engine not quiesced"));
    }
    if status.unknown_work {
        return Err(ParkError::NotParked("unknown work observed"));
    }
    if status.activity_before != status.activity_after {
        return Err(ParkError::NotParked("activity counters moved during observation"));
    }
    if status.receipt.is_empty() {
        return Err(ParkError::NotParked("empty receipt"));
    }
    if status.observed_at_ms < expected.park_committed_at_ms {
        return Err(ParkError::NotParked("observation predates the park effect"));
    }
    if expected.outstanding_request_leases != 0 {
        return Err(ParkError::NotParked("request leases outstanding"));
    }
    Ok(())
}
```

Freshness against the host TTL stays in the store, where the TTL lives; this module checks ordering only. `ProcessIdentity` derives `Ord`, so the sort in `verify_parked` compiles as written.

- [ ] **Step 4: Run the tests**

Run: `cargo test --offline -p mllm-domain park`
Expected: 4 passed.

- [ ] **Step 5: Clippy on the crate and commit**

Run: `cargo clippy --offline -p mllm-domain --all-targets -- -D warnings`

```bash
git add crates/mllm-domain/src/park.rs crates/mllm-domain/src/lib.rs
git commit -m "feat(domain): the park contract as pure rules

What the candidate warm path knew about parking, expressed without SQL: the fixed
effect order for park and restore, the facts each effect establishes, the rule that
a later effect is armed only on predecessors that committed in order at increasing
epochs, the footprint join that never shrinks a reservation, and the parked-status
predicate that is the only definition of parked in the repository. Nothing calls it
yet; the ordinary park design will."
```

---

### Task 4: Move the survivors out of the candidate module

Types and constants the ordinary path or the adapters import from inside the deletion scope. After this task the candidate module imports them from their new homes, so deleting it later removes no definition anyone else needs.

**Files:**
- Modify: `crates/mllm-store/src/lifecycle.rs` (gains `ArmResult`), `crates/mllm-store/src/ordinary_lifecycle/cleanup.rs` (gains `CleanupMode`, `CleanupExecutionContext`), `crates/mllm-store/src/candidate_creation/initialize.rs`, `crates/mllm-store/src/candidate_creation/cleanup.rs` (definitions replaced by `pub use`), `crates/mllm-store/src/ordinary_lifecycle.rs:7`, `crates/mllm-controller/src/coordinator.rs:6`, `crates/mllm-controller/src/coordinator/worker.rs:13,1825`, `crates/mllm-controller/src/coordinator/tests.rs:1469`, `crates/mllm-controller/src/coordinator/tests_cleanup.rs:172`, `crates/mllm-controller/tests/coordinator.rs:2`, `crates/mllm-controller/src/runtime.rs:190`, `crates/mllm-controller/tests/qualification_support/fixture.rs:11`
- Create: `crates/mllm-config/src/effective/sglang.rs`
- Modify: `crates/mllm-config/src/effective.rs` (add `pub mod sglang;`), `crates/mllm-config/src/effective/candidate.rs:9-11`, `crates/mllm-adapters/src/sglang/args.rs:10-12`, `crates/mllm-adapters/tests/sglang_args.rs`
- Rename in `crates/mllm-domain/src/launch.rs`: `NativeCandidateMetadata` to `NativeLaunchMetadata`, `NativeCandidateLaunch` to `NativeLaunch`; update every user (`crates/mllm-adapters/src/sglang/args.rs`, `sglang/adapter.rs`, `resolve.rs`, `tests/sglang_args.rs`, `tests/sglang_control.rs`, `tests/engine_contract.rs`, `crates/mllm-controller/src/runtime.rs`, `crates/mllm-store/src/candidate_creation/initialize.rs`).

**Interfaces:**
- Produces: `mllm_store::lifecycle::ArmResult`, `mllm_store::ordinary_lifecycle::cleanup::{CleanupMode, CleanupExecutionContext}`, `mllm_config::effective::sglang::{NATIVE_SGLANG_SOURCE_REVISION, NATIVE_CHECKPOINT_REVISION, NATIVE_SGLANG_RECIPE}`, `mllm_domain::launch::{NativeLaunch, NativeLaunchMetadata}`. Tasks 5, 7 and 8 rely on these paths.

- [ ] **Step 1: `ArmResult`**

Cut the enum from `crates/mllm-store/src/candidate_creation/initialize.rs` lines 27 to 31 and paste it into `crates/mllm-store/src/lifecycle.rs` after `DeploymentFence`:

```rust
/// What a persisted arm returned. Only `New` permits a send; `AlreadyRecorded` is a
/// replay and carries no authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArmResult {
    New { step_id: String },
    AlreadyRecorded,
}
```

In `initialize.rs`, replace the definition with `pub use crate::lifecycle::ArmResult;` so the candidate module keeps compiling until Task 7. Change every import listed above from `candidate_creation::initialize::ArmResult` to `lifecycle::ArmResult`.

- [ ] **Step 2: `CleanupMode` and `CleanupExecutionContext`**

Cut both types (with their derives and doc comments) from `crates/mllm-store/src/candidate_creation/cleanup.rs` lines 19 to about 35 into `crates/mllm-store/src/ordinary_lifecycle/cleanup.rs`, above the first use. Leave `pub use crate::ordinary_lifecycle::cleanup::{CleanupExecutionContext, CleanupMode};` in the candidate file. Update the imports in `ordinary_lifecycle/cleanup.rs:5`, `coordinator/worker.rs:13,1825`, `coordinator/tests.rs:1469`.

Record what `CleanupMode::InspectOwnedGone` does (read `candidate_creation/cleanup.rs` around line 184): it is the mode for a cleanup whose terminate already went out, so only inspection follows. Grep: `grep -rn "InspectOwnedGone" crates --include=*.rs`. If only candidate files reach it, keep the variant but add a doc comment that the ordinary path does not yet arm it and SPEC §13.2 restart recovery will; the variant is the spec's requirement even if no caller exists yet. Do not delete it.

- [ ] **Step 3: SGLang pins**

Create `crates/mllm-config/src/effective/sglang.rs`:

```rust
//! The pinned SGLang recipe the adapter renders. These identify the build and
//! checkpoint the native launch contract was written against; they are not
//! evidence that anything works (AGENTS.md: Fake tests are not qualification).

pub const NATIVE_SGLANG_SOURCE_REVISION: &str = "fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1";
pub const NATIVE_CHECKPOINT_REVISION: &str = "cdbee75f17c01a7cc42f958dc650907174af0554";
pub const NATIVE_SGLANG_RECIPE: &str = "qwen3_4b_instruct2507_tp1_dp1_bf16_disk_reload_v1";
```

Add `pub mod sglang;` to `crates/mllm-config/src/effective.rs` beside `pub mod candidate;`. In `effective/candidate.rs` replace the three `pub const` lines with `pub use super::sglang::{NATIVE_CHECKPOINT_REVISION, NATIVE_SGLANG_RECIPE, NATIVE_SGLANG_SOURCE_REVISION};`. In `crates/mllm-adapters/src/sglang/args.rs:10` import from `mllm_config::effective::sglang`. In `crates/mllm-store/src/candidate_creation/initialize.rs:118` likewise.

- [ ] **Step 4: Rename the native launch types**

In `crates/mllm-domain/src/launch.rs` rename `NativeCandidateMetadata` to `NativeLaunchMetadata` and `NativeCandidateLaunch` to `NativeLaunch`; reword the two doc comments to drop "candidate" ("Redacted native launch description", "Trusted, process-local projection of a persisted launch descriptor"). Then:

Run: `grep -rln "NativeCandidateLaunch\|NativeCandidateMetadata" crates | xargs sed -i 's/NativeCandidateMetadata/NativeLaunchMetadata/g; s/NativeCandidateLaunch/NativeLaunch/g'`

Run: `cargo check --offline --workspace --all-targets`
Expected: clean. Sed only renamed identifiers; if a doc comment now reads oddly, fix the comment.

- [ ] **Step 5: Scoped tests**

Run: `cargo test --offline -p mllm-domain -p mllm-config -p mllm-adapters -p mllm-store --all-targets -- --test-threads=4`
Expected: pass.
Run: `cargo test --offline -p mllm-controller --test runtime_binding --test coordinator`
Expected: pass.

- [ ] **Step 6: Commit**

```bash
git add crates
git commit -m "refactor: move what the ordinary path imports out of the candidate module

ArmResult moves to lifecycle, CleanupMode and CleanupExecutionContext to ordinary
cleanup, the SGLang recipe pins to effective::sglang, and the native launch types
lose their candidate prefix. The candidate module re-exports each so nothing
changes for it; when it is deleted, nothing else loses a definition."
```

---

### Task 5: The native launch handoff sources its descriptor through a trait

`NativeCandidateHandoff` is the only code that renders a protected SGLang launch. It reads its frozen descriptor and execution context from candidate store rows. Re-source both: the execution context from the ordinary store, the frozen descriptor from a trait the application implements.

**Files:**
- Modify: `crates/mllm-controller/src/runtime.rs` lines 150 to 335
- Modify: `crates/mllm-controller/tests/runtime_binding.rs` (the `native_candidate_*` tests, lines 145 to 675)
- Reference: `crates/mllm-store/src/ordinary_lifecycle/worker.rs:119` (`arm_qualified_initialize_with_context`), `crates/mllm-store/src/ordinary_lifecycle.rs:648` (`qualified_initialize_execution`)

**Interfaces:**
- Consumes: `mllm_domain::launch::NativeLaunch` (Task 4); `Store::arm_qualified_initialize_with_context(&self, &CoordinatorSession, &str, AdmissionContext) -> Result<(ArmResult, Option<StepExecutionContext>), LifecycleError>`; `Store::qualified_initialize_execution(&self, &CoordinatorSession, &str) -> Result<StepExecutionContext, LifecycleError>`.
- Produces:

```rust
/// Where an armed ordinary initialize's frozen native launch comes from. The store
/// holds no descriptor for it (the candidate rows that did are gone); the
/// application supplies one and must return the same value on every call for the
/// same step, or the handoff refuses to send.
pub trait NativeLaunchSource: Send + Sync {
    fn frozen(
        &self,
        session: &CoordinatorSession,
        step_id: &str,
        now_ms: i64,
    ) -> Result<NativeLaunch, RuntimeError>;
}

pub struct NativeLaunchService<'a> {
    pub wrapper: &'a std::path::Path,
    pub now_ms: &'a dyn Fn() -> Result<i64, RuntimeError>,
    pub source: &'a dyn NativeLaunchSource,
}

impl<'a> NativeLaunchHandoff<'a> {
    pub fn arm(
        store: &'a mllm_store::Store,
        session: &'a CoordinatorSession,
        step_id: &str,
        context: AdmissionContext<'_>,
        resolve: &dyn Fn(&str) -> Result<Vec<u8>, RuntimeError>,
        preflight: &dyn Fn(&NativeLaunch) -> Result<(), RuntimeError>,
        service: NativeLaunchService<'a>,
    ) -> Result<Option<Self>, RuntimeError>;
    pub fn command(&self) -> &RenderedCommand;
    pub fn spawn(self, launcher: &DurableSpawn) -> Result<DurableSpawnOutcome, RuntimeError>;
}
```

- [ ] **Step 1: Rewrite the tests' native source**

In `crates/mllm-controller/tests/runtime_binding.rs`, the fixture (`new()` at line 28) builds a candidate run and imports a qualification policy. Replace that with an ordinary managed deployment whose initialize is accepted, following how `crates/mllm-controller/tests/qualification_support/ordinary_initialize.rs` accepts one (it will move in Task 6; read it now). Add a test source:

```rust
struct FixedSource(std::sync::Mutex<Option<NativeLaunch>>);
impl mllm_controller::runtime::NativeLaunchSource for FixedSource {
    fn frozen(&self, _: &CoordinatorSession, _: &str, _: i64) -> Result<NativeLaunch, RuntimeError> {
        self.0.lock().unwrap().clone().ok_or(RuntimeError::Uncertain("no descriptor".into()))
    }
}
```

`NativeLaunch` has no `Clone`; add `#[derive(Clone)]` to it in `crates/mllm-domain/src/launch.rs` (it deliberately has no `Debug`, `Display` or serialization; `Clone` does not leak). Rename every `native_candidate_*` test to `native_launch_*` and keep each one's property:

- policy revoked during preflight rejects the handoff: becomes "the source returning a different descriptor during preflight rejects the handoff" (mutate the `FixedSource` inside `preflight`).
- deadline advanced during callbacks rejects: unchanged, deadline comes from the ordinary execution context.
- revocation and deadline checked before spawn and acknowledgement: unchanged shape.
- clock failure redacted and reservations retained: unchanged.
- wrapper permissions rechecked: unchanged.
- single use, secret free, ordinary dispatch stays closed: the assertion `private["kind"] == "sglang_candidate_private_launch"` stays (deviation 5).
- preflight failure consumes authority: unchanged.
- stale session, stale generation, changed binding cannot spawn: unchanged.
- ambiguous API association retains grant: unchanged.
- invalid credentials never create a handoff: unchanged.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --offline -p mllm-controller --test runtime_binding native_launch`
Expected: compile errors, the trait and renamed types do not exist.

- [ ] **Step 3: Rewrite the handoff**

In `crates/mllm-controller/src/runtime.rs`:

- Rename `NativeCandidateHandoff` to `NativeLaunchHandoff`, `NativeCandidateService` to `NativeLaunchService`, add the `source` field and the `NativeLaunchSource` trait as declared above.
- In `arm`: replace `store.arm_step(session, step_id, context)` with `store.arm_qualified_initialize_with_context(session, step_id, context)` and destructure `(ArmResult::New { step_id }, Some(execution))`; anything else returns `Ok(None)` for `AlreadyRecorded` or `Err(native_error("arm returned no execution context"))` for `New` without a context. Replace `store.candidate_native_launch(...)` with `service.source.frozen(session, &step_id, now)`. Replace `store.candidate_initialize_execution(...)` with the `execution` already returned; its fields are `execution.token.{deployment_id, operation_id, step_id, revision, generation}`, `execution.binding_id`, `execution.incarnation`, `execution.issued_at_ms`, `execution.deadline_ms`, the same names as before.
- In `validate_current`: re-read through `self.source.frozen(self.session, &self.step_id, now)` and additionally call `self.store.qualified_initialize_execution(self.session, &self.step_id)` and compare `deadline_ms`, `binding_id`, `incarnation` and the token to the values captured at arm; mismatch is `native_error("handoff is stale")`.
- Store `source: &'a dyn NativeLaunchSource` on the struct.
- Replace every `"candidate ..."` error string with the same words minus "candidate" ("arm rejected", "clock unavailable", "descriptor unavailable", "checkpoint root unavailable", "preflight failed", "credential resolution failed", "descriptor encoding failed", "descriptor creation failed", "launch uncertain", "handoff is stale", "handoff changed", "API association uncertain").
- Keep `"kind": "sglang_candidate_private_launch"` in the private descriptor JSON (deviation 5) with a comment: `// Wire contract with runtime/sglang_entry.py; renamed with the ordinary native launch design.`
- The `LaunchAssociation` impl is unchanged apart from the rename.

Add above the struct:

```rust
/// Renders and spawns a protected native launch for an armed ordinary initialize.
///
/// Native entrypoint denials stay closed (AGENTS.md): this type opens nothing.
/// `ProfileBindings` still refuses SGLang, and nothing implements
/// `NativeLaunchSource` in production until the ordinary native launch is designed.
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --offline -p mllm-controller --test runtime_binding`
Expected: pass, including the non-native tests in that file.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-controller/src/runtime.rs crates/mllm-controller/tests/runtime_binding.rs crates/mllm-domain/src/launch.rs
git commit -m "refactor(controller): the native launch handoff is sourced through a trait

The only code that renders a protected SGLang launch read its descriptor and
execution context from candidate rows. It now arms through the ordinary initialize,
takes its execution context from that arm, and asks a NativeLaunchSource the
application implements for the frozen descriptor, re-reading it before every send.
Nothing implements the source yet and no denial opens."
```

---

### Task 6: Split the shared test fixture

`crates/mllm-controller/tests/qualification_support/fixture.rs` is included by path from the ordinary coordinator unit tests and two management test files. Its `qualified()` fixture runs a candidate suite and asserts a refusal that Task 1 of ADR 0011 removed; that one assertion fails 49 tests. Give the ordinary consumers their own fixture that never touches a candidate.

**Files:**
- Create: `crates/mllm-controller/tests/support/fixture.rs`
- Modify: `crates/mllm-controller/src/coordinator/tests.rs:14-15`, `crates/mllm-management/tests/actions.rs:25-26`, `crates/mllm-management/tests/events.rs:11-12`
- Reference: `crates/mllm-controller/tests/qualification_support/fixture.rs` (`owned_source`, `managed`, `managed_edit`, `Fixture::admission`, `Fixture::scalar`), `crates/mllm-controller/tests/qualification_support/ordinary_initialize.rs`

**Interfaces:**
- Produces: `pub(crate) async fn owned_source() -> &'static OwnedFixtureSource` with the same struct (`dir`, `fence`, `other`, `observations`) the three consumers already use, so their bodies do not change.

- [ ] **Step 1: Read what the consumers use**

Run: `grep -n "fixture::\|qualification_fixture::" crates/mllm-controller/src/coordinator/tests.rs crates/mllm-management/tests/actions.rs crates/mllm-management/tests/events.rs`
Expected: only `owned_source()` and its fields. If anything else is used, it moves too.

- [ ] **Step 2: Write the ordinary fixture**

Create `crates/mllm-controller/tests/support/fixture.rs`. Its `owned_source()` must produce the same SQLite image the old one did minus the candidate run: a store with the host resource policy imported and two managed deployments (`fence`, `other`) created through `create_managed_configuration`, with the `observations` the tests replay. Copy `managed` and `managed_edit` from the old fixture; wherever they used `catalog.id()` for the profile's `qualification_id`, pass the literal `"declared:test"` (Task 9 removes the field; this fixture is updated then). Copy the host and deployment JSON builders those two functions call. Do not copy `qualified`, `completed_suite`, `secured`, `baseline`, `ready`, `initialized`, `marker_body`, `fixture_peaks`, `fixture_custom`, or any `candidate_creation` import.

```rust
#![allow(dead_code)]
//! Ordinary-lifecycle test fixture: a store with a host policy and two managed
//! deployments, vacuumed once into an immutable image that each test copies.
//! Nothing here creates a candidate run; ADR 0011 removed the concept.

pub(crate) struct OwnedFixtureSource {
    pub(crate) dir: tempfile::TempDir,
    pub(crate) fence: mllm_store::lifecycle::DeploymentFence,
    pub(crate) other: mllm_store::lifecycle::DeploymentFence,
    pub(crate) observations: Vec<mllm_domain::resources::MemoryObservation>,
}

pub(crate) async fn owned_source() -> &'static OwnedFixtureSource {
    static SOURCE: tokio::sync::OnceCell<OwnedFixtureSource> = tokio::sync::OnceCell::const_new();
    SOURCE
        .get_or_init(|| async {
            let f = fixture();
            let fence = managed(&f, "first");
            let other = managed(&f, "second");
            let dir = tempfile::tempdir().unwrap();
            f.sql
                .execute("VACUUM INTO ?1", [dir.path().join("srv.sqlite3").to_str().unwrap()])
                .unwrap();
            OwnedFixtureSource { dir, fence, other, observations: f.observations.clone() }
        })
        .await
}
```

`fixture()` opens a `Store` in a temp dir, begins a coordinator session, imports the resource policy exactly as the old `fixture_custom` did (copy that code minus the candidate manifest), and returns a struct with `store`, `session`, `observations`, `limits`, `ttl`, `sql` and `_dir`. `managed(&f, name)` creates one managed deployment and returns its `DeploymentFence`, as the old `managed_edit` did.

- [ ] **Step 3: Repoint the three includers**

```rust
#[path = "../../tests/support/fixture.rs"]
mod fixture;
```
in `coordinator/tests.rs`;
```rust
#[path = "../../mllm-controller/tests/support/fixture.rs"]
mod fixture;
```
in `mllm-management/tests/actions.rs`, and the same path with `mod qualification_fixture;` renamed to `mod fixture;` in `events.rs` (update its two call sites from `qualification_fixture::owned_source()` to `fixture::owned_source()`).

- [ ] **Step 4: Run the consumers**

Run: `cargo test --offline -p mllm-controller --lib -- coordinator::tests --test-threads=4`
Expected: pass, and in well under 40 minutes; the 49 fixture-driven failures are gone because `qualified()` is no longer run by these tests.
Run: `cargo test --offline -p mllm-management --test actions --test events`
Expected: pass.

- [ ] **Step 5: Commit**

```bash
git add crates/mllm-controller/tests/support crates/mllm-controller/src/coordinator/tests.rs crates/mllm-management/tests/actions.rs crates/mllm-management/tests/events.rs
git commit -m "test: an ordinary fixture that never runs a candidate suite

The coordinator unit tests and two management test files included the candidate
fixture by path, so one sanity assertion about a candidate run failed 49 ordinary
tests. They now share a fixture that imports the host policy and creates two
managed deployments, and nothing else."
```

---

### Task 7: Delete the consumers

Controller, management and CLI code that drives candidate runs. After this the store's candidate modules have no caller outside themselves.

**Files:**
- Delete: `crates/mllm-controller/src/qualification.rs`, `crates/mllm-controller/src/coordinator/candidate.rs`, `candidate_abort.rs`, `candidate_cleanup.rs`, `candidate_security.rs`, `candidate_warm.rs`, `tests_candidate.rs`, `tests_candidate_abort.rs`, `tests_candidate_cleanup.rs`, `tests_candidate_inference.rs`, `tests_candidate_security.rs`, `tests_candidate_warm.rs`, `crates/mllm-controller/tests/candidate_completion.rs`, `crates/mllm-controller/tests/qualification_progression.rs`, `crates/mllm-controller/tests/qualification_support/` (whole directory), `crates/mllm-management/src/candidates.rs`, `crates/mllm-management/tests/candidate_actions.rs`, `crates/mllm-management/tests/candidates.rs`
- Modify: `crates/mllm-controller/src/lib.rs:17`, `crates/mllm-controller/src/coordinator/worker.rs`, `crates/mllm-controller/src/coordinator/tests.rs:12-13`, `crates/mllm-controller/src/coordinator_port.rs` and `coordinator_port/`, `crates/mllm-management/src/lib.rs`, `actions.rs`, `configuration.rs:236-262`, `crates/mllm-management/tests/configuration.rs:248-265`, `crates/mllm-cli/src/grammar.rs`, `crates/mllm-cli/tests/grammar.rs:48-67`

- [ ] **Step 1: Remove the files**

```bash
git rm -r crates/mllm-controller/src/qualification.rs \
  crates/mllm-controller/src/coordinator/candidate.rs \
  crates/mllm-controller/src/coordinator/candidate_abort.rs \
  crates/mllm-controller/src/coordinator/candidate_cleanup.rs \
  crates/mllm-controller/src/coordinator/candidate_security.rs \
  crates/mllm-controller/src/coordinator/candidate_warm.rs \
  crates/mllm-controller/src/coordinator/tests_candidate.rs \
  crates/mllm-controller/src/coordinator/tests_candidate_abort.rs \
  crates/mllm-controller/src/coordinator/tests_candidate_cleanup.rs \
  crates/mllm-controller/src/coordinator/tests_candidate_inference.rs \
  crates/mllm-controller/src/coordinator/tests_candidate_security.rs \
  crates/mllm-controller/src/coordinator/tests_candidate_warm.rs \
  crates/mllm-controller/tests/candidate_completion.rs \
  crates/mllm-controller/tests/qualification_progression.rs \
  crates/mllm-controller/tests/qualification_support \
  crates/mllm-management/src/candidates.rs \
  crates/mllm-management/tests/candidate_actions.rs \
  crates/mllm-management/tests/candidates.rs
```

Before removing `qualification_support/`, check whether `ordinary_initialize.rs`, `ordinary_cleanup.rs`, `unarmed_stop.rs`, `start_receipts.rs`, `expired_unarmed.rs`, `owned_worker.rs` and `worker_store.rs` in it hold ordinary-path tests that no other file covers: `grep -n "#\[tokio::test\]\|#\[test\]" crates/mllm-controller/tests/qualification_support/*.rs | wc -l` and read the test names. Ordinary tests among them that depend only on the old `fixture()` and `qualified()` must be ported to `tests/support/fixture.rs` and a new `crates/mllm-controller/tests/ordinary_lifecycle.rs` that includes `support/fixture.rs` by path, before the directory goes. Port every test whose name does not contain `candidate`, `qualif`, `security`, `warm`, `marker` or `finish`; delete the rest. The known `DatabaseBusy` flake `ordinary_cleanup_races_ready_completion_and_duplicate_accept_and_arm` is deleted, not ported, and its runbook entry goes in Task 13. List the ported and deleted test names in the commit message.

- [ ] **Step 2: Worker**

In `crates/mllm-controller/src/coordinator/worker.rs`:

- Remove from `Shared`: `candidate_poll`, `active_candidate`, `retained_candidates`, `candidate_requests`, and the ordering comment about them.
- Remove `CandidateLifecycleAction`, the commands `candidate_inference`, `initialize_candidate`, `candidate_action`, and `spawn_with_candidate_factory`; `spawn` takes the `DriverFactory` only. Remove the `candidate_factory` parameter of `run`.
- In `run`, delete the five candidate lanes: the `candidate::cleanup::next` block at the top of the loop, the `candidate_requests` pop and `drive_inference` block, the `next_candidate_security`/`drive_security` block, the `next_candidate_warm`/`drive_warm` block, and the `next_candidate_initialize`/`drive` block. The loop then goes: stop check, accepting check, unarmed stop, ordinary cleanup, paused wait, ordinary initialize-or-expire. Preserve the ordinary lanes and the `Uncertain` pause exactly.
- Remove `mod candidate;` and friends from `crates/mllm-controller/src/coordinator.rs` if declared there, and `pub mod qualification;` from `lib.rs:17`.
- In `coordinator/tests.rs` delete the `#[path = "tests_candidate.rs"] mod candidate_tests;` lines.
- In `coordinator_port.rs` and its directory remove the candidate ports (`grep -n -i "candidate" crates/mllm-controller/src/coordinator_port.rs crates/mllm-controller/src/coordinator_port/*.rs`).

- [ ] **Step 3: Management**

In `crates/mllm-management/src/lib.rs` remove `mod candidates;`, the `candidates` field of `AppState`, `candidate_acceptance_router`, the two `/management/v1/qualification-runs/{id}/...` routes and the `/management/v1/qualification-runs` route, and `candidates: Some(...)` in `lifecycle_router`. In `actions.rs` remove the six candidate methods from `ActionSource` and `OwnedActionSource` and the candidate imports at lines 3 and 21. In `configuration.rs` remove the `CandidateSource` impl (lines 236 to 262). In `tests/configuration.rs` remove the `CandidateSource` impl and `rejecting_app`'s use of `candidate_acceptance_router` (use `lifecycle_router` or the plain `router` the file already uses elsewhere; read it).

- [ ] **Step 4: CLI**

In `crates/mllm-cli/src/grammar.rs` remove `Command::Qualify`, `CliCommand::Qualify`, the `format!("qualify deployment ...")` arm and the conversion arm. In `crates/mllm-cli/tests/grammar.rs` rename `invite_join_inspect_doctor_qualify` to `invite_join_inspect_doctor` and delete the two `qualify` assertions. Check `crates/mllm-cli/src/main.rs` for a `Qualify` match arm and remove it.

- [ ] **Step 5: Compile and test the touched crates**

Run: `cargo check --offline --workspace --all-targets`
Expected: clean. Fix what the compiler names; every error is a dangling candidate reference.
Run: `cargo test --offline -p mllm-controller -p mllm-management -p mllm-cli --all-targets -- --test-threads=4`
Expected: pass. Note the controller time; it should be near nine minutes.

- [ ] **Step 6: Commit**

```bash
git add -A crates/mllm-controller crates/mllm-management crates/mllm-cli
git commit -m "refactor: delete the candidate lanes, routes and CLI verb

The coordinator worker drops its five candidate lanes and commands and runs the
ordinary lanes alone; the management API loses its qualification-run routes; the
CLI loses mllm qualify. The candidate tests go with them. Ordinary tests that lived
in the qualification support directory are ported to tests/support.

Ported: <names>. Deleted: <names>."
```

---

### Task 8: Delete the producers and the negative identity

The store, config and domain modules of the subsystem, the candidate halves of shared files, and the five SQL guards that defined an ordinary deployment as one with no qualification run. The guard removal and the table drop happen together, because a guard that names a dropped table fails to prepare.

**Files:**
- Delete: `crates/mllm-store/src/candidate_creation.rs`, `crates/mllm-store/src/candidate_creation/` (whole directory), `crates/mllm-store/src/qualification.rs`, `crates/mllm-store/src/qualification/`, `crates/mllm-store/src/qualification_policy.rs`, `crates/mllm-store/src/qualification_policy/`, `crates/mllm-config/src/effective/candidate.rs`, `crates/mllm-config/tests/candidate.rs`, `crates/mllm-config/tests/candidate_snapshot.rs`, `crates/mllm-domain/src/qualification.rs`
- Modify: `crates/mllm-store/src/lib.rs:6,13,14`, `crates/mllm-store/src/lifecycle.rs`, `crates/mllm-store/src/lifecycle/completion.rs`, `crates/mllm-store/src/lifecycle/completion/tests.rs:91`, `crates/mllm-store/src/ordinary_lifecycle.rs:156,178`, `crates/mllm-store/src/ordinary_lifecycle/receipt.rs:217`, `crates/mllm-store/src/ordinary_lifecycle/cleanup.rs:181`, `crates/mllm-store/src/managed_configuration.rs:457`, `crates/mllm-store/src/resource_ledger.rs:194-220,282-306`, `crates/mllm-store/src/dispatch.rs`, `crates/mllm-store/src/events.rs`, `crates/mllm-store/src/schema.rs`, `crates/mllm-store/src/migrations.rs`, `crates/mllm-config/src/effective.rs:3`, `crates/mllm-adapters/tests/sglang_args.rs:3,443`, `crates/mllm-domain/src/lib.rs`, `crates/mllm-domain/src/completion.rs`, `crates/mllm-adapters/src/traits.rs:170`, `crates/mllm-adapters/src/fake/qualification.rs:4-8`, `crates/mllm-adapters/src/fake/engine.rs`, `crates/mllm-adapters/tests/sglang_control.rs:285`, `crates/mllm-controller/src/coordinator/tests.rs:93`

**Interfaces:**
- Produces: `mllm_domain::completion::{EffectObservation, ParkedStatusObservation}` (moved from `qualification.rs`); `SCHEMA_V13`; `Store::reserve_retained_in_transaction` is deleted (its only callers were candidate).

- [ ] **Step 1: Write the migration test first**

In `crates/mllm-store/src/migrations.rs` tests, add:

```rust
/// ADR 0011: the qualification tables are dropped. A v12 store with rows in the
/// surviving tables keeps them; none of the dropped tables remain.
#[test]
fn v13_drops_qualification_tables_and_preserves_rows() {
    let conn = Connection::open_in_memory().unwrap();
    for (index, sql) in MIGRATIONS.iter().take(12).enumerate() {
        conn.execute_batch(sql).unwrap();
        conn.execute("INSERT INTO schema_migrations(version) VALUES(?1)", [(index + 1) as i64]).unwrap();
    }
    conn.execute_batch("INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('kept','kept','model','ready',1,0,1,1);").unwrap();
    apply(&conn).unwrap();
    apply(&conn).unwrap();
    let name: String = conn.query_row("SELECT name FROM deployments WHERE id='kept'", [], |r| r.get(0)).unwrap();
    assert_eq!(name, "kept");
    for table in [
        "qualification_evidence_refs", "qualification_ready_probes",
        "qualification_request_attempts", "qualification_request_results",
        "qualification_case_actions", "qualification_parked_status",
        "candidate_cleanup_actions", "qualifications", "qualification_runs",
        "host_qualification_policies",
    ] {
        let present: bool = conn
            .query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [table], |r| r.get(0))
            .unwrap();
        assert!(!present, "{table} must be dropped");
    }
    for table in ["owned_launch_associations", "request_leases", "deployment_attempts"] {
        let present: bool = conn
            .query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [table], |r| r.get(0))
            .unwrap();
        assert!(present, "{table} must stay");
    }
}
```

Run: `cargo test --offline -p mllm-store migrations::tests::v13`
Expected: fails, `SCHEMA_V13` does not exist and the tables are present.

- [ ] **Step 2: Schema v13**

In `crates/mllm-store/src/schema.rs`, after `SCHEMA_V12`:

```rust
/// ADR 0011: qualification is not an mllm concept. The tables that held candidate
/// runs, their catalog, evidence, probes, budgets and parked-status records are
/// dropped in foreign-key order. Nothing wrote them outside tests; a v12 store from
/// this branch has no rows in them. State directories older than 2026-09-16 must
/// already be deleted for the resource-policy shape, so no data path is preserved.
pub const SCHEMA_V13: &str = r#"
DROP TABLE qualification_evidence_refs;
DROP TABLE qualification_ready_probes;
DROP TABLE qualification_request_results;
DROP TABLE qualification_request_attempts;
DROP TABLE qualification_case_actions;
DROP TABLE qualification_parked_status;
DROP TABLE candidate_cleanup_actions;
DROP TABLE qualifications;
DROP TABLE qualification_runs;
DROP TABLE host_qualification_policies;
"#;
```

`qualification_request_results` references `qualification_request_attempts`, so it drops first. Add `SCHEMA_V13` to both lists in `migrations.rs`. Do not edit any earlier constant.

- [ ] **Step 3: Remove the negative identity guards**

Each of these SQL strings contains `AND NOT EXISTS(SELECT 1 FROM qualification_runs WHERE deployment_id=?N)` or `UNION ALL SELECT 1 FROM qualification_runs WHERE deployment_id=?1`. Delete that clause and nothing else from:

- `crates/mllm-store/src/ordinary_lifecycle.rs` `check_managed_command_target` (line 156) and `effective` (line 178)
- `crates/mllm-store/src/ordinary_lifecycle/receipt.rs:217`
- `crates/mllm-store/src/ordinary_lifecycle/cleanup.rs:181`
- `crates/mllm-store/src/managed_configuration.rs:457`

Add above `check_managed_command_target`: `// ADR 0011: every managed deployment is ordinary; there is no other kind.`

In `crates/mllm-store/src/resource_ledger.rs` delete `reserve_retained_candidate_in_transaction` and the `GrantTransition::RetainedCandidate` variant with its two match arms; `reserve_in_transaction` keeps `GrantTransition::Increase` as its only variant (or collapse the enum, your choice, keep the function signature).

Run: `grep -rn "qualification_runs\|host_qualification_policies\|candidate_action_v3\|candidate_cleanup" crates/mllm-store/src --include=*.rs | grep -v schema.rs`
Expected: the `lifecycle.rs` hits handled in Step 5 only.

- [ ] **Step 4: Delete the store, config and domain modules**

```bash
git rm -r crates/mllm-store/src/candidate_creation.rs crates/mllm-store/src/candidate_creation \
  crates/mllm-store/src/qualification.rs crates/mllm-store/src/qualification \
  crates/mllm-store/src/qualification_policy.rs crates/mllm-store/src/qualification_policy \
  crates/mllm-config/src/effective/candidate.rs \
  crates/mllm-config/tests/candidate.rs crates/mllm-config/tests/candidate_snapshot.rs
```

Remove `pub mod candidate_creation;`, `pub mod qualification_policy;`, `pub mod qualification;` from `crates/mllm-store/src/lib.rs` and `pub mod candidate;` from `crates/mllm-config/src/effective.rs`. In `crates/mllm-adapters/tests/sglang_args.rs` delete the import at line 3 and the test around line 443 that calls `validate_candidate_reviewed_snapshot` (it validated a candidate manifest, not the adapter).

- [ ] **Step 5: `lifecycle.rs`**

Delete `insert_candidate_initialize_run`, `insert_candidate_cleanup_run`, `candidate_handoff_states`, `validate_candidate_initialize_run`, `PreparedBinding::prepare_candidate`, `DecodedBinding::Candidate` and the fallthrough in `decode_binding` (a binding that is not a valid `BindingDto` with `version == 1` is now `Err(LifecycleError::Invalid)`), the `"candidate_cleanup"` literal in `insert_owned_cleanup_run`'s allowed kinds, and the `candidate_action_v3` predecessor check around line 269 (read the surrounding function; if the `v3` flag only widened what a predecessor may be, remove the widening and keep the strict path).

- [ ] **Step 6: `lifecycle/completion.rs`**

Remove the import block at lines 3 to 6. Delete `accounting`. In `record_owned_launch` and `complete_step`, the `is_ordinary` branch is now the only branch: keep `if is_ordinary { ordinary::...; commit; return Ok(()) }` followed by `Err(LifecycleError::Unsupported)` rather than the candidate half, or, if `is_ordinary` becomes trivially true for every armed step, call the ordinary path unconditionally and delete `is_ordinary`. Check by reading `is_ordinary` (`ordinary_lifecycle.rs:151`): it tests `o.kind='qualified_initialize'`. Cleanup and unarmed-stop steps have other kinds and are completed by their own functions, so the branch stays as a guard with `Unsupported` on the else. Delete the candidate half of `complete_step` (from `let v = validated_initialize(...)` to the end of the function body) and the `CandidateLifecycleTransition::OwnedLaunchAssociated` event call. Delete the `#[path = "../../candidate_creation/cleanup/tests.rs"] mod cleanup_tests;` include in `completion/tests.rs` and any test in that file that constructs a candidate run.

Update the file's first line to `//! Physical completion retains conservative accounting and closed admission.`

- [ ] **Step 7: `dispatch.rs`, `events.rs`, domain**

In `dispatch.rs` delete `settle_verified_candidate_cleanup` and `DispatchTicket::candidate`. Then check `grant_dispatch`, `close_dispatch`, `finish_dispatch`, `pending_dispatches`, `mark_dispatch_uncertain`: `grep -rn "grant_dispatch\|close_dispatch\|finish_dispatch\|pending_dispatches\|mark_dispatch_uncertain" crates --include=*.rs | grep -v "mllm-store/src/dispatch.rs"`. Expected after Task 7: no production callers. Delete them and `PendingDispatch`, `DispatchRequest`, `DispatchTicket` if nothing else uses them; keep `CoordinatorSession`, `begin_coordinator_session`, `check_session`. Update the module doc.

In `events.rs` delete the variants `CandidateAbortAccepted`, `CandidateLifecycleRecorded`, `CandidateInitializeArmed`, `CandidateInitializeAccepted`, `CandidateRunAccepted`, the enum `CandidateLifecycleTransition` and its impl, their `kind()` and target arms, and the test at line 500 that builds `CandidateRunAccepted`.

In `mllm-domain`: move `EffectObservation` and `CandidateParkedStatusObservation` (renamed `ParkedStatusObservation`) from `qualification.rs` into `completion.rs`, delete `qualification.rs` and `pub mod qualification;`. Update `crates/mllm-adapters/src/traits.rs:170` to `mllm_domain::completion::EffectObservation`, and the other users listed in Files. The Fake engine's `qualification_parked_status` returns `ParkedStatusObservation` (Task 10 renames the method).

- [ ] **Step 8: Compile, test, and grep**

Run: `cargo check --offline --workspace --all-targets`
Expected: clean.
Run: `cargo test --offline -p mllm-domain -p mllm-config -p mllm-store -p mllm-adapters -p mllm-controller -p mllm-management --all-targets -- --test-threads=4`
Expected: pass, including `v13_drops_qualification_tables_and_preserves_rows`.
Run: `grep -rn -i "candidate" crates --include=*.rs | grep -v "mllm-scheduler\|served_name\|sglang_candidate_private_launch\|runtime.rs" | cut -c1-120`
Expected: nothing. `mllm-scheduler` uses "candidate" for a deployment under admission and is out of scope. The two wire strings are deviation 5.

- [ ] **Step 9: Commit**

```bash
git add -A crates
git commit -m "feat(store): delete the qualification subsystem and drop its schema

The candidate and qualification modules of the store, their config normalizer and
their domain observation types are gone. Schema v13 drops the ten tables they owned
in foreign-key order. The five SQL guards that defined an ordinary deployment as one
with no qualification run are removed: every managed deployment is ordinary now, so
the predicate had nothing left to distinguish. completion.rs and lifecycle.rs keep
their ordinary halves. EffectObservation and ParkedStatusObservation move to
mllm_domain::completion; the adapter trait imports them from there."
```

---

### Task 9: Retire the qualification identity string

`qualification_id` is a host YAML key, an effective-config field, a binding DTO field on disk, a token field the worker compares, and a `RuntimeBinding` field. The binding fields hold the declared identity `declared:{fingerprint}` and are renamed; the profile string is deleted.

**Files:**
- Modify: `crates/mllm-config/src/schema.rs:199`, `crates/mllm-config/src/effective.rs` (`RuntimeProfile.qualification_id` line 224, raw profile line 550, `MissingAwareQualification`, checks at 855 to 865 and 880 to 888), `crates/mllm-cli/src/standalone_config.rs:45`, `crates/mllm-domain/src/completion.rs:49,160`, `crates/mllm-store/src/lifecycle.rs:427,441,453,500,513,1119,1652,1705`, `crates/mllm-store/src/ordinary_lifecycle.rs:111,299,587`, `crates/mllm-store/src/ordinary_lifecycle/receipt.rs:202`, `crates/mllm-store/src/ordinary_lifecycle/cleanup.rs:190`, `crates/mllm-store/src/lifecycle/completion.rs:251,261,314`, `crates/mllm-controller/src/runtime.rs:26`, `crates/mllm-controller/src/coordinator/worker.rs:1706`, `crates/mllm-adapters/src/sglang/adapter.rs:263`, `crates/mllm-adapters/src/fake/qualification.rs:588,627,643`, `crates/mllm-controller/tests/support/fixture.rs`, `crates/mllm-config/tests/*.rs` and `crates/mllm-config/tests/fixtures/` where `qualification_id` appears

- [ ] **Step 1: Write the failing config test**

In `crates/mllm-config/tests/effective.rs` add:

```rust
/// ADR 0011: a runtime profile carries no qualification reference. A host file
/// that still has the key is refused by the strict schema with the key named.
#[test]
fn a_runtime_profile_has_no_qualification_id() {
    let (mut deployment, mut host) = common::minimal_inputs();
    host["runtime_profiles"]["p"]["qualification_id"] = serde_json::json!("x");
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert!(error.to_string().contains("qualification_id"), "{error}");
    host["runtime_profiles"]["p"].as_object_mut().unwrap().remove("qualification_id");
    resolve_effective(&deployment, &host).unwrap();
    let _ = &mut deployment;
}
```

Adapt `common::minimal_inputs()` to whatever builder `crates/mllm-config/tests/common/` exposes (read it); the shape of the assertion is what matters.

Run: `cargo test --offline -p mllm-config --test effective a_runtime_profile_has_no_qualification_id`
Expected: fails, the key is currently required.

- [ ] **Step 2: Config**

Remove `("qualification_id", SCALAR)` from `PROFILE` in `schema.rs`. Remove `qualification_id` from `RuntimeProfile` and the raw profile struct, delete `MissingAwareQualification` and both checks in `effective.rs` (lines 855 to 865 and 880 to 888), and the `qualification_policy` size check at lines 838 to 846 if it only served the candidate manifest (read it; `host.qualification_policy` is no longer a key anywhere, so the check is dead). Remove the key from `standalone_config.rs:45` and from every fixture under `crates/mllm-config/tests/fixtures/` and `crates/mllm-controller/tests/support/fixture.rs`. Rename `EffectiveDeployment.qualification_fingerprint` to `recipe_fingerprint` and `core::qualification_fingerprint` to `recipe_fingerprint` (its serialized struct `Qualification` becomes `Recipe`; the digest input is unchanged so fingerprints are stable, and there is a test in `effective_snapshot.rs` that pins a fingerprint; it must still pass).

- [ ] **Step 3: Domain and store**

In `mllm-domain/src/completion.rs` delete `TransitionToken.qualification_id` and its emptiness check. In the store rename `ReserveBinding.qualification_id`, `StoredRuntimeBinding.qualification_id` and `BindingDto.qualification_id` to `identity_id`, since that is what they hold (`ordinary_lifecycle.rs:587` writes `catalog.id()`, which is the declared identity). Update the comparisons at `ordinary_lifecycle.rs:299`, `receipt.rs:202`, `cleanup.rs:190` to `identity_id`. Delete `TokenDto.qualification_id` in `lifecycle/completion.rs` and the token construction at `ordinary_lifecycle.rs:111`. `TokenDto` is `deny_unknown_fields` and is written into `lifecycle_steps.step_json`; no compatibility is kept (spec §3). The local variable named `catalog` in `ordinary_lifecycle.rs` becomes `identity`.

- [ ] **Step 4: Controller and adapters**

Rename `RuntimeBinding.qualification_id` to `identity_id` in `crates/mllm-controller/src/runtime.rs:26` and its constructors. Delete the `context.token.qualification_id != work.effective().profile.qualification_id` line in `worker.rs:1706`; the identity is already checked through `binding_id` and the frozen binding payload. Remove `&c.token.qualification_id` from the emptiness list in `sglang/adapter.rs:263`. In `fake/qualification.rs` delete the three test lines that set `token.qualification_id` and the loop at 627 that iterated over id spellings (its assertion, that an ordinary initialize proves the model usable, is already covered by the first assertion in that test).

- [ ] **Step 5: Compile, test, grep**

Run: `cargo check --offline --workspace --all-targets`
Run: `cargo test --offline -p mllm-config -p mllm-domain -p mllm-store -p mllm-adapters -p mllm-cli --all-targets -- --test-threads=4`
Run: `cargo test --offline -p mllm-controller --lib -- --test-threads=4`
Expected: all pass.
Run: `grep -rn "qualification_id\|qualification_fingerprint\|MissingAwareQualification\|qualification_policy" crates --include=*.rs --include=*.yaml --include=*.json`
Expected: nothing.

- [ ] **Step 6: Commit**

```bash
git add -A crates
git commit -m "refactor: retire the qualification identity string

A runtime profile no longer carries a qualification_id; the strict host schema
refuses the key. The binding fields that held the declared identity are named
identity_id, the transition token drops the field, and the effective fingerprint is
called what it is, a recipe fingerprint. Digest inputs are unchanged so recorded
fingerprints still match."
```

---

### Task 10: The Fake engine keeps its lifecycle, loses its ceremony

`crates/mllm-adapters/src/fake/qualification.rs` simulates persisted effects, cleanup and parked status for the Fake engine. Production adapter resolution (`resolve.rs:102`) and the ordinary worker (`worker.rs:763,779`) use it. Its marker corpus, security cases and stream validation were candidate cases.

**Files:**
- Rename: `crates/mllm-adapters/src/fake/qualification.rs` to `crates/mllm-adapters/src/fake/lifecycle.rs`
- Modify: `crates/mllm-adapters/src/fake/mod.rs`, `crates/mllm-adapters/src/fake/engine.rs:54-215`, `crates/mllm-adapters/src/resolve.rs:102`, `crates/mllm-controller/src/coordinator/worker.rs:763,779`, `crates/mllm-controller/src/coordinator/tests.rs:59,182,1413`, `crates/mllm-controller/src/coordinator/tests_cleanup.rs:50,715`, `crates/mllm-controller/tests/support/fixture.rs`

- [ ] **Step 1: Rename and prune**

```bash
git mv crates/mllm-adapters/src/fake/qualification.rs crates/mllm-adapters/src/fake/lifecycle.rs
```

In `mod.rs`: `mod lifecycle;` and `pub use lifecycle::FakeFault;`. In `lifecycle.rs`: rename `QualificationFault` to `FakeFault` and delete the variants `CrossMarkerOutput`, `UnauthorizedAdminExec`, `UnauthorizedInferenceExec`, `UnauthorizedHealthExec`, `LostSecurityInferenceReply`, `StreamDuplicateField`, `StreamOverflow`, `StreamMissingDone`, `StreamAfterFinish` and every match arm that handles them; keep `WrongProbeOutput`, `LostProbeReply`, `FailedProbe`, `MissingProbeFinish`. Rename `QualificationState` to `LifecycleState`. Delete `security_control`, `security_request`, `forward` and `stream` if their only callers were the deleted engine methods (check with the compiler after Step 2); keep `cleanup`, `cleanup_with_clock`, `activity`, `members`, `parked_status`, `execute`, `execute_with_clock`. Update the module doc to "Deterministic lifecycle state for the Fake engine: persisted effects, cleanup and parked status."

- [ ] **Step 2: Engine methods**

In `engine.rs` rename: field `qualification` to `lifecycle`, `qualification_clock` to `lifecycle_clock`; `for_qualification` to `with_lifecycle`, `for_qualification_with_clock` to `with_lifecycle_clock`, `with_qualification_fault` to `with_fault`, `qualification_members` to `lifecycle_members`, `qualification_cleanup` to `lifecycle_cleanup`, `qualification_cleanup_observed` to `lifecycle_cleanup_observed`, `qualification_cleanup_mode_observed` to `lifecycle_cleanup_mode_observed`, `qualification_parked_status` to `parked_status`, `qualification_activity` to `lifecycle_activity`. Delete `qualification_security_control` and `qualification_security_request`. Update the doc comments to drop "qualification".

Run: `grep -rln "for_qualification\|with_qualification_fault\|qualification_members\|qualification_cleanup\|qualification_parked_status\|qualification_activity\|QualificationFault" crates | xargs sed -i 's/for_qualification_with_clock/with_lifecycle_clock/g; s/for_qualification/with_lifecycle/g; s/with_qualification_fault/with_fault/g; s/qualification_members/lifecycle_members/g; s/qualification_cleanup_mode_observed/lifecycle_cleanup_mode_observed/g; s/qualification_cleanup_observed/lifecycle_cleanup_observed/g; s/qualification_cleanup/lifecycle_cleanup/g; s/qualification_parked_status/parked_status/g; s/qualification_activity/lifecycle_activity/g; s/QualificationFault/FakeFault/g'`

- [ ] **Step 3: Compile and test**

Run: `cargo check --offline --workspace --all-targets`
Run: `cargo test --offline -p mllm-adapters --all-targets -- --test-threads=4`
Run: `cargo test --offline -p mllm-controller --lib -- --test-threads=4`
Expected: pass.
Run: `grep -rn -i "qualif" crates/mllm-adapters/src`
Expected: nothing.

- [ ] **Step 4: Commit**

```bash
git add -A crates
git commit -m "refactor(adapters): the Fake engine's lifecycle simulation under its own name

The Fake's persisted effects, cleanup and parked status were named for the
qualification path that first used them; production adapter resolution and the
ordinary worker use them too. The module is fake::lifecycle, the fault set keeps
the probe faults the ordinary initialize can hit, and the marker, security and
stream cases go with the concept."
```

---

### Task 11: Drop the "qualified" prefix from the ordinary path

Compiler-driven renames plus two grep-verified string renames. Own commit; if the diff bloats past reason, stop after Step 2 and leave the string renames for a later pass, saying so in the commit.

**Files:**
- Modify: `crates/mllm-store/src/ordinary_lifecycle.rs`, `ordinary_lifecycle/receipt.rs`, `ordinary_lifecycle/worker.rs`, `crates/mllm-store/src/events.rs`, every user the compiler names (`crates/mllm-controller/src/coordinator/worker.rs`, `coordinator_port*`, `crates/mllm-management/src/*.rs`, tests)

- [ ] **Step 1: Types and functions**

| Old | New |
|---|---|
| `QualifiedStartReceipt` | `StartReceipt` |
| `QualifiedStart` | `Start` |
| `accept_qualified_start` | `accept_start` |
| `accept_qualified_start_command` | `accept_start_command` |
| `qualified_start_command_receipt` | `start_command_receipt` |
| `QualifiedInitializeWork` | `InitializeWork` |
| `QualifiedInitializeStatus` | `InitializeStatus` |
| `QualifiedInitializePoll` | `InitializePoll` |
| `arm_qualified_initialize_with_context` | `arm_initialize_with_context` |
| `qualified_initialize_status` | `initialize_status` |
| `revalidate_qualified_initialize_send` | `revalidate_initialize_send` |
| `next_qualified_initialize` | `next_initialize` |
| `next_qualified_initialize_or_expire` | `next_initialize_or_expire` |
| `mark_qualified_initialize_uncertain` | `mark_initialize_uncertain` |
| `expire_unarmed_qualified_initialize` | `expire_unarmed_initialize` |
| `qualified_initialize_execution` | `initialize_execution` |
| `QualifiedLifecycleTransition` | `LifecycleTransition` |
| `QualifiedLifecycleRecorded` | `LifecycleRecorded` |

Apply with `sed -i` over `crates` for each pair, longest names first so `accept_qualified_start_command` is renamed before `accept_qualified_start`. Then `cargo check --offline --workspace --all-targets` and fix anything sed missed.

- [ ] **Step 2: Stored strings**

The operation kind `'qualified_initialize'` and the event kinds `qualified_initialize_accepted`, `qualified_initialize_armed`, `qualified_owned_launch_associated`, `qualified_ready_committed`, `qualified_initialize_uncertain`, `qualified_initialize_expired_unarmed` are stored in `operations.kind` and `management_events`. No compatibility is kept (spec §3). Rename the operation kind to `'initialize'` and the event kinds to the same names without the `qualified_` prefix, in every SQL literal and Rust string:

Run: `grep -rn "qualified_" crates --include=*.rs | cut -c1-140`
Rename each hit. Then run the same grep.
Expected: nothing.

Also `crates/mllm-controller/src/coordinator.rs:1` doc comment "Owned, qualified Fake initialization" becomes "Owned Fake initialization"; `crates/mllm-cli/src/roles.rs` comments at lines 44, 70, 71, 378 reword "qualified against" to "admitted against".

- [ ] **Step 3: Test**

Run: `cargo test --offline -p mllm-store -p mllm-controller -p mllm-management --all-targets -- --test-threads=4`
Expected: pass. Management event tests that pinned a `qualified_*` kind string now pin the new one; update the expected literal, not the assertion shape.
Run: `grep -rn -i "qualif" crates --include=*.rs | grep -v "mllm-cli/src/roles.rs" | cut -c1-120`
Expected: nothing. `roles.rs` may keep comments about the F1 host live verification runs if they cite the runbook by its filename; leave those.

- [ ] **Step 4: Commit**

```bash
git add -A crates
git commit -m "refactor: the ordinary path is not called qualified

The types, store methods, operation kind and event kinds of the ordinary lifecycle
carried a qualified prefix with no qualification behind it. They are named for what
they do. Stored kind strings change with them; no compatibility is kept, as the
design records."
```

---

### Task 12: Retry with a budget and a cooldown

ADR 0011 decision 5, on the loop Task 1 made survivable.

**Files:**
- Modify: `crates/mllm-controller/src/coordinator/worker.rs` (`CoordinatorOptions` at line 68, its `Default`, the option validation in `spawn`, the failure branch from Task 1)
- Test: `crates/mllm-controller/src/coordinator/tests.rs` beside `a_failed_deployment_does_not_stop_the_others`
- Reference: `crates/mllm-store/src/attempts.rs` (`Store::record_attempt(&DeploymentFence, now_ms) -> Result<AttemptRecord, StoreError>`, `Store::attempts(&DeploymentFence) -> Result<Option<AttemptRecord>, StoreError>`, `AttemptRecord { attempts: i64, last_attempt_ms: i64 }`), `crates/mllm-launchers/src/process_absence.rs` and `worker.rs:650` `observed_gone`

**Interfaces:**
- Consumes: `Store::record_attempt`, `Store::attempts`, `Store::set_admission_enabled` (Task 1), `FakeEngine::with_fault(FakeFault::FailedProbe)` (Task 10).
- Produces: `CoordinatorOptions { max_attempts: u32, retry_cooldown: Duration, .. }`.

- [ ] **Step 1: Write the failing tests**

```rust
/// ADR 0011 decision 5: a failed attempt is retried, and the deployment is given
/// up on only after the budget is spent. T20
#[tokio::test]
async fn a_failed_start_is_retried_until_the_budget_is_spent() {
    // Fixture: the ordinary fixture with a FakeEngine::with_lifecycle().with_fault(FakeFault::FailedProbe)
    // driver and CoordinatorOptions { max_attempts: 3, retry_cooldown: Duration::from_millis(20), ..Default::default() }.
    // Accept a start, run the worker until the deployment's admission_enabled reads 0.
    // Assert: store.attempts(&fence) == Some(AttemptRecord { attempts: 3, .. });
    //         the worker status is Running (not Failed, not Stopped);
    //         no fourth initialize is armed (count lifecycle_steps for the deployment == 3).
}

/// Retrying an effect that may have landed can start a second engine while the
/// first still holds memory. An uncertain attempt is not counted and not retried
/// until the recorded processes are proven gone. T20
#[tokio::test]
async fn an_uncertain_attempt_is_not_retried_while_processes_remain() {
    // Fixture: a driver whose execute_persisted returns RuntimeError::Uncertain after
    // the launch association is recorded (FakeFault::LostProbeReply does this; read
    // fake/lifecycle.rs to confirm which fault leaves identities alive).
    // Assert: worker status is Uncertain { .. }; store.attempts(&fence) is None;
    //         lifecycle_steps for the deployment == 1; admission_enabled still 1.
    // Then issue an explicit Stop through the commands and let cleanup prove gone.
    // Assert: after cleanup commits, a new Start is accepted and armed (steps == 2).
}
```

Write the bodies against the fixture helpers in `crates/mllm-controller/tests/support/fixture.rs` and the worker-driving helpers already in `tests.rs` (read how `a_failed_deployment_does_not_stop_the_others` drives the worker and copies its shape).

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --offline -p mllm-controller --lib -- coordinator::tests::a_failed_start_is_retried coordinator::tests::an_uncertain_attempt`
Expected: the first fails (one attempt only, admission closed at once); the second's current behaviour is recorded in your notes before any change.

- [ ] **Step 3: Options**

```rust
    /// ADR 0011 decision 5: how many times one configuration is attempted before the
    /// deployment is given up on. Policy, not a constant; host-published policy
    /// supplies it when remote hosts exist (F3).
    pub max_attempts: u32,
    /// The wait before the first retry. Each later retry doubles it, so a broken
    /// recipe does not burn a GPU in a tight loop and a transient failure does not
    /// wait minutes.
    pub retry_cooldown: Duration,
```

Defaults: `max_attempts: 3`, `retry_cooldown: Duration::from_secs(30)`. Where `spawn` validates the other options, reject `max_attempts == 0 || max_attempts > 16` and `retry_cooldown.is_zero() || retry_cooldown > Duration::from_secs(3600)` with the existing option error.

- [ ] **Step 4: The retry**

In the failure branch from Task 1, before `set_admission_enabled`:

```rust
// SPEC §13.2 and ADR 0011 decision 5: a failure known not to have landed is
// retried. The attempt is counted against this exact configuration, and the
// deployment is given up on once the budget is spent. An uncertain outcome never
// reaches here: it pauses above and resolves through the gone-proof first.
let fence = work.fence().clone();
let record = match shared
    .read(move |owner, now| {
        owner
            .store()
            .record_attempt(&fence, now)
            .map_err(|error| LifecycleError::from(error))
    })
    .await
{
    Ok(record) => record,
    Err(error) => return WorkerStatus::Failed(format!("{outcome:?}; attempt not recorded: {error}")),
};
if record.attempts < i64::from(shared.options.max_attempts) {
    let exponent = u32::try_from(record.attempts.saturating_sub(1)).unwrap_or(u32::MAX).min(16);
    let wait = shared
        .options
        .retry_cooldown
        .saturating_mul(1u32.checked_shl(exponent).unwrap_or(u32::MAX));
    status_tx.send_replace(WorkerStatus::Running);
    tokio::select! {
        _ = stop.changed() => {},
        _ = tokio::time::sleep(wait) => {},
    }
    shared.changed.notify_waiters();
    continue;
}
// Budget spent: terminal for this deployment.
```

then the existing `set_admission_enabled(&deployment_id, false)` and `continue`. If `StoreError` does not convert into `LifecycleError`, map it with the `store_error` closure pattern already used in `start`. A retry re-polls `next_initialize_or_expire`; check that a failed step leaves the deployment in a state that poll picks up again (read `ordinary_lifecycle/worker.rs` `next_initialize_or_expire` and `expiry.rs`). If a failed initialize is not re-discoverable, the retry must re-accept a start against the same fence through `accept_start` with an idempotency key derived from the attempt number; do whichever the store already supports, and say which in the commit.

The uncertain path is unchanged and needs no code: it pauses, an explicit Stop drives cleanup, the gone-proof (`observed_gone`) settles it, and only then does the worker resume. The second test proves that no attempt is counted meanwhile.

- [ ] **Step 5: Test**

Run: `cargo test --offline -p mllm-controller --lib -- --test-threads=4`
Expected: pass. Tests use a 20 ms cooldown; nothing sleeps for 30 s.

- [ ] **Step 6: Commit**

```bash
git add crates/mllm-controller
git commit -m "feat(controller): retry a failed deployment, and give up after a budget

A failed configuration is attempted three times with a doubling cooldown from
30 seconds, then given up on with its own admission closed. Both numbers are
coordinator options; host policy supplies them when remote hosts exist.

An uncertain outcome is not a failure and is not retried on these terms. Retrying
an effect that may have landed can start a second engine while the first still
holds memory, so the recorded processes must be proven gone first, and no attempt
is counted until then."
```

---

### Task 13: Verify and record

**Files:**
- Modify: `docs/runbooks/f2-current-status.md`, `AGENTS.md:68`

- [ ] **Step 1: Acceptance-matrix tags**

Baseline at `9687205` was two `// T15` and one `// T19` across `crates`. Run: `grep -rhoE "// T[0-9]+" crates --include=*.rs | sort | uniq -c`
Expected: T15 and T19 still present, T20 present (Task 3 and Task 12 tests). Add `// T21` to the existing deep-park gate tests: find them with `grep -rn "experimental_controls\|ParkPolicy::Denied" crates/mllm-adapters/src crates/mllm-adapters/tests crates/mllm-agent/src | grep -i "fn \|#\[test\]"` and tag at least one test in `crates/mllm-adapters` that asserts a park is refused without opt-in. If none exists, write one against `FakeEngine::with_policy(ParkPolicy::Denied)` asserting `park` returns the denial, tagged `// T21`, citing `SPEC §9.1`.

- [ ] **Step 2: Core suite**

Run: `cargo test --offline -p mllm-adapters -p mllm-store -p mllm-controller -p mllm-management -p harness --all-targets --no-fail-fast --locked -- --test-threads=4`
Expected: pass. Record the wall time and the distinct test total (excluding the owned-state child's nested summary).
Run: `cargo test --offline -p mllm-config -p mllm-cli -p mllm-router -p mllm-domain -p mllm-agent -p mllm-launchers -p mllm-scheduler -p mllm-protocol --all-targets -- --test-threads=4`
Expected: pass. `mllm-cli` runs the excluded `live_interactive.rs` as collateral of the target; that is expected and not a read of the file.
Run: `cargo clippy --offline --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 3: Runbook**

In `docs/runbooks/f2-current-status.md`:

- Under "Recent committed work", add one entry per commit from Task 1 to Task 12 with its short hash, one line each.
- Under "Remaining implementation and verification", delete the lines about the closed native qualification program (180 to 192) and replace with: "Ordinary park (drain, park, parked accounting, wake) is not designed; the contract it must satisfy is `mllm-domain/src/park.rs`. The ordinary native launch is not designed; `NativeLaunchHandoff` waits on a `NativeLaunchSource` implementation, `ProfileBindings` refuses SGLang, and the private descriptor tag `sglang_candidate_private_launch` and the served-name rule `candidate-{binding_id}` are leftovers of that design. Native parking is blocked on both engines regardless: `VllmAdapter` has no `execute_persisted`."
- Under "A1b", add a checked entry: "Remove qualification (ADR 0011). mllm guards the host; the user owns the recipe. The candidate and qualification subsystem is deleted, schema v13 drops its tables, the park contract survives as pure domain rules, a failed deployment closes its own admission and is retried three times with a doubling cooldown before it is given up on, and an uncertain attempt still resolves through the gone-proof first. CPU and Fake tests are not verification of any native recipe."
- Under "A4 — Collapse the second lifecycle", replace the body with "Discharged by deletion (ADR 0011)."
- Under "Open questions", delete item 3 (how a deployment becomes qualified to park) and item 5 (the `DatabaseBusy` flake, whose test is deleted). Renumber.
- Replace every remaining "qualification" or "qualified" with "verification" or "verified" where it means live hardware evidence, and delete it where it meant the ceremony. Run `grep -n -i "qualif\|candidate" docs/runbooks/f2-current-status.md` and read each hit.

- [ ] **Step 4: AGENTS.md**

Line 68: `- crates/mllm-cli/tests/live_interactive.rs (untracked)` becomes `- crates/mllm-cli/tests/live_interactive.rs (tracked; owner's)`.

- [ ] **Step 5: Commit**

```bash
git add docs/runbooks/f2-current-status.md AGENTS.md crates
git commit -m "docs: record the removal of qualification and the retry budget

The status runbook lists the commits, drops the open questions the deletion
answered, and names what remains: ordinary park, the ordinary native launch, and
the native adapter blockers. T20 and T21 tags are re-homed on surviving tests.
AGENTS.md's excluded-files list says live_interactive.rs is tracked."
```

---

## Spec coverage

| Spec section | Task |
|---|---|
| §1 principle | Task 2 (ADR 0011 decision 2) |
| §2.1 deleted | Tasks 7, 8 |
| §2.2 moved: park contract | Task 3 (in `mllm-domain`, deviation 1) |
| §2.2 moved: salvage types, SGLang pins, native types | Task 4 |
| §2.2 moved: native handoff | Task 5 |
| §2.2 moved: `request_leases` settle | Task 8 Step 7 deletes the dispatch API it served; no ordinary path writes `request_leases`, so nothing is ported. State this in the Task 8 commit if confirmed by the grep. |
| §2.3 kept | no change |
| §2.4 `InspectOwnedGone` | Task 4 Step 2 |
| §3 schema and identity | Tasks 8, 9 |
| §4 fixture, tests, T-tags | Tasks 6, 7, 13 |
| §5 documents | Tasks 2, 13 |
| §6 order | Tasks 1 to 13 |
| §7 not decided | Task 2 (ADR 0011 "does not decide"), Task 13 runbook |
| §8 risks | Task 4 (SGLang pins first), Task 6 (fixture first), Task 8 (guards with the drop) |

## What this plan deliberately leaves undone

- The ordinary park transition and its store module. `mllm-domain/src/park.rs` is the contract; the park ADR wires it.
- The ordinary native launch: a production `NativeLaunchSource`, the served-name rule, the private descriptor tag, and `ProfileBindings` accepting SGLang.
- Host-policy plumbing for `max_attempts` and `retry_cooldown` (F3).
- The legacy F1 `Controller` in `crates/mllm-controller/src/operations.rs` and its `park_flow.rs` tests, slated for retirement at the A2d gate by the runbook, untouched here.

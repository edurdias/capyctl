# Removing qualification from mllm

**Date:** 2026-09-17
**Status:** Approved by the owner in design review; awaiting the implementation plan.
**Governs:** the execution of ADR 0011 decision 2 and the remaining ADR 0011 tasks.
**Supersedes:** `docs/plans/2026-09-16-state-machine-owns-recovery.md` tasks 3 to 5,
which this design re-sequences.

## 1. Principle

mllm guards the host. The user owns the recipe.

The owner stated the product model on 2026-09-16: a user creates a deployment, mllm
guards against abuse of the host's resources, mllm deploys and launches, the state
machine manages the deployment, retries on failure, and stops after a bounded number
of attempts. On 2026-09-17 the owner clarified that qualification is not an mllm
concept at all. It exists in a different product and was carried into mllm by
mistake. It is removed, not relocated, and no interface to any other product is
designed for it.

Before launch mllm checks two things: that the recipe has a valid shape (engine
known, reserved flags not overridden, SPEC §8.2), and that the host can hold it
(admission against the declared ceiling, ADR 0007). It then launches and watches
readiness. Whether the recipe works is the user's responsibility. A broken recipe
surfaces as failed attempts and, after the budget, a terminal `Failed` deployment.

The test applied to every piece of the current subsystem: does it stop mllm from
blowing the machine, or does it judge the recipe? Guards stay. Judgements go.

## 2. Inventory

### 2.1 Deleted

Code whose purpose is to judge a recipe, or ceremony around that judgement.

- `crates/mllm-store/src/candidate_creation.rs` and every child except `warm.rs`:
  run acceptance, `CandidateRunState`, initialize, inference, security, abort,
  cleanup, progression.
- `crates/mllm-store/src/qualification.rs`, `qualification/`,
  `qualification_policy.rs`, `qualification_policy/`: catalog, receipts, evidence
  references, request-budget and coverage ledger, marker probes, recipe corpus,
  security cases, host qualification policy.
- `crates/mllm-controller/src/qualification.rs`, `coordinator/candidate*.rs`, the five
  candidate lanes in the worker loop (cleanup, inference, security, warm,
  initialize), `candidate_factory`, `candidate_requests`,
  `spawn_with_candidate_factory`, `CandidateLifecycleAction`, and the commands
  `candidate_inference`, `initialize_candidate`, `candidate_action`, with their
  `coordinator_port` counterparts.
- `crates/mllm-management`: the `/management/v1/qualification-runs*` routes, their
  DTOs and handlers; the `CandidateLifecycle*` event variants.
- `crates/mllm-domain/src/qualification.rs`.
- In `crates/mllm-store/src/dispatch.rs`: `grant_dispatch`, `close_dispatch`,
  `finish_dispatch`, `pending_dispatches`, which have no production caller once the
  candidate path is gone.
- The `is_ordinary` guards: literal SQL asserting no qualification run exists, at
  `ordinary_lifecycle.rs`, `ordinary_lifecycle/receipt.rs`,
  `ordinary_lifecycle/cleanup.rs`, `managed_configuration.rs`, `resource_ledger.rs`.
  They are deleted, not replaced. After removal every deployment is ordinary; a
  positive flag would always be true.
- Tests: `tests_candidate*.rs`, `qualification_progression.rs`,
  `candidate_completion.rs`, every `candidate_creation/**/tests.rs` except those of
  `warm.rs`, every `qualification/**/tests.rs`, and the candidate parts of
  `tests/qualification_support/fixture.rs`. The `DatabaseBusy` flake documented in
  the runbook lives in `qualification_progression.rs` and goes with it.

### 2.2 Moved

Code that guards the machine but lives inside the deleted module.

- The park contract in `candidate_creation/warm.rs`: effect sequencing
  (Park = Drain, Park; Restore = Restore, ReloadWeights, InvalidateCache, Probe),
  per-effect fact accumulation, the footprint join that only grows the retained
  reservation, the predecessor rule requiring a strictly increasing
  `committed_epoch`, and the parked-status acceptance predicate (no allocations,
  weights or cache; quiesced; no unknown work; activity counters unmoved; no
  outstanding request lease; observation fresher than the park effect; identities
  equal to the owned association; then `verify_completion` over the accumulated
  milestones). This is the only definition of "parked" in the repository. It moves to
  `crates/mllm-store/src/ordinary_lifecycle/park.rs` with its unit tests. It has no
  caller until the ordinary park ADR wires it; that is accepted (see §7).
- `ArmResult` from `candidate_creation/initialize.rs`, `CleanupMode` and
  `CleanupExecutionContext` from `candidate_creation/cleanup.rs`, into
  `ordinary_lifecycle`.
- The SGLang recipe pins `NATIVE_SGLANG_RECIPE`, `NATIVE_CHECKPOINT_REVISION`,
  `NATIVE_SGLANG_SOURCE_REVISION` from `crates/mllm-config/src/effective/candidate.rs`
  to `crates/mllm-config/src/effective/sglang.rs`. `crates/mllm-adapters/src/sglang/args.rs`
  imports them; deleting the file without moving them breaks the SGLang adapter.
- `NativeCandidateHandoff` in `crates/mllm-controller/src/runtime.rs` becomes
  `NativeLaunchHandoff` and produces the ordinary native launch. It is the only
  producer of a native launch and the owner wants both engines. The move opens
  nothing: native entrypoint denials stay closed until their prerequisites are met,
  and `ProfileBindings` still refuses SGLang for the missing admin credential and
  observation socket.
- The behaviour of `dispatch.rs::settle_verified_candidate_cleanup`, clearing
  `request_leases` once processes are proven gone, moves into ordinary cleanup if
  the ordinary path writes `request_leases`. Verified during execution, not assumed.

### 2.3 Kept as is

Readiness probing before `READY`; the gone-proof in
`crates/mllm-launchers/src/process_absence.rs` used by ordinary cleanup; the
admission ledger; the operational evidence vocabulary `Milestone` in
`crates/mllm-domain/src/completion.rs`; the deep-park security gates in
`crates/mllm-adapters/src/vllm/adapter.rs` and `fake/engine.rs`; `ParkPolicy::Denied`
as the agent default.

### 2.4 Resolved by inspection during execution

`CleanupMode::InspectOwnedGone` is the restart-recovery cleanup mode. SPEC §13.2
requires inspecting owned handles on restart. If ordinary reconciliation reaches it,
it stays. If only the candidate path reached it, it goes and ADR 0011 records the gap
under "what this does not decide".

## 3. Store and schema

Schema v13 is a forward-only migration dropping, in foreign-key order:
`qualification_evidence_refs`, `qualification_ready_probes`,
`qualification_request_attempts`, `qualification_request_results`,
`qualification_case_actions`, `qualification_parked_status`,
`candidate_cleanup_actions`, `qualifications`, `qualification_runs`,
`host_qualification_policies`.

Shared tables stay: `owned_launch_associations`, `request_leases`,
`deployment_attempts` (v12).

On-disk shapes that carried candidate fields change without compatibility:
`TransitionToken.qualification_id` is removed from `lifecycle_steps.step_json`; the
`CandidateLifecycle*` variants, serialized with tag `"1"` into `management_events`,
are removed; `DecodedBinding::Candidate` is removed and a non-V1 binding becomes a
decode error rather than an assumed candidate.

No compatibility path is built. State directories created before 2026-09-16 must
already be deleted because of the `StoredPolicy` shape decision. Directories at v12
from this branch have zero rows in the dropped tables, because the Fake-only
qualification path never ran outside tests. ADR 0011 states this.

`crates/mllm-store/src/lifecycle/completion.rs` is split: its candidate half
(`accounting()` dispatching into `validate_gone_history` and `progression`, the
candidate branches of `record_owned_launch`, the candidate half of `complete_step`)
is deleted and the ordinary half stays. `lifecycle.rs` loses
`PreparedBinding::prepare_candidate` and `candidate_handoff_states`.

## 4. Tests and acceptance coverage

`crates/mllm-controller/tests/qualification_support/fixture.rs` is path-included by
`coordinator/tests.rs`, `mllm-management/tests/actions.rs` and
`mllm-management/tests/events.rs`. Its `qualified()` fixture holds the one assertion
behind all 49 current controller failures. The ordinary parts (Fake store, session,
clock, worker spawn, ordinary start helpers) move to
`crates/mllm-controller/tests/support/fixture.rs`; the three includers repoint; the
candidate parts are deleted with the file. `store/lifecycle/completion/tests.rs`
drops its include of `candidate_creation/cleanup/tests.rs`.

Acceptance-matrix identifiers at risk of silent loss:

- T20 (park or reload partial failure, no blind repeated collective): re-homed to the
  `park.rs` predicate tests and to the retry tests, where an uncertain attempt whose
  processes are not proven gone is not retried.
- T21 (vLLM experimental controls denied by default): the candidate security cases
  asserted it, but enforcement is in the adapters. Adapter tests tagged `// T21` are
  added if none exist.
- T14: the SPEC row loses "old qualification invalidated"; the test tag follows.

Mechanical check: the set of `// T<nn>` tags across crates is compared before and
after. No identifier present before may be absent after, except one whose SPEC row
was removed.

Verification per task is scoped to the crates touched. The core suite runs once at
the end. The controller library suite is expected to return to about nine minutes
once the 49 failures are gone. CPU and Fake-engine tests are not verification of any
native recipe, and every status claim says so.

## 5. Documents

Deleted, because their subject is the deleted code:
`docs/plans/2026-09-12-f2c-mixed-engine-qualification.md`,
`docs/plans/2026-09-14-f2-native-candidate-handoff.md`,
`docs/runbooks/f2-mixed-engine-qualification.md`,
`docs/runbooks/f2-sglang-qualification.md`, `docs/AGENT_HANDOFF.md`, and
`docs/plans/2026-09-16-state-machine-owns-recovery.md` once the new plan
replaces it. `f2-planning-index.md` entries are removed. Git keeps the history.

Kept unchanged: the hardware evidence runbooks `spark-qualification-f1.md`,
`spark-model-size-qualification.md`, `spark-deep-wake-optimization.md`, which record
what ran on `host-a`; and the executed historical plans f2a1 to f2b.

Amended in one place each:

- ADR 0009: the paragraph "Qualification becomes an authority on the one lifecycle"
  and consequence 4 receive a one-line note that ADR 0011 supersedes them and
  `candidate_creation` is deleted rather than collapsed.
- ADR 0007 and 0008 use "candidate" for a deployment under admission. Different
  sense. Unchanged.
- `docs/design/milestones/f2-sglang-design.md` §6 and §8: the qualification
  requirement is replaced by a pointer to ADR 0011.
- `docs/SPEC.md`, citing ADR 0011 inline as ADR 0010 is cited in §6.2:
  §8.4 removed and its number left vacant, with one sentence in §8.1 on recipe
  ownership; §6.3 rows `preinitialize deployment` and `park deployment` say
  declared-tier parking; §6.4 status drops the qualification field; §13.2 "until
  requalification" becomes the attempt budget of ADR 0011 decision 5; §14 loses
  `mllm qualify deployment`; §15.1 and §17 drop qualification results and reasons;
  §18 F2 gate says declared-tier parking and live verification on authorized
  hardware, F4 is retitled "Distributed and cache verification", and the owner
  requirement paragraph near line 368 is reworded the same way; §19 names live
  verification and the vLLM security issue as release gates; §20 T14 and T36 lose
  their qualification wording; §21 says retain operational evidence.

ADR 0011 is rewritten: decision 2 carries the principle in §1 of this document,
consequences reflect §2 to §4, and "what this does not decide" keeps the ordinary
park ADR pending. Status stays Proposed until executed.

`docs/runbooks/f2-current-status.md` is updated once at the end: qualification
program lines removed, flake entry removed, attempt budget and the `park.rs`
contract module recorded, native launch still gated. `AGENTS.md` gets a one-word fix:
`live_interactive.rs` is tracked, not untracked; it remains excluded.

## 6. Execution order

Each step leaves a compiling, green tree. Nothing deletes what a later step needs.

1. Commit Task 3 as it stands in the worktree (a failed deployment closes its own
   admission, not the host's). Scoped test: `mllm-controller` coordinator tests.
2. Documents: ADR 0011 rewrite, SPEC edits, ADR 0009 note, F2 design edits, the
   deletions in §5, this design, the plan. One commit, so authority is set before
   code moves.
3. Move before delete: `warm.rs` contract to `ordinary_lifecycle/park.rs` with
   tests; the three salvage types; SGLang pins; `NativeLaunchHandoff` rename;
   `request_leases` settle into ordinary cleanup if needed; the
   `InspectOwnedGone` decision recorded. Test: `mllm-store`, `mllm-adapters`,
   `mllm-config`, `mllm-controller` lib.
4. Split shared code: `completion.rs`, `lifecycle.rs`, the test fixture into
   `tests/support/`. Test: `mllm-store`, `mllm-controller`, `mllm-management`.
5. Delete everything in §2.1. Test: the four crates above plus `mllm-domain`.
6. Schema v13. Migration test from a v12 fixture. Test: `mllm-store`.
7. Rename the `Qualified` prefix off the ordinary types: `QualifiedStartReceipt`,
   `accept_qualified_start_command`, `qualified_start_command_receipt`,
   `QualifiedInitializeWork`, `QualifiedInitializeStatus`, `QualifiedInitializePoll`,
   `next_qualified_initialize_or_expire`. Compiler-driven. Own commit; droppable if
   the diff bloats.
8. ADR 0011 tasks 4 and 5: the retry budget wired to `Store::record_attempt`,
   cooldown from host policy with defaults of three attempts and 30 seconds
   doubling, `Uncertain` resolved through the gone-proof before any retry, processes
   not gone surfaced and never retried. Test: `mllm-controller`, `mllm-store`.
9. Verify and record: the core suite once, clippy with warnings denied, the T-tag
   diff, the runbook update, the AGENTS.md fix.

One review at the end, per the owner's instruction. Mechanical steps 4, 6 and 7 are
suitable for a smaller model.

## 7. Not decided here

- The ordinary park transition: drain, park, parked accounting, wake. The next ADR
  wires `park.rs`. Until then the module is contract plus tests with no caller.
- Eviction policy.
- The native adapter blockers: `VllmAdapter` has no `execute_persisted`;
  `ProfileBindings` refuses SGLang. Both block parking on a real engine regardless.

## 8. Risks named by the survey

Recorded so the plan addresses each: the ordinary lifecycle's identity is defined
negatively in five SQL strings (§2.1 deletes them); the SGLang adapter depends on
constants inside the deletion scope (§2.2 moves them first); one path-included
fixture ties three test targets to the deleted tree (§4 splits it first); deleting
this lane removes the only machinery that ever produced native park evidence, which
is accepted because that evidence judged recipes, and the machine-guarding part of it
is the contract kept in `park.rs`.

## 9. Amendments during planning

1. The park contract moves to `crates/mllm-domain/src/park.rs` instead of
   `crates/mllm-store/src/ordinary_lifecycle/park.rs`, because its rules are
   inseparable from candidate SQL types and ADR 0009 puts database-free rules there.
2. `qualification_id` is retired everywhere — host YAML key, effective-config field,
   binding DTO field and `RuntimeBinding` field — not only as a token field.
3. The Fake engine's qualification-named lifecycle simulation is kept and renamed
   rather than deleted, stripped of its candidate-only cases.
4. The retry budget and cooldown land on `CoordinatorOptions` with the ADR's
   defaults now, since host-policy plumbing for them follows only when remote hosts
   publish policy in F3.
5. The private descriptor's `sglang_candidate_private_launch` tag and the
   `candidate-{binding_id}` served-name rule stay as wire strings belonging to the
   undesigned ordinary native launch.

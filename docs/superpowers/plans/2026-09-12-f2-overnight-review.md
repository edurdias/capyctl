# F2 overnight decisions and morning review

The owner authorized finishing implementation plans, reviewing them with
a structured document review, correcting findings, and then implementing
them task by task. Do not begin implementation until the plan review is resolved for
the work being executed. The owner will review consequential questions in the morning.

## Boundaries

- Only host-a is available to this work. Never access host-b.
- Planning and deterministic implementation do not require live engine launches.
- Do not install engines, change drivers, reboot hardware, or make destructive
  cleanup decisions on inferred overnight permission.
- Preserve the unrelated untracked CLI live-interactive test.
- The owner requested compressed communication overnight; durable technical
  documents retain precise terminology and explicit safety conditions.

## Open questions

No owner-only decision has been identified yet. Technical feasibility questions
belong in the review findings and should be investigated before escalating here.

## Progress

- Resource, reservation, completion-evidence, and dispatch-ownership plans exist.
- Coordinator integration draft now connects those primitives and production cutover.
- Management/configuration, pinned SGLang integration, and numerical live
  qualification plans are written and reviewed.
- Five local reviewers completed document review. Eight merged fixes landed:
  resident-credit accounting, domain completion contracts, 2-second observation
  TTL, scoped candidate qualification, host-policy updates, attachment detach,
  one adapters-owned lifecycle interface, and single ownership of live execution.
- Extracted examples: 69 tests pass, Clippy passes. This includes existing fixture
  baseline tests; it is not a claim of 69 new product tests or live qualification.
- Cross-model review skipped the full bundle at its size cap. No peer review claimed.
- Product implementation started with CPU-only domain contracts; live qualification
  has not started. Task reviews gate each subsequent implementation task.
- F2A1 is implemented and reviewed through `5c2cbbe`: domain contracts, validation,
  admission, epoch-bound proposals, sequence forecasts, property tests, ADR 0007.
- All six task reviews completed. Broader review found no blocking kernel defect;
  three hardening fixes landed and passed scoped re-review: API safety notes,
  admission-level device/floor regressions, and coordinator diagnostic ownership.
- Fresh root checks: 172 workspace tests excluding CLI plus 2 CLI library tests
  pass; no failures or ignored tests. Scoped Clippy passes. CLI integration targets
  remain excluded. These results do not qualify live engines.
- F2A2a started: V3 schema and legacy-preservation migration test committed at
  `e6d8d88`, with task review approved and 17 store tests passing. Snapshot reads
  and atomic grant writes follow. No production database or Spark runtime accessed.

## Execution decisions

- Continue in existing `feat/f2-sglang` checkout. Do not create an unrequested
  worktree. Preserve unrelated files; commit exact owned paths. Cost: less filesystem
  isolation if another editor changes the same files.
- Missing per-owner physical attribution gets zero admission credit. This can
  reject feasible arrangements conservatively; reservation ceilings cannot safely
  substitute for measured residency.
- Define completion validation in domain from the start. Scheduler reexports
  shared resource validation. Cost: maintaining public reexports, not a crate cycle.
- Exclude unrelated CLI integration targets from tests and Clippy. Check CLI library
  and binary explicitly. Cost: those integration targets receive no overnight evidence.
- Keep the admission commit's descriptive subject despite differing from the plan's
  example wording. Cost: history wording differs; behavior and safety are unchanged.
- Coordinator constructs contextual diagnostics; kernel keeps unit error variants.
  Cost: richer shared diagnostic types may be needed during coordinator work.
- Stage the private encoder with the grant writer, after decoder-only snapshot
  reads. Cost: round-trip test moves one task later; malformed-data coverage stays
  in the read task. Each intermediate task remains warning-free.

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

- Please check unrelated untracked `crates/mllm-cli/tests/live_interactive.rs` for
  unwanted formatting. During dispatch Task 3, the implementer ran `cargo fmt -- <paths>`;
  Cargo formatted the workspace instead of limiting scope. Tracked spill was
  restored byte-exactly; root confirms no remaining unstaged Rust diff. No original
  baseline exists for the untracked file, so no
  restoration is attempted. It is not staged or executed. Root cannot certify its
  contents unchanged after that command.

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
- F2A2a implementation through `fcf7cfb`: V3 migration, validated snapshots,
  atomic increasing grants, fencing, rollback, replay, and contention tests.
  Four task reviews and broader review approved. Fresh root checks:
  184 workspace tests excluding CLI plus 2 CLI library tests pass. Scoped Clippy
  clean. No production database or Spark runtime accessed.
- F2A2b complete through `0f5a576`: bounded host-memory observer and completion
  validator with full process/token/time/milestone checks. Task and broader reviews
  complete; formatting fix re-reviewed. Full checks at `60fd33a`: 194 CPU tests
  pass, scoped Clippy clean. Post-format memory tests and rustfmt check pass.
- Coordinator staging amendments reviewed: five local personas, three scoped local
  checks, and three independent reviews complete. Five corrections:
  staged joint cutover, claim handoff, attachment endpoint/credential isolation,
  reusable qualification identity, and verified external-accounting reconciliation.
  A3 policy/run/event foundations precede atomic arm implementation. No pending
  document decision; no runtime or hardware qualification implied.
- F2A2c complete through `4de1a1e`: durable session fences, atomic dispatch gates,
  retained uncertain requests, explicit original-generation completion. Four task
  reviews and broader review approved. Fresh root checks: 203 CPU tests and scoped
  Clippy pass; closure race20/20. Production router remains on F1 until joint cutover.
- F2A2d Tasks 1–2 complete through `ac62c1d`: V5 schema, lifetime lock, immutable
  bindings, atomic endpoint leases, gated startup, durable process identity fences.
  Review found unsafe implicit child cleanup, missing escalation identity recheck,
  incomplete uncertainty/control guards, and EOF-as-acknowledgment behavior. One
  fix wave and scoped re-review resolved all six blocking findings. Fresh root
  checks: 217 non-CLI plus 2 CLI library tests pass; scoped Clippy clean.
  Fixed-port fixture minor carries to Task 3. Full worker collectors remain future
  adapter work; missing proof stays uncertain. No production cutover or live qualification.
- F2A2d Task 3 complete at `fc7ff46`: joined activation, atomic all-member claims,
  stop/suspend fences, and retained reconciliation handoff. Store tests: 46 pass;
  scoped Clippy clean. Review approved after placing caller-timeout integration
  proof in Task 9's real wait path. Dynamic port fixtures fixed. A3 foundations
  now precede remaining coordinator arm/completion work.
- Configuration foundation liveness checkpoint: tests drafted, source not yet changed.
  Agent temporarily added `serde_yaml`, triggering an unwanted Cargo fetch. Manifest
  dependency removed; exact lockfile additions identified for removal. Resume uses
  existing strict parser and offline Cargo. No engine installation or cache cleanup.

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
- Stage dispatch session-check helper with first consumer. Cost: Task 2 owns code
  previously shown in Task 1; final interface unchanged, intermediate warnings avoided.
- Stage coordinator deployment fence and lifecycle error declarations with Task 2's
  first binding writer. Cost: declarations move earlier; Task 3 reuses exact contracts.
- Stage V6 schema/read foundations before V7 events, then introduce policy/run writers
  with atomic events. Cost: additional partial-task tracking, not an eventless bypass.
- Stage durable gated-launch supervision beside legacy launcher compatibility.
  Persist API identity before initialization; missing full worker proof stays uncertain.
  Cost: temporary additive launcher API, removed as legacy authority at joint cutover.
- Handoff also fences other transferred members' generations and closes dispatch;
  claim transfer alone cannot invalidate same-session callbacks. Cost: temporary
  route unavailability until reconciliation, never permission to stop those models.
- Test each caller's shorter wait deadline in Task 9's real router wait path, not
  a synthetic store fixture. Task 3 tests unchanged durable operation deadline.
  Cost: caller-timeout proof remains open until router integration; A2d cannot close without it.
- Freeze previously unspecified nested F2 config shapes in A3 plan. Each phase
  supplies allocations/devices explicitly; generated binding identity stays separate.
  SGLang requires separate API/admin references. Cost: schema spelling choices may
  need later compatibility handling; no new control or qualification authority.
- Resolve completion negative-coverage finding in immediately following planned
  boundary-test task, including extra PID/invalid-target cases. Cost: intermediate
  checkpoint had happy-path-only coverage; all rejection tests now reviewed and passing.

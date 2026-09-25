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

## Morning continuation decisions

On 2026-09-14 the owner explicitly approved an isolated SGLang environment on
host-a and a reviewed memory-saver observation patch if required. Existing
engine environments and drivers remain out of scope; no reboot is authorized.
The first setup preflight stopped at a Tailscale SSH authentication check before
remote commands ran. No environment was created by that attempt.

The owner returned and asked to continue after the three recommendations were
presented. Proceed with permanent case/action ownership, frozen deployment-scope
`expected_revision`, and coordinator-selected qualification cases and inputs.
Review the concrete implementation brief before code changes. These decisions do
not authorize additional runtime effects or live work.

The owner subsequently authorized merging completed and tested work. Local merges
into `main` may proceed after independent code review and fresh verification of the
complete merge scope. No push or PR publication was requested. Partial internal
foundations do not establish that F2 runtime integration or live qualification is done.

The owner changed review cadence: test and review at the end of each module, not
with detailed reviews per task. This overrides the earlier per-task review cadence.
Keep focused tests/TDD during implementation; group related tasks into cohesive
modules and run a consolidated independent review plus verification at each module
boundary. Do not relabel every small task as a module. Candidate Initialize
acceptance, planned steps and atomic arming form the next initialization module.
Document review remains a planning gate, not a repeated gate for each small task.

- Ruling: bind each accepted Initialize action permanently to the sole frozen cold
  case — prevent a new idempotency key from creating another attempt — cancellation
  or uncertain execution requires reconciliation/cleanup rather than an automatic
  replacement; a fresh run may be needed even if no process actually started.
- Ruling: keep `expected_revision` as immutable deployment/recipe scope — matches
  persisted run revision and existing creation receipt — this is not a progress CAS;
  mutable eligibility needs transactional state, claims and case ownership checks.
- Ruling: coordinator selects and validates frozen cases and inference corpus items
  — client requests cannot supply trusted evidence — explicit client case selection
  would require a later API change if operators need that control.

## Decision details and remaining integration work

- Candidate action contract needs frozen case mapping before implementation.
  Existing `recipe_v1` allows exactly one `cold_initialize`, cycle 0, count 1;
  later cycles allow park/restore, not another cold start. Recommendation: bind
  Initialize permanently to that case; uncertain attempts never create replacement
  permission. Action receipt and planned step need a distinct pending operation;
  succeeded creation operation cannot authorize execution.
  Architecture proposal recommends unique `(run_id, case_id)` action association;
  existing idempotency keys alone cannot prevent another key consuming the same case.
  Permanent ownership/no-refund direction is accepted above; exact schema remains
  subject to the implementation brief and document review.
- Candidate action `expected_revision` needs explicit meaning. Current run revision
  names frozen deployment revision, not mutable action progress. Recommendation:
  preserve that meaning; serialize action eligibility with state, claims and durable
  case ownership. A separate progress CAS would require an explicit schema/API change.
  Implementation follows the reviewed acceptance/planned-step brief.
- Atomic arm must compose with existing spawn ownership. `arm_runtime_spawn`
  currently consumes binding state `reserved` into `uncertain` in its own transaction.
  New arm cannot consume that state twice. Planned steps have no grant or issue time;
  only committed arm supplies those fields. Define joint transition before dispatch.
- Future run inference needs explicit case/corpus-item selection. Current route fields
  name request/revision only. Working recommendation: trusted coordinator selects and
  validates case/item, then freezes mapping in retry receipt; client never supplies
  evidence or pass predicates. Coordinator selection is accepted above; exact
  corpus/evaluator integration remains outside the Initialize-only slice.
- Please check unrelated untracked `crates/mllm-cli/tests/live_interactive.rs` for
  unwanted formatting. During dispatch Task 3, `cargo fmt -- <paths>` was run;
  Cargo formatted the workspace instead of limiting scope. Tracked spill was
  restored byte-exactly; root confirms no remaining unstaged Rust diff. No original
  baseline exists for the untracked file, so no
  restoration is attempted. It is not staged or executed. Root cannot certify its
  contents unchanged after that command.

## Progress

- The bounded memory-saver observer prerequisite is implemented at `782a3f4`.
  It adds a minimal patch against the pinned 0.0.9.post1 source, coherent in-flight
  mutation observation, and a strict same-loaded-library Python reader. CPU tests
  compile the real patched allocator with driver stubs; deterministic tests first
  reproduced the map-before-insertion and erase-before-unmap races, then passed
  with mutation bookkeeping. One consolidated independent module review passed
  specification and quality with no findings. Fresh root verification passed
  27 observer tests and 509 existing Rust tests, plus both canonical Clippy gates
  with warnings denied. The reviewed tree is
  `c9bfdbfa2e465658a91b24e8fec1e0fc0a6539a1`. See the
  [observer plan](2026-09-14-f2b-memory-saver-observer.md) and
  [patch provenance and limitations](../../runtime/patches/README.md).
  This module performs no installation or host operation. Protected scheduler
  integration, actual loaded-library identity, reviewed isolated CUDA build,
  native allocation/lifecycle qualification and full F2 remain open. The observer
  cannot acknowledge parking, release reservations, or establish whole-process
  residency. The owner-requested module-level review kept all three related
  checkpoints together; focused TDD and one full review covered the combined diff.

- The approved isolated SGLang environment now exists on host-a at
  `$HOME/mllm-sglang-f2-venv`. The binary-only, hash-locked installation
  contains SGLang 0.5.16, Torch 2.11.0 and the unmodified memory saver 0.0.9.post1;
  all 206 installed artifact hashes match the resolution report. `pip check`
  passes and system site packages are disabled. The directory occupies 12 GB
  including its private download cache. An isolated, offline import check with
  GPUs hidden passed: Torch 2.11.0+cu130, SGLang 0.5.16, CUDA uninitialized.
  No server/model launch, GPU qualification, observation patch or driver change
  was performed. Runtime
  and JIT compatibility remain untested; dependency resolution includes newer
  CUDA compiler components and must not be mistaken for qualified compatibility.

- Qualification implementation is complete through `37fade7` and cleared for
  local integration. The actual Fake flow completes 12 requests and 17 evidence references,
  writes an immutable catalog, performs verified owned cleanup, and resolves a
  fresh compatible managed binding without granting dispatch authority. Warm
  transitions preserve the incarnation/endpoint and retain conservative increases.
  Failed/lost-reply cleanup preserves request spending and unsuccessful history.
  The consolidated review found three blocking issues: unfinished Security history
  after session rollover, unenforced frozen request limits, and cyclic predecessor
  corruption. Fix `3209cb5` passed fresh root verification of 507 tests and both
  scoped Clippy gates. Scoped re-review cleared request limits and cyclic history.
  Fix `37fade7` resolves the remaining pending-cleanup restart ordering for both
  Security inference subchecks, including inspection-only recovery after termination.
  Fresh root verification passed 509 tests and both canonical Clippy gates; the
  second scoped re-review found no remaining blocker or new breakage. Repeated
  valid-history CPU cost remains a nonblocking follow-up for this Fake module and
  an explicit measurement gate before production coordinator cutover. Native
  qualification, ordinary execution, secure management/CLI, router carryovers and
  full F2 remain open. No native runtime qualification follows from these CPU tests.
- Subsequent native preparation used read-only file checks on host-a and
  local static inspection of a published ARM64 memory-saver artifact. No engine
  imports, GPU commands, launches, installs, driver changes or reboot occurred.
  The existing Qwen3-4B checkpoint and vLLM metadata were located; artifact
  provenance, complete launch settings and native allocation evidence remain
  qualification prerequisites. No access to host-b occurred.

- Candidate ownership/completion/cleanup module merged locally into `main` at
  `9c7feb7`. The consolidated review found a repeated-restart recovery defect:
  an unarmed inspection successor could restore termination authority. The fix
  preserves inspection-only recovery across the complete chain; a three-session
  regression proves one termination send. Scoped re-review passed. Fresh fix and
  post-merge gates each passed 465 CPU tests and scoped Clippy with warnings denied.
  Cleanup settlement also reuses one validated chain instead of repeated reads.
  This completes the candidate protocol, not ordinary lifecycle or native recovery.
- Qualification progression is the next cohesive module: accounted probes, marker
  cases, security checks, warm cycles, trusted evaluation, immutable Fake catalog,
  and verified-cleanup eligibility. Five local document lenses and three independent
  reviews completed, plus one limited local interface check. One family-label
  correction was applied. The new candidate-action version preserves legacy history;
  one accounted ReadyProbe supplies lifecycle and qualification readiness together.
  Native qualification and full F2 remain open. No host access or installs occurred.

- Fit-based planner module merged locally into `main` at `8c355c0`. One consolidated
  module review found no critical, important, or minor issues. Fresh pre-merge and
  post-merge gates each passed 443 CPU tests; scoped Clippy passed with warnings
  denied. Activation/preparation share complete-prefix forecasting, bounded
  deterministic search, qualified parking and conditional verified cleanup. These
  are pure forecasts, not engine effects. Full F2 and the F1 carryovers remain open.
  The next cohesive module implements candidate ownership association, completion,
  cleanup authority, historical replay and verified release. Native execution and
  ordinary coordinator/router/API/CLI cutover remain subsequent required work.
- Owner reconfirmed completing full F2 functionality and the relevant F1 carryovers.
  Current production audit still finds synthetic activation accounting, missing
  engine credentials, append-only logs, readiness error fallbacks and ignored stream
  send failures. These are mandatory joint-cutover work, not deferred beyond F2.
- Candidate initialization module is implemented through `a5e17bd`: acceptance,
  permanent case ownership, planned-step reads, Fake atomic arm and restart fencing.
  Five local document reviewers and three independent reviews completed;
  one correction aligned the intermediate checkpoint with module-end review.
  Fresh root module gate: 416 CPU tests pass, both scoped Clippy commands pass with
  warnings denied, and `git diff --check` passes. The unrelated CLI integration test
  remains excluded. Consolidated independent module review passed with no critical
  or important findings. Local integration is authorized; F2 remains open.
  Minor follow-up: strengthen the V7-to-V8 migration fixture to preserve a known
  event incarnation, exercise enabled foreign-key enforcement, retain revision and
  generation, and assert all eight migration stamps. The additive SQL was reviewed
  as correct; the gap is regression coverage rather than a known migration defect.
  One earlier endpoint-allocation fixture failure passed unchanged in isolation and
  did not recur in the fresh gate. Its cause remains unproven, not reported fixed.
  Native allocator validation remains an explicit F2 dependency; Fake-only arm tests
  cannot qualify vLLM or SGLang. Pinned-source investigation is documented; native
  launch validation and trusted preflight remain implementation work.
- Earlier verified checkpoint `7030923`: transaction-local reservation helper reviewed
  clean; root 394 CPU tests and scoped Clippy pass. Candidate creation store reviewed
  through `5f803fb`; historical snapshot reviewed through
  `59633e8`. One minor test risk retained: fixed port 65535 can conflict with another
  local listener. Next work: frozen-step/execution-context prerequisite planning. No runtime
  arm/dispatch integration, live qualification, F2 completion or merge claimed.
- Candidate coverage reviewed through `6d8e89c`: canonical bytes/digest, strict
  nested schema, budgets and F2C fixture (30 cases, 384 markers, 391 total requests).
  Review fixed misleading count/order/minimum-boundary tests. Root `70e23ad` check:
  347 CPU tests/Clippy pass. A later rerun exposed launcher termination race
  (`ESRCH` after short-lived process exits). Repair reviewed through `2fd3d18`;
  full CPU gate subsequently passes, including latest checkpoint above.
- Candidate shared normalization reviewed through `8a4d039`. Both wrappers share
  host/profile/recipe validation and unchanged recipe fingerprint projection.
  Five review findings fixed: bounded cycle validation, typed views, null rejection,
  text limits and UTF-8 selectors. Fresh checks: 339 CPU tests and scoped Clippy pass,
  including 89 config tests. Exhaustive fixture/error coverage subsequently closed;
  candidate acceptance subsequently completed. Runtime authority and live qualification remain open.
- Durable resource-policy store reviewed through `88c7f3f`: bootstrap, revisioned
  updates, strict receipts, retained overcommit, atomic events and host-operation
  lookup. Two fix rounds closed identity/receipt validation and failure-test gaps.
  Fresh root check: 314 CPU tests and scoped Clippy pass, including 88 store tests.
  Runtime update-versus-arm race remains
  an explicit coordinator gate; these store tests do not replace it.
- Resource-controls config dependency reviewed through `dbb4100`: immutable host
  context, owned mutable controls, shared structural validation. Review gaps fixed
  with boundary and real host-input tests; all 64 config tests and scoped Clippy
  pass. Root baseline workspace suite also passes. Durable resource-policy store
  implementation now reviewed above; atomic arm, production cutover and live tests remain open.
- Candidate normalization contract reviewed by five local review lenses, one focused
  interface check and three independent reviews. Two document corrections:
  permit required parser object-array repair and remove full host-policy retention.
  Candidate normalization itself creates no permission, grant or runtime effect.
- Qualification importer reviewed through `b7109ca`: atomic session-fenced updates,
  revision tombstones, bounded strict reads, redacted events and retained accounting.
  Root checks: 287 CPU tests pass; scoped Clippy clean. Resource-policy contract
  reviewed by five local review lenses and three independent reviews. Local
  resource config bootstraps the database; accepted DB controls subsequently win.
  Domain/device/port changes need separate migration, not resource-limit updates.
- Qualification-policy input reviewed through `cec820f`: strict optional policy,
  independent permissions, canonical allowlist, shared validation, safe nested
  errors. Root checks: 272 CPU tests pass; scoped Clippy clean. Importer contract
  reviewed by five local review lenses, one focused check and three independent
  reviews. Removing policy retains a revoked revision tombstone; re-add requires
  the next revision. Existing runs, grants and original cleanup authority remain.
- Earlier checkpoint: V6 management schema and V7 durable event foundation reviewed
  through `bbf92cd`. Event replay enforces count, byte, age and payload bounds;
  session reset appends atomically. Two fix rounds closed review findings.
  Full CPU checks at `808a8ac`: 264 tests pass and scoped Clippy clean. Final
  serializer adjustment: nine event tests and scoped store Clippy pass.
  Full snapshots, SSE, remaining writer events and live qualification remain open.
  Subsequent qualification-policy slices are recorded above.
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
  clean. No production database or host runtime accessed.
- F2A2b complete through `0f5a576`: bounded host-memory observer and completion
  validator with full process/token/time/milestone checks. Task and broader reviews
  complete; formatting fix re-reviewed. Full checks at `60fd33a`: 194 CPU tests
  pass, scoped Clippy clean. Post-format memory tests and rustfmt check pass.
- Coordinator staging amendments reviewed: five local review lenses, three scoped local
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
- Configuration foundations complete through `b46b9b7`; 253 scoped CPU tests pass,
  Clippy clean. Review confirms owned launch settings, policy defaults/bounds and
  identity regressions. Minor field-path diagnostic improvement deferred to next
  config touch. Parent A3 Task 1 remains open for authority/production integration.
  Temporary `serde_yaml` additions removed surgically before implementation; root
  verified lockfile cleanup. Subsequent Cargo offline. No engine installation or cache cleanup.

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
- Define policy maxima separately from product defaults: 4096 pending/deployment,
  16384 total, 1 GiB queued bodies, 1 h request deadline, 30 s admission window,
  10 s observation TTL, 65536 planner states, 16 parked. Missing bounded fields use
  documented defaults; capacity/credentials remain explicit. Cost: later operational
  needs may require reviewed limit changes. No live performance promise.
- Add required engine-tagged launch settings; hash explicit parallelism, allocator
  requests and expanded SGLang recipe. Arm approves exact settings, never silently
  changes a qualified allocation. Shared normalized types live in domain. Cost:
  additional manifest fields and a domain serialization dependency; native support
  and trusted qualification still require later adapter gates.
- Extend unimplemented V6 with qualification policy/catalog/evidence tables and
  exact run binding, lifetime request count, and verified-cleanup status. Cost:
  three added tables and corresponding bounded writer tests; no applied migration
  rewritten. Resource-policy API cannot edit qualification permissions.
- New contract doc-review completed with five local review lenses, two focused local
  checks, and three independent reviews. Clarified optional
  completion targets for control/cleanup steps. No owner-only decisions.
  Initial SGLang schema accepts the reviewed Qwen3-4B recipe only; further recipes
  need schema/catalog extension and separate qualification. Loopback listener
  attestation remains potential defense-in-depth work, not a claimed guarantee.
- Resolve completion negative-coverage finding in immediately following planned
  boundary-test task, including extra PID/invalid-target cases. Cost: intermediate
  checkpoint had happy-path-only coverage; all rejection tests now reviewed and passing.
- Qualification import keeps one local host identity per database. Foreign host rows
  conflict; multiple rows fail as corrupt state. Cost: hostname changes require
  reviewed migration. Earlier document review treated this restriction as optional;
  implemented and reviewed code chose stricter behavior. Future readers preserve it.
- Candidate manifests use closed ordered recipe cases and per-case request budgets.
  Cost: new case kinds need versioned extension; candidate dispatch must durably
  enforce case bounds alongside lifetime counts before runtime effects.
- Candidate config work includes narrow shared-parser array repair. Cost: parser
  behavior changes need object/nested-array and duplicate-key regression coverage.
- Candidate descriptors do not retain full host policy. Cost: store consumers
  compare frozen recipe identity against separately persisted current policy.
- Candidate descriptor cap counts complete encoded logical envelope, including
  credential references and metadata, with reviewed manifest once. Not heap usage.
  Cost: near-limit inputs passing old raw-length sum now reject; future persisted
  DTO has separate 1 MiB cap. Canonical cap is dominated by encoded input cap here.
- Snapshot document review complete: six local checks, three independent
  reviews. Three plan corrections applied; no pending owner decision.
  Permit narrow unsigned parser repair for full u64 revisions. Cost: shared scalar
  behavior changes above i64::MAX; signed/quoted/float/overflow tests required.
  Tightened-host test uses valid 3s deadline and positive matching-candidate control.
  Reuse existing exhaustive candidate matrix; add new snapshot-boundary coverage.
- Launcher disappearance race fixed and independently reviewed through 2fd3d18.
  Final CPU test and scoped Clippy command chain passed. ESRCH means already gone,
  not proof all workers exited or permission to release stored resources.
- Historical snapshot implemented at ad62126; root 363 CPU tests and scoped Clippy
  passed. Independent review requested missing boundary tests; fixed in `59633e8`.
  Parser-specific pre-fix RED was missed. Cost: weaker sensitivity evidence for
  that parser change; final tests and independent review remain mandatory.
- Candidate creation store brief prepared and reviewed. Choices
  accepted: duplicate-safe internal JSON using RawValue, private V2 binding
  with optional runtime/admin references, stable collection-scoped receipts, declared
  run/cleanup maxima bounded by policy, first unleased bindable loopback port.
  Costs: existing dependency feature, future strict decimal-string wire conversion,
  typed V2 execution integration, versioned persisted conventions, conservative
  rejection of oversized declarations and low-port preference. Store acceptance now
  implemented; no execution readiness or permanent OS endpoint ownership claimed.
- Snapshot dependency closed through 59633e8 after independent re-review; root 366 CPU
  tests and scoped Clippy pass. Candidate creation document review complete: five
  local lenses, three independent reviews, no required correction or owner decision.
  Future gates remain: accepted-only runs need verified cleanup before shared ports
  can be reclaimed; management handlers must redact internal credential/env/args views.
  These gates do not authorize automatic expiry cleanup or raw snapshot serialization.
- Corrected creation plan vocabulary before first implementation commit: deployment
  kind is `model`; runtime binding ownership is `managed`. Existing reservation code
  requires `model`. Cost: acceptance/historical tests pin both fields distinctly;
  no reservation exception or ordinary routing permission added.

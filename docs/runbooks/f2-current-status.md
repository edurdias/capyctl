# F2 continuation status

F2 is not complete. Work continues on `feat/f2-sglang`; no push or final merge is
claimed. The current user instruction is one consolidated review at the end,
not per task. Focused TDD and integration verification continue throughout.

## Recent committed work

- `67c0678`: authenticated candidate-run creation using the owned Store/session,
  shared bounded command capacity, exact durable retries, and no runtime effects.
- `1612945`: ordinary owned cleanup acceptance, arming and verified completion;
  generation fencing and atomic release only after exact cleanup evidence.
- `153f6d9`: application-owned bounded local pressure monitor with independent
  stale-read watchdog and cancellation-safe shutdown.
- `744d542`: pinned detokenizer source added to startup verification; all ten
  selected files match the isolated installation and upstream pin without imports.
- `2c2b491`: bounded coherent historical operation lookup.
- `8f238c9`: bounded F2C latency summaries with separate failures and timeouts.

The owned Fake cleanup worker passes root integration verification. It retains
the original instance, waits for Initialize to exit,
supports explicit same-session cleanup after associated uncertainty, and sends
only after a new durable cleanup arm. Unverified outcomes retain authority.

## Remaining implementation and verification

1. Add scoped Start command receipts before exposing idempotent management
   lifecycle actions.
2. Complete ordinary warm lifecycle, sequence/preinitialization, no-spawn
   terminalization, missing-association cleanup and restart reconciliation.
3. Complete owned candidate execution and API actions/inference, durable router
   accounting, management read models/policy/attachments/listener, and CLI cutover.
   Retire legacy authority only at the joint integration gate.
4. Complete guarded native startup: private launch scope, complete process
   enrollment, installed-source/device/allocator verification, scheduler observer
   attachment, and protected authentication/control composition.
5. Complete the API-driven F2C runner, protected manifest/artifacts, trusted host
   inventory, pressure-abort wiring, correctness corpus and scenario reports.
6. Run the consolidated review, fix required findings, satisfy remaining F1/M1
   gates, and perform authorized pressure-guarded native qualification after its
   prerequisites. CPU/Fake tests and source checks are not native qualification.

## Owner attention

Latest scoped root verification: Store 202, adapters 88, controller 157 and
management 42 tests pass (489 total), plus all-target Clippy. Controller and
management used four test threads to bound concurrent fixture load; internal
race tests remain enabled. The 22 harness tests and 213 Python runtime tests
passed in their respective slices. These are not native qualification results.

One existing item remains for the owner's inspection: check the untracked
`crates/mllm-cli/tests/live_interactive.rs` for formatting from the earlier
workspace-formatter incident. There is no original baseline for that file, so
the agent cannot certify or restore it. It remains excluded from reading,
editing, formatting, tests and staging. The separately modified SDD Task 2 report
also remains excluded and untouched by this continuation.

No new approval is required for the current bounded implementation. Only
host-a is authorized. The approved isolated SGLang environment and reviewed
observer patch do not authorize changing existing engine environments, drivers,
rebooting, or accessing host-b. Both native entrypoint denials remain closed;
there has been no model load or native qualification in these slices. Build and
live-effect gates remain explicit rather than inferred from passing CPU tests.

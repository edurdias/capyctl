# F2 continuation status

F2 is not complete. Work continues on `feat/f2-sglang`; no push or final merge is
claimed. The current user instruction is one consolidated review at the end,
not per task. Focused TDD and integration verification continue throughout.

## Recent committed work

- `8c9a17a`: associated candidate Cleanup through the original retained runtime,
  with clock-free discovery and verified atomic release.
- `ec05bd8`: owned candidate Abort with retained accounting and strict SSE replay.
- `c6da962`: deadline-bound owned Finish with preserved V3 catalog history.
- `8a1fbec`: owned candidate Park/Restore through the original retained Fake.
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
- `d430074`: owned ordinary Fake cleanup through verified durable release.
- `2aff45e`: scoped Start receipts preserved across cleanup and replacement.
- `949609b`: versioned private native launch scope from persisted execution.
- `ae919db`: isolated Python 3.12.3 decoder verification on host-a.
- `7d139a8`: owned Start admission serialized with shutdown and fatal closure.
- `9de1fa8`: no-site isolated interpreter startup before protected native guards.
- `14c2923`: authenticated owned Start and Stop submission.
- `f3a2684`: bounded exact-marker correctness validation for the future F2C runner.
- `3e4bd89`: expired never-armed Initialize terminalization without runtime effects.
- `e4e2e95`: separate monotonic request timing validation.
- `f40abd1`: explicit Stop for never-armed Initialize without runtime cleanup.
- `c0a07dd`: bounded metadata-only request journals.
- `e11c3de`: private descriptor-relative artifact storage with a shared byte cap.
- `861a3ad`: bounded collected JSON marker response validation.
- `eb8e572`: bounded streamed marker data-event validation.

Expired, never-armed ordinary Fake Initialize requests now terminalize atomically.
The worker proves absence of execution, grants, ownership and runtime identities
before releasing unused endpoint and binding reservations. No memory release,
cleanup evidence or ledger epoch is invented. Armed uncertainty remains retained.
Expiry events replay through the management SSE stream.

Explicit Stop before Initialize arms also passes root integration. Acceptance
fences generation and retains reservations; the owned worker releases only after
the prior task exits and a separate atomic no-effect proof succeeds. It records
distinct Stop history and SSE events without cleanup evidence or a memory epoch.
Associated runtime cleanup remains unchanged. Armed unassociated work is retained.

F2C request journals now serialize only closed outcome codes, numeric corpus
ordinals and validated monotonic durations. Private artifact storage creates
exclusive mode-0700 run directories and mode-0600 fixed files relative to a trusted
parent descriptor. Writers share an at-most100-MiB payload cap and stop on failure;
partial evidence is retained. Each file requires explicit sync. These helpers do
not validate a run manifest, prove route identity, authorize effects or complete
the API-driven runner.

Collected JSON and decoded streaming data events now have bounded marker checks.
They validate the served model, one choice, exact ordered content and natural stop;
streaming also requires one terminal event. Malformed envelopes and alternative
output fail closed without response text in diagnostics. Streaming checks accept
already-decoded data events, not raw SSE bytes, and reject usage-only events.
A bounded LF/CRLF framing helper now feeds those events across arbitrary network
byte splits, including split UTF-8. It supports comments and multiline data but
rejects other SSE fields, lone CR and incomplete frames. HTTP status/content type,
clean transport completion and binding provenance remain runner obligations.

The F2C phase-margin calculation now uses checked integer arithmetic for
`peak + max(2 GiB, ceil(peak/4))`. It rejects overflow instead of saturating or
wrapping. This numerical helper does not establish attribution, qualify a recipe
or reduce any reservation; missing attribution keeps the conservative grant.
Its pressure-case helper selects the smallest whole-GiB ceiling covering every
supplied intermediate charged demand while denying direct wake. It rejects an
empty or unsafe interval. Actual qualified attribution, complete ledger totals
and planner feasibility remain caller obligations, not numerical assumptions.

Owned Fake candidate Initialize and its mandatory Ready probe pass root integration.
Authenticated run-scoped submission returns the original durable acceptance
envelope. The same retained instance executes each separately armed child; trusted
terminal clocks, policy/session checks and durable leases govern completion.
Caller loss cannot cancel or replay accepted work. Uncertainty retains accounting;
ordinary gates stay closed and no qualification is issued.

Run-scoped candidate inference also passes root integration. Strict authenticated
requests enter a bounded owned queue; expected revision and immutable scope bind
atomically to the original V3 request grant. Only a new grant sends through the
original retained Fake runtime. Caller loss and exact retries cannot replay it;
uncertain outcomes retain leases and the full conservative grant. All four closed
Fake corpus requests complete through HTTP without opening ordinary gates.
Internal Security progression now also passes root integration. After all four
baseline results, the same retained Fake performs the fixed unauthorized control
check and two unauthorized endpoint checks. Each child requires a new durable arm,
fresh observations, exact current authority and final clock/shutdown fences.
Previously armed work never restores send permission; uncertainty retains leases
and the full grant and stops later children. The service control collector samples
its clock after the actual terminal observation. Discovery rejects malformed,
non-text and oversized records before allocating them. No public Security action
is added. Remaining actions and public operation-result reads remain open.
These Fake cases do not replace the native F2C corpus.

Owned candidate Park/Restore now use the original retained Fake through the
authenticated action endpoint. Park separates Drain, Park and read-only parked
status; Restore separates allocation restore, weight reload, cache invalidation
and the required Ready probe. Post-wake inference uses the validated Restore
anchor without replaying baseline Security. Full conservative grants remain
retained and ordinary gates remain closed. Each child requires a new durable arm;
current policy, leases, pressure and final clock/shutdown fences govern sending.
Public operation-result reads remain separate work.

The owned candidate Finish service now uses an explicitly versioned deadline-bound
catalog/receipt record while preserving legacy V3 history. It reuses the exact
complete Fake suite evaluator, atomically records qualification and checks clocks
after evaluation and immediately before commit. Expiry or regression rolls back
all writes. Exact-key retries preserve the original operation; a new service key
after completion conflicts. Current-session completion can validate immutable
older-session evidence without reconstructing a runtime. All candidate accounting
and identities remain retained; ordinary use still requires verified cleanup and
a fresh independent binding. This does not qualify a native recipe.

Candidate Abort now has strict owned submission and atomic versioned history.
It closes an eligible run without releasing grants, leases, endpoints or runtime
identity and without inventing a completion epoch. Run-scoped cancellation and
Store arm/completion fences prevent later work or late success from promoting the
run. Exact retries preserve the original operation; a new key cannot rewrite a
terminal run or passed catalog. The original driver is retained inside a newly
armed Initialize job even if its awaiting future is cancelled.
The durable Abort event replays through authenticated SSE with exact IDs and a
positive string session epoch. Strict projection rejects expanded or corrupt
payloads; exact retries emit no duplicate event and live delivery continues.

Associated candidate Cleanup now passes root integration through the original
retained Fake runtime. The single worker waits for its predecessor future to exit
and sends only after a new durable arm and exact current-context validation.
Frozen cleanup permission and a separate bounded deadline remain usable after
run expiry. Verified exact-membership gone evidence atomically settles leases,
releases ownership/endpoints/binding and advances the completion epoch once.
Unverified outcomes retain accounting; missing association or retained runtime
remains unsupported and never triggers reconstruction. After candidate uncertainty,
normal admission stays closed while an explicit Cleanup-only lane remains live.
Idle discovery is clock-free; acceptance, arm, send and completion retain their
trusted clock fences. Exact receipts and validated missing-result inference history
survive verified Cleanup without fabricated response content. Ordinary routing and
native qualification remain gated; public result capture/reads are still open.

Authenticated Start and Stop HTTP submission now passes root integration
verification. The optional lifecycle router shares the existing owned state,
trusted principal and bounded command capacity. Stop resolves generation in its
acceptance transaction after historical receipt lookup. Exact retries survive
cleanup, replacement and worker shutdown; accepted responses do not claim Ready
or cleanup completion. Narrower routers gain no lifecycle authority. No listener,
native lifecycle support or additional cleanup/recovery path is introduced.

Scoped Start command receipts now pass root integration verification. Exact
retries preserve the original operation, joined value and deadline after Ready,
verified cleanup, replacement and valid session rotation. Historical reads grant
no execution authority; current resource-policy gates still govern new acceptance.
The owned worker now provides bounded command handles. Exact receipt history is
read before current admission flags; fresh commands serialize with shutdown,
Drop, initialization pause and fatal closure. The HTTP adapter maps typed Store
errors to fixed public categories. Retained handles keep the owned state/process lock
for historical reads but cannot restart execution.

The owned Fake cleanup worker passes root integration verification. It retains
the original instance, waits for Initialize to exit,
supports explicit same-session cleanup after associated uncertainty, and sends
only after a new durable cleanup arm. Unverified outcomes retain authority.

## Remaining implementation and verification

1. Complete ordinary warm lifecycle, sequence/preinitialization, no-spawn
   terminalization, missing-association cleanup and restart reconciliation.
   Expiry and explicit Stop for never-armed Initialize are implemented. Other
   no-spawn states and missing-association/restart recovery remain open.
2. Complete owned candidate execution and API actions/inference, durable router
   accounting, management read models/policy/attachments/listener, and CLI cutover.
   Retire legacy authority only at the joint integration gate.
3. Complete guarded native startup composition: explicit child descriptor transfer,
   complete process enrollment, installed-source/device/allocator verification,
   scheduler observer attachment, and protected authentication/control composition.
   The closed native qualification program and both engines' persisted adapters
   also remain required; the current qualification program supports Fake only.
4. Complete the API-driven F2C runner, protected manifest/artifacts, trusted host
   inventory, pressure-abort wiring, correctness corpus and scenario reports.
5. Run the consolidated review, fix required findings, satisfy remaining F1/M1
   gates, and perform authorized pressure-guarded native qualification after its
   prerequisites. CPU/Fake tests and source checks are not native qualification.

## Owner attention

Execution capacity item: after Cleanup committed, fresh-worker creation for the
queued trusted response-capture unit failed with `agent thread limit reached`.
The visible workers are complete and no Cargo process remains. The available
tools expose no worker close/release operation. The current execution skill keeps
capacity-limited work queued and forbids reusing a completed worker for another
unit. Continue from a fresh session with worker capacity, or explicitly direct
inline implementation. No response-capture worker launched or changed source.

Root verification for the associated candidate Cleanup slice: Store 204,
controller 243 and management 67 tests pass (514 distinct core tests). The same
full five-crate run also passes all 88 adapter and 74 harness tests, for 676
distinct tests. Initial integration exposed an unnecessary clock sample during
idle Cleanup discovery. The narrow repair preserves every action clock fence and
the existing two-sample assertion; the complete rerun passes on final code. Two existing
caller-timeout tests failed during the earlier
unarmed Stop worker's concurrent fixture runs, then passed unchanged in isolated
reruns and subsequent bounded full core runs. No timeout or evidence-freshness
limit was changed.
Tests used four threads to bound concurrent fixture load; internal race tests
remain enabled. Separate no-site verification passed 10 renderer, 15 runtime-binding
and 218 Python runtime tests; all16 launch-decoder tests also pass on isolated Spark
Python 3.12.3.
The full root run also passes all 74 harness tests, including five phase-bound/ceiling, six exact-marker,
seven collected JSON, eight streamed-data, five SSE framing, five timing, seven journal and nine
protected-storage tests.
The full Cargo harness count includes the three pressure-ceiling tests.
All-target Clippy also passes for these five crates with warnings denied.
None is native qualification evidence.

One existing item remains for the owner's inspection: check the untracked
`crates/mllm-cli/tests/live_interactive.rs` for formatting from the earlier
workspace-formatter incident. There is no original baseline for that file, so
the agent cannot certify or restore it. It remains excluded from reading,
editing, formatting, tests and staging. The separately modified SDD Task 2 report
also remains excluded and untouched by this continuation.

Owner access item: read-only SSH connections to host-a timed out on port22
at approximately 2026-09-15 11:56, 12:30, 13:26, 14:18, 15:16 and 16:08 UTC. No remote command executed
in any attempt. Access remains unavailable from this session; restore reachability
before native qualification. These observations do not identify the network or
host cause. Local implementation continues without remote effects.

No new approval is required for the current bounded implementation. Only
host-a is authorized. The approved isolated SGLang environment and reviewed
observer patch do not authorize changing existing engine environments, drivers,
rebooting, or accessing host-b. Both native entrypoint denials remain closed;
there has been no model load or native qualification in these slices. Build and
live-effect gates remain explicit rather than inferred from passing CPU tests.

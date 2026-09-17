# F2 continuation status

F2 is not complete. Work continues on `feat/f2-sglang`; no push or final merge is
claimed. The current user instruction is one consolidated review at the end,
not per task. Focused TDD and integration verification continue throughout.

## Recent committed work

- `dc5a3c1`: a success resets the attempt budget.
- `0a0a3ee`: retry a failed deployment three times with a 30 s doubling cooldown,
  added through `CoordinatorOptions`.
- `49f8bde`: the ordinary path's types, methods, operation kind and event kinds
  lose the qualified prefix.
- `0556375`: the Fake engine's lifecycle simulation renamed under its own name
  (`fake/lifecycle.rs`, `FakeFault`).
- `9530811`: retired `qualification_id` — the host YAML key, profile field and
  token, and bindings renamed to `identity_id` and `recipe_fingerprint`.
- `92fdeee`: deleted the store/config/domain qualification modules; schema v13
  drops ten tables and removes the negative identity guards.
- `9c8f68a`: deleted the candidate lanes, management routes and the CLI qualify
  verb.
- `1f13d4f`: a closed deployment offers no work; a planned step still expires.
- `f42e84d`: an ordinary test fixture that never runs a candidate suite
  (`tests/support/fixture.rs`).
- `bf4b042`: `NativeLaunchHandoff` is sourced through a `NativeLaunchSource`
  trait.
- `6b2e073`: moved `ArmResult`, cleanup types, SGLang pins and native launch
  types out of the candidate module.
- `57247ae`: the domain park contract as pure rules (`mllm-domain/src/park.rs`).
- `df19a46`: qualification is not an mllm concept (ADR 0011 decision 2; SPEC
  §8.4 withdrawn).
- `1761f07`: a failed deployment closes its own admission, not the host's.
- `c6915fd`: the A1 gate on the Fake engine — deploy, start, and one inference
  served through the router, with the coordinator as the sole authority.
- `d2a6117`: a start binding is identified by what it is, not by one spelling, so
  a restart-only deployment can be started at all.
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
wrapping. This numerical helper does not establish attribution, verify a recipe
or reduce any reservation; missing attribution keeps the conservative grant.
Its pressure-case helper selects the smallest whole-GiB ceiling covering every
supplied intermediate charged demand while denying direct wake. It rejects an
empty or unsafe interval. Actual verified attribution, complete ledger totals
and planner feasibility remain caller obligations, not numerical assumptions.

The candidate pipeline these paragraphs used to describe in detail — Initialize,
run-scoped inference, Park/Restore, Finish, Abort, Cleanup, and the qualification
ceremony that gated them — is deleted (ADR 0011; Tasks 7–9 of the qualification-
removal plan, commits `9c8f68a`, `92fdeee`). Narrating its internal mechanics here
would describe code that no longer exists; see "Recent committed work" above for
the deletion commits and the A1b entry below for what replaced it.

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
2. Ordinary park (drain, park, parked accounting, wake) is not designed; the
   contract it must satisfy is `mllm-domain/src/park.rs`. The ordinary native
   launch is not designed; `NativeLaunchHandoff` waits on a `NativeLaunchSource`
   implementation, `ProfileBindings` refuses SGLang, and the private descriptor
   tag `sglang_candidate_private_launch` and the served-name rule
   `candidate-{binding_id}` are leftovers of that design. Native parking is
   blocked on both engines regardless: `VllmAdapter` has no `execute_persisted`.

## Milestones and review gates

Work follows [ADR 0009](../design/adr/0009-proof-carrying-reconciliation.md). Review
happens at a milestone boundary, not after each task. Units inside a milestone are
verified by focused TDD plus the core suite and carry no separate review pass.

Each milestone leaves a working system and is independently reversible. The order is
deliberate: the largest deletion is last, because doing it first would restructure the
most intricate logic in the project against tests that have never run in production.

### A1 — Production cutover

The cutover is done on the Fake engine and the gate is met there. A native engine
still cannot be started, for the reason recorded below.

- [x] Engine family to adapter resolution (`mllm-adapters/src/resolve.rs`).
- [x] Proof that recorded processes are gone, from identities rather than a live
      handle, so it survives the restart that destroys handles.
- [x] Engine-generic driver factory (`spawn_resolved`): read the declared engine,
      build an `AdapterSpec`, resolve, prove cleanup with `observed_gone`.
- [x] Wire the coordinator into `roles.rs`; retire the handle map; resolve adapters
      per binding (`23e3f35`, `ffe6af6`, `8996065`).
- [x] Accept an ordinary Start for a restart-only deployment (`d2a6117`). The start
      validator asserted the fake-engine fixture's shape, so every Start was refused
      as corrupt stored data and nothing could run at all.
- [x] Drive an accepted Start to Ready and serve through the router (`c6915fd`).
      Four fixture assumptions blocked it: the observation source reported the
      agent's `system` label rather than the host's declared domains; `AdapterSpec::Fake`
      resolved to a bare `FakeEngine` whose `execute_persisted` answers `Unsupported`;
      the Fake engine recognised an ordinary initialize by a `qualified:` id prefix;
      and the router read routes only from the legacy `route_model_id` column, which
      managed configuration clears.
- [ ] Remove the legacy authorities together, as the A2d plan requires: synthetic
      admission, empty-ledger checks, old reservation writers, router-owned eviction
      and in-memory release guards. Never two authorities at once.

**Gate:** deploy, start and serve one inference through the router with the
coordinator as the sole lifecycle authority, on a real engine.

`crates/mllm-cli/tests/a1_gate.rs` is that gate as one test and it passes **on the
embedded Fake engine**, which is what standalone declares when no live profile is
configured. That is the first end-to-end evidence the project has, and it is not
verification of a native recipe (SPEC §18).

The native half of the gate is still open. The owner confirmed on 2026-09-16 that
**both** engines are required, not one:

- `VllmAdapter` has no `execute_persisted`, so it inherits the trait default and
  answers `Unsupported` to the only call the ordinary lifecycle makes. It needs one
  written: launch from the frozen profile, record process identities, probe
  readiness.
- SGLang has a complete `execute_persisted` (`sglang/adapter.rs`), but
  `ProfileBindings` refuses to build the runtime because nothing resolves its admin
  credential and no trusted observation socket is supplied. Those are two inputs to
  wire, not lifecycle code to author. Where the admin credential comes from is an
  owner decision and is not yet answered.

Both are read from the code, not from an observed run: no native start has been
attempted since the cutover.

Park was originally part of this gate and has moved to A1b. The ordinary lifecycle
has no park at all; the candidate path that formerly had one is deleted (ADR 0011).
Ordinary stop returned with `b52f729`, which split the suspension predicate;
park has not.

The owner confirmed on 2026-09-16 that parking is the product's premise, not an
option: **one model parked while another serves, switching between them
automatically, is the reason the box holds more than one model.** Anything that
reduces eviction to stop-and-restart misses the point of the project.

### A1b — Implement eviction in the authority

Pressure-driven switching is not implemented in the product. `SwitchEngine` is the
only implementation of drain-release-wake, it lives in the router, and production
never constructs it: `mllm-cli/src/roles.rs` wires `WakeJoin` and `auto_activate`
instead. Before the port extraction, `ready_deployments_excluding` had exactly one
caller, `switch.rs`. `request_transition_inner` handles suspension flags and the
preinitialize contract and never looks at another deployment.

So when a request arrives for a deployment while another holds the exclusive pool,
nothing releases the incumbent. The engines can perform the switch — that was
measured on 2026-09-16 — but mllm has no way to ask for it. This is why the F2 exit
gate's warm-switching criterion could only be demonstrated engine-direct.

- [ ] Implement drain, release and wake in the lifecycle authority, taking
      `SwitchEngine`'s semantics as the contract: close admission, bounded drain
      grace, quiescence through the adapter, park or stop by declared tier, and on
      failure reopen the incumbent unsuspended and journal the failed switch.
- [ ] Move the activation join to the authority so simultaneous arrivals collapse to
      one operation, keyed by deployment, revision and generation.
- [ ] Delete `SwitchEngine` and, with it, the two writes the router currently makes
      through the port.
- [x] Give the ordinary stop an intent (`b52f729`). Schema v11 adds `admin_stopped`,
      carrying the operator's intent alone; `suspended` keeps its nine eligibility
      readers untouched. This is what the earlier attempt could not do by writing
      `suspended`, from either side of acceptance.
- [x] Make the park tier declarable and host-validated (ADR 0010). Residency names
      the tier (`restart_only`, `host_backed`, `deep`); a host declares each domain's
      memory topology; a host-backed park is refused at configuration time on a
      one-pool domain; SGLang's startup flags follow the declared tier. This makes
      the choice expressible and checkable. It does not implement park.
- [x] Remove qualification (ADR 0011). mllm guards the host; the user owns the
      recipe. The candidate and qualification subsystem is deleted, schema v13
      drops its tables, the park contract survives as pure domain rules, a
      failed deployment closes its own admission and is retried three times
      with a doubling cooldown before it is given up on, within the start
      command's deadline, and an uncertain attempt still resolves through the
      gone-proof first — an explicit Stop drives that cleanup and the Start
      that follows is a new generation with a fresh budget. CPU and Fake tests
      are not verification of any native recipe.
- [ ] Implement ordinary park. The ordinary lifecycle has no park at all; the
      candidate path that formerly had one is deleted. This is the premise of
      the product and the largest remaining piece of A1b.

Ported faithfully first, keeping the existing T16 and T19 tests as the contract. The
semantics were written against F1's assumptions and deserve revisiting against the
proof-carrying model, but that belongs in A3 rather than here, where it would rewrite
the tests that define correct behaviour.

**Gate:** a request for a deployment whose pool is held by another causes the
authority to release the incumbent and serve the request, with no router involvement
beyond asking.

### A2 — Extract the domain

`mllm-store` is larger than the controller, management, adapters, router and
scheduler combined because workflow logic followed the transaction into it.

- [ ] Create `mllm-domain` as a pure crate: planner, policies, resource algebra,
      proof rules. No async runtime, no clock, no engine knowledge.
- [ ] Move rules out of `lifecycle`, `progression`, `initialize`, `security`, `warm`
      and `cleanup` with no behaviour change. The store keeps its tables.

**Gate:** `mllm-domain` compiles without an async runtime and its tests run with no
database and no network. A rule that cannot be tested that way is in the wrong layer.

### A3 — Capabilities and proofs as data

- [ ] Engines declare the actions they perform and the facts they can prove instead
      of failing when called.
- [ ] The domain permits a transition when its required proofs are a subset of what
      the installation proves.
- [ ] vLLM reaches restart-only parking by mechanism rather than by special case,
      which is the fallback `SPEC.md` §6.2 already describes.

**Gate:** adding an engine family requires publishing two sets and no coordinator
edit. The proof set gates the commit, not the call.

### A4 — Collapse the second lifecycle

Discharged by deletion (ADR 0011).

## Tracked for later: naming and engine resolution

[ADR 0008](../design/adr/0008-engine-installations-and-runtime-types.md) makes
"engine installation" the term of record, but internal type names still say runtime
profile. Rename `RuntimeProfile` and its configuration key, and keep one name per
concept on every new surface in the meantime. The mockups additionally use "runtime"
for three different things — start mechanism, Python version and CUDA version — and
only the first is the runtime type; the others are build metadata that
`build_fingerprint` already covers.

Add the engine-family to adapter resolution layer. Adapters are currently selected
at hardcoded construction sites, which is what blocks both a third engine family and
the construction of `SglangAdapter` on the runtime-binding path.

Two mockup behaviours conflict with the spec and should not be implemented as drawn.
Raw engine flags include `--served-model-name`, which `engine_policy.rs` reserves,
and the interface warns that a raw flag overrides a structured setting; T14 requires
conflicts to fail with provenance instead. The CLI grammar is also resource-first
(`mllm hosts list`), where R11 requires action-first (`mllm list hosts`).

## Tracked for later: multi-node and parallelism beyond TP=1

Not in F2 scope. The pinned recipe is TP=1, DP=1 on one device, and `SPEC.md` §11
plus the F4 slice own multi-node. Recorded here so the constraints are not
rediscovered later. Applies to tensor, pipeline, data and expert parallelism, on one
host with several devices or across hosts.

1. The identity model admits exactly two processes.
   `crates/mllm-store/src/lifecycle/completion.rs:49` sorts identities and requires
   `ids[0].role == "api"` and `ids[1].role == "worker-0"`, so any third rank is
   rejected. The SGLang adapter compares whole sets and is already arity-agnostic.
2. Configuration carries `tensor_parallel_size` and `pipeline_parallel_size` but no
   data or expert parallel sizes, and validates only that they are non-zero. Nothing
   ties a parallel size to the number of claimed devices or hosts, so an infeasible
   plan is accepted at configuration time and fails at launch. `SPEC.md` T27 implies
   that cross-check.
3. There is no engine-group or member model in `mllm-domain`, although `SPEC.md` §2
   defines an engine group as the complete runtime realization of one deployment
   across processes and hosts, and §11 requires a group launch plan carrying member
   identities, rank roles, peer addresses and rendezvous data.
4. Multi-rank release and resume acknowledgement is unverified. Per the F2B plan,
   SGLang's release and resume await their communicators, and a success reply from
   the tokenizer manager does not prove every rank released. A partial release that
   reads as success would be exactly the unevidenced release `SPEC.md` §6.1 forbids.
   The 2026-09-16 verification proved the single-rank path only.
5. `NativeResidencyObserver` now fails closed when allocations span more than one
   device, because summing mapped bytes across devices cannot distinguish a fully
   restored group from one restored rank. Per-rank evidence, and cross-host
   aggregation for a multi-node group, remain unimplemented.
6. Hardware: host-a has a single GB10, so no parallel topology can be verified
   there. Tensor parallelism needs a multi-device host; multi-node needs two hosts.

## Open questions

1. A caller waits the full 600s bound to learn an operation is uncertain.
   When `drive` fails after arming, the worker marks the lifecycle run `uncertain`
   and pauses, holding the retained binding until an explicit Stop and a verified
   cleanup. That is the proof-carrying model working as intended. But the
   `operations` row stays `running`, and `CoordinatorLifecycle::classify` reads only
   that row, so `wait_terminal` polls for the whole `TERMINAL_WAIT` (600s) before
   reporting `Uncertain`. Observed on 2026-09-16: a regression run took 600.15s to
   fail. The run state is durable and says `uncertain` immediately, so the caller
   could be told at once. Changing it alters what the router does with a request
   that triggers activation — it would fail fast rather than hold the client — so it
   is the owner's call, not a repair to make unattended.

2. Stopping a deployment that was never started reports a conflict.
   `accept_ordinary_cleanup_in_transaction` finds no unreleased runtime binding and
   returns `LifecycleError::Conflict`, which reaches the caller as
   `LifecycleFault::Conflict` — "your view is stale, re-read and retry". Re-reading
   will not help: nothing was ever started, so this is an illegal transition and the
   honest answer is a refusal. `LifecycleError` has no variant for that today;
   `Disabled` is the closest and means something else. Pinned by
   `standalone_lifecycle::stop_is_illegal_from_stopped` so a change is deliberate.

3. A resource policy written before ADR 0010 cannot be read back.
   `StoredPolicy.version` stayed at `1` when `StoredDomain` gained a required
   `memory` field, so a policy row written before that change now fails to decode as
   `CorruptStoredPolicy`, and `import_resource_policy` does not overwrite it — it
   reads the existing row first and propagates the error. Failing closed is correct:
   the old row genuinely lacks the topology fact and ADR 0010 forbids inferring it.
   The defect is the diagnosis, which says "corrupt" for what is merely a superseded
   shape. No persisted policy exists on this machine. **If standalone refuses to boot
   against a state directory created before 2026-09-16, delete the directory** — the
   host policy is republished at every boot.

4. Retry cooldown sleeps inside the single worker loop. When a deployment fails
   and is waiting out its doubling cooldown before the next attempt, the worker
   sleeps in place, which blocks it from discovering and advancing any other
   deployment for up to 30 s per wait. This is accepted for A1b standalone,
   where one worker and one deployment are the common case, but it will not
   scale past that. The fix shape is a not-before time read from
   `deployment_attempts.last_attempt_ms` and checked at poll time instead of a
   blocking sleep, so the worker keeps discovering other deployments while one
   waits out its cooldown.

5. The give-up reason is recorded in the journal but not in the deployment's
   own state. Every counted attempt and the give-up itself now write a
   `journal_entries` row naming the deployment and the reason, in the same
   owned transaction that counts the attempt or closes the admission, and the
   observer reports a planned step of a closed deployment as `Closed` rather
   than `Superseded`. What is still missing is a failure category or
   last-error text on the deployment row itself, so a caller reading only
   `deployments` still cannot tell a budget exhaustion from any other reason
   admission might be closed.

6. Stored kind strings of the ordinary path were renamed in the same change
   without a data migration; a v12 state directory that holds lifecycle
   history is recreated, as the design keeps no compatibility. `operations.kind`
   went from `qualified_initialize` to `initialize`, the owned-launch
   association tag from `qualified_owned_launch` to `owned_launch`, the
   management event kinds from `qualified_*` to `initialize_*`, and the plan's
   own tag with them. Schema v13 drops tables and rewrites none of these, so a
   pre-v13 directory carrying lifecycle rows fails to decode rather than
   upgrading. **Delete such a directory**; the host policy is republished at
   every boot.

7. Fingerprint drift between an effective configuration snapshot and the
   current host is not checked at deployment start. The refusals that used to
   catch a stale or mismatched recipe came from the deleted qualification
   catalog and judged the recipe, not host capacity; nothing replaced that
   check when the catalog was removed, so a start can proceed against a
   snapshot that no longer matches the host it targets.

7. The dispatch seam — grant, close, finish and pending dispatch, and
   `request_leases` — has no production issuer. Router dispatch ownership (the
   F2A2c plan) is the intended one and has not landed. Ordinary tests use the
   seam directly today to exercise the request-lease guards, which is useful
   coverage but not evidence that anything in production calls it.

8. Two SGLang wire kinds, `sglang_candidate_launch` and
   `sglang_candidate_private_launch`, and the served-name rule
   `candidate-{binding_id}`, stay in place until the ordinary native launch is
   designed. They are leftovers of the deleted candidate path's launch
   plumbing, not a naming choice for the ordinary path.

9. `crates/mllm-controller/src/sequence.rs`'s planner still keeps a
   `qualified_park`/`qualified_restore`/`qualified_initialize` eligibility
   vocabulary inherited from the legacy F1 `Controller` lineage. This plan did
   not touch it; renaming it is work for the park ADR that wires ordinary park
   against the `mllm-domain/src/park.rs` contract. The legacy
   `crates/mllm-controller/src/operations.rs` `Controller` itself is untouched
   by this plan and remains slated for retirement at the A2d gate, per the
   milestones section above.

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
None is native verification evidence.

One existing item remains for the owner's inspection: check the untracked
`crates/mllm-cli/tests/live_interactive.rs` for formatting from the earlier
workspace-formatter incident. There is no original baseline for that file, so
the agent cannot certify or restore it. It remains excluded from reading,
editing, formatting, tests and staging. The separately modified SDD Task 2 report
also remains excluded and untouched by this continuation.

Host access item: RESOLVED. The 2026-09-15 SSH timeouts no longer reproduce.
A read-only check on 2026-09-16 connected successfully; `host-a.tailnet.ts.net`
resolves to `100.64.0.10` over Tailscale and port 22 is open.

Kernel and GPU driver item: OPEN, owner action required. host-a rebooted at
2026-09-15 22:53 into kernel `7.0.0-1019-nvidia`, which has no GPU driver module.
`modprobe -n -v nvidia` reports `FATAL: Module nvidia not found in directory
/lib/modules/7.0.0-1019-nvidia`. No nvidia modules are loaded and no `/dev/nvidia*`
nodes exist, so `nvidia-smi` fails. The previously booted kernel
`6.17.0-1031-nvidia` still carries the complete stack: `nvidia.ko`, `nvidia-uvm.ko`,
`nvidia-drm.ko`, `nvidia-modeset.ko` and `nvidia-peermem.ko`. Driver packages
`nvidia-driver-580-open 580.173.02` remain installed. `GRUB_DEFAULT=0` selects the
newest kernel, so the upgrade silently changed the boot target.

Two owner options. Booting `6.17.0-1031-nvidia` and pinning it is the faster and
more reversible one; building the 580-open driver for `7.0.0-1019-nvidia` through
DKMS is the forward fix. Either way, pin the boot entry so a future kernel upgrade
cannot silently remove GPU access again. The agent did not change drivers, modules,
boot configuration or power state; all checks were read-only.

No new approval is required for the current bounded implementation. Only
host-a is authorized. The approved isolated SGLang environment and reviewed
observer patch do not authorize changing existing engine environments, drivers,
rebooting, or accessing host-b. Both native entrypoint denials remain closed;
there has been no model load or native verification in these slices. Build and
live-effect gates remain explicit rather than inferred from passing CPU tests.

# ADR 0015 — Per-instance concurrent lifecycle work in the coordinator

**Status:** Accepted (owner decision 2026-09-23, from the live M16 findings: "lifecycle
work becomes per-deployment concurrent").
**Amends:** nothing in `SPEC.md`. It changes how the coordinator schedules work, not the
state machine of SPEC §6.1, the admission rules of SPEC §7 and ADR 0007, or the recovery
rules of SPEC §13.2 and ADR 0011.
**Related:** ADR 0011 (the state machine owns recovery; decision 4 scopes failure to the
deployment), ADR 0013 (instances are the unit of runtime). It makes runbook limitation 4
("Retry cooldown sleeps inside the single worker loop") obsolete.

## Context

The live M16 run on 2026-09-23 found two things about the coordinator worker
(`crates/mllm-controller/src/coordinator/worker.rs`):

- It ran **one loop** for every host and deployment. Each pass discovered one item and
  drove it inline: an Initialize was awaited until it reached Ready or its deadline, and
  so were a cleanup, a park, a restore and a paused launch's settlement.
- A slow or dead activation on one host therefore held **every** other Stop and Start,
  on every host, for up to the Initialize deadline (15 minutes in M16). That caused
  cleanup-timing failures and five `Endpoint capacity is unavailable` rejections.

The same loop slept in place through a failed start's retry cooldown (30 s doubling),
which runbook limitation 4 already recorded as not scaling past one deployment.

The owner decided the same day: each deployment, and each instance, progresses
independently; waiting on a load never blocks another deployment; admission,
reservations, placement and the ledger stay serialized through store IMMEDIATE
transactions; per-deployment ordering is preserved (one active lifecycle step per
instance, as the lifecycle claims already enforce).

## Decision

### 1. A scheduler discovers; tasks wait

`crates/mllm-controller/src/coordinator/scheduler.rs` replaces the loop. It is the only
discoverer. Each pass it:

1. completes unarmed Stops (store only);
2. reconciles instances (placement, revision stops, retirements; store only);
3. re-issues expired drain Stops, only while no cleanup task is in flight;
4. hands every discoverable cleanup to its own task;
5. hands every due settlement of a paused remote launch to its own task;
6. unless a launch is paused uncertain: hands every planned park or restore to its own
   task, advances preinitialize and the idle policy (store only), and hands every
   discoverable Initialize to its own task, expiring planned starts past their deadline on
   the way;
7. waits for a task to finish, a command's wake, shutdown or the poll interval.

Each task runs the unchanged step code — `drive`, `drive_cleanup`, the residency drive,
`settle_failed_launch` — and returns an outcome: done, hold this start until an instant,
pause on this uncertain binding, this binding is settled, ask this paused host again
later, or halt the worker.

### 2. The lane is the instance

A task runs on the lane `(deployment_id, instance_index)` of the instance its binding
realizes. While a lane has a task in flight, discovery skips every step on it
(`mllm_store::ordinary_lifecycle::lanes::BusyLanes`): Initialize, expiry, ordinary
cleanup and unarmed Stop discovery all take the busy set and never return, expire or
complete a step behind the task that owns it. That keeps the ordering the single loop
gave each instance — a Stop accepted while its instance's Initialize is in flight runs
only after that Initialize's task has exited and classified its own outcome — while
every other instance moves on.

As first accepted, one exception narrowed the lane: instances of one deployment
launched on one host one at a time, because the host agent keyed its inference ingress
by deployment and member, so a second concurrent Initialize displaced the first's gate
before it proved ready. **Amended 2026-09-23:** the host ingress
(`crates/mllm-agent/src/ingress.rs`, `register`) is now keyed by deployment, instance and
member, so two instances of one deployment on one host hold separate gates and a later
instance never displaces a Ready sibling's entry. The rule is removed. Starts on one host
are now ordered only by the per-host activation gate of the startup memory budget
(below), which applies to every deployment alike.

Discovery was `LIMIT 1` oldest-first. With a busy set, the oldest step on a busy lane or
a held start no longer hides every step queued behind it.

### 3. Waiting is never a sleep of the worker

- A failed start inside its budget returns a **hold** on its binding until the cooldown
  ends. Only Initialize discovery honours holds: the held start can still be stopped
  (its unarmed Stop completes at once) and still reaches its deadline through expiry.
- A start deferred while parked instances are reclaimed (SPEC §6.5) is held for one poll
  interval, as before, without holding anything else.

### 4. Bounded concurrency

`CoordinatorOptions::max_concurrent_effects` (default 8, 1 to 256) bounds activation
tasks (Initialize, park, restore) across all instances. Cleanups have their own pool of
the same size, so a Stop never waits for an activation slot. Settlements of paused
launches are bounded by the number of paused launches. A start past the bound stays
planned and is discovered when a slot frees; nothing is refused for lack of a slot.

## Invariants

1. **One effect per instance.** No two tasks are ever in flight for one lane, and the
   scheduler never arms, completes, expires or supersedes a step whose lane is busy.
   (Amended 2026-09-23: the former "one Initialize per (deployment, host)" rule is
   gone with the per-instance host ingress.)
2. **The step state machine is unchanged.** Arm, send, record, complete, expire, mark
   uncertain and settle are the same store transactions with the same checks. Every
   failure classification (ADR 0011 decision 5, SPEC §13.2) is the single loop's code,
   moved into the task.
3. **Exact accounting stays in the store.** Every placement, arm, reservation, release
   and epoch advance is one IMMEDIATE transaction under the owner mutex. Tasks interleave
   only between transactions. Two starts racing for one host's capacity both reach the
   arm; the store admits exactly one and refuses the other with nothing charged.
4. **Stops never wait behind loads.** A cleanup waits only for its own instance's task.
5. **Uncertainty keeps its scope.** An uncertain launch still pauses every new
   activation, as before, until that exact binding has verified cleanup or settlement;
   cleanups and settlements continue meanwhile. Several launches may now be paused at
   once; start admission reopens only when none is. A settlement that releases a launch
   lifts its own pause inside its release transaction, and never re-admits starts while
   another launch is still paused.
6. **No effect outlives the worker.** Shutdown, a halt or closed admission sends every
   task its stop signal and joins all of them before the worker returns and releases the
   process lock. A task stopped mid-effect records uncertainty exactly as the single loop
   did on shutdown, and a remote launch is left for the restarted worker to adopt.
7. **Restart adoption is unchanged.** Remote launch adoption, embedded launch adoption
   and retired-cleanup adoption run to completion before the scheduler's first pass.

## Consequences

- One slow or dead activation now costs only its own instance. Other deployments start,
  stop, park and wake meanwhile, on the same or another host.
- A retry cooldown no longer delays anything but its own start.
- **A halt is no longer worker-wide for an unproven cleanup.** (Amended 2026-09-24
  from review findings.) A cleanup that cannot be classified — its host dropped while
  its armed Stop was on the wire, the send timed out, or its runtime is not retained by
  this worker — no longer ends the worker. Its task returns `CleanupPaused`: the binding
  stays uncertain with its reservation charged (SPEC §6.1, ADR 0011 decision 4), a pause
  is recorded for that binding, and only that instance's cleanup discovery backs off for
  the retry cooldown. Other instances' in-flight effects continue. What still halts the
  worker is a condition of the process, not of a host: a task panic, closed admission
  (a store fault, a poisoned mutex, a binding mismatch the store refused), a planned
  cleanup that names no instance, and shutdown.
- **An unproven cleanup is retried in the same session.** When its backoff ends, the
  scheduler asks the store to return the armed step to `planned`
  (`Store::replan_unproven_cleanup`), keeping the binding, claim, reservation and
  recorded identities exactly and journaling `cleanup_retried`. The ordinary cleanup
  path then re-arms it once its host is online (owner decision 4) and sends one fresh,
  fenced Terminate to the same recorded identities; completion still needs gone evidence,
  and the settlement lifts the binding's pause. Past its accepted deadline the step is
  not re-planned and stays uncertain and charged. Before this, only a restarted
  coordinator's adoption could resume it.
- **Pauses are recorded per binding; admission is still closed while any exists**
  (invariant 5). Each uncertain launch or unproven cleanup holds its own pause and
  lifts only its own on settlement, but new activations stay refused while any pause
  remains. Scoping that refusal to a host or a deployment is possible because the
  reservation stays charged, but it is a policy change this ADR does not make.
- **Two loads on one host can now overlap** when the ledger admits both. That is what
  ADR 0007 already permits; the startup memory budget the owner decided on 2026-09-23
  (reserve the startup peak until Ready, serialize launches on one host through their
  startup phase when their peaks do not fit together) is a separate unit that builds on
  this scheduler: it belongs in the arm transaction and in the scheduler's per-host
  dispatch, not in a return to a single loop.
- Tests that relied on the loop's incidental serialization were adjusted: two clock
  fakes that recognised "outside the store" by an uncontended owner lock now recognise
  the runtime thread instead, one queue-failure test runs on two runtime threads because
  the scheduler keeps reading the store while an Initialize is in flight, and one test
  now waits for an unarmed Stop that no longer completes before an unrelated start.

## Amendment 2026-09-23 — startup memory budget and per-host activation gate

Owner decision 2026-09-23. A deployment's startup peak is `engine_config.memory.startup`
when declared, else the peak a first uncontended run on the host measured (the largest
drop in the host's published availability while its Initialize ran, kept per revision,
host and engine installation), else the placeholder `max(request, weights × 1.6 +
margin)`. Admission reserves it as the cold phase (ADR 0007) from arm until Ready, where
the reservation drops to the steady request. Placement judges an instance's peak against
the steady footprint of every launch still starting. In `Scheduler::pass`, a planned start
whose peak does not fit beside the current reservations and the peaks of the starts in
flight on its host, but would once they are Ready, waits planned (a hold, not a refusal)
until one of them reaches Ready; a start that would not fit even then goes to its arm,
which refuses it as before. Store: `ordinary_lifecycle/startup.rs`; scheduler:
`coordinator/scheduler.rs`; tests: `coordinator/tests_startup.rs`. CPU and Fake-engine
tests only; no startup peak of any model has been measured by this code live.

## Implementation notes for later units

- The W10 switching unit can express "stop A, then start B on the same host" as today:
  placement and the arm already sequence it through the ledger, and a scheduler-level
  host rule (activations on a host wait for that host's in-flight cleanups) can be added
  in `Scheduler::pass` without touching the tasks.
- The startup-budget unit can add a per-host activation gate beside
  `max_concurrent_effects`; the lane and hold machinery already let a start wait
  without blocking the worker.

## Verification

CPU and Fake-engine tests only; this is not qualification of any engine recipe, and the
behaviour is not yet live-verified. Coverage in
`crates/mllm-controller/src/coordinator/tests_concurrency.rs`: a hung Initialize does
not hold another deployment's start or stop and then pauses alone at its deadline; a
Stop completes while another deployment loads, which then reaches Ready; two starts
racing for one host's capacity admit exactly one with an exact ledger; a retry cooldown
holds only its own start; shutdown with two Initializes in flight joins both and a
restarted worker resends neither; the activation bound queues starts past it and is
validated. Added 2026-09-24: a dropped host mid-Stop pauses only its binding while
another load reaches Ready; an unproven cleanup is re-planned in session, re-armed
once, completes on gone evidence and lifts its pause
(`an_unproven_cleanup_is_retried_in_session_and_lifts_its_pause`; store tests
`an_unproven_cleanup_is_retried_in_its_own_session` and
`an_unproven_cleanup_past_its_deadline_is_not_retried`).

## Amendment 2026-09-23 — solo first start, `--evict`, switch closure reasons

Owner decisions 2026-09-23, building on the startup budget and W10:

- **Solo first start.** An unmeasured model whose placeholder startup estimate exceeds its
  host's managed limit, while its steady request fits, may start only when no other
  engine holds a charge on that host. Its plan freezes the whole managed limit as the
  cold phase (`startup_bytes`, `whole_host`; provenance `whole_host` in status) until
  Ready. The run is uncontended by construction, so its peak is recorded, and later
  starts reserve the measured peak. Beside another charge, placement refuses the host
  `startup_requires_empty_host`, and a start command gets that typed refusal (HTTP 409).
  Request-driven activation treats it like a capacity refusal and empties the host by the
  W10 rules; an explicit start does so only with `--evict`. Store:
  `ordinary_lifecycle/startup.rs`, `placement.rs`; scheduler: `placement.rs`.
- **Closure reasons.** A dispatch gate records why it is closed (`switch`,
  `host_session`, `engine_exit`) per instance incarnation (`dispatch_closures`, store
  schema v28). A failed switch removes only its own reason and reopens the gate only when
  none remains, so a host-loss, unresponsive-host or engine-exit closure made during the
  drain window stays closed until its own evidence clears it; a passing host probe
  likewise leaves a gate a switch holds closed. A new coordinator session drops switch
  reasons with the switches.
- **Status and configuration.** The switch in progress (target, host, victims, phase) is
  mirrored in `active_switches` and shown on the target's and each victim's deployment.
  The switch drain bound is `switching.drain_timeout` in the server document (and a
  standalone document's `server:` block), 30 s by default, 1 s to 600 s. The router's
  waiting-request bounds come from the hosts' published `resource_policy.queue`, the
  tightest across hosts, re-read every 5 s on the server.

CPU and Fake-engine tests only; not live-verified.

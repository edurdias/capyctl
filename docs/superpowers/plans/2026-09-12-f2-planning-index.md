# F2 implementation planning index

The [F2 spec](../../design/milestones/f2-sglang-design.md) is approved. The owner chose
to finish the implementation plans before starting production implementation.
This index tracks that planning work; it is not an implementation or qualification claim.

## Plan coverage

| Slice | Planning status | Scope |
|---|---|---|
| [F2A1](2026-09-12-f2a1-resource-contracts-and-admission.md) | Implemented; task and whole-branch review complete | Physical domains, phase footprints, sharing, forecasts, admission |
| [F2A2a](2026-09-12-f2a2a-durable-reservation-transactions.md) | Implemented; task and whole-branch review complete | Durable resource increases, epochs, revision/generation fences |
| [F2A2b](2026-09-12-f2a2b-runtime-evidence.md) | Implemented; task and whole-branch review complete | Host observations, qualified completion tokens and process identities |
| [F2A2c](2026-09-12-f2a2c-durable-dispatch-ownership.md) | Written; examples checked | Coordinator sessions, durable request leases, atomic dispatch closure |
| [F2A2d coordinator integration](2026-09-12-f2a2d-coordinator-integration.md) | Written; integrated review corrected | Runtime/endpoint ownership, lifecycle steps, verified reductions, preparation, recovery, router cutover |
| [F2A3 management/configuration](2026-09-12-f2a3-management-and-configuration.md) | Written; integrated review corrected | Strict effective schemas, authenticated API/CLI, revisions, snapshots/events, attachment |
| [F2B SGLang](2026-09-12-f2b-sglang-adapter.md) | Written; integrated review corrected | Pinned adapter, readiness, qualified parking/restoration, cache/security contracts |
| [F2C mixed-engine qualification](2026-09-12-f2c-mixed-engine-qualification.md) | Written; integrated review corrected | Numerical guardrails, selected recipes, repetitions, concurrency, warm switching and failures on host-a |

The subdivision of F2A2 is for independently reviewable code and tests. It does not
change the approved three-stage milestone or defer any required single-host capability.
Examples checked in temporary workspaces do not mean those examples exist in product code.

## Coordinator integration review checklist

The coordinator draft covers the following sequence. Review it with the remaining
management, adapter, and qualification plans before execution. This checklist does
not replace the implementation tasks in that document.

| Boundary | Durable action | Work outside the transaction | Crash/uncertainty rule |
|---|---|---|---|
| Controller startup | Begin a new fenced session; close dispatch; retain all ownership | First acquire lifetime single-controller ownership, then inspect surviving runtimes | A DB fence cannot stop an old process from sending engine controls |
| Accept or join activation | Persist one active operation for the target revision/generation | Bounded callers observe the same operation | Client exit does not cancel accepted work |
| Plan a transition | Claim all affected deployment lifecycles and freeze effective recipes/sequence | Forecast each cold/parking/wake peak against current observations | No claim means no permission to perform the transition |
| Reserve and arm a step | Commit resource increase, unique step identity, and dispatch intent together | Send the engine command at most once from the owning task | A possibly dispatched step becomes uncertain; no blind replay |
| Complete parking | Revalidate current session/token/qualification/identities; commit retained footprint and evidence | Collect qualified drain/release evidence | Until this commit, keep the peak and the endpoint allocation |
| Complete initialization/wake | Commit verified serving footprint and readiness after all completion milestones | Restore, reset required caches, and run the qualified model probe | No HTTP-liveness-only readiness; no stale completion reopening dispatch |
| Admit inference | Atomically register a request lease against the current open generation | Forward once through the immutable deployment binding | Guard destruction or transport closure does not settle work |
| Stop/undeploy/recovery | Record explicit authority and close dispatch; release only after verified cleanup | Operate only on verified owned processes | Failure, timeout, or PID reuse does not free reservations or ports |

### Interface changes the integration plan must resolve

- F2A2a's `reserve_increase` currently owns its transaction. Refactor an internal
  transaction-scoped helper so resource grant, session/operation checks, lifecycle
  claims, and step intent commit together. Calling separate transactions in sequence
  is not the required atomic boundary.
- Carry `CoordinatorSession` through resource-changing writes and completion commits,
  not only dispatch. Fence obsolete callbacks in the same transaction as each write.
- Construct F2A2b `CompletionExpectation` from the persisted step and qualified recipe,
  not from a request payload. Recheck its expiry and all identity fields at commit.
- Keep `admission_enabled`/route availability distinct from `dispatch_enabled`.
  Parking must leave routes discoverable and eligible to queue according to policy.
- Preserve request leases from all generations during drain. A newer lifecycle
  generation does not erase outstanding work from the preceding ready generation.
- Per-deployment runtime bindings must own ports, credentials, process/worker identities,
  engine/profile/checkpoint revisions, and forwarders. A profile-kind lookup cannot
  safely distinguish two deployments using the same engine.
- Remove production access to the synthetic admission, empty-ledger checks, old
  reservation writers, router-owned eviction logic, and old in-memory release guards
  together at cutover. Do not enable two scheduling/accounting authorities.

### Required runtime scenarios

- Two ready deployments continue serving when all budgets and sharing policies fit.
- When B's peak does not fit, evaluate A's parking peak before closing A. Commit A's
  verified parked residue before reserving B's peak; recheck observations at that point.
- Preinitialize only the requested set, reporting completed members and partial failures.
  Both parked is usable only if each member has a feasible later wake sequence.
- Simultaneous routed wake and administrative start join one durable activation.
- Opposing switches cannot claim the same deployments or double-spend capacity.
- Non-resetting admission windows bound new admissions under contention; they do not
  promise that an existing stream ends before another request's deadline or authorize a kill.
- Lost acknowledgements, task cancellation, controller restart, and PID reuse retain
  uncertain ownership and close dispatch until qualified reconciliation or authorized cleanup.
- Streaming delivery never silently drops chunks and then claims successful completion.
  Backend completion and downstream delivery success are separate outcomes.

## Before implementation starts

All eight plans exist. Five local document reviewers completed the integrated
review; eight merged fixes landed, including physical-credit accounting and scoped
first qualification. Extracted examples pass 69 tests and Clippy; these are not
production integration tests. Cross-model review skipped the full bundle because
it exceeded the peer tool's size cap; no independent peer result is claimed.
The owner authorized subsequent task-by-task implementation while away overnight.
Record consequential open questions for morning review; do not invent permission
for hardware changes or unsafe qualification. The CPU-only admission kernel is
complete, as are durable reservation transactions and runtime-evidence helpers.
Durable dispatch comes next. Full checkpoint `60fd33a` passes 194 CPU tests and
scoped Clippy; formatting fix `0f5a576` passes focused checks. No live gate is closed.

Only host-a is in scope for future live qualification. Do not access host-b.

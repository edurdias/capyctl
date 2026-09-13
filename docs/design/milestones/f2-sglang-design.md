# F2 — Shared Single-Host Foundation and SGLang: Design

**Status:** Approved by the owner after document review, September 12, 2026
(America/New_York). Implementation planning may proceed; execution is a separate step.
**Baseline:** `0efd718`, following the F1 mainline closeout.
**Sources:** [product specification](../../SPEC.md),
[F1 design](f1-vllm-path-design.md), [F1 carryovers](f1-open-items.md), and
[live qualification record](../../runbooks/spark-model-size-qualification.md).

## 1. Outcome and scope

F2 delivers the single-host mllm contract for both vLLM and SGLang through one
controller, scheduler, management API, and inference router. Different models can
use different engine-specific recipes on the same hardware. The same checkpoint
can also have separate deployments and route names for different recipes.

When capacity allows, both engines remain ready and serve overlapping requests on
the same GPU. When their active footprints cannot coexist, mllm retains initialized
runtimes and sequences parking and waking. Both deployments may be parked at once.
Ordinary switching must not require a cold process restart.

Warm residency preserves initialized runtime processes, not necessarily weight
contents or KV contents. Deep-park wake may reread checkpoint weights. Retaining a
host weight backup is a different, explicitly accounted policy, not an implicit
promise of this milestone. No wake-time or throughput improvement is guaranteed.

Parking and restoration must be qualified for at least one selected recipe for
each engine to close F2. Restart-only remains a supported policy and an intermediate
integration baseline, but it cannot satisfy the warm-residency exit gate.

Three stages, in dependency order:

1. Complete shared single-host foundations and relevant vLLM carryovers.
2. Add SGLang through those contracts, including qualified parking and restoration.
3. Qualify mixed-engine concurrent serving and warm switching through the product
   API and CLI on host-a.

"Full" means the single-host platform contracts below, not every upstream engine
endpoint or tuning combination. Supported combinations require evidence; unavailable
ones must be reported explicitly. Remote enrollment and multi-host recovery remain
F3; distributed recipes and advanced private/shared external-cache qualification
remain F4; broader launchers, packaging, and selected inference API expansion remain
F5. A graphical UI is later work, with no delivery milestone assigned here; the API
it will consume is part of F2. Native private KV budgeting and invalidation during
parking are part of F2, not deferred cache integrations.

Only host-a is authorized for subsequent live work. This design does not
authorize access to host-b or hardware/software changes during document review.

## 2. Baseline gaps and scope reconciliation

F1's historical closure accepts bounded evidence; it does not establish complete
implementation of its design. The following are F2 requirements, not claims that
the existing code already satisfies them.

| Baseline evidence | F2 requirement |
|---|---|
| `mllm-controller/src/operations.rs`: synthetic activation estimate and admission against an empty ledger | Real observations, complete retained-owner ledger, phase budgets, atomic reservation transitions, verified release |
| `mllm-router/src/switch.rs`: other ready deployments are treated as exclusive pool occupants | Fit-based coexistence and one coordinated transition planner |
| `mllm-cli/src/roles.rs`: single adapter wiring; router forwarders selected by profile kind | Deployment-bound runtimes, private endpoints, credentials, recipes, and forwarding |
| `mllm-router/src/chat.rs`: routed activation calls the controller separately from the switch coordinator | Administrative and request-driven activation share coordination, generation checks, and admission gates |
| F1 carryovers: CLI dispatch, credentials, log bounds, argument validation, failed-state cleanup | Normal management surface and safe lifecycle/launch behavior |
| `mllm-config/src/schema.rs`: several required configuration blocks only accept empty objects | Strict usable profile, deployment, resource, and policy schemas with effective-value inspection |
| F1 evidence: reproduced 4B workload passed 32/32 exact routed responses | Preserve that regression; add post-restore streaming and mixed-engine proof rather than extrapolating it |

Paths in this table are relative to `crates/`. Per-deployment port allocation moves
from the historical F4 carryover into F2 because parked and concurrent processes
need distinct endpoints. Exclusive device assignment remains supported, but is not
the sole residency mode. The original exclusive-pool example in SPEC §16 is one
policy example, not a prohibition on explicitly shared assignments.

These owner-approved F2 refinements tighten the earlier documents: SPEC §9.2's
restart-only support is not sufficient for F2 closure; warm residency prohibits the
generic idle-stop fallback unless explicitly relinquished; and F1's historical
unknown-work drain allowance does not establish safe parking under this contract.
These refinements are synchronized into the product spec. Historical carryovers are
marked as rescheduled without rewriting old test evidence.

An adapter-only fast follow was rejected because it would retain shared safety and
management gaps. Reopening all of historical F1 was rejected in favor of completing
its relevant carryovers within these ordered F2 stages.

## 3. Components and ownership

The deployment is the unit of routing, lifecycle, and resource ownership. Its
durable record binds checkpoint identity, engine/profile/recipe revisions, route
names, device-sharing policy, phase budgets, and residency/recovery policies.
Material configuration changes require an explicit revision-aware operation; editing
a profile cannot mutate a running deployment or silently reuse qualification.

| Component | Responsibility |
|---|---|
| Management API | Authenticate and validate commands; durably accept operations; expose snapshots and events |
| Controller/coordinator | Own lifecycle and generation gates; plan transitions; join activation requests; reconcile failures |
| Scheduler/accounting | Evaluate physical domains, phase peaks, sharing constraints, headroom, and retained reservations |
| Host supervision/launcher | Reserve endpoint ownership, launch and track owned processes, observe pressure, verify cleanup |
| Engine adapter | Render granted settings and implement engine-specific readiness, park, restore, and work observation |
| Inference router | Resolve a route to a deployment, bound waiting work, acquire current-generation admission, forward to its endpoint |
| Durable store | Persist deployment revisions, operations, ownership evidence, reservations, and event ordering |

Each managed deployment has its own adapter binding, process ownership identity,
private endpoint and credential references. Parked processes retain their endpoint
allocation. Credentials are not exposed in status, events, recorded command lines,
or logs. Launch arguments are validated against mllm-owned settings; log retention
is bounded. Engine administrative controls are never public inference passthroughs.
Retaining only the API server PID is insufficient: worker ownership and start
identities must also establish that the initialized engine runtime survived parking.

Adapters do not choose eviction victims, launch competing runtimes, or maintain a
second reservation authority. Engine-neutral capabilities describe supported release
and restoration behavior; SGLang must not inherit vLLM numeric sleep-level semantics.
Qualification binds the effective engine, checkpoint, recipe, environment, and
hardware fingerprints. Changes invalidate affected evidence.

Attached services retain the existing ownership boundary: F2 must complete local
attached routing without silently adopting, parking, restarting, or terminating the
external service. Their consumption cannot be assumed free capacity for managed
deployments. They do not satisfy the managed warm-residency qualification gate.

## 4. Capacity and transition planning

Every managed deployment has finite, inspectable estimates for cold initialization,
serving, parking-transition peak, parked residue, and wake-transition peak. Estimates
include weights, private KV/cache allocations, runtime overhead, temporary loading
and restoration buffers, retained copies, and an explicit safety margin. Unknown
recipes require an explicit conservative estimate or controlled qualification under
a safe reservation; they are never launched without a bound to discover their size.

On unified-memory hardware, CPU/GPU allocations share one physical domain. Labels
and sub-limits do not create extra capacity. A deployment's parked reservation is
replaced by its full wake-transition reservation, not added again; that wake peak
must already include any retained allocation that overlaps restoration.

For each step and physical domain, the sum of other owners' reservations and the
candidate's complete transition peak must fit the managed ceiling and preserve
protected host headroom. Headroom is not counted twice when already excluded from
that ceiling. Current external pressure and observation freshness are independent
checks; a ledger fit does not override unsafe live conditions. A low usage sample
does not authorize shrinking a qualified peak reservation.

GPU assignments explicitly declare shared or exclusive use. Overlap is allowed only
when the participating assignments permit sharing and aggregate budgets fit. Shared
execution provides no throughput or latency isolation guarantee.

The planner evaluates the full sequence, not only the final ready footprints:

1. If B's cold/wake peak fits while A is ready, reserve the transition and activate B.
2. Otherwise, evaluate A's parking transition, A's retained residue, and B's peak.
3. If feasible and policy permits, close A's admission, drain, park, and verify release.
4. Replace A's reservation only after release evidence. Recheck current conditions
   and reserve B's peak before any resource-increasing action.
5. Restore/initialize B and establish model readiness before replacing its peak with
   its serving reservation and admitting requests.

Peak accounting includes parking itself: a release protocol that temporarily copies
weights cannot be assumed to decrease memory monotonically. A plan that ends with
both ready may still require staging through a parked state because startup peaks
are larger. Each intermediate step must fit.

Reservation decisions and competing transition claims are atomic across deployments.
The planner must prevent simultaneous A-to-B and B-to-A plans from double-spending
capacity or issuing conflicting lifecycle actions. It does not serialize normal
inference. Resource observations and operation generations are revalidated before
each increasing step; a changed condition blocks or replans safely.

If no safe sequence preserves the requested warm-residency arrangement, report the
blocking owners and budgets and queue within the configured deadline or reject.
Do not silently stop a parked deployment, shrink model settings, or exceed headroom.
Stopping for capacity is available only under an explicitly selected policy that
relinquishes warm residency, and is reported as such.

## 5. Preinitialization, routing, and lifecycle

Preinitialization of an explicitly selected set is one observable sequence: start A,
validate model readiness, park A; start B while accounting for A, validate B, and park
B or leave B ready as requested. Additional members follow the same rules. A complete
preflight checks every stage; live conditions are rechecked during execution.
Preinitialization does not evict live user work solely to prepare idle models.

Completion reports each member's actual state and whether the requested arrangement
was established. Partial failure remains partial failure with retained resources and
completed members visible; it does not erase successful initialization or pretend
the entire set is warm. A final both-parked state still requires a feasible later
wake path for each member to claim a usable warm-residency arrangement.

Requests authenticate, resolve to a deployment, enter bounded queues, and join its
single activation operation if necessary. Explicit administrative start uses the
same join/coordination path. Stale or missing deployment state cannot bypass readiness.
Dispatch atomically checks current generation and open admission while registering
in-flight work, preventing races with admission closure.

When ready deployments fit together, both continue serving. Under contention, bounded
non-resetting admission windows prevent a busy deployment from starving a waiting
one. Queue byte/count limits and deadlines apply during cold activation, wake, and
switching. Once reclamation begins, new work cannot sneak into the draining generation.

Drain requires accepted work to complete or cancellation to be established. Client
disconnect is not proof of engine cancellation. Unknown engine work does not qualify
as safe quiescence for parking; uncertainty must be reconciled or the operation must
fail within its bound. Drain timeout does not implicitly authorize a process kill.
Streaming responses are not replayed or padded with synthetic waiting tokens.

Readiness stays closed through allocation restoration, weight restoration, required
cache invalidation/reset, and the qualified model-usability check. A listening HTTP
server or successful allocation-resume response is insufficient. Readiness probes
that execute inference are accounted for and must not accidentally wake parked models
through a status read.

Parking leaves a deployment eligible for on-demand wake. Warm-residency policy does
not allow an idle timer to silently cold-stop a retained runtime. Administrative stop
explicitly terminates owned processes and disables automatic activation; subsequent
requests cannot undo it. Undeploy removes the route and deployment only after
authorized cleanup, without implicitly deleting checkpoints or user-owned caches.

## 6. Failure, recovery, and engine qualification

An uncertain park/restore result enters reconciliation with admission closed. No
blind repetition of a possibly applied engine operation is allowed. Failed states,
timeouts, controller loss, and expired leases do not free reservations or ports.
Only verified release/cleanup evidence permits reuse.

Recovery can inspect and reconcile a surviving runtime or terminate verified owned
processes when authorized. Cold-restart recovery requires an explicit permitted
policy, is visible in operation history, and is not counted as successful warm
switching. Local controller/host restart must reconcile durable ownership and process
start identities; PID reuse must not lead to adoption or termination of another
process. Streams interrupted by a controller crash fail honestly, without an
exactly-once or resumable-inference promise.

Both adapters must establish work quiescence, release outcomes, retained budgets,
restoration completion, and cache validity for their selected recipes. Keep the
vLLM experimental-control security gate. SGLang's precise control calls, launch
prerequisites, authentication, and acknowledgements must be checked against the
pinned build during planning/qualification; endpoint availability alone is not
evidence that parking is safe or weights are restored.

Advertise capabilities as qualified, unknown, unsupported, or disabled, with reasons
and evidence references. Security permission and technical qualification are separate
checks. F2 cannot close by substituting restart-only for a missing qualified park path.

## 7. Management API, CLI, and future UI

The management API is the single semantic boundary for the CLI, future UI, and
integrations. F2 includes durable deployment submission, explicit revision handling,
inspection/list/status, lifecycle actions, local attachment, effective configuration,
local runtime/capability inventory, and qualification evidence inspection. Management
and inference credentials have separate authority; neither bypasses host policy.

Accepted commands persist the operation and affected durable state before returning
an ID. Retried submissions must not create duplicate deployments or processes.
Optional waiting observes that same operation; client exit or watch timeout does not
cancel accepted work. Status and list requests never activate engines.

Machine-readable snapshots expose:

- Deployment identity, revision, routes, engine/checkpoint/recipe fingerprints,
  desired and actual state, readiness, generation, and verified process identity.
- Serving, parked, cold-start and wake reservations; estimate provenance; observed
  usage separately from reservations; host ceiling and headroom.
- Current operation, planned transition sequence, verified releases, blocked reasons,
  supported actions, and the expected warm/cold activation mode or why it is unknown.
- Capability/security status, recovery requirements, and any cold-restart fallback.

Operations expose structured progress and stable error codes rather than requiring
clients to parse human log messages. A versioned snapshot and resumable event stream
let a future UI reconnect without starting new operations. Events have stable ordering
and cursors; snapshot/event continuity must avoid gaps. Retention is bounded; expired
cursors explicitly require a fresh snapshot rather than silently omitting history.
Supported-action hints explain availability but never replace server-side validation.

Request observations distinguish queue wait, activation wait, time to first token for
streams, and completion time. Phase timing definitions must identify overlaps rather
than imply overlapping intervals can be summed. Records distinguish cold initialization
from warm restoration and cache conditions, without logging prompts or secrets by
default. Both clients use the same authentication/authorization rules.

The UI itself, browser-specific interaction design, and dashboards are not F2
deliverables. Exact wire schemas, paths, error-code mappings, event retention settings,
and CLI fixtures are implementation-plan contracts to freeze before coding, not
permission to invent a separate UI control path later.

## 8. Verification and exit gates

Every requirement needs deterministic coverage where feasible. Live runs supplement
that coverage; skipped live tests are not hardware evidence. Preserve existing vLLM
regressions and run the same controller/adapter contracts for both engines.

| Gate | Required evidence |
|---|---|
| Q1 — Deployment isolation | Two recipes of the same engine and mixed engines route to the correct deployment; unique retained endpoints/ownership; credential and reserved-argument tests |
| Q2 — Resource transitions | Cold, park, and wake peaks; both-ready and both-parked accounting; sharing/exclusivity; stale/unknown pressure; competing plans; no premature release or double charge |
| Q3 — Sequential preparation | Both managed engines initialize once and remain retained, including both parked; partial failures and infeasible arrangements report honestly |
| Q4 — Concurrent serving | Overlapping routed inference to vLLM and SGLang on the same GPU, correct responses/routes, both ready, no unnecessary eviction |
| Q5 — Warm switching | Repeated vLLM → SGLang → vLLM with unchanged verified runtime identities, no cold initialization, correct post-wake responses, measured phases and retained footprints |
| Q6 — Pressure sequencing | A constrained managed budget forces park-before-wake; release is verified before increasing allocations; impossible warm arrangements block without silent cold stop |
| Q7 — Requests and streams | Concurrent wake bursts join one operation; simultaneous cross-model demand; administrative-start races; generation gates; bounded/fair queues; safe stream drain and disconnect accounting |
| Q8 — Failure/recovery | Lost acknowledgements, failed restore/release, process loss, PID reuse, and local controller restart cannot falsely open readiness or release reservations; explicit fallback is visible |
| Q9 — Product operations | API/CLI deployment and lifecycle tests, idempotent retry, wait/reconnect, effective configuration, stop suspension, attached ownership, and revision conflicts |
| Q10 — Future-UI contract | Authorized snapshots/events, cursor replay and expiry/resnapshot, structured blocked reasons and supported actions, redacted credentials, bounded retention |
| Q11 — Engine qualification | Pinned recipe for each engine passes readiness, parking, restoration, cache-correctness and security gates; unknown combinations remain unqualified |

Start live testing with small models for repeatability. Use conservative constrained
managed budgets to exercise pressure without deliberately exhausting physical memory.
Larger model recipes are qualified separately; earlier 14B/27B smoke results are not
post-fix streaming/concurrency or mixed-engine evidence. No specific larger recipe is
promised to fit with another model until its entire transition sequence is measured.

For each live case retain commands, build/checkpoint/recipe/hardware fingerprints,
effective budgets, process start identities, phase/event traces, memory and swap
observations, request correctness, and latency distributions. Include repeated and
fresh prompts to expose stale cache behavior. Record test counts, failures, recovery,
and limitations; HTTP success alone is insufficient. Numerical resource guardrails
and repetition counts must be fixed in the implementation plan before live runs.

F2 closes only when all gates have automated evidence where feasible and the selected
recipes have live evidence for preparation, concurrent serving, warm switching,
pressure sequencing, request bursts, streaming, and representative failure/recovery.
All remaining unsupported combinations must be explicit; qualifying one recipe does
not qualify every model, engine version, or native engine feature.

## 9. Review and implementation boundary

The owner approved the architecture, transition sequencing, retained-runtime guarantee,
lifecycle/recovery behavior, management/observability, future-UI API direction, and
the consolidated document after review. Approval is not implementation or live evidence.

Write implementation plans in the three stages from §1, splitting the shared foundation
into bounded resource-contract, runtime-coordination, and management plans. They must
map the gates to exact code and tests, select pinned runtime recipes and safe live
guardrails, freeze public schemas, and preserve these lifecycle/resource invariants.
No implementation starts merely because this design file exists.

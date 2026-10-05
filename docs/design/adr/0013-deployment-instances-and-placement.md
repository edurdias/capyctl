# ADR 0013 — Deployment instances and scheduler placement

**Status:** Accepted (owner decision P1, 2026-09-22), amended by owner decision Q7 the same
day: per-instance `stop instance` and `start instance` verbs are added (see Amendment).
Owner answers Q5 and Q8 confirm decisions 9 and 7 as written.
**Amends:** `SPEC.md` §1.1 (R06), §2 (vocabulary), §10 (routing and switching) and §16.4
(two-host example). §6, §7, §11 and §13 are unchanged and govern everything this
document relies on; ADR 0007 governs every capacity check named here.
**Replaces:** units W7 and W9 of `docs/plans/2026-09-22-two-host-control-plane-plan.md`
(separate replica deployments bound to one route).
**Related:** ADR 0014 (deployment engine configuration, owner decision E1) supplies the
per-instance memory request (owner decision P2) and the checkpoint digest every instance
shares.

## Context

Matrix decision D9 requires one route to be served by engines on both Sparks, balanced by
the router on its own in-flight counts plus engine load the host agents report, with
failover on host loss. The first plan modelled that as several deployments bound to one
route (W7) with a router that picked among them (W9). Reviewing SPEC §2, §3, §6 and §10
and ADR 0008 with the owner on 2026-09-22 (P1), the owner rejected that shape:
load-balanced replicas live inside one deployment. A deployment declares a count of
instances plus optional placement constraints; the server's scheduler places each
instance from live capacity when it activates; the router balances across the
deployment's ready instances.

What exists today assumes one engine group per deployment:

- A deployment names one host (`host:` in the deployment document). The server scopes the
  document to that host at deploy time
  (`crates/mllm-management/src/configuration.rs`, `scope_deployment_document`), so
  placement is fixed before any capacity is known.
- The store allows one retained binding per deployment (`one_retained_binding`), one
  lifecycle claim per deployment (`lifecycle_claims` keyed by `deployment_id`), one
  activation per (deployment, revision, generation) and request leases keyed by deployment
  (`crates/mllm-store/src/schema.rs`).
- `deployments.current_generation` is one counter, and every wire message (W3
  `LoadSample`, `MemberExit`, ingress generation fencing) identifies a runtime by
  deployment id plus generation.
- The router resolves a route to one deployment (`crates/mllm-router/src/chat.rs`).

## Decision

### 1. Vocabulary

An **instance** is one engine group realizing a deployment: its own generation, runtime
binding, reservation owner, lifecycle claim and runs, request leases, ingress endpoint
and state. A deployment has `instances: N` (default 1). Instances of one deployment share
everything the deployment declares — model and checkpoint digest, engine family and
installation name, `engine_config`, residency, route and policy. They differ only in
placement and runtime identity.

An instance is not a TP2 group member. A TP2 instance is one engine group spanning two
hosts (SPEC §11); `instances: 2` of a TP1 recipe is two independent engine groups. The
two compose: a TP2 deployment with `instances: 2` would need four host slots. This ADR
places single-host instances only and refuses a multi-host `topology` with a named
error. The refusal stays in code until ADR 0028 ships; ADR 0028 then replaces it.

### 2. Deployment document

```yaml
instances: 2                     # default 1; changing it is a revision (decision 7)
placement:
  hosts: ["host-a", "host-b"]   # allowed set; omitted = every enrolled host
  selector: {gpu: gb10}                 # optional host-label match, ANDed with hosts
  strategy: spread                      # spread (default) | pack
  max_per_host: 1                       # default: unbounded
```

The existing `host: <name>` field remains as shorthand for `placement.hosts: [<name>]`,
so every current deployment keeps its meaning with `instances: 1`. Naming a device
(`devices: [{id: gpu:0}]`) is allowed only when the allowed set is one host; otherwise a
deployment states a device count and sharing mode and the scheduler picks devices on the
chosen host. Host labels are host policy (`resource_policy.labels`), published with the
host document, never asserted by the deployment.

### 3. Validation at deploy time

The deployment is resolved against every allowed host whose policy is currently published
(`resolve_effective` per host, which already checks engine installation, security policy
and recipe shape). The deploy succeeds if at least one allowed host resolves; hosts that
refuse are listed in status with their reason and are never candidates. `max_per_host`
times the number of resolving hosts must be at least `N`, or the deploy is refused as
unplaceable. Nothing is reserved at deploy time and no host is chosen. Effective
revisions become keyed by (deployment, revision, host), because host policy and the
installation's build fingerprint differ by host (SPEC §8: global installation names do
not prove matching builds).

### 4. Placement at activation

Placement runs inside the coordinator when an instance must activate, as one step of the
activation transaction, under ADR 0007: fresh observations, epoch compare-and-swap, full
phase footprint.

1. Candidates: allowed hosts that resolved in decision 3, are eligible (live reconciled
   session, not revoked, not draining; W12), host the checkpoint with the recorded digest
   (ADR 0014), and are below `max_per_host` for this deployment.
2. Fit: the instance's activation footprint, derived from its memory request (P2,
   ADR 0014 §5), must pass ADR 0007 against current reservations and observations. Device
   selection follows SPEC §7.3: exclusive claims conflict; two instances may share a
   device only when the host device and the deployment both permit sharing. On a GB10
   (one GPU per host) co-residence therefore needs `sharing: shared`.
3. Order: `spread` prefers the host with fewest instances of this deployment, then the
   most remaining headroom; `pack` prefers the host with most instances of this
   deployment, then the least headroom that still fits (best fit). Ties break by host id
   so the choice is deterministic and testable.
4. No host fits without releasing capacity: the planner hands the request to the
   switching rules in decision 8; it never places over budget.
5. The chosen host, devices and the drawn generation are written durably with the
   reservation. A placement is sticky: a parked instance can only wake where it parked;
   a stopped instance (verified cleanup) keeps its host as a preference and may be placed
   elsewhere at its next activation if that host is ineligible or full.

Instances may co-reside on one host whenever the fit and device rules allow. Pinning an
instance to a host remains possible by naming one host.

### 5. Identity and the store

- `deployment_instances(deployment_id, instance_index, host_id, device_json, generation,
  state, placed_at)`; indices are dense `0..N-1` and stable across restarts.
- Generations stay one monotonic counter per deployment. Each instance activation draws
  the next value and records it on its row, so (deployment id, generation) still
  identifies exactly one instance incarnation. The W3 wire messages, ingress fencing and
  stale-generation checks (T18, T34) need no change; "current generation" becomes a
  per-instance value.
- `one_retained_binding`, `lifecycle_claims`, `one_activation` and `request_leases` gain
  `instance_index`. Resource owners become per instance
  (`deployment:<id>/instance:<k>`), so each instance's reservation is charged and released
  only on its own evidence (SPEC §7.3, ADR 0011).
- The migration is additive: every existing deployment becomes `instances: 1` with
  instance 0 on its current host, keeping its generation, binding and reservation.

### 6. State, status and commands

Instances run the SPEC §6.1 state machine independently. The deployment's desired state,
admission switch, suspension and revision stay deployment-level. Status shows the
deployment's aggregate and every instance (index, host, devices, state, generation,
reservation, last error, load sample age):

- `ready` when at least one instance is READY; the condition `degraded` when fewer than
  the desired count are READY;
- `failed` only when every instance is FAILED; one failed instance is a per-instance
  failure with its own attempt budget (ADR 0011 decision 5, counted per instance).

Commands act on the deployment. `start` targets all N instances READY; `park` parks every
READY instance; `stop` drains and stops every instance; `delete deployment` removes the
deployment and its route only once every instance's cleanup is verified, and `delete
deployment --stop` stops every instance first and then deletes (SPEC §6.3; owner decision
2026-09-23 renamed `undeploy model`). The operation succeeds when every instance reaches the target;
otherwise it reports per-instance outcomes, and an uncertain instance keeps its
accounting (AGENTS.md, SPEC §6.1). Per-instance effects also come from a count change
(decision 7), `drain host` (W11) and failure handling. Owner decision Q7 adds per-instance
verbs; see Amendment.
Idle policy (SPEC §6.5) is evaluated per instance on that instance's router in-flight
count.

### 7. Count change is a revision

Changing `instances` is a revision (SPEC §6.3 "revision-aware operation"). A revision that
changes only the count is non-disruptive:

- Increase: new indices are created stopped; they activate at the next `start`, or on
  demand under decision 9.
- Decrease: surplus instances are chosen in this order: stopped, then parked, then READY
  on the host with most instances of this deployment, then highest index. Each is drained
  (SPEC §10) and stopped with verified cleanup before its row is retired; indices are
  compacted only after retirement.

Any other change (model, engine configuration, residency, placement constraints) produces
a new revision whose instances start from scratch: existing instances drain and stop with
verified cleanup, then the new revision activates. Rolling replacement is not in scope.

### 8. Automatic switching with instances (D4, SPEC §10)

The unit of eviction is an instance on one host; a switch plan is always per host (M35).

1. **Use what is READY.** A request for B is dispatched to any READY instance of B; no
   switch is planned while one exists.
2. **Place without eviction first.** If B has no READY instance, wake a parked instance of
   B when its host can take it; otherwise place a new or stopped instance on a host where
   it fits without eviction (decision 4).
3. **Evict only on the host that needs room.** When eviction is required, the planner
   picks one host (the candidate needing the least eviction, ties by decision 4 order) and
   selects victims there only. It never parks or stops instances on another host to make
   room on this one.
4. **Victim order on that host.** Among instances not serving a waiting group and not
   holding a warm-residency commitment (SPEC §6.5): first instances whose deployment keeps
   another READY instance elsewhere (its route keeps serving), then least recently used.
   Only the minimum set whose release makes B fit is chosen, using ADR 0007 forecasts.
5. **Fairness applies per deployment.** Evicting an instance that is not its deployment's
   last READY instance drains only that instance (close its gate, drain, quiesce) and does
   not open a fairness window: the deployment keeps serving elsewhere. Evicting the last
   READY instance of A follows SPEC §10 steps 3–5 in full, including the bounded,
   non-resetting admission window.
6. **No thrash.** An instance evicted by a switch stays parked or stopped until demand
   needs it (A has no READY instance) or an operator runs `start`. The scheduler never
   refills A's count by evicting another deployment in the background.
7. **One instance per switch.** A switch guarantees B one READY instance. Further
   instances of B activate only where they fit without eviction, unless an operator's
   `start` asks for all N.
8. Park or stop follows each victim's declared residency (`deep` parks; `restart_only`
   stops with absence proof). A drain timeout fails the switch; nothing warm-committed is
   cold-stopped silently (SPEC §18 F2).

### 9. On-demand activation

A request for a deployment with no READY instance joins one deployment-level activation
(T15), which brings up one instance by decision 8 rules 2–4 and dispatches as soon as it
is READY. Remaining instances activate only where they fit without eviction. Explicit
`start` targets all N and may evict under decision 8.

### 10. Router

Candidates are the deployment's instances with open admission, a live reconciled session
and an eligible host. Score = max(router in-flight for the instance, engine running plus
waiting from a fresh W3 `ReportLoad` sample), plus a KV-pressure penalty above a
threshold; ties rotate. A sample older than the staleness bound (default 3 s) is ignored
and the score falls back to router in-flight. The load table keys samples by (deployment,
generation), which identifies the instance (decision 5), and drops stale generations.

Failover is allowed only before upstream acceptance: connection refused, gate closed, or
session lost before dispatch. An accepted request is never replayed (SPEC §10, T38).
Host loss closes that instance's admission at the controller and removes it from
candidates; its reservation stays charged until reconciliation (T32). Queue bounds apply
per deployment, as today (T19). Every selection is logged with its inputs.

### 11. Memory request (P2)

Each instance reserves the deployment's declared per-instance memory request, or, when
omitted, the request derived from checkpoint size, requested KV and a per-engine overhead
margin (ADR 0014 §5). The first live run of each model measures its peak and matrix
budgets are recomputed from measurements. Placement and switching use only the declared
or derived request, never a sampled value.

## SPEC amendments (exact text)

- **§1.1 R06**, replace the row text with: "Deploy a model as a deployment of one or more
  instances, placed by the scheduler from live capacity on an explicit allowed host set
  or selector, or pinned to one host, without manually launching any engine (ADR 0013)."
- **§2**, add after the Deployment row: "| **Instance** | One engine group realizing a
  deployment, with its own generation, runtime binding, reservation, lifecycle and
  placement. A deployment declares its instance count; instances share its model,
  engine configuration, route and policy and differ only in placement and runtime
  identity (ADR 0013). |". Replace the Deployment row's text with: "Durable named
  declaration of model, engine installation and configuration, instance count, placement
  constraints, route and policy. Its deployment ID survives start, park, stop, count
  changes and controller restarts." Replace the Engine group row's text with: "The
  complete runtime realization of one instance, potentially multiple processes and
  hosts." Replace the Generation row's text with: "Monotonic per-deployment identity; each
  instance activation draws a new value, so deployment and generation identify one
  instance incarnation. Used to reject stale commands and inference dispatch. Not evidence
  that an old process stopped."
- **§10**, insert after "Resolve model aliases to explicit deployments.": "A deployment
  with several instances is served by all of its READY instances; the router selects
  among instances with open admission using its own in-flight counts and fresh
  host-reported engine load, and fails over to another instance only before upstream
  acceptance (ADR 0013)." Insert before "A request for B while A owns the pool follows:":
  "Eviction is planned per host and per instance: B is served by an existing READY
  instance when one exists; otherwise the planner releases capacity only on the one host
  that will run B's instance, preferring instances whose deployment keeps serving
  elsewhere. Steps 3–5 below apply in full when the victim is its deployment's last
  READY instance (ADR 0013)."
- **§16.4**, replace the heading with "Two-host engine group (one TP2 instance) with private
  host-cache and persistent storage" and add after the example's closing paragraph: "This
  example is one instance whose engine group spans two hosts. Load-balanced instances are
  a different shape: `instances: 2` with `placement: {hosts: [host-a, host-b],
  strategy: spread, max_per_host: 1}` and a single-host topology runs two independent
  engine groups behind one route (ADR 0013). Multi-host group placement is not yet
  specified."
- **§19** needs no text change; "explicit placement" is read as explicitly constrained
  placement, which an allowed host set or a single named host provides.

## Consequences

- W7 (replica routes) and W9 (router replica selection) are replaced by instance units:
  the instance model in store and configuration, placement in the scheduler and
  coordinator, and instance selection in the router.
- The route table keeps one deployment per route; M55 (a replica with another checkpoint)
  becomes impossible by construction, and a second deployment claiming the route is still
  refused.
- Deploy no longer binds a host. Status can show a deployment with no placement yet,
  which is an accepted on-demand deployment, not a failure (SPEC §6.4).
- Every lifecycle path (activation, park, wake, stop, delete, recovery, re-attach, exit
  detection) addresses an instance; the coordinator serializes per instance, not per
  deployment.

## Open issues

1. **Mixed-engine replicas (M61).** Instances share one engine installation name and
   configuration, so vLLM plus SGLang behind one route is not an instance set. SPEC §1.2
   says separate runtime configurations are separate deployments. M61 needs recasting as
   two deployments on two routes, or dropping.
2. **Per-instance operator actions.** Resolved by owner decision Q7 (see Amendment): M62
   (stop one replica) is `stop instance`; M65 (remove one replica under load) is a count
   decrease or `drain host`.
3. **Non-count revisions interrupt service.** Stop-all-then-start is honest but drops the
   route to zero READY instances during a model or configuration change. Rolling
   replacement needs the SPEC §19 update design first.
4. **Installation drift across hosts.** Instances on two hosts may run different builds
   under one installation name. Effective revisions are per host and status shows each
   build fingerprint, but nothing forces equality. Requiring equal fingerprints is a
   possible policy, not decided here.
5. **Placement quality.** Spread and pack use reservations and headroom only; they do not
   predict load. Rebalancing a skewed placement requires a stop and start.
6. **Parked-residue cost of N.** Every parked instance keeps its residual floor, so
   `max_parked` and aggregate residual budgets (SPEC §6.5) count instances, not
   deployments. A deployment with many instances can exhaust `max_parked` alone.
7. **TP2 composition.** Group placement (k hosts per instance, rendezvous, rank
   readiness) is designed in ADR 0028; the refusal in decision 1 stays in code until
   ADR 0028 ships, and ADR 0028 then replaces it.
8. **Evidence.** None of this is qualified. Instance rows need live runs on both Sparks;
   CPU and Fake-engine tests only validate the logic.

## Amendment (owner decision Q7, 2026-09-22)

Decision 6 proposed no per-instance verb. The owner added two:

- `mllm stop instance <deployment>/<n>` stops instance `n` with verified cleanup and records
  the operator's stop on that instance. It is an ordinary stop for the deployment: the
  deployment stays eligible for on-demand activation of its other instances (SPEC §6.3),
  while on-demand activation never restarts the stopped instance.
- `mllm start instance <deployment>/<n>` lifts that instance's stop and brings it to READY.
- `mllm start deployment` targets all N instances (decision 9, owner answer Q5) and lifts
  every per-instance stop.

The management API is `POST /management/v1/deployments/{id}/instances/{n}/actions` with the
deployment action body (`action`, `expected_revision`, `deadline_ms`); only `start` and
`stop` are accepted. The CLI grammar stays action-first. An uncertain instance keeps its
accounting exactly as a deployment-level stop does.


## Amendment (owner decisions 2026-09-23): warm residency, explicit eviction

**Warm residency (SPEC §6.5).** A deployment may declare a warm-residency commitment:

```yaml
lifecycle:
  warm: true
```

It is a deployment-level policy like `instances`, recorded per revision
(`deployment_revision_instances.warm`, store schema v28) and never part of the per-host
recipe or its fingerprint. Omitted it is false and the command identity is unchanged.
While the deployment's current revision declares it, its instances are:

- never chosen as victims by switching (decision 8, request-driven or `--evict`);
- never parked or stopped by the idle policy (ready-idle or parked-idle);
- never stopped to reclaim parked capacity for another start.

Only an explicit stop (deployment or instance) or a recovery action ends its residency. A
request whose deployment fits only by evicting a warm instance is refused for capacity,
exactly as when nothing can be released. Status reports `warm: true`.

**Explicit eviction.** `mllm start deployment <d>` and `mllm start instance <d>/<n>` never
evict. With `--evict` they run the decision 8 switch plan (the host needing the fewest
evictions, the same victim order, fairness window and drain timeout) for the deployment's
next instance or the named one, then accept the ordinary start, and report the released
victims (`victims`, `switch_id` in the receipt). The management body carries `"evict":
true`; only `start` accepts it. The switch runs as its own task, so a client that
disconnects never abandons victims mid-drain, and the CLI journals the body, flag
included, so a rerun by request id replays it. On a host whose agent holds one journal
claim at a time, the plan releases every launch occupying the host (decision 4
`occupied`), and refuses the host while any occupant is not an eligible READY victim.

Evidence: CPU and Fake-engine tests only; not live-verified.

## Amendment 2026-10-05 — group placement (ADR 0028)

Decision 1's refusal of a multi-host `topology` stays in code until ADR 0028 ships; ADR 0028
then replaces it. A group is one
instance whose members run on exactly the hosts named in `placement.hosts`, head first,
with `instances: 1`. Instances of a single-host recipe are unchanged.

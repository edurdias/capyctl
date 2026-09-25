---
title: Two-Host Control Plane - Implementation Plan
type: implementation-plan
date: 2026-09-22
status: W0, W1 and W3 done locally (not live); W0 live verification in progress; rest proposed
matrix: docs/plans/2026-09-22-two-host-engine-matrix.md
designs: docs/design/adr/0013-deployment-instances-and-placement.md, docs/design/adr/0014-deployment-engine-configuration.md
---

# Two-Host Control Plane - Implementation Plan

## 1. Goal and inputs

Build what the two-host matrix (M01–M72) needs, in dependency order: one control-host server
controlling both Sparks running vLLM and SGLang single-rank deployments of the four D5
models, with instances, automatic switching, parking, recovery, deployment deletion and graceful
shutdown. Owner decisions D1–D11 (matrix §2) and E1, P1–P4 (status runbook, 2026-09-22)
are authoritative; TP2 stays parked. E1 is designed in ADR 0014 (engine configuration,
checkpoint digest, P2 memory request); P1 in ADR 0013 (instances and placement), which
replaces the former replica-route units W7 and W9. The request path is controller
(router) → host agent → engines; clients never reach an agent or engine. Status lives only
in `docs/runbooks/f2-current-status.md`. CPU and Fake-engine tests are not qualification;
a unit is done only when its matrix rows pass live. AGENTS.md conventions apply to every
unit (SPEC citations, T-ID tags, uncertainty retains accounting, core suite plus Clippy,
excluded files untouched).

## 2. Work units

Each unit: goal; owning files (exclusive while it runs); design; tests; rows unlocked;
conflicts. Line numbers are from the 2026-09-22 tree.

### Done locally (not live)

- **W0. U5 gap closure.** U5-G1 uncertain remote launch settled by an authenticated host
  Terminate with gone evidence, or kept uncertain with accounting; U5-G2/G06 session loss
  suspends dispatch and a `Probe` action re-proves readiness against the same owned
  processes; U5-G3 status reports `uncertain`. **A live verification of W0 is in
  progress.** Rows: M39–M41, M47. Known limits: mid-Initialize agent death waits for the
  Initialize deadline; adoption refuses in-flight leases; host journal never compacted.
- **W1. Deep-park default on (D1, G16).** ADR 0012; SPEC §9.1, §16.2, §18, T21 and
  AGENTS.md amended. Status marking of exposed profiles is W14.
- **W3. Protocol additions.** Session protocol version 2: Park/Restore member actions,
  residency evidence, bounded `ReportLoad` samples, member-exit reports; command encoding
  stays version 1. Later wire needs: `DigestCheckpoint` (WE3).

### W2. Models, budgets and live matrix harness (D5, D6, D10, G11, G12)

- **Goal.** Everything a live row needs that is not product code.
- **Files.** New `scripts/live/matrix/` (runner, E0 and I1 capture, load generator, fault
  helpers, budget policies `normal.yaml` and `tight.yaml`, per-model deployment fixtures).
  The earlier `scripts/live/run-on-spark.sh` runner and its in-process suites
  (`live_vllm.rs`, `live_sglang.rs`) were retired into rows M38 and M73–M75 on 2026-09-23
  (owner decision 2026-09-22); no in-process live test is added.
- **Design.** Budgets from matrix §3; allocations are the P2 memory request (ADR 0014 §5),
  recomputed from M16 measured peaks. Fixtures use `engine_config` and `instances` once
  WE1 and I1 land. Fault helpers signal only PIDs from agent ownership evidence (SPEC
  §13.2); the external allocation is one bounded process with a cleanup trap (D6).
  Read-only host-b vLLM parity check closes G12.
- **Tests.** Syntax check and a Fake-engine dry run (harness validation only).
- **Conflicts.** None.

### WE. Deployment engine configuration (E1, P2, G17; ADR 0014)

Three sub-units. WE1 first; WE2 and WE3 then run in parallel.

**WE1. Configuration, policy and memory request.**
- **Files.** `crates/mllm-config/src/schema.rs` (deployment `engine_config`, host
  `security.extra_args`, `approved_options`, `approved_paths`; profile tuning fields
  removed), `effective.rs` (`normalize_launch` `:769-969`), `effective/core.rs`,
  `effective/sglang.rs` (constants removed), `engine_policy.rs` (`:13-49`: approved list
  replaced by reserved and sensitive lists, prefix matching, per-family
  `validate_extra_args`); `crates/mllm-domain/src/launch.rs` (settings types);
  `crates/mllm-cli/src/standalone_config.rs` and its tests (template).
- **Design.** Typed common and per-family fields (ADR 0014 §2), reserved lists (§3), safe
  defaults with `mllm default` provenance (§4), extra arguments behind
  `accept_extra_args` (§6), sensitive options behind host approval (§8). P2: memory
  request declared or derived from weights bytes, KV and a per-family margin; derived
  phases; explicit `resources:` still overrides (§5). Old profile tuning fields refused
  with a message naming `engine_config`. WE1 makes only the smallest adapter edits needed
  to keep the workspace compiling; WE2 owns their redesign.
- **Tests.** T14 (effective config and provenance), T03 (strict refusal of moved fields),
  reserved prefix and duplicate refusal, sensitive option refused without approval,
  derivation arithmetic and overflow.

**WE2. Rendering and launch-time enforcement.**
- **Files.** `crates/mllm-adapters/src/sglang/args.rs` (`validate_settings`, KV bound),
  `sglang/frozen.rs`, `vllm/args.rs`, `vllm/frozen.rs`; `runtime/sglang_entry.py`
  (`_SETTINGS` and pins, checkpoint call removed), `runtime/sglang_server_args.py`
  (`_FIXED` split; `enable_metrics` on as a reserved field, formerly in W8),
  `runtime/sglang_source_preflight.py` (inventory from the registered manifest), new
  `runtime/vllm_entry.py`; `runtime/tests/`.
- **Design.** Render typed fields per engine; parse `extra_args` with the installed
  engine's parser; revalidate only the reserved subset after resolution; closed
  `effective_args_mismatch`. vLLM runs through the protected entry with the key guard.
- **Tests.** T14, T21 (guard still loaded), T22 (both engines on the same contract),
  abbreviation and config-file bypass attempts refused at launch.

**WE3. Checkpoint digest.**
- **Files.** New `crates/mllm-agent/src/checkpoint.rs`; `native_execution.rs` (verify
  before launch and wake); proto and `crates/mllm-protocol/src/execution.rs`
  (`DigestCheckpoint`); `crates/mllm-controller/src/remote_execution.rs` (plan carries the
  recorded digest); `crates/mllm-management/src/configuration.rs` (record at deploy,
  `checkpoint_digest_pending`); delete `runtime/checkpoint_manifest.py` and
  `runtime/checkpoint_preflight.py` with their tests after WE2 merges.
- **Design.** ADR 0014 §7: canonical manifest digest; per-host cache by stat identity;
  stat check plus small-file rehash at launch and wake, full rehash on any change;
  mismatch refuses before any effect. The manifest supplies weights bytes to WE1's
  derivation.
- **Tests.** T14, T34 (stale digest refused), symlink escape refused, changed file forces
  rehash and refusal, standalone path identical.
- **Unlocks (WE).** M16 for q14, q30, q27 on both engines; G17. **Conflicts.** See §3.

### I1. Instance model in configuration, store and status (P1; ADR 0013 §1–3, §5–7)

- **Files.** `crates/mllm-config/src/schema.rs`, `effective.rs` (`instances`,
  `placement`, `host` shorthand, per-host resolution); `crates/mllm-store/src/schema.rs`,
  `migrations.rs`, `deployments.rs`, `ordinary_lifecycle*`, `resource_ledger.rs` owner
  ids, `managed_configuration*` (effective revisions per host), `snapshot.rs`;
  `crates/mllm-management/src/actions.rs` read paths only.
- **Design.** `deployment_instances` table; per-instance binding, claim, activation,
  request lease and resource owner; generation drawn per instance activation from the
  deployment counter; additive migration to `instances: 1`. Deployment aggregate plus
  per-instance status; `degraded` condition; per-instance attempt budget. Count change
  as a revision (non-disruptive), other changes stop-all-then-start.
- **Tests.** Migration replay of existing state (T08), T09, T10, T18 (stale generation),
  per-instance reservation release only on evidence (T16, T32).
- **Unlocks.** Everything instance-based. **Conflicts.** Config with WE1; store lifecycle
  with W5, W6; `snapshot.rs` with W14.

### I2. Placement in the scheduler and coordinator (ADR 0013 §4, §9)

- **Files.** New `crates/mllm-scheduler/src/placement.rs`;
  `crates/mllm-controller/src/coordinator_port.rs`, `coordinator/worker.rs` (activation
  path); `crates/mllm-management/src/configuration.rs` (deploy validates against allowed
  hosts, binds none).
- **Design.** Candidates, ADR 0007 fit with the P2 request, device and sharing rules,
  spread and pack ordering, deterministic ties, durable placement with the reservation,
  sticky parked placement. On-demand activation brings up one instance; `start` targets
  N. No fit without eviction hands off to W10 (until then: queue within the deadline and
  fail with a capacity diagnostic).
- **Tests.** Placement table tests (spread, pack, `max_per_host`, co-residence on a shared
  device, ineligible host), T15 single activation, T23 budget, stale observation refusal.
- **Unlocks.** M54, M63 (recast). **Conflicts.** `coordinator_port.rs` with W5, W6, W10;
  `configuration.rs` with WE3.

### I3. Router instance selection and failover (D9; ADR 0013 §10)

- **Files.** `crates/mllm-router/src/chat.rs` (`resolve` `:21-77`), `lib.rs`,
  `admission.rs`, new `balance.rs`, `forwarders.rs`; wiring in
  `crates/mllm-cli/src/remote_roles.rs` (`RouterDeps`).
- **Design.** Candidates are READY instances with open admission, a live reconciled
  session and an eligible host. Score = max(router in-flight, fresh running + waiting)
  plus a KV-pressure penalty; stale samples fall back to in-flight; ties rotate. Failover
  only before upstream acceptance; accepted requests never replayed. Host loss closes the
  instance and keeps its reservation. Every choice logged with inputs.
- **Tests.** Fake-transport balance, skew, staleness, failover; T15, T19, T32, T37, T38.
- **Unlocks.** M54–M60, M62–M63 (recast). **Conflicts.** `chat.rs` with W10; `lib.rs`,
  `remote_roles.rs` with W11.

### W4. Remote Park/Restore execution and vLLM persisted park (D3, G02, G04)

- **Files.** `crates/mllm-agent/src/native_execution.rs` (authorize `:598-645`, effect
  `:409+`), `journal.rs`; `crates/mllm-controller/src/remote_execution.rs` (`:97-212`);
  `crates/mllm-adapters/src/vllm/adapter.rs` (`:214-221`), `vllm/http.rs`.
- **Design.** Per binding, hence per instance. Park only from `ready`, Restore only from
  `parked`; intent persisted before the effect (SPEC §13.1); ingress gate closed, ingress
  in-flight 0 and adapter quiescence before parking (SPEC §10 steps 3–5). vLLM Park is
  `POST /sleep?level=2`; Restore is `wake_up` (weights), `collective_rpc reload_weights`,
  `wake_up` (KV), prefix-cache reset (SPEC §9.1). Evidence: identity unchanged, release
  or restore milestones, `MemAvailable` delta. Gate opens only after a fresh model probe
  (SPEC §6.1). Partial failure quarantines, never repeats a collective (T20).
- **Tests.** T16, T20, T21 (`restart_only` issues no sleep), T22, T34.
- **Unlocks.** M28–M30 with W5. **Conflicts.** `native_execution.rs` with WE3, W13;
  `remote_execution.rs` with WE3, W12.

### W5. Park, wake and preinitialize in the coordinator (G03)

- **Files.** `crates/mllm-controller/src/coordinator_port.rs` (`:462-481`),
  `coordinator/worker.rs`; `crates/mllm-store/src/ordinary_lifecycle*`, `residency*`,
  `resource_ledger.rs`; `crates/mllm-scheduler/src/sequence.rs`;
  `crates/mllm-management/src/actions.rs` (`:127`).
- **Design.** Per instance, following `crates/mllm-domain/src/park.rs`. Parked instances
  keep a residual-floor reservation; `max_parked` and residual budgets count instances;
  every transition budgeted before resources increase (SPEC §6.5, §7.3). `park
  deployment` parks every READY instance. A request to a parked deployment wakes one
  instance through the activation join (T15). Preinitialize runs initialize, verify,
  park, then the next; refused for `restart_only`.
- **Tests.** T15, T16, T20, T23, T26.
- **Unlocks.** Phase C. **Conflicts.** `coordinator_port.rs` with I2, W6, W10; store
  lifecycle with I1, W6.

### W6. Delete deployment (D11, G10)

Owner decision 2026-09-23 renamed the command from `mllm undeploy model` to `mllm delete
deployment <name|id>` (action first, naming the object removed; no alias) and added
`--stop`. The management action is `delete`, the refusal code `delete_requires_cleanup`,
the event `deployment_deleted`, the operation kind `delete`, and the tombstone kind
`deleted`.

- **Files.** `crates/mllm-store/src/delete.rs` (tombstone), `snapshot.rs`, `events.rs`;
  `coordinator/worker.rs`; `actions.rs`, `configuration.rs`; CLI `grammar.rs`,
  `client.rs`, `client/delete.rs`, `client_journal.rs`.
- **Design.** SPEC §6.3. The server accepts a delete only when nothing on any instance
  is held (runtime, lease, reservation, claim, open run, step or operation) and answers
  409 `delete_requires_cleanup` otherwise; it never stops anything or releases
  accounting. An accepted delete removes the routes, instance rows and checkpoint digest
  records in one transaction and leaves the deployment as a tombstone (id and history
  kept; name and route reusable). Checkpoints and caches are never deleted. `--stop` is
  a CLI flow over two durable request-journal steps: the operator (administrative) stop
  of every instance, so on-demand activation cannot restart it before the delete, then
  the delete once every operation has settled. If an instance's host is offline, or
  cleanup is otherwise not proven within the Stop window, it reports `cleanup: pending`
  with the operation ids and leaves the deployment intact; a rerun with the same
  `--request-id` replays the same Stop and resumes. Idempotent by request id (T09).
  Removing one instance is a count decrease (I1), not a delete.
- **Tests.** T09, T10, T16, T17, T32. **Unlocks.** M64, M66, M67, M72; M65 recast as a
  count decrease under load. **Conflicts.** Run last among store and coordinator units.

### W8. Host load reporting (D9, G14)

- **Files.** New `crates/mllm-agent/src/load.rs`; one hook in `session.rs`; `ingress.rs`
  (read in-flight); new `crates/mllm-controller/src/load_table.rs`; receive arm in
  `agent_sessions.rs`. SGLang `enable_metrics` moved to WE2.
- **Design.** One loopback `/metrics` scrape per Ready binding per tick (default 1 s,
  bounds 250 ms–5 s, 300 ms timeout), per-launch key where the guard requires it. vLLM
  `vllm:num_requests_running`, `vllm:num_requests_waiting`, KV usage; SGLang
  `sglang:num_running_reqs`, `sglang:num_queue_reqs`, `sglang:token_usage`; names pinned
  to the recorded engine sources, `scrape_ok = false` rather than guessing. Samples are
  ephemeral; the server keeps the latest per (deployment, generation), which identifies
  the instance, drops stale generations, and marks samples older than 3 s stale. Metrics
  never reachable through ingress or the router (SPEC §13.3, M08).
- **Tests.** Parsers on captured metrics text, stale generation drop (T34), ingress
  refuses `/metrics` (T37), batch bounds.
- **Unlocks.** M56, M57, M59 with I3. **Conflicts.** `session.rs`, `ingress.rs` with W11;
  `agent_sessions.rs` with W12, W13.

### W10. Automatic request-driven switching (D4, G05; ADR 0013 §8)

- **Files.** `crates/mllm-router/src/switch.rs` (`:122`), `chat.rs`; new
  `crates/mllm-controller/src/coordinator/switching.rs`, `coordinator_port.rs`;
  `crates/mllm-scheduler/src/sequence.rs`.
- **Design.** Planner in the coordinator (SPEC §10). Use a READY instance of B if any;
  else wake or place without eviction; else plan on one host only, evicting instances
  whose deployment keeps serving elsewhere first, then least recently used, honoring
  warm commitments; minimum victim set by ADR 0007 forecast. Fairness window only when a
  victim is its deployment's last READY instance. No background refill; one instance of B
  per switch. Park (deep) or stop (`restart_only`), verify release, reserve, launch or
  restore, probe, open, dispatch. Drain timeout fails the switch.
- **Tests.** T15, T16, T17, T19, T20, T23, T27 (independent per-host plans).
- **Unlocks.** Phase D. **Conflicts.** `chat.rs` with I3; `coordinator_port.rs` with W5,
  W6; `sequence.rs` with W5.

### W11. Role shutdown and explicit drain (D11, G15, P3)

- **Files.** `crates/mllm-cli/src/remote_roles.rs` (`:356-373`, `:447-459`), `roles.rs`
  (standalone has no handler today), `grammar.rs` (`drain host`, `stop standalone
  --drain`); `crates/mllm-agent/src/ingress.rs`, `session.rs`;
  `crates/mllm-router/src/lib.rs`; `crates/mllm-controller/src/runtime.rs` (embedded
  re-attach).
- **Design.** P3: SIGTERM or stop on any role, standalone included, is a service restart.
  Admission closes (router answers 503), in-flight streams finish or cancel within a bound
  (default 30 s), engines stay running and owned, journals flush, exit 0. The next start
  re-attaches them through the fresh-probe path: remote via W0's `Probe` (W12 extends it
  to server restart), standalone via the same probe on its embedded bindings. `drain host
  <name>` and `stop standalone --drain` stop every engine with verified cleanup, mark the
  host not eligible while draining, and leave deployments eligible for on-demand
  activation (SPEC §4.3: a role restart never deletes a deployment).
- **Tests.** T01, T33, T38; signal tests in the CLI binary suite; standalone restart keeps
  the engine PID and re-proves readiness before dispatch.
- **Unlocks.** M68–M72. **Conflicts.** `lib.rs`, `remote_roles.rs` with I3; `session.rs`,
  `ingress.rs` with W8; `runtime.rs` with W12.

### W12. Server restart re-attach and host eligibility (D8, G07, U5-G4)

- **Files.** `crates/mllm-controller/src/remote_execution.rs`, `agent_sessions.rs`
  (`:461`), `runtime.rs`, store ownership reads.
- **Design.** SPEC §13.2: reconcile before dispatch. At start every remote instance
  binding is closed and `reconciling`; on each reconciled session the server issues
  Inspect, then Probe per retained launch; success reopens under a new ingress
  generation; failure stays charged and closed until cleanup evidence (D8). `eligible`
  derives from a live reconciled session, no revocation and no drain (I2 reads it).
- **Tests.** T33, T34, T38. **Unlocks.** M42, M44, M68. **Conflicts.**
  `remote_execution.rs` with W4, WE3; `agent_sessions.rs` with W8, W13; `runtime.rs` with
  W11.

### W13. Remote engine exit detection (G08)

- **Files.** `crates/mllm-agent/src/native_execution.rs` (process-group watch),
  `journal.rs`; `crates/mllm-controller/src/agent_sessions.rs`,
  `coordinator/native_failure.rs`.
- **Design.** Watch each owned process group by PID and start ticks, journal the exit,
  send `member_exit`. The controller closes that instance's admission and marks it failed
  with its reservation retained until Terminate returns absence evidence (SPEC §13.2,
  ADR 0011); other instances keep serving. No restart beyond the attempt budget.
- **Tests.** T20, T33. **Unlocks.** M36, M37. **Conflicts.** `native_execution.rs` with
  W4, WE3; `agent_sessions.rs` with W8, W12.

### W14. Development-control exposure in status (P4, W1 follow-up)

- **Files.** `crates/mllm-store/src/snapshot.rs`, `crates/mllm-management/src/hosts.rs`,
  `crates/mllm-cli/src/output.rs`.
- **Design.** SPEC §9.1 and T21: `status` and `inspect` mark every deployment, instance
  and host installation whose launch enables vLLM development mode (deep park on, vLLM,
  parking residency), with the mitigations in force (loopback, per-launch key guard, no
  ingress or router path). Derived from the effective configuration, never declared.
- **Tests.** T21 status surface, T14. **Unlocks.** M08 evidence; park rows run only after
  M08 passes live (P4). **Conflicts.** `snapshot.rs` with I1.

## 3. File-conflict map

| File | Units (in required order) |
|---|---|
| `mllm-config` `schema.rs`, `effective.rs`, `effective/*` | WE1 → I1 |
| `mllm-config/src/engine_policy.rs`, `mllm-domain/src/launch.rs` | WE1 |
| `mllm-adapters` `sglang/*`, `vllm/args.rs`, `vllm/frozen.rs`; `runtime/sglang_*.py` | WE1 → WE2 |
| `mllm-adapters` `vllm/adapter.rs`, `vllm/http.rs` | W4 |
| `runtime/checkpoint_*.py` (deletion) | WE2 → WE3 |
| `mllm-agent/src/native_execution.rs` | W0 → W4 → WE3 → W13 |
| `mllm-protocol` proto, `execution.rs` | W0 → W3 → WE3 |
| `mllm-controller/src/remote_execution.rs` | W0 → W4 → WE3 → W12 |
| `mllm-controller/src/agent_sessions.rs` | W0 → W8 → W12 → W13 |
| `mllm-controller/src/runtime.rs` | W11 → W12 |
| `mllm-controller/src/coordinator_port.rs`, `coordinator/worker.rs` | W0 → I2 → W5 → W10 → W6 |
| `mllm-management/src/configuration.rs` | WE3 → I2 |
| `mllm-management/src/actions.rs` | I1 (reads) → W5 → W6 |
| `mllm-store` schema, migrations, `deployments.rs` | I1 → W6 |
| `mllm-store` ordinary lifecycle, residency, ledger | W0 → I1 → W5 → W6 |
| `mllm-store/src/snapshot.rs` | W14 → I1 |
| `mllm-scheduler/src/sequence.rs` | W5 → W10 |
| `mllm-router/src/lib.rs` | W11 → I3 → W10 |
| `mllm-router/src/chat.rs` | I3 → W10 |
| `mllm-cli/src/remote_roles.rs` | W11 → I3 |
| `mllm-cli/src/standalone_config*` | W1 → WE1 |
| `mllm-agent/src/session.rs`, `ingress.rs` | W0 → W11 → W8 |

## 4. Waves and interleaved live phases

| Wave | Units in parallel | Starts when | Live phase after the wave |
|---|---|---|---|
| 1 | W0, W1, W3 done locally; W2 ongoing | — | **Phase A** (current binary, SGLang q4, restart-only): M01–M04, M07–M10, M12, M14, M15, M17, M20–M24, M26, M38, M43, M45, M46. **W0 live verification** in progress |
| 2 | WE1, W4, W11, W14 | Now | **Phase B** (remote vLLM, W0 recovery): M05, M06, M11, M13, M16 (vLLM q4), M18, M19, M25, M39–M41, M47; M08 with W14 status |
| 3 | I1, WE2, WE3, W8 | WE1 merged (I1, WE2); W4 merged (WE3); W11 merged (W8) | **Phase A2** (E1): M16 for every model on both engines; measured peaks set P2 margins and recompute matrix budgets |
| 4 | I2, W12 | I1 and WE3 merged (I2); W8 merged (W12) | **Phase F part 1**: M42, M44, M68 |
| 5 | W5, I3 | I2 merged (W5); I1, W8 merged (I3) | **Phase C** (park and wake, only after M08 passed live): M28–M30, M32–M34; **Phase E** (instances): M54–M60, M62, M63 as recast |
| 6 | W10, W13 | W5 and I3 merged (W10); W12 merged (W13) | **Phase D** (switching): M27, M31, M35, M51–M53; **Phase F part 2**: M36, M37, M69–M71 |
| 7 | W6 | W10 merged | **Phase F part 3**: M64–M67, M72; then **Phase G**: M48 soak, M49, M50 |

Units in one wave touch disjoint files and may run as parallel agents in separate
worktrees. Wave 2 needs no instance or E1 work, so remote vLLM and recovery rows proceed
on the current configuration shape. Live phases use the release binary built on both
Sparks from the merged tree, record E0 and I1, and update the runbook; a failing row
reopens its unit.

Matrix rows written for separate replica deployments (M54–M63, M65) need recasting to
one deployment with `instances: 2`; the matrix document is edited when I1 lands. M55
becomes a deploy-time refusal of a second deployment on the same route. M61 waits for
Q6.

## 5. Risks and open owner questions

Answered on 2026-09-22 and folded in: former Q1 allocations (P2: declared or derived
request, then measured), Q2 replica declaration (P1: instances), Q3 shutdown (P3).

- **Q4 anchor support (G17).** E1 makes the NVFP4/FP8 hybrid multimodal anchor deployable
  with typed quantization and `language_model_only`; whether the pinned engine builds
  serve it is found in Phase A2. Engine upgrades still need approval. Recommendation:
  run it first; on failure fall back to BF16 `qwen3.8-27b` and record the failure.
- **Q5 on-demand width.** With no READY instance, activate one instance or all N?
  Recommendation: one, dispatching as soon as it is READY; the rest only where they fit
  without eviction; explicit `start` targets N.
- **Q6 mixed-engine replicas (M61).** One deployment has one installation name, so vLLM
  plus SGLang behind one route is not an instance set. Options: recast as two
  deployments on two routes; drop M61; allow per-host installation override (contradicts
  SPEC §1.2). Recommendation: recast.
- **Q7 per-instance actions (M62, M65).** Options: count change and `drain host` only; an
  instance verb. Recommendation: no instance verb now.
- **Q8 non-count revisions.** Stop-all-then-start drops the route to zero READY instances
  during a model or configuration change; rolling replacement needs the SPEC §19 update
  design. Recommendation: stop-all now, rolling later.
- **Q9 checkpoint re-verification cost.** Full rehash at every launch and wake (about a
  minute per large model) versus stat identity plus small-file rehash with a full hash
  on first placement and on any change. Recommendation: the second.
- **Q10 extra-argument flag.** Deployment `accept_extra_args: true` plus host
  `security.extra_args` defaulting to `allowed`, versus defaulting to `denied`.
  Recommendation: default allowed for ordinary arguments; sensitive options always need
  the host's named approval.
- **Q11 vLLM launch wrapper.** Enforcing reserved flags after vLLM's own parser needs a new
  protected entry `runtime/vllm_entry.py`; the alternative is the Rust denylist only,
  which argument abbreviations and configuration files can bypass. Recommendation: the
  wrapper.
- **Default-on deep parking** widens exposure of vLLM development controls; W1
  controls are mandatory, W14 marks them, and M08 must pass before Phase C (P4).
- **Drift.** Metric names and reserved ServerArgs names are pinned to recorded engine
  sources and fail closed when missing (`scrape_ok = false`, `effective_args_mismatch`).

---
title: Two-Host Single-Rank Engine Matrix - Plan
type: test-plan
date: 2026-09-22
status: owner decisions recorded 2026-09-22; no matrix row has been run or passed
---

# Two-Host Single-Rank Engine Matrix

## 1. Purpose

The owner directed on 2026-09-22 that, before any two-rank (TP2) work, mllm must prove
a two-host control plane using single-rank recipes: one server on control-host controlling
host-a and host-b, running several vLLM and SGLang deployments of four distinct
models on each host, serving them singly and as cross-host replicas of one route,
switching automatically on demand, parking and waking, recovering from failures, and
deleting deployments and shutting down cleanly. This document maps those scenarios into a
numbered matrix (M01–M80), records what the code supports on the remote path, and lists
the owner decisions that govern it. The build order is in
`docs/plans/2026-09-22-two-host-control-plane-plan.md`.

Authority order is unchanged: `docs/SPEC.md`, then
`docs/design/milestones/f2-sglang-design.md`, then the plans. Execution status belongs
only in `docs/runbooks/f2-current-status.md`; this file holds no results. Every row is
"not run". CPU and Fake-engine tests are not evidence for any row.

The only live result that precedes the matrix is the U5 gate: remote single-host SGLang
(qwen3-4b-instruct) passed on host-a on 2026-09-22 with init, enrollment, deploy,
activation, routed inference, stop and verified cleanup over three generations. Its
evidence is in `target/live/u5-remote-0292/` (`run1/`, `run2/`). It is a precondition,
not a pass of any M row, because it used one model and predates the matrix harness.

## 2. Decisions (owner, 2026-09-22)

| ID | Decision | Effect on this matrix |
|---|---|---|
| D1 | Deep parking is enabled by default; a host opts out. SPEC §9.1 and T21, which say opt-in, are amended by ADR 0012 (plan unit W1) | Deep rows need no opt-in; M34 tests the opt-out; G16 |
| D2 | Remote vLLM is built now | Implemented locally (`crates/mllm-agent/src/native_execution/vllm.rs`, `crates/mllm-adapters/src/vllm/frozen.rs`); live-unproven; G01 |
| D3 | Park and wake are built once, through the coordinator, with remote Park/Restore member actions | One engine-generic contract; G02–G04 |
| D4 | Automatic request-driven switching is required now | M27, M31, M51–M53 expect automatic switching; operator switching (M24, M25) remains a baseline; G05 |
| D5 | Models: qwen3.8-27b NVFP4 (anchor), qwen3-30b-a3b, qwen3-14b, qwen3-4b-instruct, on both hosts | Switching rows check distinct model identity (I1); G17 |
| D6 | Fault injection: signals to mllm-owned processes and one bounded external memory allocation; no firewall, interface or reboot changes | M21, M36, M37, M39, M59 are authorized |
| D7 | "Host restart" means restarting the host agent process; reboot recovery stays untested | M40, M60, M69 |
| D8 | A server restart fully re-attaches live remote engines through fresh probes; failures stay charged and closed | M42, M44, M68 expect re-attach; G07 |
| D9 | One route may be served by replicas on both hosts. The router balances on its own in-flight counts plus engine Prometheus metrics that each host agent scrapes on loopback and reports over the control session about every second; it fails over on host loss. The request path stays layered: controller (router) → host agent → engines | Tier 5 (M54–M63) is in scope; G13, G14 |
| D10 | Normal per-host budget: 80% managed limit, 10% free reserve. Tight budget admits exactly one of the two largest models | §3 budgets; M20, M27, M31, M46, M51–M53 |
| D11 | Undeploy (renamed `delete deployment` by owner decision 2026-09-23) and graceful role shutdown (server, host, standalone SIGTERM) are built now | Tier 6 (M64–M72); G10, G15 |
| — | TP2 and every multi-rank group stay parked until this matrix passes | Excluded (§2.2) |

### 2.1 In scope

The shipped `mllm start server` on control-host and `mllm start host` on both Sparks;
enrollment; remote single-rank managed deployments of vLLM and SGLang for the four D5
models; routed inference by route name through control-host only; cross-host replicas with load
balancing and failover; lifecycle actions (deploy, start, stop, park, preinitialize,
delete) through the CLI and management API; co-residency on one unified-memory GB10;
automatic and operator switching with model-identity checks; deep parking and waking;
failure and recovery including server re-attach; graceful role shutdown; verified cleanup.

### 2.2 Excluded, with reasons

- **TP2 and every multi-rank group** (two-Spark plan U6, U7, and the group half of U8/U9).
- **Host reboot**, forbidden by AGENTS.md (D7).
- **Network partition by firewall or interface change** (D6). Disconnect is simulated by
  SIGSTOP or kill of the agent process.
- **`host_backed` parking and vLLM sleep level 1.** On one unified pool they free
  nothing; ADR 0010 refuses `host_backed` at configuration time. Only M34 covers the refusal.
- **Cross-host switching** (park A on host-a to wake B on host-b). Capacity on one host is
  not freed by the other; M35 covers the two independent operations. Replica failover
  (M58) is routing, not switching.
- **Attached services (T11), retained or shared KV caches (T24, T25, T28, T35, T36),
  numerical default replay (T39), performance comparison (T40).** Latency and balance
  distributions are recorded as evidence but not compared against a baseline.
- **Driver, engine-environment or system-package changes** (AGENTS.md).

## 3. Environment

| Item | Value |
|---|---|
| Control plane | control-host: `mllm start server` (GPU-free), management `127.0.0.1:7443`, inference `127.0.0.1:8443`, bootstrap and control listeners on Tailscale |
| Hosts | host-a, host-b: one NVIDIA GB10 each, unified memory, reported capacity 130,663,170,048 B, aarch64, driver 580.173.02 |
| Host role | `mllm start host`, `ingress.transport: trusted_private_link` on a Tailscale 100.64/10 address |
| SGLang | 0.5.20, source commit `94602c9c2b7cbdb8efd5c52802dac6a1c180089e`, `~/mllm-sglang-0.5.20-venv`, byte parity recorded 2026-09-22 |
| vLLM | 0.29 venv (`~/mllm-vllm-venv2` on host-a); host-b parity not yet recorded (G12) |
| Models | `~/models/{qwen3-4b-instruct, qwen3-14b, qwen3-30b-a3b, qwen3.8-27b-nvfp4}`, identical payload SHA-256 on both hosts (runbook, 2026-09-22). The anchor is a hybrid multimodal `Qwen3_5ForConditionalGeneration` NVFP4/FP8 build, 23.75 GB on disk; no engine has served it yet (G17) |
| Deep park | Default on per D1 after W1; until W1 lands, host policies set `deep_park: enabled` explicitly |

**Budgets (D10), per host.** Capacity C = 130,663,170,048 B (121.69 GiB).

| Policy | `managed_limit` | `free_reserve` | `max_parked` |
|---|---|---|---|
| Normal | 104,530,536,038 B (⌊0.8·C⌋, 97.35 GiB) | 13,066,317,005 B (⌈0.1·C⌉, 12.17 GiB) | 2 |
| Tight | 83,751,862,272 B (78 GiB) | 13,066,317,005 B | 1 |

Proposed per-model declared allocations (owner question Q1 in the plan; measured
footprints may change them, and the tight limit is then recomputed by the same rule):
q4 16 GiB (17,179,869,184 B), q14 40 GiB (42,949,672,960 B), q27 40 GiB
(42,949,672,960 B), q30 72 GiB (77,309,411,328 B). Engine sizing follows the allocation:
SGLang `mem_fraction_static` = allocation / C; vLLM KV bytes = allocation − measured
weights − measured runtime overhead. Under the normal policy q30 + q4 (88 GiB) and
q27 + q14 + q4 (96 GiB, 103,079,215,104 B) fit; q30 + q14 (112 GiB) does not. The tight
limit admits q30 alone, or one of q27/q14 with q4, and never two of {q30, q27, q14}.

**Fixtures.** Name `<engine><host>-<model>`: engine `v`/`s`, host `92`/`17`, model
`4`/`14`/`27`/`30`, e.g. `s92-14`. A standalone fixture's route is its name. Replica
routes use the model name (`qwen3-4b`, `qwen3-14b`, `qwen3.8-27b`, `qwen3-30b-a3b`) and
bind two fixtures.

**E0, base evidence for every row** (live only, `target/live/<ts>-matrix/<Mxx>/`,
non-secret): mllm commit and dirty-tree digest; release-binary SHA-256 on each machine;
engine source identity; checkpoint digests; host IDs and policy fingerprints; deployment
id, revision, generation, operation id; management status before and after; per-host
owned process identities (pid, boot id, start ticks) from the agent; resource-ledger
snapshot; per-host `MemAvailable` and `nvidia-smi` compute processes before and after;
every routed request and response (status, `model`, answering deployment and host,
answer marker); timestamps. HTTP 200 alone is not evidence.

**I1, model-identity evidence** (every row that serves more than one model or replica):
the answering deployment's checkpoint digest from agent launch evidence equals the
expected model's; and a fixed greedy probe (temperature 0, 32 tokens, top-1 logprobs)
matches that model's golden, captured at its first Ready in the run. Goldens of the four
models must differ pairwise, or the row is void.

## 4. Capability gap table (remote path, 2026-09-22 working tree)

Line numbers drift while W0 is in progress; the plan owns the fixes.

| ID | Capability | Code today | Needed | Unit |
|---|---|---|---|---|
| G01 | Remote native launch | Agent dispatches SGLang and vLLM (`crates/mllm-agent/src/native_execution.rs:142`, `:217-221`, `native_execution/vllm.rs`) | Live proof (M05, M11) | done locally |
| G02 | Park/Restore protocol | `MemberAction` has Launch, Inspect, Terminate, CloseIngress, Probe; no Park/Restore (`crates/mllm-protocol/src/execution.rs:75-86`; proto `ExecuteMember` oneof `management.proto:199-211`); agent authorizes none (`native_execution.rs:598-645`); `RemoteEngine::execute_persisted` accepts Initialize only (`crates/mllm-controller/src/remote_execution.rs:102`), park/restore return `UnsupportedCapability` (`:182-211`) | Typed Park/Restore with agent authorization, journal, released/restored evidence | W3 |
| G03 | Park, wake, preinitialize in the coordinator | `request_transition` refuses Park (`crates/mllm-controller/src/coordinator_port.rs:477-479`); management maps Park, Preinitialize, Undeploy to `Unsupported` (`crates/mllm-management/src/actions.rs:127`) | Operations per `crates/mllm-domain/src/park.rs` with parked accounting and `max_parked` | W4 |
| G04 | vLLM persisted park | `VllmAdapter::execute_persisted` handles Initialize only (`crates/mllm-adapters/src/vllm/adapter.rs:214-221`); SGLang persisted Park/Restore exist (`crates/mllm-adapters/src/sglang/adapter.rs:397-428`) | vLLM level-2 sleep, wake, `reload_weights`, KV wake on the persisted path | W3 |
| G05 | Automatic switching wiring | `SwitchEngine` (`crates/mllm-router/src/switch.rs:122`) is never constructed; server wires only `WakeJoin` (`crates/mllm-cli/src/remote_roles.rs:292`) and `auto_activate` (`crates/mllm-router/src/chat.rs:61`); nothing releases an incumbent | SPEC §10 steps 1–8 planner in the coordinator, reached from the router | W9 |
| G06 | Readiness after agent reconnect or restart | `MemberAction::Probe` exists (`execution.rs:82-85`); controller-driven re-probe is being built (U5-G2) | Ready only after a fresh probe; 500 at the gate never coexists with Ready | W0 |
| G07 | Server restart re-attach (D8) | Not implemented | Rebuild bindings from the store, Inspect + Probe each retained launch, reopen or keep charged and closed | W11 |
| G08 | Remote engine exit detection | No agent-originated exit report (`management.proto:42-51`) | Exit reported as evidence; admission closed; reservation kept until cleanup | W12 |
| G09 | U5 remote lifecycle gate | Passed live on host-a, SGLang, one model (`target/live/u5-remote-0292/`) | Rerun inside M01 with I1 | — |
| G10 | Undeploy (now `delete deployment`, 2026-09-23) | CLI parses `undeploy model` (`crates/mllm-cli/src/grammar.rs:210`, `:376`); management returns `Unsupported` (`actions.rs:127`) | Drain, stop, verified cleanup, then route and deployment removal (SPEC §6.3) | W5 |
| G11 | Co-residency admission | Ledger admission at arm plus agent `MemAvailable` check against allocation and free reserve (`native_execution.rs:629`) | Live proof under §3 budgets | W2 |
| G12 | host-b vLLM parity | Not recorded | Read-only wheel and RECORD digest comparison | W2 |
| G13 | Replica routing and admission | A route is the primary key of `deployment_routes` (`crates/mllm-store/src/schema.rs:185-186`); `find_deployment_by_route` returns one row (`crates/mllm-store/src/deployments.rs:640`); router resolves to one deployment (`chat.rs:21-37`) | Route to replica set; replicas must share checkpoint digest and served model id; selection excludes closed or stale replicas; failover before dispatch only | W6, W8 |
| G14 | Load-balancing metrics plumbing | Router counts in-flight per deployment (`crates/mllm-router/src/admission.rs:22-44`); agent ingress counts per scope (`crates/mllm-agent/src/ingress.rs:45`, `:177`); no engine scrape; no load message in `AgentToServer` (`management.proto:42-51`); SGLang metrics off (`runtime/sglang_server_args.py:110`) | Agent scrapes engine `/metrics` on loopback about every 1 s and reports load with generation and sample time; server keeps it in memory with a staleness bound | W7 |
| G15 | Graceful role shutdown | Server and host catch SIGTERM but drop listeners without draining (`remote_roles.rs:364-373`, `:459`); standalone has no handler (`crates/mllm-cli/src/roles.rs`); no role-stop verb | Bounded drain, gate closure, journal flush; engines retained on ordinary shutdown (SPEC §4.3); explicit terminate mode | W10 |
| G16 | Deep-park default flip (D1) | `DeepPark` defaults to `Disabled` (`crates/mllm-config/src/effective.rs:283-286`); parking residency refused when disabled (`crates/mllm-config/src/effective/core.rs:102-106`); standalone requires `MLLM_DEEP_PARK=on` (`roles.rs:289`); SPEC §9.1 and T21 say opt-in | Default enabled, explicit opt-out, ADR 0012, SPEC amendment | W1 |
| G17 | Model recipes for D5 | Only qwen3-4b has run; the anchor's architecture and NVFP4/FP8 scheme are unproven on SGLang 0.5.20 and vLLM 0.29 | Per-model recipes and allocations; a live smoke per model and engine | W2 |
| U5-G1 | Uncertain remote launch cannot be stopped or settled | In progress (W0) | Stop settles uncertainty only on evidence | W0 |
| U5-G2 | Host restart leaves controller Ready while the gate returns 500 | In progress (W0) | Covered by G06 | W0 |
| U5-G3 | Status shows `stopped` while an uncertain engine runs | In progress (W0) | Status reflects uncertainty | W0 |
| U5-G4 | Host `eligible` hard-coded false (`crates/mllm-controller/src/agent_sessions.rs:461`) | Open | Derived from session, reconciliation and revocation | W11 |

Headline: remote SGLang Initialize, routed inference and Stop are live-proven for one
model. Remote vLLM is built but unproven. Park, wake, preinitialize, undeploy, automatic
switching, replicas and graceful shutdown exist on no path.

## 5. Matrix

Columns: **Setup → Action**; **Expected**; **Evidence** beyond E0 (and I1 where marked);
**T** = SPEC §20 IDs; **Gaps**. G09 precedes every row.

### Tier 0 — single host, single engine (host-a first)

| ID | Setup → Action | Expected | Evidence | T | Gaps |
|---|---|---|---|---|---|
| M01 | Server + host-a enrolled; deploy `s92-4` `--activate --wait`; one completion | Ready; routed 200 with correct answer; one owned process group | Ownership record, ingress scope and generation, I1 | T08, T37 | — |
| M02 | `s92-4` Ready; 4 streaming + 4 non-streaming concurrent | All complete; SSE well-formed; ingress in-flight returns to 0 | Per-request timing, final counters | T19, T37 | — |
| M03 | `s92-4` Ready; `mllm stop`; request to its route; `mllm start` | Stop proves absence; no autoactivation; start yields generation n+1 and Ready | Cleanup receipt, post-stop `nvidia-smi` | T10 | — |
| M04 | Deploy `s92-14` without `--wait`; kill the CLI; status from a new client; resubmit same request id | One deployment, one launch | Operation id equality | T08, T09 | G17 |
| M05 | Deploy `v92-4` `--activate --wait`; one completion | vLLM Ready remotely; guard refuses unkeyed control routes | As M01 for vLLM | T08, T37 | G01 |
| M06 | `v92-4` Ready; streaming burst; stop; start | As M02 + M03 for vLLM | As M02/M03 | T10, T12 | G01 |
| M07 | After M03, replay a captured ingress request with the old gate token and generation | Rejected at ingress; no engine hit | Ingress rejection, engine log unchanged | T18, T34 | — |
| M08 | `s92-4` and `v92-4` Ready; call admin, native and `/metrics` paths through ingress; probe engine ports from control-host | Only inference paths forwarded; engine ports loopback-only | 403/404 set, socket list | T37 | — |
| M09 | Deploy with an unknown profile, then a mismatched checkpoint digest | Preflight rejects before effects; ledger unchanged | Error category | T07, T14 | — |

### Tier 1 — both hosts, each engine, each model

| ID | Setup → Action | Expected | Evidence | T | Gaps |
|---|---|---|---|---|---|
| M10 | Enroll host-b; M01–M03 with `s17-4` | First native SGLang on host-b | E0 on host-b | T05, T08, T10 | — |
| M11 | M05–M06 with `v17-4` | First native vLLM on host-b | Venv parity record | T08, T10 | G01, G12 |
| M12 | `s92-4` and `s17-14` Ready; interleaved requests | Each route answered by its own host and model | Per-host ingress counters, I1 | T37 | G17 |
| M13 | `v92-14` + `s17-27`, then `s92-30` + `v17-4` | Mixed engines and models across hosts | As M12 | T22 | G01, G17 |
| M14 | `s17-4` deployed stopped (on demand); 8 simultaneous requests | One activation, one process, all served | Joined operation id | T15 | — |
| M15 | Both hosts serving; stop `s92-4` | host-b unaffected | host-b identity unchanged | T33 | — |
| M16 | Model smoke: each of q14, q27, q30 on each engine on host-a, then host-b | Ready and correct, or a recorded engine incompatibility (not a pass) | Per-model golden captured, load time | T08, T22 | G17 |

### Tier 2 — co-residency on one host (normal budget unless noted)

| ID | Setup → Action | Expected | Evidence | T | Gaps |
|---|---|---|---|---|---|
| M17 | `s92-14` + `s92-4`; overlapping inference | Both Ready; distinct ports, keys, routes | Two reservations within limit, I1 | T26, T27 | G11 |
| M18 | `v92-27` + `v92-4` | As M17 for vLLM | As M17 | T26, T27 | G01, G11, G17 |
| M19 | `v92-30` + `s92-4`; overlapping inference | Mixed engines coexist on one GB10 | Per-engine memory attribution | T22, T26 | G01, G11 |
| M20 | Tight budget; `s92-30` Ready; start `s92-14` operator-driven | Denied at admission with a capacity diagnostic; no spawn; `s92-30` unaffected | Blocked reason, ledger unchanged | T23, T29 | — |
| M21 | Ledger fits but `MemAvailable` short (bounded external allocation, D6) | Agent rejects the stale plan; no spawn; no leaked reservation | Agent rejection, `MemAvailable` sample | T29 | — |
| M22 | M17 state; stop `s92-14` | `s92-4` keeps serving; only the stopped reservation released after absence proof | Ledger delta | T16 | — |
| M23 | M17 state; send `s92-14`'s key and route to `s92-4`'s scope | Ingress rejects cross-deployment credentials and model mismatch | Ingress rejection | T37 | — |

### Tier 3 — switching, park and wake

| ID | Setup → Action | Expected | Evidence | T | Gaps |
|---|---|---|---|---|---|
| M24 | Tight; operator: `s92-14` Ready → stop → start `s92-30` → stop → start `s92-14` | Restart-only A→B→A; absence proof precedes each arm | Ordered events, I1 at each step | T16 | — |
| M25 | As M24 cross-engine: `v92-14` → `s92-30` → `v92-14` | Restart-only cross-engine switch | As M24 | T16, T22 | G01 |
| M26 | Normal; `s92-4` on demand; request while `s92-14` serves | Auto-activation; `s92-14` untouched | Joined operation, I1 | T10, T15 | — |
| M27 | Tight; `s92-14` Ready (restart_only); request to on-demand `s92-30` | Automatic: admission to `s92-14` closes, drains, stops with absence proof; `s92-30` served within deadline | Step order per SPEC §10, I1 | T15, T19, T23 | G05 |
| M28 | `s92-14` (deep) Ready → park → request → wake | Parked accounting; same process identity; correct answer | Identity unchanged, release and restore evidence, phase timings, I1 | T16, T20, T22 | G02, G03 |
| M29 | `v92-14` (deep, sleep level 2) Ready → park → wake | Sleep, wake, reload weights, prefix reset; same process | As M28; guard policy recorded | T16, T21 | G02–G04 |
| M30 | Park `s92-14` during a long stream | Park waits for quiescence or fails bounded; no replay | Stream completion order | T17 | G02, G03 |
| M31 | Tight, deep; requests alternate `v92-14` ↔ `s92-30`, three cycles | Automatic warm switching; no cold init after first; release verified before increase | Per-step ledger, identities, I1 | T16, T22, T23 | G02–G05 |
| M32 | Normal; `s92-14` and `s92-27` both parked; wake one, then the other; park a third | Both-parked accounting; `max_parked` enforced | Ledger per state | T23, T26 | G02, G03 |
| M33 | Preinitialize `s92-14`, `v92-27` | Each starts, verifies, parks before the next | Ordered events | T16 | G02, G03 |
| M34 | Park a `restart_only` deployment; `deep` on a host that opted out; `host_backed` on the unified domain | Park fails clearly; deep refused at resolution; `host_backed` refused at configuration | Error categories, no sleep call | T21 | G03, G16 |
| M35 | host-a switches `s92-14`→`s92-30` while host-b switches `v17-30`→`s17-14` | Independent per-host plans; no double charge | Both ledgers, event order | T27 | G05 |
| M51 | Tight; `s92-14` serving a burst; request to `v92-30` | Fairness window bounded; `s92-14` drained, not killed; queued work served after | Window timing, no fake tokens | T17, T19 | G05 |
| M52 | Tight; requests for q30, q14 and q27 arrive together on host-a | Oldest waiting group first; per-group order kept; no thrash | Queue order, switch count | T19 | G05 |
| M53 | Tight; switch target fails to initialize (bad recipe) | Switch fails closed; incumbent not restarted silently; accounting retained | Failure category, ledger | T20, T30 | G05 |

### Tier 4 — failures and recovery

| ID | Setup → Action | Expected | Evidence | T | Gaps |
|---|---|---|---|---|---|
| M36 | `s92-14` Ready; SIGKILL the owned engine | Routing closes; reservation retained until absence evidence; fresh start works | Detection path and time, ledger | T20, T33 | G08 |
| M37 | Same for `v17-4` | As M36 | As M36 | T33 | G01, G08 |
| M38 | Bad model path (an empty directory in the model store), then an engine that exits at once (an extra argument the engine's own parser rejects), then the sound recipe on the same host (`rows/M38.sh`, per fixture) | Each failure closed, never uncertain; bounded retries; no leftovers; the sound recipe then Ready and correct, with verified cleanup | Retry count, absence proof, operation outcome, recovery answer | T20, T30 | — |
| M39 | `s17-4` Ready; SIGSTOP the host-b agent past the lease; SIGCONT | Gate closes; reservation charged; after resume readiness is re-proven by a fresh probe | Session events, probe record | T13, T32 | G06 |
| M40 | `s92-4` Ready; kill and restart the host-a agent | Journal reconciles; no duplicate launch; Ready only after fresh probe; never Ready with a 500 gate | Identity equality, probe record | T33 | G06 |
| M41 | Kill the agent between spawn and result | One engine; uncertainty settles on evidence and stop works | Single identity, settlement record | T09, T34 | G06, U5-G1 |
| M42 | Engines Ready on both hosts; kill and restart the server | Full re-attach by Inspect + fresh probe; no false Ready in between; failures stay charged and closed | Post-restart status, identities unchanged | T33, T38 | G07 |
| M43 | Kill the server during a stream | Honest client failure; no replay after restart | Client transcript | T38 | — |
| M44 | After M42, replay old-session commands and gate tokens | Rejected; ambiguous effects reconciled | Rejection records | T34 | G07 |
| M45 | Revoke host-b while `s17-4` Ready | Session closed; forwarding refused; ownership retained; host-a unaffected | Revocation event | T06, T37 | — |
| M46 | Tight; activate `s92-30` and `s92-14` simultaneously (operator) | Exactly one arms; the other denied or queued; no double charge | Single arm event | T15, T27 | — |
| M47 | Stop `s92-14` during Initialize | Cleanup verified; release only after absence proof | Event order | T30 | U5-G1 |

### Tier 5 — replicas and load balancing (D9)

| ID | Setup → Action | Expected | Evidence | T | Gaps |
|---|---|---|---|---|---|
| M54 | Route `qwen3-14b` bound to `v92-14` + `v17-14`; 20 sequential requests | Both replicas serve; correct model on each | Per-replica counts, I1 per replica | T37 | G13 |
| M55 | Deploy a replica of `qwen3-14b` with q4's checkpoint | Rejected at admission (no silent model substitution, SPEC §10) | Error category | T14 | G13 |
| M56 | M54; 64 concurrent short requests | Balanced: neither replica above 60% of requests; router in-flight never exceeds limits | Selection log with inputs (router in-flight, reported running and waiting, sample age) | T19 | G13, G14 |
| M57 | M54; 8 long-prompt streams (≥16k tokens) pinned by timing to `v17-14`, then 32 short requests | Short requests skew to `v92-14` in proportion to reported load | Load samples, selection log | T19 | G14 |
| M58 | M56 under load; SIGSTOP the host-b agent past the lease | New requests go to `v92-14` only; accepted host-b requests fail honestly, never replayed; host-b reservation retained | Failover time, client transcripts | T32, T38 | G13, G14 |
| M59 | M56 under load; SIGSTOP the `v17-14` engine for 10 s, then SIGCONT (D6) | Scrapes fail, samples go stale; after the staleness bound new work steers to `v92-14` on in-flight counts; stalled requests finish after SIGCONT or fail within deadline; no replay | Sample ages, selection inputs, transcripts | T19, T38 | G14 |
| M60 | After M58, SIGCONT or restart the agent | Replica rejoins only after a fresh probe; traffic returns | Rejoin time, probe record | T13, T33 | G06, G13 |
| M61 | Mixed-engine replicas: route `qwen3-4b` bound to `v92-4` + `s17-4`; concurrent load | Both serve; I1 matches the same checkpoint on both; balance as M56 | Per-engine metrics mapping | T22 | G13, G14 |
| M62 | M54; operator stops `v17-14`, later starts it | Route keeps serving on `v92-14`; rejoin after Ready | Route availability | T10 | G13 |
| M63 | Both replicas of `qwen3-4b` stopped on demand; 8 simultaneous requests | One activation of one replica (least-loaded host with capacity); no double activation | Joined operation | T15 | G13 |

### Tier 6 — deployment deletion and graceful shutdown (D11)

Owner decision 2026-09-23: the command is `mllm delete deployment <name|id>`. Plain, it
is refused with 409 `delete_requires_cleanup` while anything is held; `--stop` stops
every instance, waits for verified cleanup and deletes, durably by `--request-id`.

| ID | Setup → Action | Expected | Evidence | T | Gaps |
|---|---|---|---|---|---|
| M64 | `s92-14` Ready; `mllm delete deployment s92-14` (refused 409), then `mllm delete deployment s92-14 --stop` | Plain delete refused with nothing changed; with `--stop`: drain, stop, absence proof, then route removed from `/v1/models` and the name reusable; checkpoint files untouched | Route list, ledger zero, file digests | T16 | G10 |
| M65 | Remove one replica of `qwen3-14b` under load (count decrease or `drain host`, ADR 0013 Q7) | Route keeps serving on the other; no failed request after the drain begins | Client transcripts | T17 | G10, G13 |
| M66 | `delete deployment --stop` during a long stream | Stream completes or fails within the drain deadline; no premature removal | Order of events | T17 | G10 |
| M67 | `delete deployment s17-4 --stop` while host-b is disconnected; rerun with the same `--request-id` after it reconnects | Reports `cleanup: pending` with the Stop's operation id; deployment and route stay until cleanup evidence; the rerun replays the same Stop and then deletes | Pending state, retained ledger | T09, T32 | G10 |
| M68 | Engines Ready on both hosts; SIGTERM the server; restart | Stops accepting, drains in-flight within its deadline, exits 0; engines retained; restart re-attaches (as M42) | Exit code, drain log, identities unchanged | T33, T38 | G07, G15 |
| M69 | SIGTERM the host-a agent under load; restart | Gates close, in-flight drained, journal flushed, exit 0; engines retained; Ready again only after fresh probe | Exit code, gate events | T33 | G06, G15 |
| M70 | Standalone role with one engine; SIGTERM | Clean exit 0; state records retained engine or stop consistently; next start reconciles without an orphan | Process scan, state directory | T02, T33 | G15 |
| M71 | Explicit terminating shutdown of the host-b host role | Every owned engine drained and stopped with absence proof before exit; server shows them stopped | Cleanup receipts | T16, T33 | G15 |
| M72 | `delete deployment --stop` every deployment, then SIGTERM server and hosts | No owned process, empty ledger, `MemAvailable` at baseline, roles exit 0 | Final scans | T33 | G10, G15 |

### Tier 7 — soak and closure

| ID | Setup → Action | Expected | Evidence | T | Gaps |
|---|---|---|---|---|---|
| M48 | All model fixtures deployed, including two replica routes; seeded random walk (≥200 steps) over host × deployment × {infer, stream, start, stop, park, wake, delete/redeploy, agent restart} within budgets | After every step: ledger within limits; owned processes equal the Ready+Parked set; every response passes I1; no orphan | Step log with seed, per-step invariants | T15, T16, T26, T27 | G01–G15 |
| M49 | Final cleanup (M72 on the soak state) | As M72 | As M72 | T33 | G10, G15 |
| M50 | Repeat M01, M05, M10, M11, M54 on the final binary | Same outcomes | E0 | T08 | — |

### Tier 8 — engine gates through the product (2026-09-23)

The owner decided on 2026-09-22 that the remaining engine tests that bypassed the shipped
product (`crates/mllm-cli/tests/live_vllm.rs`, `live_sglang.rs` and
`scripts/live/run-on-spark.sh`, which drove standalone in process) become matrix rows
driven through the shipped CLI and roles, and are then deleted. Their scenarios map as
follows: L1–L5 and L11 and SGL1–SGL3 are M73; L6–L8 are M38; L9 is M74; L10 is M75 plus
`scripts/check-release-clean.sh`, which `sync.sh build` runs on every binary it builds.
The keyed half of L3 (the engine key reaches the control routes) is not reproduced: the
harness never reads engine secrets, and M28/M29 exercise the keyed routes through the
product's own park.

| ID | Setup → Action | Expected | Evidence | T | Gaps |
|---|---|---|---|---|---|
| M73 | `v92-4` and `s92-4` (one run each, deployment `<fixture>-lc`): deploy `--activate --wait`; plain and streaming completion; router and engine access probes; stop; start; stop; delete | Ready; the api process's recorded argv is the protected entry with the rendered launch settings (SGLang: `--public-settings-json` and a full `GPU-` UUID in `CUDA_VISIBLE_DEVICES`) and no credential; router refuses a keyless caller and has no `/metrics`; engine listens on loopback only, is refused from control-host, answers unkeyed calls with 401; stop leaves no identity, process, port or charge; restart is a higher generation with a new binding and no reused pid; `MemAvailable` drops at Ready and returns within 2 GiB | Argv and environment probe (`proc_probe.py`), socket list, per-generation owned identities and accounting, `mem.txt` | T08, T10, T37 | — |
| M74 | `v92-14` or `s92-14` with `timeouts.initialize: 30s` (deployment `<fixture>-it30`); a variant declaring it above the request deadline; `start --initialize-timeout 2h`; plain start; `start --initialize-timeout 20m` | Over-deadline declaration refused at deploy; over-deadline override refused before sending; effective config shows 30 000 ms `declared`; the plain start fails at the declared deadline and the deployment reads closed, never uncertain, with nothing left on the host; the override start is Ready and correct, then stops clean | Effective timeouts, operation outcome, closure accounting, host scan | T14, T20 | — |
| M75 | On each Spark, the snapshot release binary runs `start standalone` in a fresh state directory with every engine variable removed | Refused at once with `invalid_config` naming no engine installation; nothing spawned or listening. Its other half: `check-release-clean.sh` passes on the control-host and Spark builds | Exit code and structured error, host scan, build log | T02 | — |

### Tier 9 — performance benchmark (owner request 2026-09-23)

M80 measures what users feel, through the shipped router, for every model on both
engines and both hosts (`rows/M80.sh`, one fixture per run: `run_row.sh M80 --tag
<fixture> -- <fixture> [<switch-from fixture>]`). It records distributions and workload
context, not a pass threshold: SPEC §17 promises no Spark wake time, throughput or
speedup, and no M80 figure becomes one. `bench_report.py` folds every `M80-*` directory
into one model × engine × host table.
Path overhead is separated through the product (owner decision 2026-09-23): the server's
mllm latency view holds the router's per-request phases, the host ingress's own times and
the engine histograms the host agent forwards (`source: engine`, vLLM and SGLang), and the
row windows it per cell.

| ID | Setup → Action | Expected | Evidence | T | Gaps |
|---|---|---|---|---|---|
| M80 | Host idle. **cold**: three times, deploy `<fixture>-c<i>` on demand, one routed request, stop with verified cleanup, delete. **bench**: `<fixture>-bn` Ready; for prompts of 128, 2048 and 8192 tokens (capped to the context) × concurrency 1, 4 and 16, a warmup (excluded) then N streaming requests per worker, `max_tokens` 256, temperature 0, `ignore_eos` when accepted; engine `/metrics` scraped on loopback before and after each cell. **park** (deep only): three times park, then one request. **switch** (with a second fixture on the same host under a policy where the two do not co-fit): three times request B while A serves, then A again. Stop and delete everything | Every request answered; per cell TTFT, TTLT, prefill and decode rates, ITL p50/p95/p99 and throughput recorded; lifecycle times to first token recorded for cold start, wake and switch; the incumbent yielded at each switch; verified cleanup after each phase and an idle host at the end | `bench.json`, `summary.md`, `cells/*.json`, `records.jsonl` (per-chunk timestamps and `usage`), `lifecycle.jsonl`, `metrics/*.prom`, `netfloor.json`, `pagecache.txt`, `switch-states.txt`, E0 snapshots, cleanup checks | T40 | — |

Router and ingress overhead against direct engine calls is measured without reading any
engine secret. SGLang serves `/metrics` without its key, so each cell's scrape delta gives
the engine's own mean TTFT and end-to-end latency for the same requests, and the client
mean minus the engine mean is the router, ingress, agent and network path. vLLM keys
`/metrics` and the harness never reads the per-launch key, so vLLM has no direct baseline;
its periodic throughput log lines and a TCP-connect network floor (router and ingress,
no request sent) are kept instead. The checkpoint stays in the OS page cache between
iterations (dropping it would change host state), so cold start here means a new engine
process, not a cold NVMe read; `pagecache.txt` records `Cached` and `MemAvailable`
before each lifecycle request.

## 6. Execution order

Never run live rows on a host while another session is live on it. Record results in the
status runbook after each phase, not here. Phases map to plan units in the plan's §4.

1. **Phase A (now, SGLang and restart-only):** M01–M04, M07–M10, M12, M14, M15, M38, M43,
   M45; M16 (SGLang half); M17, M20–M23, M24, M26, M46.
2. **Phase B (remote vLLM, parity, W0):** M05, M06, M11, M13, M16 (vLLM half), M18, M19,
   M25, M40, M41, M47, M39; M38, M73 and M74 for both engines; M75.
3. **Phase C (park and wake, W1–W4):** M28–M30, M32–M34.
4. **Phase D (automatic switching, W9):** M27, M31, M35, M51–M53.
5. **Phase E (replicas, W6–W8):** M54–M63.
6. **Phase F (recovery and shutdown, W5, W10–W12):** M36, M37, M42, M44, M64–M72.
7. **Phase G (soak and closure):** M48, M49, M50. Completing it is the owner's exit: a
   two-host control plane supporting both engines in all meaningful permutations.
8. **Performance (after the current live phase, owner 2026-09-23):** M80 per fixture.

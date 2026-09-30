# F1 — First vLLM Path: Detailed Design

**Status:** Approved direction, September 11, 2026.
**Source:** [`../0000-full-picture.md`](../0000-full-picture.md) and
[`../../SPEC.md`](../../SPEC.md) rev 0.2. Implements milestone F1 (SPEC §18). The spec is
authoritative where summaries differ.
**Requirements covered:** R06, R07, R08, R12, R14.
**Tests targeted:** T07, T10, T11, T12, T14, T15, T16, T17, T18, T19, T20, T21.
**Owner decisions (2026-09-11):** live vLLM validation included in F1; environment is the
owner's DGX Spark (128 GiB unified memory); vLLM is installed by the operator as part of
F1 following the documented environment contract; the checkpoint is chosen by the design
and approved by the owner in design review; the experimental deep-park path is validated
in F1 under an explicitly opted-in isolated profile. The owner confirms vLLM development
mode is required for the sleep path to operate.

## 1. Scope

F1 delivers the first working vLLM path: a real engine adapter behind the F0 contracts, a
streaming router with admission and fairness, durable deployment operations on real
processes, attachment, restart-only switching between two profiles, and an explicitly
gated experimental deep-park path — validated live on the DGX Spark. Every claim stays
tiered (simulator vs live-Spark); F0's fake engine remains the conformance-suite
regression base and gains nothing engine-specific.

F0 deferrals absorbed here: generation machinery and `Envelope.generation` population
(G3), reservation rows written to the store (G3), a distinct internal exit code (G1),
`LifecycleAction` vocabulary unification (G3).

Out of scope (later milestones): remote hosts/enrollment (F3), multi-node groups (F4),
cache integrations (F4), container/service launchers (F5).

## 2. Slices

Six slices in dependency order; each leaves a coherent tested product.

| Slice | Deliverable | Tests |
|---|---|---|
| G1 — vLLM adapter + launch contract | Real `vllm` adapter over the OpenAI-compatible HTTP API; native-argument launch contract; Spark environment-contract doc; real `doctor host` fingerprinting | Adapter conformance on the fake + contract tests; T07, T12, T14 (render-plan conflicts, fingerprint rules; live fingerprint evidence via the §8 sequence) |
| G2 — Router | Real `capyctl-router`: `/v1/models` + streaming/non-streaming `/v1/chat/completions`, admission, queue bounds, cancellation accounting | T17, T19 (simulator) |
| G3 — Durable operations on real adapters | start/park/stop/preinitialize/undeploy via controller + real launcher (process groups, signals, exit status); admin-stop vs idle-stop; generation machinery; reservation persistence | T10, T12, T18 (late dispatch needs the generation machinery G3 delivers); generation/reservation coverage |
| G4 — Attachment | `attach model` with ownership ≠ reachability | T11 |
| G5 — Switching + fairness | A→B activation, one wake, bounded non-resetting window, drain → quiescence → release evidence; two profiles alternate | T15, T16, T17, T19 |
| G6 — Experimental deep-park + Spark qualification | Policy gate end-to-end; isolated `vllm-sleep` profile; Spark environment contract, doctor capture, restart-only live qualification, then experimental park/reload | T20, T21; live-tier T16/T20/T21 evidence |

## 3. vLLM adapter (`capyctl-adapters/src/vllm/`)

Implements the F0 `EngineAdapter` trait over vLLM's OpenAI-compatible HTTP API. All
engine-specific endpoints and launch parameters stay inside the adapter (SPEC §9).

**Parked-state observability (contract extension in G1):** in vLLM sleep mode the API
server stays alive and `/v1/models` keeps listing the model — a parked engine is
indistinguishable from Ready through those surfaces alone. G1 therefore extends the F0
adapter contract: `Phase` gains a `Parked` variant, `EngineState` gains a
`build_fingerprint` channel, and parked-vs-Ready is established by an engine-side signal
when the pinned release exposes one (e.g. a sleep-state endpoint enabled with sleep
mode), falling back to operation provenance from the store (the controller knows it
issued the park). `/v1/models` alone never establishes Ready after a park.

| Operation | Contract |
|---|---|
| `inspect` | Engine build fingerprint (from doctor capture), observed phase, advertised capabilities |
| `render_plan` | Resolve native parameters: model path, port, device/memory settings from the granted budget (explicit units, SPEC §7.5); reject reserved-flag conflicts (T14) |
| `check_readiness` | `/v1/models` returning the served model — liveness (`/health`) is explicitly not readiness (SPEC §6.1: liveness of an HTTP server is not model readiness) |
| `prepare_park` | Quiescence: zero in-flight observed, no queued requests; reports only what it can prove |
| `park` | `POST /sleep?level=1\|2` [S1] — level 1 retains a CPU weight backup, level 2 discards weights+KV; **reachable only under the host-policy gate** (§7) |
| `restore` | Wake allocations + `reload_weights` through the collective RPC, invoked once through the lead [S1]; waking allocations alone is not successful restoration — a generation check follows |
| `reload_weights` | Collective control, gated identically |
| `observe_work`/`cancel_work` | In-flight accounting via stream correlation; vLLM exposes no cancellation-ack API — the adapter reports `Uncertain` unless the stream closed cleanly |

Ambiguous outcomes reconcile, never repeat blindly: a lost park/reload ack routes to
RECONCILING (F0's state machine), not to a second collective (T20).

## 4. Launch contract (SPEC §8.1)

`exec`-type runtime profiles: configured launch prefix (e.g. `/opt/vllm/bin/vllm serve`)
plus the adapter-rendered engine-native argument tail. Rules carried from F0 + hardened:

- The wrapper must self-replace (`exec`) or stay foregrounded, forward signals, wait, and
  propagate exit status (T12). A process that backgrounds children and exits without a
  durable ownership handle is invalid.
- No shell interpolation. F0's PID + start-identity ownership becomes real process
  groups (`setsid`-equivalent), so terminate covers the whole owned tree.
- Private bind address from the host profile; port allocated from the host's configured
  range; engine settings (memory/KV/device) rendered from granted budgets with explicit
  units — never a blind concatenation of conflicting flags (SPEC §8.2, T14).
- Fingerprint at launch: engine build, checkpoint revision, launch args, environment
  identity — recorded with the operation (provenance; changes invalidate qualification
  evidence, T14).
- Internal failures (store/I-O/runtime boot) exit with a distinct internal code, not 2 or
  5 (F0 deferral absorbed in G1).

## 5. Router (`capyctl-router`)

**Surfaces**: `GET /v1/models` (lists configured enabled public IDs; never wakes) and
streaming (SSE) + non-streaming `POST /v1/chat/completions`. Nothing else in F1.

**Request path**: authenticate (API key) → resolve alias to an explicit deployment →
admission against the ledger (bounded queues: max requests per deployment, max buffered
bytes total; no unbounded multimodal buffering) → enqueue with deadline → join the
deployment's single activation operation when not READY (T15) → dispatch with the current
generation.

**Fairness and switching (T16, T17, T19)**: while A serves and B waits, A holds a
bounded, non-resetting admission window (busy A cannot extend it forever). When the
window closes: close A's admission → drain accepted work at the router → confirm engine
quiescence via the adapter → park-or-stop A with release evidence on every participating
host → reserve B's complete activation plan → launch/restore B → verify whole-group
readiness → open B's admission gate → dispatch the queued requests in order. A drain
timeout fails the switch by default; forced termination is separately authorized.

**Drain liveness policy (for unverifiable cancellations):** a drain cannot block forever
on work whose cancellation the adapter cannot prove. After a `cancel_work` with an
`Uncertain` result, a bounded grace period runs (versioned default, set in the
implementation plan); when it expires, the drain proceeds only on engine-side evidence —
the adapter's own in-flight telemetry or stream-closure observation — and a best-effort
abort is issued. Any residual uncertainty (work that may still be running) is recorded in
the release evidence instead of blocking quiescence: the switch proceeds, but the
park/stop of A is then treated as a best-effort release whose physical verification
follows the F0 rules (uncertainty never becomes free capacity — the ledger keeps A's
reservations until verified release or a stop with evidence). Quiescence blocks forever
only on work the engine can still prove live, not on accounting uncertainty.

**Switch-failure branch:** when the switch fails (drain timeout being the default
cause), A's admission reopens with its window state preserved — the failed switch does
not punish A — and B's queued requests receive a structured switch-failed error, with a
bounded re-queue option that honors each request's remaining activation deadline. The
deployment records a failed-switch event (feeding SPEC §17's failed-switches metric).
Forced termination remains separately authorized and is never the default path.

**Streaming accounting**: SSE events pass through with in-flight accounting. Client
disconnect is not proof the engine stopped — conservative accounting until completion or
confirmed cancellation. No fake tokens for waiting requests; no replay of partially
streamed responses; queued bodies and open streams are not promised to survive a router
restart (documented; T38 semantics).

**Late dispatch (T18)**: the ingress gate validates the assignment generation and refuses
stale dispatch after its gate closes — real with G3's generation machinery.

**Queue bounds (T19)**: max requests per deployment, max buffered bytes total, explicit
deadlines including activation wait. Client deadlines must include activation when
appropriate.

## 6. Attachment (T11)

`attach model` registers an already-running service for routing and observation only:

- Grants no permission to sleep, kill, restart, or evict; lifecycle commands on an
  attached deployment fail with a structured error (`unsupported_operation`-class).
- Attached usage on a managed host is represented conservatively; uncertain attached
  usage is never reclaimable capacity (SPEC §5.2).
- No external supervisor integration configured → status marks restart guarantees
  unavailable.
- Direct external clients may bypass capyctl's in-flight counts; no drain-based lifecycle
  operation on attached deployments without exclusive admission control (SPEC §5.2).

## 7. Deep-park gate (T20, T21)

capyctl's purpose is running multiple models on shared hardware: models are loaded and
parked/offloaded to disk, then loaded/activated on demand, using each engine's own
capabilities — vLLM's sleep/awake path where available, full restart where not; SGLang
the same. Park/reload is core F1 functionality. The security gate below exists because
the vLLM path to it runs through development mode (an upstream security constraint), not
because the feature is optional.

The `vllm-sleep` profile requires `security.allow_development_engine_controls:
true` in host policy — an explicit, documented opt-in. vLLM development mode is required
for the sleep path to operate (owner-confirmed): the profile's launch contract includes
the sleep-mode/development startup flags. **The opt-in gates the profile itself, not just
the park/reload operations**: the dangerous surface exists from the moment a
development-mode engine starts, so launching `vllm-sleep` in any mode — including
restart-only — requires the opt-in. The §8 restart-only qualification baseline therefore
alternates two restart-only profiles (stock + stock-alt); the `vllm-sleep` profile enters
only at step 4 under the opt-in. Controls:

- Default denial enforced at every layer: fake (F0), adapter, host policy, controller
  (T21). No public admin passthrough.
- `auto` under default policy resolves to restart-only. `deep_required` fails validation
  rather than changing meaning (SPEC §6.2).
- The upstream security warning (vLLM docs: development mode is not for production;
  collective RPC surface is dangerous [S2]) is carried into `docs/runbooks/` and the
  compatibility notes — it is not erased by private binding or the ingress gate.
- Park/reload failure: no blind repeated collectives — reconcile, quarantine, verified
  restart; repeated failures disable the profile's parking capability until
  requalification (SPEC §13.2).
- Qualification evidence is tiered: simulator (fake) first, live Spark second, never
  conflated (AGENT_HANDOFF).

**Preinitialize on real adapters (G3):** `preinitialize deployment` sequentially starts,
validates readiness, and parks — never displacing live user work. It requires a
*qualified* parking capability, which on real vLLM exists only under the experimental
opt-in after G6 qualification: when parking is unqualified or policy-denied,
preinitialize fails clearly with an unsupported-capability-class error rather than
claiming a prewarmed restart-only deployment (SPEC §6.3). `parking: auto` under default
policy resolves to restart-only, so preinitialize fails for such deployments until their
profile's parking capability is qualified. Until G6 lands, preinitialize is
simulator-tier only.

## 8. Spark environment contract and qualification sequence

New runbook: `docs/runbooks/vllm-env.md`. The operator executes the install; capyctl
installs nothing (T07). The contract pins:

- A dedicated venv path with a pinned vLLM release (pip/uv install performed by the
  operator per the runbook).
- The pinned checkpoint, chosen by this design and approved by the owner: proposal — a
  small instruct model in the Qwen3-4B/8B class, BF16, sized so the F1 recipe exercises
  ledger headroom on the Spark's 128 GiB unified memory (`memory.system` single domain).
  Exact pin frozen in the design review from the approved choice.
- Weights pre-downloaded to `/srv/models/...` (read-only storage pool).

Qualification sequence, each step gated:

1. Operator runs the environment contract (install + download).
2. `capyctl doctor host` (now real in F1) captures build fingerprints and memory
   observations; the live recipe is frozen only from reported reality.
3. **Restart-only live qualification first**: deploy → READY → serve → stop → re-deploy;
   A→B→A switching across the two restart-only profiles (stock + stock-alt) on the
   Spark; attach an
   already-running stock-vLLM service and verify routing works while lifecycle commands
   are rejected (T11 live); issue simultaneous first requests against a stopped
   deployment and verify a single wake operation (T15 live). This alone satisfies
   "first working vLLM path."
4. **Park/reload under the opt-in profile** — park/reload is core F1 functionality, not
   an optional experiment (owner decision: "the framework will have the park/reload
   feature otherwise there is no reason to build this thing"). capyctl's purpose is running
   multiple models on shared hardware by loading them and parking/offloading them to
   disk, then loading/activating on demand, using each engine's own capabilities: for
   vLLM the sleep/awake path (level 2, weights restored from the checkpoint on wake);
   full restart is the universal baseline used wherever an engine or profile lacks the
   capability. The Spark sequence: park → wake → generation check → repeat until clean
   cycles with recorded end-to-end request-to-first-token and memory behavior (SPEC §17;
   page-cache conditions recorded). If the initial pinned build's sleep path misbehaves
   on the Spark's unified-memory platform, the build/recipe is revised (adapter flags,
   vLLM release, platform configuration) and validation retried — the milestone does not
   downgrade away from the feature; restart-only remains the engine-capability fallback
   in the product, never a substitute for the qualified path on engines that have one.
5. Every live claim is tiered: simulator evidence vs live-Spark evidence never conflated.

Exit gate (SPEC §18 F1): two profiles alternate safely on the Spark; experimental
park/reload working on the Spark via vLLM's sleep/awake path; failures reconcile — with
T07, T10–T12, T14–T21
green at the simulator tier and live-tier evidence for the Spark sequence.

## 9. Test map (F1)

| Test | Simulator tier | Live tier (Spark) |
|---|---|---|
| T07 online host without prepared runtimes | inventory visible; deploy preflight fails specifically | doctor reports missing profile |
| T10 administrative stop vs idle stop | controller semantics tests | live stop/re-deploy |
| T11 attached service | fake-attached routing; lifecycle rejection | attached stock-vLLM service on Spark |
| T12 patched foreground wrapper | signal/exit-status harness | real vLLM launch on Spark |
| T14 reserved flags/profile change | render-plan conflict tests | fingerprint invalidation on profile change |
| T15 simultaneous activation | one wake operation | live concurrent first requests |
| T16 A → B → A | fake alternation with release evidence | Spark two-profile alternation |
| T17 streaming during swap | router drain semantics on fake | live streaming through a switch |
| T18 late ingress request | stale generation rejected | — (simulator) |
| T19 fairness and queue bounds | window non-reset; byte/count limits | — (simulator) |
| T20 park/reload timeout or partial failure | ambiguous park → reconcile | live experimental park failure injection |
| T21 experimental-controls policy | default denial at all layers | policy opt-in live path only |

## 10. Toolchain additions

Same workspace; new deps: `reqwest` (adapter/router HTTP client), `eventsource-stream` or
equivalent SSE handling (router streaming), `hyper`/`axum` for the router surface (pick
the tonic-ecosystem-compatible stack). No new languages, runtimes, or brokers.
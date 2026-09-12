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
| G1 — vLLM adapter + launch contract | Real `vllm` adapter over the OpenAI-compatible HTTP API; native-argument launch contract; Spark environment-contract doc; real `doctor host` fingerprinting | Adapter conformance on the fake + contract tests; T07, T12 |
| G2 — Router | Real `mllm-router`: `/v1/models` + streaming/non-streaming `/v1/chat/completions`, admission, queue bounds, cancellation accounting | T17, T18, T19 (simulator) |
| G3 — Durable operations on real adapters | start/park/stop/preinitialize/undeploy via controller + real launcher (process groups, signals, exit status); admin-stop vs idle-stop; generation machinery; reservation persistence | T10, T12; generation/reservation coverage |
| G4 — Attachment | `attach model` with ownership ≠ reachability | T11 |
| G5 — Switching + fairness | A→B activation, one wake, bounded non-resetting window, drain → quiescence → release evidence; two profiles alternate | T15, T16, T17, T19 |
| G6 — Experimental deep-park + Spark qualification | Policy gate end-to-end; isolated `vllm-sleep` profile; Spark environment contract, doctor capture, restart-only live qualification, then experimental park/reload | T20, T21; live-tier T16/T20/T21 evidence |

## 3. vLLM adapter (`mllm-adapters/src/vllm/`)

Implements the F0 `EngineAdapter` trait over vLLM's OpenAI-compatible HTTP API. All
engine-specific endpoints and launch parameters stay inside the adapter (SPEC §9).

| Operation | Contract |
|---|---|
| `inspect` | Engine build fingerprint (from doctor capture), observed phase, advertised capabilities |
| `render_plan` | Resolve native parameters: model path, port, device/memory settings from the granted budget (explicit units, SPEC §7.5); reject reserved-flag conflicts (T14) |
| `check_readiness` | `/v1/models` returning the served model — liveness (`/health`) is explicitly not readiness (T07 principle) |
| `prepare_park` | Quiescence: zero in-flight observed, no queued requests; reports only what it can prove |
| `park` | `POST /sleep?level=1\|2` [S1] — level 1 retains a CPU weight backup, level 2 discards weights+KV; **reachable only under the host-policy gate** (§6) |
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

## 5. Router (`mllm-router`)

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
- Direct external clients may bypass mllm's in-flight counts; no drain-based lifecycle
  operation on attached deployments without exclusive admission control (SPEC §5.2).

## 7. Experimental deep-park gate (T20, T21)

The `vllm-sleep` experimental profile requires `security.allow_development_engine_controls:
true` in host policy — an explicit, documented opt-in. vLLM development mode is required
for the sleep path to operate (owner-confirmed): the profile's launch contract includes
the sleep-mode/development startup flags. Controls:

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

## 8. Spark environment contract and qualification sequence

New runbook: `docs/runbooks/spark-vllm-env.md`. The operator executes the install; mllm
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
2. `mllm doctor host` (now real in F1) captures build fingerprints and memory
   observations; the live recipe is frozen only from reported reality.
3. **Restart-only live qualification first**: deploy → READY → serve → stop → re-deploy;
   A→B→A switching across the two profiles (stock + experimental) on the Spark. This
   alone satisfies "first working vLLM path."
4. Experimental park/reload only under the opted-in isolated profile: park → wake →
   generation check → repeat; measurements are end-to-end request-to-first-token and
   memory behavior, not reload RPC duration (SPEC §17); page-cache conditions recorded.
5. Every live claim is tiered: simulator evidence vs live-Spark evidence never conflated.

Exit gate (SPEC §18 F1): two profiles alternate safely on the Spark; experimental
park/reload validated where allowed; failures reconcile — with T07, T10–T12, T14–T21
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
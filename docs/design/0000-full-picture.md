# mllm — Full-Picture Design

**Status:** Approved direction, September 10, 2026.
**Authoritative source:** [`../SPEC.md`](../SPEC.md) revision 0.2. This document records
decisions and structure around that spec; it never overrides it. Where summaries differ,
`SPEC.md` wins.
**Companion brief:** [`../AGENT_HANDOFF.md`](../AGENT_HANDOFF.md).

## 1. Purpose and boundary

mllm is a fresh, engine-neutral lifecycle and inference-routing controller: one endpoint,
bring-your-own inference engines, explicit deployment ownership, safe model residency
transitions, and aggregate resource control. It owns routing, admission, deployment intent,
reservations, lifecycle coordination, local supervision, and visibility. Engines own
computation. Cache backends own KV storage. Host agents own approved local process execution.

Non-goals (SPEC §1.2): no driver installation, kernel compilation, implicit checkpoint
download/quantization, tensor transport, new KV storage formats, cloud placement, billing,
training, marketplace, or HA consensus.

## 2. Architecture overview

One platform executable supplies action-first CLI roles:

```text
mllm start server | mllm start host | mllm start standalone
mllm <action> <resource> [identifier] [options]
```

Path separation (SPEC §3.2) is the hard boundary:

```text
Management: CLI -> server -> host-agent control contract -> engine/launcher
Inference:  client -> server router -> API-head ingress -> engine
Storage:    engine/cache connector <-> configured checkpoint or KV storage
Compute:    engine ranks <-> engine ranks, using the engine's own transport
```

Management RPCs are never an implicit tunnel for prompts, tokens, weights, or KV payloads.

## 3. Module boundaries

Crate layout (proposal, refined at F0 design time):

```text
mllm/
  crates/
    mllm-domain/        # state machines, deployment/generation/owner types,
                        # lifecycle states, no I/O
    mllm-store/         # SQLite server store + agent journal, transactions,
                        # migrations
    mllm-scheduler/     # resource ledger, physical domains, exclusive pools,
                        # reservation admission, fairness queues
    mllm-protocol/      # gRPC protos + generated types, schema versioning
    mllm-controller/    # operation engine, reconciliation, lifecycle intents
    mllm-router/        # /v1/models, /v1/chat/completions, admission, streams,
                        # cancellation accounting
    mllm-agent/         # host supervision, launch/ownership handles, ingress gate
    mllm-adapters/      # vllm, sglang, fake adapter (trait + per-engine impls)
    mllm-launchers/     # exec/foreground; later: container/service
    mllm-config/        # strict YAML schema, validation, default generation
    mllm-cli/           # action-first grammar, binary entrypoint
  tests/
    harness/            # fake-engine simulator: slow start, sleep/reload,
                        # retained memory/cache, cancellation, crashes,
                        # ambiguous outcomes
```

Enforced separations:

- `mllm-domain` has no async/I-O; state machines are unit-testable.
- The fake engine adapter lives beside real adapters behind the same trait, so the
  conformance suite runs engine-free first; F0 needs no GPUs.
- The router never imports engine crates; adapters keep all engine-specific endpoints and
  launch parameters.
- Launch/terminate/owned-handle inspection belong to launchers; reservation policy, retries,
  fallback, timeouts, and fairness belong to the controller (SPEC §8.3).

## 4. Milestone decomposition

Each milestone has its own detailed design doc and implementation plan under
`docs/design/milestones/`, gated by the spec's exit criteria (SPEC §18). Slice-to-test
mapping below is at full-picture level; each milestone design makes it exact.

| Milestone | Scope | Requirements | Test IDs | Exit gate |
|---|---|---|---|---|
| **F0 — Contracts & foundation** | CLI skeleton, strict config/defaults, domain + store + ledger, abstract host/adapter/launcher traits, fake-engine harness | R02, R11, R13 (foundation of R10, R12) | T01–T04, T08, T09, T26, T27 (ledger-level, fake data) | State, allocation, idempotency, bootstrap/default tests pass without GPUs |
| **F1 — First vLLM path** | Standalone managed launch + attachment, streaming router, durable deployment ops, restart-only switching, security-gated deep parking | R06, R07, R08, R12, R14 | T07, T10–T12, T14–T21 | Two profiles alternate safely; experimental park/reload validated where allowed; failures reconcile |
| **F2 — SGLang fast follow** | Second adapter through same contracts + conformance suite | R01 (completed) | T22 | vLLM -> SGLang -> vLLM passes shared conformance suite; no parallel controller |
| **F3 — Remote host operation** | Invitation enrollment, persisted identity, agent-initiated control stream, private ingress, multi-host ownership and recovery | R04, R05 | T05, T06, T13, T29, T30, T33, T34, T37, T38 | CLI-to-server deployment on remote hosts; reconnect, revocation, stale-command, orphan tests |
| **F4 — Distributed & cache qualification** | Two-Spark group recipes, per-host release evidence, private/shared-cache combinations | R09, R10, R14 (multi-node) | T23–T25, T28, T31, T32, T35, T36, T40 | A -> B -> A under worker failure, retained caches, aggregate pressure; quota/persistence tests |
| **F5 — Broader packaging/backends** | Service/container launchers, platform packages, restart-only backends, API expansion | R03 (completed) | coverage matrix + platform tests | Each advertised backend/platform has explicit supported tests and dependency docs |

Sequencing rules:

- SGLang is not postponed behind a dashboard, marketplace, or cluster scheduler (F2
  immediately follows F1).
- Remote-host and resource-owner contracts are designed from F0 and validated at F3/F4.
- Some test IDs span two milestones (e.g., T23/T24: ledger logic proven in F0 with fake
  data; enforcement under real engine pressure proven in F4). Intentional.

## 5. Decision register

Recorded decisions (2026-09-10):

| Decision | Choice | ADR |
|---|---|---|
| Language/packaging | Rust; one executable per OS/arch; Python only for external launch helpers and integration tests | [0001](adr/0001-language-packaging.md) |
| Persistence | Embedded transactional SQLite: server store + local agent journal | [0002](adr/0002-persistence.md) |
| Remote control | Versioned gRPC + mutual TLS; agent-initiated sessions | [0003](adr/0003-remote-control-transport.md) |
| Config surface | Versioned strict YAML with SPEC §15 authority boundaries | [0004](adr/0004-config-schema.md) |
| `auto` numerical defaults | Fraction-of-observed-memory resolution, versioned constants, persisted provenance; admission closed without safe recipe estimate | [0005](adr/0005-auto-numerical-defaults.md) |
| License | Apache-2.0 | [0006](adr/0006-license.md) |
| Deployment updates | Revision-aware update/restart; no hot mutation of active processes | Adopted from SPEC §19 (no ADR needed) |

Deliberately open, decided at their milestone's design:

- Exact auto-resolution constants (F0 design).
- gRPC proto field numbers and exact HTTP paths (frozen at F0 design; SPEC §14 forbids
  inferring them from the sketches).
- Pinned vLLM/SGLang build versions with live verification (F1/F2 designs).
- Shared-cache-service record schema (SPEC §16.4: reject that variant until designed).

## 6. Testing strategy

Four distinct tiers, never conflated (AGENT_HANDOFF, verification workflow):

1. **Fixture checks** — config parsing, schema validation, ledger math, state machines.
2. **Simulator tests** — the fake engine simulates slow startup, sleep/reload, retained
   memory/cache, cancellation, crashes, and ambiguous operation outcomes, enabling
   concurrency and recovery tests without GPU hardware.
3. **Live-engine tests** — real vLLM/SGLang builds, real checkpoints, controlled hardware.
4. **Real-hardware tests** — the DGX Spark lab; named milestones only.

Every milestone report states implemented requirements, commands actually executed,
passing/failing tests, and unsupported combinations — tiered by the list above. Before an
adapter claims parking support: repeated A -> B -> A with exact pinned profiles. Before
distributed support: head failure with surviving workers and worker-management-channel
loss. Before cache support: retained-cache pressure, quotas, store outages, reuse across
park and process restart. Measurements are end-to-end request-to-first-token and memory
behavior, not reload RPC duration.

## 7. Invariants (review gates for every milestone)

Carried verbatim from AGENT_HANDOFF:

1. **Separate ownership from reachability.** Attaching an endpoint grants no permission to
   unload, kill, or restart it.
2. **Treat a deployment as a group.** Agent on every directly managed host; ingress only on
   API-facing members; engine collectives invoked once through the designated lead.
3. **Keep control and inference separate.** An outbound management connection is not an
   automatic prompt/token tunnel.
4. **Make parking qualified and security-gated.** Unknown or disallowed support uses
   restart-only under `auto`; explicit `deep_required` fails validation instead of changing
   meaning.
5. **Account for every physical owner once.** Activation peaks, parked residue, private
   caches, shared services, retained disk namespaces. Unified CPU/GPU memory is not two
   pools.
6. **Never treat uncertainty as free capacity.** Failed endpoint, expired lease, or lost
   connection does not prove cleanup; retain/quarantine ownership until reconciliation or
   verified fencing.
7. **Honor in-flight work.** Admission closure, ingress drain, and engine quiescence are
   distinct; no parking during an active stream; no replay of uncertain/partially streamed
   requests.
8. **Do not bypass local boundaries.** The control plane selects host-approved profiles; it
   is not a remote shell.
9. **Generate defaults safely.** Missing implicit config may be generated; missing/invalid
   explicit config must fail.

## 8. Standing security note

vLLM's security documentation warns against enabling development mode in production and
identifies the collective RPC surface as dangerous (SPEC §9.1, [S2]). The initial
deep-parking path is an explicitly authorized, isolated experimental integration. This
warning travels into compatibility and release documentation and is not erased by putting
an API behind an agent.
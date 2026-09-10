# mllm — Coding-Agent Handoff

**Specification:** `SPEC.md`, revision 0.2, September 10, 2026.  
**Status:** Architecture and requirements, not existing implementation.  
**Owner:** Eduardo Rodrigues Dias.

## Assignment

Build mllm as a fresh, engine-neutral lifecycle and inference-routing controller. Use the accompanying specification as the source of truth. Read it before proposing a repository structure or implementing a single-engine shortcut. This handoff does not provide an existing repository, production deployment credentials, an inference runtime, or a qualified model recipe.

The initial delivery order is foundation, a working vLLM path, then SGLang immediately next. Remote-host and multi-node contracts must be designed from the beginning and validated at their named milestones. Do not postpone SGLang behind a dashboard or general cluster scheduler.

## Product boundary

The server owns policy, admission, placement, reservations, and deployment intent. A host agent owns approved local process execution and safety. An inference engine owns model computation and distributed tensor communication. Cache backends own KV blocks and storage formats.

One executable supplies action-first CLI roles:

```text
mllm start server
mllm start host
mllm start standalone
mllm deploy model --file deployment.yaml
mllm status deployment <deployment-id>
```

These are required UX directions, not currently executable commands. `deploy` without `--wait` must return a durable deployment ID after persistence. `--wait` watches the same work; a CLI disconnect must not abandon it.

## First deliverable

Prepare an implementation plan with small, testable slices, mapped to R01–R14 and T01–T40 in the specification. Record brief architectural decisions for the proposed Rust implementation, embedded persistence, transport, schema, and numerical defaults. Exact choices not independently approved are explicitly marked as proposals in the spec; do not misrepresent them as established facts.

Plan the first vertical slice around a fake external engine, a durable deployment, a resource reservation, streamed requests, and failure injection. The fake engine should simulate slow startup, sleep/reload, retained memory/cache, cancellation, crashes, and ambiguous operation outcomes. This enables concurrency and recovery tests without GPU hardware.

After approval of the implementation plan, use test-driven slices. Implement restart-only control before optimizing deep parking. Keep all engine-specific endpoints and launch parameters inside adapters. Add SGLang using the same conformance suite instead of building a second controller.

## Invariants to preserve

**Separate ownership from reachability.** Attaching an endpoint must not grant permission to unload, kill, or restart it. Managed lifecycle requires approved launch and cleanup contracts.

**Treat a deployment as a group.** For directly managed distributed inference, each host has an agent. Only API-facing members need ingress. Invoke engine collectives once through the designated lead; use all agents for local ownership and release evidence.

**Keep control and inference separate.** Host management is agent-initiated and authenticated. The router needs its own path to inference ingress. An outbound management connection is not an automatic prompt/token tunnel.

**Make parking qualified and security-gated.** Retained runtime state, restored model contents, drafters, cache settings, and memory release must all be verified. Unknown or disallowed support uses restart-only under `auto`; an explicit required feature fails validation instead of changing meaning.

**Account for every physical owner once.** Host policy bounds aggregate use; deployments request per-phase resources. Include activation peaks, parked residue, private caches, shared services, and retained disk namespaces. Shared-service client quotas do not multiply the service's physical footprint. Unified CPU/GPU memory is not two independent pools.

**Never treat uncertainty as free capacity.** A failed endpoint, expired lease, or lost worker connection does not prove cleanup. Retain/quarantine ownership until reconciliation or verified fencing resolves it.

**Honor in-flight work.** Admission closure, router/ingress drain, and engine quiescence are distinct. Do not park during an active stream or replay an uncertain/partially streamed inference request.

**Do not bypass local boundaries.** The control plane selects host-approved profiles; it is not a remote shell. Native parameter pass-through cannot override reserved ownership, networking, identity, or resource settings.

**Generate defaults safely.** Missing implicit config may be generated. Missing/invalid explicit config must fail. Do not overwrite identities, expose remote listeners, execute detected scripts, or install engines without authorization.

## Required verification workflow

For each slice, report implemented requirements, commands actually executed, passing/failing tests, and unsupported combinations. Keep fixture checks, simulator tests, live-engine tests, and real-hardware tests distinct.

Before an adapter claims parking support, test repeated A -> B -> A cycles with exact pinned profiles. Before distributed support is claimed, include head failure with surviving workers and loss of the worker management channel. Before cache support is claimed, include retained-cache pressure, quotas, required-store outages, and cache reuse across park and process restart.

The specification includes a current upstream security warning affecting the experimental vLLM development-endpoint path. Do not erase that warning by putting an API behind an agent; carry it into compatibility and release documentation.

Measure end-to-end request-to-first-token and memory behavior, not just reload RPC duration. Do not copy earlier conversational model benchmarks into claims about the user's Sparks.

## Completion reporting

At the end of each slice, provide the changed files, test evidence, remaining release gates, and the next bounded slice. Do not claim mllm, an engine build, or a hardware combination works because the YAML parsed or a mock passed. The examples in this package are schema sketches, not calibrated runtime recipes.

The project is open source, but its exact license has not been selected. Obtain the owner's choice before publication rather than adding an assumed license grant.

# ADR 0005 — `auto` numerical defaults: fraction of observed memory

**Status:** Accepted (2026-09-10, owner approval)
**Context:** SPEC §15.2 requires "a deterministic conservative headroom policy in the
implementation ADR"; exact tuned numbers are not inferred from example YAML. `auto` values
(e.g., standalone's `managed_limit: auto`, `free_reserve: auto`) must resolve to finite,
versioned, inspectable allocations before any admission (SPEC §15.2/§16.5).

## Decision

`auto` resolves deterministically from observed system memory: a versioned fixed fraction
with absolute caps, applied per physical domain. Resolved values are persisted with their
provenance (schema version, observed inputs, policy version) alongside the deployment.
Deployment admission additionally requires a safe recipe estimate; `auto` never invents
engine-fit capacity. If a safe estimate cannot be established, admission blocks with a
useful diagnostic (SPEC §15.2).

## Rationale

- Deterministic and inspectable: the same host produces the same resolved values; operators
  can see how they were derived.
- Conservative: the free reserve and managed ceiling leave headroom for system/agent/router
  overhead and external workloads (SPEC §7.2).
- Fail-safe: generated defaults enable local-only listeners and inventory discovery, but
  never authorize engine execution or unbounded startup (SPEC §15.2).

## Consequences

- Exact constants (fraction, caps, per-domain rules) are selected and tested in the F0
  design — not copied from the example YAML.
- Resolution happens before admission, per deployment; numeric defaults never apply
  retroactively to pinned deployments (T39).
- A schema/policy version bump changes defaults only for new deployments unless an explicit
  update is requested (SPEC §15.2).
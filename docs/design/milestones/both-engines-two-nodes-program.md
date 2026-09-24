# Program: both engines, two nodes

Goal: vLLM and SGLang both fully functional — start, serve, park, wake, switch —
managed across two Spark nodes from one control plane. This document identifies
the work, its dependencies, and the live gate each piece must pass. It is a map,
not a spec: each unit below gets its own spec, plan, and review.

## 1. Where we are

| Capability | State |
|---|---|
| vLLM single-host launch | live-green (S1 run 6, `docs/runbooks/spark-live-f2.md`) |
| SGLang single-host launch | blocked: entrypoint denial + no adapter Initialize path |
| Ordinary park / wake / switch | not built; the product's premise |
| Remote control (server + agents) | not built; proto and CLI grammar only |
| Two-node group launch | not built; F4 |

The SPEC slice table (`docs/SPEC.md:797-808`) places remote operation at F3 and
distributed verification at F4. This program covers F2's remainder (SGLang, park)
plus F3 and F4.

## 2. Sub-projects

### S3a — Ordinary SGLang launch path, mllm side

The Rust half of "SGLang launches like vLLM".

- `ProfileBindings::spec` builds `AdapterSpec::Sglang` from the frozen effective
  configuration and the leased endpoint.
- A production `NativeLaunch` builder (every existing one is test-only).
- `SglangAdapter` gains `with_launch` / `with_tools` / `with_credentials` and an
  Initialize path mirroring `vllm/initialize.rs`.
- `check_readiness` implemented (served name in SGLang's model list).
- Fresh per-launch inference and admin credentials, sealed under the binding;
  `engine_secrets` gains a role and a schema bump.
- Observer optional on the adapter; control actions refuse honestly.

Gate: CPU contract tests only. Live launch is blocked by S3b.

Depends on: nothing. Safe to build immediately.

### S3b — SGLang audited native startup contract

The Python/security half; the reason SGLang cannot start today.

- Compose the protected package root and revalidate the pinned source map before
  imports.
- Worker enrollment: guards in every spawned interpreter, before native `Process`
  arguments unpickle.
- Plugin closure in every interpreter.
- Physical placement attestation.
- Service observation composition (scheduler saver, bridge, transport, listener).
- Checkpoint revalidation before loading.
- Rename the candidate descriptor contract to ordinary forms (Rust literals and
  Python validators together).

Gate: SGLang launch, Ready, one routed inference, stop with the group proven
gone, memory returns — on host-a. The SGLang twin of run 6.

Depends on: S3a (the Rust half it composes with). **Security-sensitive; opening
the denial requires explicit owner authorization.**

### S2 — Ordinary park, wake, switch (single host)

- Coordinator drain → park → parked accounting → wake → restore, with verified
  release, and wake-failure handling (SPEC §6).
- Production observer construction at the arm, resolving the driver-before-arm
  timing (`worker.rs:1603` before `:1626`).
- vLLM park (sleep mode + guard) and restore (wake, reload, cache reset).
- SGLang park (`/release_memory_occupation`) and restore
  (`/update_weights_from_disk`), with milestone evidence.
- Switch in the authority: close admission, bounded drain, quiesce, park or stop
  by tier, and on failure reopen the incumbent; activation join keyed by
  deployment, revision, generation; delete router `SwitchEngine`.

Gate: A parked while B serves, wake A, switch A↔B, both engines, single host,
routed inference after each transition, memory accounted.

Depends on: S3a and S3b (SGLang must launch before it can park).

### F3 — Remote control

- `mllm start server` on `control-host`: REST management + coordinator + store +
  gRPC `AgentControl` server.
- `mllm start host` on each spark: thin agent, outbound session, local launcher
  execution, local journal.
- `invite` / `join` enrollment, persisted host identity, reconnect without a new
  host record, revocation, stale-command rejection, orphan reconciliation.
- Transport: gRPC agent↔server, REST cli/web→server; Tailscale trust first,
  mutual TLS deferred.

Gate: CLI → server on control-host → agent on host-a deploys, starts, serves, stops
a real engine on the remote spark; two enrolled hosts reconcile in the registry.

Depends on: S3 and S2 for a meaningful remote lifecycle, but its transport can be
built against a Fake engine earlier.

### F4 — Two-node groups

- Group launch plan: member identities, rank roles, per-host devices and model
  paths, private peer addresses, rendezvous data.
- Two-Spark tensor-parallel groups for both engines (head on one node, worker on
  the other), ingress on the head only.
- Reserve all members before launching; compensating cleanup; per-rank release
  and restore evidence; distributed park; the lead agent invokes each collective
  once.

Gate: A → B → A under worker failure, retained caches, and aggregate pressure
(SPEC §18 F4).

Depends on: F3 and S2.

## 3. Cross-cutting

- **Cross-host evidence.** The coordinator's proof primitives (identities,
  `verify_gone`) are local today; over F3 they become agent reports, and the
  server must trust agents' evidence without inventing it.
- **Accounting across hosts.** Aggregate ceilings and per-host admission
  (SPEC §7, §10).
- **Ingress.** Only the API-facing member listens; the router forwards to the
  head (SPEC §11).
- **Security.** Remote control credentials (mTLS deferred), the deep-park gate
  (SPEC §9.1, T21), and the S3b native startup contract.

## 4. Order

S3a → S3b → S2 → F3 → F4. Each slice leaves a working, tested system and has a
live gate. S3b is the first security-sensitive step and needs authorization
before its code is written. S3a, and F3's transport scaffolding against a Fake
engine, are unblocked.

## 5. Known clashes with the current code

1. **Candidate-shaped SGLang descriptor.** Served name `candidate-{binding_id}`
   and both wire kinds are validated on the Rust and Python sides. S3a reuses
   them; S3b renames them.
2. **Two-process identity model.** `completion.rs:49` admits exactly `api` and
   `worker-0`. Two-node groups may fit this shape (one process per node) but
   per-rank evidence is not modeled; F4 must settle it.
3. **Fingerprint drift at start.** Nothing re-checks a frozen snapshot against
   the current host (status runbook open item 7); F3's remote hosts make this
   sharper.
4. **Route order.** Frozen revisions sort routes alphabetically, so
   `--served-model-name` is the alphabetically first route, not document order.
   Inherited by both engines; owner decision pending.
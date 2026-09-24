# ADR 0009 — Proof-carrying reconciliation

**Status:** Accepted (2026-09-16)
**Amends:** the module boundaries recommended in `SPEC.md` §18. The lifecycle,
resource and security requirements are unchanged; this decides how they are arranged.

## Context

Two runtime ownership models exist in the tree. The F1 model holds one adapter for
every deployment and owns processes in an in-memory map; it is the only one wired
into production. The F2 model carries per-binding drivers, durable steps, fencing
and evidence; it is reachable only from tests. F1 cannot run SGLang at all, because
the SGLang adapter refuses the unfenced control path by design, and it cannot
re-establish ownership after a restart because its handle map does not survive one.

F2's semantics are right. Its decomposition is not. Workflow logic lives in
`mllm-store`, which is larger than the controller, management, adapters, router and
scheduler combined, because that is where the transaction lives. The evidence model
is shaped around one engine's allocator, which is why vLLM has no evidence source.
And there are two state machines, `ordinary_lifecycle` and `candidate_creation`,
over the same transitions, differing in authority rather than in meaning.

## Decision

Adopt ports and adapters around a pure domain, driven by one reconciliation loop.

- `mllm-domain` is pure: entities, the planner, policies, resource algebra and proof
  rules, with no I/O, no clock and no engine knowledge. It must compile without an
  async runtime, and its tests must run with no database and no network. A rule that
  cannot be tested that way is in the wrong layer.
- `mllm-app` owns use cases, transactions, leases and fencing, and calls ports.
- Everything touching the world is an adapter: the store, each engine family, each
  launcher, the transports.

One loop drives all lifecycle work: observe fresh evidence, plan purely, arm one
action with a fencing token in a transaction, perform it at most once, then settle
the outcome as confirmed with proof, uncertain, or refused. Uncertainty re-enters as
input to the next tick and is never retried, which is what non-idempotent engine
effects require. Crash recovery is this same loop after a restart, so there is no
second recovery path that can disagree with the first.

Engines declare two sets rather than failing when called: the actions they perform
and the facts they can prove. The domain permits a transition when its required
proofs are a subset of what the installation can prove. vLLM therefore gets
restart-only parking because it can prove only that a process exited, which is the
fallback `SPEC.md` §6.2 already describes, reached by mechanism rather than by a
special case.

**The proof set gates the commit, not the call.** A transition whose proofs are
unavailable cannot be settled as confirmed regardless of what the engine's response
said. This is the invariant behind the observed failure where resumed allocations
returned plausible output from weights that had never been reloaded.

Qualification becomes an authority on the one lifecycle rather than a second
implementation of it. A candidate run is ordinary reconciliation under scoped
authority that cannot promote itself.

> Superseded by ADR 0011 (2026-09-17): qualification is not an mllm concept.
> `candidate_creation` is deleted rather than collapsed; consequence 4 below is
> discharged by that deletion.

## Consequences

Sequenced so that each step leaves a working system and is independently reversible.

1. Cut over to the F2 coordinator in production: wire it into `mllm-cli/src/roles.rs`,
   retire the handle map, and resolve adapters per binding. Nothing runs through the
   product until this lands, so it comes first and gives later steps live tests.
2. Extract the domain from `mllm-store` with no behaviour change. The store keeps its
   tables and loses its decisions.
3. Replace capability failures on call with declared capability and proof sets.
4. Collapse `candidate_creation` into the single planner under a candidate authority.

Step 4 deletes the most code and is deliberately last: doing it first would mean
restructuring the most intricate logic in the project against tests that have never
run in production.

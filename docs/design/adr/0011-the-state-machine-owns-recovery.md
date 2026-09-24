# ADR 0011 — Parking needs no qualification; the state machine owns recovery

**Status:** Proposed (2026-09-16)
**Amends:** `SPEC.md` §6.2 (a parking deployment no longer requires a qualified
deep-park path) and §8.4 (capability qualification leaves the product). §13.2's
recovery rules are adopted as written, not changed.

## Context

The product's model, as the owner stated it on 2026-09-16:

> The user creates a deployment, we guard against abuse of resources of the host, but
> we deploy and launch, the state machine should manage the deployment, if it fails,
> it retries, if fails X times, we stop.

Most of that exists. Admission against the host's declared ceiling works. Launch to
Ready works and is covered end to end by `crates/mllm-cli/tests/a1_gate.rs`. Proving
that a runtime's recorded processes are gone works, from identities rather than a live
handle, so it survives the restart that destroys handles.

Three things stand between that and the stated model.

### The qualification gate blocks parking, and guards nothing

`binding_identity` (`crates/mllm-store/src/ordinary_lifecycle.rs:450`) splits on
whether the deployment parks. A deployment that does not park gets a
`DeclaredBindingV1` derived from its recipe and host fingerprints. A deployment that
parks must instead produce a `QualificationReceipt` from the catalog, which only a
candidate qualification run creates.

That gate does not do what its name suggests:

- `qualified_effective` has exactly one caller, `binding_identity`. Nothing reads the
  catalog when a deployment parks or wakes.
- What the catalog hands the binding is `QualifiedBindingV3 { qualification_id, host,
  recipe }` — identity. The evidence the run gathered, its phase bounds, its evidence
  references, its request counts, stays in the catalog row and reaches nothing.
- `DeclaredBindingV1` already carries the recipe fingerprint, host name, hardware
  fingerprint and environment fingerprint: the same identity information, derived
  rather than recorded.

So the gate is an admission token that happens to be produced by a proof, not a proof
that anything checks. It also hard-requires `Engine::Fake`, so no real engine can pass
it at all.

The owner has said three times that users will not run a qualification step. Each
earlier design in this session kept the concept and moved it — automated at deploy,
renamed to a capability table, deferred to first park. A capability table is a
qualification with the serial numbers filed off. The concept goes.

### Failure is fatal to the coordinator, not to the deployment

`crates/mllm-controller/src/coordinator/worker.rs` returns from its loop on any
outcome that is not `Uncertain`, and the surrounding task then calls
`close_admission()`. That closes admission for **every** deployment, not the one that
failed, and the worker task ends.

So today one deployment failing to initialize stops the whole coordinator. Nothing
counts attempts; the schema has no attempt or retry column for a deployment.

### Retrying an uncertain effect is not the same as retrying a failed one

The lifecycle distinguishes two bad outcomes, and the distinction is the reason the
project exists:

- **Failed** — the effect is known not to have landed.
- **Uncertain** — nobody knows whether it landed.

Retrying a failed launch is safe. Retrying an uncertain one can start a second engine
while the first still holds GPU memory, or park a runtime twice. `AGENTS.md` states
the invariant: never release a reservation, advance an epoch, or replay a dispatch
without verified evidence.

"If it fails, retry" is therefore implementable, but only if uncertainty is resolved
into a fact before the next attempt — which is what the gone-proof already does.

## Decision

**1. Delete the qualification gate from the ordinary lifecycle.** `binding_identity`
returns a declared identity for every residency. `DeclaredBindingV1` carries the
deployment's real residency instead of the hardcoded `"restart_only"` it writes today.
The `BindingIdentity::Qualified` variant and the `qualified_effective` call go with it.

The three validators that police a binding — `ordinary_lifecycle.rs:300`,
`ordinary_lifecycle/receipt.rs:203`, `ordinary_lifecycle/cleanup.rs:190` — compare
against whatever `binding_identity` returns and need no change.

**2. Qualification is not an mllm concept and is removed.** mllm guards the host;
the user owns the recipe. The concept exists in another product and was carried into
this one by mistake. Nothing is relocated and no interface to another product is
designed. Before launch mllm checks recipe shape (SPEC §8.2) and host fit
(ADR 0007); it then launches and watches readiness. A recipe that does not work
surfaces as failed attempts and, after the budget in decision 5, a terminal
`Failed` deployment.

That is about a third of this workspace — roughly 15,700 lines in the store, over half
of it, plus 5,400 in the controller and 8,400 of controller tests. The store is
described elsewhere as larger than the controller, management, adapters, router and
scheduler combined, attributed to workflow logic following the transaction into it;
half of it is machinery for a ceremony the product does not have.

Three small types the ordinary lifecycle uses live in that module and move rather than
go: `ArmResult`, `CleanupMode` and `CleanupExecutionContext`.

What survives is what guards the machine: the park contract in
`candidate_creation/warm.rs`, which is the only definition of "parked" in the
repository (no allocations, weights or cache; quiesced; no unknown work; activity
counters unmoved; no outstanding request lease; identities equal to the owned
association; then milestone verification), the Fake engine's lifecycle simulation,
and the native launch handoff. The design at
`docs/superpowers/specs/2026-09-17-qualification-removal-design.md` sorts every piece.

**3. Park is an ordinary transition.** It gets no gate the other transitions do not
have. A deployment declares its tier under ADR 0010; the engine either performs it or
reports a failure, and the failure is handled like any other.

**4. Failure is scoped to the deployment.** A failed step must not close admission for
the host or end the worker. The worker records the failure against the deployment and
continues serving every other deployment.

**5. The state machine retries, with a budget.**

| Outcome | Action |
|---|---|
| Succeeded | Terminal. Attempts reset. |
| Failed — known not to have landed | Count the attempt, wait the cooldown, retry. |
| Launch failed after arm | Terminate the recorded processes, prove them gone, release with evidence; terminal for that start. Unprovable: Uncertain. |
| Uncertain — processes not gone | No retry. Surface it; this is the one case a machine must not guess at. |
| Attempts exhausted | Terminal `Failed`. Admission closed for **this deployment**. |

**Budget: three attempts. Cooldown: 30s, doubling** — 30s, 60s, 120s. A broken recipe
must not burn a GPU in a tight loop, and a transient failure should not wait minutes.
Both are policy, not constants, because a host with slower storage may need a longer
cooldown. They are coordinator options with these defaults today; the host-policy
plumbing follows when remote hosts publish policy (F3), since the policy shape has
one writer in `mllm-config`.

Attempts are counted per deployment, revision and generation. A new revision is a new
configuration and starts fresh; a retry of the same configuration does not. Retries
happen within the start command's deadline; a deadline reached before the budget is
spent is terminal for that start.

**6. Recovery from a wake failure follows `SPEC.md` §13.2 as written**: keep admission
closed, allow a bounded clean restart after verified cleanup, and do it before any
inference is dispatched. That restart is one of the three attempts, not free.

## Consequences

A deployment that cannot park is discovered when it first parks, and costs a cold
restart on a live request rather than a wake. That is the trade the owner has
accepted: the alternative is a proof step on every deployment, and the proof was never
consulted anyway.

One deployment can no longer take the host down with it. That is a behaviour change to
the coordinator worth calling out on its own — the current `close_admission()` on any
failed step is a blast radius nobody chose.

`Engine::Fake` stops being a requirement for parking. A real engine can park as soon
as its adapter can perform the control, which is the remaining native work rather than
a policy decision.

The catalog, the candidate runs and `qualified_effective` are deleted along with their
tests. The 49 controller tests that fail once the gate is removed all assert one
fixture's sanity check about a candidate run; they go with the concept rather than
being repaired.

A deployment in terminal `Failed` after three attempts needs an operator to act. This
ADR does not define that action beyond the existing Start; a deliberate restart of a
failed deployment is ordinary Start against a deployment whose attempts have been
reset by a new revision.

## What this does not decide

**Ordinary park itself is not designed here.** This ADR removes its gate and defines
how its failures are handled. The transition — drain, park, parked accounting, wake —
is the implementation work that follows.

**Eviction policy is not decided here.** Which deployment to park when another needs
memory is a scheduling question. This ADR only makes parking reachable.

**The native adapters are only partly built.** The vLLM Initialize path landed in
S1: `VllmAdapter` implements `execute_persisted`, and the coordinator launches a
real engine through it. SGLang is still refused by `ProfileBindings` for a missing
admin credential and observation socket, and stays refused until S3. Parking on a
real engine is S2 work in either case; this ADR removes its gate and does not
build it.

**The ordinary native launch is not designed here.** The handoff that renders a
protected SGLang launch survives as `NativeLaunchHandoff`, sourced through a trait
the application implements. Nothing implements it yet, `ProfileBindings` still
refuses SGLang, and the private descriptor's `sglang_candidate_private_launch` tag
and the served-name rule `candidate-{binding_id}` stay until that design.

# ADR 0010 — Declared park tiers

**Status:** Accepted (2026-09-16)
**Amends:** `SPEC.md` section 6.2 (residency vocabulary). Sections 8.4, 9.1, 9.2 and
13.2 are unchanged and are the authority for everything this document relies on.

## Context

Parking is the product's premise, not an optimisation. One model parked while another
serves, switching between them automatically, is the reason a single box holds more
than one model. The owner confirmed this on 2026-09-16. Anything that reduces
eviction to stop-and-restart misses the point of the project.

No ordinary park exists today. Only the candidate qualification path implements one.
Standalone declares `restart_only`, so parking is unreachable from the product.

### What the engines actually do

Two rounds of checking, because the first round got it wrong in both directions.

**vLLM sleep.** Level 1 offloads weights to CPU RAM and discards the KV cache. Level 2
discards weights and KV, keeping only buffers such as rope scaling tensors; restoring
calls `reload_weights`, which re-reads the original checkpoint files. `wake_up` takes
tags, but `tags=["kv_cache"]` reallocates the KV *memory*, not its contents. Level 2
additionally requires `reset_prefix_cache` after waking. Published wake times are
~0.1–0.8s (level 1, small models) and ~0.8–2.6s (level 2, small models), with level 1
roughly 3–10x faster and needing ~10–100GB of host RAM per model against level 2's
~MB.

**SGLang.** `release_memory_occupation` frees the KV cache GPU memory and flushes the
radix tree. Since v0.5 `--enable-weights-cpu-backup` copies weights to pinned host
memory on sleep and restores from there rather than from disk. That flag — not a vLLM
level number — is SGLang's fast tier, which is what `SPEC.md` §9.2 means by
"qualify its actual release, retained-copy, and restoration behavior rather than
assigning vLLM level numbers to it".

**Neither engine preserves KV contents across a park.** This was checked specifically
because hierarchical cache systems suggested otherwise. They are a real and separate
layer — SGLang HiCache extends RadixAttention with a HiRadixTree over GPU, host and
external storage; LMCache does the equivalent for vLLM into CPU DRAM, disk, Redis or
S3 — but:

- SGLang's host tier is flushed on release today. The open request
  [sgl-project/sglang#22243](https://github.com/sgl-project/sglang/issues/22243),
  "KV Cache CPU Offload during sleep/wakeup (`--enable-kv-cache-cpu-backup`)", is
  evidence the feature does not yet exist.
- HiCache L1 and L2 are private to a single instance. Host memory cannot be pooled
  across instances, not even two on the same node. Only L3, the external store, is
  shareable.
- LMCache's interaction with vLLM sleep is undocumented, and its own example destroys
  the backend at session end.

So an external L3 store is the only path by which KV could survive a switch, and
whether it does is unverified. `SPEC.md` §8.4 already names "cache integration" in the
qualification fingerprint and forbids turning an unsupported cache combination into a
supported one by accepting configuration. That rule decides how this document treats
the question: measured, never assumed.

### Why the tier cannot be chosen at runtime

SGLang's `--enable-memory-saver` and `--enable-weights-cpu-backup` are startup flags.
A running engine cannot acquire them. A fallback ladder — attempt the fast tier, drop
to the deep tier on failure — is therefore impossible for SGLang; the engine either
launched with the capability or it did not.

vLLM is the exception: `--enable-sleep-mode` is a startup flag, but level 1 versus
level 2 is a per-call parameter. A ladder is buildable there and nowhere else.
Building machinery only one engine can use, to paper over a choice that must be made
before launch anyway, buys nothing.

### Why the fast tier is not universally better

On the GB10 in host-a, device and host memory are one physical pool; the host
policy declares this as the `unified` domain. A weight backup "to CPU" therefore frees
nothing at all. The fast tier is meaningful only where device and host memory are
distinct, which is why the choice belongs in configuration validated against the host
rather than in a constant.

Deep parking is still worth it there. Measured on host-a and recorded in
`docs/runbooks/deep-wake-optimization.md`: median wake-to-response of 7.505s,
against a cold vLLM start of a minute or more. Waking skips process spawn, CUDA
context creation and graph compilation, and pays only the weight read.

### What already exists

More of this is built than the gaps suggest.

`SglangLaunchSettings` already carries `memory_saver`, `cpu_weight_backup`,
`cpu_kv_offload`, `external_cache` and `weight_restore`. Launch settings live on the
runtime profile and are frozen into the effective deployment at creation, fenced by
its revision.

The deployment document declares `residency`, and `normalize_profile` already receives
it, so intent and capability already meet in one place. The pattern this document
generalises is already there:

```rust
if residency == Residency::Warm && !value.enable_sleep_mode {
    return Err(invalid(
        "runtime_profiles.launch_settings.enable_sleep_mode",
        "warm vLLM requires sleep mode",
    ));
}
```

Four things are missing. `Residency` is `Warm | RestartOnly`, which cannot say *which*
warm. `VllmLaunchSettings` has no park level, so the level is derived at runtime from
a security policy — `ParkPolicy::ExperimentalAllowed` yields level 2 and anything else
yields level 1 — which conflates the operator's willingness to enable development mode
with the memory strategy the host requires. Nothing rejects a host-backed tier on a
unified-memory host, where it frees nothing.

And the host cannot currently say that it is such a host. `DomainPolicy` carries
`managed_limit`, `free_reserve`, `host_kv_limit` and `parked_limit`, and `DevicePolicy`
maps a device to a domain; none of them records whether a domain's device memory and
host memory are one physical pool. The `unified` in standalone's policy is a name
`standalone_config.rs` chose, not a declared property. The topology could be guessed —
a domain carrying both a mapped device and a `host_kv_limit` is probably unified — but
inferring a hardware fact from a coincidence of configuration is how a host silently
gets the wrong answer. It has to be declared.

## Decision

**1. One declared tier per deployment. No ladder, no runtime inference.**

**2. The deployment declares intent; the profile declares capability; resolution
verifies they agree.** This is the existing division. A deployment names a runtime
profile and a residency; `resolve_effective` rejects the pair when the profile cannot
deliver what the deployment asks for. Nothing is discovered at runtime.

**3. Residency names the tier.** Replace `Warm | RestartOnly` with:

| Residency | Meaning | Requires |
|---|---|---|
| `restart_only` | Stop and initialize again. `SPEC.md` §6.2 keeps this first-class. | nothing |
| `host_backed` | Weights retained in host RAM; KV dropped. | vLLM sleep mode with level 1, or SGLang `cpu_weight_backup`; a domain where host memory is distinct from device memory |
| `deep` | Weights and KV released; weights re-read from the checkpoint on wake. | vLLM sleep mode with level 2, or SGLang `memory_saver` |

`SPEC.md` §6.2's `auto` is deliberately **not** adopted. Selecting a tier at runtime is
the ladder this document rejects, and on the only hardware in hand the selection is
forced anyway. `deep_required` collapses into `deep`, because a declared tier that
cannot be delivered is already a validation failure.

**4. Residency determines the engine's park strategy, at the place each engine takes
it.** The two engines take it at different times and the configuration must not pretend
otherwise:

- SGLang's `memory_saver` and `cpu_weight_backup` are startup flags, so residency
  determines **launch settings**. They are constants today — `memory_saver: true`,
  `cpu_weight_backup: false` — and become derived.
- vLLM's level is a parameter of the sleep call, not a launch flag. Residency therefore
  determines the **level passed at park time**; only `enable_sleep_mode` remains a
  launch setting, and the existing check that warm requires it is kept.

The runtime derivation of the level from `ParkPolicy::ExperimentalAllowed` is removed
with the F1 controller that holds it; `crates/mllm-controller/src/operations.rs` has no
production caller and is deleted by A1's legacy-authority removal. The security gate of
`SPEC.md` §9.1 is unchanged and still independently governs whether the experimental
control path may be used at all; it stops doubling as a memory-strategy switch.

**5. The host declares its memory topology, and it validates the tier.** `DomainPolicy`
gains one field recording whether that domain's device memory and host memory are one
physical pool. The operator registering the host knows this; nothing else does, and it
must not be inferred from a domain's name or from which limits happen to be set.

`host_backed` is then rejected at configuration time on a domain declared as one pool,
with a reason naming the domain. It fails where the sleep-mode check already fails: at
configuration creation, not at 3am.

**6. Evidence records what the declared tier achieved, not whether a capability
exists.** The configuration says what to do; the evidence says what happened. A park
record carries the tier reached, the measured wake duration, and whether the model
generated correctly afterwards. This is what the switching policy needs regardless:
wake cost differs several-fold between tiers, and eviction choices should be driven by
measured cost rather than by a declared intent.

**7. KV is never assumed to survive a park.** Every tier above drops it, and the first
request after a wake pays full prefill. An external cache tier that claims otherwise is
a measured claim about a specific cache integration on a specific host, per `SPEC.md`
§8.4, and is out of scope here.

**8. Failure is recoverable and revocable.** `SPEC.md` §13.2 already governs: on wake
failure keep admission closed, allow a bounded clean restart after verified cleanup,
and disable the profile's parking capability after repeated failures. Cold restart is
always the floor. A failed wake costs a slow restart, not lost data — the checkpoint on
disk is never modified by any tier.

## Consequences

A deployment's parking behaviour becomes readable from its configuration. Today it is
the product of a security flag, an engine default and the host's memory topology, none
of which appear together anywhere.

Two deployments wanting different tiers on the same engine build need two runtime
profiles. That follows ADR 0008, where an installation is the unit that carries launch
settings, and it is honest: the flags differ, so the installations differ.

Invalid combinations become impossible to deploy rather than failing at first park. A
`host_backed` deployment on host-a is refused at configuration time with a reason,
instead of parking successfully and freeing nothing.

The residency change is a schema change to a field that already exists, and
`Residency::Warm` has no production callers that survive it — standalone declares
`restart_only` and nothing else creates deployments today. Existing stores carry no
`warm` rows to migrate.

The host policy gains a required field. Every host document must state its memory
topology, including hosts that will only ever run `restart_only` deployments. That is
deliberate: a default would be wrong on one class of hardware, and the failure it
causes — a park that frees nothing and an eviction that does not relieve pressure — is
silent. Better to ask once at registration.

This document does not decide **when** the park/restore proof runs. The deploy path is
the strong candidate, because the owner's flow already loads and parks there, leaving
only a wake and one generation check to add. That is a separate decision.

It also does not address the two native gaps that block parking on a real engine:
`VllmAdapter` has no `execute_persisted`, and `ProfileBindings` refuses SGLang because
nothing resolves its admin credential or supplies a trusted observation socket. Both
are tracked in the status runbook.

## Sources

- vLLM sleep mode documentation — <https://docs.vllm.ai/en/latest/features/sleep_mode/>
- vLLM, zero-reload model switching — <https://vllm-project.github.io/2025/10/26/sleep-mode.html>
- SGLang HiCache design — <https://docs.sglang.ai/advanced_features/hicache_design.html>
- LMSYS, SGLang HiCache — <https://www.lmsys.org/blog/2025-09-10-sglang-hicache/>
- sgl-project/sglang#22243, KV cache CPU offload during sleep/wakeup — <https://github.com/sgl-project/sglang/issues/22243>
- LMCache KV offload quickstart — <https://docs.lmcache.ai/getting_started/quickstart/offload_kv_cache.html>
- Measured wake times on host-a — `docs/runbooks/deep-wake-optimization.md`

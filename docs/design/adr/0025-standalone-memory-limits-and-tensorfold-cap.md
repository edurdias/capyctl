# ADR 0025 — Standalone memory limits, and TensorFold capped by its declaration

**Status:** Accepted (owner decision 2026-10-03).
**Amends:** `SPEC.md` §9.3 (TensorFold's memory) and §16.5 (the standalone
document), ADR 0023 §4 (TensorFold deployment configuration).
**Related:** ADR 0019 (discrete GPUs: the `system` and `device` domains), ADR 0018 §5
and the owner rules of 2026-09-25 (standalone is a server and one host; every
setting via YAML, `--set` and `CAPYCTL_SET__…`, flag > environment > YAML > default).

## Context

Two gaps showed in the three-engine comparison on a GB10 (121.7 GiB of unified
memory) on 2026-10-03.

1. Standalone derives its host's memory limits from the memory it observes: a
   managed limit of 50 %, a free reserve of 20 %, a parked limit of 25 % and a
   host-KV limit of 10 %. Its document accepted only `auto` for them, so the
   managed limit was fixed at 60.8 GiB. A TensorFold deployment declaring 84 GiB and
   a vLLM deployment requesting 72 GiB were refused `capacity_blocked` before
   anything started, though the machine had the memory. An enrolled host states its
   limits in its document; standalone, a server and one host, could not.

2. A TensorFold deployment declared 60 GiB to start and 58 GiB Ready, and the
   machine's used memory reached 77.9 GiB (about 74 GiB above idle) at a 256k-token
   prompt. ADR 0023 assumed `--context` fixes TensorFold's KV allocation and that
   TensorFold has no flag capping its memory. Neither holds for 0.6.3 on CUDA:
   - its CUDA budget is the machine's available memory less a floor of
     `max(4 GiB, 10 %)` (on a unified GPU, `MemAvailable`), and it caps that budget
     with `TENSORFOLD_CUDA_MEMORY_LIMIT_GB` when the variable is set;
   - with `--parallel` above 1 its streams' caches grow by use "while memory lasts",
     and the prompt states it keeps for later turns stay resident until that budget
     runs out; neither is bounded by `--context`'s window.
   CapyCTL set no cap, so the engine sized itself to the whole machine and grew
   past the reservation the ledger held for it. Seen live again with this change:
   one 255k-token prompt took the machine from 30 to 47 GiB, and each further long
   prompt kept its state (67, then 84 GiB) until the cap stopped the growth.

## Decision

### 1. Standalone memory limits

The standalone document's `host.resource_policy.memory.system.managed_limit` and
`free_reserve` take `auto` (the default, unchanged: 50 % and 20 %), a size
(`90GiB`) or a whole percentage of the observed memory (`75%`). Like every
standalone setting they are set by YAML, `--set` and `CAPYCTL_SET__…`, with one
precedence. They apply to the one host-memory domain the standalone publishes:
`unified` on a unified machine, `system` (host RAM) on a discrete one (ADR 0019);
a card's `device` domain keeps its derived limits.

Checks: the form when the document is read (a managed limit above zero, a free
reserve below 100 %), and at start, when the memory is known, that the managed limit
and the free reserve together fit it (they are simultaneous constraints, SPEC §16).
The parked and host-KV sub-limits are lowered to a managed limit below them.

At every start the embedded host's stored limits follow the document, as an
enrolled host's publication does (matrix M32): a changed limit is a new policy
revision, and a limit set back to `auto` returns to the default. Before, standalone
kept the limits it first stored. A lowered limit the current charges exceed is
refused, and the start fails naming it, as on a host.

### 2. TensorFold launched with its declared allocation as its cap

CapyCTL launches TensorFold with `TENSORFOLD_CUDA_MEMORY_LIMIT_GB` set to the
deployment's declared Ready allocation on the memory its GPU allocates from (the
`unified` or `device` domain), in GiB, rounded down to a whole MiB. Ready is the
charge the engine runs under after it starts. The variable is part of the engine's
closed environment; it is not an operator setting.

TensorFold 0.6.3 honours the variable; TensorFold 0.6.0 to 0.6.2 ignore it, so on
them the declaration remains the operator's estimate. The cap bounds what TensorFold
allocates through PyTorch; the process's other memory (the CUDA context, the Python
heap) is outside it, so the declaration should keep a few GiB above what the
model needs. A deployment whose window or streams do not fit its cap is refused by
TensorFold at start or has its requests queued by TensorFold, instead of growing
past what CapyCTL reserved.

## Consequences

- Standalone admits a larger deployment when the operator raises the limit, and an
  operator who raises it owns the smaller headroom that leaves the machine.
- Existing standalone installations keep their limits: the derived values are the
  ones they stored.
- A TensorFold deployment's declaration is now a cap as well as a reservation. A
  declaration sized from an earlier run without a cap may be too small for the
  same window; raise it rather than drop the cap.
- CapyCTL still does not measure a running engine's use against its charge; that
  remains observation, not enforcement (SPEC §7).

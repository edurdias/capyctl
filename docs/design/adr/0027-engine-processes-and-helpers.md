# ADR 0027 — An engine's own processes and its helpers

**Status:** Accepted (owner decision 2026-10-03: the exit watcher judges only the
engine's own processes, not helper processes they start).
**Amends:** `SPEC.md` §13.2 (an exit of an owned engine process while Ready, and the
recorded group a later proof must name again).
**Related:** ADR 0023 §6 (a single-process engine is its `api` process alone), ADR
0014 amendment A12 (kernel builds in the engine's process group), ADR 0016 (the
recorded identities on Terminate).

## Context

A launch records its process group at readiness: the `api` process CapyCTL started,
and every other process in its process group as `worker-0`, `worker-1`, ... in start
order. The exit watcher (the embedded role's `engine_exit::local_pass`, the host
agent's `exits::scan`) treats any recorded process that is gone as the engine
exiting: it closes dispatch and stops the instance, which then reads `failed`.

With CUDA graphs on, SGLang's scheduler starts a torch inductor compile-worker pool
just before readiness. On a GB10 with Qwen3.8-27B NVFP4 and a DFlash draft, the
launch recorded 25 processes, against 4 for Qwen3-4B. The pool's workers exit when
idle, a few minutes after Ready, with or without requests. The watcher read the first
of them gone ("exited, exit status unobserved") as the engine exiting and stopped an
engine that was decoding normally. vLLM's `torch.compile` workers, and any helper
pool an engine starts, carry the same risk. The same set was also the one every later
liveness proof (the re-proof after a restart, a park, a wake) required alive in full,
so a park after the pool went idle would have been refused as well.

## Decision

### 1. Engine processes and helpers

At readiness the group is split by parentage, from kernel facts:

- `api`: the process CapyCTL started (the process group leader).
- `worker-N`: a process the `api` process started itself (its parent is `api`). For
  SGLang these are the schedulers and the detokenizer; for vLLM the engine core. They
  are numbered from 0 in start order.
- `helper-N`: any other process in the group, such as a compile worker pool started
  by a scheduler, or a process that outlived its parent. They are numbered from 0 in
  start order, apart from the workers, so a helper exiting renames no worker.

A single-process engine stays the `api` process alone (ADR 0023 §6). A group of an
`api` process and helpers without a worker is not a launch.

### 2. What a helper's exit means

- **Not an engine exit.** Both exit watchers judge only `api` and `worker-N`. The
  store refuses a helper exit report (`record_engine_exit` returns nothing). The
  engine's own processes are still watched exactly as before: the `api` process or a
  worker gone is the engine exiting.
- **Liveness proofs name the engine.** Every later proof that the recorded engine is
  still the one running (the standalone re-proof after a restart, the remote re-proof
  after a session loss, quiescence of retired leases, park, wake and the host's
  residency claims) requires every `api` and `worker-N` process recorded, alive, and
  no engine process the record does not name (`same_engine`). A recorded helper may
  be missing, and a helper the engine started later may be present.
- **The readiness proof is unchanged.** The group recorded at Ready is the group
  observed at Ready, helpers included.

### 3. Helpers stay owned

Helpers are recorded, persisted and adopted like the engine's own processes, with the
same identity checks (PID, boot id, start ticks). A stop, an exit settlement and a
host Terminate still signal the whole process group and then each recorded process,
and release nothing until every recorded process, helpers included, is proven gone and
the group is empty. A process started after readiness is a helper, whatever started
it. It is not in the readiness record, and the group-empty check holds a release until
it is gone; a host agent that restarts records it as a new `helper-N`, numbered past
the recorded helpers.

### 4. Where it is decided

`DurableProcessLaunch::observe_group` (launchers) names the roles.
`ProcessIdentity::is_helper`, `engine_members` and `same_engine` (domain) are the one
definition every watcher and proof uses. The store's canonical membership accepts
distinct `helper-N` roles, with gaps, beside `api` and contiguous `worker-N`.

## Consequences

- An SGLang deployment can keep CUDA graphs on with a compile-worker pool; the pool
  going idle no longer stops it.
- A worker of an engine that exits on its own is still an engine exit. A helper that
  crashes is not; an engine that cannot run without it fails its own way (its `api`
  process or a worker exits, or a request fails).
- vLLM tensor-parallel workers are started by the engine core, not by the `api`
  process, so they are helpers by this rule. vLLM shuts its engine core down when a
  worker dies, and the engine core is a worker, so the exit is still seen.
- A launch recorded before this change keeps its recorded roles (every process a
  `worker-N`) until it is started again.
- A host and a server agree on roles because the host's own observation names them,
  and a host newer than its server is refused (ADR 0017). An older host names no
  helpers, which the newer server reads as before.

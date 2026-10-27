# ADR 0034 — LMCache by default: deep parking, pool sizing and `restart_only`

**Status:** Proposed (2026-10-24), for 0.2.0 (retargeted from 0.1.9 on 2026-10-27, after 0.1.9 shipped with ADR 0035 only). Nothing is implemented; the owner decides
the open questions at the end.
**Amends (on acceptance, with the implementation):** ADR 0033 §1 (the launch-time check
reads the transfer-mode and eviction options), §3 (the server's argument vector), §4 (the
rendered connector), §6 (the pool size), §7 (parking residencies and the qualified table),
§9 (reasons), §14 (codes and reasons) and its Verification (T45 and live rows LC12–LC17);
`SPEC.md` §7.4 (the pool's default size), §9.1 and §9.2 (which parks keep LMCache), §15.1
(`lmcache_pool: auto`), §20 (T45, LC12–LC17).
**Related:** ADR 0010 (park tiers), ADR 0012 (deep parking and its protections), ADR 0014
§5 (the context fit), ADR 0019 (memory domains), ADR 0025 and its amendment of
2026-10-23 (standalone's settable `host_kv_limit`), ADR 0028 (groups and their pickled
peer listeners), ADR 0033 (LMCache, amendments A1–A15), ADR 0035 (proposed: one model
across several GPUs on one host), ADR 0036 (proposed: embeddings).

## Context

### Where 0.1.8 leaves LMCache

ADR 0033 made LMCache the default for every deployment that can use it, and decided that
parking wins: a deployment whose residency resolves `deep` (or `host_backed` on a
discrete GPU) uses LMCache only on an engine version, model type and GPU combination in
the released table `QUALIFIED_PARKING`, and elsewhere keeps its residency with LMCache
off (`park_unqualified`). The table is empty in 0.1.8
(`crates/capyctl-config/src/lmcache.rs`, `QUALIFIED_PARKING = &[]`): LC4 was never run
with deep park. On a GB10, where every deployment parks deep by default, LMCache is
therefore on only for `restart_only` deployments: those that declare it, decision
deployments (which run without LMCache on SGLang anyway, A10), and machines with deep
parking turned off. "LMCache by default" is, in practice, "LMCache for `restart_only`".

Three findings since then shape this ADR:

- **The benchmark** (`target/live/lmcache-bench/README.md`, host A, GB10, Qwen3-4B,
  0.1.7 release binaries). With the prompt pushed out of the engine's own cache, LMCache
  brings back a 32k-token prompt in 0.35 s instead of 7.85 s on vLLM 0.32.0 (pool
  20 GiB), and 16k in 0.18 s instead of 2.85 s (vLLM) and 2.76 s (SGLang 0.5.22). After a
  park, `restart_only` with LMCache answers the first 32k request in 0.37 s (vLLM) and
  0.60 s (SGLang) once the engine is up, but the restart takes 63–70 s (vLLM) and
  93–99 s (SGLang), against 7.7–8.8 s and 47.8–54.6 s for the default deep park without
  LMCache. Counting the wake, the request after a park is slower with LMCache: 64.6 s
  against 16.5 s on vLLM at 32k, 94.8 s against 60.3 s on SGLang.
- **Held KV** (`target/live/held-kv-growth/evidence.md`, ADR 0033 A13 and A15). The
  server maps the engine's KV cache through CUDA IPC and keeps a stopped engine's KV
  mapped; on SGLang 0.5.22 every restart-park added 5,328 MiB to the server until a wake
  was refused for memory at the tenth or fifteenth cycle. 0.1.8 bounds it (#200, A15): a
  second park while an earlier engine's KV is held stops the instance and its server
  instead. Measured with the bound (`target/live/held-kv-bound/evidence.md`): on SGLang
  every second park is a stop (6 of 12 cycles), and the request after a stop is a cold
  start without the cache (`cached_tokens` 3, start 100.6–114.7 s); on vLLM 4 of 12
  cycles stopped, every time the request after the wake stored no new KV.
- **The pool** (`target/live/lmcache-bench/README.md`, "Why the 12 GiB pool only partly
  holds a 32k prompt"). LMCache 0.5.6 evicts when its pool reaches 80 % and then drops
  20 % of what it holds, so about 64 % of the pool is what is sure to survive. Qwen3-4B
  holds 144 KiB of KV per token (2 × 36 layers × 8 KV heads × 128 × 2 bytes), so the
  default 4 GiB pool keeps about 18,600 tokens (4 GiB × 0.64 / 144 KiB), less than one
  32k context. A 12 GiB pool returned 17,152–28,160 of a 32k prompt after 40k tokens of
  other prompts; 20 GiB returned all of it.

### What a deep park does to the engine and the connector

From the code CapyCTL runs:

- **vLLM** (`crates/capyctl-adapters/src/vllm/residency.rs`). A deep launch starts with
  `--enable-sleep-mode` (rendered only for parking residencies,
  `crates/capyctl-adapters/src/vllm/frozen.rs`). The park is `POST /sleep?level=2`, which
  discards weights and KV; the wake is `POST /wake_up?tags=weights`, a
  `collective_rpc reload_weights`, then `POST /wake_up?tags=kv_cache` and
  `POST /reset_prefix_cache`. Sleep mode allocates the weights and the KV cache through
  vLLM's `CuMemAllocator`: the CUDA virtual memory API, `cuMemCreate` and `cuMemMap`.
  Sleep calls `unmap_and_release` on every allocation (`cuMemUnmap`, `cuMemRelease`); the
  wake calls `create_and_map`, which makes a **new** physical allocation and maps it at
  the same virtual address
  ([cumem.py](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/device_allocator/cumem.py),
  [cumem_allocator.cpp](https://github.com/vllm-project/vllm/blob/v0.32.0/csrc/cumem_allocator.cpp)).
  `cuMemCreate` asks for an exportable handle only on a device that reports fabric
  handle support, and falls back to a POSIX file descriptor only when the fabric
  request fails; on other devices the allocation has no exportable handle. CapyCTL's
  `/reset_prefix_cache` passes no `reset_external`, so vLLM leaves the connector's cache
  alone (`reset_external` defaults to false,
  [cache api_router.py](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/entrypoints/serve/dev/cache/api_router.py)).
- **SGLang** (`crates/capyctl-adapters/src/sglang/http.rs`). A deep launch runs with the
  memory saver; the park is `/release_memory_occupation` with the tags `kv_cache` and
  `weights` (`kv_cache` alone for a resident-weights park, ADR 0014 A17); the wake is
  `/resume_memory_occupation`, `/update_weights_from_disk` with `flush_cache: true`, then
  `/flush_cache`. The memory saver backs the tagged regions with the same virtual memory
  API: a pause is `cuMemUnmap` and `cuMemRelease`, a resume a new `cuMemCreate` at the
  same address ([torch_memory_saver](https://github.com/fzyzcjy/torch_memory_saver),
  `csrc/core.cpp`, `csrc/utils.h`). The LMCache radix cache's `reset` drops its pending
  stores and flows
  ([lmcache_unified_radix_cache.py](https://github.com/sgl-project/sglang/blob/v0.5.22/python/sglang/srt/mem_cache/storage/lmcache/lmcache_unified_radix_cache.py));
  clearing LMCache's own store is a separate call CapyCTL never makes.
- **The LMCache server.** By default the server pulls and pushes KV itself
  (`lmcache_driven` transfer): at start the connector registers the engine's KV tensors,
  and the server maps them through CUDA IPC. The default wrapper rides PyTorch's storage
  IPC (`cudaIpcGetMemHandle`), whose own error text says: "CUDA IPC memory handles only
  support cudaMalloc-style allocations. Memory created through the CUDA VMM API cannot be
  shared this way -- common sources are ... vLLM's sleep mode (CuMemAllocator)"
  ([ipc_wrapper.py](https://github.com/LMCache/LMCache/blob/v0.5.6/lmcache/v1/platform/devices/cuda/ipc_wrapper.py)).
  A third wrapper (`lmcache.mp.use_vmm_api`) shares virtual-memory allocations, but only
  exportable ones, and "an imported mapping keeps the physical memory alive after the
  exporter releases it" (same file). Neither vLLM 0.32.0 nor SGLang 0.5.22 unregisters
  LMCache's mapping before a sleep or registers it again after a wake (ADR 0033 context;
  no sleep or wake hook in
  [lmcache_mp_connector.py](https://github.com/LMCache/LMCache/blob/v0.5.6/lmcache/integration/vllm/lmcache_mp_connector.py)).
  The upstream issue on sleep states the consequence: "engine_driven is the only
  transport compatible with vLLM sleep mode (lmcache_driven's CUDA-IPC handles are
  invalid on cumem/VMM allocations)"
  ([LMCache issue #4071](https://github.com/LMCache/LMCache/issues/4071), closed
  2026-10-24 by [PR #4410](https://github.com/LMCache/LMCache/pull/4410)).

Put together, for today's rendering with a parking residency [INFERENCE: not run; LC12
checks it]:

1. With the default wrapper, registering a sleep-mode or memory-saver KV cache fails, so
   the engine fails to start with the connector and ADR 0033 §8 relaunches it without
   LMCache (`engine_start_failed`, sticky for the revision on that profile).
2. Had registration succeeded (the virtual-memory wrapper on an exportable allocation),
   the server's mapping would keep the "released" KV alive through the park, so a deep
   park would free no KV memory: A15's held KV again, on every park. After the wake the
   engine's KV lives in new physical memory while the server's mapping points at the old
   one, so a hit would write KV the engine never reads and a store would copy stale
   memory: wrong answers at the next hit.

### Engine-driven transfer

LMCache 0.5.6 has a second transfer mode: in `engine_driven` mode the engine's worker
copies the KV of each stored or loaded chunk to or from host memory itself and sends it to
the server (PREPARE/COMMIT), and the server never maps the engine's GPU memory
([vllm_multi_process_adapter.py](https://github.com/LMCache/LMCache/blob/v0.5.6/lmcache/integration/vllm/vllm_multi_process_adapter.py),
`mp_transfer_mode`;
[engine_driven_transfer.py](https://github.com/LMCache/LMCache/blob/v0.5.6/lmcache/v1/multiprocess/modules/engine_driven_transfer.py)).
It is chosen on both sides:

- the server, `--supported-transfer-mode engine_driven` (default `lmcache_driven`;
  `auto` enables both) ([multiprocess config.py](https://github.com/LMCache/LMCache/blob/v0.5.6/lmcache/v1/multiprocess/config.py));
- vLLM, `lmcache.mp.mp_transfer_mode: engine_driven` in `kv_connector_extra_config`;
  SGLang, the same key in the LMCache configuration file's `extra_config` mapping
  ([unified_lmcache_mp_connector.py](https://github.com/LMCache/LMCache/blob/v0.5.6/lmcache/integration/sglang/unified_lmcache_mp_connector.py),
  `get_extra_config_value("lmcache.mp.mp_transfer_mode")`).

Because nothing outside the engine maps its KV, a sleep that releases the KV releases it,
and a wake's new allocation is what the next copy reads. It is the mode the upstream
issue names as the one compatible with sleep. Its costs:

- **Speed.** Each store and load is a copy through host memory rather than the server's
  zero-copy pull. Without a shared-memory segment (`--shm-name`, empty by default, and
  CapyCTL passes none, A2) the chunks travel pickled over the request socket
  ([server_transfer.py](https://github.com/LMCache/LMCache/blob/v0.5.6/lmcache/v1/multiprocess/modules/server_transfer.py),
  `PickleTransferStrategy`). No figure for it on this hardware exists.
- **Local code execution.** The pickle path's `commit_store` runs
  `pickle.loads(cpu_data)` on bytes received on the server's request port
  (`server_transfer.py`, `PickleTransferStrategy.commit_store`); the shared-memory
  strategy keeps that path as its fallback for a non-empty payload. The request port is
  loopback-only and unauthenticated (ADR 0033 §3, "Local reach"), so any local process
  that reaches it can make the server unpickle its bytes, which runs code as the role's
  service user [INFERENCE: from the handler, not exercised]. ADR 0033 accepted that a
  local process can read, store or clear KV; this is more. Today's servers do not expose
  it: they run the default `lmcache_driven` mode only (`server.py` registers the
  engine-driven module only under `engine_driven` or `auto`).
- **Hybrid models.** In 0.5.6 the engine-driven contexts refuse hybrid KV cache groups
  (sliding-window, recurrent or indexer layers beside full attention). Multi-group support
  is PR #4410, merged 2026-10-24; the release candidates tagged 0.5.7rc1 the same day may
  or may not carry it [INFERENCE], and no stable release does yet
  ([releases](https://github.com/LMCache/LMCache/releases)). Until an approved environment
  has it, only dense full-attention model types can qualify.

### The pool, the context and `host_kv_limit`

The pool is a fixed 4 GiB per instance unless the deployment states `lmcache_pool` (ADR
0033 §6). It is charged in the system domain, counts against `host_kv_limit`, and stays
charged while the model is parked. Two consequences of a fixed size:

- **It ignores the model and the context.** 4 GiB holds about 18,600 tokens of Qwen3-4B
  after eviction, 10,500 of Qwen3-32B (256 KiB per token: 64 layers, 8 KV heads, head
  dimension 128), and does not depend on whether the deployment's context is 4k or 128k.
  CapyCTL already computes a model's KV bytes per token from its `config.json` for the
  context fit (ADR 0014 §5, `crates/capyctl-config/src/context_fit.rs`,
  `bytes_per_token`), counting sliding-window and hybrid layers as full attention, and
  falling back where the shape cannot be read (multi-head latent attention, an unknown KV
  dtype).
- **It can exceed the limit.** Standalone derives `host_kv_limit` as 10 % of memory
  (`HOST_KV_FRACTION`, `crates/capyctl-cli/src/standalone_config.rs`); 12.17 GiB on a
  128 GB GB10. 0.1.8 made it settable on standalone (#193, ADR 0025 amendment of
  2026-10-23), and a refusal names the limit to raise (#201). On a machine with less than
  40 GiB of memory the derived limit is below 4 GiB, and a start whose pool does not fit
  the limit is refused at arm, `lmcache: auto` included (T45 test
  `the_pool_counts_against_host_kv_limit`): on such a machine every deployment of an
  engine with LMCache installed is refused until the operator raises the limit or turns
  LMCache off [INFERENCE: from the 10 % derivation and that test; not run on such a
  machine].

### `restart_only` under the held-KV bound

ADR 0033 §7 and the parking guide tell a deep-parking user who wants LMCache to declare
`residency: restart_only`. The measurements say what that costs, for the first 32k-token
request after a park (TTFT at the client, including the wake or start):

| Engine | Deep park, LMCache off | `restart_only`, LMCache, after a restart-park | `restart_only`, LMCache, after the bound's stop |
|---|---|---|---|
| vLLM 0.32.0 | 16.5 s (wake 8.6 s + 8.0 s prefill) | 64.6 s (restart 64.2 s + 0.37 s) | about 85–93 s (start 77.0–84.9 s + about 7.9 s cold prefill) [INFERENCE: sum of measured parts] |
| SGLang 0.5.22 | 60.3 s (wake 53.0 s + 7.2 s) | 94.8 s (restart 94.3 s + 0.60 s) | about 108–122 s (start 100.6–114.7 s + about 7.2 s) [INFERENCE: same] |

The restart costs 41–56 s more than a deep wake, and the cache saves at most 7.6 s on a
32k prompt; the saving would only match the restart's cost on prompts several times
longer than any context these deployments run [INFERENCE: prefill time grows at least
linearly with length]. The held-KV bound makes it worse: a model parked and woken
repeatedly alternates a warm restart and a cold start on SGLang, and on vLLM whenever the
request after a wake stores nothing. While parked, a restart-parked instance also holds
the pool, the server's overhead and the stopped engine's KV (5 GiB in these runs); a deep
park holds the pool and the overhead.

For a model that cannot park deep (decision deployments, a host with deep parking
disabled, gpt-oss and the other models of the parking guide's list), the comparison is
different: without LMCache such a model stops instead of parking, so a restart-park with
a warm cache is never slower than the stop it replaces, and costs only memory while
parked.

### Multi-GPU and embeddings

ADR 0035 (proposed) runs one model across several GPUs of one host with tensor
parallelism; LMCache with several ranks connected to one private server has never run.
ADR 0036 (proposed) adds `api: embeddings`, which resolves LMCache off with its own
reason (`embeddings_unsupported`). This ADR takes the reason for the first.

## Decision

### 1. Parking residencies use engine-driven transfer

A launch whose residency resolves `deep` or `host_backed`, on a combination in the
qualified table (§3), runs LMCache with engine-driven transfer:

- the server's argument vector (ADR 0033 §3, A2) gains `--supported-transfer-mode
  engine_driven`; never `auto`, so a server serves one mode only;
- vLLM's `kv_connector_extra_config` gains `"lmcache.mp.mp_transfer_mode":
  "engine_driven"`;
- SGLang's owner-only configuration file gains `extra_config` holding the same key,
  beside `mp_host` and `mp_port`;
- no `--shm-name`: the pickle transport, so no shared-memory segment to size, charge or
  clean up. LC16 measures it; if its cost fails LC16's bounds, a variant with a segment
  is measured before anything ships with one (and its segment is then charged and
  removed with the launch like A5's files).

A `restart_only` launch keeps `lmcache_driven` transfer in 0.2.0 unless the owner
decides otherwise (open question 5). The launch-time check (ADR 0033 §1) also verifies,
by shape, that the server declares `--supported-transfer-mode` with `engine_driven`, that
vLLM's adapter reads `mp_transfer_mode` from its extra configuration and that SGLang's
unified connector reads `lmcache.mp.mp_transfer_mode`; a release without them fails
`check_failed` for parking residencies only.

The protections of deep parking (ADR 0012) are unchanged: the LMCache server is not an
engine control path, and the engine keeps its loopback listener, its per-launch key and
CapyCTL's key guard.

### 2. The qualification plan

LMCache reaches a deep-parking deployment only through `QUALIFIED_PARKING`, row by row,
as ADR 0033 §7 says. 0.2.0 qualifies on host A (GB10), with the two approved environments
`~/vllm-0.32.0-lmcache-venv` and `~/sglang-0.5.22-lmcache-venv` (LMCache 0.5.6), Qwen3-4B
(`qwen3`) as the required model type, and any other dense full-attention model type whose
checkpoint is already on host A as optional rows. Rows LC12–LC17 (Live qualification
below) are run on a build whose table lists the combination under test, as LC4 was meant
to be.

**Pass criteria.** A combination (exact engine version, `model_type`, GPU product name,
residency) enters the released table when, on that combination:

- **Correctness (LC13, LC14).** Every first request after a wake, at temperature 0, gives
  the answer a `lmcache: off` deep deployment of the same model gives for the same prompt;
  LMCache's hit counter shows the prompt's whole 256-token chunks coming back (for a
  32,0xx-token prompt, 32,000 tokens), so the answer comes from the cache; a salt stored
  before the park hits only under the same salt after the wake; a park armed right after
  a long request (the store possibly still in flight) still answers correctly after the
  wake. No `wake_mismatch` and no `wake_uncertain` attributable to LMCache in 15 cycles.
- **Memory (LC13, LC14).** The parked footprint (system `MemAvailable` on unified
  memory) is within 1 GiB of the same deployment's `lmcache: off` deep park plus the pool
  and the server's measured overhead: the park releases the engine's KV. The server's GPU
  figure never exceeds its figure after the first start by more than 256 MiB across 15
  cycles; no `lmcache_server_stopped` and no held KV is ever charged; the charge covers
  every reading.
- **Time (LC13, LC16).** The wake is within 10 % of the `lmcache: off` deep wake (vLLM
  7.7–8.8 s, SGLang 47.8–54.6 s in the benchmark); a cold prompt's TTFT is within 10 % of
  LMCache off; the first 32k request after a wake reaches its first token in at most a
  quarter of LMCache off's post-wake TTFT (7.96 s on vLLM, 7.22 s on SGLang).
- **Hosts (LC15).** Once per engine version, the same park, wake and hit through the
  server on the control host with host A enrolled (A14 found that path broken for
  restart-parks): a failure there blocks the table for that engine version, since the
  table does not distinguish hosts.

**When a combination fails:**

- It stays out of the table. Its deployments keep parking deep with LMCache off
  (`park_unqualified`), as in 0.1.8; nothing changes for users.
- A wrong answer (correctness) on any model type keeps every row of that engine version
  out, not only the failing model type, and is recorded with its prompts and both
  answers in this ADR's amendment and the change file; a draft upstream issue is kept
  with the evidence, not filed.
- A failure of memory or time alone keeps the row out and is recorded the same way; the
  owner may accept a time-only failure explicitly for a later release.
- An engine that does not start with the connector (LC12 for today's rendering, or LC13
  on a model type the transport refuses) is the expected ADR 0033 §8 path and is recorded;
  it qualifies nothing.
- If engine-driven transfer fails on both engines, 0.2.0 ships no table rows and
  "LMCache by default" stays as in 0.1.8; the pool sizing (§4) still applies to
  `restart_only` deployments.

### 3. The table

Rows are written as ADR 0033 A12 reads them: the registered engine version (`0.32.0`,
`0.5.22`), the checkpoint's `model_type` (`qwen3`), the GPU's product name as `nvidia-smi`
reports it (`NVIDIA GB10`), and the residency (`deep`). `host_backed` rows need a
discrete GPU and an LMCache environment there (open question 7). A hybrid model type is
not added while the approved LMCache lacks multi-group engine-driven transfer; it stays
`park_unqualified`, so no start fails on it.

### 4. Pool size from the model and its context

`lmcache_pool` takes `auto` (the new default) or a size as today. With `auto`, the pool
of a launch is

```
target = max(4 GiB, ceil_GiB(N × C × B / U))
pool   = min(target, cap)
```

- **B**, the KV bytes per token: the context fit's `bytes_per_token` for the checkpoint
  (and its draft model, as the fit counts it), so the same conservative figure the engine
  is sized with.
- **C**, the launch's context: `engine_config.context_length` when stated, else the
  fitted context (ADR 0014 §5). The fit is a function of the checkpoint's configuration
  and the deployment's KV grant, so the store computes the same number the host renders.
- **N**, the full contexts the pool holds after eviction: 1 (open question 2).
- **U**, the share of the pool sure to survive an eviction: 0.64, the trigger watermark
  0.8 times what an eviction keeps (1 − 0.2). CapyCTL now renders
  `--eviction-trigger-watermark 0.8 --eviction-ratio 0.2`, LMCache 0.5.6's defaults
  ([distributed config.py](https://github.com/LMCache/LMCache/blob/v0.5.6/lmcache/v1/distributed/config.py)),
  so a later 0.5.x that changes them does not change what the formula assumes; the
  launch-time check verifies both options.
- **cap**: the placed host's system-domain `host_kv_limit`, less the host-KV bytes the
  deployment's own resources declare, rounded down to a whole GiB, and at least 1 GiB. A
  host that declares no `host_kv_limit` caps the pool at the floor, 4 GiB, as today: the
  pool grows past it only against a stated bound.
- **Unknown B or C** (a shape the fit cannot read, or a remote checkpoint its host has
  not sized yet): the pool is 4 GiB, as today, and status says why.

Examples (GB10 standalone, cap 12 GiB from the derived 12.17 GiB):

| Model, context | One context | Target | Pool | Tokens kept after eviction |
|---|---|---|---|---|
| Qwen3-4B, 8,192 | 1.1 GiB | 4 GiB (floor) | 4 GiB | about 18,600 |
| Qwen3-4B, 32,768 | 4.5 GiB | 8 GiB | 8 GiB | about 37,300 |
| Qwen3-4B, 65,536 | 9 GiB | 15 GiB | 12 GiB, warning | about 55,900 |
| Qwen3-32B, 32,768 | 8 GiB | 13 GiB | 12 GiB, warning | about 31,500 |

**The warning.** When the pool keeps fewer tokens than one full context (pool × U < C × B),
`deploy`, `status deployment` and `inspect deployment --effective-config` say so, with
both numbers and the remedy: "the LMCache pool (12 GiB) keeps about 55,900 tokens after
eviction; one full context of 65,536 tokens needs 15 GiB: raise host_kv_limit or set
lmcache_pool". The event `lmcache_pool_short` records it once per launch. It is a warning;
the deployment runs.

**Room at admission** (open question 3). With `auto` and `lmcache: auto`, when the host-KV
room left on the host cannot hold the pool, the pool shrinks to the room left in whole
GiB, at least 1 GiB, with the warning; with less than 1 GiB left the launch runs without
LMCache, reason `host_kv_room`. A stated `lmcache_pool` or `lmcache: required` keeps
today's behaviour: the start waits or is refused with #201's message. The pool a launch
got is recorded with the launch, and a later launch computes it again.

**Status** gains the basis: `LMCache   on: pool 8 GiB (auto: one 32,768-token context of
Qwen3 at 144 KiB per token), used 1.3 GiB, ...`; JSON gains `pool_source` (`auto`,
`stated`, `default`), `pool_tokens` and `warning`.

A stated `lmcache_pool` is used as given (whole GiB, at least 1 GiB), and is still
refused beside `lmcache: off`.

### 5. `restart_only` is no longer the way to get LMCache

The guide and ADR 0033 §7 stop telling a deep-parking deployment to declare
`restart_only` for LMCache: by the measurements above, a deep park without LMCache
answers the first request after a park sooner than a restart with it, and the held-KV
bound turns many restart-parks into cold stops. The parking guide says instead: a model
that can park deep should; it gets LMCache once its combination is qualified;
`restart_only` is for models that cannot park deep, and for them LMCache keeps a warm
cache across the restart.

The behaviour does not change: a `restart_only` deployment, declared or resolved, keeps
LMCache as in 0.1.8, with A13's charge and A15's bound. Status for a deep deployment off
with `park_unqualified` no longer suggests `restart_only`.

### 6. Several GPUs on one host

A deployment that runs one model across several GPUs of one host (ADR 0035,
`tensor_parallel` above 1) runs without LMCache, reason `multi_gpu_unqualified`
("LMCache has not been qualified with a model split across GPUs"), and `lmcache:
required` on it is refused `lmcache_unavailable:multi_gpu_unqualified`, until an LC row
runs it. A deployment with a `topology` keeps `group_unsupported`.

### 7. Codes and reasons

Added to ADR 0033 §14: the reasons `host_kv_room` and `multi_gpu_unqualified`
(`embeddings_unsupported` is ADR 0036's); the event `lmcache_pool_short`;
`lmcache_unavailable:multi_gpu_unqualified` at resolution; `invalid_config` for an
`lmcache_pool` that is neither `auto` nor a valid size.

### 8. Texts that change with the implementation

- **SPEC §7.4**, the LMCache sentence: "its pool (by default sized to hold one full
  context of the deployment's model after LMCache's eviction, at least 4 GiB and at most
  the host's `host_kv_limit`)".
- **SPEC §9.1 and §9.2**: "A parking residency uses LMCache with engine-driven transfer,
  only on a qualified combination."
- **SPEC §15.1**: `lmcache_pool: auto | <size>`. **SPEC §20**: T45 and LC12–LC17.
- **`docs/guide/parking.md`**, "Parking with LMCache": the `deep` bullet names the
  qualified combinations and drops "To have LMCache, declare `residency: restart_only`";
  the `restart_only` bullet says it is for models that cannot park deep.
- **`docs/guide/engines.md`**, LMCache: the qualified table, the transfer modes, the
  "Local reach" paragraph extended with engine-driven's pickle path, "Long prompts"
  rewritten around the automatic pool and the warning.
- **`docs/operations/configuration.md`** and **`docs/guide/deploy.md`**: `lmcache_pool:
  auto`. **`docs/guide/errors.md`**: the new reasons.

## Consequences

- Default deployments on GB10 (deep park) get LMCache for each qualified combination: a
  prompt evicted from the engine's cache, or the conversation that was running when the
  model parked, comes back from host RAM, without the restart that `restart_only` costs.
- A deep park keeps the pool and the server's overhead charged, as every LMCache park
  does; it never holds engine KV, since the server maps none.
- Any local process that reaches a parking deployment's LMCache request port can make
  the server unpickle its bytes, so run code as the role's service user. The host's
  `lmcache: off` and `retention: zero`, and the deployment's `lmcache: off`, remain the
  remedies, and the guide says so (open question 1).
- Pools grow with the model and the context: a Qwen3-4B deployment with a 32k context
  holds 8 GiB instead of 4 GiB, parked or not. The cap keeps any one pool within the
  host-KV limit, and admission shrinks or drops an automatic pool rather than refusing an
  `auto` deployment, so a small machine no longer refuses every deployment for LMCache's
  sake.
- Two transfer modes exist side by side (engine-driven for parks, LMCache-driven for
  restarts) until open question 5 picks one.
- The table is per GPU product: discrete GPUs, other model types and later engine
  versions stay `park_unqualified` until a row runs on them.
- CPU and Fake-engine tests are not qualification; only LC12–LC17 add table rows.

## Alternatives considered

- **Keep `lmcache_driven` for parks with the virtual-memory wrapper
  (`lmcache.mp.use_vmm_api`).** Rejected: vLLM's allocations are exportable only on
  fabric-capable devices, the server's imported mapping keeps released KV alive (no KV
  freed by a park), and the mapping is stale after the wake's new allocation; nothing in
  vLLM 0.32.0 or SGLang 0.5.22 re-registers it.
- **Unregister and re-register the connector around a park from CapyCTL.** Not
  available: the connector lives in the engine's workers, and neither engine exposes a
  call to do it; it would need upstream changes.
- **`--supported-transfer-mode auto` on every server.** Rejected: it would expose the
  pickle path on servers that never use it.
- **Run every LMCache launch engine-driven now.** Deferred to open question 5: it would
  remove the held-KV path for restarts too, but its cost is unmeasured.
- **Switch unqualified deep deployments to `restart_only`.** Rejected again (ADR 0033
  alternatives): the measurements above show it slower.
- **Pool from the engine's KV grant** (hold everything the engine had cached). Rejected
  as the default: on GB10 the grant can be tens of GiB, far beyond what one conversation
  needs; a stated `lmcache_pool` covers it.
- **A per-deployment `lmcache_contexts: N`.** Not proposed: a stated `lmcache_pool` already
  covers a deployment that wants more (open question 2).
- **Refuse or wait for an `auto` deployment whose pool does not fit**, as 0.1.8 does.
  Rejected for `auto` (open question 3): it turns a cache into a reason not to run.

## Implementation plan (target 0.2.0)

Each slice lands with CPU tests tagged T45 (and T14, T25 where they apply); none is
qualification.

1. **Transfer mode.** Render `--supported-transfer-mode engine_driven` and
   `--eviction-trigger-watermark 0.8 --eviction-ratio 0.2` in the server's argument
   vector (the latter for every launch), `lmcache.mp.mp_transfer_mode` in vLLM's
   connector and SGLang's file for parking residencies; extend the launch-time check;
   tests on the exact argument vectors and on a check that fails each missing piece.
2. **Pool sizing.** A pure function for B and C shared by the store and the host; the
   remote host reports B and the fitted context with its checkpoint sizing (a schema
   step, as A12 did for `model_type`); `lmcache_pool: auto`; the cap, the shrink and
   `host_kv_room`; the warning, `lmcache_pool_short`, status and JSON; tests on the
   table of examples above and on a host whose limit is below 4 GiB.
3. **Reasons.** `multi_gpu_unqualified` with ADR 0035's implementation.
4. **Live rows LC12–LC17** on host A with a build that lists the combinations under test;
   the passing rows go into `QUALIFIED_PARKING`, the results into an amendment of this
   ADR and the change file.
5. **Documentation** (§8).

## Live qualification

Host A (GB10), standalone unless stated, with `~/vllm-0.32.0-lmcache-venv` and
`~/sglang-0.5.22-lmcache-venv` (LMCache 0.5.6), Qwen3-4B, `context_length: 32768`,
`memory.kv_cache: 5GiB`, prompts and method as the benchmark's (8k, 16k and 32k tokens,
temperature 0, five repeats). "Off" is the same deployment with `lmcache: off`.

| Row | What it shows |
|---|---|
| LC12 | Today's transfer under deep park, per engine: a build whose table lists the combination and that renders `lmcache_driven`. Expected: the engine fails to register its sleep-mode (vLLM) or memory-saver (SGLang) KV and is relaunched without LMCache (`engine_start_failed`, the log line recorded). If it starts instead: park, read the server's GPU figure and `MemAvailable`, wake, and compare the first hit's answer with off. Also reads, without changing anything, whether the GB10 reports fabric handle support. Qualifies nothing; it settles the Context's inference. |
| LC13 | Deep park with engine-driven transfer, per engine: the server's argument vector and listener check as rendered; a 32k prompt, then park; the parked footprint against off's; wake time against off's; the same prompt after the wake hits from LMCache (hit tokens, `external_prefix_cache_hits` on vLLM) with off's answer; salts A, B and none across a park (only A hits A); a park armed right after a 32k request, then a hit with the right answer; the server's GPU figure and resident set at each step. |
| LC14 | Endurance: 15 deep park and wake cycles per engine, alternating the same prompt and a new one, one request each; every answer equals off's; the server's GPU figure stays within 256 MiB of its first reading; no held KV, no `lmcache_server_stopped`, no `lmcache_charge_raised` beyond a measurement; the charge covers every reading. |
| LC15 | Enrolled host, per engine: the server on the control host and host A enrolled; LC13's park, wake and hit; the wake canary passes (A14). |
| LC16 | Cost, per engine: TTFT for cold, evicted and after-park requests at 8k, 16k and 32k with engine-driven transfer, against off and against the benchmark's `lmcache_driven` figures; the engine's and the server's resident memory while transferring. If the bounds of §2 fail, the same rows with a `--shm-name` segment, its size recorded. |
| LC17 | Pool sizing: an `auto` deployment shows its pool and basis (8 GiB for Qwen3-4B at 32k, 4 GiB at 8k); its 8 GiB pool returns a whole 32k prompt after a park with one other 4k prompt sent before the park; a deployment whose one context needs a pool above 12 GiB (Qwen3-4B at 65,536 tokens where the checkpoint allows it, or a larger dense model at 32k) shows the warning and a 12 GiB pool under the derived 12.17 GiB limit, and with `host_kv_limit` raised to 24 GiB gets its full pool and no warning; a second `auto` deployment with 3 GiB of room left starts with a 3 GiB pool and the warning, and with no room runs without LMCache (`host_kv_room`); a stated `lmcache_pool` is used as stated; the server answers `/status` within the start bound with a 15 GiB pool. |

## Open questions for the owner

1. **Accept engine-driven transfer's pickle path for parking deployments?**
   (a) Yes, by default, documented beside ADR 0033's "Local reach", with the host's
   `lmcache: off` as the remedy and an upstream issue drafted (not filed) asking for a
   payload format that is not pickle; (b) only behind a host opt-in, so deep deployments
   stay without LMCache unless the operator allows it; (c) not in 0.2.0.
   **Recommendation: (a).** The port is loopback-only, the audience runs single-user
   machines, the engine keeps its own key guard, and ADR 0028 accepted a wider pickle
   exposure for groups. (b) would leave "LMCache by default" off by default again.
2. **How much the automatic pool holds.** (a) One full context, at least 4 GiB; (b) two
   full contexts, at least 4 GiB; (c) one context, at least 1 GiB (smaller pools for
   short contexts); (d) a new field to choose N.
   **Recommendation: (a).** One context is the conversation that was running when the
   model parked; the 36 % above it absorbs other prompts; on GB10 a 32k Qwen3-4B pool is
   8 GiB, and two contexts would already hit the default cap. A stated `lmcache_pool`
   serves anyone who wants more.
3. **An automatic pool that does not fit the room left.** (a) Wait or refuse, as 0.1.8;
   (b) shrink to the room (whole GiB, at least 1 GiB) with the warning, else run without
   LMCache (`host_kv_room`); (c) run without LMCache whenever the whole pool does not fit.
   **Recommendation: (b).** An `auto` deployment should never be refused for its cache;
   a smaller cache is better than none, and the warning says what was given.
4. **Standalone's derived `host_kv_limit` (10 % of memory).** (a) Keep it; (b) raise it to
   20 % on unified memory.
   **Recommendation: (a).** It caps one pool at 12 GiB on a 128 GB GB10, enough for one
   55,900-token context of Qwen3-4B; the warning names the setting, and #193 made it
   settable. Raising it would let parked pools crowd out models.
5. **Transfer mode for `restart_only`.** (a) Keep `lmcache_driven`, with A13's charge and
   A15's bound; (b) engine-driven for every LMCache launch, if LC16's bounds pass and a
   restart-park row (LC14's protocol on `restart_only`) shows no held KV and no stops.
   **Recommendation: (b), conditional on those rows.** One transfer mode, no held KV, no
   alternating stops; A13 and A15 stay as guards for any server that maps engine memory.
6. **The `restart_only` advice.** (a) Keep telling deep-parking users to declare
   `restart_only` for LMCache; (b) drop the advice, keep the behaviour.
   **Recommendation: (b)**, as §5 says: the measurements show it slower after a park.
7. **`host_backed` on a discrete GPU.** (a) Approve an LMCache environment on a
   maintainer's discrete-GPU machine and run LC13–LC16 with `host_backed`; (b) leave
   `host_backed` unqualified in 0.2.0.
   **Recommendation: (b)** for 0.2.0, (a) when a discrete machine runs vLLM 0.32.0 or
   SGLang 0.5.22 anyway: the mechanism (sleep level 1, the KV discarded) is the one LC13
   tests, but the table is per GPU product.
8. **Hybrid model types.** (a) Qualify dense full-attention types only in 0.2.0; (b) ask
   for an environment with the first stable LMCache release that has multi-group
   engine-driven transfer (PR #4410) and qualify a hybrid type too.
   **Recommendation: (a)**, and (b) as soon as such a release is stable; a release
   candidate is not an environment to approve.
9. **Which model types beyond `qwen3`.** (a) Qwen3-4B only; (b) add the dense model types
   already on host A that the owner names.
   **Recommendation: (b)**, one more at least, so the table is not a single row; LC13 and
   LC14 per added type, LC15 and LC16 once per engine.

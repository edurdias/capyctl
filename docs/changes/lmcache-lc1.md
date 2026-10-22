# Status: LMCache 0.5.6 environments on host A and the LC1 measurements — 2026-10-22 (branch `docs/lmcache-envs`)

Owner approval 2026-10-21 (ADR 0033): two new environments on host A, now made. ADR 0033
§6 asks LC1 for the server's overhead beyond its pool, its GPU memory and its start time;
these were measured with LMCache and the engines run directly, without CapyCTL. This is
not the LC1 row: LC1 through CapyCTL is still needed. No code changes; there are no
tests. CPU and Fake-engine tests are not qualification.

**Release.** LMCache 0.5.6 (PyPI, 2026-10-15) is the newest 0.5.x release and the only
one at or above the 0.5.6 floor. It passes `cache_salt` to the server on both engines:

- SGLang, unified path: in `lmcache/integration/sglang/unified_lmcache_mp_connector.py`,
  `_create_key` (lines 600–624) builds `IPCCacheServerKey(cache_salt=operation.cache_salt)`
  for both lookups (`submit_lookup`, 626–641) and stores (`submit_store`, 1018–1038);
  `build_cache_salt` (453–459) uses the salt alone, or the SHA-256 of the salt and
  SGLang's extra key when the request has one. SGLang 0.5.22's
  `lmcache_unified_radix_cache.py` passes `req.cache_salt` (lines 853 and 946).
- vLLM: `lmcache_mp_metadata.py:85` takes `request.cache_salt`; `lmcache_mp_connector.py`
  passes it on retrieve (924, 936), store (1003, 1017) and lookup (1298, 1370, 1456).
- Server: `ipc_key_to_object_keys` (`lmcache/v1/distributed/api.py:608–649`) copies the
  salt into every `ObjectKey`.

SGLang 0.5.22 declares `enable_lmcache` and `lmcache_config_file` in
`sglang/srt/arg_groups/fields/memory.py`, not in `server_args.py`. The legacy
`sglang_adapter.py` and `multi_process_adapter.py` are still in the 0.5.6 package.

**Environments.** Each was made with its base's recipe (`uv venv --python 3.12 --seed
--managed-python`), constrained to the base environment's exact package list. Before
LMCache was added, the vLLM one matched its base exactly and the SGLang one differed only
in its seeded pip:

- `~/vllm-0.32.0-lmcache-venv`: `uv pip install vllm==0.32.0 --torch-backend=cu130`,
  then the `lmcache==0.5.6` aarch64 wheel. That wheel is built against torch 2.13.0,
  which is vLLM's torch.
- `~/sglang-0.5.22-lmcache-venv`: `uv pip install --prerelease=allow sglang==0.5.22`,
  then the build requirements (`ninja`, `setuptools<81`, `setuptools_scm`,
  `grpcio-tools==1.78.0`, `wheel`), then `lmcache==0.5.6` built from its source archive
  against torch 2.14.1+cu130 (`--no-build-isolation --no-binary lmcache`, CUDA 13.0
  `nvcc`, `sm_120` code).

Both import and see the GPU; torch, vLLM and SGLang did not change. LMCache's bounds
moved numpy 2.3.5 to 2.2.6, protobuf 7.x to 6.33.6, grpcio to 1.78.0,
OpenTelemetry 1.45.1 to 1.40.0 and prometheus-client 0.26.0 to 0.24.1. In the SGLang
environment they also moved setuptools 84.0.0 to 80.10.2 and fastapi 0.143.3 to 0.141.1.

**Measurements**, host A (GB10), Qwen3-4B. Every server ran as `lmcache server --host
127.0.0.1 --port P --transport zmq --http-host 127.0.0.1 --http-port H --l1-size-gb
{4,8} --eviction-policy LRU`. vLLM 0.32.0 connected through `LMCacheMPConnector` by
module path with autostart off; SGLang 0.5.22 used `--enable-lmcache` with a file of
`mp_host` and `mp_port`. Engine KV caches were held to about 7k tokens so that 3,000-token
prompts pushed earlier prompts out of the engine's own prefix cache.

- The server's resident memory beyond its pool was 781–794 MiB when ready, 801–814 MiB
  with an engine connected, and 807–824 MiB after the pool filled past its size and LRU
  evicted. It was the same for 4 GiB and 8 GiB pools. The maximum was **824.3 MiB**.
- The server's GPU memory (nvidia-smi, per process) was 170 MiB alone and 406–408 MiB
  with an engine connected. The maximum while an engine was up was **408 MiB**.
- From start to `GET /status` 200 took 2.90–4.80 s over 20 starts (5 per environment
  and pool size), and 3.1–3.8 s in the engine runs. The slowest was **4.80 s**.
- Only `127.0.0.1` was listened on, on both ports. SIGTERM stopped the server in
  1.8–6.7 s in 25 of 27 stops, and in 12.8 s and 13.7 s in the other two.
- **Allowances (ADR §6):** overhead 824.3 × 1.25 = 1030 MiB, rounded up to **1280 MiB**;
  device 408 × 1.25 = 510 MiB, rounded up to **512 MiB**; start bound 4.80 s × 3 = 14.4 s,
  raised to the 30 s minimum, so **30 s**.
- On GB10, per-process GPU memory is not counted in RSS but does come out of
  MemAvailable: allocating 2 GiB on the device left RSS unchanged and lowered
  MemAvailable by 2,096 MiB; 2 GiB of pinned host memory raised RSS by 2,049 MiB and
  left the GPU figure unchanged. On a unified-memory machine the server therefore costs
  its pool plus its RSS beyond the pool plus its GPU memory, with no overlap: at most
  1,232 MiB was measured, and the two allowances together give 1,792 MiB.
- On both engines, a prompt held by LMCache but no longer in the engine's prefix cache
  hit for 2,816 of its 3,000 tokens (11 chunks of 256), and none of them came from the
  engine's own cache. SGLang reported `storage: 2816, storage_backend: LMCache, device: 0`;
  vLLM reported a prefix cache hit rate of 0% and an external hit rate above 0%.
- The same prompt sent under salt A, then salt B, then with no salt never hit LMCache.
  Sent again under A, and then under B, after the engine had evicted it, it hit for
  2,816 tokens each time. The server's own `ObjectKey`s carried
  `cache_salt='lc1-salt-A'` and `'lc1-salt-B'`, with the same chunk hash but as separate
  objects. Its `lmcache_mp_lookup_hit` counters were labelled by salt.
- After its engine exits, the server keeps that engine's KV cache allocated through CUDA
  IPC. Its GPU memory rose to 1,430 MiB on vLLM (1 GiB of KV) and 1,656 MiB on SGLang,
  and was unchanged 92 s later. When a new engine connected to the same server (as on a
  `restart_only` wake), the figure dropped back to 406–408 MiB, so the server does not
  hold two engines' KV caches at once. The new engine hit, for 2,816 tokens, on prompts
  the first engine had stored. Until the server stops or another engine connects, a
  stopped engine's KV memory is not freed (ADR §7 parking and LC4).
- 0.5.6 defaults to `--l1-use-lazy` with `--l1-init-size-gb 20`. The pools measured
  here (4 and 8 GiB) were therefore allocated in full at start. A pool above 20 GiB
  would start at 20 GiB and grow, unless `--no-l1-use-lazy` is passed.
- Every server that had an engine connected left a 0600 `/dev/shm/cuda.shm.<uid>.<pid
  hex>.1` file (80 KiB, created by the CUDA driver) after its SIGTERM exit. Servers that
  never had an engine left nothing. No segment with an LMCache name appeared.

Evidence is in capyctl `target/live/lmcache-lc1/` (harness, logs, JSON results,
`summary.txt`). Host A was cleaned afterwards; the two environments were kept.

Live check still needed: LC1 through CapyCTL (the defaults above as the implementation
renders them), and LC2–LC11.

# Release note: none

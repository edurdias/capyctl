# ADR 0033 — LMCache: a private KV cache server per deployment instance

**Status:** Accepted (owner decisions 2026-10-21). Target release 0.1.7. Nothing is
implemented yet; the SPEC, guide and operations texts this ADR lists change with the
implementation (§14), as for ADRs 0030 and 0031.
**Amends (with the implementation):** `SPEC.md` §7.4 (a private cache service per
instance), §8.2 (the rendered connector is owned), §9.1 and §9.2 (deep parking with
LMCache only where qualified), §10 (`cache_salt` is no longer forwarded unchanged), §12
(the LMCache integration and its data lifetime), §15.1 (deployment fields `lmcache`,
`lmcache_pool`, `retention`; host setting `retention`), §20 (T45 and live rows LC1–LC6);
ADR 0014 §3 (SGLang's `enable_lmcache` and `lmcache_config_file` move from "owned until
designed" to rendered by CapyCTL); ADR 0010 decision 7 (this ADR is the measured claim
for one external cache integration that decision 7 leaves out of scope).
**Related:** ADR 0008 (installations and drift), ADR 0010 (park tiers), ADR 0012 (deep
parking and its protections), ADR 0013 (instances), ADR 0014 (engine configuration, A2
measured startup peak), ADR 0017 (capability gating), ADR 0018 (registration), ADR 0019
(memory domains), ADR 0028 (groups), ADR 0029 (llama.cpp), ADR 0030 (decision models),
ADR 0031 (custom engines; its loopback listener check).

## Context

[LMCache](https://github.com/LMCache/LMCache) stores the KV cache of request prefixes
outside the engine's GPU pool, so a prompt that begins like an earlier one is not
recomputed even after the engine's own prefix cache dropped it. For CapyCTL the point is
parking: with the cache held by a process that outlives the engine's GPU memory, a model
that wakes (or restarts) can answer its next request from a warm cache instead of a full
prefill.

CapyCTL keeps LMCache out today. ADR 0014 §3 reserves SGLang's `--enable-lmcache` and
`--lmcache-config-file` ("cache integrations are owned under SPEC §12 until designed");
vLLM's `--kv-transfer-config` is an approval option that "can reach off the host"; and
the data retention page promises that engine caches live in GPU memory and are discarded
when a model parks or stops. This ADR is the design SPEC §12 asks for, for one
integration.

What LMCache 0.5.6 provides (read 2026-10-21 against the tag; sources inline):

- **Release.** [v0.5.6](https://github.com/LMCache/LMCache/releases/tag/v0.5.6), published
  2026-10-14 (tag commit `3e11e10812b448142c6cdb177a3058652bd826fa`), is the latest
  stable release. It fixes device-IPC descriptor pickle deserialization,
  [CVE-2026-105192 / GHSA-w5f2-9q9c-ff4h](https://github.com/LMCache/LMCache/security/advisories/GHSA-w5f2-9q9c-ff4h),
  so no older release is acceptable. Its wheels exist for aarch64 and x86_64, CPython
  3.10–3.13, CUDA 13 on the main channel; they are built against torch 2.13.0 and depend
  on an unpinned `torch` ([pyproject](https://github.com/LMCache/LMCache/blob/v0.5.6/pyproject.toml)).
  vLLM 0.32.0 pins torch 2.13.0; SGLang 0.5.22 pins torch 2.14.1, so for SGLang LMCache
  is built against the installed torch (`--no-build-isolation`).
- **Two ways to run it.** In process, as vLLM's bundled `LMCacheConnectorV1`, where the
  engine's workers own the cache; or as a separate multiprocess (MP) server, `lmcache
  server`, which engines connect to: vLLM through LMCache's external
  `LMCacheMPConnector`, SGLang 0.5.22 through `--enable-lmcache --lmcache-config-file`
  ([SGLang's LMCache README](https://github.com/sgl-project/sglang/blob/v0.5.22/python/sglang/srt/mem_cache/storage/lmcache/README.md)).
  Only the MP server can outlive the engine's GPU memory.
- **Salt.** The MP object key is `(chunk_hash, model_name, kv_rank, object_group_id,
  cache_salt)` ([api.py](https://github.com/LMCache/LMCache/blob/v0.5.6/lmcache/v1/distributed/api.py)).
  The vLLM MP connector takes `request.cache_salt` into it
  ([lmcache_mp_metadata.py](https://github.com/LMCache/LMCache/blob/v0.5.6/lmcache/integration/vllm/lmcache_mp_metadata.py)),
  and SGLang 0.5.22 passes the salt on lookup and store
  ([lmcache_unified_radix_cache.py](https://github.com/sgl-project/sglang/blob/v0.5.22/python/sglang/srt/mem_cache/storage/lmcache/lmcache_unified_radix_cache.py)).
  The in-process `LMCacheConnectorV1` ignores `cache_salt`: two requests with different
  salts share cached chunks ([LMCache issue #2878](https://github.com/LMCache/LMCache/issues/2878),
  closed by the stale bot as not planned). An MP salt is at most 128 characters and must
  not contain `@`, `/`, `\` or NUL; CapyCTL accepts any salt of up to 1024 bytes today
  (SPEC §10).
- **Admin HTTP.** The MP server has a request port (ZMQ) and an HTTP port that serves
  `/status`, `/metrics`, `/config`, `/env`, `/quota` and the `/cache` operations (clear,
  enumerate, delete). In 0.5.6 that HTTP API has no authentication
  ([http_api.rst](https://github.com/LMCache/LMCache/blob/v0.5.6/docs/source/mp/http_api.rst)).
  `--run-script-api-enabled` (off by default) adds a route that runs caller-supplied
  Python. Later documentation describes an API key file; 0.5.6 has none.
- **Tiers and limits.** The MP server keeps an L1 pool of pinned host RAM
  (`--l1-size-gb`), and optionally L2 adapters (filesystem, Redis, S3, Mooncake, NIXL),
  GDS or Device-DAX L1, and P2P. Eviction is `LRU`, `ARC`, `IsolatedLRU` or `noop`;
  `IsolatedLRU`'s per-salt quotas are soft (writes are not refused, over-budget data is
  evicted at the next cycle, about a second later)
  ([configuration](https://docs.lmcache.ai/mp/configuration.html)). The L1 size is the
  pool, not the process's whole footprint.
- **Metrics.** With `lmcache server`, Prometheus text is served at `/metrics` on the HTTP
  port, with `lmcache_mp_l1_memory_usage_bytes`, `lmcache_mp_l1_usage_ratio`,
  `lmcache_mp_l2_usage_bytes`, `lmcache_mp_inflight_load_memory_usage_bytes`, L1
  allocation-failure counters and lookup and hit counters labelled by model and salt
  ([metrics.rst](https://github.com/LMCache/LMCache/blob/v0.5.6/docs/source/mp/observability/metrics.rst)).
- **Parking.** vLLM's sleep and wake do not shut LMCache down, and SGLang 0.5.22's memory
  release resets its radix tree without clearing the LMCache connector; neither performs
  a dedicated unregister and register of LMCache's GPU buffers around sleep. Sleep with
  engine-driven transfer and hybrid KV groups is open upstream
  ([LMCache issue #4071](https://github.com/LMCache/LMCache/issues/4071); its fix, PR
  #4410, was awaiting review in October). A cache that survives a park is therefore
  architecturally possible with the MP server and unproven for any given engine, model
  and machine.
- **Engines without an integration.** LMCache 0.5.6 has no llama.cpp integration and none
  for TensorFold. A custom engine (ADR 0031) is a command CapyCTL cannot render a
  connector into.
- **Cost.** No startup time or throughput figure for these engines on this hardware is
  published. An open report of MP CPU spikes
  ([issue #4797](https://github.com/LMCache/LMCache/issues/4797)) and an old low-reuse
  slowdown ([issue #1326](https://github.com/LMCache/LMCache/issues/1326)) are not
  predictions for 0.5.6; default-on is not free, and only measurement says by how much.

## Decision

### 1. Version and integration path

CapyCTL integrates **LMCache 0.5.6** (the security floor: CVE-2026-105192 is fixed in it)
in **multiprocess server mode only**: CapyCTL starts `lmcache server` itself, and the
engine connects to it.

- **vLLM** connects through LMCache's external `LMCacheMPConnector`, named with its module
  path, `kv_connector_module_path: lmcache.integration.vllm.lmcache_mp_connector`, so a
  connector of the same name bundled with vLLM is never picked instead.
- **SGLang 0.5.22** connects through `--enable-lmcache --lmcache-config-file <file>`.
- The in-process **`LMCacheConnectorV1` is refused**: it ignores `cache_salt` (issue
  #2878), so it would silently share one tenant's cached prefixes with another's, and its
  cache dies with the engine.

LMCache is installed in the engine's own environment (`pip install lmcache==0.5.6` for
vLLM; built against SGLang's torch with `--no-build-isolation` for SGLang). CapyCTL never
installs it (SPEC §15.2). `capyctl engine add` reads the `lmcache-*.dist-info` of the
registered environment and records LMCache's version and a `sha256:` digest of its files
on the profile, as ADR 0008 does for the engine package. Every launch measures it again:
a different digest is drift, flagged and journaled, and handled by the profile's
`security.installation_drift` (warn by default: LMCache is used and the drift shown;
`refuse`: the launch is refused `installation_drift`). A package installed after
registration is not loaded; the reason says to register the engine again. The server runs
from that environment (`<environment>/bin/lmcache`).

### 2. Supported engines

| Engine | LMCache | Shown when off |
|---|---|---|
| vLLM 0.32.0 or newer | yes | |
| vLLM 0.31 and older | no | `engine_version_unsupported`: "LMCache needs vLLM 0.32.0 or newer" |
| SGLang 0.5.22 | yes | |
| SGLang other than 0.5.22 | no | `engine_version_unsupported`: "LMCache is supported on SGLang 0.5.22" |
| llama.cpp | no | `engine_unsupported`: "llama.cpp has no LMCache integration" |
| TensorFold | no | `engine_unsupported`: "TensorFold has no LMCache integration" |
| custom engine | no | `engine_unsupported`: "CapyCTL does not render options for a custom engine" |

The engine version is the registered build (ADR 0018). An unsupported engine is never an
error by itself: its deployments run as they do today, and status and `capyctl engine
list` say "LMCache off" with the reason (§8).

Within a supported engine, LMCache is also off with a reason for a layout it does not
serve in 0.1.7: a deployment with a `topology` (`group_unsupported`: one private server
on one machine cannot serve the ranks of a group across machines, ADR 0028), and the
model layouts open question 5 settles (`layout_unsupported`).

### 3. Topology: one private server per instance

Each instance of a deployment (ADR 0013) that uses LMCache gets **its own LMCache
server**, started and stopped by CapyCTL with the instance. Two instances of one
deployment have two servers and two pools; no server is shared between deployments.

The server is a CapyCTL-owned process, held as the engine is (SPEC §8.1): its own process
group as the role's service user, recorded in the launch journal before it starts, stopped
by SIGTERM to the group with a verified exit before its memory is released. Its argument
vector is CapyCTL's alone; no deployment, host or profile setting adds to it in 0.1.7:

```
<environment>/bin/lmcache server \
  --host 127.0.0.1 --port <leased> --transport zmq \
  --http-host 127.0.0.1 --http-port <leased> \
  --l1-size-gb <pool in GiB> --eviction-policy LRU
```

- **Loopback only, on ports CapyCTL chooses.** Both ports are leased from the host's
  `endpoint_port_range` like an engine's port. The admin HTTP API has no authentication
  in 0.5.6, so the server must never be reachable off loopback. After the server answers
  `GET /status` on its HTTP port, the host runs ADR 0031 §7's listener check on the
  server's process group (its socket inodes in `/proc/<pid>/fd` against `/proc/net/tcp`
  and `tcp6`): any listener on an address other than `127.0.0.1` or `::1` stops the server
  with gone evidence, and LMCache is off for that launch with reason `listener_exposed`
  (or the launch fails, under `lmcache: required`). The check runs again whenever the host
  re-proves the launch. As in ADR 0031, it narrows the exposure; it does not prove a
  listener opened later absent.
- **No script API.** `--run-script-api-enabled` is never passed; 0.5.6 defaults it off.
- **Host RAM (L1) only.** No L2 adapter, no disk, GDS, Device-DAX, remote or P2P tier in
  0.1.7, so nothing LMCache holds reaches disk. `--l1-use-lazy` is not passed: the pool is
  allocated at start, and the charge (§6) covers it from the start.
- **Eviction is LRU within the private pool.** Only the instance's own requests evict its
  entries; no quota is registered (with `LRU` LMCache ignores quotas).
- **No autostart.** vLLM's connector is rendered with `lmcache.mp.autostart: false`: an
  engine must not start a server CapyCTL does not own (SPEC §8.3).
- **Closed environment.** The server gets the environment's `PATH`, `HOME`, the
  instance's `CUDA_VISIBLE_DEVICES` and nothing else: no engine key, no Hugging Face
  token, no `LMCACHE_*` variable. `/env` shows its environment to any local process, so
  nothing secret may be in it. Optional lookup-hash and event logs stay off.
- **Logs.** The server's standard output and error go to the launch's engine log through
  the redacting writer, under the host's `engine_logs` mode (§9 forces `memory`).

**Order.** Activation reserves the engine and the server together (§6), then starts the
server, waits for `GET /status` within the start bound (open question 4), runs the
listener check, and only then launches the engine with the connector. A server that does
not come up is stopped (verified gone) and the engine launches without the connector:
LMCache off, reason `server_start_failed`.

**Local reach.** Any process on the machine can reach both ports, as it can reach a
llama.cpp or TensorFold engine today (ADR 0029 §4): it can read the configuration,
enumerate cached object keys, clear the cache, and through the request port store or look
up KV under any salt. KV is request-derived data. On a machine with untrusted local users,
set `lmcache: off` or `retention: zero` (§9). vLLM and SGLang themselves keep their
per-launch keys and ADR 0012's protections unchanged.

**Lifetime.** The server stops with the instance's stop (operator stop, idle stop, a stop
as part of a switch, deletion) and with a host's verified cleanup of the instance. A new
revision of the deployment launches with a new server; the old revision's server stops
with its instance, so no cached entry crosses a revision (SPEC §12: isolate by checkpoint
revision). Parking keeps it (§7).

### 4. Rendering

**vLLM** (0.32.0 or newer), on argv:

```
--kv-transfer-config '{"kv_connector":"LMCacheMPConnector",
  "kv_connector_module_path":"lmcache.integration.vllm.lmcache_mp_connector",
  "kv_role":"kv_both",
  "kv_connector_extra_config":{"lmcache.mp.host":"127.0.0.1",
    "lmcache.mp.port":<port>,"lmcache.mp.autostart":false}}'
```

**SGLang 0.5.22**: `--enable-lmcache --lmcache-config-file <file>`, where `<file>` is an
owner-only file in the launch's private runtime directory holding exactly
`mp_host: 127.0.0.1` and `mp_port: <port>`, removed with the launch's cleanup.

Both are owned settings: a deployment cannot set them (§10). The effective configuration
(`inspect deployment --effective-config`) shows them with provenance `capyctl lmcache`.

### 5. Salt: a fixed digest, the same for the engine and LMCache

CapyCTL no longer forwards `cache_salt` unchanged. For a request that carries one, the
router sends the engine the **effective salt**: the lowercase hexadecimal SHA-256 digest,
64 characters, of

```
"capyctl/cache-salt/v1" 0x00
u32be(len(deployment_id)) deployment_id
u64be(revision)
u32be(len(caller_salt)) caller_salt
```

where `deployment_id` is the deployment's durable ID, `revision` the revision the serving
launch runs (both read from the launch the router forwards to, as it reads the engine
family today), and `caller_salt` the request's salt as UTF-8 bytes. The engine passes the
same value to LMCache, so the engine's own prefix cache and LMCache are partitioned
alike. It is the same on every engine that honours a salt, with or without LMCache.

- The caller's salt keeps its contract: a non-empty string of at most 1024 bytes, else
  `400 invalid_request`. Every digest fits LMCache's 128 characters and character set, so
  no salt is truncated, refused by LMCache or turned into no salt.
- **No salt stays no salt.** A request without `cache_salt` is sent without one, and
  shares the cache with every other unsalted request to that deployment (its instance's
  engine cache and its private server).
- Different deployments, or revisions, never share an effective salt; within a revision,
  equal caller salts share and different ones never do.
- **A salt partitions a cache; it does not authorize anything.** Any caller holding the
  API key may send any salt, as today. The caller is trusted.
- Engines that ignore a salt keep refusing it (`cache_salt_unsupported`, SPEC §10).
- The effective salt is never logged or stored by CapyCTL. LMCache's hit counters carry
  it as a label on loopback; CapyCTL sums over that label and never re-exports it (SPEC
  §17).

### 6. Pool size and accounting

The L1 pool is **4 GiB per instance by default**; a deployment sets another with
`lmcache_pool` (whole GiB, at least 1 GiB). It is rendered as `--l1-size-gb`.

An instance's reservation charges, in the host's system memory domain (ADR 0019; on a
unified-memory machine the same domain the GPU uses), **the pool plus the server's
measured overhead**:

- Before the server's first start the overhead is a fixed allowance (open question 4).
  The host samples the server's process group's resident memory as it samples the
  engine's (ADR 0014 A2) and records the peak overhead above the pool with the deployment
  (`lmcache.measured` in status JSON); later launches charge the pool plus that figure.
- On a discrete GPU, any device memory the server's own CUDA context holds is measured
  per process like the engine's and charged to the device domain.
- The pool counts against the host's `host_kv_limit` where one is declared (SPEC §7.4),
  and in the parked footprint while the model is parked (§7).
- Observed use above the charge is a degraded condition, as SPEC §7.3 requires; it is
  never hidden.
- A deployment whose memory CapyCTL sizes from the checkpoint (ADR 0014) includes the charge;
  `memory.request` stays the engine's own GPU memory.

LMCache's own limits are soft (the L1 size bounds the pool, not the process; quotas never
refuse a write) and never stand in for CapyCTL's reservations. Admission decides with the
charge above.

### 7. Parking

**The server stays up while its model is parked**, and its pool stays charged (SPEC §7.3:
"retained host-cache allocations remain charged"). It stops with the instance's stop.

**Deep parking with LMCache only where qualified.** Deep park with the connector is
allowed only for a combination of engine version, model type and hardware that live row
LC4 qualified on host A: park, wake, and a cache hit after the wake gives the same answer
as without the cache (issue #4071 is why this is not assumed). The combinations ship as a
table in the release: the exact registered engine version, the checkpoint's `model_type`,
and the GPU's product name as the host's inventory reports it. The table starts with the
combinations LC4 passes; nothing else is in it.

**Unqualified combinations run `restart_only`.** A deployment whose residency would
resolve `deep` by default, and that uses LMCache on a combination outside the table,
resolves `restart_only`, with provenance `lmcache_deep_unqualified` in the effective
configuration and the reason in status. Parking stops the engine and keeps the server;
waking launches the engine again on the same server, so the cache stays warm across the
restart. (A declared `residency: deep` is open question 1.)

`host_backed` is unchanged (refused where memory is unified, ADR 0010). On a host with
`security.deep_park: disabled` every deployment is `restart_only` already.

### 8. Default on; `off` and `required`

A deployment's `lmcache` is `auto` (the default), `off` or `required`:

- **`auto`.** Every deployment that can use LMCache gets it. When it cannot — the
  engine or its version is unsupported, the package is missing or below 0.5.6, the
  layout is unsupported, an approved `--kv-transfer-config` is in `extra_args` (§10),
  retention is zero (§9), the host predates this ADR, or the server does not start or
  fails the listener check — the deployment runs without it. `capyctl status deployment
  <name>` and `capyctl engine list` show "LMCache off" with the reason, and the event
  `lmcache_off` records the instance and reason once per launch.
- **`off`.** No server, no connector; status shows "LMCache off: turned off by the
  deployment".
- **`required`.** The deployment refuses to start instead of running without it:
  reasons known at resolution or placement refuse it there, with
  `lmcache_unavailable:<reason>` (or `retention_zero:lmcache`,
  `host_capability_missing:lmcache`); reasons found at launch (`server_start_failed`,
  `listener_exposed`) fail the launch with the same code, and nothing is left running.

`lmcache_pool` beside `lmcache: off` is contradictory and refused (`invalid_config`).

**Status.** `capyctl status deployment <name>` gains a line per instance:

```
LMCache   on: pool 4 GiB, used 1.3 GiB, overhead 0.8 GiB (measured), 212 hits / 260 lookups
LMCache   off: LMCache needs vLLM 0.32.0 or newer
```

and the JSON view `lmcache` (`state`, `reason`, `pool_bytes`, `used_bytes`,
`overhead_bytes`, `measured`, `lookups`, `hits`). `capyctl engine list` gains a column
`LMCACHE`: the version (`0.5.6`), or `off (<reason>)` for a profile that cannot use it.

### 9. Zero data retention

**Host setting** `retention`: `standard` (the default) or `zero` (standalone:
`host.retention`; `--set retention=zero`, `CAPYCTL_SET__RETENTION=zero`). With `zero`:

- engine logs are kept in memory (`engine_logs: memory`); a document that states
  `engine_logs: files` with `retention: zero` refuses the start, and
  `--debug-engine-logs` is refused as it is with `engine_logs: memory`;
- LMCache is never started on the host: an `auto` deployment runs without it (reason
  `retention_zero`), and a deployment cannot re-enable it: `lmcache: required` is refused
  `retention_zero:lmcache` at placement;
- turning it on stops existing LMCache servers. A role that starts with `retention: zero`
  and finds LMCache servers it owns drains each instance using one, stops its engine and
  its server (verified gone), and relaunches it without LMCache; a parked instance using
  one is released like an idle stop, so it stays on-demand eligible (T10) and its next
  start has no LMCache. The servers held RAM only, so their exit leaves nothing on disk
  to purge. The journal records each stop; the event `lmcache_off` carries
  `retention_zero`.

**Deployment setting** `retention: zero`, for a host that does not set it: the
deployment's launches keep their engine output in memory whatever the host's
`engine_logs`, and never use LMCache (`lmcache: required` beside it is refused
`retention_zero:lmcache`). It is part of the revision. A host that predates this ADR is
refused at placement, `host_capability_missing:retention`, since it would not honour it.

**What zero retention does not cover**, stated in the guide and the data retention page:

- **The engine's own prefix and KV cache in GPU memory.** vLLM and SGLang keep it for the
  life of the process; it is written nowhere and dies with the process, and it is
  partitioned by salt. Zero retention does not turn it off.
- Whatever a custom engine's manifest makes it write (ADR 0031 §8).
- Operating-system crash dumps and swap (open question 9).

### 10. Engine options and environment

The engine-flags table changes with the implementation:

- **SGLang**: `--enable-lmcache` and every `--lmcache-*` option become owned (class
  `owned` in `capyctl engine flags`, reserved in the guide's table): CapyCTL renders them
  from `lmcache`. `--enable-hierarchical-cache`, `--hicache-storage-*` and `--flexkv-*`
  stay reserved as today.
- **vLLM**: `--kv-transfer-config` naming an LMCache connector (`LMCacheConnectorV1`,
  `LMCacheMPConnector`, any `kv_connector_module_path` under `lmcache.`, or either inside
  a `MultiConnector`) is refused from `extra_args` in every spelling (`invalid_config`
  naming `lmcache`), because CapyCTL renders it. Another connector stays an approval
  option as today; a deployment that passes one runs without LMCache (reason
  `kv_transfer_config_set`, since the engine takes one connector configuration), and
  `lmcache: required` beside it is refused `lmcache_unavailable:kv_transfer_config_set`.
- **Environment**: `LMCACHE_*` is reserved for the engine's environment
  (`engine_env_reserved:<name>`) on every engine, since the 0.5.6 vLLM helper reads
  configuration from the environment after any file.

### 11. Metrics read

The host reads the server's `/metrics` on its loopback HTTP port when it samples the
engine's load, and shows: `lmcache_mp_l1_memory_usage_bytes` (used), `lmcache_mp_l1_usage_ratio`,
`lmcache_mp_inflight_load_memory_usage_bytes`, the L1 allocation-failure counters, and the
lookup and hit counters summed over their model and salt labels. `lmcache_mp_l2_usage_bytes`
is expected absent or zero (no L2); a value above zero is a degraded condition. A missing
or unreadable figure is unknown, never zero. These are cache figures, not the process's
footprint; the charge (§6) comes from the measured process memory.

### 12. Host capabilities

A host that implements this ADR declares `lmcache` and `retention` (ADR 0017). The server
places `lmcache: required` only on a host that declared `lmcache`
(`host_capability_missing:lmcache`), and `retention: zero` only on one that declared
`retention`. An `auto` deployment on an older host runs as in 0.1.6, and status shows
"LMCache off" with reason `host_capability_missing`.

### 13. Codes and reasons

| Where | Code | When |
|---|---|---|
| resolution, placement | `lmcache_unavailable:<reason>` (exit 5) | `lmcache: required` and a reason below applies |
| placement | `retention_zero:lmcache` (exit 5) | `lmcache: required` where the host or the deployment has `retention: zero` |
| placement | `host_capability_missing:lmcache`, `host_capability_missing:retention` (exit 5) | as §12 |
| launch | `lmcache_unavailable:server_start_failed`, `lmcache_unavailable:listener_exposed` | under `required`, at launch |
| deploy | `invalid_config` | `lmcache_pool` invalid or beside `lmcache: off`; an LMCache `--kv-transfer-config`, `--enable-lmcache` or `--lmcache-*` in `extra_args` |
| deploy | `engine_env_reserved:LMCACHE_*` | an `LMCACHE_*` engine variable |
| role start | `invalid_config` | `retention: zero` with `engine_logs: files` |

Reasons (status, engine list, `lmcache_off`): `disabled`, `engine_unsupported`,
`engine_version_unsupported`, `not_installed`, `not_registered`, `version_unsupported`
(below 0.5.6), `layout_unsupported`, `group_unsupported`, `kv_transfer_config_set`,
`retention_zero`, `host_capability_missing`, `server_start_failed`, `listener_exposed`.

### 14. Texts that change with the implementation

**SPEC §10**, the `cache_salt` note: "The router forwards it unchanged to vLLM and
SGLang." becomes "The router does not forward it unchanged: it sends vLLM and SGLang the
effective salt, the 64-character hexadecimal SHA-256 digest of a domain-separated
encoding of the deployment's ID, the revision the serving launch runs and the caller's
salt (ADR 0033 §5), which the engine also passes to LMCache. A request without
`cache_salt` is sent without one and shares the deployment's cache with every other
unsalted request. A salt partitions a cache; it authorizes nothing, and the caller is
trusted."

**SPEC §7.4**, after its second paragraph: "LMCache (ADR 0033) runs as a private server
per deployment instance, never shared: its pool (4 GiB by default) and its measured
overhead are charged to the instance in the system domain, count against `host_kv_limit`
and stay charged while the model is parked. LMCache's soft limits never replace these
reservations."

**SPEC §9.1 and §9.2**, after the deep-park paragraphs: "With LMCache (ADR 0033), deep
parking is allowed only for an engine version, model type and GPU combination qualified
live; any other combination with LMCache resolves `restart_only`, and its LMCache server
survives the restart."

**SPEC §12**, a note: "LMCache 0.5.6 is integrated in multiprocess server mode only, host
RAM tier only, one private loopback server per instance, on by default for supported
engines (ADR 0033). It holds request-derived KV in RAM while its model runs or is parked,
and nothing on disk; `retention: zero` keeps it off."

**SPEC §8.2**: the LMCache connector settings (§10 above) join the reserved settings.
**SPEC §15.1**: the deployment fields `lmcache`, `lmcache_pool` and `retention` and the
host setting `retention`. **SPEC §20**: T45 and the LC rows (Verification). **SPEC §21**:
the amendment entry.

**`docs/operations/data-retention.md`**: the summary gains an "LMCache" row (KV of
recent request prefixes, keyed by salt digest; the host RAM of the instance's private
loopback server; while the model runs or is parked, gone when it stops; turn off with
`lmcache: off` or `retention: zero`). The "Engine logs" row names `retention: zero` beside
`engine_logs: memory`. The "Engine caches" row and section keep GPU memory and add that
zero retention does not cover them. The salt paragraph says CapyCTL sends a digest of
the deployment, its revision and the salt, not the salt itself. A new section, "Zero data
retention", gives §9, and the opening paragraph names LMCache's RAM as the one place
request-derived data outlives a request besides the engine's own GPU caches.

**`docs/guide/engine-flags.md`**: the rows of §10 (vLLM "KV offload, LMCache" and SGLang
"KV offload, LMCache" split so the LMCache options read owned), and the environment note.

**Also**: `docs/operations/configuration.md` (host `retention`, standalone
`host.retention`), `docs/guide/configuration.md` and `deploy.md` (the deployment fields),
`docs/guide/requests.md` (the salt section), `docs/guide/engines.md` (installing LMCache
in an engine environment, the `LMCACHE` column, a qualified-combinations list),
`docs/guide/parking.md` (what parks with LMCache) and `docs/guide/errors.md` (§13's
codes).

## Consequences

- A second CapyCTL-owned process per instance: ownership, journal, ports, listener check,
  measurement, metrics and cleanup for the server; a connector rendering per engine; a
  salt derivation in the router; the qualified-combinations table; one host setting and
  three deployment fields; the `lmcache` and `retention` capabilities.
- Default on costs host RAM: N instances that use LMCache hold N × (4 GiB + overhead),
  parked or not. On a 128 GB unified machine that is memory the next model cannot use;
  admission accounts for it rather than discovering it.
- On a combination the table does not list, a default deployment that parked `deep` in
  0.1.6 parks by restarting in 0.1.7: a cold engine start on wake (a minute or more, where
  deep wake took a median 7.5 s on host A, ADR 0010), with a warm cache after it.
  `lmcache: off` keeps the 0.1.6 behaviour. Status names the reason.
- The router's salt change alters what vLLM and SGLang receive for every salted request,
  with or without LMCache; the existing test that the salt reaches them byte for byte
  changes to the digest.
- KV derived from requests now lives in host RAM in a process any local user can reach
  on loopback (§3). Zero retention and `lmcache: off` are the remedies; the guide says so.
- Discrete-GPU machines get LMCache by default without a live row; the LC rows run on
  host A only. CPU and Fake-engine tests are not qualification.

## Alternatives considered

- **The in-process `LMCacheConnectorV1`.** Refused (owner decision): it ignores
  `cache_salt` (issue #2878) and its cache dies with the engine, so it cannot survive a
  park.
- **One shared server per host.** Rejected for 0.1.7 (owner decision): a shared LRU pool
  lets one deployment evict a parked one's cache, `IsolatedLRU` quotas are soft, and
  sharing needs reference-counted ownership across deployments (SPEC §12). A private
  server makes the charge, the lifetime and the eviction one instance's.
- **Forward the salt unchanged, or truncate it.** Rejected: LMCache refuses salts over 128
  characters or with `@`, `/`, `\`; truncation would merge partitions, and mapping a bad
  salt to none would silently share.
- **Disk or remote tiers.** Out of 0.1.7 (owner decision): they write request-derived KV
  to disk or off the host, need quotas outside LMCache's soft limits, and change the
  retention story.
- **Off by default.** Rejected (owner decision): the warm cache across parking is the
  point; `lmcache: off` and `retention: zero` opt out.
- **Fail the deployment whenever LMCache cannot be used.** Rejected for the default
  (owner decision); `lmcache: required` gives it to whoever wants it.

## Verification

A new conformance row, **T45 — LMCache**: the supported-engine table resolves on, and off
with each reason, for every engine and version of §2; `lmcache: off` renders no server
and no connector; `required` is refused with `lmcache_unavailable:<reason>`,
`retention_zero:lmcache` or `host_capability_missing:lmcache` before anything starts, and
a launch-time failure leaves nothing running; the server's argument vector is exactly
§3's (loopback hosts, leased ports, `LRU`, no script API, no lazy L1, no L2), its
environment closed; the server starts, answers `/status` and passes the listener check
before the engine launches, and an off-loopback listener stops it
(`listener_exposed`); the vLLM `--kv-transfer-config` names `LMCacheMPConnector` with its
module path and `autostart: false`, and SGLang's config file holds only host and port;
LMCache connectors in `extra_args`, `--enable-lmcache`, `--lmcache-*` and `LMCACHE_*` are
refused, and another approved connector turns LMCache off; the effective salt is §5's
digest (fixed vectors), identical for equal inputs, different across deployments,
revisions and salts, absent without a salt, never logged; the reservation charges the
pool plus the allowance, then the measured overhead, in the system domain, against
`host_kv_limit`, and stays charged while parked; the server stays up through a park and
stops with the stop, with verified exit before release; a new revision gets a new
server; deep resolves `restart_only` with provenance outside the qualified table and
stays deep inside it; `retention: zero` forces memory engine logs, refuses `engine_logs:
files`, keeps LMCache off, and at role start stops existing servers and relaunches their
instances without LMCache; status, JSON and `engine list` show on or off with the reason,
and `lmcache_off` is recorded. Tests also tag T14, T24, T25, T35, T36 and T37 where they
apply.

Live rows, on host A with `~/vllm-0.32.0-lmcache-venv` (a copy of the approved vLLM
0.32.0 environment with LMCache 0.5.6) and `~/sglang-0.5.22-lmcache-venv` (a copy of the
approved SGLang 0.5.22 environment with LMCache 0.5.6 built against its torch 2.14.1),
owner approved 2026-10-21 and listed in `AGENTS.md`, with Qwen3-4B on each engine:

| Row | What it shows |
|---|---|
| LC1 | Start with cache: each engine's deployment starts with LMCache on by default; the server listens only on loopback (listener check and `ss`), `/metrics` reads, the charge shows pool plus measured overhead, and status and `engine list` show it on. |
| LC2 | Cache hit after an identical prefix: a long prompt, then a second request with the same prefix after the engine's own prefix cache no longer holds it; LMCache's hit counter rises, and at temperature 0 the answer equals the one a `lmcache: off` deployment of the same model gives. |
| LC3 | Salts isolate: the same prefix under salt A, then salt B, then no salt: no LMCache hit and no cached tokens across different salts or between a salt and none; salt A again hits. |
| LC4 | Park and wake per engine: `restart_only` with LMCache (the server stays up and charged while parked; the first request after the wake hits from LMCache with the same answer); then `deep` with LMCache for the combination under test: park, wake, a hit with the same answer as without the cache. A pass adds the combination to the qualified table; a failure leaves it `restart_only`. |
| LC5 | Zero retention: a host with `retention: zero` refuses `lmcache: required` (`retention_zero:lmcache`), runs an `auto` deployment with LMCache off (`retention_zero`) and writes no engine log file; turning it on with a server running stops that server (verified exit) and relaunches its instance without LMCache. |
| LC6 | Missing package: a deployment on the approved environment without LMCache (`~/sglang-0.5.22-venv` as registered) runs with LMCache off (`not_installed`) in status, `engine list` and an `lmcache_off` event; `lmcache: required` on it is refused. |

## Owner decisions of 2026-10-21

1. **Version and path.** LMCache 0.5.6 (security floor: CVE-2026-105192 fixed), MP server
   mode only, through vLLM's external `LMCacheMPConnector` with an explicit module path
   and SGLang 0.5.22's `--enable-lmcache --lmcache-config-file`; the in-process
   `LMCacheConnectorV1` is refused (it ignores `cache_salt`, issue #2878). Supported: vLLM
   0.32.0 or newer and SGLang 0.5.22; vLLM 0.31, llama.cpp, TensorFold and custom engines
   are not, shown off with the reason (§§1–2).
2. **Topology.** One private server per deployment instance, started and stopped with it,
   loopback-only on CapyCTL-chosen ports, never reachable off loopback (ADR 0031's
   listener check), script API off, host RAM (L1) only, LRU within the private pool
   (§3).
3. **Parking.** The server stays up while its model is parked (its pool charged) and
   stops with the stop. Deep park with LMCache only for combinations qualified on host
   A; unqualified combinations run `restart_only`, with the server surviving the restart
   (issue #4071) (§7).
4. **Salt.** A fixed 64-hex SHA-256 digest, domain-separated, of the deployment revision
   identity and the caller's salt, sent to the engine and LMCache alike; no salt stays no
   salt; SPEC §10 amended; salt partitioning is not authorization (§5).
5. **Size.** 4 GiB L1 pool per deployment by default, settable per deployment; the charge
   is pool plus measured overhead in host RAM (the GPU's domain on unified memory);
   LMCache quotas never substitute for CapyCTL reservations (§6).
6. **Default on.** Every supported deployment uses LMCache unless it sets `lmcache: off`;
   if LMCache cannot be used it runs without, shown off with the reason and an event;
   `lmcache: required` refuses to start instead (§8).
7. **Zero data retention.** Host `retention: zero` (engine logs in memory, LMCache never
   started, deployments cannot re-enable either, turning it on stops existing servers)
   and a per-deployment `retention: zero`; engine-native GPU prefix caches are not
   covered (§9).
8. **Environments.** `~/vllm-0.32.0-lmcache-venv` and `~/sglang-0.5.22-lmcache-venv` on
   host A for qualification, added to `AGENTS.md`.

## Open questions

Details the owner did not decide; each has the proposal this ADR would follow.

1. **A declared `residency: deep` on an unqualified combination.** Decision 3 covers the
   defaulted residency. Proposed: a declared `deep` is honoured and LMCache is off for
   that deployment (reason `deep_unqualified`); with `lmcache: required` beside it, the
   deployment is refused `lmcache_unavailable:deep_unqualified`.
2. **An engine that fails to start with the connector.** Decision 6 lists the server's
   failure, not the engine's. Proposed: the launch fails as any engine start failure; no
   automatic retry without LMCache (SPEC §7.5 forbids improvised reconfiguration), and the
   failure reason suggests `lmcache: off`.
3. **The server exits while its engine runs.** Proposed: the instance fails like an engine
   exit and recovers under SPEC §13.2; its next launch starts a new server.
4. **Numbers.** Proposed: an overhead allowance of 2 GiB host RAM (and 1 GiB device memory
   on a discrete GPU) until the first measurement; a server start bound of 120 s; pools in
   whole GiB from 1 GiB.
5. **Layouts.** Proposed: LMCache by default only for full-attention decoder layouts
   (dense and mixture-of-experts); hybrid layouts (Mamba or gated-delta-net state, linear
   or sliding-window attention) are off with `layout_unsupported` until a live row
   qualifies one (issue #4071 concerns hybrid KV groups).
6. **Decision deployments** (`api: decisions` on SGLang 0.5.22 or vLLM 0.32). Proposed:
   off with reason `decisions_unsupported` in 0.1.7, until a live row shows a decision
   answered from the cache equals one answered without it.
7. **LMCache after 0.5.6.** Proposed: a newer 0.5.x is used when its launch-time shape
   probe passes (SPEC §8.1) and shown as not qualified; another minor line is off with
   `version_unsupported` until a release adds it.
8. **SGLang after 0.5.22.** Proposed: off (`engine_version_unsupported`) until qualified;
   decision 1 names 0.5.22 alone, where vLLM is "0.32.0 or newer".
9. **Crash dumps under zero retention.** Proposed: engine and LMCache processes are
   started with a zero core-dump limit when retention is zero; swap stays the operator's.
10. **A host-wide LMCache opt-out without zero retention.** Proposed: none in 0.1.7;
    `lmcache: off` per deployment, or `retention: zero` per host.

# ADR 0033 — LMCache: a private KV cache server per deployment instance

**Status:** Accepted (owner decisions 2026-10-21: the design, and the owner's answers to
the ten open questions of the first draft, recorded below). Implemented for 0.1.7
(2026-10-22), with the amendments of the implementation at the end of this record (A1–A13):
the measured defaults of §6, the server's command line and log, the SGLang salt check
(LMCache issue 4825), the charge of the engine KV a server keeps while its engine is
stopped and after the wake until a reading shows it released (A13), `restart_only`
decision deployments parking with LMCache, and decision deployments on SGLang running
without LMCache until LC7 passes on a release (A10).
**Amends (with the implementation):** `SPEC.md` §7.4 (a private cache service per
instance), §8.2 (the rendered connector is owned), §9.1 and §9.2 (LMCache beside a
parking residency only where qualified), §10 (`cache_salt` is no longer forwarded
unchanged), §12 (the LMCache integration and its data lifetime), §15.1 (deployment fields
`lmcache`, `lmcache_pool`, `retention`; host settings `lmcache`, `retention`), §20 (T45
and live rows LC1–LC11); ADR 0014 §3 (SGLang's `enable_lmcache` and
`lmcache_config_file` move from "owned until designed" to rendered by CapyCTL); ADR 0010
decision 7 (this ADR is the measured claim for one external cache integration that
decision 7 leaves out of scope).
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

### 1. Version, integration path and the launch-time check

CapyCTL integrates **LMCache 0.5.6** (the security floor: CVE-2026-105192 is fixed in it)
and any later **0.5.x** release that passes the launch-time check below, in
**multiprocess server mode only**: CapyCTL starts `lmcache server` itself, and the engine
connects to it.

- **vLLM** connects through LMCache's external `LMCacheMPConnector`, named with its module
  path, `kv_connector_module_path: lmcache.integration.vllm.lmcache_mp_connector`, so a
  connector of the same name bundled with vLLM is never picked instead.
- **SGLang** connects through `--enable-lmcache --lmcache-config-file <file>`.
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

**The launch-time check** is SPEC §8.1's probe by shape: it reads files of the registered
environment and executes nothing. It passes when:

- LMCache's version is 0.5.6 or a later 0.5.x release;
- the LMCache package holds the connector the engine needs
  (`lmcache/integration/vllm/lmcache_mp_connector.py` defining `LMCacheMPConnector`, or
  `lmcache/integration/sglang/unified_lmcache_mp_connector.py`), the server entry point
  (`lmcache server`), and the server options §3 renders in its multiprocess configuration
  (`lmcache/v1/multiprocess/config.py`);
- for vLLM, its KV transfer configuration accepts `kv_connector_module_path`; for SGLang,
  its server arguments declare `enable_lmcache` and `lmcache_config_file`, and its LMCache
  radix cache module is present.

A check that fails turns LMCache off for the launch with reason `check_failed`, naming
what was missing. A pass is not qualification: the server's start (§3) and the engine's
start with the connector (§8) are the live checks, and the LC rows the qualification.

### 2. Supported engines

| Engine | LMCache | Shown when off |
|---|---|---|
| vLLM 0.32.0 or newer | yes, when the launch-time check passes | |
| vLLM 0.31 and older | no | `engine_version_unsupported`: "LMCache needs vLLM 0.32.0 or newer" |
| SGLang 0.5.22 or newer | yes, when the launch-time check passes | |
| SGLang before 0.5.22 | no | `engine_version_unsupported`: "LMCache needs SGLang 0.5.22 or newer" |
| llama.cpp | no | `engine_unsupported`: "llama.cpp has no LMCache integration" |
| TensorFold | no | `engine_unsupported`: "TensorFold has no LMCache integration" |
| custom engine | no | `engine_unsupported`: "CapyCTL does not render options for a custom engine" |
| SGLang, a decision deployment or `--disable-radix-cache` (A10) | no | `engine_unsupported`: "decision deployments on SGLang run without LMCache: …" or "SGLang with --disable-radix-cache runs without LMCache: …" |

vLLM 0.32.0 and SGLang 0.5.22 are the qualified versions (the LC rows); a newer one is
used on its launch-time check and is not qualified until a live row runs on it. The
engine version is the registered build (ADR 0018). An unsupported engine is never an
error by itself: its deployments run as they do today, and status and `capyctl engine
list` say "LMCache off" with the reason (§9).

**Every model gets LMCache by default**, hybrid layouts included (Mamba or gated-delta-net
state, linear or sliding-window attention): models are qualified as they are run, and a
model whose engine fails to start with the connector falls back as §8 says. **Decision
deployments** (`api: decisions`, ADR 0030) were to use it like chat deployments wherever
their engine serves decisions and supports LMCache: SGLang 0.5.22 or newer (vLLM is
refused for decisions, ADR 0030 §6 amendment of 2026-10-21). Amended 2026-10-22 (A10):
on SGLang they run without LMCache until LC7 passes on a release. A decision request
carries no salt, so decisions share the deployment's unsalted partition. The one layout
without LMCache in 0.1.7 is a deployment with a `topology` (`group_unsupported`: one
private server on one machine cannot serve the ranks of a group across machines, ADR
0028).

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
  the redacting writer, under the host's `engine_logs` mode (§10 forces `memory`).

**Order.** Activation reserves the engine and the server together (§6), then starts the
server, waits for `GET /status` within the start bound (§6), runs the listener check, and
only then launches the engine with the connector. A server that does not come up within
the bound is stopped (verified gone) and the engine launches without the connector:
LMCache off, reason `server_start_failed`.

**The server exits while its engine runs.** The instance fails: the host stops its engine
with a verified exit and the instance recovers by its normal rules (SPEC §13.2), which
relaunch the engine and a new server; the cache starts cold. The event
`lmcache_server_exited` records it. A server that exits while its engine is still
starting is an engine start failure with the connector (§8).

**Local reach.** Any process on the machine can reach both ports, as it can reach a
llama.cpp or TensorFold engine today (ADR 0029 §4): it can read the configuration,
enumerate cached object keys, clear the cache, and through the request port store or look
up KV under any salt. KV is request-derived data. On a machine with untrusted local users,
set the host's `lmcache: off` (§9) or `retention: zero` (§10). vLLM and SGLang themselves
keep their per-launch keys and ADR 0012's protections unchanged.

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

**SGLang** (0.5.22 or newer): `--enable-lmcache --lmcache-config-file <file>`, where
`<file>` is an owner-only file in the launch's private runtime directory holding exactly
`mp_host: 127.0.0.1` and `mp_port: <port>`, removed with the launch's cleanup.

Both are owned settings: a deployment cannot set them (§11). The effective configuration
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

### 6. Pool size, accounting and the measured defaults

The L1 pool is **4 GiB per instance by default**; a deployment sets another with
`lmcache_pool` (whole GiB, at least 1 GiB). It is rendered as `--l1-size-gb`.

An instance's reservation charges, in the host's system memory domain (ADR 0019; on a
unified-memory machine the same domain the GPU uses), **the pool plus the server's
measured overhead**:

- The host samples the server's process group's resident memory as it samples the
  engine's (ADR 0014 A2) and records the peak overhead above the pool with the deployment
  (`lmcache.measured` in status JSON); later launches charge the pool plus that figure.
  Before a deployment's first measurement, the charge is the pool plus the overhead
  allowance below.
- On a discrete GPU, any device memory the server's own CUDA context holds is measured
  per process like the engine's and charged to the device domain, with a device
  allowance before the first measurement.
- The pool counts against the host's `host_kv_limit` where one is declared (SPEC §7.4),
  and in the parked footprint while the model is parked (§7).
- Observed use above the charge is a degraded condition, as SPEC §7.3 requires; it is
  never hidden.
- A deployment whose memory CapyCTL sizes from the checkpoint (ADR 0014) includes the
  charge; `memory.request` stays the engine's own GPU memory.

LMCache's own limits are soft (the L1 size bounds the pool, not the process; quotas never
refuse a write) and never stand in for CapyCTL's reservations. Admission decides with the
charge above.

**Measure first.** The overhead allowance, the device allowance and the server's start
bound are not committed numbers yet: live row LC1 on host A measures them, and the
implementation sets the defaults from those measurements:

- **Overhead allowance:** the largest overhead beyond the pool that LC1 measures across
  its runs (both engines, chat and decisions, the default pool and one larger pool), plus
  25 %, rounded up to a whole 256 MiB.
- **Device allowance:** the largest per-process GPU memory LC1 measures for the server,
  chosen the same way.
- **Start bound:** the slowest time from the server's start to `GET /status` answering
  that LC1 measures, times 3, rounded up to whole 10 s, and never below 30 s.

The implementation records the measured figures and the resulting defaults in its change
file and in this ADR; until then the implementation carries no number for them.

### 7. Parking

**The server stays up while its model is parked**, and its pool stays charged (SPEC §7.3:
"retained host-cache allocations remain charged"). It stops with the instance's stop.

**`restart_only` uses LMCache.** Parking stops the engine and keeps the server; waking
launches the engine again on the same server, so the cache stays warm across the
restart. This holds for a deployment that declares `restart_only` and for one that
resolves it (a decision deployment, or any deployment on a host with
`security.deep_park: disabled`), on every supported engine.

**A parking residency uses LMCache only where qualified; parking wins.** A deployment
whose residency resolves `deep` (declared or by default), or `host_backed` on a discrete
GPU, uses LMCache only on a combination of engine version, model type and GPU qualified
live for that residency: park, wake, and a cache hit after the wake gives the same answer
as without the cache (issue #4071 is why this is not assumed). The combinations ship as a
table in the release: the exact registered engine version, the checkpoint's `model_type`,
the GPU's product name as the host's inventory reports it, and the residency. The table
starts with the combinations LC4 passes; nothing else is in it.

On any other combination the deployment **keeps its parking residency and runs without
LMCache**, default deployments included: there is no automatic switch to
`restart_only`. Status says "LMCache off: not qualified with deep park on <engine
version> / <model type> / <GPU>" (reason `park_unqualified`), and the event `lmcache_off`
records it. A deployment that wants LMCache there declares `residency: restart_only`.
Under `lmcache: required` such a deployment is refused
`lmcache_unavailable:park_unqualified`. A later release that adds the combination to the
table turns LMCache on for it at the next launch.

### 8. The engine fails to start with the connector

An engine launched with the connector that exits, or does not pass readiness within its
bound, before it is ready (the server exiting meanwhile included) is stopped with a
verified exit, and so is its server. The host then **relaunches it once without
LMCache**: a warning in the role's log, the event `lmcache_off` with reason
`engine_start_failed` (naming the engine log's last lines through the redacting writer),
and status shows "LMCache off: the engine did not start with LMCache". The fallback is
recorded with the revision on that host's profile, so later launches of the same revision
on the same profile go straight to the launch without LMCache and show the same reason,
until the revision or the profile changes. If the relaunch without LMCache also fails,
the launch fails as any failed launch.

Under `lmcache: required` there is no relaunch: the launch fails
`lmcache_unavailable:engine_start_failed`, and nothing is left running.

### 9. Default on; `off` and `required`

A deployment's `lmcache` is `auto` (the default), `off` or `required`:

- **`auto`.** Every deployment that can use LMCache gets it. When it cannot — the
  engine or its version is unsupported, the package is missing, below 0.5.6 or fails the
  launch-time check, the deployment has a `topology`, its parking residency is not
  qualified with LMCache (§7), an approved `--kv-transfer-config` is in `extra_args`
  (§11), the host's `lmcache` is `off`, retention is zero (§10), the host predates this
  ADR, the server does not start or fails the listener check, or the engine did not start
  with the connector (§8) — the deployment runs without it. `capyctl status deployment
  <name>` and `capyctl engine list` show "LMCache off" with the reason, and the event
  `lmcache_off` records the instance and reason once per launch.
- **`off`.** No server, no connector; status shows "LMCache off: turned off by the
  deployment".
- **`required`.** The deployment refuses to start instead of running without it:
  reasons known at resolution or placement refuse it there, with
  `lmcache_unavailable:<reason>` (or `retention_zero:lmcache`,
  `host_capability_missing:lmcache`); reasons found at launch (`check_failed`,
  `server_start_failed`, `listener_exposed`, `engine_start_failed`) fail the launch with
  the same code, and nothing is left running.

`lmcache_pool` beside `lmcache: off` is contradictory and refused (`invalid_config`).

**Host setting** `lmcache`: `auto` (the default) or `off` (standalone: `host.lmcache`;
`--set lmcache=off`, `CAPYCTL_SET__LMCACHE=off`). With `off`, LMCache is off for every
deployment on that machine (reason `host_off`), and a deployment with `lmcache: required`
is refused there at placement, `lmcache_unavailable:host_off`. A role that starts with
`lmcache: off` and finds LMCache servers it owns handles them as §10 does for zero
retention: each instance using one is drained, stopped with its server and relaunched
without LMCache, and a parked one is released like an idle stop.

**Status.** `capyctl status deployment <name>` gains a line per instance:

```
LMCache   on: pool 4 GiB, used 1.3 GiB, overhead 0.8 GiB (measured), 212 hits / 260 lookups
LMCache   off: LMCache needs vLLM 0.32.0 or newer
```

and the JSON view `lmcache` (`state`, `reason`, `pool_bytes`, `used_bytes`,
`overhead_bytes`, `measured`, `lookups`, `hits`). `capyctl engine list` gains a column
`LMCACHE`: the version (`0.5.6`), or `off (<reason>)` for a profile that cannot use it.
(The figures above illustrate the shape; they are not measurements.)

### 10. Zero data retention

**Host setting** `retention`: `standard` (the default) or `zero` (standalone:
`host.retention`; `--set retention=zero`, `CAPYCTL_SET__RETENTION=zero`). With `zero`:

- engine logs are kept in memory (`engine_logs: memory`); a document that states
  `engine_logs: files` with `retention: zero` refuses the start, and
  `--debug-engine-logs` is refused as it is with `engine_logs: memory`;
- LMCache is never started on the host: an `auto` deployment runs without it (reason
  `retention_zero`), and a deployment cannot re-enable it: `lmcache: required` is refused
  `retention_zero:lmcache` at placement;
- engine and LMCache processes start with `RLIMIT_CORE` 0, so a crash writes no core dump
  of request data;
- turning it on stops existing LMCache servers. A role that starts with `retention: zero`
  and finds LMCache servers it owns drains each instance using one, stops its engine and
  its server (verified gone), and relaunches it without LMCache; a parked instance using
  one is released like an idle stop, so it stays on-demand eligible (T10) and its next
  start has no LMCache. The servers held RAM only, so their exit leaves nothing on disk
  to purge. The journal records each stop; the event `lmcache_off` carries
  `retention_zero`.

**Deployment setting** `retention: zero`, for a host that does not set it: the
deployment's launches keep their engine output in memory whatever the host's
`engine_logs`, start with `RLIMIT_CORE` 0, and never use LMCache (`lmcache: required`
beside it is refused `retention_zero:lmcache`). It is part of the revision. A host that
predates this ADR is refused at placement, `host_capability_missing:retention`, since it
would not honour it.

**What zero retention does not cover**, stated in the guide and the data retention page:

- **The engine's own prefix and KV cache in GPU memory.** vLLM and SGLang keep it for the
  life of the process; it is written nowhere and dies with the process, and it is
  partitioned by salt. Zero retention does not turn it off.
- Whatever a custom engine's manifest makes it write (ADR 0031 §8).
- Swap: the operating system may page any process's memory out; the operator decides
  whether the machine has swap.

### 11. Engine options and environment

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

### 12. Metrics read

The host reads the server's `/metrics` on its loopback HTTP port when it samples the
engine's load, and shows: `lmcache_mp_l1_memory_usage_bytes` (used),
`lmcache_mp_l1_usage_ratio`, `lmcache_mp_inflight_load_memory_usage_bytes`, the L1
allocation-failure counters, and the lookup and hit counters summed over their model and
salt labels. `lmcache_mp_l2_usage_bytes` is expected absent or zero (no L2); a value
above zero is a degraded condition. A missing or unreadable figure is unknown, never
zero. These are cache figures, not the process's footprint; the charge (§6) comes from
the measured process memory. A later 0.5.x release that renames a metric leaves that
figure unknown; it does not fail the launch.

### 13. Host capabilities

A host that implements this ADR declares `lmcache` and `retention` (ADR 0017). The server
places `lmcache: required` only on a host that declared `lmcache`
(`host_capability_missing:lmcache`), and `retention: zero` only on one that declared
`retention`. An `auto` deployment on an older host runs as in 0.1.6, and status shows
"LMCache off" with reason `host_capability_missing`. The host's `lmcache` and `retention`
settings reach the server with the host's published document, so placement refuses
`required` on such a host before anything is sent.

### 14. Codes and reasons

| Where | Code | When |
|---|---|---|
| resolution, placement | `lmcache_unavailable:<reason>` (exit 5) | `lmcache: required` and a reason below applies (`host_off`, `park_unqualified`, `engine_version_unsupported`, ...) |
| placement | `retention_zero:lmcache` (exit 5) | `lmcache: required` where the host or the deployment has `retention: zero` |
| placement | `host_capability_missing:lmcache`, `host_capability_missing:retention` (exit 5) | as §13 |
| launch | `lmcache_unavailable:check_failed`, `:server_start_failed`, `:listener_exposed`, `:engine_start_failed` | under `required`, at launch |
| deploy | `invalid_config` | `lmcache_pool` invalid or beside `lmcache: off`; an LMCache `--kv-transfer-config`, `--enable-lmcache` or `--lmcache-*` in `extra_args` |
| deploy | `engine_env_reserved:LMCACHE_*` | an `LMCACHE_*` engine variable |
| role start | `invalid_config` | `retention: zero` with `engine_logs: files`; a host `lmcache` other than `auto` or `off` |

Reasons (status, engine list, `lmcache_off`): `disabled`, `host_off`, `engine_unsupported`,
`engine_version_unsupported`, `not_installed`, `not_registered`, `version_unsupported`
(below 0.5.6, or not 0.5.x), `check_failed`, `group_unsupported`, `park_unqualified`,
`kv_transfer_config_set`, `retention_zero`, `host_capability_missing`,
`server_start_failed`, `listener_exposed`, `engine_start_failed`. Events: `lmcache_off`
and `lmcache_server_exited`.

### 15. Texts that change with the implementation

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

**SPEC §9.1 and §9.2**, after the deep-park paragraphs: "With LMCache (ADR 0033), a
parking residency uses LMCache only on an engine version, model type and GPU combination
qualified live for it; on any other combination the deployment keeps its residency and
runs without LMCache. A `restart_only` deployment uses LMCache, and its LMCache server
survives the restart."

**SPEC §12**, a note: "LMCache 0.5.6 and later 0.5.x releases are integrated in
multiprocess server mode only, host RAM tier only, one private loopback server per
instance, on by default for supported engines (ADR 0033). It holds request-derived KV in
RAM while its model runs or is parked, and nothing on disk; a host's `lmcache: off` or
`retention: zero` keeps it off."

**SPEC §8.2**: the LMCache connector settings (§11 above) join the reserved settings.
**SPEC §15.1**: the deployment fields `lmcache`, `lmcache_pool` and `retention` and the
host settings `lmcache` and `retention`. **SPEC §20**: T45 and the LC rows
(Verification). **SPEC §21**: the amendment entry.

**`docs/operations/data-retention.md`**: the summary gains an "LMCache" row (KV of
recent request prefixes, keyed by salt digest; the host RAM of the instance's private
loopback server; while the model runs or is parked, gone when it stops; turn off with
`lmcache: off` on the deployment or the host, or `retention: zero`). The "Engine logs"
row names `retention: zero` beside `engine_logs: memory`. The "Engine caches" row and
section keep GPU memory and add that zero retention does not cover them. The salt
paragraph says CapyCTL sends a digest of the deployment, its revision and the salt, not
the salt itself. A new section, "Zero data retention", gives §10, and the opening
paragraph names LMCache's RAM as the one place request-derived data outlives a request
besides the engine's own GPU caches.

**`docs/guide/engine-flags.md`**: the rows of §11 (vLLM "KV offload, LMCache" and SGLang
"KV offload, LMCache" split so the LMCache options read owned), and the environment note.

**Also**: `docs/operations/configuration.md` (host `lmcache` and `retention`, standalone
`host.lmcache` and `host.retention`), `docs/guide/configuration.md` and `deploy.md` (the
deployment fields), `docs/guide/requests.md` (the salt section), `docs/guide/engines.md`
(installing LMCache in an engine environment, the `LMCACHE` column, the qualified
combinations), `docs/guide/parking.md` (what parks with LMCache, and why a deep-parking
model may show LMCache off) and `docs/guide/errors.md` (§14's codes).

## Consequences

- A second CapyCTL-owned process per instance: ownership, journal, ports, listener check,
  measurement, metrics and cleanup for the server; a connector rendering per engine; the
  launch-time check; the relaunch without LMCache and its record; a salt derivation in
  the router; the qualified-combinations table; two host settings and three deployment
  fields; the `lmcache` and `retention` capabilities.
- Default on costs host RAM: N instances that use LMCache hold N × (4 GiB + overhead),
  parked or not. On a 128 GB unified machine that is memory the next model cannot use;
  admission accounts for it rather than discovering it.
- Parking wins: a deployment that parks `deep` keeps parking as in 0.1.6, and runs
  without LMCache until its engine, model type and GPU are qualified. Until the table
  grows, LMCache's warm cache across a park reaches `restart_only` deployments (decision
  deployments among them) and the combinations LC4 qualified; status says why it is off
  elsewhere.
- An engine that cannot start with the connector costs one failed start the first time,
  then runs without LMCache for that revision on that profile.
- The router's salt change alters what vLLM and SGLang receive for every salted request,
  with or without LMCache; the existing test that the salt reaches them byte for byte
  changes to the digest.
- KV derived from requests now lives in host RAM in a process any local user can reach
  on loopback (§3). The host's `lmcache: off`, zero retention and the deployment's
  `lmcache: off` are the remedies; the guide says so.
- Newer engine and LMCache releases are used on a check by shape, before any live row
  has run on them; status shows them as not qualified.
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
  point; `lmcache: off` (deployment or host) and `retention: zero` opt out.
- **Fail the deployment whenever LMCache cannot be used.** Rejected for the default
  (owner decision); `lmcache: required` gives it to whoever wants it.
- **Switch an unqualified deep-parking deployment to `restart_only`.** The first draft's
  rule. Rejected (owner decision): parking is the product's premise (ADR 0010), and a
  cold restart on every wake would cost more than a warm cache saves.
- **Only full-attention layouts, or no decision deployments, by default.** Rejected
  (owner decision): every supported model gets LMCache and is qualified as it runs; the
  relaunch without LMCache (§8) covers a model the connector cannot serve.
- **Pin exactly LMCache 0.5.6 and SGLang 0.5.22.** Rejected (owner decision): a later
  0.5.x LMCache or SGLang is used when its launch-time check passes, so a security fix
  does not wait for a CapyCTL release.
- **Commit the overhead allowance and start bound now.** Rejected (owner decision): no
  figure for these engines on this hardware exists; LC1 measures first.

## Verification

A new conformance row, **T45 — LMCache**: the supported-engine table resolves on, and off
with each reason, for every engine and version of §2, decision deployments included; the
launch-time check passes on 0.5.6 and a later 0.5.x shape and on SGLang 0.5.22 and a
later shape, and fails (`check_failed`, naming what is missing) on each missing file,
class or field, below 0.5.6 and on another minor line, executing nothing; `lmcache: off`
renders no server and no connector; a host's `lmcache: off` turns every deployment off
(`host_off`) and refuses `required` at placement (`lmcache_unavailable:host_off`), and at
role start stops existing servers and relaunches their instances without LMCache;
`required` is refused with `lmcache_unavailable:<reason>`, `retention_zero:lmcache` or
`host_capability_missing:lmcache` before anything starts, and a launch-time failure
leaves nothing running; the server's argument vector is exactly §3's (loopback hosts,
leased ports, `LRU`, no script API, no lazy L1, no L2), its environment closed; the
server starts, answers `/status` within the start bound and passes the listener check
before the engine launches, and an off-loopback listener stops it (`listener_exposed`);
an engine that fails to start with the connector, or whose server exits while it starts,
is stopped with its server and relaunched once without LMCache, with the warning, the
`lmcache_off` event and status, later launches of that revision on that profile skip
LMCache, and under `required` the launch fails `lmcache_unavailable:engine_start_failed`
with no relaunch; a server that exits while its engine serves fails the instance, which
recovers with a new server (`lmcache_server_exited`); the vLLM `--kv-transfer-config`
names `LMCacheMPConnector` with its module path and `autostart: false`, and SGLang's
config file holds only host and port; LMCache connectors in `extra_args`,
`--enable-lmcache`, `--lmcache-*` and `LMCACHE_*` are refused, and another approved
connector turns LMCache off; the effective salt is §5's digest (fixed vectors), identical
for equal inputs, different across deployments, revisions and salts, absent without a
salt, never logged; the reservation charges the pool plus the allowance, then the
measured overhead, in the system domain, against `host_kv_limit`, and stays charged while
parked; the server stays up through a park and stops with the stop, with verified exit
before release; a new revision gets a new server; `restart_only` keeps LMCache across the
restart; a parking residency keeps its residency and runs without LMCache outside the
qualified table (`park_unqualified`, refused under `required`) and with it inside;
`retention: zero` forces memory engine logs, refuses `engine_logs: files`, starts engine
and LMCache processes with `RLIMIT_CORE` 0, keeps LMCache off, and at role start stops
existing servers and relaunches their instances without LMCache; status, JSON and
`engine list` show on or off with the reason, and `lmcache_off` is recorded. Tests also
tag T14, T24, T25, T35, T36, T37 and T43 where they apply.

Live rows, on host A with `~/vllm-0.32.0-lmcache-venv` (a copy of the approved vLLM
0.32.0 environment with LMCache 0.5.6) and `~/sglang-0.5.22-lmcache-venv` (a copy of the
approved SGLang 0.5.22 environment with LMCache 0.5.6 built against its torch 2.14.1),
owner approved 2026-10-21 and listed in `AGENTS.md`, with Qwen3-4B for chat on each
engine:

| Row | What it shows |
|---|---|
| LC1 | Start with cache, and the measurements: each engine's deployment starts with LMCache on by default; the server listens only on loopback (listener check and `ss`), `/metrics` reads, and status and `engine list` show it on. Records, over repeated starts on both engines, chat and decisions, with the default pool and one larger pool: the server's overhead beyond its pool, its per-process GPU memory, and its time from start to `/status`. §6 sets the defaults from these. |
| LC2 | Cache hit after an identical prefix: a long prompt, then a second request with the same prefix after the engine's own prefix cache no longer holds it; LMCache's hit counter rises, and at temperature 0 the answer equals the one a `lmcache: off` deployment of the same model gives. |
| LC3 | Salts isolate: the same prefix under salt A, then salt B, then no salt: no LMCache hit and no cached tokens across different salts or between a salt and none; salt A again hits. |
| LC4 | Park and wake per engine: `restart_only` with LMCache (the server stays up and charged while parked; the first request after the wake hits from LMCache with the same answer); a `deep` deployment on a combination outside the table runs deep with LMCache off and the reason shown; then `deep` with LMCache for the combination under test, on a build whose table lists it: park, wake, a hit with the same answer as without the cache. A pass adds the combination to the released table; a failure leaves it out. |
| LC5 | Zero retention: a host with `retention: zero` refuses `lmcache: required` (`retention_zero:lmcache`), runs an `auto` deployment with LMCache off (`retention_zero`), writes no engine log file, and its engine shows `Max core file size 0` in `/proc/<pid>/limits`; turning it on with a server running stops that server (verified exit) and relaunches its instance without LMCache. |
| LC6 | Missing package: a deployment on the approved environment without LMCache (`~/sglang-0.5.22-venv` as registered) runs with LMCache off (`not_installed`) in status, `engine list` and an `lmcache_off` event; `lmcache: required` on it is refused. |
| LC7 | Decisions (amended 2026-10-22, A10): a decision deployment (Clef-Flash) on SGLang 0.5.22 starts with LMCache off (`engine_unsupported`, the upstream issue in the detail) and serves; `lmcache: required` on it is refused before anything starts (`lmcache_unavailable:engine_unsupported`). The row as first written (LMCache on, the same state asked twice hitting from LMCache with the same pick within ADR 0030's 0.05, a `restart_only` park whose first decision after the wake hits) runs again on the first SGLang release with the upstream fix, and a pass lifts A10 for that release. (vLLM is refused for decisions, ADR 0030 §6.) |
| LC8 | Relaunch without LMCache: the LMCache server of a starting instance is killed (SIGKILL to its owned PID) before the engine is ready; the engine is stopped with a verified exit and relaunched once without LMCache, which serves; the warning, the `lmcache_off` event (`engine_start_failed`) and status show it, and the next start of that revision goes without LMCache. With `lmcache: required` the same launch fails `lmcache_unavailable:engine_start_failed` and nothing is left running. |
| LC9 | Server exit while serving: the server of a ready instance is killed; the instance fails, its engine is stopped with a verified exit, and recovery relaunches the engine and a new server (`lmcache_server_exited`). |
| LC10 | Host off: a host with `lmcache: off` runs an `auto` deployment with LMCache off (`host_off`) in status and `engine list`; restarting a role with a server running into `lmcache: off` stops that server (verified exit) and relaunches its instance without LMCache. |
| LC11 | Required on an off host: a deployment with `lmcache: required` placed on the host of LC10 is refused at placement, `lmcache_unavailable:host_off`, and nothing starts. |

## Owner decisions of 2026-10-21

The design:

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
   stops with the stop. Deep park with LMCache only for combinations qualified on host A
   (issue #4071) (§7; the answer to open question 1 below replaces the first draft's
   switch of unqualified combinations to `restart_only`).
4. **Salt.** A fixed 64-hex SHA-256 digest, domain-separated, of the deployment revision
   identity and the caller's salt, sent to the engine and LMCache alike; no salt stays no
   salt; SPEC §10 amended; salt partitioning is not authorization (§5).
5. **Size.** 4 GiB L1 pool per deployment by default, settable per deployment; the charge
   is pool plus measured overhead in host RAM (the GPU's domain on unified memory);
   LMCache quotas never substitute for CapyCTL reservations (§6).
6. **Default on.** Every supported deployment uses LMCache unless it sets `lmcache: off`;
   if LMCache cannot be used it runs without, shown off with the reason and an event;
   `lmcache: required` refuses to start instead (§9).
7. **Zero data retention.** Host `retention: zero` (engine logs in memory, LMCache never
   started, deployments cannot re-enable either, turning it on stops existing servers)
   and a per-deployment `retention: zero`; engine-native GPU prefix caches are not
   covered (§10).
8. **Environments.** `~/vllm-0.32.0-lmcache-venv` and `~/sglang-0.5.22-lmcache-venv` on
   host A for qualification, added to `AGENTS.md`.

The answers to the first draft's ten open questions:

1. **Parking wins.** On a combination not qualified for LMCache with deep park, the
   deployment keeps deep park and LMCache is off for it ("LMCache off: not qualified with
   deep park on <engine/model/hardware>") until qualified, default deployments included;
   no automatic switch to `restart_only`. A `restart_only` deployment uses LMCache (§7).
2. **The engine fails to start with the connector:** relaunch once without LMCache, log a
   warning and record `lmcache_off` with the reason; status shows it. Under
   `lmcache: required` the launch fails instead (§8).
3. **The server dies while its engine runs:** the instance fails (verified stop) and
   recovers by its normal rules, relaunching both (§3).
4. **Numbers: measure first.** LC1 measures the server's overhead beyond its pool and its
   time to ready; the defaults come from those measurements plus margin; no number is
   committed until then (§6).
5. **Layouts:** every model gets LMCache by default, hybrid included; models are
   qualified as they run, and one whose engine fails with the connector falls back by
   answer 2 (§2).
6. **Decision deployments:** on where supported, like chat, with a qualification row
   (§2, LC7). Amended 2026-10-21: SGLang only, since vLLM stays refused for decisions
   (ADR 0030 §6 amendment of 2026-10-21). Amended 2026-10-22 (owner decision, A10): off
   on SGLang too until LC7 passes on a release.
7. **LMCache versions:** any 0.5.x release from 0.5.6 is used when CapyCTL's launch-time
   check passes (§1).
8. **SGLang after 0.5.22:** LMCache is used when the launch-time check passes (§§1–2).
9. **Core dumps:** under `retention: zero`, `RLIMIT_CORE` 0 for the engine and LMCache
   processes (§10).
10. **Host switch:** a host setting `lmcache: auto|off`; `off` turns LMCache off for every
    deployment on that machine, and `lmcache: required` is refused there with a code
    (§9).

## Amendments with the implementation (2026-10-22)

Facts found while implementing and measuring, within the owner's decisions above.

**A1. The measured defaults (§6).** Live row LC1 on host A (GB10; LMCache 0.5.6, Qwen3-4B
chat on vLLM 0.32.0 and SGLang 0.5.22, pools of 4 and 8 GiB, five engine runs and twenty
bare server starts) measured: the server's resident memory beyond its pool at most
824.3 MiB (781–824 MiB across runs, about the same at 8 GiB); its per-process GPU memory
at most 408 MiB while its engine ran (170 MiB alone); the slowest start to `GET /status`
4.80 s (2.90–4.80 s). The defaults are therefore: overhead allowance 1280 MiB (824.3 MiB
plus 25 %, rounded up to 256 MiB), device allowance 512 MiB (408 MiB plus 25 %), start
bound 30 s (4.80 s × 3 = 14.4 s, rounded up to 20 s, raised to the 30 s floor). On a
unified-memory machine the server's GPU memory lies outside its resident set and inside
the one memory pool (measured: a 2 GiB CUDA allocation moved `MemAvailable` by 2 GiB and
the resident set by nothing), so there the charge is the pool, the overhead and the device
figure, all in the one domain; on a discrete GPU the device figure goes to the device
domain as §6 says.

The figures measured on later launches, which replace these allowances for the revision on
that host, are taken on the allowances' own basis (found live 2026-10-22 on GB10: a SGLang
server's 4903 MiB resident set was 436 MiB anonymous, 4138 MiB shared and 329 MiB
file-backed, and the driver attributed 406 MiB of GPU memory to it; measuring anonymous and
shared pages alone, with no GPU figure, undercharged it by about 735 MiB). The overhead is
the server group's whole resident set (`VmRSS`, file-backed pages included) beyond its
pool, on every path: the embedded watch while it runs, the reading at Ready, and a remote
host's load reports. The device figure is the GPU memory the driver attributes to the
server's processes (the per-process `nvidia-smi` sample of ADR 0007), on unified memory as
on a discrete GPU. A figure no source read is recorded as zero and keeps its allowance: a
source that cannot see that memory never replaces an allowance. Each figure is the
largest seen for the revision on that host, so a smaller later reading never lowers it.

**A2. The command line (§3).** In LMCache 0.5.6 the L1 pool is lazy by default
(`--l1-use-lazy` is a boolean option defaulting to true, starting at
`--l1-init-size-gb 20`), so §3's "the pool is allocated at start" needs the option stated:
CapyCTL renders `--no-l1-use-lazy` after §3's options. The server's `--shm-name` default
(empty) keeps LMCache's shared-memory transfer off and is not passed. The L1 options are
declared in `lmcache/v1/distributed/config.py`, the server's own in
`lmcache/v1/multiprocess/config.py`; the launch-time check reads both.

**A3. The server's log (§3).** The server's output goes to its own log beside the engine's
(`<launch>.lmcache.log`), through its own redacting writer in the same `engine_logs` mode,
rather than into the engine's log: the in-memory log buffer is one per log path, the
writer is started with the engine's credentials (which do not exist yet when the server
starts first), and a `restart_only` server outlives each engine. §8's log tail stays the
engine's.

**A4. The SGLang salt check (§1).** LMCache issue 4825 (owner input 2026-10-22) found that
LMCache's legacy SGLang multiprocess adapter drops `cache_salt`; only the unified SGLang
integration threads it end to end (LMCache main b28250e, regression tests in LMCache PR
5964). LMCache 0.5.6 has the unified path: `unified_lmcache_mp_connector.py` builds
`IPCCacheServerKey(..., cache_salt=...)`, and SGLang 0.5.22's
`lmcache_unified_radix_cache.py` passes `req.cache_salt` on lookup and store (confirmed on
host A, and LMCache's own object keys carried the salt in LC1). The launch-time check for
SGLang therefore verifies the salt support, not only the files: the unified connector
passes the salt into the key, the IPC key and object key carry it, SGLang's radix cache
passes the request's salt to the unified connector and imports neither
`sglang_adapter` nor `multi_process_adapter`, and SGLang's cache registry selects the
unified radix cache under `enable_lmcache` (SGLang 0.5.22 declares `enable_lmcache` and
`lmcache_config_file` in `arg_groups/fields/memory.py`, not `server_args.py`). A release
without it fails `check_failed`, and SGLang runs with LMCache off until a release has it.
Live row LC3 on SGLang also reads LMCache's own record of the stored keys for a
non-empty salt.

**A5. A stopped engine's KV (§§6, 7).** LC1 found that after an engine exits, its LMCache
server keeps the engine's KV cache mapped through CUDA IPC (the server's GPU figure rose
to 1430 MiB with a 1 GiB vLLM KV cache, 1656 MiB on SGLang, unchanged after 92 s) until
the next engine registers (it then dropped back to 406–408 MiB, the old KV released, not
both held; corrected by A13: the release comes with the new engine's first stored KV, not
its registration) or the server stops. A `restart_only` instance parked with its server
therefore charges, in its parked phase, the server's charge plus the engine's KV cache.
Each server that had an engine connected also leaves a CUDA driver file,
`/dev/shm/cuda.shm.<uid>.<pid>.1` (80 KiB, owner-only), after its verified exit; CapyCTL
removes the stopped server's file. The engine processes leave the same kind of files, as
they did before this ADR.

**A6. Decision deployments park with LMCache.** A decision deployment resolves
`restart_only` (ADR 0030); with LMCache on, it parks as §7 says (the engine stops, the
server stays) instead of stopping, and the engine started at the wake is checked against
the launch's wake reference (ADR 0030 §3) like any wake.

**A7. LMCache's usage telemetry is off (§§3, 10).** Found live on host A: LMCache 0.5.6
reports usage from every process that imports it to `stats.lmcache.ai:8080` over plain
HTTP (`lmcache/usage_telemetry`, default `LMCACHE_USAGE_TRACK_URL`) unless
`LMCACHE_TRACK_USAGE=false` or `DO_NOT_TRACK=1`, and the server's closed environment set
neither. CapyCTL now renders both into the server's environment, and into the engine's
whenever its connector is on (the connector imports LMCache in the engine), embedded and
remote; the engine's resolved environment never replaces them. Nothing about a launch
leaves the host. With the host setting `engine_telemetry` (owner decision 2026-10-22),
`LMCACHE_TRACK_USAGE=false` stays rendered whenever LMCache runs, whatever that setting
says, and `DO_NOT_TRACK=1`, which also silences the engine's own reporting, follows the
setting: set under `off` (the default), in the server and the engine; not set under
`allow`, so that the engine keeps its own default and LMCache still reports nothing.

**A8. A restart-park's quiescence (§7).** Found live on SGLang 0.5.22: an embedded
restart-park was always refused as not quiescent, because SGLang's quiescence proof is the
memory saver's observer, which a `restart_only` launch never has. A restart-park stops
the engine and releases nothing in place, so its proof is the one SPEC §10 step 4 needs
without a saver: nothing in flight at ingress (the park is armed only then) and the
engine's own running and queued gauges at zero, read now; an unreadable gauge is not
quiescence. vLLM's proof was already its gauges. A remote host's restart-park checked its
ingress alone before it stopped the engine; it now also reads the engine's gauges after
its ingress reads zero, and an engine that is busy or cannot say leaves the launch
unchanged. Decision deployments (A6) park the same way. The deep-park proof is unchanged.

**A9. The server is its group's leader (§§3, 9).** Live runs on SGLang with LMCache 0.5.6
found the server starting a short-lived child of its own group. Three consequences:

- The listener check reads each member's `/proc/<pid>/fd` and socket tables. A member
  whose entries are missing and whose presence is verified gone exited during the read:
  it holds no listener, and the group is listed again and checked as it is now (three
  readings at most), so the group recorded never names it. The leader gone, a member of
  unknown presence, any other read failure, or a group that changed on every reading
  still fails the check (`server_start_failed`).
- A server stopped with a verified exit before its engine was spawned (`server_start_failed`
  or `listener_exposed` under `auto`) leaves the launch's record before the engine
  launches without the connector: each of its processes is proven gone by identity, then
  the record drops it, journaled (`lmcache_server_departed` in the controller's journal
  for an embedded launch; the host journal's departed processes for a remote one). The
  launch's Ready then names the engine alone, as its evidence does. Letting the Ready
  accept a recorded server absent from its evidence instead would release a recorded
  process on evidence that never names it. A server whose stop or departure was not
  verified keeps its record for the launch's own stop: the launch is uncertain and no
  engine is spawned.
- Only the leader's exit is the server's: a member of its group (`lmcache-N`) that exits
  while the instance is Ready or parked fails nothing, as a helper's does (ADR 0027), and
  re-proofs and restart-park evidence judge the server by its leader. Every member
  recorded stays recorded until the launch's verified stop proves it gone.

**A10. Decision deployments on SGLang run without LMCache (§2; owner decision 2026-10-22,
LC7).** LC7 failed on SGLang 0.5.22: the decision deployment's engine started with the
connector and failed at its first idle check with `AttributeError: 'UnifiedRadixCache'
object has no attribute 'has_pending_cache_operations'`. The cause is in SGLang 0.5.22
(read on host A; upstream issue
[sgl-project/sglang#45929](https://github.com/sgl-project/sglang/issues/45929)):
`mem_cache/registry.py`, `default_radix_cache_factory`, returns the plain
`UnifiedRadixCache` whenever `disable_radix_cache` is set, before its `enable_lmcache`
branch, and `managers/scheduler.py` (line 5233), `is_fully_idle`, calls
`tree_cache.has_pending_cache_operations()` whenever `enable_lmcache` is on. Decision
models run as embedding models (`is_embedding`), for which SGLang resolves
`disable_radix_cache` to true. LMCache is therefore planned off, reason
`engine_unsupported` with the issue in its detail, at plan time in the store, on the
host and at resolution, so no start fails on it: for every decision deployment on SGLang
(any version), and for any SGLang deployment whose profile arguments or `extra_args`
state `--disable-radix-cache`. `lmcache: required` on either is refused at resolution,
`lmcache_unavailable:engine_unsupported`. Other models SGLang disables the radix cache for
on its own are not recognised in advance; their first start fails with the connector and
falls back as §8 says (`engine_start_failed`, sticky for the revision on that profile).
A6 stands for an engine that serves decisions with LMCache; none does until LC7 passes on
a release.

**A11. A standalone role plans from the settings it runs under (§§9, 10).** Found live
(LC5, LC10): a standalone role restarted into `retention: zero` or `lmcache: off` stopped
its LMCache server correctly, then relaunched the instance with a new server, and a later
start of the same revision started one too. The embedded host publishes no approved
document, and LMCache was planned from the settings the revision was resolved under. A
standalone role now records the `lmcache` and `retention` it starts with before anything
is placed or started, and placement and LMCache's plan read them (a server host's are its
current approved publication, as before); a revision's own copy is read only for a host
with neither. `capyctl engine list` likewise shows LMCache off with the reason under the
settings the running role reports, falling back to its role document when the role is
not running. An `lmcache_off` or `lmcache_server_exited` detail longer than its bound
keeps its end, marked `…` where it was cut: an engine log tail's last line is its error.

**A12. Where the qualified table's model type and GPU come from (§7).** Found in review: a
deep deployment always showed "LMCache off: not qualified with deep park on SGLang 0.5.22 /
an unknown model type / an unknown GPU", because neither plan (the store's at a start, the
host's at its launch) was given the checkpoint's model type or the GPU's product name, so
no row of the table could ever match. Both plans now compare real values:

- **The GPU.** Each host publishes, with its GPU inventory, the product name `nvidia-smi`
  reports at boot (`resource_policy.devices.<id>.product_name`, kept across a reload as
  the physical UUID is). A launch's GPU is the product its selected devices name, or its
  host's devices when the deployment selects none; devices naming different products, or
  none, are an unknown GPU. The product name is read from the revision's frozen host
  policy, as every other host fact a launch uses is, so a revision accepted before its host
  published product names plans with an unknown GPU until it is applied again.
- **The model type.** It is the checkpoint's `config.json` `model_type`. The host reads it
  where its launch reads the checkpoint, and so does the store for its embedded host, which
  shares that machine. An enrolled host's checkpoint is not on the server's machine, so the
  host reports the model type with its sizing and its measured digest, and the store keeps
  it with them (schema v55: `model_type` on `checkpoint_sizes` and `checkpoint_digests`).
  A remote start plans with the model type its host reported; before that host has sized
  or measured the checkpoint the model type is unknown and the start plans LMCache off,
  saying so. A digest recorded before v55 takes the model type the next identical
  measurement names.
- **The comparison.** Both values are compared trimmed, and only as printable ASCII of 1
  to 128 bytes; anything else is unknown, which no row matches. A host's report naming a
  model type in any other form is refused.

The released table is unchanged: it is still empty until LC4 passes, so every parking
combination stays off, now naming its real model type and GPU.

**A13. The stopped engine's KV after the wake (§§6, 7; found live on vLLM, 2026-10-23).**
The live rerun on host A found A5's release point wrong: after a restart-park's wake the
server kept the old engine's KV mapped (1574 MiB of GPU memory, Ready, at least 37 s)
until the new engine's first request that stored KV, then 406 MiB. Two errors followed.
The Ready charge in that window was the server's steady figure, below what it held; and
the measurement recorded that reading as the server's GPU figure (1,650,458,624 B), so
every later launch of the revision charged about 1.14 GiB too much. SGLang servers also
held 1608–1656 MiB between their engine's exit and their own stop. The rule, on both
engines and on every host:

- **The charge.** Once a restart-park completes, the server holds the stopped engine's
  KV until a GPU reading of the server shows it released, and nothing else (no timer,
  no engine registration) ends that. The wake is charged its cold start beside the
  server plus that KV (and at least the Ready charge below), and the Ready instance
  after the wake keeps the KV in its charge, in the device domain on a discrete GPU
  and the one shared pool on unified memory, as the parked phase does. The evidence of
  the release is a GPU reading of the server's processes below the engine's KV cache
  (a server still holding that KV cannot show less), taken while the instance is Ready
  on that launch with no lifecycle run open; a reading of zero is none. On it the
  instance's reservation moves to the Ready charge without the KV, journaled
  `lmcache_kv_released`. A host whose GPU readings name no server process keeps the KV
  charged until the instance stops, and so does a server whose own GPU memory is not
  below the engine's KV cache: uncertainty keeps its accounting. Status shows the
  charge with the KV in it while it is held.
- **The measurement.** From a restart-park's arm until that release reading, no GPU
  reading of the server is recorded as its measurement (the resident set beyond the
  pool still is): the largest figure seen per revision never takes a reading that
  counts a stopped engine's KV. The release reading itself is a measurement.
  Callers that skip samples no larger than the last still send a reading below the
  held KV while the server may hold it.
- **Where it is kept.** `launch_lmcache.held_kv_state` (schema v56): `parking` from the
  park's arm (measurement only), `held` from its completion (measurement and charge).
  A remote host's co-residence re-check still charges the wake and the Ready phase
  without the KV; the control plane's admission, which charges it, stays the authority.

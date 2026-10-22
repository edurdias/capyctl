# Status: LMCache — 2026-10-22 (branch `feat/lmcache`)

ADR 0033 (owner decisions of 2026-10-21, amended by #179; owner input of 2026-10-22 on
LMCache issue 4825) is implemented on this branch for 0.1.7, with the implementation
amendments A1–A6 recorded in the ADR.

What changed:

- **Configuration.** Deployment fields `lmcache` (`auto`, `off`, `required`),
  `lmcache_pool` (whole GiB, at least 1 GiB, refused beside `off`) and `retention`
  (`standard`, `zero`), frozen only when stated so older revisions keep their
  fingerprints; host settings `lmcache` (`auto`, `off`) and `retention`
  (`standard`, `zero`), published with the host document; `retention: zero` beside a
  stated `engine_logs: files` refuses the role's start and forces memory engine logs.
  A vLLM `--kv-transfer-config` naming an LMCache connector is refused in every
  spelling, another connector turns LMCache off (`kv_transfer_config_set`); SGLang's
  LMCache options are owned; `LMCACHE_*` is reserved. `capyctl_config::lmcache::plan`
  decides on or off with the closed reasons of ADR 0033 §14.
- **Registration and the launch-time check.** `engine add` records LMCache's version
  and digest on the profile; the check by shape reads files only and, on SGLang,
  verifies the salt threading of LMCache's unified integration and refuses the
  legacy adapter path (issue 4825). LMCache 0.5.6 on host A has it (unified connector
  `IPCCacheServerKey(cache_salt=...)`, SGLang 0.5.22 `cache_salt=req.cache_salt`).
  `capyctl engine list` shows `LMCACHE`.
- **Salt.** The router sends vLLM and SGLang the effective salt, a domain-separated
  SHA-256 digest of the deployment, the serving revision and the caller's salt; no salt
  stays no salt.
- **The server.** One private `lmcache server` per instance, gated and journaled
  before it runs, own process group, argv exactly ADR §3 plus `--no-l1-use-lazy` (A2),
  environment `PATH`, `HOME`, the instance's `CUDA_VISIBLE_DEVICES` (and
  `CUDA_DEVICE_ORDER` when the GPU is pinned by index), `GET /status` within 30 s,
  listener check, own log beside the engine's (A3), multi-group verified stop, the
  server's `/dev/shm/cuda.shm.*` file removed after its verified exit (A5).
  `RLIMIT_CORE` 0 and per-launch memory logs under zero retention.
- **Accounting.** Pool plus allowance (1280 MiB overhead and 512 MiB device, from LC1,
  A1) then the measured figures, in the system domain (the shared pool on unified
  memory), the pool against `host_kv_limit`; a restart-parked instance also holds the
  engine KV its server keeps mapped (A5).
- **Lifecycle.** Fallback relaunch once without LMCache (sticky per revision and
  profile), `required` failing instead; restart-only parking keeps the server and wakes
  a new engine on it; a server exit fails the instance (`lmcache_server_exited`);
  role-start retirement under host `lmcache: off` or `retention: zero`. Placement gates
  and capabilities `lmcache`, `retention`; events `lmcache_off`,
  `lmcache_server_exited`; CLI exit 5 for the new codes.
- **Docs.** SPEC §§7.4, 8.2, 9.1, 9.2, 10, 12, 15.1, 20 (T45, LC rows), 21; the ADR's
  amendments; data retention, configuration, engine options, engines, requests,
  deploy, parking and errors.

Not done yet on this branch (the integrator's state at the time of writing):

- the server side of the remote path: filling `SingleLaunchPlan.lmcache` from the
  store's grant (`remote_execution.rs`), recording the host's `LaunchLmcache` reports
  and the fallback for remote hosts (`agent_sessions.rs`), the retirement pass on a
  host publication (`host_publication.rs`), and the embedded metrics targets
  (`embedded_load.rs`);
- status display: the per-instance `LMCache` line and JSON `lmcache` object
  (`snapshot.rs`, `table.rs`, `client.rs`, the management snapshot merge with the load
  table);
- coordinator-level tests for the embedded fallback, server exit and restart-park
  round trip;
- the fast and deep gates on the merged branch;
- live rows LC2–LC11 (LC1 measured, A1).

CPU and Fake-engine tests are not qualification.

# Release note: LMCache

- **LMCache: a private shared KV cache per deployment.** On vLLM 0.32.0 and SGLang
  0.5.22 or newer with LMCache 0.5.6 (or a later 0.5.x) installed and registered, each
  model instance gets its own LMCache server in host RAM, on loopback only, started and
  stopped with it, so a prompt that begins like an earlier one is answered from the
  cache after the engine's own cache dropped it, across a `restart_only` park too. On
  by default (`lmcache: auto`); `lmcache: off` or `required` on a deployment, `lmcache:
  off` on a host. The pool (4 GiB by default, `lmcache_pool`) and the server's own
  memory are reserved. `cache_salt` now reaches the engine and LMCache as a digest of
  the deployment, its revision and the salt. `retention: zero` keeps engine logs in
  memory, turns core dumps off and never starts LMCache
  ([LMCache](../guide/engines.md#lmcache),
  [zero data retention](../operations/data-retention.md#zero-data-retention)).

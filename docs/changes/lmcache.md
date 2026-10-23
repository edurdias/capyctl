# Status: LMCache — 2026-10-22 (branch `feat/lmcache`)

ADR 0033 (owner decisions of 2026-10-21, amended by #179; owner input of 2026-10-22 on
LMCache issue 4825) is implemented on this branch for 0.1.7, with the implementation
amendments A1–A9 recorded in the ADR.

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
  engine KV its server keeps mapped (A5). Measured on the allowances' basis (A1): the
  server's whole resident set (`VmRSS`) beyond its pool and the GPU memory the driver
  attributes to its processes, on unified memory too; a figure nothing read keeps its
  allowance.
- **Lifecycle.** Fallback relaunch once without LMCache (sticky per revision and
  profile), `required` failing instead; restart-only parking keeps the server and wakes
  a new engine on it; a server exit fails the instance (`lmcache_server_exited`);
  role-start retirement under host `lmcache: off` or `retention: zero`. Placement gates
  and capabilities `lmcache`, `retention`; events `lmcache_off`,
  `lmcache_server_exited`; CLI exit 5 for the new codes.
- **Remote hosts.** The launch grant carries the server's ports and charge; the host's
  per-launch LMCache report reaches the coordinator as an embedded launch's does, so
  the fallback, its codes and stickiness are the same; restart-park evidence (the server
  alone while parked, a new engine beside the same server after the wake) is checked by
  the server and again by the store; a host publishing `lmcache: off` or
  `retention: zero` has its servers stopped and Ready instances started again without
  LMCache; the servers' measured overhead and GPU memory from host load reports replace
  the allowances for later launches of the revision on that host.
- **Status.** Each instance's `lmcache` object (state, reason, server, pool, overhead,
  device, held KV, charge, measured) and a `LMCache` line in `capyctl status`, with the
  pool's used bytes, lookups and hits from the load read; LMCache drift shown under the
  engine (standalone) and in `/hosts` (remote, additive
  `RuntimeProfileStatus.lmcache_observed_digest`), journaled once per digest as
  `<profile>:lmcache`.
- **Docs.** SPEC §§7.4, 8.2, 9.1, 9.2, 10, 12, 15.1, 20 (T45, LC rows), 21; the ADR's
  amendments; data retention, configuration, engine options, engines, requests,
  deploy, parking and errors.
- **The server's short-lived child (A9, live on SGLang).** A member of the server's
  group that exits during the listener check (verified gone) is listed out and the
  group read again, instead of failing the start. A server stopped before its engine
  (`server_start_failed`, `listener_exposed` under `auto`) leaves the launch's record,
  journaled, before the engine launches without the connector, so its Ready is no
  longer refused `lifecycle conflict`; an unverified stop leaves the launch uncertain
  with no engine. Only the `lmcache` leader's exit fails the instance; re-proofs and
  restart-park evidence judge the server by its leader, and every recorded member
  stays recorded until the verified stop. Tests: the listener check's vanished member,
  the adapter's re-listing and departure, the store's departure and member exit, the
  coordinator's exposed server (embedded and remote) and child exit, and the host's
  exposed server and child exit on real processes.

Tests (T45 with related rows): coordinator tests on the real worker for the fallback
(once, sticky, `lmcache_off`), `required` failing `lmcache_unavailable:engine_start_failed`
with nothing left, a server exit and its recovery with a new server, and the restart-park
round trip, each embedded and through the host's report where it applies; the remote
report and restart-park evidence, publication retirement, host measurements, drift
(standalone, remote, CLI), the status object, line and load merge, the load table and
capacity read, the standalone scrape; store tests for ports, charge, held KV, fallback,
parking, exit and retirement. They found three bugs, fixed here: a restore of a
restart-parked remote launch was accepted with the server alone and no new engine; a
host's LMCache sample for another host's launch hid that launch's own later report; an
embedded fallback relaunch journaled `lmcache_off` twice. Two older tests were brought up
to date: a placement test now leaves `host_kv_limit` room for the default pool, and the
reserved `LMCACHE_` row's reason is checked without a requirement citation.

Fixes after the first live rows (each with its test, failing before): the vLLM entry no
longer reads CapyCTL's own `--kv-transfer-config` as an extra (vLLM 0.32.0 gives it a
random `engine_id` on every parse; `0bb4d59b`); LMCache's usage telemetry is off in the
server and in the engine with the connector (A7; `3dd2d9eb`, `ce90c0f0`); a restart-park
is proven quiescent by the engine's own gauges, embedded and remote (A8; `6ac5cb3e`);
the server's measured charge is its group's VmRSS beyond the pool plus its sampled GPU
memory, a figure nothing read keeping its allowance (A1; `d8ab848d`); the server's
short-lived child and a server stopped before its engine (A9; `7d9ab7fa`).

Verification, rebased onto `main` (`800e6614`): `scripts/ci-local.sh` passes (unit 1600
passed, 1 ignored; runtime 369); `scripts/ci-local.sh --deep --ci-shape` passes (core
1705 passed; workspace 3652 passed, 1 ignored).

Live rows, host A (GB10), Qwen3-4B (context 4096, KV cache 1 GiB, `restart_only`
unless stated), 2026-10-22, standalone role, builds of `49f10efa` (the branch before the
fixes) and `8958142d` (with the vLLM entry and engine telemetry fixes, now `0bb4d59b` and
`3dd2d9eb`; the branch was rebased since); evidence in `target/live/lmcache-live/` on the
laptop. The later fixes are not yet run live.

- **LC1, fail.** SGLang on `49f10efa`: on by default with `restart_only`, argv exactly
  §3 plus `--no-l1-use-lazy`, listeners 127.0.0.1 only (`ss`), environment `HOME` and
  `PATH`, `GET /status` 5.6–7.9 s after start (bound 30 s), `/metrics` read, `engine
  list` 0.5.6, first charge pool + 1280 MiB + 512 MiB (6,174,015,488 B). Server VmRSS
  beyond the pool 807–816 MiB (4 and 8 GiB pools), GPU 406–408 MiB. vLLM on `49f10efa`:
  every LMCache launch failed `vllm_startup_failed: sensitive_option_refused` (the
  entry read CapyCTL's own `--kv-transfer-config` as an extra, since each parse gets a
  random `engine_id`); on `8958142d` it starts on, `/status` within the bound, loopback
  only. Failures: the measured charge drops the server's GPU memory (device recorded 0,
  overhead 485 MiB as RssAnon plus RssShmem, so later launches charge pool + 485 MiB
  where the server holds about 807 MiB beyond its pool and 406 MiB of GPU memory); a
  short-lived server child (`lmcache-0`) made one start fail `server_start_failed`
  (listener check read a vanished `/proc/<pid>/fd`), after which the launch without
  LMCache failed `lifecycle conflict`; on two other starts that child's exit failed a
  Ready instance as `lmcache_server_exited`. Every server on `49f10efa` and `8958142d`
  connected to `stats.lmcache.ai:8080` (LMCache usage telemetry; on `8958142d` the
  engine had the opt-out variables but the server did not, fixed in `ce90c0f0`, not
  yet run live).
- **LC2, pass** (both engines): after three 3.4k-token fillers evicted the engine's own
  cache, the prompt again hit 3,328 tokens (13 chunks) from LMCache, and the answer at
  temperature 0 equalled the first one and a `lmcache: off` deployment's.
- **LC3, pass** (both engines): salt A, salt B, no salt: no LMCache hit across them;
  salt A after eviction hit 3,328 tokens, no salt again hit its own entry. LMCache's
  own record of the stored keys (`lmcache_mp_l1_write_chunks_total` by `cache_salt`,
  grouped from the ObjectKeys written to L1) shows 13 chunks under each salt's digest,
  equal to §5's digest computed independently, and unsalted keys under `""`.
- **LC4, fail on SGLang, pass on vLLM, part 3 blocked.** SGLang (`49f10efa`):
  restart-park refused `park_refused` ("the engine is not quiescent": the deep-park
  saver check ran for a restart-park; fixed in `6ac5cb3e`, not yet run live). vLLM
  (`8958142d`): parked with the server up (same PID, GPU 1430 MiB, charge pool + 1280 +
  512 MiB + 1 GiB held KV), the wake hit 3,328 tokens with the same answer. A `deep`
  deployment runs deep with LMCache off, but the reason reads "an unknown model type /
  an unknown GPU": the plans never carry the checkpoint's `model_type` or the GPU name,
  so no qualified-table row can match; part 3 waits for that fix.
- **LC5, fail.** New deployments under host `retention: zero` pass (`auto` off
  `retention_zero` with the event, no engine log file, `Max core file size 0`;
  `required` refused `retention_zero:lmcache`). Restarting the role into zero
  retention stopped the running server and engine (verified gone) but relaunched the
  instance with a new LMCache server, and a later start of that revision did too: the
  standalone plan reads the revision's frozen host settings.
- **LC6, pass.** `~/sglang-0.5.22-venv`: off `not_installed` in status, `engine list`
  and an `lmcache_off` event; `required` refused `lmcache_unavailable:not_installed`.
- **LC7, fail (SGLang upstream).** Clef-Flash: SGLang 0.5.22 disables its radix cache
  for this model and then crashes at idle under `--enable-lmcache`
  (`UnifiedRadixCache` has no `has_pending_cache_operations`); the relaunch without
  LMCache served decisions. The server's overhead while it ran: 787 MiB beyond the
  pool and 170 MiB of GPU memory, within the allowances, but no engine registered
  with it, so the decision model's figures with LMCache in use remain unmeasured.
- **LC8, pass** (both engines): SIGKILL to the recorded server while its engine
  started stopped the engine (verified), relaunched it once without LMCache, which
  served; warning, `lmcache_off` `engine_start_failed`, status; the next start of the
  revision went without LMCache. With `required` (SGLang) the launch failed
  `lmcache_unavailable:engine_start_failed` with nothing left running or charged.
  `start --wait` exits 13 while the relaunch is still starting.
- **LC9, pass** (both engines): SIGKILL to a Ready instance's server failed the
  instance, stopped its engine with a verified exit and journaled
  `lmcache_server_exited`; the next request relaunched the engine with a new server
  (cold cache).
- **LC10, fail.** A new deployment under host `lmcache: off` runs off `host_off`
  (status, event), but `engine list` still shows 0.5.6; restarting the role into
  `lmcache: off` stopped the running server and relaunched the instance with a new one
  (same cause as LC5).
- **LC11, pass (standalone).** `required` on the off host refused at placement
  `lmcache_unavailable:host_off`, nothing started. Not run on a server and host pair.
- **Status while Ready.** `held_kv_bytes` (the engine's KV cache) shows while the
  instance is Ready although it is charged only while restart-parked (Ready charge
  6,174,015,488 B, parked 7,247,757,312 B); the JSON reads as if the KV were held now.

Still missing: the live rows again on this head (LC1 and LC4 on SGLang, the egress
check with the server's environment fixed, the measured charge, the server-child cases,
LC1 through CapyCTL); LC5 and LC10 (a role restarted into `retention: zero` or
`lmcache: off` relaunches with a new server: the standalone plan reads the revision's
frozen host settings; not fixed); LC4 part 3 (qualified parking needs the checkpoint's
`model_type` and the GPU name in the plan; not fixed); LC7 (SGLang 0.5.22 crashes with
LMCache on Clef-Flash, upstream) and so the decision-model allowance measurement.

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

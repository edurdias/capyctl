# Status: LMCache — 2026-10-22 (branch `feat/lmcache`)

ADR 0033 (owner decisions of 2026-10-21, amended by #179; owner input of 2026-10-22 on
LMCache issue 4825) is implemented on this branch for 0.1.7, with the implementation
amendments A1–A12 recorded in the ADR.

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
  engine KV its server keeps mapped (A5), and after the wake until a GPU reading of the
  server shows it released (A13). Measured on the allowances' basis (A1): the
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

Verification, with `main` `9c7c7ab0` merged in (#187, ADR 0032: the router's forwarder
carries both the effective salt and ADR 0032's request-ID lane; #188,
`engine_telemetry`; #189, docs; merges `012105f8`, `0a73eaf3` and `54aa3740`, no
rebase), both fix branches below and the A13 fix: `scripts/ci-local.sh --deep
--ci-shape` passes, every step (unit 1632 passed, 1 ignored; runtime 369; core 1736
passed; workspace 3707 passed, 1 ignored).

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

Fixed after these rows (`fix/lmcache-host-settings` `1055f321` and
`fix/lmcache-qualified-parking` `36f39c71`, CPU tests only, not yet run live):

- **LC5, LC10 (A11).** A standalone role records the `lmcache` and `retention` it starts
  with (store schema v54, `embedded_host_settings`) before anything is placed or
  started, and placement and the LMCache plan read them in place of the revision's
  frozen copy (a server host's stay its approved publication). A role restarted into
  `retention: zero` or `lmcache: off` now relaunches the retired instance without
  LMCache, and a later start of the revision stays off. `capyctl engine list` shows
  LMCache off with the reason under the settings the running role reports (both roles'
  `list` replies carry them), falling back to the role document when it is not running.
- **LC7 (A10, owner decision 2026-10-22).** LMCache is planned off, `engine_unsupported`,
  for every decision deployment on SGLang and for any SGLang deployment stating
  `--disable-radix-cache`, at resolution, in the store's plan and in the host's, so no
  start fails on it; `lmcache: required` there is refused
  `lmcache_unavailable:engine_unsupported`. Cause, in SGLang 0.5.22:
  `mem_cache/registry.py` returns the plain radix cache under `disable_radix_cache`
  before its LMCache branch, and `managers/scheduler.py` `is_fully_idle` then calls
  `has_pending_cache_operations()` on it
  ([sgl-project/sglang#45929](https://github.com/sgl-project/sglang/issues/45929)).
  Other models SGLang disables its radix cache for are left to the
  `engine_start_failed` fallback. LC7 as first written runs again on a SGLang release
  with the fix.
- **The `lmcache_off` detail.** A detail over its 512-character bound keeps its end
  (marked `…`), so an engine log tail's last line, the error, is kept (it was lost in
  LC7).
- **LC4 part 3 (A12, finding 8).** The qualified parking table now compares the
  checkpoint's real `config.json` `model_type` (read from local disk on the embedded
  host; from the host's reported sizing or digest on a remote host, store schema v55
  adding `model_type` to `checkpoint_sizes` and `checkpoint_digests`) and the GPU
  product name hosts publish from `nvidia-smi` (`devices.<id>.product_name`). A
  combination that does not match stays off and names both. Limits: a remote first
  start before sizing plans off as "an unknown model type"; a revision frozen before its
  host published product names shows an unknown GPU until it is applied again. The
  released table is still empty, so every deep-parking deployment still runs without
  LMCache.

With the host setting `engine_telemetry` from `main` (#188, merged here; owner decision
2026-10-22, ADR 0033 A7): `LMCACHE_TRACK_USAGE=false` is set in the server and in the
engine with the connector whatever `engine_telemetry` says, and `DO_NOT_TRACK=1`
follows the setting (set under `off`, the default; not set under `allow`, in the server
too), so `allow` leaves the engine's own default and LMCache still reports nothing.
Tests: the server's environment and the vLLM engine's under both settings, SGLang's
render, the launcher's closed server environment (CPU only; not run live).

Live rerun, host A (GB10), build `0e04823c` (both fix branches and `main` `67a09823`; not
#188), Qwen3-4B (context 4096, KV cache 1 GiB, `restart_only` unless stated), standalone
role, 2026-10-23 03:50–04:49 UTC; record `target/live/lmcache-rerun/spark-0292/rows.md`
on the laptop:

- **LC1, pass** (both engines): on; first charge pool + 1280 + 512 MiB (6,174,015,488 B);
  measured overhead 821.6 MiB (SGLang) and 810.9 MiB (vLLM), device 408 and 406 MiB
  (non-zero); the second launch charged pool plus the measured figures (5,584,244,736 B
  and 5,570,973,696 B, `measured: true`); `/status` 5.23–5.86 s (bound 30 s); listeners
  127.0.0.1 only; the server's environment `HOME`, `PATH`, `DO_NOT_TRACK=1`,
  `LMCACHE_TRACK_USAGE=false`, and the engines with the connector carry both.
- **Measured charge (finding 6), pass:** an independent 0.25 s sampling of VmRSS beyond
  the pool and per-process `nvidia-smi` equals the recorded figures (SGLang exactly,
  vLLM within 268 KiB).
- **LC4, pass on vLLM:** park in about 1 s with the same server PID, server GPU 406 →
  1430 MiB, parked charge 6,644,715,520 B (the Ready charge plus 1 GiB held KV); the wake
  by request took 67.0 s and hit 3,328 tokens (13 keys) with the same answer, a new engine
  beside the same server. SGLang's restart-park was not reported in this rerun.
- **LC4 part 3, pass:** a `deep` deployment runs off `park_unqualified`, "not qualified
  with deep park on SGLang 0.5.22 / qwen3 / NVIDIA GB10" (and "vLLM 0.32.0 / qwen3 /
  NVIDIA GB10"), in status, JSON and the event.
- **LC5, pass:** the role restarted with `retention: zero` stopped servers and engines
  (PIDs gone), journaled `lmcache_off` `retention_zero` and relaunched both off; a later
  stop and start stays off; core size 0; no server; `engine list` "off (retention_zero)".
- **LC10, pass:** the same under `lmcache: off`, `host_off`, "turned off on this host"; a
  later start stays off; `engine list` "off (host_off)".
- **Egress, pass for LMCache:** `ss -tuanp` every 0.2 s over the role's process tree
  (12,872 samples across start, requests, park, wake and stop on both engines): no
  socket from an LMCache server or a connector engine off loopback. vLLM's own usage
  report (`stats.vllm.ai:443`) left the host from every vLLM engine with LMCache off
  (the `deep` deployment, and under `retention: zero` and host `lmcache: off`), since
  that build rendered `DO_NOT_TRACK` only with the connector. #188, merged since, sets
  `DO_NOT_TRACK`, `VLLM_DO_NOT_TRACK` and `VLLM_NO_USAGE_STATS` on every vLLM launch under
  the default `engine_telemetry: off`; not yet checked live.
- **The server's child (finding 7), pass, case not met:** Ready 11–18 min with 5 s polls
  and requests and a 10 ms group watch over 13 servers: extra members appeared only in
  the first 1.2 s after a server's start (the gate, a short fork on SGLang, `file` on
  vLLM), each gone in 10–20 ms, before the listener check; no `lmcache-N` member was ever
  recorded, no `lmcache_server_exited`, no instance failed. The fixed path (a member
  exiting during the check or while Ready) did not occur live.
- **Finding (fixed after this rerun, A13, below): KV still mapped after a vLLM wake.**
  After a vLLM restart-park wake the server kept the old engine's KV mapped (1574 MiB of
  GPU memory, Ready, at least 37 s) until the new engine's first storing request. The
  measurement recorded it, so vLLM's device figure became 1,650,458,624 B and later
  launches charge 6,795,710,464 B, about 1.14 GiB over the steady state; during that
  window the Ready charge (device 406 MiB) was below what the server held.

Fixed after the rerun (`fix/lmcache-wake-charge`, CPU tests only, not yet run live), ADR
0033 A13, on both engines (SGLang's restart-park goes through the same park and wake, and
its servers held 1608–1656 MiB after their engine's exit):

- **The charge after a wake.** A completed restart-park marks the launch's server as
  holding the stopped engine's KV (store schema v56, `launch_lmcache.held_kv_state`).
  The wake is charged its cold start plus that KV, and the Ready instance after it keeps
  the KV in its charge until a GPU reading of the server below the engine's KV cache,
  taken while Ready with nothing in flight, shows it released; then the reservation
  moves to the Ready charge without it, journaled `lmcache_kv_released`. No timer ends
  it; a host whose readings name no server process keeps it charged until the stop.
  Status's `charge_bytes` includes it while held.
- **The measurement.** From a restart-park's arm until that release reading, the
  server's GPU readings are not recorded as its measurement (largest seen per revision),
  embedded and from a host's load reports alike; the release reading is recorded. The
  callers that skip samples no larger than the last (the embedded watch, a host's
  samples) still send a reading below the held KV.
- Tests: the wake keeps the held KV charged through an inflated reading and a missing
  one, a low reading while parked releases nothing, the release reading lowers the
  charge once and journals it, no reading in the window becomes the revision's figure,
  and the next launch charges the steady one, on vLLM and on SGLang; the callers'
  sample filter; the v56 migration. The restart-park round trip now expects the held KV
  in the charge after the wake.
- **The embedded SGLang wake (found live on `d2901204`).** An embedded SGLang
  restart-park's wake failed in about 2 s, `sglang_startup_failed:
  loopback_rendezvous_failed` (restore `wake_uncertain`, the server stopped by cleanup):
  the park stopped the engine but left its rendezvous directory
  (`<state>/rendezvous/<incarnation>`), and the wake's engine, of the same incarnation,
  refuses one that exists. The embedded restart-park now removes it once the engine's
  stop is verified, as a remote host's already did (only this user's own directory,
  never through a symlink). Test: an embedded SGLang restart-park then restore starts a
  new engine on the same server, the directory absent when it is spawned.

Still missing: LC4 on SGLang (the restart-park quiescence fix); LC7 rerun as amended
(decisions on SGLang off, `required` refused); LC1 through CapyCTL on a server and host
pair; the vLLM usage-report egress check on this head (#188) and the egress check under
`engine_telemetry: allow` (no connection to `stats.lmcache.ai` from the server or the
engine); the post-wake KV charge and measurement fix (A13) live on both engines; the
decision-model allowance measurement, which waits for a SGLang release with the upstream
fix.

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
  memory, turns core dumps off and never starts LMCache. Decision deployments on
  SGLang run without LMCache for now: SGLang fails with it while its radix cache is
  disabled ([sgl-project/sglang#45929](https://github.com/sgl-project/sglang/issues/45929);
  [LMCache](../guide/engines.md#lmcache),
  [zero data retention](../operations/data-retention.md#zero-data-retention)).

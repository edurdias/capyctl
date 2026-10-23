# Current implementation and launch status

## LMCache — 2026-10-22 (branch `feat/lmcache`)

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
rebase), both fix branches below and the A13 fix with the embedded SGLang wake fix
(`417a2643`): `scripts/ci-local.sh --deep --ci-shape` passes, every step (unit 1632
passed, 1 ignored; runtime 369; core 1737 passed; workspace 3708 passed, 1 ignored).

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

Fixed after the rerun (`fix/lmcache-wake-charge`; run live in the third rerun below), ADR
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

Second live rerun, host A (GB10), build `d2901204` (before A13 and the SGLang wake fix),
Qwen3-4B (context 4096, KV cache 1 GiB, `restart_only` unless stated), standalone role,
fresh state, 2026-10-23 05:24–06:04 UTC; record `target/live/lmcache-rerun2/spark-0292/`
on the laptop:

- **LC4 on SGLang, fail at the wake.** The restart-park now passes (about 6 s, no
  `park_refused`): engine PIDs gone, the same server PID, server GPU 408 → 1656 MiB,
  parked charge 7,247,757,312 B (the Ready charge 6,174,015,488 B plus 1 GiB held KV).
  The wake by request failed in about 2 s, `loopback_rendezvous_failed` (fixed above in
  `a3b9cc48`, not yet run live). The parked reading also became the measurement (device
  1,736,441,856 B), so the next launch charged 6,886,703,104 B while the server held 406
  MiB of GPU memory: the SGLang case of the A13 finding.
- **LC7 as amended, pass.** Clef-Flash (`api: decisions`, SGLang 0.5.22) runs off
  `engine_unsupported` with the reason and the SGLang issue link in status, JSON and one
  `lmcache_off` event; no LMCache server, launch settings without LMCache; decisions
  served (0.77–0.80 s). With `lmcache: required` the deploy is refused, exit 5,
  `lmcache_unavailable:engine_unsupported`, nothing created.
- **`--disable-radix-cache` on SGLang, pass.** Off `engine_unsupported` with the event,
  served, no server; `required` refused, exit 5, `lmcache_unavailable:engine_unsupported`.
- **LC2 on SGLang, pass:** after three fillers the prompt hit 3,328 tokens (13 chunks) and
  the answer equalled the `lmcache: off` deployment's fresh answer. SGLang 0.5.22's
  temperature-0 output varied with the prefix cache's shape with and without LMCache
  (the first cold answer, and the off deployment's own radix-cache hit), not an LMCache
  effect.
- **LC3 on SGLang, pass:** no hit across salt A, salt B and no salt; after the fillers
  each hit 3,328 tokens with the same answer; LMCache's
  `lmcache_mp_l1_write_chunks_total` shows 13 chunks under each salt's digest, equal to
  §5's digest computed independently, and unsalted keys under `""`.
- **LC8 on SGLang, pass:** SIGKILL to the recorded server 5 s into the start stopped the
  engine (verified), relaunched it once without LMCache (served), one `lmcache_off`
  `engine_start_failed`, the warning, `start --wait` exit 13; the next start planned
  off. With `required` the launch failed `lmcache_unavailable:engine_start_failed` with
  every recorded PID gone and nothing running.
- **Egress, pass.** `ss -tuanp` every 0.2 s over the role's process tree: under the
  default `engine_telemetry` (a `deep` vLLM deployment off `park_unqualified` Ready 20
  min, and the SGLang rows; 6,729 samples) and under `retention: zero` (vLLM, 1,993
  samples) nothing left loopback, vLLM engines carried `DO_NOT_TRACK=1`,
  `VLLM_DO_NOT_TRACK=1` and `VLLM_NO_USAGE_STATS=1`, and vLLM wrote no usage file. Under
  `engine_telemetry: allow` with LMCache on both engines (Ready 9 min, 14 requests, 2,964
  samples) nothing left loopback and no process reached `stats.lmcache.ai`; the server
  and every process importing LMCache had `LMCACHE_TRACK_USAGE=false` and none of the
  other opt-outs. vLLM's own reporter ran under `allow` (its usage file was written) but
  no `stats.vllm.ai` socket was caught; a short request can fall between 0.2 s samples,
  for LMCache too.

Third live rerun, host A (GB10), build `417a2643` (A13 and the SGLang wake fix), the same
setup, fresh state, 2026-10-23 06:24–06:55 UTC, with a 0.5 s sampler of the footprint,
`held_kv_state`, the measurement, the server's GPU memory and the journal; record
`target/live/lmcache-rerun2/spark-0292/rows.md` on the laptop:

- **LC4 on SGLang, pass.** Park in about 6 s, no `park_refused`; the same server PID, GPU
  408 → 1656 MiB; the rendezvous directory of the incarnation removed (its root kept,
  0700); parked charge 7,247,757,312 B (Ready plus 1 GiB), `held_kv_state` held, the
  measured device figure unchanged (427,819,008 B). The wake by request took 86.6 s and
  hit 3,328 tokens (13 keys) with the same answer, a new engine beside the same server.
- **A13 on SGLang, pass.** The woken Ready instance stayed charged with the held KV
  (charge 7,247,757,312 B) while the server held 1800 MiB; 31 s after Ready a 408 MiB
  reading, with no request, released it, journaled `lmcache_kv_released`, and the charge
  went back to 6,174,015,488 B.
- **LC4 and A13 on vLLM, pass.** First cycle: the server's GPU fell to 240 MiB at the park
  (the KV not kept this time) and the parked instance was still charged the 1 GiB; the
  wake (62.0 s) hit 3,328 tokens with the same answer, and the first Ready reading
  (406 MiB) released it at once, journaled. Second cycle: park with the server at 406 →
  1430 MiB; the wake (62.4 s) hit 3,328 tokens with the same answer; Ready with the server
  at 1574 MiB stayed charged 7,247,757,312 B for 52 s, then a 406 MiB reading, with no
  request between, released it (journaled; 6,174,015,488 B). The measured device figure
  stayed 425,721,856 B throughout, never the parked or woken reading.
- **The next launch, pass:** SGLang charged 5,579,173,888 B (4 GiB + 856,387,584 +
  427,819,008) and vLLM 5,565,747,200 B (4 GiB + 845,058,048 + 425,721,856),
  `measured: true` (the same SGLang sequence on `d2901204` had charged 6,886,703,104 B).
- **Egress, pass.** Under the default `engine_telemetry` (a `deep` vLLM deployment Ready
  11 min and every LC4 run; 4,642 samples) and under `retention: zero` (1,668 samples)
  nothing left loopback and vLLM wrote no usage file; vLLM engines carried the three
  opt-outs and core size 0 under zero retention. Under `engine_telemetry: allow` with
  LMCache on both engines (8 min, 2,536 samples) the one socket off loopback was vLLM's
  own usage report (EngineCore to `stats.vllm.ai:443`, its usage file written the same
  second), as `allow` intends; nothing from either LMCache server or the SGLang engine,
  and nothing to `stats.lmcache.ai`; servers and engines carried
  `LMCACHE_TRACK_USAGE=false` and no `DO_NOT_TRACK`.

Still missing: LC1 through CapyCTL on a server and host pair (every row above ran on a
standalone role); the decision-model allowance measurement, which waits for a SGLang
release with the upstream fix.

CPU and Fake-engine tests are not qualification.

## LMCache 0.5.6 environments on host B — 2026-10-22 (branch `docs/lmcache-envs-host-b`)

Owner approval 2026-10-22 (ADR 0033): host B gets byte-identical copies of host A's two
LMCache environments, so the ADR 0033 LC rows can run on either host. No code changes;
there are no tests. CPU and Fake-engine tests are not qualification.

**Copies.** `rsync -a` from host A to host B over the direct link, same paths:

- `~/vllm-0.32.0-lmcache-venv`: 70,203 files, 4 symlinks, 8,250 directories.
- `~/sglang-0.5.22-lmcache-venv`: 89,994 files, 4 symlinks, 10,708 directories.

For each environment, a manifest was computed on both hosts: the SHA-256 of every
regular file, every symlink with its target, and every directory with its mode. Each
manifest is identical on the two hosts (vLLM `adbeb70b7f0a…`, SGLang `ac50e7d84930…`).
`rsync -a` does not keep hard links. On host A the SGLang environment shares hard-linked
files with the vLLM one, so on host B it takes 9.0 GiB of disk instead of 4.9 GiB; the
contents are the same.

**Models.** Host B's `~/models/qwen3-4b-instruct` (Qwen3-4B-Instruct-2507) is the same
checkpoint as host A's: its 13 checkpoint files have the same SHA-256 on both hosts. Only
the `.cache/huggingface/` download metadata differs, and CapyCTL leaves that out of the
checkpoint digest. Clef-Flash was not in host B's `~/models`, so host A's
`~/models/clef-flash` was copied there. All 54 of its files, `.cache` included, have the
same SHA-256 on both hosts.

**Still to check on host B.** Host B was in use by other work, so nothing was run there:
`import vllm, lmcache` and `import sglang, lmcache`, CUDA being available in each
environment, and the LC rows. These wait until the owner says host B is free.

## Engines no longer report usage off the host by default — 2026-10-22 (branch `feat/engine-telemetry-off`)

Owner decision 2026-10-22: the engines' own usage reporting is off for every engine by
default, and a host opts in. SPEC §13.3 (the closed engine environment) and ADR 0028
§2.1 rule 1 (names CapyCTL owns) govern where it lands.

What each engine reports, read from its own source:

- vLLM (0.31.0, `vllm/usage/usage_lib.py`, `vllm/envs.py`): rank 0 posts the platform,
  the model architecture and the engine settings to `https://stats.vllm.ai` when it
  starts, and its id and runtime figures every ten minutes after, unless
  `VLLM_NO_USAGE_STATS=1`,
  `VLLM_DO_NOT_TRACK=1` or `DO_NOT_TRACK=1` (a set `VLLM_DO_NOT_TRACK` hides
  `DO_NOT_TRACK`, so all three are set). 0.30.0 and 0.32.0 read the same three names
  (their `usage_lib.py`, read on the lab hosts); there the report is made by the rank 0
  worker (`v1/worker/gpu_worker.py`), which for one rank runs in the `VLLM::EngineCore`
  process.
- SGLang 0.5.22 (the published wheel's source): no usage reporting of its own, no usage
  library and no tracking switch; its OpenTelemetry tracing needs an exporter the
  launch asks for. The Hugging Face Hub library it imports turns its telemetry off
  under `DO_NOT_TRACK=1` (`huggingface_hub/constants.py`, read in the vLLM and
  TensorFold environments).
- llama.cpp: no usage reporting in the v0.6.0 source. [INFERENCE] v0.7.0 the same.
- TensorFold 0.6.6: no usage reporting; its GitHub release check was already off on
  every launch (`--no-update-check`, `TENSORFOLD_NO_UPDATE_CHECK=1`) and stays off
  whatever the new setting says.
- Custom engines (ADR 0031): unknown programs; `DO_NOT_TRACK=1` as the general
  convention.

What changed:

- A host setting `engine_telemetry: off | allow` (default `off`; standalone
  `host.engine_telemetry`), role-local like `engine_logs`: it never reaches the
  document deployments are resolved against, so no recipe or revision changes. Any
  other value refuses the start by name. The host and standalone roles hand it to the
  adapters when they start (`capyctl_adapters::engine_telemetry`), so it holds for
  every launch, embedded and remote.
- Under `off`, every launch environment gets `DO_NOT_TRACK=1`; vLLM also gets
  `VLLM_DO_NOT_TRACK=1` and `VLLM_NO_USAGE_STATS=1`. That covers vLLM (group workers
  too), SGLang, llama.cpp, TensorFold and custom engines. Under `allow` CapyCTL sets
  none of them and each engine keeps its own default.
- `DO_NOT_TRACK`, `VLLM_DO_NOT_TRACK` and `VLLM_NO_USAGE_STATS` are CapyCTL-owned
  names: a profile, a deployment or a custom manifest that sets one is refused with
  `engine_env_reserved:<name>`, whatever the approval says and whatever the case.
  A custom profile whose manifest already set `DO_NOT_TRACK` is refused at its next
  launch, as for any newly reserved name (ADR 0031 §2).
- Docs: the settings reference (the owned names, the setting, the variables CapyCTL
  sets), data retention (a "Nothing reported off the host" section) and the custom
  engine environment in the engines guide.

## Tests

Each failed before the change (the names were absent or settable) and passes after:

- `capyctl-adapters` `vllm::initialize` `usage_reporting_is_off_unless_the_host_allows_it`
  (T21): a single-rank and a group-worker launch get the three vLLM opt-outs under
  `off` and none under `allow`; `tests/vllm_initialize.rs`
  `the_engine_environment_is_a_closed_allowlist` (T21) sees them in the spawned
  environment.
- `tests/sglang_initialize.rs`
  `initialize_spawns_protected_waits_probes_and_reports_the_group` (T21): the spawned
  SGLang environment carries `DO_NOT_TRACK=1`.
- `tests/llamacpp_args.rs` `the_environment_is_closed`, `tests/tensorfold_args.rs`
  `the_engine_environment_is_closed`, `custom::args`
  `the_environment_is_closed_and_layered` and `tests/custom_adapter.rs` (T21):
  `DO_NOT_TRACK=1` under `off`, nothing under `allow`, TensorFold's release check off
  either way.
- `engine_telemetry` `off_writes_the_opt_outs_and_allow_leaves_the_environment` and
  `tests/engine_telemetry_host.rs` `the_role_setting_reaches_every_launch` (T21): the
  role's setting reaches every launch and every opt-out is an owned name.
- `capyctl-config` `tests/remote_roles.rs`
  `engine_telemetry_defaults_to_off_and_stays_role_local` and `standalone`
  `standalone_honours_engine_telemetry` (T21): `off` when omitted, `allow` accepted,
  role-local, any other value refused with its path; `tests/engine_env.rs` (T21): the
  three names are refused at the profile and the deployment level.

CPU and Fake-engine tests are not qualification.

## Live check (host A and host B, 2026-10-23)

Release build of `58c1bf99` from a git archive on each host (the same binary sha256 on
both). Each host had a standalone role on 127.0.0.1:38443 (management 37443) with a fresh
state directory, a 108 GiB limit and an 8 GiB free reserve. `HOME` and
`XDG_CONFIG_HOME` pointed into the run directory. The engines inherit that `HOME`: their
caches and vLLM's usage file were written there, and neither host's own
`~/.config/vllm/usage_stats.json` changed (same size, modification time and sha256
before and after). Host A ran vLLM 0.32.0 and SGLang 0.5.22 on Qwen3-4B, and llama.cpp
v0.7.0 on Qwen3-4B Q4_K_M. Host B ran the rest of step 3 on vLLM 0.30.0 with
Qwen3-4B-Instruct-2507, because host A was needed for another run and vLLM 0.32.0 is
installed only on host A. Context was 8192 everywhere. Environments were read from
`/proc/<pid>/environ` for the three names only. The evidence is in
`target/t188-live/` on the laptop.

1. **The default sets the opt-outs: pass.** On host A, vLLM 0.32.0's API process and its
   `multiprocessing` resource tracker showed `DO_NOT_TRACK=1`, `VLLM_DO_NOT_TRACK=1` and
   `VLLM_NO_USAGE_STATS=1`. SGLang's entry process and its tracker showed
   `DO_NOT_TRACK=1`, and so did `llama-server`. On host B, vLLM 0.30.0 showed the same
   three.

   Processes that rename themselves (`VLLM::EngineCore`, `sglang::scheduler`,
   `sglang::detokenizer`) show none of the names, because setproctitle zeroes the
   start of the area `/proc/<pid>/environ` reads. That is the first 728 to 733 bytes,
   and the names that survive after it are the tail of the tracker's list, the three
   names sitting at the head. EngineCore makes vLLM's report. Under the default it
   wrote no usage file in its `HOME`; under `allow` the same process wrote one and
   connected (step 3). That shows the names reach it.
2. **No egress: pass.** On host A, `ss -tuanp` was sampled every 0.2 s over the role's
   whole process tree, 77 PIDs including compile workers, from before the first
   start (03:08:06 UTC) to 03:23:47. That is 11 min 52 s after vLLM was Ready
   (03:10:53), 11 min 23 s after SGLang (03:12:24) and 11 min 21 s after llama.cpp
   (03:12:26). There were 4685 samples and 22682 loopback socket observations. There
   were no connections to anything else, and no listening or unconnected socket off
   loopback. Each engine answered a request ("42").

   On host B (vLLM 0.30.0, start to 22 s after Ready) a second watcher read the tree's
   sockets every 20 ms, 12375 times. It found no resolver lookup and no connection
   off loopback.
3. **`allow`: pass.** The role was restarted with
   `--set host.engine_telemetry=allow`, keeping the same state directory.
   - **Host A (vLLM 0.32.0):** the API process and its tracker showed none of the three
     names. vLLM wrote one report record to `<run-dir HOME>/.config/vllm/usage_stats.json`,
     1473 bytes with the architecture, GPU and engine settings. It writes that record
     just before it posts, so it attempted the report. The 0.2 s sampler saw no
     connection, because the post is too short for it. The engine still served (200,
     "42").
   - **Host B (vLLM 0.30.0):** the 20 ms watcher caught the report. EngineCore looked up
     a name through 127.0.0.53 at 03:37:08.39. It then opened a TCP connection to
     `[2606:4700:3032::6815:58f5]:443`, an address `stats.vllm.ai` resolves to, at
     03:37:08.55 (SYN-SENT, then ESTAB until 03:37:08.79). The usage file gained one
     record and the engine still served (200, "42").
   - **Restored `off` (host B):** the role was restarted without the setting. vLLM again
     showed the three names, the usage file stayed at its one record, and neither
     watcher saw anything off loopback.
4. **Reserved names are refused: pass.** `capyctl engine add <vLLM venv>` with
   `--env DO_NOT_TRACK=0`, `--env vllm_do_not_track=0` or
   `--env VLLM_NO_USAGE_STATS=0`, and with `--approve-env DO_NOT_TRACK`, was refused
   with `engine_env_reserved:<name>` (exit 2), and no profile was saved. A deployment
   with `engine_config.env` setting `DO_NOT_TRACK` or `VLLM_NO_USAGE_STATS` (vLLM), or
   `do_not_track` (llama.cpp), was refused at `deploy model` with `invalid_config` at
   `engine_config.env: engine_env_reserved:<name>`.

Not run: a TensorFold launch and a custom launch under the default (optional).

## SGLang: a cancelled request is aborted by its ID and settles on proof (ADR 0032) — 2026-10-22 (branch `feat/sglang-request-abort`)

Implements [ADR 0032](../design/adr/0032-cancelled-request-settlement.md) (owner decisions
of 2026-10-21) and its SPEC text: §10 (two amendments and the note of 2026-10-19), §13.1
and §20 T17.

## The request ID as `rid`

A launch is capable when its engine is SGLang at exactly 0.5.22 (the registered
version, from the profile's build fingerprint, never the adapter's descriptor contract)
with one tokenizer worker (`capyctl_adapters::sglang::request_abort_supported`), and, for
a remote instance, when the host's session declares the new `request_abort` capability.
On a capable lane every chat request carries CapyCTL's request ID (the
`X-CapyCTL-Request-Id` value, the first lease's ID, kept across failover offers; never the
caller's `X-Request-Id`):

- on the router's machine the forwarder adds `"rid"` to the JSON it sends the engine,
  after the caller's body passed validation, so a caller's `rid` is still refused;
- to a host the router sends it as the `X-CapyCTL-Request-Id` header. The ingress refuses
  a value that is not one canonical ULID (400, before the engine), never forwards the
  header, and sets `rid` only on chat to a capable launch.

Decisions never carry it. SGLang returns the ID as the answer's `id`, which is kept.

## Abort and keep reading

On a capable lane a cut (client hang-up, a failed, slow or oversized delivery, the request
deadline before the first output, the stream idle bound after it), streamed or collected,
settles the lease `cancelling` as before, with the same answered flag, and the cut client
gets its answer as before. The router then keeps polling the same engine response with a
sink that delivers nothing, asks the engine once to abort the `rid`, and bounds the rest
of the read by `queue.stream_idle_timeout` from the abort
(`crates/capyctl-router/src/stream.rs`, `AfterCut`). If the response ends with its final
output read whole (a finish chunk then `[DONE]`; on a capable lane the parser accepts
SGLang's graceful `finish_reason: "abort"`), the lease is acknowledged by its own ID:
`LeaseWrite::AcknowledgeCancelled` closes that one cancelling lease of the current session
and journals `request_cancellation_acknowledged` (count 1, receipt "the engine finished
this request after it was asked to abort it; request lease <ID>") in the same
transaction. Anything else (the abort not sent, the bound passed, an error event, a
truncated or unreadable end) drops the response, which closes the engine connection, and
leaves the lease to the rules before it. The role log records `router_request_abort`
(the cut and whether the abort was sent) and `router_abort_settled` (acknowledged or the
fallback's reason, with the time since the abort). Lanes without the capability behave
exactly as before.

The abort is `POST /abort_request {"rid": …}` on the engine's loopback port with the
launch's admin key (CapyCTL starts SGLang with both keys, and SGLang 0.5.22 then accepts
only the admin key there), bounded at 3 s; its 200 is never evidence (`cancel_work`
reports `Uncertain`). On the router's machine the coordinator calls the SGLang adapter's
`cancel_work`; on a host it sends the new `AbortRequest` member action (field 19), which
the agent runs only against the launch it retains: the same member, deployment, profile
and exact generation, claim held, launched, ready, not parked, the same live engine
group, and a capable launch. The agent does not journal it (one row per cancelled request
would grow without bound, and a repeated abort is harmless); a refusal or a failed post
never ends the session. At most 2 aborts are in flight per host; a third is not sent and
its request falls back.

## Restart when full

When a durable grant is refused as full, the router asks the authority whether the
deployment's requests are all held until the next stop. In one transaction the store
checks that the open lease rows have reached the limit of 32 and that every one is an
in-flight cancellation of the current session held until the next stop (cancelled before
the engine answered, or on a custom engine with no idle signal), with no live or
uncertain row. If so, for each instance holding them that is not already stopping, it
accepts an ordinary verified stop (principal `system:held_requests`, idempotency key
`held-restart:<index>:<generation>`), marks the instance's start pending (the same
reconciliation a revision restart uses), and journals `instance_restarted_for_held_requests`
(deployment, stop operation, instance index, generation, count). The stop's existing
verified-exit cleanup settles the held requests with its receipt; a stop that cannot
verify the exit keeps them. The router waits for the instance through the existing
activation wait, within the request's deadline, and offers the request again; a full
bound that is not all held answers `queue_full` as before. The trigger is the count,
never a timer.

## Tests (T17)

CPU and Fake-engine tests are not qualification.

- Adapters: `the_request_id_reaches_the_engine_only_as_its_lane_says`,
  `an_abort_finish_is_a_final_output_only_where_the_id_was_sent`,
  `cancel_work_posts_abort_request_with_the_admin_key_and_stays_uncertain`.
- Router (`router_core.rs`): `capyctl_request_id_reaches_only_a_capable_sglang_lane`; a
  stream hang-up, a collected hang-up, a deadline cut and an idle cut on a capable lane
  each abort once with the request's ID and settle on their own final output; an error or
  truncated end, an abort that never ends (closed at the bound) and an abort not sent
  (engine connection closed at once) are not acknowledged; a lane without the capability
  is not aborted; a full bound of held requests restarts and the request waits (with and
  without an instance view); a full bound not held answers `queue_full`.
- Store: an acknowledgement closes exactly that cancelling lease and journals it, also a
  lease held until the next stop, and leaves uncancelled, settled, uncertain and
  other-session leases alone; 31 held requests restart nothing, 32 restart the instance
  once, an uncertain or a live lease keeps the restart from triggering, an instance
  already stopping is not stopped again, and the restart's verified exit settles the held
  requests and starts the instance again (an unverified exit keeps them).
- Controller: only SGLang 0.5.22 with one tokenizer worker aborts (0.5.21 and two workers
  do not); `abort_request` reaches the lane's driver with the request ID (a vLLM lane
  sends nothing); the authority acknowledges one cancelled request by its lease; held
  requests filling the limit restart the instance, which serves again, and below the
  limit or beside live work nothing restarts; a request abort is sent only to a host that
  declared it (version skew), an `attempted` answer is not redelivered, and the per-host
  cap holds.
- Protocol: `AbortRequest` round-trips and names one canonical ULID; its results name
  their launch only; it needs its capability.
- Agent: the head posts an abort for its retained capable launch only, with the admin key
  on loopback, and refuses an unknown handle, a forged deployment, the next generation,
  another member, a passed deadline and a terminated launch; chat through the ingress
  gets `rid` and no ID header reaches the engine; SGLang 0.5.21 is refused and gets no
  `rid`; the ingress sets `rid` from a valid header only on a capable chat launch.

## Live (host A, SGLang 0.5.22, 2026-10-22)

The binaries were built from `8df7a399`: host A built its own from the synced tree, and
the control host's server binary came from the snapshot before the rebase, whose
`crates/` and `Cargo.*` are the same. The standalone rows used a fresh state directory
with `managed_limit` 108 GiB and `free_reserve` 8 GiB. The chat model is Qwen3-4B-Instruct.
Gauges are `sglang:num_running_reqs` and `sglang:num_queue_reqs`, read from the engine's
loopback `/metrics` every 50 ms. Times are the router's `router_abort_settled`
`elapsed_ms`, which runs from the router asking for the abort to the settlement.

- **L1, passes (criterion clarified 2026-10-22: within one decode step of the abort
  reaching SGLang's scheduler; from CapyCTL's request, at most two steps plus the request
  to the engine; see ADR 0032).** Standalone,
  context 8192. 4 background streams kept the engine busy, and a fifth (the victim) hung
  up mid-stream, 8 times in all.
  - Every victim was acknowledged by its own ID 66–78 ms after the abort (68, 78, 75, 74,
    66, 76, 77, 76 ms).
  - The engine took 38.8 ms per step with 5 requests running (5 / 128.8 tokens/s, the mean
    of 23 decode-batch lines), so settling took 1.7–2.0 steps after the router asked.
  - Between each hang-up and the end of the background traffic, running + waiting never
    read below 4.
  - In the 7 runs with the longer victim (120–200 chunks), the running count went from 5
    to 4. In the first run the victim hung up after 8 chunks, before any batch report
    counted it.
  - 40 `request_cancellation_acknowledged` events, one per lease (8 victims and the 32
    background streams cut at the end), and no lease left open.
  - Where those 66–78 ms go was measured afterwards; see "L1: when the abort reaches the
    scheduler" below.
- **L2, passes.** Standalone, `queue.request_deadline` 240 s. A 90 s deadline cannot be
  used: `timeouts.initialize` may not exceed it, and the SGLang start did not fit in 90 s
  (`launch_failed`, the engine was cleaned up). The deployment ran 1 request at a time,
  context 32768. A blocker streamed up to 24000 tokens, and the victim, sent 0.10 s
  after it:
  - waited in SGLang's queue (gauges: 1 running, 1 waiting from 1.2 s to 241.8 s);
  - was cut at its deadline with no output (stream headers, 0 chunks, then the end);
  - was aborted and acknowledged by its ID 42 ms after the abort, against a stream idle
    bound of 120 s. The waiting count went to 0 while the blocker still ran.

  The collected variant was not run.
- **L3, passes.** A server on the control host, with host A as its host: the harness's
  `normal` host policy, the ingress on port 9443 and the same model. The victim and 4
  background streams went through the server's endpoint, 3 times; gauges were read over an
  SSH forward to the engine's loopback port.
  - Each answer's `id` was the response's `X-CapyCTL-Request-Id`: the ingress set `rid`.
  - Each abort was sent (`sent: true`: the agent's `AbortRequest` answered within its
    bound), and the lease was acknowledged on the final output relayed through the
    ingress, 79, 92 and 76 ms after the abort.
  - Running + waiting never read below 4, and the running count went from 5 to 4.
  - 15 acknowledgements (3 victims, 12 background streams) and no lease left open.
  - The host ran without engine logs, so the engine's own record of `/abort_request` was
    not captured.
- **L4, passes.** Standalone, a decisions deployment of Clef Flash (context 8192). One
  decision with this state took 1.74 s (8185 input tokens). 32 decisions were sent and all
  hung up 1.0 s later, before any answer. The 33rd was sent 3 s after the hang-ups.
  - Within 35 ms the store accepted a stop and journaled
    `instance_restarted_for_held_requests` (count 32, generation 1).
  - On the verified exit, `request_cancellation_acknowledged` settled all 32 with the
    receipt "cancelled before the engine answered; stop operation … verified the exit of
    the instance's process group".
  - The instance started again 6.3 s after the stop.
  - The 33rd request was answered 200 by the restarted instance (a new binding) 140.2 s
    after it was sent; its own time to response was 1.71 s. No lease was left open.

Host A was cleaned afterwards: no role, engine or listener left, and the run
directories, the synced tree, and this session's `/tmp` and `/dev/shm` entries (65, the 6
per-launch `sglang_loads_*.shm` included) were removed.

## L1: when the abort reaches the scheduler (host A, 2026-10-22)

L1 was run again from `5bc0668a` with the same setup (standalone, fresh state, 108 GiB /
8 GiB, Qwen3-4B-Instruct, context 8192, 4 background streams, a victim that hangs up after
120 chunks), 8 runs after one trial run, with timestamps on both sides of the abort. No
CapyCTL code, protection or engine environment was changed:

- **SGLang's side**, from its own logging. The profile set `SGLANG_LOG_MS=1` (millisecond
  log times) and `SGLANG_LOG_FORWARD_ITERS=1` (step numbers) with `capyctl engine add
  --env`. The deployment's `extra_args` set `--decode-log-interval 1` (one line per decode
  step, written when the scheduler has the step's result) and
  `--enable-request-time-stats-logging` (a line when the scheduler sends a request's final
  output). The role ran with `--debug-engine-logs`, whose debug level makes the scheduler
  log `Abort running request` when it applies the abort. A `sitecustomize` hook is not
  possible: profile and deployment environments refuse every `PYTHON*` name.
- **CapyCTL's side**, from the role's event lines, each stamped with the wall clock as it
  arrived. The ask is the `router_abort_settled` stamp less its `elapsed_ms`.
  `router_request_abort` is written when the engine has answered the abort's POST, and
  SGLang's tokenizer manager answers only after handing the abort to the scheduler, so it
  bounds the tokenizer manager's receipt. The tokenizer manager logs nothing of its own,
  and the HTTP access log has whole seconds, so the receipt is bounded, not stamped.

Times are good to about 1 ms. SGLang ran its default overlap scheduler, which starts step
N+1 before it processes step N's result. Steps below are the median decode step of the
second before the abort, 37.0–38.0 ms with 5 requests running. Per-step logging did not
slow the engine (38.8 ms before).

| Run | Ask → scheduler | Scheduler → final output | Step that carried it | Within one step | Ask → settled |
|---|---|---|---|---|---|
| 1 | 31.8 ms (0.86 steps) | 38.0 ms (1.03 steps) | 38 ms | yes | 75 ms |
| 2 | 34.4 ms (0.93) | 38.0 ms (1.03) | 38 ms | yes | 76 ms |
| 3 | 35.1 ms (0.94) | 38.0 ms (1.01) | 38 ms | yes | 85 ms |
| 4 | 31.3 ms (0.85) | 38.0 ms (1.03) | 39 ms | yes | 74 ms |
| 5 | 31.1 ms (0.83) | 37.0 ms (0.99) | 38 ms | yes | 74 ms |
| 6 | 33.7 ms (0.90) | 37.0 ms (0.99) | 38 ms | yes | 89 ms |
| 7 | 28.3 ms (0.75) | 37.0 ms (0.99) | 38 ms | yes | 69 ms |
| 8 | 34.6 ms (0.91) | 37.0 ms (0.97) | 39 ms | yes | 76 ms |

- The engine answered the abort's POST 1.5–4.7 ms after the ask.
- The abort then waited for the step in progress. In every run the scheduler applied it
  0–1 ms after that step's result, with exactly one step ending in between.
- The final output left with the result of the next step: the one the overlap scheduler
  had already started. Measured from when the scheduler applied the abort, that took 37–38 ms,
  never longer than that step (38–39 ms). Each run settled within one decode step of the
  abort reaching the scheduler (the trial run too: 28.8 and 39.0 ms).
- From the ask to the final output leaving the scheduler took 65.3–73.1 ms. The router
  settled 3.6–18.3 ms after that, 69–89 ms after the ask. The two slowest runs (85 and
  89 ms) spent that extra time after the engine's scheduler, not in it.
- The asks came 2.9–9.7 ms after a step ended because the victim hangs up just after a
  chunk, and chunks leave at step ends. That is why the wait for the scheduler was close to
  a whole step each time. [INFERENCE] A cut at any other moment waits 0–1 step, so from
  CapyCTL's ask the final output takes 1–2 steps plus the HTTP hop.
- 45 `request_cancellation_acknowledged` (9 victims, 36 background streams) and no lease
  left open. Host A was cleaned the same way as before: role, engine, listeners, the run
  directory, the synced tree and the 11 `/tmp` and `/dev/shm` entries the launch left
  were removed.

## SGLang: a failed launch leaves no shared memory behind — 2026-10-22 (branch `fix/sglang-shm-cleanup`)

Found live in the live-017 recheck on host A (SGLang 0.5.22): with the scheduler
SIGKILLed during the weight load, the launch failed promptly and nothing it started kept
running, but `/dev/shm/sglang_loads_*.shm` was left behind. `/dev/shm` is RAM. SGLang's
scheduler creates that file (its load snapshot, `managers/load_snapshot.py`) and maps it,
and SGLang never removes it outside its own CI sweep; segments made through Python's
`multiprocessing.shared_memory` are removed only by the resource tracker, which the
entry's cleanup kills with everything else (SPEC §13.2: the entry ends every process it
started, verified gone).

`runtime/sglang_entry.py` now watches `/dev/shm` while `launch_server` runs and claims the
files that are the launch's on evidence, never on a name pattern: a file is claimed only
if it appeared after the launch began and, when first seen, the entry or one of its
descendants held it open or mapped (read from `/proc/<pid>/fd` and `/proc/<pid>/maps`).
When the launch fails, the entry claims once more while the processes still run, ends
them as before (frozen, killed, and `/proc` read again until none runs), and only then
removes each claimed file that is still the same file (device and inode), owned by this
user with no group or other access, and held by no live process other than the exiting
entry. Every process the user can inspect is checked. A process `/proc` hides
(non-dumpable) keeps every file only when it is alive and in the entry's process group,
where it could be one of the launch's own; found live, the user's SSH sessions and
service manager hide theirs, and so do the entry's own zombie children, and a zombie
holds nothing. A segment that existed before the launch, one another process made
during it, and one a process outside the launch still maps all stay. The watch lists
the directory every 0.25 s, so a file whose creator ended within that time of making it
is not claimed and stays. A launch that serves is not affected: its stop is unchanged.

Found on host A and not changed here: SGLang never removes its load snapshot file, so
[INFERENCE from its source] every SGLang launch, a normal stop included, leaves one
`sglang_loads_*.shm` (16 KiB) behind; host A held 38 of them from 2026-10-17 to
2026-10-22, with 31 multiprocessing `sem.mp-*` semaphores (32 bytes each) of the same
period whose source was not traced. A stop kills the whole process group, the entry
included, so nothing in the entry runs after it.

## Tests

- `runtime/tests/test_sglang_startup_death.py`,
  `test_a_failed_launch_removes_the_shared_memory_it_left` (T12): a stub SGLang scheduler
  creates and maps a `sglang_loads_*.shm` file as SGLang 0.5.22 does, creates a
  `multiprocessing.shared_memory` segment, maps a segment made before the launch, and
  makes one more that a process outside the launch (detached into its own session) maps;
  the test makes another segment of its own during the launch and then SIGKILLs the
  scheduler. The launch fails with `engine_launch_failed` as its last line, the load
  snapshot file and the multiprocessing segment are gone, and the other three remain.
  Before the fix the load snapshot file remained. During the launch a non-dumpable
  process outside the entry's process group runs too; it does not keep the files (with
  the first version of this change, which let any hidden process of the user keep them,
  this failed).
- `HiddenHolderTests` (T12): a non-dumpable process in the caller's process group makes
  the holder check uncertain, the same process outside the group does not, and once it
  is a zombie it does not either.

CPU and Fake-engine tests are not qualification.

## Live check (host A, SGLang 0.5.22)

Release build of this branch at `18dd5f0d`, standalone role on 127.0.0.1:38443 with a
108 GiB limit and an 8 GiB free reserve, Qwen3-4B-Instruct-2507. The scheduler was
SIGKILLed after `Load weight begin` and before `Load weight end`, picked from the
recorded entry's process group after its pid, start ticks and boot id were checked.
Two runs: the launch failed as `launch_failed` 2.48 s and 2.37 s after the kill, the
process group was empty, the binding was released with no reservation left, and the
engine log ended with the `engine_launch_failed` line. The load snapshot file the
launch made was in `/dev/shm` just before the kill and was gone after; no new
`/dev/shm` entry was left. A first build, whose rule let any hidden process of the
user started after the entry keep the files, left the file: diagnostic lines showed
the entry's own zombie detokenizer, then SSH (tailscaled) and systemd user processes,
as the hidden processes. The evidence is in `target/live/sglang-shm-cleanup/` on the
laptop.

## LMCache 0.5.6 environments on host A and the LC1 measurements — 2026-10-22 (branch `docs/lmcache-envs`)

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

## Prompt hang-up record before the first token; SGLang startup failure line last — 2026-10-22 (branch `fix/live-017-findings`)

Two findings of the live-017 run on host A (SGLang 0.5.22, release build of
`466b6fa8`), both fixed here.

## A stream hung up before its first token

Found live: a streamed request queued in SGLang behind a streaming request was
hung up before its first token, and CapyCTL counted it as cancelling only about
10 s later. The 10 s was the run's own pause before it hung up the request
ahead: SGLang started the queued request only then, and its first chunk was
when the router noticed. The router learned of a stream's hang-up only when it
next delivered a chunk to the client and the delivery failed (SPEC §10). After
the first token chunks come continuously, so that was prompt; before it there
was nothing to deliver, so the engine connection stayed open, the engine kept
the request queued, and its cancellation was recorded only when the engine
produced a first chunk, however long that took. The client's own connection
was closed at once: hyper drops the response body when the client goes, and
that was not watched.

`crates/capyctl-router/src/stream.rs` now races each attempt's forward against
the response body's channel closing: when the client goes, the forward is
dropped at once, which closes the engine connection, and the attempt ends as
`Cancelled`. A forward whose end is ready at the same moment still ends with
that end (the race is biased toward it), as for collected requests (#180).
Nothing about settlement changed: a request hung up before the engine's first
token is cancelling as unanswered and settles only on its instance's verified
stop; one hung up after it settles on the engine's idle reading. A request
through a host's ingress already closed its engine connection when the router's
went away (the ingress relays the engine's body directly).

## SGLang's failure line was not the log's last

Found live: with the scheduler SIGKILLed during the weight load, the launch
failed promptly (#181), but the engine log tail ended with five lines after
`sglang_startup_failed: engine_launch_failed`: `Exception ignored in:
ResourceTracker.__del__ … ChildProcessError: [Errno 10] No child processes`.
The entry's cleanup reaps every child it ended, multiprocessing's resource
tracker included (a child of the entry, started when SGLang spawns its first
process). At interpreter exit the tracker object's finalizer then waited for
its process again; Python 3.12 (3.12.13 here, 3.12.14 on host A) raises
`ChildProcessError` there and prints it, while 3.14 ignores it silently.

`runtime/sglang_entry.py` now has the tracker stop itself before the entry
reaps the rest: once every descendant is killed, `_reap` calls the tracker's
own `_stop`, which closes its pipe and reaps its process, so its finalizer at
exit has nothing to do. The verified-exit cleanup is unchanged: the tree is
frozen, killed and read again until `/proc` shows none of it running, and only
then does the entry write its failure line and exit.

## Tests

- `runtime/tests/test_sglang_startup_death.py` (T12) now asserts that the
  failure line is the last line of the entry's stderr, and that the resource
  tracker's pipe is closed after `main` returns (so the finalizer has nothing
  to wait for on any Python). Before the fix it failed under Python 3.12.13
  (the five `ChildProcessError` lines followed the failure line) and under
  3.14.7 (the tracker's pipe was still open); after it, it passes under both.
- `crates/capyctl-router/tests/router_core.rs`,
  `a_stream_hung_up_before_the_first_token_closes_the_engine_connection` (T17
  T38): a held engine, with no response head and with a head and an opening
  chunk; the streamed client hangs up before any token, the engine sees its
  connection close within 2 s (well inside the 15 s between SSE keep-alive
  comments), the lease ends cancelling as unanswered and the in-flight slot is
  released. Before the fix the engine connection stayed open (failed at the
  2 s bound); after it, it closes at once.
- `crates/capyctl-router/tests/stream_safety.rs`: the transitional test
  `a_backend_that_completes_after_a_hang_up_releases_on_completion` assumed the
  router waits for a forward after its client has gone, which it no longer
  does; it is replaced by `a_hang_up_before_the_first_token_drops_the_forward`,
  which checks that a hang-up drops a forward that has produced nothing, and
  that without a ledger the slot stays charged.

CPU and Fake-engine tests are not qualification.

## Live recheck

On host A on 2026-10-22, with a release build of this branch at `90071793`
(before its rebase onto #182, which changed only docs), the approved SGLang
0.5.22 environment, Qwen3-4B-Instruct-2507 BF16, and a standalone role set up
as in the live-017 run (managed limit 108 GiB, free reserve 8 GiB, engine logs
not in debug mode).

- **Scheduler SIGKILL during the weight load:** sent to `sglang::scheduler`
  after `Load weight begin` and before `Load weight end`, its pid taken from
  the recorded entry's process group after checking the entry's pid, start
  ticks and boot id. The resource tracker was a member of that group. The
  launch failed 2.31 s later as `launch_failed`; the process group was empty
  at 2.23 s, the binding was released and no reservation was left. The engine
  log's last line is `sglang_startup_failed: engine_launch_failed (…)`; no
  `ResourceTracker` or `ChildProcessError` line and no `FileNotFoundError`
  appear in it.
- **Streamed hang-up before the first token** (two runs): a deployment with
  `max_concurrent_requests: 1`, a streaming blocker, and a streamed victim
  waiting in SGLang's queue (running 1, waiting 1) with no body byte received.
  The victim's curl was SIGKILLed; the first status reading after it, taken
  within 0.1 s, already counted it as cancelling (`settle_at_stop` 1). SGLang
  dropped it from its queue 2.65 s and 2.44 s after the hang-up while the
  blocker kept running; that delay is SGLang's own disconnect check. The
  blocker, hung up after its first token, settled on the next idle reading
  (0.39 s and 0.37 s after its hang-up), and the victim stayed held until the
  stop settled it on its verified exit. Before the fix the same case was
  counted about 10 s late, at the moment SGLang started the request.

Evidence is on the laptop under `target/live/live-017-findings/`. The role was
stopped and host A left with no engine, CapyCTL or GPU process, its ports free,
and the 30 `/tmp` and `/dev/shm` entries the engines created removed. The
killed launch's `/dev/shm/sglang_loads_*.shm` was among them: the tracker that
would unlink it is ended with the rest of the tree.

## Live checks of #173, #180 and #181, and how quickly each engine stops — 2026-10-22 (branch `docs/abort-measurements`)

Run on host A (one NVIDIA GB10) on 2026-10-22 with a release build of `main` at
`466b6fa8`, one standalone role with its own state directory (managed limit
108 GiB, free reserve 8 GiB), and the approved environments: SGLang 0.5.22,
vLLM 0.32.0, llama.cpp v0.7.0 and TensorFold 0.6.6. Signals went only to
processes named by ownership evidence (the recorded launch identity) or to the
test's own clients. No code changes; the guide gains the measurement
([When a request is cut short](../guide/requests.md#how-quickly-each-engine-stops)).

## #181: an SGLang scheduler killed while the weights load

Qwen3-4B-Instruct-2507 on SGLang 0.5.22. As soon as the engine log showed
`Load weight begin` (no `Load weight end` yet), the `sglang::scheduler` in the
process group of the recorded entry (its pid, start ticks and boot id matched
the binding) got SIGKILL. The start failed 2.35 s later as a launch failure
(`initialize failed (launch_failed)`, "the engine exited before readiness"),
not ownership uncertainty; the process group was empty at the 2.25 s reading,
the binding was released and no reservation was left. The engine log had no
prometheus `FileNotFoundError`. Two runs gave the same result, one with
`--debug-engine-logs` (2.58 s) and one without (2.35 s).

One expectation did not hold: the log tail does not end with
`sglang_startup_failed: engine_launch_failed`. That line is followed by five
lines from Python's interpreter exit, `Exception ignored in: <function
ResourceTracker.__del__ ...>` ending `ChildProcessError: [Errno 10] No child
processes`. The entry reaps its own children, multiprocessing's resource tracker
among them, before it exits, and the tracker's finalizer then finds no child to
wait for (read from the traceback, not traced further). The failure is reported
correctly; the tail only has this noise after the line. With
`--debug-engine-logs` the launch-failure journal entry omits the tail, as
designed for raw logs.

A scheduler killed after Ready (an earlier run, by mistake) was reported as
`engine_exited`, and ordinary cleanup verified the group gone in 7.2 s.

## #180: a client hang-up on non-streamed requests, SGLang 0.5.22

- Chat, Qwen3-4B: four collected requests (`max_tokens` 4096); the clients were
  killed when `sglang:num_running_reqs` read 4, and the next reading, 0.1 s
  later, was 0 running and 0 queued. Status showed the four `cancelling`, all
  settling on the idle signal, and the next reading had settled them
  (`request_cancellation_acknowledged`, count 4, on the engine's counters). They
  were not held until the stop: CapyCTL collects a non-streamed answer from a
  stream, so each had produced output before the hang-up, and ADR 0032
  decision 6 settles an answered request on idle.
- Decisions, Clef-Flash: SGLang's gauge never read 4. It answered these long
  decisions (about 15,600 input tokens) one at a time and read 0 while
  answering the first. The clients were killed when that first answer came
  (gauge then 0 running, 2 queued): SGLang ran the other three to their ends,
  about 3.4 s each, and its gauge read 0 only 10.3 s after the hang-up. In an
  earlier run (clients killed at 1.5 s, gauge at 0), two of the four showed
  in SGLang's queue 2 s later and ran within 7 s. Status held all seven as
  `settle_at_stop` until the stop (3 min 15 s for the first four), through
  every idle reading, and the stop settled them when it verified the process
  group's exit (count 7, "cancelled before the engine answered; stop
  operation ... verified the exit of the instance's process group"). The
  expectation that the gauge drops within seconds therefore holds for chat
  and not for decisions: SGLang does not stop abandoned decisions.
- Not run: a decision or chat request through a host's ingress.

## #173: settling a cut request, SGLang 0.5.22 and vLLM 0.32.0

Qwen3-4B with `max_concurrent_requests: 1` and the standalone's
`queue.request_deadline` at 300 s: a streamed request (the victim) waited in the
engine's queue behind a streaming one (the blocker).

| | SGLang 0.5.22 | vLLM 0.32.0 |
|---|---|---|
| Victim cut at the deadline, before any token | 300.0 s; `settle_at_stop` 1 | 300.0 s; `settle_at_stop` 1 |
| Engine drops it from its queue | within 1 s | within 1 s |
| Blocker hung up after its first token, settled on idle | 0.25 s after the hang-up | 0.14 s after the hang-up |
| Victim still counted through idle readings | 59 readings, 61 s | 59 readings, 61 s |
| Settled at the stop | when the stop verified the exit | when the stop verified the exit |

A further SGLang run cut the queued victim by a client hang-up instead: it was
held until the stop in the same way. CapyCTL counted that cancellation only
about 10 s after the hang-up, in the same second as the blocker's hang-up let
SGLang start the queued request; how it noticed it then was not traced.

A first attempt set `request_deadline: 180s` on the deployment, under the
standalone's default `queue.request_deadline` of 1800 s. The queued stream was
not cut in 20 minutes: the bound on a reply's first output is the host's
`queue.request_deadline`, as the guide says, and the deployment's
`request_deadline` did not shorten it. In the code it bounds the deployment's
lifecycle windows (Initialize, wake, stop).

## How quickly each engine stops (0.1.7 item)

Four long generations at once, the clients killed once the engine counted four
running (for a stream, after each had read generated text), the engine's own
running and waiting counts read every 100 ms; three repeats, streamed and
non-streamed, through CapyCTL. Seconds from the hang-up to the first reading of
0 (median / max): SGLang 0.5.22 0.22 / 0.22 streamed and 0.10 / 0.10
non-streamed; vLLM 0.32.0, llama.cpp v0.7.0 (Qwen3-4B Q4_K_M) and TensorFold
0.6.6 (FrogNano-4B-2609 NVFP4) 0.10 / 0.10 both ways, the smallest figure the
100 ms reading shows. Directly against the engine (llama.cpp and TensorFold;
SGLang and vLLM answer only CapyCTL), streams stopped as fast; llama.cpp did
not stop non-streamed requests (0 after 64.6 to 65.9 s), which CapyCTL avoids by
streaming from the engine. vLLM's `/metrics` needs the engine's key, read from
the engine's environment by the measuring script and never printed.

## Other observations

- Ordinary stops leave the engines' scratch files in `/tmp` (the engine's
  environment carries no `TMPDIR`): each SGLang launch left a prometheus
  directory with `counter_<pid>.db` files and ZMQ sockets, and each vLLM launch
  a `vllm_dist_*` file and sockets. The #181 failure runs left no prometheus
  directory. 108 such entries from this session were removed by hand.

Evidence (laptop, not committed): `target/live/live-017/` (scripts, per-run
samples, status and management-event extracts). CPU and Fake-engine tests are
not qualification; this file records live runs only.

## SGLang: a startup failure fails the launch promptly — 2026-10-22 (branch `fix/sglang-startup-death`)

Found live on host A on 2026-10-20, in a container: SGLang's scheduler failed at
startup (a missing CUDA library) but the protected entry stayed alive, so the host
waited out the whole readiness deadline and reported that runtime ownership was
uncertain instead of a launch failure (SPEC §§6.4, 13.2). No raw log survives. The
cause below is read from SGLang's public source (v0.5.20, v0.5.21 and v0.5.22 alike)
and reproduced with a synthetic SGLang tree; it is not confirmed against that run.

A scheduler that dies before it reaches its own exception handler (an import that
fails in the spawned child, a missing shared library) sends its parent no SIGQUIT.
SGLang's `_wait_for_scheduler_ready` (`entrypoints/engine.py`) polls the scheduler
every 5 s, sees it dead and raises out of `launch_server`. The entry reported
`sglang_startup_failed: startup_error` and returned, but interpreter exit then joins
the processes SGLang had already started (the detokenizer, any live scheduler), which
are non-daemon children that never exit. The entry stayed alive, so the host's
readiness loop saw the engine present until its deadline. A scheduler that raises
inside its handler was already prompt: it sends SIGQUIT, and SGLang's handler kills
the whole tree, the entry included.

`runtime/sglang_entry.py` now catches an exception raised out of `launch_server`
and ends everything it started before it leaves: it stops every running descendant
(reading the tree again until each one is stopped, so none forks a survivor), kills
them, reaps its own children, and exits only once `/proc` shows none running,
reporting `sglang_startup_failed: engine_launch_failed` with a fixed hint. The host
sees the entry gone at its next presence check and reports a launch failure with the
engine's log tail. Nothing about release changed: memory is still released only on
the host's verified gone evidence for the launch's process group. A process that will
not die keeps the entry alive, and the readiness deadline reports uncertainty as
before, never a release.

The repeated prometheus `FileNotFoundError` traceback about a missing `/tmp` file
was ours, and the same fix removes it. With `enable_metrics` on (CapyCTL's reserved
setting), SGLang 0.5.22's `set_prometheus_multiproc_dir` (`utils/common.py`) makes
the multiprocess directory a `tempfile.TemporaryDirectory` owned by the parent, and
its children create their metric files in it. The engine's environment is cleared
and carries no `TMPDIR`, so the directory lands in `/tmp`; that only places it. The
entry's normal exit after the failure ran that directory's finalizer before
multiprocessing's join of the still-running children (atexit runs the last-registered
handler first, and `import torch` loads `multiprocessing.util`, which registers the
join, long before SGLang makes the directory), so the
children kept failing to create files in it. With the children ended first, the
directory is removed with nothing left writing into it. SGLang's own failure path
never reaches this: its SIGQUIT handler kills the parent outright, with no finalizer.

Tests: `runtime/tests/test_sglang_startup_death.py` (T12) runs the entry to
interpreter exit as its own process group's leader, as the launcher does, against a
synthetic SGLang tree whose `launch_server` does what 0.5.22's does: the parent-owned
prometheus directory, a non-daemon spawned detokenizer with a child of its own, a
scheduler that exits 1 without signalling its parent, and the readiness poll. The
entry exits 1 within seconds (about 1 s here) with `engine_launch_failed`, no running
process is left in its group, the detokenizer and its child are gone, no
`FileNotFoundError` was printed, and the prometheus directory is removed. Before the
fix the same test timed out at 60 s with the entry alive, and the surviving child
printed 83 `FileNotFoundError` tracebacks in its first 5 s. CPU and Fake-engine tests
are not qualification.

Live check still needed, on host A with the approved SGLang 0.5.22 environment: start
a deployment and, while the weights load, send SIGKILL to its `sglang::scheduler`
process (a member of the launch's process group, whose leader is the recorded
entry). A killed scheduler runs no handler, which is the path seen live. Expect the
launch to fail within about 10 s as a launch failure, not ownership uncertainty; the
log tail to end with `sglang_startup_failed: engine_launch_failed`; the launch's
process group to be empty and its memory released on that evidence; and no prometheus
`FileNotFoundError` in the engine log.

## A client hang-up stops the engine's work for non-streamed requests too — 2026-10-21 (branch `fix/collected-hangup-abort`)

Owner decision of 2026-10-21 (0.1.7); SPEC §10 amended 2026-10-21, §20 T17. A
client that hung up on a streamed request already closed the engine connection
(SPEC §10, amended 2026-10-01 and 2026-10-02), but a non-streamed request, a
collected chat completion or any decision at `/v1/systemone`, ran on to its end:
its forward runs on a task of its own so that a dropped handler never leaves the
request counted, and nothing told that task the client had gone. Now the task
races the forward against the client's departure (the answer channel's receiver
dropped with the handler) and drops the forward when the client goes, which closes
the engine connection. The request is `cancelling`, settled by the amendment of
2026-10-21 and [ADR 0032](../design/adr/0032-cancelled-request-settlement.md)
decision 6: a collected chat request the engine had answered (an output event
read) settles on the engine's idle reading; one cut before its answer, and every
decision, is held until the instance's next verified stop or restart. The task
still settles every lease, and an answer it observed whole before it saw the
client go still closes the lease completed (a ready completion wins the race). A
client gone before a request was sent to an instance closes its lease as not
accepted, and no other instance is offered it. A host's ingress already ran its
engine request inside the forwarded request and its relayed body, so when the
router's connection goes away the ingress drops its engine connection, before the
engine's response head as after; this is now documented in the code and pinned by
a test. SGLang's per-request abort (ADR 0032) is a separate later change and is
not implemented here.

Tests (T17 T38 T43, CPU and scripted engines; a new testkit engine double,
`HeldEngine`, reads each request on a raw socket, never finishes answering, and
counts the connections its client closes): router
`a_hang_up_mid_collected_request_closes_the_engine_connection` (before the
engine's head and after a head and a chunk that only opens the reply; the engine
sees its connection close within 5 s; the lease ends `cancelling`, unanswered;
the in-flight slot is released),
`a_hang_up_mid_decision_closes_the_engine_connection` (the same for a decision),
`a_collected_request_cut_at_its_deadline_closes_the_engine_connection`, and unit
tests `a_completion_ready_with_the_hang_up_wins` and
`a_hang_up_cuts_and_drops_a_pending_forward`; agent
`the_engine_connection_closes_when_the_upstream_goes_away` (a chat request before
the engine's head and after its opening chunk, and a decision, through the
ingress; the forwarding slot is freed). The two router hang-up tests fail on the
code before the change (the engine connection stayed open past the 5 s bound);
the deadline and ingress tests pass on it too, pinning behavior that already held.
CPU and Fake-engine tests are not qualification.

Live check still needed (host A, SGLang 0.5.22, in an owner-scheduled live
session): start 4 non-streamed long generations against a chat deployment (for
example `max_tokens` 4096 each, `stream: false`), wait until the engine's
`sglang:num_running_reqs` reads 4, kill the 4 clients, and read the gauge again:
it should drop to 0 within a few seconds instead of staying at 4 until the
generations end. `capyctl status deployment` should show the 4 requests
cancelling (settling on the idle signal if each had produced output, otherwise
at the next stop or restart). Repeat with decision requests on a decisions
deployment, and once through a host's ingress (host A as a host of a server on
the control host).

## vLLM 0.32 qualified for chat; decisions stay refused — 2026-10-21 (branch `feat/vllm-0.32-decisions`)

Owner decision 2026-10-21. ADR 0018 §1: vLLM 0.32.0 joins the verified set for chat.
ADR 0030 §6 amendment: `api: decisions` stays refused on vLLM on every version
(`capability_missing:decisions`), now with the measured reason: vLLM 0.32.0 serves
`POST /v1/systemone` but answers Clef and Clef-Flash from their text output instead of
their decision head; vLLM is admitted once a release uses these models' decision heads.
vLLM 0.32.0's `--grpc-port` (a second listener) is reserved (SPEC §8.2, T21). The vLLM
adapter takes the deployment's API and, for a decisions launch, proves readiness, the
post-wake probe and the wake canary with the fixed decision (ADR 0030 §§3, 4), never a
chat completion; no resolved deployment reaches it while the refusal stands. The host A
venv `~/vllm-0.32.0-venv` was made with `uv venv --python 3.12 --seed --managed-python`
and `uv pip install vllm==0.32.0 --torch-backend=cu130` (torch 2.13.0+cu130).

Tests: T43 (`capyctl-config` decisions: vLLM 0.31.0, 0.32.0, 0.33.0 and a versionless
build are refused with the reason; `vllm_initialize`: a decisions launch is ready only on
the fixed decision, no chat probe, the canary is a decision; `vllm_residency`: a woken
decisions launch is proven by the decision), registration (0.32.0 verified), T14/T21
(`runtime/tests/test_vllm_entry.py`: the 0.32.0 parser launches, `--grpc-port` reserved
however spelled). CPU and Fake-engine tests are not qualification.

Live on host A (GB10), isolated standalone, managed limit 108 GiB:

- Qwen3-4B chat with deep park on vLLM 0.32.0: start 96 s (digest included), chat and a
  stream answered, park freed about 12 GiB, two wakes on request (8.3 s and 9.3 s) passed
  the wake canary (no mismatch recorded), stop 5 s.
- Decisions, measured with a build that let vLLM resolve `api: decisions`: Clef-Flash and
  Clef (BF16) became ready on the fixed decision, answered through the router, and were
  refused `api_mismatch` on chat and `invalid_request` on a malformed body. capyctl-bench
  (Clef-Flash; results in capyctl `target/live/vllm-032/`): API-Bank refused on all 508
  requests ("at most 26 options for this model"), Home appliances 0.0%, GPQA Diamond
  44.4%, Typed Decisions 69.5% with ECE 0.243, AG News 90.2%, BoolQ 89.8%; 1 question
  about a 2k-token text p50 517 ms. On SGLang 0.5.22 the same model scores API-Bank
  93.1%, Home appliances 97.7%, GPQA 51.0%, Typed 70.2% with ECE 0.012. Clef on vLLM
  scored 43.2% on Home appliances (SGLang 79.5%).
- Parking, for the later ADR 0030 §7.2 decision: Clef-Flash run directly on vLLM 0.32.0
  with sleep mode slept at level 2, woke, reloaded its weights and answered the fixed
  decision with identical probabilities.

Live check still needed: none for chat; decisions on vLLM once a release reads these
models with their decision heads.

## A changed hostname is reported clearly — 2026-10-21 (branch `fix/hostname-changed`)

SPEC §15.2 ("existing identity missing or mismatched: fail for recovery"). A
standalone role publishes its host under the machine's hostname (the device
inventory's `host_id`; the fixed name `standalone` when it observes no
inventory), and its store records that name as the embedded host's namespace.
Started on existing state under another hostname, as every new container is
unless `docker run --hostname` fixes it, the role failed late, when it
published its policy, with the opaque `internal: deploy: blocked: resource
policy refused: resource policy revision conflict` (found in the discrete-GPU
container run, `docs/container-qualification-discrete`).

The role now compares the hostname with the stored embedded host name right
after it derives its host policy, before the secrets key, the owned state, the
rendezvous sweep or any policy is written, and refuses with the new code
`host_name_changed`, exit 2 (invalid configuration, which the packaged units do
not restart). The message names the stored name, the current hostname and the
fix: `docker run --hostname <stored>`, or setting the machine's hostname back.
No setting names the host instead of the hostname, so none is offered. Only
the embedded host's record is read: the enrolled hosts a server store also
holds are never compared, and the host role, which uses its enrolled name, is
unchanged. A start without a device inventory, which publishes `standalone`
rather than the hostname, is not compared here and keeps the earlier refusal.

Code: `Store::embedded_host_name` (`capyctl-store`), `roles::check_host_name`
and `StartError::HostNameChanged` (`capyctl-cli`), the code and its exit in
`output.rs`. Tests (T06): the store returns no name on fresh state and the
published one after an import, unchanged by an enrolled host; a booted
standalone proceeds under its own name and under no hostname-derived name,
fresh state proceeds under any name, and another name is refused
`host_name_changed`, exit 2, naming both names and `docker run --hostname`;
the errors page documents the code with its exit. Docs: the errors guide, the
exit-code table and the container section of the install guide.

CPU and Fake-engine tests are not qualification. Live on the laptop,
2026-10-21 (release build of `main` d9d5640d with this branch and
`fix/discrete-startup-peak`, the discrete container setup: `ubuntu:24.04`,
CDI, `--user 1000:1000`): on state created under `--hostname capyctl-laptop`
with a stopped vLLM deployment, the container recreated without `--hostname`
(hostname `22be0919ef8d`) exited 2 with `host_name_changed` naming
`capyctl-laptop`, `22be0919ef8d` and `docker run --hostname capyctl-laptop`;
recreated with `--hostname capyctl-laptop` it started, its host online and the
deployment listed.

## Discrete GPUs: the startup peak is measured per memory domain — 2026-10-21 (branch `fix/discrete-startup-peak`)

Owner decision 2026-10-21; ADR 0019 amendment A2, with ADR 0014 amendments A2
(measured startup), A12 (kernel builds) and A13 (process identities). Found
during the discrete container qualification on the 16 GB laptop card: on a
discrete GPU CapyCTL never measured a deployment's startup peak. The cold phase
charges two domains, the card and host RAM (ADR 0019 §3), and the measurement
ran only for a cold phase of one allocation (`StartupPeak::new` in the
coordinator, `measurable` in `capyctl-config`), so every start kept the
estimate (`startup.provenance` `default`).

What changed:

- `capyctl_config::effective::startup_measure` replaces `measurable`: one pool
  is measured as before; a discrete cold phase of one card and one host RAM
  allocation is measured per domain.
- The coordinator samples the host's processes with its availability while an
  Initialize runs on a discrete GPU and, once the engine is ready, matches them
  to the process identities it reported: the card's peak is the most GPU memory
  the driver attributed to them in one sample, host RAM's the most resident
  pages (`RssAnon` + `RssShmem`). Another program's memory is not counted.
  Samples inside a kernel build and a start beside another activation on its
  host record nothing, as on unified memory.
- `startup_measurements` is keyed by memory domain (store schema v52; each
  earlier peak keeps the domain of its revision's one cold allocation, read
  from the frozen revision). A start freezes each measured domain's peak into
  its plan (`startup_domains`), never below that domain's steady charge; a
  domain not yet measured keeps its estimate. A declared `memory.startup` or
  `resources:` block is never replaced.
- A remote host is sent the card's figure as `SingleLaunchPlan.startup_bytes`
  and host RAM's as the new `startup_host_bytes` (field 18, capability
  `startup_host_bytes`, ADR 0017), and charges each on its own domain. A host
  without the capability is refused `host_capability_missing:startup_host_bytes`
  for such a launch before anything is sent.
- Status: `startup.device_provenance` and `startup.host_provenance` beside
  `startup.device_bytes` and `startup.host_bytes` (deployment and starting
  instance), `startup.provenance` the card's, and each `startup.measured`
  record names its `domain`.

Tests (T14 T26): `a_discrete_startup_peak_is_measured_per_memory_domain` and
`a_discrete_domain_keeps_its_estimate_until_it_is_measured` (coordinator, both
failing before: nothing was recorded on a discrete host),
`v52_keys_startup_peaks_by_domain_and_carries_rows_across` (store migration),
`a_launch_carries_the_host_ram_startup_charge_only_beside_the_cards` and the
capability cases in `every_post_baseline_field_names_its_capability`
(protocol). The unified startup tests pass unchanged. CPU and Fake-engine tests
are not qualification.

Live on the laptop, 2026-10-21 (RTX 4090 Laptop GPU, 16 GiB; vLLM 0.31.0;
release build of `main` d9d5640d with this branch and `fix/hostname-changed`),
a deployment stating only `name`, `engine` and `model`. Qwen3-4B did not fit:
its host RAM estimate (15.2 GiB) was above the 12 GiB the laptop had free, so
the run used Qwen2.5-1.5B-Instruct. Isolated standalone on the host: the first
start became Ready in 200 s with `startup.measured` one `gpu0` record of
8149532672 bytes (7772 MiB; `nvidia-smi` peaked at 7774 MiB for the engine's
processes) and one `system` record of 6874185728 bytes (sampled peak
`RssAnon` + `RssShmem` of the engine's processes 7.3 GB), both `measured`.
After a stop, the starting instance's `startup` reserved 13.2 GiB on the card
and 8.3 GiB of RAM (`measured`; each the steady charge, above the measured
peak) and was Ready in 39 s. In the discrete container setup (`ubuntu:24.04`,
CDI, `--user 1000:1000`, `--hostname capyctl-laptop`): first start Ready in
165 s, `gpu0` 8149532672 bytes (`nvidia-smi` 7774 MiB) and `system`
6209007616 bytes (sampled 7.1 GB), both `measured`; the restart reserved the
same figures and was Ready in 42 s.

## A cancelled request settles on engine idle only after the engine answered it — 2026-10-21 (branch `fix/settle-after-engine-output`)

Owner decision 6 of 2026-10-21 in
[ADR 0032](../design/adr/0032-cancelled-request-settlement.md); SPEC §10
amended 2026-10-21, §20 T17. A `cancelling` request (client hang-up, request
deadline or stream idle cut; streamed, collected or a decision) used to settle
on the first engine-wide idle reading taken after its cancellation. Reading the
pinned engines' source showed that such a reading does not prove a request the
engine never answered cannot still reach its scheduler: vLLM 0.31.0 and 0.32.0
send no abort for a request cancelled while it is being added to the engine
core; SGLang 0.5.22 dispatches with no disconnect check after tokenizing, and
its grammar queue is not in the gauge CapyCTL reads; llama.cpp b11669 answers
`/metrics` through a task posted ahead of queued inference tasks and counts
neither its primary queue nor work before posting; TensorFold 0.6.6 (CUDA)
checks for a disconnect before it counts the request in `/health`. Source
reading only, not reproduced live.

Now the idle reading settles only a cancellation the engine answered first:
for a stream or a collected chat request, an event read from the engine that
is backend progress (content, reasoning, tool-call or finish; not headers and
not a chunk that only opens the reply, which TensorFold sends before it counts
the request); and a complete engine error answer (5xx other than 503 with a
JSON object body), which is the engine's whole answer. Any other cancellation,
including every decision or collected request cut before its answer, settles
only on its instance's verified exit at the next stop or restart, the rule ADR
0031 §6 gave engines without an idle signal: it is never asked about, stop,
idle stop and switch drains do not wait for it, an idle park or a switch
releases the instance by a stop (journaled `ready_idle_unanswered_cancellation`
or the switch's stop reason), and the verified exit journals
`request_cancellation_acknowledged` with the receipt "cancelled before the
engine answered; stop operation ...". An operator's park still drains every
recorded request first, so it does not proceed while one remains. The same
holds for remote instances (the host's load report path).

Schema v51 adds `request_lease_cancellations.engine_answered` (0 or 1). A row
written by 0.1.6 reads 0, not answered: the conservative reading. In practice
an upgrade restarts the role, which already turns every lease of the previous
session uncertain, so those leases keep settling as retired leases do.
Status: each instance's `requests` gains `settle_when_idle` and
`settle_at_stop` (decimal strings); the text line reads, for example,
`cancelling: 3 (2 settle on the engine's idle signal; 1 at the next stop or
restart: cancelled before the engine answered)`.

Tests (T17 T38 T43 T33, CPU and scripted engines): store
`an_unanswered_cancellation_is_never_settled_by_an_idle_reading`,
`an_unanswered_cancellation_stays_charged_until_the_verified_stop`,
`an_unverified_stop_keeps_an_unanswered_cancellation`,
`an_unanswered_cancellation_turns_a_park_into_a_stop`,
`v51_reads_an_existing_cancellation_as_unanswered`; controller
`a_cut_before_any_engine_output_settles_only_at_the_verified_stop`,
`a_switch_releases_an_unanswered_cancellation_by_a_stop`,
`a_remote_cancelling_lease_settles_only_on_an_idle_sample_after_the_hang_up`
(an unanswered one beside it stays charged), `closing_as_cancelling_keeps_the_lease_charged`;
router `a_stream_cut_before_any_engine_output_is_unanswered`,
`a_hang_up_after_an_engine_chunk_is_answered`,
`a_collected_request_cut_before_its_answer_is_unanswered`,
`a_decision_is_bounded_by_the_request_deadline` (unanswered),
the engine error answer tests (answered); CLI
`status_shows_each_instances_cancelling_requests_and_what_settles_them`.
Before the change the old code settled every cancellation on the first idle
reading after it, so the tests that keep an unanswered one charged through
idle readings could not pass (they do not compile against the old types
either). CPU and Fake-engine tests are not qualification. Live check still
needed: on each engine, a request cut at its deadline before its first token
stays `settle_at_stop` through idle readings and is acknowledged by the next
stop; a hang-up after the first token still settles on the idle reading.

## Tests no longer depend on the machine's GPU, memory or speed — 2026-10-21 (branch `test/hardware-independent`)

Tests only; no product behavior changes. Several tests passed or failed with
the machine running them rather than with the code (AGENTS.md: tests state
their observations). Each now states what it observes through an existing
seam, and none was deleted, skipped or loosened:

- **GPU (SPEC §7.2, design §1).** Every `capyctl` process a binary test spawns
  observes no GPU through `CAPYCTL_TEST_PINNED_GPU`, read only by debug builds
  like the existing `CAPYCTL_TEST_PINNED_HOST_MEMORY`: `nvidia-smi`'s memory
  and process samples and the device inventory read nothing of the machine.
  On the laptop, whose GPU another program held (1.9 GiB of 16 free), every
  standalone launch in `engine_exit` (2 tests) and `role_shutdown` (5) failed
  `insufficient_device_memory`. Their documents are now always sized for the
  unified shape the pinned binary publishes.
- **Device inventory (SPEC §3).** A standalone boot through the test entry
  points collects no device inventory, as it already sampled no GPU. On
  host A the real collector named the host after the machine, so
  `admission_figures::a_role_warns_when_its_limits_exceed_the_memory_available`
  and `switching_cli::standalone_memory_limits_follow_the_document_and_its_overrides`
  and `standalone_parked_limit_follows_the_document_and_its_overrides` found
  their warnings and policy under `spark-0292` instead of `standalone`.
  Production boots still run the collector.
- **Model store (ADR 0008).** `prune_sources_lists_then_removes_only_unreferenced_copies`
  resolves the store with an empty environment layer; the process's own
  named host A's real store, which the listing then showed.
- **Speed.** `only_an_engine_answering_busy_is_not_signalled` no longer
  requires the idle, exited and loading engines to finish within 900 ms: they
  get a minute-long idle-check bound and must complete inside the 15 s the
  stop is awaited, so one that waited for its bound still fails.
  `group_engine_env_is_approved_everywhere_and_equal` and
  `empty_journal_reconnect_settles_on_recorded_identities_only` state the
  longest observation TTL a policy accepts (10 s); under the default 2 s a
  loaded run (703 s for the controller suite) committed on stale evidence and
  the store refused it (`evidence freshness`).

Each fixed test passed 20 runs in a row on the laptop, under the gate lock.
CPU and Fake-engine tests are not qualification; no live check is needed.

## Hosts under a systemd user service are no longer refused — 2026-10-21 (branch `fix/cgroup-path-at`)

SPEC §7.2 and the operations guide's "Memory limits" section
([install](../operations/install.md)): a host names the cgroup v2 cgroup that
bounded its memory reading in each host domain's `memory_source`
(`cgroup_v2:<cgroup>`). The controller checked that token with the alphabet it
uses for every other name (letters, digits, `-_.:/`), so a cgroup systemd
names with `@` (a user service, `user@1000.service`, or any template unit
instance, `foo@bar.service`) failed the inventory check and ended the session
with `host session authorization failed`. Found by the CI-shape deep gate,
which runs under `systemd-run --user --scope` and so places the tests' hosts
under `user@1000.service`.

`memory_source` now has its own check: letters, digits, `-_.:/@`, and `\`
only as the start of a systemd `\xNN` escape (`dev-disk-by\x2duuid`), up to
512 characters. Control characters, whitespace, other punctuation and
non-ASCII are still refused. Every other inventory name keeps the narrower
check. The token is status only: it reaches `capyctl inspect host --json` and
the session view, never a file path or an SQL statement, and the host does not
validate it before publishing.

Tests (failing before, passing after where new): the controller accepts a
session whose memory source is a user-service cgroup or a template-unit
cgroup and shows it in the host view, and still refuses one with a newline,
an escape sequence, a space, or past 512 characters (T07, T26); unit tests
cover `\xNN` escapes, the length bound at 512 and 513, malformed escapes and
other refused characters, and that other names still refuse `@` (T26). The
CI-shape deep gate passes. CPU and Fake-engine tests are not qualification. A
live check is still needed: a role run as a systemd user service or template
unit joins its server and reports its cgroup.

## Containers run live on a discrete GPU — 2026-10-21 (branch `docs/container-qualification-discrete`)

Live qualification of a standalone role running as the main process of a
container on the development laptop (RTX 4090 Laptop GPU, 16 GiB, driver
615.71.09, Docker 29.1.3 with CDI), with the published 0.1.6 x86-64 release
binary (commit `04087627`, verified against `SHA256SUMS`). Documentation only:
no code changed. It follows the GB10 run (`docs/container-qualification`).

The container: `docker run --init --device nvidia.com/gpu=all --cap-drop ALL
--security-opt no-new-privileges --user 1000:1000 --shm-size 8g --hostname
capyctl-laptop`, the container's own `/proc` and cgroup namespace, ports 38443
and 37443 published to the host's loopback only, and read-only mounts at their
host paths of the binary, vLLM 0.31.0, TensorFold 0.6.6, the CUDA 13.4 toolkit,
the `uv` interpreter directory and the models, beside a fresh writable state
directory. Host RAM limits stated for the run: managed 16 GiB, free reserve
4 GiB. The image is `ubuntu:24.04` plus `python3`, `gcc`, `g++`, `ninja-build`,
`/usr/local/cuda` linked to the mounted toolkit and a loader cache for the CUDA
libraries, built with Docker's legacy builder (no `buildx` on the laptop): a
build stage holds a copy of CUDA's shared libraries at the mount path, runs
`ldconfig`, and only `/etc/ld.so.cache` reaches the image.

Results:

- Qwen3-4B on vLLM 0.31.0 (`residency: deep`, context 8192, KV cache 2 GiB):
  cold start 209 s (start request to the engine's listener, with FlashInfer's
  and torch.compile's first-start work: compile 42.9 s), chat collected
  (`Jupiter is the largest planet in our solar system.`, 12 tokens) and
  streamed (14 data lines) HTTP 200 in about 0.25 s each (`/v1/models`
  answered HTTP 200 through the published port from the host); park under a
  second (0.57 s from the park request to the measured parked charge; vLLM
  fell asleep in 0.34 s), wake
  through a request 14.3 s, stop 2.4 s with every engine process gone from the
  container. `parked.measured` 1251999744 bytes on `gpu0`. A second start, in
  the `--memory` run, took 60 s with the compile caches in the state
  directory.
- FrogNano-4B-2609 MLX 4-bit on TensorFold 0.6.6 (the laptop recipe
  `tensorfold/frognano-4b-mlx-4bit-rtx4090`: `restart_only`, declared 11 GiB
  card and 2 GiB RAM, context 32768, `--no-drafts`): cold start 249 s
  (TensorFold built its CUDA extensions on this first start and reported
  "loaded in 176.7s"; the readiness request then built one more), chat
  collected (`Jupiter is the largest planet in our solar system.`,
  `finish_reason` `stop`) and streamed (401 data lines) HTTP 200; park refused
  as it should be (`unsupported_capability`), stop 2.4 s.
- `startup.measured`: absent for both, and not because of the container. On a
  discrete card the derived cold phase charges two domains (the card and host
  RAM), and the startup peak is measured only for a single-domain cold phase
  (`StartupPeak::new` in the coordinator, `measurable` in
  `capyctl-config/src/effective/startup.rs`), so vLLM's status keeps the
  derived 14220787655 card bytes plus 4294967296 RAM bytes (`provenance`
  `default`). TensorFold's peak is declared (`provenance` `resources`) and is
  never replaced.
- Per-process GPU memory (ADR 0007, ADR 0019): `nvidia-smi
  --query-compute-apps` inside the container reports the container's PIDs on
  the discrete card too, equal to the container's `/proc` and to the host's
  `NSpid`: vLLM host 1306164 → container 654 (11088 MiB ready, 1194 MiB parked,
  the same in both readings), TensorFold host 1525551 → 3564 (3030 MiB ready,
  3198 MiB after two requests), vLLM in the `--memory` run host 1685166 → 315
  (10368 MiB). The parked charge above was measured from those readings, so
  attribution works without a translation.
- Card sizing (ADR 0019): the card's domain has the card's total, 17171480576
  bytes (16376 MiB), with or without `--memory`; its managed limit is
  15797762136 bytes (14.71 GiB, an 8 % reserve). vLLM rendered
  `--gpu-memory-utilization 0.75` and `--kv-cache-memory-bytes 2147483648`, and
  was charged 14220787655 bytes: 75 % of the card plus the 1.25 GiB CUDA context
  and graphs. A deployment asking `memory.request: 14GiB` was refused at deploy
  (`insufficient_device_memory`: 16374562816 bytes needed, above the
  15797762136 the device domain manages).
- Memory limits: without `--memory` the role reported `meminfo` (cgroup `/`,
  `memory.max` `max`, capacity 65743319040 bytes); with `--memory 24g` it
  reported `cgroup_v2:/`, capacity 25769803776 bytes (24576 MiB), said so at
  start, and vLLM started and served within it.
- Found and documented: TensorFold's registration needs `ninja` on the engine's
  `PATH` (`toolchain_missing` without it); the legacy builder needs the
  build-stage form of the loader cache; and a role restarted from persisted
  state in a new container, which Docker gives a new hostname, refuses to start
  (`internal: deploy: blocked: resource policy refused: resource policy
  revision conflict`), since the host is named after the hostname: a fixed
  `--hostname` avoids it.
- Nothing left running: the container and the image removed, `docker ps`
  showing only the laptop's four other containers, no GPU processes.

CPU and Fake-engine tests are not qualification; this entry records a live
run. Still not run live in a container: Podman and a host role joined to a
server.

## ADR 0033 accepted: LMCache — 2026-10-21 (branch `docs/adr-0033-lmcache`)

ADR 0033 records the owner's decisions of 2026-10-21 for LMCache in 0.1.7, including the
owner's answers to the ten open questions of its first draft. LMCache 0.5.6 (the security
floor: CVE-2026-105192 is fixed in it), or a later 0.5.x release that passes CapyCTL's
launch-time check by shape, runs in multiprocess server mode only: vLLM 0.32.0 or newer
connects through LMCache's external `LMCacheMPConnector`, named with its module path, and
SGLang 0.5.22 or newer through `--enable-lmcache --lmcache-config-file`; the in-process
`LMCacheConnectorV1` is refused because it ignores `cache_salt` (LMCache issue #2878).
vLLM 0.31, older SGLang, llama.cpp, TensorFold and custom engines show "LMCache off" with
the reason. Every model gets LMCache by default, hybrid layouts and decision deployments
included (decisions on SGLang only: vLLM stays refused for decisions, ADR 0030 §6
amendment of 2026-10-21; amended 2026-10-22, A10: decision deployments on SGLang, and
SGLang deployments stating `--disable-radix-cache`, run without LMCache until LC7 passes on
a release, [sgl-project/sglang#45929](https://github.com/sgl-project/sglang/issues/45929)). Each deployment instance gets its own LMCache server, started and stopped by
CapyCTL with it, on loopback ports CapyCTL leases, checked with ADR 0031's listener check
because the 0.5.6 admin HTTP API has no authentication; the script API stays off; host
RAM (L1) only, LRU within the private pool, nothing on disk. The server stays up while
its model is parked, with its pool charged, and stops with the stop; a `restart_only`
deployment keeps a warm cache across the restart. Parking wins: a deep-parking deployment
uses LMCache only on an engine version, model type and GPU combination qualified live,
and elsewhere keeps deep parking and runs with LMCache off and the reason shown. An
engine that fails to start with the connector is relaunched once without LMCache, with a
warning and an `lmcache_off` event (under `lmcache: required` the launch fails); a server
that dies while its engine runs fails the instance, which recovers with both relaunched.
`cache_salt` is no longer forwarded unchanged: vLLM, SGLang and LMCache receive a
domain-separated SHA-256 digest (64 hex characters) of the deployment, its revision and
the caller's salt; no salt stays no salt; a salt is not authorization. The pool is 4 GiB
per instance by default (`lmcache_pool`), charged with the server's measured overhead in
host RAM; the overhead allowance and the server's start bound are set from live row LC1's
measurements plus a stated margin, and no number is committed before that. LMCache is on
by default (`lmcache: auto`); a deployment's `lmcache: off` opts out, `lmcache: required`
refuses to start rather than run without it, and a host's `lmcache: off` turns it off for
every deployment on the machine and refuses `required` there. Zero data retention: a host
or deployment `retention: zero` keeps engine logs in memory, starts engine and LMCache
processes with no core dumps, and never starts LMCache; turning it on for a host stops
existing LMCache servers; the engine's own GPU prefix caches are not covered. The ADR
also gives the engine-option changes (SGLang's LMCache options owned, an LMCache
`--kv-transfer-config` refused from vLLM `extra_args`, `LMCACHE_*` reserved), the metrics
read from the server's `/metrics`, the refusal codes, conformance row T45, and live rows
LC1–LC11 (decisions, the relaunch without LMCache, a server exit, a host's `off` and
`required` refused on it among them). `AGENTS.md` lists the two host A qualification
environments the owner approved on 2026-10-21, `~/vllm-0.32.0-lmcache-venv` and
`~/sglang-0.5.22-lmcache-venv`. Nothing is implemented; the SPEC, guide and
data-retention changes the ADR lists land with the implementation. No code or tests
change; CPU and Fake-engine tests are not qualification, and the LC rows are the live
check still needed.

## ADR 0032 accepted: settling a cancelled request on evidence about that request — 2026-10-21 (branch `docs/adr-0032-settlement`)

[ADR 0032](../design/adr/0032-cancelled-request-settlement.md) records the owner's
decisions of 2026-10-21 on cancelled requests, which today stay counted until their
engine reads idle (SPEC §10, amended 2026-10-01) and under steady traffic can fill the 32
requests a deployment may hold. From the public source of SGLang 0.5.22, vLLM 0.31.0 and
0.32.0 and llama.cpp v0.7.0 (b11669), only SGLang gives proof about one request: its own
final output after an abort by `rid`.

The decisions: on SGLang, CapyCTL sends its own request ID (the `X-CapyCTL-Request-Id`
value, never the caller's) as `rid`, also through a host's ingress (amending the note of
2026-10-19). On a client hang-up, a deadline cut or a stream idle cut, it aborts that
`rid` on loopback (on a host through a new host-agent operation), keeps reading, and
settles on that request's final output within the stream idle bound, falling back to
today's rule. The answer's `id` stays CapyCTL's request ID. Other engines keep today's
rule, as amended separately on 2026-10-21: an idle reading settles only a request the
engine produced output for. When requests held until the next stop alone fill a
deployment's limit, CapyCTL restarts the instance, journals the restart, and settles them
on its verified exit, never on a timer.

The ADR gives the plan for 0.1.7 and four live rows on host A with SGLang 0.5.22.
Nothing is implemented; no code or tests change. CPU and Fake-engine tests are not
qualification, and those live rows are still needed.

## Summaries brought up to 0.1.6 — 2026-10-21 (branch `docs/after-0.1.6`)

Docs and website copy only. The README's list of what works in 0.1.6 gains
custom engines, request IDs and decision metrics, engine logs kept in memory,
the controller guide and running a role in a container, each linked to its
guide page; its documentation list links the user guide, the container section
and the 0.1.6 release notes. `docs/README.md` links the user guide and the
0.1.5 release notes, which it had skipped. The guide's overview, install,
how-it-works and engine options pages mention custom engines, and the install
page gains "In a container", which points to the operations guide's container
section and states what has not run in a container yet. The operations guide's
engine log section and the data retention page no longer say that no engine
logs prompts: they name llama.cpp, and say that a custom engine logs whatever
its manifest's command prints. On the website, the landing page's capabilities
table lists custom engines, containers, decision models and request IDs,
engine logs kept in memory and the controller guide, and the hero line and the
site description name custom engines. No code or tests change; CPU and
Fake-engine tests are not qualification, and no live check is needed.

## The exact decision readiness request — 2026-10-21 (branch `docs/decision-probe-text`)

Docs only. ADR 0030 §3 now publishes the exact v1 readiness request of a
decisions deployment (kind `decision`, version `1`), copied from
`crates/capyctl-adapters/src/decision_probe.rs`: the JSON body with `model`
as the engine's served name, the readiness rule (every option present and no
other, finite probabilities in [0, 1], sum within 1e-3, the pick one of the
options; a well-formed answer, not the correct choice), and the rule that any
change to the request is a new version. The deploy guide gains "The readiness
question" with the same JSON. No code or tests change; CPU and Fake-engine
tests are not qualification, and no live check is needed.

## Custom engines (ADR 0031) — 2026-10-21 (branch `feat/custom-engines`)

Implements [ADR 0031](../design/adr/0031-custom-engines.md) (owner decisions of
2026-10-20) with its amendment A1 (owner decision 2026-10-21), for 0.1.6. A
fifth engine kind, `custom`, runs a program CapyCTL has no integration for from a
host-approved manifest: `capyctl engine add --custom <executable> --name <profile>
--manifest <file>` executes nothing, validates the manifest (closed, at most 64
KiB; `api` `chat` or `decisions`; `args` with the placeholders `{host}`, `{port}`,
`{model_dir}` and optionally `{served_name}`, `{context_length}`, `{api_key}`;
optional `version`, `environment`, `env`, `health`, `idle`), prints what will run
and asks for confirmation on a terminal, or needs `--yes` (`not_interactive`
otherwise; a refusal at the prompt is `declined`, nothing written). The manifest
is copied into the profile with its digest, which is part of the recipe
fingerprint. The installation digest covers the executable and, for one in a
virtual environment, the whole environment with its interpreter, or the
manifest's `environment` root; drift warns unless `installation_drift: refuse`.
Resolution makes a custom engine a custom/fixed recipe: `resources` required,
`restart_only` (`capability_missing:deep_park`), no `topology`
(`group_shape_unsupported:custom`), `api` equal to the manifest's
(`capability_missing:decisions` / `capability_missing:chat`), every
`engine_config` field but `context_length` and `env` refused
`custom_field_unsupported:<field>`, and `{context_length}` without a declared one
refused `custom_placeholder_unset:context_length`. The launch renders each
placeholder into one argument without a shell, in a closed environment where the
manifest's `env` is the lowest layer; readiness is the port, the optional
`health` path, then the API's existing probe; after it the host lists the
launch's TCP listeners and stops a launch listening off loopback
(`custom_listener_exposed`). Without an idle signal, a request left cancelling by
a hang-up, a deadline cut or an engine error settles only on the instance's
verified exit at the next stop or restart, drains do not wait for it, and its
charge and the instance's reservation are kept until then; status shows each
instance's cancelling count and how it settles (`requests.cancelling`,
`requests.cancelling_settles`), and `engine list` gains `IDLE SIGNAL` and
`ENGINE KEY`. Placement needs the new host capability `custom_engines`
(`host_capability_missing:custom_engines`). New CLI exit 27,
`custom_manifest_invalid`.

Amendment A1: 0.1.5 reads `engines.yaml` strictly against a closed profile shape
and would refuse the whole file over a copied manifest, so custom engine profiles
live in `custom-engines.yaml` beside `engines.yaml` (same revision header, lock
and atomic write; document kind `custom_engines`). `engines.yaml` keeps 0.1.5's
shape and refuses `engine: custom`; a profile name is unique across both files
and the role document (`profile_exists`). A downgrade below 0.1.6 ignores
`custom-engines.yaml` and loses the custom engines; every other engine keeps
working. For a decisions deployment, ADR 0030's document check refuses a
`topology` or a parking residency before the profile is known, so those two
refusals carry ADR 0030's wording there; a chat deployment gets ADR 0031's codes.

Review fixes (PR #166 review, 784366a5): (1) the standalone role keyed its
installations by executable and `environment` only, so a built-in and a custom
profile on one executable shared one measurement and the second was
drift-checked against the first's; installations are now keyed by engine too
(and, for a custom engine, its root and pinned digest). (2) `engine add` and
`engine add --custom` locked their own file, so two concurrent adds of one name
could both write; both files now share one lock (`engines.yaml.lock`), taken
before either is loaded, and the size bound is checked on both under it. (3) A
remote host whose profile set no longer held a launch's approved document read
its idle signal from the current profiles, so after a remove and re-add a
running instance could be sampled with another manifest; the signal now travels
with the launch, and without it the launch's idleness is unknown (no load
figures, cancelling requests settle only at the verified stop, and the stop
gate waits its bound). (4) A remote host's ingress forwarding and idle reads
sent the engine key to a custom engine whose manifest does not use
`{api_key}`; they no longer do. (5) A hand-written custom profile without
`security.credential_ref` validated and then failed every start with a generic
conflict; resolution now refuses it `custom_credential_ref_missing`. (6) The
installation digest shown at the confirmation was not saved, so the role took
whatever it measured at its next start as the baseline; it is now saved as
`custom.installation_digest`, and a different measurement at registration is
drift, never adopted. (7) A deployment's served name such as
`--log-decisions=/tmp/x` reached the engine as an option when the manifest
placed `{served_name}` as an argument; a served name or model path that starts
with `-` or holds a control character is refused `custom_value_invalid:<name>`
at resolution and again at launch. Amendment A1 records each of them.

SPEC amendments landed with the implementation: §8.2 (a custom engine is a
custom/fixed recipe whose flags CapyCTL does not validate), §9.4 (custom engines
as a later backend), §10 (the settlement of `cancelling` requests on an engine
with no idle signal, and the `idle` signal), §15.1 (the second engines file), §20
(T44, live rows CE1–CE4) and §21. Guide: [Custom engines](../guide/engines.md#custom-engines),
with the basal-serve 2.2.0 example and its 422 caveat; settings:
[Custom engines](../operations/configuration.md#custom-engines); the new codes in
[Exit codes and errors](../guide/errors.md).

Tests: tagged T44 (and T13, T14, T17, T37, T43 where they apply), in
`crates/capyctl-config/src/custom.rs` and `crates/capyctl-config/tests/custom_engines.rs`
(manifest validation, placeholders, digests, profile files, resolution
refusals), `crates/capyctl-adapters/src/custom/` (rendering), the CLI
(`crates/capyctl-cli/tests/errors.rs`: codes and exits), the controller's
installation gate and version-skew capability tests, and the store's cancelling
settlement (`crates/capyctl-store/src/ordinary_lifecycle/park_cancelling_tests.rs`).
The review fixes add, by finding: (1)
`a_builtin_and_a_custom_profile_on_one_executable_are_two_installations` and
`embedded_installations_are_keyed_by_executable_and_engine`
(`installation_gate.rs`); (2)
`a_normal_and_a_custom_add_of_one_name_serialize_on_one_lock` and
`the_size_bound_counts_both_engines_files` (`tests/custom_engines.rs`); (3)
`a_republished_profile_never_supplies_a_running_launch_idle_signal`
(`native_execution.rs`) and an unknown launch in
`a_custom_launch_reads_quiescent_only_on_its_idle_signal` (agent `tests/load.rs`);
(4) `the_native_key_reaches_only_an_engine_that_takes_it` (agent
`tests/ingress.rs`, recording fake engines); (5)
`a_custom_profile_without_a_credential_reference_is_refused`; (6)
`a_profile_block_carries_its_pinned_installation_digest` (`custom.rs`),
`a_pinned_installation_digest_is_the_baseline_and_never_adopted`
(`installation.rs`), `a_pinned_installation_digest_is_the_embedded_baseline`,
`a_pinned_drift_is_journaled_once_at_publication` (`agent_sessions.rs`) and
the pinned digest in `tests/engine_custom.rs`; (7)
`a_value_that_reads_as_an_option_is_refused`,
`a_dash_leading_or_control_value_is_refused_at_render` and
`a_dash_leading_deployment_value_refuses_the_launch` (`custom/args.rs`). CPU and
Fake-engine tests are not qualification.

Live qualification, host A (GB10), basal 2.2.0 in `~/basal-2.2.0-venv` (torch
2.11.0 cu130, `basal[media]`) and basal-2.0-4.5B at revision e5ba914, through
`capyctl start standalone` (ports 38443/37443, isolated state and config):
CE1 passed on 2026-10-21: `engine add --custom` without a terminal and without
`--yes` was refused `not_interactive` (exit 23) with nothing written; a "n" at
the terminal prompt was `declined` (exit 2) with nothing written; `--yes` and a
"y" wrote `custom-engines.yaml` (revision 1, manifest and digest) and left
`engines.yaml` absent; the pinned root was the whole virtual environment with
its external interpreter (23,021 files, 4.87 GB); `engine list` showed `CUSTOM
yes`, `IDLE SIGNAL none`, `ENGINE KEY none`. The run found one listing bug (a
second custom profile on the same root showed the first one's version), fixed
here.

Drift launch passed: after a file under a scratch `environment` root changed,
the profile registered with `--drift refuse` was refused `installation_drift`
before any effect, and the `warn` one launched (basal `--mode eager`, ready in
about 60 s) with `installation_drift_flagged` journaled and the host's profile
shown `drifted`. CE2 passed: `basal-20` (`--mode fast --template-prefix off`,
`resources: {gpu: 24GiB, ram: 8GiB}`, `timeouts.initialize: 30m`) became ready
on the fixed decision in 703 s with a warm compile cache; the engine's only
listener was 127.0.0.1:38500; a decision through the router answered in 67 ms;
a manifest binding `--host 0.0.0.0` failed `custom_listener_exposed: an owned
process listens on 0.0.0.0:38500, not on loopback; the engine was stopped`,
with no listener and no process left. CE4 passed: after an operator stop
(verified exit, `ordinary_cleanup_completed`) `start deployment basal-20 --wait`
was ready again in 691 s through the decision probe, and its first decision gave
the same probabilities as before the restart (0.9638675 for `technical`).

CE3 is not demonstrated on basal. A client that hangs up on `POST /v1/systemone`
does not make the request `cancelling`: a collected answer is still completed
when the engine answers (the hung-up request's lease closed completed, status
showed `cancelling: 0` before and after; basal kept computing it, so the next
request waited 9.6 s). A deadline cut cannot be produced on basal either: a
deployment's `request_deadline` must be at least its `timeouts.initialize`, and
basal's start takes 11 to 14 minutes.

CE3 passed on 2026-10-21 on a chat custom engine: llama-server from the
approved llama.cpp v0.7.0 build, added with a manifest of `api: chat` and no
`idle` (`-m "{model_dir}/Qwen3-4B-Q4_K_M.gguf" --host "{host}" --port "{port}"
-c 4096 --alias "{served_name}"`), deployment `resources: {gpu: 8GiB, ram:
4GiB}` on the Qwen3-4B GGUF directory, ready in under 20 s. A streamed chat
request (`stream: true`, `max_tokens: 3000`) cut by the client after 3 s
(53 KB received) left `instances[].requests` at `{cancelling: "1",
cancelling_settles: "stop"}` (0 before), and the text status read `cancelling:
1 (settles at the next stop or restart: the engine reports no idle signal)`.
`capyctl stop deployment` returned in 0.05 s without waiting on it; 3 s later
the deployment was `stopped` with `cancelling: 0`, no llama-server left, and the
journal held `request_cancellation_acknowledged` (count 1, receipt: no idle
signal, the stop operation's cleanup step verified the exit of the process
group) beside that operation's `ordinary_cleanup_completed`.

CE3 passed again on 2026-10-21 with the review fixes (working tree on
784366a5, release build, host A, the same llama-server manifest and
deployment): `engine add --custom --yes` wrote `custom.installation_digest`
equal to the digest its summary showed, and the role registered the engine
`measured` at that digest; the deployment was ready in 7 s; a streamed
request cut by the client after 3 s (55 KB received) moved
`instances[].requests` from `{cancelling: "0", cancelling_settles: "stop"}` to
`{cancelling: "1", ...}`; `stop deployment` returned in 0.04 s; 3 s later the
deployment was `stopped` with `cancelling: "0"` and no llama-server left, and
the journal held `request_cancellation_acknowledged` (count 1, receipt: no
idle signal, the stop's cleanup step verified the exit of the process group)
beside that operation's `ordinary_cleanup_completed`. A deployment on it with
`routes: ["--log-file=/tmp/x"]` was refused
`custom_value_invalid:served_name` (exit 2). Fixes (3) and (4) change only a
remote host's sampling and forwarding; CE3 runs standalone, whose load
sampling and quiescence path they leave as they were.

Deep gate (`scripts/ci-local.sh --deep --only workspace`, host A, branch head
3a99062a): 3436 passed, 3 failed, 1 ignored over 244 test binaries. The three
failures are `a_role_warns_when_its_limits_exceed_the_memory_available`
(`admission_figures`), `standalone_memory_limits_follow_the_document_and_its_overrides`
and `standalone_parked_limit_follows_the_document_and_its_overrides`
(`switching_cli`); they fail the same way on `main` (169081b9) on the same
host, so they are host-specific, not regressions: their assertions depend on
the host's free memory, and host A had 10.0 GiB of its 32 GiB unified pool
available, less than its 16 GiB limit plus 6.4 GiB reserve
(`admission_figures.rs:165`, `switching_cli.rs:336` and `:422`).

With the review fixes, on the laptop: `scripts/ci-local.sh` passed every step
(fmt, name, clippy, unit, runtime, shellcheck, installer, changes), and
`scripts/ci-local.sh --deep --only workspace` gave 3445 passed, 7 failed, 1
ignored over 244 test binaries. The 7 are the standalone launches of
`engine_exit` (2) and `role_shutdown` (5), refused
`insufficient_device_memory` because another program held 14 of the laptop
GPU's 16 GiB (1.9 GiB available against 8.7 GiB needed). Both binaries passed
in full on host A in a git-initialised archive of the same tree (3 and 25
passed).

basal's `--mode fp8` needs torchao, which the approved environment does not
have; `--mode fast` (bf16, compiled) served a decision directly in 69 ms after
an 843 s cold start (683 s with a warm compile cache), so deployments on it
state `timeouts.initialize`.

## Containers run live on a GB10 — 2026-10-20 (branch `docs/container-qualification`)

Live qualification of a standalone role running as the main process of a
container on host A (GB10, unified memory, CUDA 13.0, Docker 29 with CDI),
built from `main` at `fdbca583`. Documentation only: no code changed.

The container: `docker run --init --device nvidia.com/gpu=all --cap-drop ALL
--security-opt no-new-privileges --user 1000:1000 --shm-size 16g`, the
container's own `/proc` and cgroup namespace, ports 38443 and 37443 published
to the host's loopback only, the `capyctl` binary, llama.cpp v0.7.0, SGLang
0.5.22, vLLM 0.31.0, CUDA, the `uv` interpreter directory and the models
bind-mounted read-only at their host paths, a fresh writable state directory,
managed limit 108 GiB. The image is `ubuntu:24.04` plus `python3`, `gcc`,
`g++` and a loader cache entry for the CUDA libraries.

Results:

- Julia-1 (llama.cpp, `api: decisions`, resources 1 GiB + 1 GiB, context
  8192, batch and ubatch 8192): cold start 3 s, decision HTTP 200, stop 2 s,
  `startup.measured` peak 1197170688 bytes.
- Qwen3-4B on SGLang 0.5.22 (deep park): cold start 148 s, chat collected
  (`323`) and streamed (16 data lines) HTTP 200, park 2 s, wake through a
  request 59 s, stop 6 s; `startup.measured` peak 22354280448 bytes,
  `parked.measured` 6970511360 bytes.
- Qwen3-4B on vLLM 0.31.0 (deep park, KV cache 4 GiB): cold start 152 s, chat
  collected and streamed (14 data lines) HTTP 200, park 2 s, wake through a
  request 9 s, stop 2 s; `startup.measured` peak 21130612736 bytes,
  `parked.measured` 3953291264 bytes.
- Per-process GPU memory (ADR 0007, ADR 0019): `nvidia-smi
  --query-compute-apps` inside the container reports the container's PIDs,
  equal to the container's `/proc` (host 3226640 → container 780 per
  `NSpid`, 14873 MiB in both readings; vLLM host 3228849 → 2066, 13081 MiB
  ready, 1139 MiB parked). The measured startup peaks and parked charges come
  from those readings, so attribution works without a translation and no fix
  was needed.
- Memory limits: without `--memory` the role reported `meminfo` (cgroup `/`,
  `memory.max` `max`); with `--memory 100g` it reported `cgroup_v2:/`,
  capacity 102400 MiB.
- Found and documented: llama.cpp and SGLang need the CUDA libraries in the
  image's loader cache (the version check runs with a cleared environment),
  vLLM and SGLang need `g++` for FlashInfer's first-start compile, and `uv`
  environments need the `uv` interpreter directory mounted.
- Nothing left running: `docker ps` empty, no engine processes.

CPU and Fake-engine tests are not qualification; this entry records a live
run. Still not run live in a container: a discrete GPU, Podman, and a host
role joined to a server.

## Group launch tests clean up their processes and directories — 2026-10-20 (branch `fix/group-launch-test-cleanup`)

Found on the development laptop on 2026-10-20: four `capyctl-group-launch-*`
fixture directories from 2026-10-09 in the home directory, and two fake SGLang
engine processes (`runtime/sglang_entry.py`) from them, running for 11 days.
They came from `crates/capyctl-agent/tests/group_launch.rs`. The fake engine's
leader watched its parent, but the children it forked watched only the fixture
directory. A test that is interrupted or killed runs no `Drop`, so its
directory stays, and children whose leader had already died ran on. On a
panic, the cleanup terminated only the launches the host's journal still
claimed.

The fix is in that test file only (SPEC §13.2: the cleanup is of the test's
own process groups, found by the fixture path on their command lines):

- **The fixture guard.** The host's fixture directory is now owned by a guard
  whose drop runs when a test passes, fails an assertion or panics. It kills
  every process whose command line names a path inside the fixture, each with
  its whole process group (never the test's own group), waits until none is
  left, then removes the directory, retrying while the host's threads may
  still write into it. It replaces the journal-based cleanup.
- **Children die with the test.** Every process of the fake engine's tree (the
  leader, the child it forks at once and its late children) watches the test
  process that spawned it, by pid and start time, and the fixture directory,
  and kills its process group once either is gone. `PR_SET_PDEATHSIG` fires
  when the spawning thread exits, and the agent starts engines from tokio's
  blocking pool (`journal.execute` under `spawn_blocking`), whose threads exit
  when idle, so it cannot be used for the engines; the new interruption test
  uses it, as the CLI tests' `Guarded` does, for the child test process it
  starts.
- The fixture stays under the home directory, because the owner-only checks of
  the runtime directory and the host's private state (SPEC §13.3) refuse a
  shared `/tmp` ancestor. The other tests that create temporary directories
  under the home directory remove them on drop and were not changed.

Tests (T12): `an_interrupted_test_leaves_no_engine_process_behind` runs the
test binary again as a test whose SGLang worker's leader died and left its
children, kills it with SIGKILL, and requires every process of the tree to end
by itself. With the old fake engine it failed (three processes of the killed
test's tree still running after 10 s); it passes now.
`a_failed_test_leaves_no_engine_process_or_directory_behind` panics while it
owns such a host and requires no fixture process and no fixture directory
afterwards; it passed before as well and guards the new cleanup. Checked by
hand with the whole `group_launch` binary (four test threads) interrupted after
12 s with SIGINT, SIGTERM and SIGKILL: each time 8 fixture processes were
running, none 3 s after the signal. The four fixture directories of an
interrupted run remain, since no `Drop` runs on a signal death, but nothing
runs in them.

CPU and Fake-engine tests are not qualification. No live check is needed: the
change is to a test file only.

## Guide: driving CapyCTL from a controller — 2026-10-20 (branch `docs/controller-guide`)

Documentation only; no code changes. Owner decision of 2026-10-20: an outside
controller is one more user of the existing management API (SPEC §14: the API
is the source of semantics for the CLI and integrations), with no special
mode, and requests still start and wake models unless
`lifecycle.activation: explicit` is set (SPEC §6.5, §10). Idempotency keys and
revision preconditions are SPEC §6.4's. The new guide page,
`docs/guide/controller.md`, describes that use: the admin token and the
loopback base path; the loop of reading `GET /management/v1/snapshot` (what
each field a controller needs means, and how an instance's `observed_state`
maps to what is running), creating with `POST /management/v1/deployments`
(`activate: false`, then a start; `activate: true` is answered `503
unsupported_capability`), replacing with `PUT /management/v1/deployments/{id}`
and `expected_revision`, and acting with
`POST /management/v1/deployments/{id}/actions` (`start`, `stop`, `park`,
`preinitialize`, `delete`, with `deadline_ms` an absolute Unix time taken from
the snapshot's `timeouts`); following `GET /management/v1/events` from the
snapshot's cursor; `Idempotency-Key` retries (same key and body replays the
first answer, a different body is `409 idempotency_conflict`, a refused change
does not use up its key); `409 revision_conflict` on a stale revision; request
IDs, which belong to the inference endpoint while the management API
correlates by key and `operation_id`; which codes to retry, fix or stop on, by
`retryable`; `lifecycle.activation: explicit`, `max_pending_per_deployment: 0`
and `--no-inference-auth` for a controller that decides everything; and a
worked `curl` and `jq` script. The page is linked from the guide index,
`docs/README.md` and the site's Tasks sidebar (`site/scripts/pages.mjs`,
`site/astro.config.mjs`).

Every request and answer shown was taken from the handlers in
`crates/capyctl-management/src` and checked against a local
`capyctl start standalone` built from `main` (`fdbca58`) with the site's
stand-in engine (`site/scripts/transcript/fake-vllm.py`), which loads nothing:
create, replay and conflicting replay, `activate: true`, an unpinned `hf:`
reference and a `~/` path, `route_conflict`, stale and current revisions,
start (with `checkpoint_digest_pending` and its retry), stop, park,
preinitialize, delete before and after the stop, `runtime_retained`,
deadlines out of range, a string revision, the event stream and its cursor,
`deployment_stopped` and `deployment_inactive` on the inference endpoint, and
the worked script end to end, whose output the page shows. CPU and Fake-engine
runs are not qualification; no native engine was run for this page, and none
is needed for a documentation change.

## Request IDs and decision metrics — 2026-10-20 (branch `feat/request-ids`)

Owner decisions of 2026-10-19, recorded as two SPEC §10 notes and an amendment
entry. A request to `/v1/chat/completions` or `/v1/systemone` may carry the
caller's own `X-Request-Id`: 1 to 128 bytes of printable ASCII with no space or
control character, refused `400 invalid_request` naming the header after
authentication and before the body is read. A well-formed one is echoed on the
answer, success or error, a stream's response headers included. CapyCTL consumes
it: the router never sends it to a host's ingress or an engine (the ingress
rebuilds every upstream header, which a test now pins for both request IDs), it
goes only into the router's role logs for the request, and it is never written
to the ledger or an event. Every answer of both endpoints carries
`X-CapyCTL-Request-Id`, the ID of the request's first granted lease, or a fresh
ULID when the request was refused before any lease. The router's
`router_selection` and `router_failover` lines carry `capyctl_request_id` and,
when given, `caller_request_id` in JSON output; in the default text output
(ADR 0021) they read `request <capyctl id> (caller <id>) <deployment> -> <host>
instance <n>`, the caller part only when given (found by the live check on host
A: the text form first showed neither ID). Lines decided before the first lease
are held until the offer is decided, so they carry the lease's ID. A lease close
that could not be written names both IDs. `request_cancellation_acknowledged` is
unchanged and keeps only CapyCTL's identifiers. A decision's answer now carries
`capyctl.metrics`: the router's `total_ms` and `time_to_response_ms`, the
engine's `input_tokens` from `usage`, and `queue_ms`, `prefill_ms` and
`cached_tokens` when the engine reports them, each labelled with its source;
absent values stay absent.

Tests: `crates/capyctl-router/tests/request_ids.rs` (T17 T19 T21 T43: echo on
collected, streamed and decision answers and on errors after a lease; the CapyCTL
ID equals the lease ID; a fresh ULID per request refused before any lease; a
malformed or repeated header refused before anything is leased or sent; an engine
double that records headers never sees either ID, and its own `X-Request-Id`
never reaches the client), the `request_id` and `engine_metrics` unit tests,
the `role_text` unit test `router_lines_name_the_request_ids` (the text form
names both IDs, and nothing extra without a caller ID),
`decisions.rs` (decision metrics present, no time to first token or decode
speed), `crates/capyctl-cli/tests/router_payloads.rs` (router, engine forwarder,
host ingress and engine: both IDs on the answer, neither at the engine) and
`crates/capyctl-agent/tests/ingress.rs` (the ingress forwards neither ID). Before
the change the headers were absent and the decision test asserted no
`capyctl.metrics`. CPU and Fake-engine tests are not qualification.

Live on host A, 2026-10-20, with this branch (`5de86c59`) merged with
`feat/zero-retention` and `fix/sglang-flag-reservations` on main (integration
build `daffad0d`), an isolated standalone, Julia-1 Q8_0 as a decision
deployment on llama.cpp v0.7.0 and Qwen3-4B chat on SGLang 0.5.22, each request
sent with `X-Request-Id: live-check-1`: the decision, a collected chat and a
streamed chat (`text/event-stream`, 6 data lines) answered 200 with
`x-request-id: live-check-1` and an `x-capyctl-request-id` ULID, a different one
per request; a request without the header got only `x-capyctl-request-id`. The
decision's `capyctl.metrics` held `input_tokens` 33 (engine), `time_to_response_ms`
30.2 and `total_ms` 31.4 (router); llama.cpp reported no `queue_ms`,
`prefill_ms` or `cached_tokens`, and they were absent. `X-Request-Id: live check`
was answered `400 invalid_request` ("X-Request-Id must be one value of 1 to 128
printable ASCII characters with no spaces") with an `x-capyctl-request-id` and
no echo. Each answered request had one `router_selection` line naming its
`X-CapyCTL-Request-Id`: with `--format json` as `capyctl_request_id` and
`caller_request_id: "live-check-1"`, and in text as `request
01M5C29SXMR5FQ3133Q2RE59QG (caller live-check-1) 01M5C26QQBW5 -> <host A>
instance 0` (the caller part absent for the request without the header). Neither
engine log contained `live-check-1` or `capyctl-request-id`. The standalone's
embedded host was used, so the path through a separate host's ingress was not
run live; `ingress.rs` and `router_payloads.rs` cover it.

## Engine logs in memory; vLLM request logging off; data retention docs — 2026-10-20 (branch `feat/zero-retention`)

Owner decisions of 2026-10-19 and 2026-10-20, under SPEC §13.3 (engine logs are
private, redacted and served only as a bounded tail):

- **`engine_logs: files | memory`.** A new role-local host setting (standalone:
  `host.engine_logs`; also `--set` and `CAPYCTL_SET__…`), `files` by default,
  which is the existing behaviour. With `memory`, the redacting log writer keeps
  the last 4 MiB of each launch's redacted output in its own memory and writes
  no file. It answers reads on a Linux abstract Unix socket named from a digest
  of the launch's log path, and both ends refuse a peer that is not the same
  user. It keeps answering for 60 seconds after the engine exits, then leaves
  with the buffer. Every reader of a launch's output goes through
  `capyctl_adapters::engine_log::tail_bytes`, which falls back to the buffer
  when no file exists. That covers the management engine-log read (the host's
  `EngineLogTail` and the standalone tail), the launch-failure reason quoted
  by vLLM, llama.cpp, TensorFold and group workers, and SGLang's own tail,
  which used to read the file directly and now uses the shared reader. A role
  started with `--debug-engine-logs` and `engine_logs: memory` refuses to
  start with a named reason; the flag is otherwise unchanged.
- **vLLM request logging off.** Every launch renders `--no-enable-log-requests
  --no-enable-log-outputs`. vLLM 0.29.0, 0.30.0 and 0.31.0 define both as
  `BooleanOptionalAction` switches, checked against the source of each tag, so
  no version gate or entry pin is needed. `--enable-log-outputs` is now
  reserved beside `--enable-log-requests` and `--disable-log-requests`, in
  `VLLM_RESERVED_FLAGS` and in the protected entry's `RESERVED`.
- **Data retention.** A new page, `docs/operations/data-retention.md`, linked
  from the guide and docs indexes and the site, lists what CapyCTL keeps per
  request and for how long, and how to turn each part off where that is
  possible. The settings reference documents `engine_logs`.

Tests (T20, T21), each failing before this change and passing after:
`memory_mode_writes_no_file_and_serves_the_tail` and
`a_startup_failure_is_classified_from_memory` (capyctl-launchers, a real child
through the exec launcher with the test binary as the writer),
`the_memory_buffer_is_bounded_to_whole_lines` and
`a_log_with_neither_file_nor_buffer_is_missing` (capyctl-adapters),
`engine_logs_default_to_files_and_stay_role_local` and
`standalone_honours_engine_logs` (capyctl-config),
`debug_engine_logs_are_refused_with_memory_engine_logs` (the `capyctl` binary),
`vllm_0_29_renders_request_and_output_logging_off`, `vllm_0_30_…`, `vllm_0_31_…`,
`request_and_output_logging_cannot_be_enabled`, `request_logging_off_on_every_rank`
and the Python `test_request_and_output_logging_are_off_on_every_supported_version`.

CPU and Fake-engine tests are not qualification.

Live on host A, 2026-10-20, with this branch merged with `feat/request-ids`
and `fix/sglang-flag-reservations` on main (integration build `23f72fd8`), an
isolated standalone with `--set host.engine_logs=memory` and Qwen3-4B on
SGLang 0.5.22, vLLM 0.31.0 and vLLM 0.30.0 (each ready in 90 to 110 s and
answering chat, collected and streamed): the state log directory held one
empty directory per deployment and no file, before and after a failed launch
and a role restart. The management engine-log read (by deployment ID, admin
token) answered 200 for all three, 16 and 256 KiB tails, `truncated` set when
older output existed. vLLM 0.31.0 and 0.30.0 were running with
`--no-enable-log-requests --no-enable-log-outputs` in their arguments and
accepted them; after two collected and one streamed chat request carrying a
marker word, no tail held the marker or a request or output line, only the
access lines (`POST /v1/chat/completions` with status). A vLLM 0.31.0
deployment with `accept_extra_args` and `extra_args: [--bogus-flag-016a, "1"]`
failed before readiness; with no log file to read, `status deployment` showed
`initialize failed (launch_failed)` and the instance's last error `the engine
exited before readiness; it rejected argument --bogus-flag-016a`. Stopping the
standalone left the six engine processes running; after it started again all
three deployments were ready and the engine-log read answered 200 with the
same launch incarnations. `start standalone --debug-engine-logs --set
host.engine_logs=memory` exited 2 before starting (`invalid_config`:
"--debug-engine-logs writes full engine output to files, which `engine_logs:
memory` forbids"). vLLM 0.29 is installed only on host B and was not run.

## SGLang dump options and DUMPER_ reserved; engine flags table — 2026-10-20 (branch `fix/sglang-flag-reservations`)

Owner decision 2026-10-20 (SPEC §8.2, T21; ADR 0014 §3, §8; ADR 0028 §2.1;
ADR 0021). SGLang options and environment variables that write requests,
tensors or debug data to disk are now reserved: refused in every spelling,
whatever the host approves, where before the path-shaped ones only needed a
named approval. The list was read from the installed SGLang 0.5.21 and 0.5.22
parsers (`ServerArgs.add_cli_args`) on host A and from the code that reads
each field or variable:

- options (`SGLANG_DUMP_RESERVED` in `engine_policy.rs`, `_RESERVED_DUMP` in
  `runtime/sglang_server_args.py`): `--watermark-config` (0.5.22; the
  watermark key file or JSON), `--debug-tensor-dump-output-folder`,
  `--debug-tensor-dump-layers`, `--debug-tensor-dump-input-file` and the whole
  `--debug-tensor-dump-*` family, `--msprobe-dump-config`,
  `--export-metrics-to-file` and `--export-metrics-to-file-dir` (a record of
  every request, hourly files), and `--spec-trace-dir`. `--log-requests`,
  `--log-requests-target` and `--crash-dump-folder` were already CapyCTL's.
- environment (`engine_env.rs`, refused as `engine_env_reserved:<name>`,
  compared upper-cased at both levels, and refused as an approval glob): the
  `DUMPER_`, `SGLANG_DUMP`, `SGLANG_KERNEL_API_` and `SGLANG_CUDA_COREDUMP`
  families, and `SGLANG_DSPARK_DEBUG_DUMP`, `SGLANG_DSPARK_ENABLE_SPS_RECORD`,
  `SGLANG_EXPERT_DISTRIBUTION_RECORDER_DIR` and `SGLANG_TORCH_PROFILER_DIR`.

New command `capyctl engine flags <family> [--version V]` prints every option
and environment name the checks know for a family with its class (owned,
reserved, sensitive with the approval kind, or ordinary) and the reason; text
by default, `--format json` on request (ADR 0021). It is built from the same
tables the checks read (`engine_policy::flag_table`, `engine_env::env_table`).
Documented in `docs/guide/engine-flags.md`.

Tests (T21): `sglang_dump_destinations_are_reserved_in_every_spelling`
(exact, abbreviated, `_` and `=value` spellings and the family, with every
option approved; failing before), `dump_environment_names_are_reserved`
(`DUMPER_*` and the SGLang names in any case, both levels, approval globs;
failing before), `the_environment_table_matches_the_check`,
`engine_flag_ownership.rs` (every table row run through
`validate_extra_args` agrees with its class; every reserved and typed name is
listed; the JSON form carries the classes), the runtime parity test and
`test_dump_destinations_are_refused_however_spelled` in
`runtime/tests/test_sglang_server_args.py`, and `engine_flags_parses`. CPU
and Fake-engine tests are not qualification.

Live on host A, 2026-10-20, with this branch merged with `feat/request-ids`
and `feat/zero-retention` on main (integration build `23f72fd8`), an isolated
standalone and Qwen3-4B on SGLang 0.5.22: `deploy model` refused all four
before anything was stored or launched, exit 2. `extra_args: [--watermark-config,
x]` and `[--export-metrics-to-file]` gave `invalid_config` ("reserved option
... is rendered by capyctl and cannot be passed"); `engine_config.env`
`DUMPER_ENABLE: "1"` gave `engine_env_reserved:DUMPER_ENABLE` and
`dumper_enable: "1"` gave `engine_env_reserved:dumper_enable`. `engine add
--approve-env 'DUMPER_*'` was refused with `engine_env_reserved:DUMPER_`. No
engine process started for any of them. `capyctl engine flags sglang` printed
the table (text, and `--format json` with `class`, `match`, `approval` and
`reason` per row), listing `--watermark-config`, `--export-metrics-to-file`,
`--export-metrics-to-file-dir` and `DUMPER_*` as reserved. An ordinary SGLang
0.5.22 deployment of the same model then started (ready in 106 s) and answered
a chat request.

## three accounting bugs from the formal review, fixed on main — 2026-10-20 (branch `fix/lean-review-bugs`)

The Lean review in closed PR #16 (tag `archive/formal-review-lean`) found three
bugs that never reached `main`. Each was re-checked against `main` at
`af661ac2` and was still live; each new test failed there and passes now. The
Lean models are not ported.

- **A cancelled or refused park reopened a gate a restart had closed**
  (SPEC §6.1, §10). A restart closes every dispatch gate without a closure row;
  an adopted launch reopens only through `reverify_local_dispatch`, on fresh
  readiness evidence with no lease left by the retired session. A park
  accepted on the adopted launch and then cancelled at its deadline, or
  refused, called `reopen()` and opened dispatch over the retired session's
  lease. The residency plan now records `reopens_dispatch`: true when the park
  found the gate open, or when a switch releasing its drained victim handed it
  the closure it recorded on this session's open gate. A park adopted from a
  retired session has it cleared. Plans stored without the field never reopen;
  readiness supervision reopens those gates on evidence. Test (T33 T19):
  `a_cancelled_park_does_not_reopen_a_gate_a_restart_closed`.
- **A Ready phase could claim a device its peak phase lacked** (SPEC §7.3).
  Start and wake completions write the Ready footprint without admission, and
  `validate_recipe` compared bytes between phases but never device claims. A
  declared `resources:` block with no device in `cold` and `gpu0` exclusive in
  `ready` resolved, and completion then put a second exclusive owner on the
  GPU. A revision written now (deploy, replace, `capyctl validate`) must have
  every base-phase claim held by its peak phase on the same device, equally or
  exclusively (`validate_device_coverage`). The rule is not applied to a
  revision accepted before it (`CheckpointFacts::accepted_revision`, set when
  a stored snapshot is decoded or re-resolved and when a host resolves the
  revision its server accepted). Such a revision still decodes, starts, stops
  and can be replaced, because more than twenty store paths decode stored
  revisions, placement and startup among them. Tests (T26 T27):
  `recipe_peaks_cover_each_adjacent_phase_device_claims` (scheduler),
  `a_ready_phase_may_not_claim_a_device_its_cold_phase_lacks` (config) and
  `a_revision_stored_before_device_coverage_still_decodes_stops_and_is_replaced`
  (store). The store test failed while the rule also applied to stored
  revisions.
- **A granted request lease leaked when its caller was dropped after the reply
  was sent** (SPEC §10). The writer closed a grant as not accepted only when
  `reply.send` failed. The first lease of a request is opened in the handler
  future, before the dispatch task is spawned (#152 moved only the close), so
  a client disconnect between the send and the caller's next poll dropped the
  ticket inside the channel and left the lease `inflight` for the session. The
  ticket now travels in a guard whose `Drop` queues its `Finish`; the idle
  writer looks for such orphans every 50 ms and flushes them before it stops.
  Test (T17 T19): `a_grant_sent_but_never_taken_is_closed_as_not_accepted`.

CPU and Fake-engine tests are not qualification. No live check is needed for
the recipe rule; the park and lease paths have no live repro yet.

## ADR 0031 accepted: custom engines — 2026-10-20 (branch `docs/adr-0031-command-engines`)

ADR 0031 records the owner's decisions of 2026-10-20 (proposed 2026-10-18) for custom
engines in 0.1.6: a generic engine kind, `custom`, for one-off engines CapyCTL has no
integration for. It is not a custom build of a known engine, and not ADR 0008's runtime
type `command`. A host administrator registers one with `capyctl engine add --custom <exe>
--name <n> --manifest <file>`, confirmed on a terminal or with `--yes`. The manifest is the
whole argument template (placeholders `{host}`, `{port}`, `{model_dir}`, optional
`{served_name}`, `{context_length}`, `{api_key}`), its API (`chat` or `decisions`), an
optional idle signal and an optional version string. The digest covers the executable and
its whole environment by default (site-packages and interpreter), or a narrower root the
manifest names; drift warns unless `installation_drift: refuse`. CapyCTL keeps process
ownership, the loopback port, routing, authentication, admission, readiness through the
existing chat and decision probes, measured peaks and logs. It refuses parking, groups,
derived sizing (`resources` is required), extra arguments, typed engine fields and
parsers. After readiness, a loopback-listener check stops a launch whose processes listen
off loopback (`custom_listener_exposed`). Without an idle signal, a lease left cancelling
by a hang-up, a deadline cut or an engine error settles only at the instance's next
verified stop or restart, drains do not wait for it, its memory is not freed before that
exit, and status says so (a SPEC §10 amendment). The host capability is `custom_engines`;
the acceptance row is T44. `/v1/systemone` stays closed, and basal's extensions need
their own ADR. The worked example is `basal-serve` 2.2.0 `--mode eager` for basal-omni
decision models: no API key, no in-flight counts, and a 422 for every failure, which may
mean an engine failure (upstream issue rkinas/basal#22). Live rows CE1–CE4 will run on
host A with `~/basal-2.2.0-venv` (owner approved 2026-10-20, added to AGENTS.md). Nothing
is implemented; the SPEC amendments it lists land with the implementation.

## SGLang 0.5.22 finds `cutlass` without running its `.pth` hook — 2026-10-19 (branch `fix/sglang-cutlass-path`)

Found live on host A on 2026-10-19: SGLang 0.5.22 under CapyCTL failed to
start Qwen3-4B-Instruct (dense, BF16). Its scheduler raised
`ModuleNotFoundError: No module named 'cutlass'` from
`sglang/srt/layers/dual_gemm.py::_select_mode`, which imports
`sglang.kernels.ops.gemm.cutedsl_dual_gemm` and with it `cutlass`. The same
installation launched directly worked. `cutlass` belongs to
nvidia-cutlass-dsl 4.8.0 (SGLang 0.5.22 requires it), which installs it under
`<site-packages>/nvidia_cutlass_dsl/dsl_packages/` and adds that directory to
the search path only through the code hook `nvidia_cutlass_dsl_packages.pth`.
CapyCTL starts SGLang under `python -IS` and composes the search path itself
without running `.pth` hooks (SPEC §9.2), so the directory was never added.

ADR 0008 amendment 2026-10-19: the composition
(`runtime/sglang_startup_guards.py::trusted_search_path`) appends
`nvidia_cutlass_dsl/dsl_packages` after the launcher-selected site-packages
when both components are real directories inside it (neither a symlink). No
`.pth` file is read or run, nothing is imported to compute it, and the plugin
closure enumerates it like any other entry. The entry
(`runtime/sglang_entry.py::_compose_search_path`, before the plugin closure
and again at the guarded import) and the SGLang capability probe
(`runtime/engine_capabilities.py`) use it. SGLang's scheduler and detokenizer
are spawned; CPython spawn hands each child the parent's `sys.path` before it
runs the protected main script, so they import from the same composition.
No other `.pth` path is honoured.

Tests (`runtime/tests/test_sglang_startup_guards.py`,
`CutlassDslSearchPathTests`, T21): a fresh venv laid out as
nvidia-cutlass-dsl installs, with the vendor hook plus a marker write. Under
`-I` alone the hook runs (positive control); under `-IS` the entry's
composition appends exactly site-packages and `dsl_packages`, `import cutlass`
loads the package from `dsl_packages` in the entry's process and in a real
spawn child with the entry as main (both under `-IS`), a stdlib module of the
same name in `dsl_packages` does not shadow the stdlib, and the marker is never
written. A `dsl_packages` symlinked outside the tree is not added and
`import cutlass` fails, as it did for every installation before this change;
a symlinked `nvidia_cutlass_dsl` or a `dsl_packages` that is a file is not
added either. CPU and Fake-engine tests are not qualification.

Found and checked live on host A, 2026-10-19: before this change, Qwen3-4B-Instruct (BF16)
on SGLang 0.5.22 through CapyCTL `main` (`07ee2ea6`) failed to start (scheduler
`ModuleNotFoundError: No module named 'cutlass'` from `sglang/srt/layers/dual_gemm.py`),
while the same venv launched directly served it. With this branch (`e63dd670`) the
deployment initialized and served 4 concurrent streams; when their clients hung up, SGLang's
`num_running_reqs` went from 4 to 0 within 0.15 s (0.13 to 0.14 s for SGLang launched
directly), and CapyCTL recorded the cancellation as acknowledged once the engine was idle.

## The eager loader's startup placeholder is capped at 32 GiB over the weights — 2026-10-18 (branch `fix/eager-placeholder-cap`)

Owner decision 2026-10-18 (ADR 0014 amendment A22, option b of the question left open on
2026-10-16 about the 3.6 × weights placeholder). An unmeasured vLLM deployment that renders
`--safetensors-load-strategy eager` (every parking deployment by default) had the startup
placeholder `max(request + graphs, weights × 3.6 + margin)` (amendment A21). It is now
`max(request + graphs, weights + min(weights × 13/5, 32 GiB) + margin)`
(`VLLM_EAGER_STARTUP_EXTRA_CAP_BYTES`), which is `min(weights × 3.6, weights + 32 GiB)` to
the byte: a checkpoint up to 13215283988 bytes (about 12.3 GiB) keeps exactly the old figure,
a larger one is capped. The non-eager factor (2.25) is unchanged; a measured peak still
replaces the placeholder.

Evidence (CapyCTL's `startup.measured`, vLLM 0.30.0, `eager`, GB10 host A): the peak over the
weights was 12.4 to 30.2 GiB, KV cache included, from 22 to 71 GiB of weights. Placeholder
before and after, against the measured peak:

| Checkpoint | Before (A21) | After (A22) | Measured peak |
|---|---|---|---|
| Qwen3.8-27B NVFP4, KV 4 GiB | 94087532268 (87.6 GiB) | 66699005648 (62.1 GiB) | 56220348416 (52.4 GiB) |
| Qwen3-14B BF16, KV 4 GiB | 114921930896 (107.0 GiB) | 72486338600 (67.5 GiB) | 44531412992 (41.5 GiB) |
| Qwen3-30B-A3B BF16, KV 4 GiB | 228429606924 (212.7 GiB) | 104016248608 (96.9 GiB) | 76076666880 (70.9 GiB) |
| Qwen3.8-27B BF16, KV 4 GiB | 208616758985 (194.3 GiB) | 98512679736 (91.7 GiB) | 68840255488 (64.1 GiB) |
| Nemotron 3.5 Super VL 120B-A12B NVFP4 | 283371241601 (263.9 GiB) | 119277813796 (111.1 GiB) | 101695578112 (94.7 GiB) |
| Qwen3.6-35B-A3B NVFP4 (A21's run, 23.5 GB) | about 86.8 GiB | about 61.9 GiB | 54.2 GiB |

Every row stays covered. The derived cold phase adds the 1.25 GiB CUDA context: Super VL's
is 120619991076 bytes (112.3 GiB, from 284713418881), still above a 108 GiB managed limit, so
its unmeasured first start still runs alone on the machine and reserves the whole managed
limit.

The cap is recorded in the revision as `startup_eager_cap_bytes`, beside
`startup_graphs_bytes`, and re-derived when a snapshot decodes; the store's solo-first-start
recomputation uses it. A revision frozen under amendment A21 records none and keeps
`weights × 3.6` exactly as it was frozen, as amendment A8 let earlier revisions keep 1.6: a
frozen revision must re-resolve to its own bytes. It holds the larger figure only until its
first start is measured; a new revision takes the cap.

Tests (`capyctl-config` `startup_budget`):
`the_eager_placeholder_keeps_3_6_below_the_cap_and_caps_above_it` (the last byte below the
cap and the first above, a 10 GiB checkpoint unchanged),
`the_capped_eager_placeholder_covers_every_measured_peak` (each row above),
`a_revision_frozen_before_the_eager_cap_keeps_its_placeholder` (a frozen A21 revision decodes
with 116 GiB, a new one gets 70 GiB, a snapshot that drops its cap is refused), and
`the_startup_placeholder_follows_the_effective_loader` (the recorded cap, a forged one refused)
and `the_placeholder_startup_covers_the_first_starts_graphs` updated from 116 to 70 GiB for
30 GiB of weights. `capyctl-controller`
`status_shows_the_whole_host_a_start_above_the_managed_limit_reserves` now pins Super VL's
120619991076 and still expects `whole_host` at 108 GiB. CPU and Fake-engine tests are not
qualification. Live check still needed: a first start of an unmeasured large eager checkpoint
whose capped placeholder fits beside other work, to confirm its peak stays under it; the
measurements cover vLLM 0.30.0 on GB10 only.

## Request ledger commits off the answer's path — 2026-10-18 (branch `perf/ledger-commits`)

Owner decision 2026-10-18 (SPEC §10 note of that date), following the
router-overhead measurements: two synchronous SQLite commits per request
(lease grant, lease close) were most of what CapyCTL added to a decision
request. One commit on host A's NVMe took 4.6 ms p50 and 6.8 ms p95 with
`synchronous=FULL`, and 0.01 ms with `NORMAL`. The owner chose three
changes:

- **Answer before the lease close** (`capyctl-router`). A completed
  non-streamed answer (chat or `POST /v1/systemone`) is handed to the client
  before its lease close is committed; the dispatch task then closes the lease
  on the same completion evidence and only after that releases the request's
  in-flight slot. A completed stream sends its timing and metrics comments and
  `[DONE]` while the close commits beside them, so a stalled client does not
  hold the slot. Refusals, engine errors, uncertain ends and failover are
  unchanged: their leases are settled before the answer, as before. Cost: a
  client's next request can briefly find the previous one still counted
  against the deployment's bound, and a crash between answer and commit
  leaves the lease charged for the quiescence path.
- **`synchronous=NORMAL` for the standalone request ledger**
  (`capyctl-store`, `capyctl-controller`, `capyctl-cli`). The store states
  `synchronous=FULL` when it opens. A standalone role opens its owned state
  `with_local_engines_only()`, and its request-lease batches alone commit
  with `NORMAL`; the connection is back to `FULL` before any other write uses
  it, whether the batch commits or is refused. In WAL mode the commit order
  holds, so the next `FULL` commit makes earlier batches durable too. A
  process crash loses no lease write; an OS crash or power loss that loses the
  last ones also ends the engines on that machine whose work they account
  for. The control-plane server, whose engines run on other hosts, keeps
  `FULL` for every write. Test: `a_local_lease_batch_leaves_other_writes_fully_synchronous`.
- **History indexes** (store schema v50): `lifecycle_steps(binding_id)` and
  `journal_entries(operation_id)`. The router's serving-instance read now
  searches both instead of scanning them (checked with `EXPLAIN QUERY PLAN`);
  results are unchanged. Without them the read cost about 1.2 ms per 1000
  past activations. v49 (ADR 0030) already makes this release's store
  unreadable by 0.1.4, so v50 adds no new downgrade limit.

Measured on the laptop reproduction (i9-14900HX, ext4 on NVMe, release
build, standalone role on the Fake installation with an HTTP stub engine,
decision requests one at a time), three alternating rounds of 400 requests
per case: the router's added p50 went from 4.2–4.8 ms (PR #150 alone) to
1.8–2.0 ms. Over 100 requests per case the process made 78 `fsync` calls
instead of 1117.

The ledger durability is recorded as an amendment to ADR 0002 and in the
SPEC's amendment list and T17 row. Tests (CPU, tagged T17 or T33):

- `the_standalone_ledger_commits_for_local_engines_only` and
  `the_server_ledger_commits_fully_synchronously` (`capyctl-cli`): each
  role opens its store through one function (`standalone_owned_state`,
  `server_owned_state`), and only the standalone one relaxes the ledger.
- `a_completed_answer_reaches_the_client_before_its_lease_close`
  (`capyctl-router`): with the close held back, a collected answer and a
  stream through `[DONE]` both reach the client while the lease is still
  open and the request still holds its slot; the close then ends the lease
  completed and frees the slot.
- `a_client_that_leaves_after_its_answer_still_has_its_lease_closed`: over
  a real connection, a client that hangs up right after its whole answer,
  collected or streamed, does not stop the close.
- `a_crash_between_answer_and_close_leaves_the_lease_charged` (router: the
  runtime dies with the close pending, the lease is never closed) and
  `a_local_grant_survives_a_crash_before_its_close` (store: a grant
  committed with `NORMAL` on a connection abandoned without closing is read
  back by the next session as an uncertain, charged lease).
- `v50_indexes_the_history_the_instance_read_searches`: a v49 store gains
  both indexes, keeps its rows, and the lookups by binding and operation
  use them.

`scripts/ci-local.sh --deep` passes. CPU and Fake-engine tests are not
qualification. Live check still needed: repeat the host A decision-request
comparison (Julia-1 and Clef-Flash, one and three questions, about 200 and
2k tokens of state) with this build.

## Router per-request overhead trimmed; ledger unchanged — 2026-10-18 (branch `perf/router-overhead`)

Measured live on host A on 2026-10-18 (llama.cpp v0.7.0, decision requests
one at a time): CapyCTL's router added about 12–26 ms per `POST /v1/systemone`
over the same engine addressed directly. Reproduced on a laptop
(i9-14900HX, ext4 on NVMe, release build) with a standalone role on the Fake
installation and an HTTP stub engine at the launch's recorded endpoint that
answers at once, through the production listener, admission gate, router,
request ledger and engine forwarder; the router added 5.8–6.0 ms (p50).
Per-phase timings on that reproduction (p50, before):

| phase | ms |
|---|---|
| request lease grant: one group commit (fsync ≈ 1.1 ms) plus the writer thread round trip | 1.66 |
| request lease close, before the answer is returned: one commit | 1.42 |
| store reads: route, `serving_instances` read twice, forwarder endpoint (engine key, routes, ingress, engine) | ≈ 2.1 |
| forward to the engine over the pooled keep-alive connection | ≈ 0 over direct |
| body read and parse, strict answer parse, spawn and join, timing, selection log line | < 0.1 |

Connection reuse to the engine works (the forwarder is cached per
incarnation, reqwest pools the connection; llama.cpp's HTTP server sets
`TCP_NODELAY`, and a keep-alive connection to a local llama-server was
measured faster than a fresh one). The router answers in one `writev`, so
Nagle's algorithm costs nothing on loopback today.

What changed, with no change to what the ledger records, when, or with what
durability (SPEC §10, T17, T38):

- **Store** (`capyctl-store`): the statements the router reads on every
  request and the request-ledger statements are prepared once per connection
  (`prepare_cached`, cache capacity 64). A cached statement is reset after
  use and re-prepared by SQLite after a schema change.
- **Router** (`capyctl-router`): a request admitted without waiting is
  ranked on the instance view admission already read (`InstanceView`), not
  on a second read of the same rows. The lease grant still re-checks the
  gate in its own transaction (T18), and any request that waited reads the
  instances afresh. `router_selection` now times the read only when it
  happens.
- **Listeners** (`capyctl-cli` `shutdown::serve`): every accepted inference,
  management and host-ingress connection has `TCP_NODELAY` set; axum 0.8 no
  longer sets it. Test: `served_connections_have_nagle_off`.

After, on the same reproduction: three alternating rounds of 400 requests
per case on a quiet machine, the router's added p50 went from 3.20–4.08 ms
(mean 3.66) to 3.00–3.50 ms (mean 3.28); in earlier runs on a busier
machine, from 5.6–6.4 ms to 4.25–4.85 ms. Planning a request now costs
0.05 ms instead of 0.5 ms, and the store reads on the way 0.9 ms instead of
2.1 ms. The remaining overhead is the two synchronous ledger commits (about
1.1 ms each here, one `fsync` of the write-ahead log) and their writer round
trips. The owner took all three options on 2026-10-18 (see "Request ledger
commits off the answer's path"): `synchronous=NORMAL` for the standalone
request ledger, closing the lease after the answer is sent, and indexes on
`lifecycle_steps(binding_id)` and `journal_entries(operation_id)` (the
instance read scanned both tables, about 1.2 ms per 1000 initialize
operations of history).

CPU and Fake-engine tests are not qualification. Live check still needed:
repeat the host A decision-request comparison (Julia-1 and Clef-Flash, one
and three questions, about 200 and 2k tokens of state) with this build.

## Decision models measured on one GB10; the guide's llama.cpp example — 2026-10-18 (branch `docs/decision-models-guide`)

Live on host A, 2026-10-17 to 2026-10-18, `main` at `075023d7` and `8824b622`, isolated
standalone, managed limit 108 GiB: eight decision deployments (Clef, Clef-Flash, Kev-4B,
lev, Julia-1 and Laya on llama.cpp v0.7.0; Clef and Clef-Flash on SGLang 0.5.22) became
ready, answered `POST /v1/systemone` through the router, and ran capyctl-bench 0.3.0's
decisions suites (about 4,200 requests each: latency grid, cache, throughput at 1 to 8
streams, AG News, BoolQ, Typed Decisions, and the Decision Index components API-Bank,
GPQA Diamond and Home appliances). Published scores reproduce: Clef 91.9% on API-Bank,
Clef-Flash 93.1% (SGLang), Julia-1 73.0% on Typed Decisions. The results, six recipes and
the comparison are in capyctl-recipes (`comparisons/decision-models-gb10`).

Findings for CapyCTL:

- llama.cpp v0.7.0 reads a decision request's whole input in one batch; with its default
  batch sizes a longer input is refused (HTTP 500, "increase the physical batch size").
  Every measured llama.cpp deployment sets `--batch-size` and `--ubatch-size` to the
  context. The guide's example (`docs/guide/deploy.md`) now does too and states
  `resources`.
- Decisions/s does not rise with concurrent requests on llama.cpp (flat from 1 to 8) and
  rises 7% to 18% on SGLang. A repeated input is at most 5% faster on either engine.
- CapyCTL's endpoint adds 12 to 26 ms per decision request against llama-server launched
  directly with the same arguments (Julia-1: 19.1 against 7.5 ms at one question about a
  200-token input); at 8 concurrent requests the throughput is the same.
- Measured peaks (`startup.measured`) of the recipes: Julia-1 1.4 GiB, Clef-Flash 15.8 GiB,
  Kev-4B 25.9 GiB and Clef 35.4 GiB on llama.cpp (16,384-token context and batch; 8,192 for
  Julia-1); Clef-Flash 51.9 GiB and Clef 87.5 GiB on SGLang, whose pool follows the
  reservation.

Documentation only; no code changed. These are live measurements of the listed engines and
checkpoints, not qualification of other recipes.

## Engine error answers end cancelling, not uncertain — 2026-10-17 (branch `fix/engine-error-answers`)

Found live on 2026-10-17 on host A (llama.cpp v0.7.0, decision deployments
Julia-1 and Laya, context 8192): an input over the context got llama.cpp's
complete JSON error with HTTP 500 (`input (8195 tokens) is too large to
process. increase the physical batch size`). The forwarder read every 5xx but
a `shutting_down` 503 as uncertain, the router ended the request lease
uncertain, and an uncertain lease stays charged for the life of the session,
so after about 56 such answers the deployment refused every request with 429
`queue_full` ("outstanding request bound reached") until it was deleted.
Clients saw only `engine_error` "backend completion unverified".

Spec §10 is amended (2026-10-17, with the T17 row and the amendment list):
an engine's complete error answer, HTTP 5xx other than 503 with a JSON object
body read whole within the bounds a rejection is read with (16 KiB, 5 s), or,
before any output, a stream error event with such a code, is relayed to the
client as HTTP 502 `engine_error` with the engine's bounded message,
`retryable: false`, in-band on a stream, and never offered to another
instance. Uncertainty still retains accounting: the answer does not prove the
engine stopped, so the lease ends `cancelling`, charged until the instance's
adapter reports engine-wide quiescence, exactly as after a client hang-up
(the coordinator's existing settlement; its receipt now reads "after the
request was cancelled or answered with an error"). That settlement already
covers local llama.cpp (`/metrics` `requests_processing` and
`requests_deferred` both 0), SGLang and vLLM (their running and waiting
gauges) and remote launches (the host's load report, including the ingress's
in-flight count). A truncated, oversized, unreadable or non-JSON 5xx body,
and every 503 but a role's `shutting_down` refusal, stays uncertain.

- **Forwarder** (`capyctl-adapters` `forward.rs`): `engine_error_status`,
  `engine_error_message` and a new `AdapterError::EngineError { status,
  message }`, on every path: decisions, collected and streamed chat, and the
  in-band error event before any chunk.
- **Host ingress**: relays such an answer with the engine's status as
  `engine_error` and its message; its own failures keep the code
  `inference_unavailable` (`INGRESS_FAILURE_CODE`), which the forwarder never
  reads as the engine's answer.
- **Router**: `EngineError` ends the lease `cancelling` and answers 502
  `engine_error` (collected, decisions) or an in-band `engine_error` event
  (streams); the in-memory slot is released as for an uncertain end, since
  the ledger keeps the charge.
- **Controller**: `EngineError` is `Uncertain` in fault classification and
  operations (`engine_error:<status>`).

Tests (T17, T19, T43): `capyctl-cli`
`roles_f1` `engine_error_answers_never_exhaust_the_deployment` (a standalone
role with the real coordinator and ledger; an engine answering every request
with llama.cpp's 500 gets three times the outstanding bound of answers
through, each a 502 with the engine's message, never replayed) and
`a_cut_engine_error_answer_stays_uncertain` (a 500 whose body is cut short
leaves its lease uncertain and the bound refuses with 429 after several
quiescence questions, as before the change); `capyctl-adapters` forwarder unit tests
`an_engine_error_answer_carries_the_engine_message` and the updated status
and in-band event tests; `capyctl-router` `router_core`
`an_engine_error_answer_is_relayed_and_leaves_the_lease_cancelling`,
`decisions` `an_engine_error_answer_is_relayed_and_leaves_the_lease_cancelling`
and the cut 500 in `an_unverified_decision_answer_leaves_the_lease_uncertain`,
`stream_safety` lease ends; `capyctl-agent` ingress
`engine_answers_are_relayed_and_other_engine_errors_stay_bad_gateway`. Every
new engine-error test, and the full-stack one, fails with the forwarder's
5xx reading turned off; with the router ending the lease uncertain instead
of cancelling, the full-stack test fails on the deployment staying
exhausted. CPU and Fake-engine tests are not qualification.

Live on host A, 2026-10-18, this branch's build in an isolated standalone,
llama.cpp v0.7.0: Julia-1 and Laya (Q8_0, context 8192) under capyctl-bench's
decisions run. Each model's 8k-token latency points sent 31 inputs over the
context; every one answered 502 `engine_error` with llama.cpp's message
("input (… tokens) is too large to process. increase the physical batch
size"), and the ledger journaled 41 `request_cancellation_acknowledged`
events. Both deployments kept serving: the throughput runs that followed
(200 requests at each of 1, 2, 4 and 8 streams) had no failures, where
before this change the same runs on `main` got 429 `queue_full` for every
request.

## SGLang 0.5.22 launches: its own plugins closed by selection — 2026-10-17 (branch `fix/sglang-0522-plugins`)

Found live on host A on 2026-10-17 with the approved SGLang 0.5.22
environment: every launch failed `sglang_startup_failed:
plugin_closure_failed`. SGLang 0.5.22 requires `smg-grpc-servicer`
(`Requires-Dist: smg-grpc-servicer>=0.9.0` in its `METADATA`); the installed
0.14.0 registers two `sglang.srt.plugins` entry points, `smg-servicer ->
smg_grpc_servicer.sglang.plugin:register` and `smg-zmq ->
smg_grpc_servicer.sglang.zmq_plugin:register`. The runtime guard
(`runtime/sglang_startup_guards.py::enforce_closed_plugins`) refused any
installed plugin, so it raised `external_plugins_present`. SGLang 0.5.21's
`smg-grpc-servicer` 0.13.0 registers no entry point. Recorded as an ADR 0008
amendment (2026-10-17); the plugin closure's intent (owner decision
2026-09-23, SPEC §13.3, T21) is unchanged.

SGLang's loader, read on host A
(`sglang/srt/plugins/__init__.py::load_plugins_by_group`; the 0.5.20 and
0.5.21 environments have the same whitelist lines):

```python
    allowed_set: set[str] | None = None
    allowed_str = envs.SGLANG_PLUGINS.get()
    if allowed_str:
        allowed_set = {x.strip() for x in allowed_str.split(",") if x.strip()}
    ...
    for ep in discovered:
        if allowed_set is not None and ep.name not in allowed_set:
            logger.info("Skipping plugin %s (not in SGLANG_PLUGINS)", ep.name)
            continue
        ...
        try:
            func = ep.load()
```

`load_plugins()` (scheduler, detokenizer, data-parallel controller, engine)
goes through it; the diffusion loader reads the same variable. The platform
path loads directly only when `SGLANG_PLATFORM` is set.

- **Selection.** `SglangLaunch::render_for_launcher` renders
  `SGLANG_PLUGINS=capyctl-loads-no-plugins` (`capyctl_config::engine_env::
  SGLANG_NO_PLUGINS`) into every SGLang command, single launches and group
  ranks, the adapter's and the controller's native handoff alike; SGLang's
  spawned processes inherit it. The agent's SGLang capability probe gets it
  too, since its guard runs the same closure.
- **Reserved names.** `SGLANG_PLUGINS`, `SGLANG_PLATFORM` and the `SMG_`
  prefix are owned names (`engine_env_reserved:<name>`), so neither a profile
  nor a deployment sets them; an approval glob `SMG_*` is refused.
- **Guard.** Installed general plugins are accepted only when
  `SGLANG_PLUGINS` is exactly that selection and each comes from a
  distribution SGLang requires (`smg-grpc-servicer`, name normalized) and
  none is named as the selection. Still refused: any other `SGLANG_PLUGINS`,
  any `SGLANG_PLATFORM`, any `SMG_*` variable (smg's zmq plugin activates on
  `SMG_SGLANG_ZMQ_HANDSHAKE`), any `sglang.srt.platforms` entry point, a
  plugin from another distribution, `native_already_imported`, an unreadable
  inventory. The module docstring records the 0.5.22 contract.

Tests: `runtime/tests/test_sglang_startup_guards.py` (smg-grpc-servicer's two
entry points pass under the selection and are refused without it or with an
empty one; an unknown distribution's plugin, alone or beside smg's, a plugin
named as the selection and a platform entry point are refused under it;
other selections, `SGLANG_PLATFORM` and `SMG_*` variables are refused; a real
spawn's `__mp_main__` guard passes with the inherited selection and refuses
with the handshake variable; no plugin target is imported); four of them fail
on the previous guard. `runtime/tests/test_sglang_entry.py` (the entry's
`main` launches with smg's plugins under the selection and renders
`plugin_closure_failed` without it or beside another distribution's plugin).
`capyctl-adapters` `sglang_args`, `sglang_group_args`, `sglang_initialize`
(the rendered and spawned environments carry the selection);
`capyctl-config` `engine_env` (the names are reserved; the Rust selection
matches the guard's); `capyctl-agent` installation (only the SGLang probe
carries it). CPU and Fake-engine tests only; not qualification.

Live on host A, 2026-10-17, this branch built there, an isolated standalone: Clef-Flash
(`Cloudflare/clef-flash@8b2e5fd1`, BF16) on SGLang 0.5.22 as an `api: decisions` deployment
was ready in 165 s; its engine log shows `Skipping plugin smg-servicer (not in SGLANG_PLUGINS)`
and `Skipping plugin smg-zmq (not in SGLANG_PLUGINS)`; a decision through the router answered in
0.11 s, and capyctl-bench's decisions run completed (AG News 89.4%, BoolQ 89.6% on its pinned 500
rows each). Qwen3-4B on SGLang 0.5.21 as a chat deployment was ready in 108 s and answered,
unchanged.

## Decision models, slice C: readiness and the wake check — 2026-10-17 (branch `feat/decisions-readiness`)

ADR 0030 (accepted, owner decisions 2026-10-17) §3 and §4; SPEC §6.1 is
amended and the amendment log gains the entry. Depends on slice A (the `api`
field, #144); the `/v1/systemone` route is slice B.

- **The fixed decision probe** (`capyctl_adapters::decision_probe`, kind
  `decision`, version 1): one `POST /v1/systemone` with the state "The ticket
  says: my card was charged twice." and one `choice` question (billing,
  shipping, technical, each with a description, and the instructions every
  engine requires). Ready when the answer is complete and well-formed: the
  question answered as a `choice`, a probability for every option and no
  other, each finite in [0, 1], summing to 1 within 1e-3, the pick one of
  the options (not necessarily billing).
- **Where it runs instead of a chat probe** for a decisions deployment: the
  llama.cpp and SGLang Initialize (bounded by the remaining startup window,
  since SGLang's first decision took 36 s live), SGLang's post-wake Probe
  step, the host agent's re-proof after a reconnect and after a restore
  (`fresh_probe`), and a restarted standalone role's re-proof of an adopted
  launch (which for chat stays a model list, unchanged). A decisions
  deployment is never sent a chat probe; vLLM and TensorFold cannot run one
  (slice A) and are unchanged.
- **The wake check:** the first readiness records the decision answer as
  the launch's reference (deployment, instance, generation, launch
  incarnation, as the chat canary), with the probe's version and the rule:
  same pick and every probability within 0.05. Store schema v49 rebuilds
  `wake_canaries` with `kind`, `probe_version`, `decision_json` and
  `tolerance`; existing references migrate as `completion`. A reference of
  another kind or probe version is never compared: the answer finds no
  usable reference. A wake that picks differently or moves a probability by
  more than 0.05 is `wake_mismatch` with the existing stop. The
  `Unsupported => passed` escape is never taken for a decisions launch (the
  adapters and the remote binding answer uncertain instead).
- **Which transitions compare today: none.** A decisions deployment
  resolves `restart_only`, so it never parks and never wakes; every restart
  (idle stop then start, operator restart, crash) is a new generation and
  launch that records its own reference. A host reconnect and a standalone
  restart keep the launch but re-prove readiness only (well-formedness), as
  for chat. The comparison is implemented in the wake path and tested
  there; it becomes live when parking a decisions deployment is allowed.
- **Protocol** (ADR 0017): `ExecuteMember.probe_decision` (field 18) asks a
  host for the decision probe of a retained decisions launch and needs the
  `decisions` capability; `MemberExecutionResult.probe_decision` (field 20)
  carries the answer, accepted only for a completed decision probe and only
  well formed. Both encode nothing when unset, so every existing command
  digests as before. The controller judges a relayed answer well-formed
  again before it counts.
- **Fake engine:** `FakeEngine::with_decisions`/`set_decision` answer
  `/v1/systemone` as scripted (well formed, malformed, another pick, other
  probabilities) and refuse chat, counting both; its lifecycle Initialize is
  proven by the decision, and its wake canary is the decision.
  Its `forward_decision` serves the public `/v1/systemone` route (#145),
  so the Fake answers a decision request end to end.

Tests (T43, with T20, T34): `decision_probe` unit tests (request shape,
well-formed and malformed answers, the 1e-3 sum); llama.cpp
`a_decisions_launch_is_ready_only_on_a_well_formed_decision` against a stub
engine (no chat request, the canary is a decision); controller
`a_decisions_launch_is_ready_only_on_a_well_formed_decision` (Fake),
`a_decision_canary_within_tolerance_completes_the_wake`,
`a_changed_pick_or_a_moved_probability_fails_the_wake`,
`a_chat_reference_is_never_compared_with_a_decision_answer`, the
`wake_canary` unit tests, and
`a_decision_canary_counts_only_a_well_formed_relayed_decision`; agent
`a_decisions_reproof_is_the_fixed_decision_never_chat`; store round trip,
refusals and the v48 to v49 migration; protocol encoding, validation and
capability tests; router
`a_fake_decision_model_becomes_ready_and_answers_through_the_router` (ready
on the decision, answered through the router, lease closed completed, no
chat request). CPU and Fake-engine tests are not qualification. Live
check still needed: a decisions deployment on llama.cpp 0.7.0 and SGLang
0.5.22 becomes ready on the probe, and re-proves after a host reconnect.

## Decision models, slice B: `POST /v1/systemone` — 2026-10-17 (branch `feat/decisions-route`)

ADR 0030 (accepted, owner decisions 2026-10-17) §2 and §5. Spec §10 gains
the decision endpoint (note and amendment log entry). Readiness and the wake
check for decisions deployments (ADR 0030 §§3–4) are another slice.

- **Router.** `POST /v1/systemone` shares the chat endpoint's API key, body
  bound, JSON parse, lookup by `model`, waiting, activation, admission,
  instance selection, request lease and settlement (one dispatch path,
  `chat::dispatch_timed`, with an `Endpoint`). The request is validated
  against its own closed contract before anything is touched: `model`,
  `state` (required: string, object, array, or null passed to the engine),
  `questions` (non-empty object; each `type` `noul`, `choice` or `score`,
  optional `instructions` string, object or array, optional `criteria`: an
  object for `choice` and `noul`, an array for `score`), optional `images` and
  `files` (arrays of strings), nothing else. SGLang's `chat_template_kwargs`
  is refused, since llama.cpp does not read it. Shapes only: counts and
  semantics are the engine's.
- **`api_mismatch`.** A chat deployment asked on `/v1/systemone`, or a
  decisions deployment asked on `/v1/chat/completions` (streaming or not), is
  HTTP 400 `api_mismatch` naming the endpoint to use, after the route lookup
  and before waiting, activation or a slot. The router reads the API from the
  deployment's current revision (`LifecyclePort::deployment_api`, store
  `deployment_api`).
- **Forwarding.** The engine forwarder's path allowlist gains exactly
  `/v1/systemone`; `ChatForward::forward_decision` sends one JSON body with
  `model` replaced by the served name and reads the answer whole (at most
  16 MiB, the chat collection bound), returning it with the public name. A
  whole, unambiguous JSON object whose `answers` is a non-empty object of
  objects each naming a `type` is completion; an engine 400, 413 or 422 with
  a complete JSON body is `engine_rejected` (SGLang's 422 validation
  `detail` now gives its message); anything else, a truncated or duplicated
  body included, is uncertain and never replayed. There is no first-token
  event: the whole answer is due by the request deadline, and a cut there
  is cancelling, as for chat.
- **Host ingress.** Exactly `POST /v1/systemone` besides chat completions,
  with its own body check (the decision contract), its fixed upstream path,
  JSON-only answers, and the same keys, gate, 8 MiB bound, guard and timings.
- **Metrics.** Generic timings and lease accounting are unchanged; a decision
  records no time to first token or decode speed and its answer carries no
  `capyctl.metrics`; the engine's `usage` passes through. No new labels.
- **`/v1/models`.** Each entry carries `capyctl.api` (`chat` or
  `decisions`), read from the store without waking anything
  (`LifecyclePort::list_enabled_routes` replaces `list_enabled_route_ids`).

Tests (T43): `capyctl-router/tests/decisions.rs` (admitted, forwarded and
settled like chat through the real forwarder; contract refusals and
`api_mismatch` both ways before activation, lease or engine; engine 400 and
SGLang 422 relayed with the lease completed; truncated, partial, ambiguous,
non-JSON and failed answers leave the lease uncertain without replay; the
request deadline bounds a stalled engine; `/v1/models` names the API);
`capyctl-agent/tests/ingress.rs` (path, contract and model checks, engine
rejection relayed); forward unit tests (contract, answer evidence, path
allowlist, validation detail); store test (the API follows the current
revision). CPU and engine-double tests only; not qualification. The live
check still needed: a decision request through the router and a host ingress
to llama.cpp v0.7.0 and SGLang 0.5.22, and whether SGLang's running and
waiting gauges count decision requests.

## Decision models, slice A: the `api` deployment field — 2026-10-17 (branch `feat/decisions-api-field`)

ADR 0030 (accepted, owner decisions 2026-10-17) §1 and §7, the part that
resolves or refuses a deployment's API. Spec §15.1 and §16 gain the field and
§20 gains T43; the readiness, routing and canary parts of ADR 0030 (§§2–5,
spec §6.1 and §10) are other slices.

- **`api: chat | decisions`** on a deployment, `chat` when omitted, in the
  strict schema. It is frozen in the revision, part of its recipe fingerprint
  and of the command fingerprint only when it is `decisions`, and round-trips
  through the frozen snapshot. Every existing revision keeps its fingerprint:
  the golden vLLM and SGLang revisions decode unchanged, and `api: chat`
  resolves to exactly the revision that omits it.
- **Resolution refusals**, all before anything is stored or launched
  (`capyctl_config::decisions`): an installation that cannot serve decision
  models is `capability_missing:decisions` (vLLM and TensorFold always;
  llama.cpp below 0.7.0 and SGLang below 0.5.22, read from the profile's
  registered `build_fingerprint`; a fingerprint that names no version is
  refused, so it fails closed). The server checks it against the profile the
  host published and the host checks it again at launch against the same
  profile. From the document alone, with the instance spec: a `topology` of
  any shape and a declared `deep` or `host_backed` residency are refused; an
  undeclared residency resolves `restart_only`. A stated parser
  (`engine_config.<engine>.tool_call_parser`/`reasoning_parser` other than
  `auto`, or the options in `extra_args`) is refused, and parser selection
  returns none for a decisions deployment (ADR 0024 applies to chat only).
- **Host capability `decisions`** (ADR 0017): every current agent declares it.
  At deploy, a host whose latest session did not declare it is recorded
  refused `host_capability_missing:decisions` (a single named host refuses the
  deploy); placement passes such a host over; and a launch, park or wake of a
  decisions deployment on a live session without it is refused typed before
  anything is sent.
- **Status:** `capyctl status deployment` shows an `API` line and `"api":
  "decisions"` in JSON for a decisions deployment; `validate config --host`
  shows `effective.api`. Both new codes exit 5 and are in the errors page.

Tests (T43, with T14, T34, T39): `capyctl-config` `tests/decisions.rs` (round
trip and default, fingerprints of the golden revisions unchanged, each engine
and version refused or accepted, topology and parking residency refused,
parsers refused and none chosen) and the version parser's unit test;
`capyctl-protocol` `decisions_gates_only_decision_deployments`;
`capyctl-store` `a_decisions_deployment_is_never_placed_on_a_host_without_the_capability`;
`capyctl-management` `a_host_without_decisions_never_takes_a_decisions_deployment`;
`capyctl-controller` `a_decisions_launch_needs_the_capability`; the CLI's exit
codes, errors page and status rendering. CPU and Fake-engine tests are not
qualification. No live check yet: a decisions deployment cannot become ready
until the readiness slice lands.

## ADR 0030 accepted: decision models — 2026-10-17 (branch `docs/adr-0030-decision-models`)

ADR 0030 records the owner's decisions of 2026-10-17 for decision models in 0.1.5: a deployment
field `api: decisions`, the inference route `POST /v1/systemone`, engines (llama.cpp v0.7.0 and
SGLang 0.5.22 first; vLLM once a release ships the endpoint; TensorFold 1.0 as its own change),
and recipes for Clef-Flash, Clef, Kev-4B and one of Julia-1 or Laya. It proposes readiness by
one fixed, well-formed decision and a wake check that compares probabilities within 0.05, and
three scope limits for the first version, confirmed by the owner (one machine,
restart instead of park, no readout in CapyCTL). Nothing is implemented yet. The SPEC
amendments it lists land with the implementation.

## 0.1.4 release preparation — 2026-10-17

Version bumped to 0.1.4 with release notes in
`docs/operations/release-notes-0.1.4.md`, assembled from the 13 change files by
`scripts/assemble-changes.sh 0.1.4` and edited into one note (SGLang groups park again,
`engine add --allow-remote-code`, the startup reservation in `status`, `capyctl version`
with the commit, engine latency in standalone, Nemotron 3.5 Super VL refused parking on
vLLM, and known issues). The owner decided on 2026-10-17 to release what is merged as
0.1.4 and to make decision models (`api: decisions`, `POST /v1/systemone`) 0.1.5.

Owner approval 2026-10-17: SGLang 0.5.22 on both hosts, for decision models
(`~/sglang-0.5.22-venv`: sglang 0.5.22, torch 2.14.1+cu130 with the GPU visible,
`torch_memory_saver` 0.0.10, `/v1/systemone` with Clef's joint schema head). Installed;
not yet run through CapyCTL, and not listed as supported in the guides.

Open for the owner: recipes PR #30 (Super VL with prefix caching off and the long-prompt
table), and the 3.6 × weights startup placeholder for a parking vLLM deployment (Super VL
measured 1.33 ×). CPU and Fake-engine tests are not qualification.

## SGLang observation transport test accepts both outcomes of its deadline race — 2026-10-17 (branch `test/observation-transport-deadline-race`)

`test_each_connection_reports_where_it_stopped_and_what_the_scheduler_did` failed once in a local
`ci-local` run (2026-10-16) and passed on reruns. In its first case the bridge expires the
request's slot at the same deadline the transport sends by. Usually the transport's deadline
passes first and no frame goes out (stage `send`); when the bridge's expiry is polled just
before it, the transport sends the correlated `uncertain` frame instead, which the transport's
contract allows ("valid observer failures may receive a correlated uncertain frame"). The test
asserted only the first. It now accepts either, and in both requires the outcome `uncertain`,
no observation, and a frame (if any) correlated to the request. Evidence: 120 runs on one core
under six busy-loop processes gave 103 runs without a frame and 17 with the uncertain frame
(each of those would have failed before), no failures. No product code changed.

## gpt-oss wake rechecked on vLLM 0.31.0; 0.1.4 status entry — 2026-10-16 (branch `fix/deep-wake-families-vllm-031`)

gpt-oss-20b was served by vLLM 0.31.0 alone on host A with sleep mode, put to sleep at level 2, woken and its weights reloaded (the deep park and wake CapyCTL performs). The reload logged `OAIAttention: Failed to load weights` for each of the 24 attention layers, as on 0.30.0, and the engine failed its first request and shut down. The gpt-oss family rule in `DEEP_WAKE_BROKEN_FAMILIES` now records both vLLM versions; its behavior is unchanged. The same check of Gemma 4 26B-A4B NVFP4 still answered garbled text after the wake, while the BF16 Gemma 4 E2B answered identically before and after, so no family rule was added for Gemma: the fault is with that checkpoint and the wake canary catches it. The status runbook gains the 0.1.4 progress entry.

## Group parks and wakes commit only fresh member reports; SGLang groups park again — 2026-10-16 (branch `fix/sglang-group-wake`)

Found live 2026-10-15 at MN4 (SGLang 0.5.21, TP2 across two GB10 hosts): a group deep park completed, but the wake failed twice. The head reloaded its weights for 186-222 s, the readiness probe and the 8-token canary ran, and the restore went `restore_uncertain` and the group stopped. For 0.1.3 SGLang groups were made `restart_only` (ADR 0028 note of 2026-10-15). The live failure text was not kept.

Cause, found in the controller (ADR 0028 note of 2026-10-16): `wake_group` read every member's own host report, then ran the head's readiness probe and the canary, then the worker committed those reports. The store accepts a report only while it is within its host's observation TTL (`fresh`, 2 s by default and on the lab hosts), so probes that took longer than that made the reports stale at the commit: `Rejected("evidence freshness")`, `group_member_uncertain`, group stop. vLLM's probes are fast, so its wakes passed. The group world reproduces it with head probes that outlast the TTL.

A park can hit the same rule. Found live 2026-10-16 on this branch's first MN4 SGLang run: the first park went `park_uncertain` with `evidence freshness` 9 s after acceptance and the group was stopped. The members settle at different times (each host pushes its reports on its own period; an SGLang member's saver map about every 2 s, and a park also waits for a residue sampled after the head's reply), and `collect` kept each member's first settling report while it waited for the others, so the earliest could be older than the TTL when the last settled. The two 0.1.3 SGLang parks that passed did so by timing.

- `collect` hands the commit only reports that are fresh by the store's rule. The worker reads the TTL the step's commit applies (`Store::group_residency_observation_ttl`: the observation TTL of the host policy `complete_group_residency` checks against) into `ResidencyTarget::observation_ttl`. Every poll asks every member's host again, settled members included (never keeping an older report over a newer one, and keeping a residue already measured after the head's reply when a round's process sample cannot be read), and returns only when each member's latest report settles and is between 0 and the TTL less `FRESHNESS_MARGIN` (300 ms, at most half the TTL) old by the controller's clock, the one the commit uses. A member with no report that young by the end of the evidence window is silent (`group_member_uncertain`, charge kept); one still resident (or asleep) still fails the group at its rank. Park and wake, vLLM and SGLang.
- `wake_group` (`crates/capyctl-controller/src/group_residency.rs`) keeps its first read of every member's report (a rank not resident still fails the group at that rank before the head is probed), and once the canary passed reads every member's own report again with a fresh `since`, observed after the canary, and returns those for the commit. Same rules: each member on its own host's report, the bounded evidence window (from the canary), the collective sent once.
- A single launch's deep wake had the same order: the restore's evidence, then the canary, then `complete_residency` against the same TTL. `EngineAdapter::wake_canary` now answers a `CanaryAnswer` (the probe's answer and when it was observed: this host's clock for an embedded vLLM or SGLang engine, the host agent's for a remote launch). Once the canary passed, the worker commits the restore's evidence as observed with the canary's answer; an embedded engine's processes are checked alive again first, and a remote canary counts only when the host's reply has the claim retained and names the launch's engine alive (`same_engine`, as a restore's result must). An engine without a canary keeps the restore's own evidence.
- SGLang groups park again: `group_support("sglang").deep_park` is true, `resolve_effective_with_checkpoint`'s group-member `restart_only` default is removed (an SGLang group with no residency resolves `deep`), and `deep` is accepted at deploy. `check_engine_shape` still refuses both parking tiers for an engine whose group cannot park (TensorFold).
- Docs: `docs/guide/engines.md` (groups table, the 0.1.3 note replaced) and `docs/guide/several-machines.md` (park and wake) say SGLang groups park and wake again. ADR 0028 keeps its note of 2026-10-15 and adds one of 2026-10-16. The live row MN4 runs the deep cycles for SGLang again (`scripts/live/matrix/rows/MN4.sh`, `gen_group.py`, the matrix README).

Tests (T20, T03, T22). Group world (`crates/capyctl-controller/src/coordinator/tests_groups.rs`): `a_wake_whose_probes_outlast_the_observation_ttl_completes` (vLLM and SGLang, each head probe a little over half the TTL) failed before the change with `group_member_uncertain: the members' reports were not accepted: ... evidence freshness` and passes now. `members_reporting_out_of_phase_park_and_wake_on_fresh_reports` (vLLM and SGLang; a 700 ms TTL, host B reporting each change of its rank 1.1 s after it, so host A's first settling report is stale when host B's settles) failed before the `collect` change with `vllm: the park failed: group_member_uncertain: the members' reports were not accepted: ... evidence freshness` and passes now. `park_needs_every_rank`, `a_park_settles_on_released_device_memory_and_charges_each_members_residue`, `a_member_without_evidence_of_its_own_fails_the_park_within_the_window`, `wake_canary_mismatch_stops_the_group` and `repeated_cycles_are_clean` run SGLang deep groups beside vLLM ones; `sglang_deep_group_with_differing_paths_is_refused` expects `group_model_path_mismatch` again. Single launch (`tests_residency.rs`): `a_wake_whose_canary_outlasts_the_observation_ttl_completes` (compound and stepwise restore, the canary answered one TTL and a second later) failed before with `wake_uncertain` and passes now. Config (`crates/capyctl-config/tests/topology.rs`): `vllm_and_sglang_groups_resolve_deep_by_default` replaces `sglang_group_resolves_restart_only_and_refuses_parking`; `engine_shape_support` accepts SGLang `deep` and still refuses TensorFold `deep` and `host_backed`. CPU and Fake-engine tests are not qualification.

Live, 2026-10-16, both lab hosts, server on the control host (SGLang 0.5.21, vLLM 0.31.0, the two GB10 hosts over the direct link). The first MN4 SGLang run (commit `2e9e7d14`, before the `collect` change) failed its first park with `evidence freshness`, as described above. With the `collect` change (`23744456`): MN4 SGLang (TP2, Qwen3-30B-A3B) passed five deep park and wake cycles, five `parked` and five `restored` journal entries, the same processes and the canary matching on every wake; parks settled in about 15 s and wakes, which reload the weights from disk, took about 190 s. MN4 vLLM passed its five cycles unchanged. On this branch rebased onto `main` (`38330292`), the single-machine rows passed on host A: M29 vLLM (deep park, a wake on request in 81 s, the same processes, the reference answer with a log-probability difference of 0.0) and M28 SGLang (the same, the wake in 196 s). The final stop of each group needed SIGKILL after SIGTERM for its members (`group_member_escalated`), as in 0.1.3; every process was gone. CPU and Fake-engine tests are not qualification; these live rows are the evidence for this change.

## Nemotron 3.5 Super VL is not parked on vLLM — 2026-10-16 (branch `fix/supervl-deep-wake-refusal`)

In the recipe runs for Nemotron 3.5 Super VL (NVFP4) on vLLM 0.31.0, every deep park's wake
failed CapyCTL's wake canary (`wake_mismatch`), with and without a drafter, so the model was
stopped after each wake. vLLM alone reproduces it on host A, with no CapyCTL: served with
`--enable-sleep-mode` (8k context, one sequence), a greedy prompt answered the same prose twice;
after `/sleep?level=2`, `/wake_up` and `reload_weights` (the deep park and wake CapyCTL performs)
the same prompt repeated `</think>` to the token limit, twice. The defect is in vLLM's sleep and
reload of this model, not in CapyCTL; the canary caught it as designed.

The checkpoint's family (`model_type: nemotron_h_omni`, architecture
`NemotronH_Omni_Reasoning_V3`) joins gpt-oss in `DEEP_WAKE_BROKEN_FAMILIES` for vLLM, so a
parking deployment of it is refused at deploy with `capability_missing:deep_park` instead of
failing at its first wake. `residency: restart_only` serves it, as the recipe does. The
text-only Nemotron H family (`nemotron_h`) is not affected by the rule. The parking guide now
lists the models CapyCTL refuses to park.

Test (T21, T22): `nemotron_super_vl_refuses_parking_on_vllm_only` in
`crates/capyctl-config/tests/deep_wake_refusal.rs` (fails before, passes after). CPU tests only;
not qualification.

## `capyctl version` reports the build commit; standalone records engine latency — 2026-10-16 (branch `fix/version-commit-and-standalone-latency`)

Two small 0.1.4 items.

**Build commit.** `capyctl --version` printed only `capyctl 0.1.3`, while the
release tarball's BUILDINFO records the commit (`commit: 345806a5…`), so a
binary could not say which commit it came from. `packaging/release.sh` now
exports `CAPYCTL_BUILD_COMMIT` for the compile (the BUILDINFO commit, `-dirty`
appended for a tree built with `CAPYCTL_RELEASE_ALLOW_DIRTY=1`), and a new
`capyctl version` command prints `capyctl <version>` and then `commit: <sha>`
(`--json`: `{"name", "version", "commit"}`, `commit` null for a build that
recorded none, such as a plain `cargo build`). `capyctl --version` is
unchanged on purpose: `packaging/install.sh` compares its whole output with
`capyctl <version>`, as do `scripts/verify-packaging.sh` and
`scripts/test-install.sh`, and an installer from an earlier release would
refuse a newer binary whose `--version` grew a second line. Keeping that line
and adding a subcommand changes no consumer. The commit is fixed for a given
commit, so rebuilding it stays byte-identical, and the shipped-binary check's
rebuild sees the same exported value. `scripts/verify-packaging.sh` now also
checks that `capyctl version` reports BUILDINFO's commit. The install guide,
SUPPORT.md, the bug report template and the release build section of
`docs/operations/install.md` say where the commit is.

**Standalone engine latency.** Found live 2026-10-10: standalone's latency
read (`GET /management/v1/metrics/latency`, and `latency` in `capyctl status
deployment --json`) carried router series only. `EmbeddedLoad::tick` accepted
each report's load sample into its load table but dropped the engine latency
histograms the same reports carry, so `source: engine` series were absent on
standalone for every engine. A server records a host's reports into its
latency table before the load table (`agent_sessions`); `EmbeddedLoad` now
does the same into its own `LatencyTable` (SPEC §17, M80), and standalone
reads that table in its latency view and for the timing header's engine
family, as a server reads its hosts'. There is still no ingress tier on
standalone: it has no host ingress.

Tests: `standalone_reports_its_engine_latency` (T40, `roles_f1`) boots
standalone with an SGLang-shaped Fake engine at the launch's endpoint whose
`/metrics` carries a time-to-first-token histogram; the latency read shows
the instance as `sglang` with an `engine_time_to_first_token` series, `tier:
engine`, `source: engine`, count 2. It failed before the change (no instance
in the read at all). `version_keeps_the_installer_line_and_adds_the_build_commit`
(`terminal_output`) checks that `--version` is exactly `capyctl <version>`,
that `capyctl version` starts with that line and that its JSON carries the
recorded commit; it passes with and without `CAPYCTL_BUILD_COMMIT` set for the
build. CPU and Fake-engine tests are not qualification; the live check still
needed is a standalone run on a native vLLM or SGLang engine reading engine
series from the latency view.

## Status shows the whole-host startup reservation of an unmeasured model — 2026-10-16 (branch `fix/startup-placeholder-deep`)

Found live 2026-10-16 (Nemotron 3.5 Super VL 120B-A12B NVFP4 on vLLM 0.31.0, one GB10 of
121 GiB, standalone managed limit raised to 108 GiB): every new parking deployment
(`residency: deep`, the default) of the model, `engine_config.memory.request: 96GiB`, showed
`startup` 284713418881 bytes (265.2 GiB, provenance `default`) in `status deployment`, more
than the machine has. The checkpoint weighs 76328140836 bytes with the LiLiCorr drafter's
files (75010188240 without, which showed 260.7 GiB for the MTP deployments).

The figure is the placeholder exactly as specified, not an arithmetic fault. A parking vLLM
deployment gets `--safetensors-load-strategy eager`, so its placeholder uses
`VLLM_EAGER_STARTUP_WEIGHTS_FACTOR` (3.6, ADR 0014 amendment A21):
`max(request + graphs, weights × 3.6 + 8 GiB)` = 76328140836 × 18 / 5 + 8 GiB =
282.4e9 bytes, plus the 1.25 GiB CUDA context placeholder every derived cold phase carries:
284713418881. With `residency: restart_only` the loader stays vLLM's default and the factor
is 2.25 (about 169 GiB); those deployments showed 97.2 GiB only because their first start
had already been measured.

What was wrong is the status figure, not admission. Owner decision 2026-09-23 (solo first
start, ADR 0015 amendment of 2026-09-23): an unmeasured placeholder above the managed limit,
with a steady request that fits, makes the start reserve the whole managed limit, alone on
its host, provenance `whole_host`. `frozen_plan_startup` did that for every start here (the
265 GiB cold phase could not have passed the arm against a 108 GiB limit; the session always
started on an emptied host). But the deployment's `startup` in status
(`ordinary_lifecycle/startup.rs`, `status`) re-implemented only the measured and
weighed-placeholder branches of `frozen_plan_startup` and never its whole-host branch, so it
showed the placeholder as `default`, contrary to the owner decision of 2026-10-07 that status
shows the figure admission reserves. Only an instance still starting showed `whole_host`.
`status` now calls `frozen_plan_startup` itself, so the two cannot drift again: such a
deployment shows `bytes` = the managed limit, provenance `whole_host`, until a first start is
measured.

Measurement of deep first starts: they are recorded. Decoding the commands' request
identities against `measured_at_ms` shows the peaks of `supervl-vllm`, `-nosr`, `-nospec` and
`-nopc` (deep) and of `-ro7` and `nemotron-super-vl` (restart only) recorded within a second or
three of Ready (the later stop requests of the session followed Ready immediately, which made
some look recorded at stop). The five deployments with no row (`-lc5`, `-lc5-4seq`,
`-lc7-4seq`, `-mtp3`, `-mtp5`) are the configurations started for the first time with a new
speculative or concurrency shape, whose starts took 308 to 892 s against 166 to 188 s for the
same shapes once warm. [INFERENCE] Their samples fell inside reported kernel builds, which
ADR 0014 amendment A12 leaves out, or the start was contended; the engine logs that would
show which were not kept. Status lists every recorded row, so it is not a display issue, and
each such start keeps reserving the whole managed limit until one is measured, as specified.

Open for the owner, not changed here: the measured eager-loader peak of this model on deep
park was 101695578112 bytes (94.7 GiB, 1.33 × the weights), against the 3.6 × the weights
amendment A21 derived from one Qwen3.6-35B-A3B NVFP4 run.

Tests: `status_shows_the_whole_host_a_start_above_the_managed_limit_reserves`
(`capyctl-controller`, `coordinator/tests_startup.rs`) resolves the live shape (parking
vLLM, request 96 GiB, 76328140836 bytes of weights, managed limit 108 GiB), checks the
placeholder is 284713418881 and that status shows `whole_host` 108 GiB, the same figure the
start then holds; it failed before the fix (status `default`, 284713418881).
`an_unmeasured_model_above_the_managed_limit_starts_alone_and_is_measured` and
`weights_sized_while_the_digest_is_pending_trigger_the_solo_first_start` asserted the old
status figure and now assert `whole_host` at the managed limit. CPU and Fake-engine tests
only; not qualification. Live check still needed: a new parking deployment of a model whose
placeholder is above the managed limit shows `whole_host` at the limit before and during its
first start, and `measured` after it.

## `engine add --allow-remote-code` — 2026-10-16 (branch `feat/engine-add-remote-code`)

A deployment with `engine_config.trust_remote_code: true` is refused unless its
installation has `security.trust_remote_code: true` (Spec §3, ADR 0014 §8), and
`capyctl engine add` always wrote `false`. Serving Nemotron 3.5 Super VL on vLLM
needed checkpoint code, so `engines.yaml` had to be edited by hand and the role
restarted.

`capyctl engine add` now takes `--allow-remote-code`, which writes
`security.trust_remote_code: true` for the new profile (`ProfileSpec` carries the
switch, so the environment profiles built from `local_engine.trust_remote_code`
use the same field instead of patching the document afterwards). The result
reports `trust_remote_code`, and the text view adds `Remote code   allowed` when
it is set. TensorFold and llama.cpp have no remote-code option and already refuse
`trust_remote_code: true` in a deployment (ADR 0023 §4, ADR 0029 §5), so
`engine add --allow-remote-code` is refused for them with `capability_missing`
before anything runs or is written. The deploy-time refusal keeps its text and
now names `capyctl engine add --allow-remote-code` (and `--trust-remote-code true`
for the role's own engine); the refusal of a profile argument
`--trust-remote-code` names the option too.

Tests: `add_allows_checkpoint_code_only_when_asked` (engine_cli) adds one vLLM
profile with the option and one without, then validates a deployment with
`trust_remote_code: true` against each through `capyctl validate config`: the
first resolves, the second is refused naming `--allow-remote-code`. The
TensorFold and llama.cpp add tests assert the refusal and that nothing is
written or run. CPU and Fake-engine tests are not qualification; the live check
still needed is registering the vLLM installation on a lab host with
`--allow-remote-code` and deploying Nemotron 3.5 Super VL on it without editing
`engines.yaml`.

## Runbook: September entries archived — 2026-10-16 (branch `docs/runbook-archive-2026-09`)

`docs/runbooks/f2-current-status.md` had grown to 6,815 lines, half of them entries up to
2026-09-30 and planning sections written before the rename to CapyCTL (milestones, open
questions, owner attention items such as a September driver question on host A) that no
longer describe the current state. They move unchanged to
`docs/runbooks/archive/f2-status-2026-09.md` (a byte-for-byte split, checked with `cmp`; two
relative ADR links adjusted for the new directory), and the runbook ends with a pointer to
them. The runbook keeps every entry from the 0.1.0 release candidate (2026-10-01) on.

## llama.cpp v0.7.0 build approved on both hosts — 2026-10-16 (branch `feat/nemotron-3.5-llamacpp-070`)

Owner approval 2026-10-16: a llama.cpp v0.7.0 build (tag commit `620d222`) in the home
directory of host A and host B, for Nemotron 3.5 Bolt 74B-A9B 24GB and Nemotron 3.5 Super VL
120B-A12B. v0.7.0 adds both architectures and the LiLiCorr drafter; v0.6.0 has neither.
AGENTS.md lists the build. `scripts/live/llamacpp-build.sh` takes `LLAMACPP_TAG=v0.7.0`
and checks the tag's commit as it does for v0.6.0; any other tag is refused. Built on both
hosts on 2026-10-16: `version: 0.7.0 (build 1, commit 620d222)`, fingerprint `0.7.0+620d222`,
byte-identical binaries and libraries on the two hosts (`llama-server` sha256 `af2ea2c8…`).
CapyCTL 0.1.3 registers it unchanged (custom, as every llama.cpp build) and runs Bolt and Super VL
with stated `resources`, the LiLiCorr drafter through the approved `--spec-draft-model`, vision
through `llamacpp.mmproj_file`; text, tool calls, images and streaming pass through the router.

## Docs show the current engine versions — 2026-10-16 (branch `docs/current-engine-versions`)

The guides still showed the engine versions of their first writing (vLLM 0.29.0, SGLang 0.5.20,
TensorFold 0.6.0 and 0.6.1) after 0.1.3 verified vLLM 0.31.0 and TensorFold 0.6.6. All three
stay supported; the docs now lead with the newest.

- **"First model on each engine" was re-run live** on host A with the published 0.1.3
  binary (isolated config, state and prefix; checkpoints from the shared cache), vLLM 0.31.0,
  SGLang 0.5.21 and TensorFold 0.6.6. Ready in 53 s, 91 s and 109 s (TensorFold restart
  after stop: 12 s); every chat answered. The output blocks are that run's. The Qwen3 answers no
  longer carry `<think></think>` in `content`: the reasoning parser CapyCTL picks moves it to
  `reasoning` (vLLM) or `reasoning_content` (SGLang). Everything was removed afterwards.
  Evidence: `target/live/docs-walkthrough-0.1.3/`.
- **Install an engine:** the vLLM steps pin 0.31.0 (0.30.0 and 0.29.0 named as alternatives,
  all on PyTorch 2.13.0) and link vLLM's 0.31.0 guide.
- **Engine options:** the vLLM table covers 0.30 and 0.31 and lists 0.31's logging options
  (`--log-level`, `--log-config-file`, `--logging-config`) as reserved, which the deploy-time
  policy already does; `engine_flag_table` checks the row.
- **Requests:** the streamed-field and per-request-metrics tables include vLLM 0.31.
- **Illustrative output** (engine detect, add and list, the example host documents'
  `build_fingerprint`): vLLM 0.31.0, SGLang 0.5.21, TensorFold 0.6.6, and a 0.32.0rc1 nightly
  for the custom-build example. The site's transcript capture uses the same versions; its
  one-machine part was rerun with the 0.1.3 binary and printed these lines.
- **Stale release references:** `multi_gpu_unsupported` no longer says "one GPU per model in
  0.1.0 ... planned after 0.1.0" (in the error and in the install guide); it points to
  `topology` for a model across machines. The `engines.yaml` compatibility note dates the
  skip of an unknown engine's profile to 0.1.2 instead of "this release".
- The groups table says its versions are the ones the two-machine live runs used.

Measured figures attributed to a version (wake times, memory, sources read) keep the version
they were measured on.

## Launcher tests wait for reparented zombies to be reaped — 2026-10-16 (branch `test/launchers-orphan-zombie`)

`owned_launch::tests::an_unrecorded_worker_of_a_dead_held_leader_is_terminated` failed once in
hosted CI (PR #130): right after `terminate_escalating` returned, `observe_group` still listed the
terminated worker. The worker had exited (`runs` read false), but its leader died first, so it
was reparented to init and stayed a zombie in the process group until init reaped it, which on a
loaded runner is not immediate. Four tests asserted an empty group at the very next scan; they
now go through `assert_group_empties`, which allows up to 5 s for the group to empty. The
product path is unchanged: Terminate already treats an exited process as gone. Test only;
`cargo test -p capyctl-launchers --lib owned_launch` passed 6 of 6 times under 4 cores.

## Live rows: the harness files #126 left out — 2026-10-15 (branch `test/live-rows-harness-followup`)

PR #126 (MN1–MN8 and LC1–LC6) merged without eight of the harness files the rows ran with, which
stayed uncommitted when the change was ported to a clean branch. This adds them exactly as they
ran in the three live sessions (identical to `test/live-rows-0.1.3`):

- `lib.sh`: the run server's loopback ports (`CAPYCTL_SERVER_MGMT_PORT` and
  `CAPYCTL_SERVER_INFER_PORT`, for a control host already serving 7443/8443), and the harness's
  own tmux server (`tmux -L capyctl-mx`; an old tmux server passed an 8 MiB memlock to the host
  roles, which broke NCCL at group bring-up).
- `roles.sh` and `gen_host_doc.py`: each host document declares `resource_policy.groups` (its
  direct-link peer address and rendezvous ports) and a TensorFold profile when
  `TENSORFOLD_VENV_DIR` is set.
- `ledger.py`: the read-only `group` and `members` views.
- `fault.sh member`: signals a group member's spawned process by its recorded identity.
- `hosts.example.env`, `README.md`, `rows/ENG2.sh`: the new variables, the MN and LC rows,
  and the `local` profile name.

Harness only; no product change. Checks: `bash -n`, `scripts/ci-local.sh --only shellcheck`,
`python3 -m unittest discover -s scripts/live/matrix/tests`.

## Releasing: per-architecture packaging checks and installing a draft — 2026-10-15 (branch `docs/releasing-draft-install`)

Found while releasing 0.1.3. `releasing.md` step 3 suggested passing both tarballs to
`scripts/verify-packaging.sh`; the script checks only the first and runs its binary, so the
aarch64 tarball failed on an x86_64 machine (`Exec format error`, and the install tests looked
for a tarball of the host's architecture). Each architecture is now checked on a machine of that
architecture (0.1.3: x86_64 on a maintainer machine, aarch64 on a GB10, 24 checks each, no SKIP),
with a temporary `SHELLCHECK` binary where none is installed.

Step 5 said a draft installs with `gh auth login` or `GITHUB_TOKEN`. GitHub's release-by-tag API
returns 404 for a draft, so `install.sh` with only a token reports the release as not found. The
step now gives the two ways that work: `gh` logged in on the host, or the draft's assets copied
into a directory and `CAPYCTL_INSTALL_BASE_URL=file://...` (the installer still verifies
`SHA256SUMS`), with `PREFIX` to keep the check apart from an installed capyctl. It also names
the several-machines guide for the server and host (both on one machine), since the one-machine
guide covers standalone only. Docs only; no behaviour changed.

## 0.1.4 in progress — 2026-10-16

Merged on `main` since 0.1.3, each with `ci-local` and the deep workspace suite
passing locally and hosted CI green (details in each change file):

- #134: the entries up to 2026-09-30 moved unchanged to
  [archive/f2-status-2026-09.md](archive/f2-status-2026-09.md).
- #135: `capyctl engine add --allow-remote-code` writes
  `security.trust_remote_code`.
- #136: `status` shows the startup reservation admission holds; a parking
  vLLM deployment whose placeholder exceeds the managed limit reads
  `whole_host` (it showed 265 GiB `default` for Nemotron 3.5 Super VL while
  admission held the 108 GiB limit).
- #137: `capyctl version` prints the build commit; standalone records engine
  latency histograms.
- #138: Nemotron 3.5 Super VL (`nemotron_h_omni`) is refused deep and
  host-backed parking on vLLM. vLLM 0.31.0 alone reproduces the broken wake.
- #139: group parks and wakes commit only member reports still within the
  observation TTL; SGLang groups park again. Live on both lab hosts: MN4
  SGLang and MN4 vLLM 5/5 deep park and wake cycles, M28 SGLang and M29 vLLM
  single-machine park and wake pass (the first MN4 SGLang run, before the
  `collect` change, failed its first park with `evidence freshness`).

Recipe rechecks on vLLM 0.31.0 (host A, 2026-10-16, vLLM alone with sleep
level 2 and `reload_weights`): gpt-oss-20b still fails its reload (24
`OAIAttention` warnings, then the engine fails its first request); Gemma 4
26B-A4B NVFP4 still answers garbled text; the BF16 Gemma 4 E2B answers
identically before and after. The recipes stay `restart_only`; the gpt-oss
family rule records 0.31.0. No Gemma rule was added: the fault is specific
to the NVFP4 checkpoint and the wake canary catches it.

Owner attention:

- The placeholder startup factor for a parking vLLM deployment with eager
  loading is 3.6 × weights (ADR 0014 A21); Super VL measured 1.33 ×. Left
  unchanged pending a decision.
- Probe checkpoints on host A in `~/probe-models` (gpt-oss-20b, Gemma 4
  26B-A4B NVFP4, about 57 GB) and the Nemotron 3.5 files on both hosts are
  kept until the owner decides.

CPU and Fake-engine tests are not qualification.

## 0.1.3 published — 2026-10-15

The owner published `v0.1.3` (tag on `345806a5`, marked Latest) after the
Release build workflow (run 38822609377, Rust 1.99.0), the strict packaging
check on x86_64 and aarch64, and a live install from the draft on host A. The
site installer then installed 0.1.3 at that commit on host B.

Evidence loss, 2026-10-16: the local row evidence of the LC1–LC6 and MN1–MN8
runs (`target/live/matrix/LC*/`, `MN*/` and `0.1.3-summary.md`, cited below)
was deleted with the worktree it was written in during the post-release
cleanup. The per-run directories remain under `~/capyctl-runs/matrix-*` on
host A, host B and the control host; the results are as recorded in the
entries below.

## 0.1.3 release preparation — 2026-10-15

Version bumped to 0.1.3 with release notes in
`docs/operations/release-notes-0.1.3.md`, assembled from the 46 change files
by `scripts/assemble-changes.sh 0.1.3` and edited into one note (multi-node
groups qualified on two GB10 hosts, llama.cpp as a fourth engine, TensorFold
0.6.6 and vLLM 0.31.0 verified, the memory, request and wake fixes since
0.1.2, and known issues). The status entries of those change files are folded
in below. The verification result is in the release pull request.

The release PR's hosted CPU checks failed once on
`group_observation::tests::unrelated_process_exits_during_a_scan_do_not_fail_the_observation`
(1 of 200 scans `InvalidData`), an old flake: a process exiting during a group scan can read
back an empty or truncated `/proc/<pid>/stat`, which failed to parse, and only a failed read
of a vanished pid was skipped. `classify_listed` (capyctl-launchers `group_observation.rs`)
now also skips an unparsable `stat` when the pid is gone on a second look; for a pid still
listed it fails closed as before. New unit test
`an_unparsable_stat_is_skipped_only_when_the_pid_is_gone`; the stress test passed 5 of 5 under
4 cores.

## llama.cpp GB10 rows LC1–LC6 run live on host A, all pass — 2026-10-15 (branch `test/live-rows-0.1.3`)

Owner approval 2026-10-14: a llama.cpp build on host A, in the home directory, with
no system package, driver or CUDA change. [ADR 0029](../design/adr/0029-llamacpp-engine.md),
plan slice L6, and the "Still open" list of `llamacpp-docs-live.md` (the GB10 row and the
row scripts).

- `scripts/live/llamacpp-build.sh` (`source`, `model`, `prebuilt`, `check`, each
  idempotent, every effect through the harness's `rsh`, `--dry-run` prints the plan):
  clones tag `v0.6.0` (checked against commit `d812350`) into `~/llamacpp-v0.6.0` and
  builds `llama-server` with `-DGGML_CUDA=ON -DLLAMA_BUILD_IS_DEV=OFF
  -DCMAKE_CUDA_ARCHITECTURES=121a-real` (GB10 is sm_121; the tag's `ggml-cuda` CMake
  turns a plain `12X` into `12Xa`, and its own default list uses `121a-real` with CUDA
  12.9 and later, which works with any CMake); prints the `--version` line, the build
  fingerprint `<v>+<commit>` and the sha256 of the binary and its libraries. `model`
  downloads `Qwen/Qwen3-4B-GGUF` `Qwen3-4B-Q4_K_M.gguf` at the laptop rows' revision
  and checks its size and digest. `prebuilt` fetches the `b11429` Ubuntu CUDA 13.4 arm64
  archive and its CUDA runtime only to record the version line (live check 9); the rows
  never run it.
- `scripts/live/matrix/rows/LC1.sh`–`LC6.sh`, with `llamacpp_rows.sh` (shared steps)
  and `llamacpp_probe.py` (run on the host: chat and stream shapes, the engine's
  `/metrics`, `/slots`, `/props`, the management load read, hang-ups, concurrent
  streams, and memory samples every 0.5 s from `/proc/meminfo`, the process's resident
  set, `nvidia-smi` and the GGUF's page-cache bytes by `mincore`). Each row stops host
  A's matrix role, runs a standalone role with its own state directory (as ENG2), and
  restarts the matrix role. LC2 measures the default mmap load and `--load-mode none`
  one after the other, which answers whether mmap keeps a page-cache copy beside the
  device copy. LC4 runs llama.cpp beside host A's owner-approved vLLM 0.30.0 environment
  under a lowered managed limit (40 GiB by default) and checks first that each request
  fits alone and the two do not. LC6 does not run `--spec-type draft-mtp`: Qwen3-4B has
  no MTP heads and no model with them is staged.
- `lib.sh`: `ENGINE_PGREP` also matches `llama-server`, so `host_clean` reports a
  leftover llama.cpp engine.

CPU and Fake-engine tests are not qualification; these rows are. Live 2026-10-15 on
host A (llama-server 0.6.0+d812350, sm_121; evidence `target/live/matrix/LC*/`, summary
`target/live/matrix/0.1.3-summary.md`): LC1–LC6 pass.

- LC2: Ready 8.3 s with the default mmap load, 3.2 s with `--load-mode none`; decode
  about 71 tok/s. mmap: steady MemAvailable drop 8.24 GB, device 7264 MiB; `none`:
  5.13 GB and 2567 MiB steady. The GGUF stays in the page cache in both, so mmap keeps
  a page-cache copy beside the device copy (about 3 GB more MemAvailable used).
- LC4 at 40 GiB: llama.cpp request 17.26 GB (measured), vLLM va-4 38.89 GB before its
  first start and 27.11 GB once measured. A request to vLLM stopped llama.cpp (30.6 s);
  a request to llama.cpp parked vLLM (9.2 s, vLLM device 12495 → 311 MiB); vLLM woke
  in 9.9 s. At 44 GiB the measured pair fitted and nothing was parked, which is correct.
  Rerun on the 0.1.3 build (with #121's vLLM wake path), same day: pass. Switch to
  vLLM 35.2 s, to llama.cpp 7.5 s with vLLM `parked` (12495 → 311 MiB), vLLM wake
  10.2 s.
- LC3 park refused `unsupported_capability`; LC5 hang-ups free the slot in 55 ms
  (decode) and 2 ms (prefill); LC6 three concurrent streams all complete.

Harness fixes from the run: a standalone with only `CAPYCTL_LLAMACPP_BIN` publishes the
profile `local`, not `local-llamacpp` (LC2/3/5/6); LC4's limit (32 GiB was below vLLM's
default request) and its park assertion; LC4's early exit stops its memory sampler.
Still to do: recompute `LLAMACPP_OVERHEAD_MARGIN_BYTES` and the host overhead
placeholder (`crates/capyctl-config/src/effective/engine_config.rs`) from these figures.

## Multi-node live rows MN1-MN8 pass on the 0.1.3 build — 2026-10-15; SGLang groups are restart_only (branch `test/live-rows-0.1.3`)

Plan `docs/plans/2026-10-05-multi-node-groups.md` Task 21, design
`docs/specs/2026-10-05-multi-node-groups-design.md` §17.2, ADR 0028. The live
harness gains the multi-node group rows, first run live 2026-10-15 (below).

- `scripts/live/matrix/rows/MN1.sh` … `MN8.sh`: bring-up at TP 2 for vLLM,
  SGLang and TensorFold (Ready, 20/20 routed completions, RDMA counters before
  and after on both hosts recorded as `transport: rdma` or `socket`), five deep
  park and wake cycles with per-member reservations, MemAvailable on each host,
  the wake canary's reference tokens and one `parked`/`restored` journal entry
  per cycle (MN4 vLLM; MN4 SGLang checks the restart_only park and the refusal of
  `residency: deep` instead), SIGKILL of the worker's or the head's engine from its
  recorded member identity (MN5, MN6), host B's agent stopped and restarted
  through the harness's role control while Ready (MN7: `uncertain` with the
  charge held, then settled), and the group alternating with a single-rank
  deployment on host A under `start --evict` (MN8). Every row starts with a
  read-only preflight (both hosts online with `engine_groups`, each host's
  direct-link address as its peer address, one build fingerprint for the
  engine's profile on both hosts, the checkpoint on both with the same files)
  and cleans up on failure through the row trap; MN7's trap restarts host B's
  agent first.
- `grouplib.sh` (group status and ledger views, cleanup on both hosts),
  `gen_group.py` (fixtures `vg-30`, `sg-30`, `tg-n35`), `rdma_counters.py`
  with `tests/test_rdma_counters.py`, `ledger.py group` and `members`
  (read-only views of `group_plans`, `group_members` and member charges), and
  `fault.sh member`.
- `roles.sh host-doc` declares `resource_policy.groups` (peer address
  `HOST_<A|B>_DIRECT_ADDR`, rendezvous range `GROUP_RENDEZVOUS_PORTS`,
  default 25000-25099; `CAPYCTL_GROUPS=0` leaves it out) and, with
  `TENSORFOLD_VENV_DIR`, a `tensorfold` profile written as `engine add`
  writes one. The harness's engine process pattern now covers
  `bin/tensorfold serve`.

Checked: `bash -n` and `shellcheck -x` (v0.11.0) on the rows and changed
scripts, a dry run of every row, `python3 -m unittest discover -s
scripts/live/matrix/tests`, `test_bench.py`, and the three fixtures resolved
by `capyctl validate config --host … --host …` against generated host
documents. These are harness checks, not evidence: CPU and Fake-engine tests
are not qualification; the rows MN1-MN8 on the two hosts are.

Final rerun, third session, on the qualifying build (the second session's build plus
three fixes: departed processes are never recorded, TensorFold probe ids from
`tensorfold.token_ids`, SGLang groups restart_only). Fresh run, every row passed:

- MN2 SGLang: Ready 225 s, 20/20, rdma.
- MN3 TensorFold: Ready 111 s on a cold kernel build (13 s warm), 20/20, rdma. Real
  canary token ids, and no text-evidence notice in the controller log.
  `TF_NCCL_LIB` on both ranks is the venv's
  `…/site-packages/nvidia/nccl/lib/libnccl.so.2`, read from each member's recorded
  process.
- MN5 SGLang and TensorFold, MN6 TensorFold: the Ready plan settles in 2–4 s at the
  killed rank, the relaunch is Ready (SGLang 223 s, TensorFold 12 s), answers, and
  stops clean. SGLang's transient child lands in the host journal's
  `departed_processes`, not the record.
- MN4 SGLang, restart_only: status reads `restart_only`; a park stops the group in
  4 s with nothing left on either host; a request brings it back on plan 2 (217 s).
  `residency: deep` on the same fixture is refused at deploy
  `capability_missing:deep_park`, and the message names `restart_only`.
- MN1 vLLM sanity: Ready 110 s, 20/20, rdma.

MN4 vLLM, MN5/MN6 vLLM, MN6 SGLang, MN7 and MN8 pass from the second session (below)
and were not rerun.

Second session, 2026-10-15, on the 0.1.3 build before those three fixes: origin/main
with #124 and the
group fixes (TF_NCCL_LIB, unreachable member, orphan worker, deep park, stable role
names, unrecorded-member settlement). Evidence is under `target/live/matrix/MN*/`, the
summary in `target/live/matrix/0.1.3-summary.md`. vLLM 0.30.0 with one build
fingerprint on both hosts, SGLang 0.5.21, TensorFold 0.6.5.

- Pass:
  - MN1 vLLM (Ready 120 s, 20/20) and MN2 SGLang (Ready 252 s, 20/20), both
    `transport: rdma` by RDMA operation counters on `roceP2p1s0f1` and
    `rocep1s0f1` on both hosts, memory back after stop.
  - MN4 vLLM: five deep park and wake cycles. Parked in 10 s, woken in 49–74 s,
    87–90 % of the Ready drop released on each host, each member charged its
    measured residue (about 3.4 GB), the canary's reference tokens equal every
    cycle, 5 `parked` and 5 `restored`.
  - MN5 vLLM and MN6 vLLM/SGLang: the Ready plan settles in 2–7 s with the killed
    rank as `failed_rank`, the group relaunches under `recovery: reconcile`,
    answers, and stops with nothing left on either host.
  - MN7 vLLM: rank 1 `uncertain` and still charged while host B's agent is down,
    then back and serving.
  - MN8 vLLM: two `--evict` alternations.
- Fixed during this session, verified live: a SGLang worker's transient child gave
  the tree two `worker-0` roles, so its identities were never recorded and the
  member stayed `uncertain` with 54 GB charged. After a restart on the same state
  with the fix, it settled within 12 s and the charge was released.
- Product failures (owner decisions 2026-10-15 in brackets):
  - MN4 SGLang: the park completes, but the wake fails. The head reloads its weights
    from disk (186–222 s); within seconds of its reply the restore is
    `restore_uncertain` and the group stops (two runs). [SGLang groups resolve
    `restart_only` and refuse deep for 0.1.3.]
  - MN5 SGLang: the relaunched plan recorded a transient child that had already
    exited, so the member failed right after Ready and the group relaunched again.
    [Being fixed: record only processes alive at report.]
  - TensorFold (MN3, MN5, MN6): both ranks load and serve, but readiness fails
    because TensorFold's `/v1/completions` answers no `token_ids`. [Being fixed:
    text probe.]
- Harness fixes from both sessions:
  - roles run under their own tmux server (an older one passed memlock 8 MiB, and
    NCCL failed);
  - configurable loopback ports for the run server;
  - MN5/MN6 judge the Ready plan and accept the reconcile relaunch;
  - TensorFold group members declare 40 GiB GPU (TensorFold's TP 2 startup estimate
    is 32.49 GiB, so 30 GiB + 2 GiB refused every window);
  - `fault.sh member` picks a group worker's spawned process now that its whole
    tree is recorded;
  - the SGLang group fixture declares no residency, and MN4 SGLang is the
    restart_only check; MN1-MN3 record each member's collective environment
    (`TF_NCCL_LIB`, …) with `proc_probe.py`.
- First session (an earlier build): TensorFold failed on libnccl, MN4 stayed
  `parking`, MN5 vLLM left an orphan `VLLM::Worker_TP1` and stayed charged, and MN7
  never showed `uncertain`. The group fixes above address all four.
- Deviation from the fault rule (first session, MN5 vLLM, 2026-10-15 about 10:30Z):
  - What was sent: to free host B after the stuck member, `signal_owned.py` sent TERM
    and then KILL to `VLLM::Worker_TP1` (pid 3037932), and TERM to its resource
    tracker (pid 3037931).
  - Why it was a deviation: neither pid was in the ledger. Ownership was inferred from
    their recorded parent (the host role's pid file) and matched by pid, start ticks
    and boot id. AGENTS.md allows signals only to ledger-recorded identities, so this
    should not have been done.
  - Evidence: `MN5-vllm.prev-20261015T195859Z/recovery-orphan-term.txt`.
  - In the second and third sessions no process outside the ledger was signalled;
    a row that would need it records the process and stops instead.

## A group's deep park completes on every member's own evidence — 2026-10-15 (branch `fix/group-deep-park`)

Found live at MN4 (vLLM 0.30.0 and SGLang 0.5.21, TP2 over two GB10 hosts): the head's collective succeeded (`POST /sleep?level=2`, `POST /release_memory_occupation`, both 200), yet the group stayed `parking` for the whole 15-minute park step, then `park_uncertain`, `residency_handed_to_stop`, the park failing `resolved_by_owned_cleanup`. Owner decision 2026-10-15, recorded as ADR 0028's amendment of that date. Three causes:

- A group worker's Launch reported its process tree one second after the spawn, before the engine forked its rank process (vLLM's `VLLM::Worker_TP1`, SGLang's rank scheduler), so the member's recorded processes were the leader alone (MN4 rank 1: one process; the GPU-holding `Worker pid=3000772` never recorded). Its host's `process_residency` named none of them (vLLM) and its saver enrollment was refused `owner_not_a_launch_member` (SGLang; `failed_rank` null, silent). The worker's Launch now waits, bounded by its deadline, until its tree holds one process per local rank besides the spawned one (none for a TensorFold follower), then settles as before (`capyctl-adapters` `group::worker_spawned`). The same record is what cleanup and supervision act on. Found live with that wait: SGLang's worker forks a short-lived child before its rank processes. At MN2 each scan named children `worker-<i>` by start order among those alive, so the rank process was journaled as a second `worker-0` beside the gone child and the controller refused the member's identities (`mark_member_launched`: Plan); at MN5 the gone child was one of the member's recorded processes, so supervision read its exit as the member's and failed the group (`failed_rank` 1) right after Ready. Now a launch step's own scans first move a journaled process that is no longer seen and is proven gone by identity out of the launch's recorded processes into `departed_processes` (history only; the spawned leader and anything of unknown presence stay recorded), and then a recorded process keeps its role on every scan while a new one takes the next index of its kind (`journal::depart_gone`, `journal::stable_roles`, persisted, so a restarted host role names them the same). The Launch reply, the controller's record and supervision name only processes alive when the step reported, with distinct roles, and a head's recorded workers stay `worker-0..n` with no gap.
- A vLLM member was judged on its processes' device memory plus anonymous pages against its 2 GiB parked placeholder, which a parked vLLM never meets on a unified host (MN4 vLLM: `failed_rank` 0, `group_member_failed`). Park and wake are now judged on the device memory the driver attributes to the member's processes (what the collective releases); SGLang stays on its saver map.
- The coordinator waited for member evidence until the step's deadline. It now waits a bounded window after the head's reply (60 s): a member still resident fails the group (`group_member_failed`), a member with no evidence of its own is `group_member_uncertain`; charges are retained either way and the group stop settles each member on its own host's evidence.

Each member's park also needs the residue its own processes hold in a host sample taken after the head's reply; for a revision whose phases CapyCTL derived, it is recorded for the revision on that member's host and for the launch (ADR 0014 amendments A13, A19) and the member is charged it instead of the placeholder (never above its Ready charge). A19's growth bound applies per member against its own host's `parked_growth_limit`. Declared `resources` keep their declared parked phase.

Tests (T20, T22, T31, T32): `a_worker_reports_its_rank_process_forked_after_its_imports` and `a_worker_without_its_ranks_by_the_deadline_is_uncertain` (adapters); `a_child_after_a_gone_transient_one_never_takes_its_role` (agent journal) and `a_gone_transient_child_is_never_a_recorded_process_of_the_member` (agent, a fake worker forking a child that exits before its rank process: the reply and the supervised processes are all alive with distinct roles; failed before with the gone child in the reply, as live); `a_park_settles_on_released_device_memory_and_charges_each_members_residue` and `a_member_without_evidence_of_its_own_fails_the_park_within_the_window` in the group world, for vLLM and SGLang. CPU and Fake-engine tests are not qualification. Live check outstanding: MN2 and MN5 SGLang (the group reaches Ready and stays Ready; the member's recorded identities hold no gone process, and host B's journal `departed_processes` holds the transient child), and MN4 vLLM and SGLang; the park reaches `parked` within about a minute of the head's call, each member charged its measured residue (status `held.member_owners` footprints above 2 GiB), the wake and canary pass, and no `owner_not_a_launch_member` appears in host B's log.

## A killed group worker's unrecorded processes are stopped, and a member with no recorded identities settles — 2026-10-15 (branch `fix/group-orphan-worker`)

Found live in MN5 vLLM (0.1.3 matrix, run matrix-20261015T071325Z): SIGKILL of the
worker member's vLLM process (rank 1 on host B) left its `VLLM::Worker_TP1` (38 GiB of
device memory) and its resource tracker running, reparented to the host role. Rank 1
stayed `uncertain` with its 52.9 GB charge held, the deployment stayed `stopping` for
more than 1 h 45 min, `start` answered `still_stopping`, `delete` answered
`delete_requires_cleanup`, and host B logged `host control session ended: a local
effect failed; reconnecting` 15 times.

Cause. A group worker's recorded tree is the one `worker_spawned` reports once the
tree is unchanged for one second after the spawn; vLLM forks its TP worker about four
seconds later, so rank 1 recorded only its leader (`members-vg-30-ready.json`). The
leader's launcher reaped it at once, and a leaderless process group never authorized a
signal (ADR 0028 §8, §11: its id can name an unrelated group once the pid is reused).
Each `Terminate` (one per stop retry) therefore signalled only the dead leader and
ended in `recorded processes gone but the group still has members`, which the host's
Terminate effect turns into a session end (the 15 reconnects); no gone evidence was
ever answered. The stop's retries ended with its deadline (the last `cleanup_retried`
at 08:39:48), so nothing asked again after the orphans were killed by hand; a fresh
`stop` is refused `still_stopping` and host-role restarts only reconcile a journal
whose launch claim was still retained.

Fix (ADR 0028 amendment of 2026-10-15, SPEC §13.2): the durable launcher holds each
launch's leader unreaped (`waitid` with `WNOWAIT`) until no other process of its group
runs, which pins the group id; `terminate_escalating` signals the whole group through
a held leader, alive or dead, so the unrecorded worker gets the group's SIGTERM and
SIGKILL, and the Terminate answers gone evidence once every recorded identity is gone
and the group is empty. A held leader that has exited reads gone, and its exit is
reported as before with the status the wait observed. A leader this role did not spawn
(launched before a restart of the host role) is not held, and its leaderless group is
still never signalled: that case stays uncertain and charged. Recording the worker's
whole tree at launch is a separate change (MN4 park).

Tests (T31, T33). `capyctl-launchers`
`an_unrecorded_worker_of_a_dead_held_leader_is_terminated` fails on `main` (the
Terminate errors) and passes now; `an_unheld_leaderless_group_is_never_signalled`
guards the unheld case and passes on both. `capyctl-agent` `group_launch`
`a_dead_worker_leaders_unrecorded_child_is_terminated_and_the_stop_settles` replaces
`late_worker_children_are_recorded_and_unrecorded_survivors_stay_uncertain`: it fails
on `main` (the Terminate ends the session) and passes now, with gone evidence for
exactly the recorded identities and the claim released. CPU and Fake-engine tests are
not qualification. Live check still needed: rerun MN5 vLLM; after the rank-1 kill, the
plan should settle within seconds with `failed_rank 1`, no `VLLM::Worker_TP1` or
resource tracker left on host B, no `a local effect failed` in host B's log, and the
member's charge released.

Found live in MN2 SGLang TP2 (diagnosis `MN2/diag-*`): host B's Launch reply for rank
1 named two processes under one role (`worker-0` twice in its journal; that naming
bug is fixed separately), so `mark_member_launched` refused it and the member stayed
dispatched with no identities. `settle_member` settles a dispatched member only on
its recorded identities, and the stop's own fallback recorded the Terminate reply's
identities first through the same refused call, so rank 1 stayed `uncertain` with its
54 GB cold charge and a worker lease held, although host B had answered 54 Terminates
and no engine process remained. Any dispatched member whose identities were never
recorded (a lost, refused or unrecordable Launch reply) dead-ended the same way.

Fix (ADR 0028 amendment of 2026-10-15, R23, SPEC §11): such a member now settles on
its own host's evidence for the Launch it was dispatched with. The Terminate reply
already carries it: the owned handle (that Launch's command id), every process the
host's journal recorded for it with its presence, and whether the claim is retained.
The controller passes it to a new store call, `settle_unrecorded_member`, only when
every process is gone and the claim is released. The store checks the member's
host, that it was dispatched with no identities recorded, that the handle is the
Launch it was dispatched with, and that the evidence names at least one process (pid,
boot id, start ticks, one boot, none twice; roles may repeat). It then releases the
charge and records those processes. Anything else, an empty journal included, keeps
the member uncertain and charged. The protocol does not change: the fields are the
ones every agent already sends, so older hosts give the same evidence.

Tests (T30, T31, T33). `capyctl-controller` group world
`an_unrecorded_member_settles_on_its_hosts_launch_evidence` fails on `main` (the
member never settles; timed out) and passes now;
`an_unrecorded_member_without_launch_evidence_stays_charged` (the host came back
with an empty journal) passes on both. `capyctl-store`
`an_unrecorded_member_settles_only_on_evidence_for_its_own_launch` covers the
refusals (another Launch, another host, no process, a process twice, two boots) and
the settlement. Live check still needed: rerun MN2; a worker member whose identities
were not recorded should settle on the stop's first Terminate, its charge and worker
lease released.

## TensorFold group members get the installation's libnccl — 2026-10-15 (branch `fix/group-tf-nccl`)

Found live 2026-10-15 (MN3): both ranks of a TensorFold 0.6.5 TP2 group
exited `RuntimeError: libnccl not found (set TF_NCCL_LIB)`. TensorFold 0.6.5
and 0.6.6 (`tensorfold/cuda/comm.py` `_library`) load libnccl from the file
`TF_NCCL_LIB` names, else `find_library("nccl")`, system `/usr/lib/*` and
`dist-packages`, else torch's `lib`; a pip venv keeps it in the nvidia-nccl
wheel (`<venv>/lib/python3*/site-packages/nvidia/nccl/lib/libnccl.so.2`),
which none of those reach. CapyCTL owned `TF_NCCL_LIB` (ADR 0028 §2.1) and
stripped it, but never set it; ADR 0028 §10's note that TensorFold's NCCL is
built in was wrong.

Now the TensorFold Initialize step of a group member resolves the
installation's library from the registered executable's venv (the wheel's
`libnccl.so.2`, else torch's `lib/libnccl*.so*`) and renders it as
`TF_NCCL_LIB` on both ranks, inside the closed environment; the agent's own
value never passes through. With no library it refuses before any effect with
the closed category `nccl_library_missing` and an operator hint. Single-rank
launches are unchanged.

Tests (`capyctl-adapters`): `both_group_ranks_start_with_the_installations_libnccl`
and `a_group_member_without_libnccl_is_refused_before_any_effect` fail before
(no `TF_NCCL_LIB`, the launch spawns) and pass after;
`no_transport_variables_and_no_credentials` now asserts the rendered value on
both ranks. The agent's `group_launch` fake venv now holds the wheel's
`libnccl.so.2`, as a real installation does. CPU and Fake-engine tests are not qualification; live check still
needed: MN3 and MN5 tensorfold rows must reach Ready.

## A group member on an unreachable host reads uncertain — 2026-10-15 (branch `fix/group-unreachable-member`)

ADR 0028 §11 says a member whose host cannot be reached stays charged and
`uncertain` (`group_member_uncertain`). The 0.1.3 live row MN7 (vLLM TP2 across
two GB10 hosts) stopped host B's agent: host B showed offline at once, but
rank 1 stayed `launched` for seven minutes (`13-uncertain-check.out`,
`18-held-check.out`). Nothing moved a member on session loss. Only a stop, a
silent park or an unanswered Launch marked a member uncertain, and a
single-rank instance only gets its dispatch closed (`reconciling`).

The readiness supervisor (`remote_readiness.rs`) already runs a pass whenever
a host session changes, and every 250 ms. Each pass now also visits every
launched or uncertain member of an active group plan:

- A `launched` member whose host has no current session (lost, draining, or
  silent past the heartbeat bound) becomes `uncertain`
  (`Store::mark_member_unreachable`). Its charge, dispatch fence and recorded
  identities are kept, and nothing is released.
- An `uncertain` member becomes `launched` again only when its host's
  reconciled session replayed a journal that still claims the member's Launch
  (`AgentSessions::claims_launch`, the session's `ReconcileHistory`), through
  `Store::mark_member_reconciled`. The store refuses this when another path
  owns the uncertainty: no identities recorded (an unanswered Launch), a
  recorded group failure, a stop run in progress, or an instance status that
  names an unsettled stop. A host back with an empty journal proves nothing,
  so its member stays uncertain until a stop settles it on evidence. An
  engine that exited while its host was away is reported again once the
  member is back to `launched`, and that exit fails the group as any exit
  does.

Tests (T30, T32, `coordinator/tests_groups.rs`, which drives the production
supervisor over the group world):
`a_lost_member_host_reads_uncertain_until_its_journal_returns`,
`a_member_host_back_with_an_empty_journal_stays_uncertain` and
`a_member_that_exited_while_its_host_was_away_fails_the_group_on_return`.
All three fail without the member pass and pass with it. CPU and Fake-engine
tests are not qualification. Live check still needed: rerun MN7 (vLLM) and
check that rank 1 reads `uncertain` with its charge held within a second or so
of host B's agent stopping, then reads `launched` after the agent returns,
and that the group still stops cleanly.

## SGLang groups restart instead of parking — 2026-10-15 (branch `fix/sglang-group-restart-only`)

Found live at MN4 (SGLang 0.5.21, TP2 across two GB10 hosts): a group deep park completes, but the first wake fails. The head reloaded weights from disk for 222 s, the wake canary ran, and 3 s later the wake went `restore_uncertain` and the group was stopped. It fails safe, but group wake is not usable. vLLM groups pass 5/5 deep park cycles. Owner decision 2026-10-15, recorded as ADR 0028's note of that date.

- `group_support("sglang").deep_park` is now false (`crates/capyctl-config/src/group_support.rs`). `check_engine_shape` refuses every parking tier (`deep` and `host_backed`) for an engine whose group cannot park, with `capability_missing:deep_park: a sglang group cannot park (residency `deep`); declare `residency: restart_only``.
- `resolve_effective_with_checkpoint` (`crates/capyctl-config/src/effective.rs`) defaults the residency of a member of a group of more than one host to `restart_only` when its engine's group cannot park (the group's shape, else the member's `member_of`).
- Single-host SGLang and vLLM groups are unchanged; single-host identities are untouched.
- `docs/guide/engines.md` (groups table, known issue) and `docs/guide/several-machines.md` (park and wake) say SGLang groups restart instead of parking in 0.1.3.

Tests (T03, T22), in `crates/capyctl-config/tests/topology.rs`: `sglang_group_resolves_restart_only_and_refuses_parking` and `engine_shape_support` failed before; `vllm_group_resolves_with_default_residency` (now asserting `deep`) and `single_host_sglang_still_parks` pass before and after. Controller group world (`crates/capyctl-controller/src/coordinator/tests_groups.rs`): SGLang groups deploy `restart_only`; `sglang_deep_group_with_differing_paths_is_refused` now expects the deploy refusal; the three deep group park tests run on vLLM only. CPU tests only; not qualification. Live recheck: redeploy the MN4 SGLang group with no residency and confirm `restart_only` in status and that a park stops and a wake starts the group; with `residency: deep` confirm the deploy refusal.

## TensorFold group readiness reads TensorFold's token ids — 2026-10-15 (branch `fix/tensorfold-group-probe`)

Found live 2026-10-15 (MN3, MN5, MN6): with libnccl fixed, both ranks of a
TensorFold 0.6.5 TP2 group loaded, then activation failed `group_member_failed:
rank 0: the completion probe through the head failed: the head generated no
token`. TensorFold 0.6.5 (609ca419) and 0.6.6 (cb2ebf05) honor
`return_token_ids` on `/v1/completions`, but answer the ids in the response's own
`tensorfold` block (`tensorfold.token_ids`, `src/tensorfold/cuda/server.py:558`,
`cuda/http.py:240-242`), never in `choices[0].token_ids`, where CapyCTL read
them. The head answered text with no ids found, and a group probe needs ids
(ADR 0028 §9).

The `TypeError: cannot unpack non-iterable NoneType object` lines in the rows'
error captures are unrelated to the probe. Each one ends a process's `main()`
(`raise SystemExit(main())`). The capture greps the whole run directory, so its
lines come from every row run there so far, carry no timestamps, and also
appear in the vLLM and SGLang rows' captures (MN4-vllm and MN4-sglang of the
07:13 run). A probe request cannot produce such a trace: TensorFold's request
handler turns any exception into a 500 answer and logs
`[tensorfold] request error`. Neither appeared, and the probe got an answer
(text without ids). No request field CapyCTL sends causes the crash, so
`return_token_ids` stays in the request.

Owner decision 2026-10-15, ADR 0028 §9 amended:

- The completion probe reads TensorFold's ids from `tensorfold.token_ids`,
  falling back to `choices[0].token_ids` (`completion_probe::tensorfold_answer`,
  the one reader of TensorFold completion ids: group readiness, the
  request-stall check and the single-launch `complete_probe` all use it).
  TensorFold group readiness is now judged on token ids, as vLLM's and SGLang's
  is.
- If a TensorFold head's answer carries no ids in either place (a build without
  them in its block), the group readiness, stall and re-proof checks accept its
  non-empty generated text (`group_activation::HeadEvidence::Text`). The
  controller logs a notice naming the deployment when it does. A vLLM or SGLang
  head without ids still fails as before. The wake canary compares ids only
  (`HeadEvidence::into_tokens`): a canary answered with text alone fails the
  wake and records no reference. TensorFold groups are restart-only and never
  wake.

Tests: adapters `tensorfold_asks_for_token_ids_without_a_key` (TensorFold's
0.6.5/0.6.6 response shape, ids in the block) fails before (`tokens: []`) and
passes after; the new unit test `tensorfold_ids_come_from_its_own_block` pins
both shapes (with the block's ids, and without them, which answers text only)
plus the choice fallback and malformed ids. Controller
`tensorfold_readiness_passes_on_text_without_token_ids` (a TensorFold-shaped
fake head answering text and no ids) fails before (the activation never reaches
Ready) and passes after. `vllm_and_sglang_readiness_still_needs_token_ids`
passes before and after. CPU and Fake-engine tests are not qualification. Live
check still needed: the MN3, MN5 and MN6 tensorfold rows must reach Ready, with
no TensorFold text-evidence notice in the controller log (the 0.6.5 head
answers ids).

## A sizing refusal leads with the request to raise to — 2026-10-15 (branch `fix/refusal-hint-first`)

Found live on host A (B18, Qwen3.8-27B with an EAGLE MTP draft and
`--max-mamba-cache-size 60` at a 65 GiB request): the SGLang sizing refusal
(owner decision 2026-10-14) reached `start --wait` and status cut off at
`... more than the 69793218560-byte memory request; raise engin`. Status bounds
an operation's reason to 512 bytes (SPEC §6.4,
`capyctl_domain::diagnostics::MAX_REASON_BYTES`), and the refusal named the
request to raise to last, so the bound removed the one part the operator acts
on.

- Both SGLang sizing refusals (`crates/capyctl-config/src/context_fit/sglang_pool.rs`:
  arguments that fix the state pool, and a hybrid model's recurrent state
  that does not fit) now lead with the request to raise to, then give every
  figure: `raise engine_config.memory.request to "75GiB" (80530636800 bytes),
  keeping memory.kv_cache at 31138512896 bytes: SGLang's static pool holds ...`.
  The live case names 75 GiB (71163366912 + 8589934592 = 79753301504 bytes,
  rounded up to whole GiB).
- A bounded reason is cut on a character boundary and ends in a visible `…`,
  within its bound (`diagnostics::bound_bytes`, `bound_chars`): status's
  operation reason, the unusable-checkpoint diagnostic, a parked instance's
  release reason, a switch's detail, and the live-profile and retirement
  refusals sent to a host. Status picks an operation's hint from the whole
  reason before it bounds it (`diagnostics::public_text`), so a closed code
  past the bound still gives the hint.

Tests (failing before, passing after):
`the_live_eagle_refusal_leads_with_the_request_to_raise` (capyctl-config, the
live figures; the request named is the smallest whole GiB that holds them),
`a_refusal_names_the_smallest_request_that_fits` (now asserts the request
leads), `a_cut_reason_ends_in_an_ellipsis_and_keeps_its_lead`
(capyctl-domain), `a_bounded_reason_keeps_the_request_to_raise_and_shows_the_cut`
(capyctl-store: status keeps the request, ends in `…` within 512 bytes, and
classifies a code past the bound). `wait_ends_at_once_when_the_start_gave_up`
(capyctl-cli) and `a_deterministic_refusal_gives_up_without_retrying`
(capyctl-controller) use the new order. CPU and Fake-engine tests only; they
are not qualification. Live check still needed: repeat the B18 start on host A
and confirm `start --wait` and status name `75GiB`.

## 0.1.3 release live checks driver — 2026-10-15 (branch `test/release-checks-0.1.3`)

A driver for the live checks the changes merged since 0.1.2 still owe (B1-B17
of the 0.1.3 live backlog), on one GB10 host: `scripts/live/release-checks/run.sh`,
with `remote.sh` (host phases, run detached) and `rc.py` (requests, reads and
the verdict, standard library only). Modelled on `scripts/live/engine-quick-check.sh`
and the B18 comparison driver: CapyCTL built on the host into
`~/capyctl-0.1.3-live`, the checks run in `~/c13/<run>` (a short path, since
deep parking's observation socket lives under the state directory), results
copied back to `target/live/0.1.3-backlog/<run>/` with host names redacted,
and the host's work directory removed. `--dry-run` prints the whole plan,
every deployment file and each check's pass condition; `--only` runs a subset.

Each group of checks runs on its own standalone role with its own state,
config and model-sources directories and port (18643, management 18644, engines
8340-8359), each profile with empty kernel caches of its own; the main role
raises `managed_limit` to 100GiB with a 10GiB `free_reserve` and a 45GiB
`parked_limit`. Deployments are shared to keep cold starts down: vLLM 0.30.0
model 1 with no declared loader (B1, B2's three GB10 wakes, B3, B5, B14), its
eager variant (B4), TensorFold 0.6.6 model 1 (B3 with the CUDA `prefill_ms`
mapping, B14's refusal), SGLang 0.5.21 Qwen3-4B (B3, B5, B6, B14; B8 with
explicit activation), SGLang model 1 (B9's two small-KV launches, B15, B12 with
`model.draft`), Qwen2.5-0.5B downloaded by CapyCTL (B11, and the tar payloads
B13 serves from a loopback `python3 -m http.server` to a role that approves
plain HTTP for that origin only). B7 has its own role (110GiB managed, 11GiB
reserve). Every field the checks assert was read from the code: the effective
configuration (`inspect deployment --effective-config`), status
(`parked.growth[]`, `checkpoint_digest.provenance`, `model_sources[]`,
`activation`), `list hosts` (`session.memory_warnings`), the load read, the
engine-log tail route, the role log's `router_selection` events, the
`engine build limits: MAX_JOBS=` line, and the ledger's `wake_canaries` and
`journal_entries` (read-only SELECTs). Engine keys are never read: the log
redaction check is by rule, by the role's own key and token, and by a unique
prompt marker. Every role, deployment, profile, HTTP server and container is
removed at the end, after a failure too.

SKIP by default: B10 (the host's Flash-Next copy is not a complete checkpoint;
`--download-flash-next` lets CapyCTL download it into `~/models/sources`), B16
(no gpt-oss-120b on a host) and B17 (`--container` builds an image on the host).
B18 has its own driver in capyctl-recipes.

Checked: `bash -n`, `shellcheck -x` (v0.11.0) clean, the dry run, and two live
runs on host A (below).

Live runs on host A (evidence in `target/live/0.1.3-backlog/<run>/`):

- 2026-10-14 full run (`2026-10-14-2311-release-checks`): B5, B6, B7, B8 and
  B11 pass; B10, B16 and B17 SKIP (the reasons above). B1/B2 failed from a
  product bug, vLLM deep wake (fixed in #121). B3/B14 TensorFold, B4, B9, B12,
  B13, B14 vLLM and B15 failed on harness faults, fixed below; B3 vLLM/SGLang
  and B14 SGLang passed.
- 2026-10-15 rerun of B1-B4, B9 and B12-B15 (`2026-10-15-0133-release-checks`,
  with #121 and #122): every row passes except B12's, which read an empty draft
  path while the engine's server_args named the drafter in the role's sources
  store (`src1/sources/huggingface/z-lab--Qwen3.8-27B-DFlash2@50307d4c...`),
  sources verified, request answered; the same evidence passes under the fixed
  B12 below.

So every check passes, or is SKIP for B10, B16 and B17.

Harness fixes after the first run on host A (2026-10-14, evidence in
`target/live/0.1.3-backlog/2026-10-14-2311-release-checks/`):

- B3/B14 TensorFold: TensorFold 0.6.6 needs a drafter for Qwen3.8-27B; the
  deployment passes `--drafter ~/drafters/Qwen3.8-27B-DFlash2` (the `tf66`
  profile approves `--drafter` and `~/drafters`, as the quick check does) with
  the quick check's 60GiB reservation; B3 still requires `prefill_ms` source
  engine (the CUDA `prefill_s` mapping).
- B9/B15/B12: SGLang `quantization: modelopt` with deep parking is refused
  `capability_missing:deep_park` by design; these deployments declare
  `residency: restart_only`. B12 asks for the 67486MiB `memory.request` CapyCTL
  named when it refused 65GiB, and retries once at whatever size a refusal names.
- B13: GNU tar's `--format=posix` writes a pax extended header per member, and
  CapyCTL's tar reader (ADR 0008, `crates/capyctl-agent/src/sources/tar.rs`)
  refuses extension headers and links as `unsafe_archive`. The payloads are
  ustar with symlinks dereferenced (`-h`) and relative member names. The
  secret URL's `sig` marker is looked for in the whole role state directory
  (ledger, logs, config; the secrets directory excepted) besides the CLI
  output and status/inspect JSON.
- B14 vLLM: the cache_salt check runs on dense Qwen3-4B (`restart_only`); the
  hybrid model reuses no prefix cache.
- B4: its own role with `resource_policy.parked_growth_limit: 0B` (ADR 0014
  amendment A19; a park is past the bound only when its charge exceeds the
  first by more than the bound, so 0B makes any growth a stop). Zero growth
  over every park is SKIP `n/a (no growth in N parks ...)` with the per-park
  first/last bytes, not FAIL.
- B1: the gating rows check whatever loader `capyctl default` provenance
  reports (lazy or eager) and that the engine argv carries it; a second
  deployment with `safetensors_load_strategy: lazy` stated runs the cycles as
  an `info` row (`rc.py verdict` never lets `info` decide a check; a wake gives
  up after 10 minutes and stops the variant).
- B12 (after the rerun): SGLang 0.5.21 logs `server_args` as a dict
  (`'speculative_draft_model_path': '...'`), not `ServerArgs(key='...')`;
  `rc.py facts` reads both forms, and B12 asserts the rendered path is the
  pinned `z-lab--Qwen3.8-27B-DFlash2@<commit>` directory under the main role's
  sources directory (`--draft-source local`: the local drafter path).

## A wake that does not complete stops the instance — 2026-10-15 (branch `fix/vllm-deep-wake`)

Found live on host A (GB10, standalone, vLLM 0.30.0, Qwen3.8-27B NVFP4, deep park, no
declared loader, so `lazy` under ADR 0014 amendment A21; run
`target/live/0.1.3-backlog/2026-10-14-2311-release-checks/`, check B1). The first wake's
`/wake_up?tags=weights` returned at once, then vLLM's `Reloading weights inplace` took
123 s. CapyCTL's `reload_weights` collective had a fixed 120 s bound
(`WEIGHT_RELOAD_TIMEOUT_SECS`), so at 121 s the step failed as uncertain (`collective_rpc:
error sending request`) and the request got `activation_uncertain`. The engine finished the
reload 2 s later and was never asked anything again. An uncertain restore is never re-armed
and nothing stopped it, so the instance read `uncertain` with its restore `running`, its
memory charged and dispatch closed, for 21 minutes until the script deleted the deployment;
the next two wakes failed at once on it.

vLLM logged `WARNING [layerwise.py:268] MRotaryEmbedding: Failed to load weights` during that
reload. It is not evidence of a failed reload: the six correct `eager` wakes of check B4 on
the same model log it too. In vLLM 0.30 it is `finalize_layerwise_processing` putting a
layer's kernel tensors back when the checkpoint holds none of its weights, and
`reload_weights` returns `None` either way, so the collective's answer carries no load
result. CapyCTL does not scrape engine logs for it. The structured signal vLLM does give is
the collective's status: when `reload_weights` raises, `/collective_rpc` answers an error.

What changed (SPEC §9.1, §13.2, T20; the 2026-10-09 owner decision for `wake_mismatch`,
extended):

- **One deadline for the whole restore.** The persisted restore was already bounded by its
  wake window (`timeouts.wake`, under the Initialize ceiling), but the reload had its own
  shorter, fixed 120 s bound and the wake canary was bounded only by the adapter honouring
  the bound it was passed. Now vLLM's persisted `ReloadWeights` step gives the collective
  what is left of the restore's deadline (`EngineHttp::collective_rpc(timeout)`), and the
  coordinator runs the canary under the same deadline (`execute_residency`); the canary at
  first readiness is bounded by `CANARY_DEADLINE` in the coordinator too. The adapter-level
  `EngineAdapter::reload_weights` keeps the 120 s bound.
- **A failed reload fails the wake.** A `reload_weights` collective vLLM answers with an
  error status is `RuntimeError::ReloadFailed` (new): the wake fails with the new closed code
  `wake_reload_failed`, and the adapter refuses every later step, as before.
- **A wake that ended without its evidence stops the instance.** Every single launch's wake
  that ends uncertain (deadline passed, a step's outcome unknown, canary unanswered, evidence
  not accepted, the controller stopped during it) records `wake_uncertain` as the instance's
  last error (`Store::record_wake_failure`), unless it is `wake_reload_failed` or
  `wake_mismatch`, and the step reads uncertain. `Store::wake_stops_due` (replacing
  `wake_mismatch_stops_due`) lists every single-launch instance whose restore at its current
  generation ended uncertain and that still reads desired ready, whatever the code, and the
  scheduler retries each one's ordinary stop every pass under `EXIT_PRINCIPAL`
  (`retry_wake_stops`, keyed `wake-stop:<operation>:<generation>`). As for `wake_mismatch`,
  the host terminates the recorded engine and the reservation, claim and gate are released
  only on its gone evidence; status reads `failed` with the code as last error, and the next
  request starts the instance fresh. A group's wake still owes its group stop
  (`group_residency_stops_due`); an uncertain park still waits for an operator stop.
- Remote hosts report every failed restore step as uncertain (the host protocol has no
  reload-failed category), so a remote reload failure reads `wake_uncertain`, with the same
  stop.
- New closed status codes `wake_reload_failed` and `wake_uncertain` (no exit; CLI codes,
  errors guide, parking guide).

Tests, failing before and passing after:

- `capyctl-controller`
  `tests::residency::a_wake_whose_reload_never_returns_is_stopped_and_starts_fresh`
  (embedded, the reload never answers), `a_wake_whose_canary_never_answers_is_stopped_and_starts_fresh`
  (the canary never answers) and `an_uncertain_wake_is_stopped_and_starts_fresh` (remote,
  outcome lost): on `origin/main` all three time out after 30 s waiting for the release (the
  wake stays uncertain and charged; with the canary, the worker hangs); now the instance is
  stopped once, released on the host's gone evidence, reads `stopped` with
  `wake_uncertain`, and the next on-demand request starts a fresh launch that reaches Ready.
- `capyctl-controller` `tests::residency::a_wake_whose_reload_fails_is_stopped_and_starts_fresh`
  and `capyctl-adapters` `vllm_residency::a_failed_reload_fails_the_wake_and_is_never_repeated`
  (renamed from `a_failed_reload_is_uncertain_and_never_repeated`; a 500 from
  `/collective_rpc` was uncertain before): new API, so they did not build before.
- `capyctl-store` `wake_canary::tests::an_uncertain_wake_owes_the_stop_until_a_new_generation`
  and `a_mismatch_names_its_code` (replacing `a_mismatch_owes_the_stop_while_its_wake_is_uncertain`).
- `capyctl-cli` `group_status_codes_have_no_exit` covers the two new codes.

CPU and Fake-engine tests only; they are not qualification. Live check still needed: on host
A, model 1 (Qwen3.8-27B NVFP4) on vLLM 0.30 with deep park and a declared
`safetensors_load_strategy: lazy`, park and wake three times. Each wake either answers
correctly (the reload now has the 180 s wake window), or ends within the wake window with
the instance stopped, `wake_uncertain` or `wake_reload_failed` as its last error, its memory
released once the engine is gone, and the next request starting it fresh. Never an instance
left `uncertain`.

## SGLang ReplaySSM spec-verify and MTP drafts sized as SGLang allocates them — 2026-10-14 (branch `fix/sglang-replayssm-state`)

Reported for a Qwen3.8-27B NVFP4 deployment with EAGLE (4 draft tokens,
`--speculative-eagle-topk 1`), `--enable-linear-replayssm-spec`,
`--max-mamba-cache-size 110`, `max_concurrent_requests: 22` and
`memory: {request: 79GiB, kv_cache: 29GiB}`: CapyCTL counted
`(22 + 1) × 4` intermediate recurrent states (13.2 GiB) that SGLang does not
keep under that flag, so the static pool it rendered was the whole request
and the margin was 0 (ADR 0014 amendment A14 and its note on SGLang's static
pool on unified memory).

SGLang 0.5.21 and 0.5.20 (both define the flag; their sizing is the same)
read from the published wheels: with the flag on a gated-delta-net or KDA
hybrid, `_handle_max_mamba_cache` (`kv_cache_configurator.py`) reserves no
intermediate states and charges a ReplaySSM ring for
`max_running_requests + 1` requests instead, and `MambaPool.__init__`
(`memory_pool.py`) allocates no `intermediate_ssm`, but allocates that ring
(per linear-attention layer and record, bfloat16 compact d and k with their
low parts and a float32 gate; 16 records unless `--linear-replayssm-cache-len`
says otherwise) and still allocates the deduplicated conv windows
(`draft tokens + kernel - 2` columns) for each running request plus one. The
flag also makes the temporal state float32 unless `--mamba-ssm-dtype` names
another dtype (`handle_linear_attn_backend`).

`crates/capyctl-config/src/context_fit/sglang_pool.rs` now counts that: the
state the arguments fix (`argument_state`) and the state CapyCTL sizes itself
hold the slots plus, for each running request plus one, either the
intermediate states (no flag, unchanged) or the ring and the conv windows
(flag), and the temporal state is float32 under the flag. For the reported
deployment that is 0.67 GiB instead of 13.2 GiB, and the static pool is the
request less its 8 GiB margin. The flag is recognised as SGLang's parser takes
it: the full name or an abbreviation that is not `--enable-linear-replayssm`
(the decode ring, another option), in the deployment's or the host-fixed
arguments; it is a `store_true` flag, so `=true` (which SGLang refuses)
counts nothing. A derived request, whose resolution reads no checkpoint,
still reserves the intermediate states under the flag; they exceed the ring
at its default length, and the launch sizes what SGLang keeps within that
reservation. Deployments without the flag size and fingerprint as before.
ADR 0014's state formula notes the flag.

Tests (`sglang_pool/tests.rs`, T14): the reported deployment without the flag
counts its 111 slots and 92 intermediate states as before (it is now refused
at 79 GiB, see below) and with it renders a 71 GiB static pool, 475136 KV
tokens; at a tighter request the refusal counts exactly the
111 slots and 23 ring-and-window sets; an abbreviation, the host-fixed
arguments and a longer ring are counted, and `--enable-linear-replayssm` or
`=true` are not; CapyCTL's own sizing at 79 GiB was refused and now holds 22
running requests; the flag's float32 temporal state. Those three tests
failed before the change and pass after; a derived request's reservation
under the flag is pinned unchanged, and the existing pool, recipe-fingerprint
and golden effective-revision tests pass unchanged.

## An EAGLE draft from the checkpoint's own MTP head

Found live on host A on 2026-10-14 (SGLang 0.5.21, a standalone built from
this branch's first commit, d4b86843): the deployment above as
RadixArk/Qwen3.8-27B-NVFP4-BF16-LMHead at 009632fe (23749332688 bytes of
measured weights) with `kv_cache_dtype: fp8_e4m3`, EAGLE 3/1/4 drafting with
the checkpoint's MTP layer and `--enable-mixed-chunk`. CapyCTL passed 950272
KV tokens (29 GiB at 32 KiB) and a 71 GiB static pool; SGLang ran the 22
requests but held 770984 and 776670 KV tokens, and the process took 78.7 to
81.8 GiB at Ready against the 79 GiB request (an 80 GiB Ready charge with
the engine overhead). The engine log's `server_args` were not in the evidence;
the rendered figures are CapyCTL's sizing at d4b86843.

Cause, read in SGLang 0.5.21 and 0.5.20 (the same code): without
`--speculative-draft-model-path`, the draft worker loads the checkpoint as
`Qwen3_5ForCausalLMMTP`. It is built before the target's KV pool
(`Scheduler.init_model_worker`: the draft worker, then `init_memory_pools`),
so the target's profile (`_profile_available_bytes`) charges against the
static pool its MTP layer (in the measured weights) and its own BF16
embedding and LM head, 2 × 248320 × 5120 × 2 bytes = 4.74 GiB, which it frees
only after the target's pool exists (`init_lm_head`). Its KV pool, one
full-attention layer (`_build_hybrid_linear_kv_pool`, layer `[0]`) for the
target's token count in the target's KV dtype, is allocated after the
profile and outside it: the target's `eagle_draft_num_layers` comes from the
target configuration, which names no `num_nextn_predict_layers`. Modelled,
the 71 GiB static pool left the target 804866 tokens; the live figures sit
0.86 to 1.03 GiB below, the draft runner's own allocations, which no figure
here models.

CapyCTL now counts that draft (`EAGLE`, `EAGLE3` or `NEXTN` on a
gated-delta-net hybrid whose configuration names an MTP head, with no
separate draft model): its KV layer shares the KV cache, as a draft model's
layers do (amendment A9), so the tokens passed are 29 GiB / (32768 + 2048) =
894373, and the static pool holds its embedding and LM head (one table when
they are tied; `--speculative-draft-kv-cache-dtype` and `--dtype` apply), 75.44
GiB here, which leaves SGLang's profile 0.7 GiB beyond the target's pool even
with the GiB it was short live. When CapyCTL sizes the state itself the tables
count with the weights, so that deployment without `--max-mamba-cache-size`
is refused at 79 GiB, naming them, and holds its 22 requests at 82 GiB. The
tables are freed at start-up, so the margin gets them back. ADR 0014's rule
for the static pool on unified memory states it. A launch without speculative
decoding, with a separate draft model, or of a checkpoint without an MTP head
sizes as before.

## A stated request that cannot hold the state its arguments fix is refused

Owner decision 2026-10-14 (AGENTS.md: uncertainty must retain accounting):
until now the static pool CapyCTL rendered for a state fixed by
`--max-mamba-cache-size` was capped at the memory request, and what it lacked
came out of the margin unannounced: with the MTP draft counted, the deployment
above would have rendered 75.44 GiB of its 79 GiB, a 3.6 GiB margin,
under-charged. Now, when a stated (not derived) memory
request's static pool, the request less its family's margin (8 GiB on unified
memory, `(request − kv_cache) / 11` on a discrete GPU), cannot hold the
weights (with an MTP draft's tables), the KV cache, that state and, on
unified memory, the 3 GiB of SGLang's own allocations, CapyCTL refuses it as
it refuses a CapyCTL-sized state that does not fit: `status` and `validate`
warn beforehand and the start is refused (resolution reads no checkpoint, so
it cannot refuse earlier), naming each component, their total and the
smallest whole-GiB request that holds them. For the live deployment: 81000320720
bytes (75.44 GiB) with the 8 GiB margin, so `request: 84GiB`, which passes
894373 KV tokens and a 76 GiB static pool; 83 GiB is refused. Without
`--enable-linear-replayssm-spec` the same deployment (92 intermediate states)
needs 96 GiB. A derived request still takes what it lacks from its margin.
The pinned recipe, fingerprint and golden tests (`capyctl-config`,
`capyctl-adapters`, `capyctl-store`, `capyctl-cli`, `capyctl-controller`)
resolve unchanged; three unit tests that showed the capped static pool with a
stated request now show the refusal and the request it names
(`arguments_that_size_the_state_pool_win` moves its 48 GiB request to 64 GiB
so the 65 slots it fixes fit).

Tests (`sglang_pool/tests.rs`, T14):
`an_mtp_draft_keeps_its_kv_layer_and_tables_in_the_static_pool` reproduces the
live deployment from the checkpoint's configuration and safetensors headers
(the old figures and SGLang's profile of them, the new 894373 tokens and
static pool, `NEXTN`, `EAGLE3`, a draft KV dtype, the CapyCTL-sized refusal
and 82 GiB; the 79 GiB request refused naming 84 GiB, and 84 GiB passing
894373 tokens); `other_launches_of_an_mtp_checkpoint_size_as_before` pins the
unchanged cases and the tied tables. Both failed before the change and pass
after, and `arguments_that_fix_the_state_pool_are_held_by_the_static_pool`
pins the refusal's whole-GiB request and its boundary.

CPU and Fake-engine tests only; they are not qualification. Live check still
needed: launch the MTP deployment on SGLang 0.5.21 with `request: 84GiB` and
confirm the engine log shows `--max-total-tokens 894373` and
`max_total_num_tokens` 894373, the "Mamba Cache is allocated" and "GDN
ReplaySSM ring buffers allocated" lines (no `intermediate_ssm_state_cache`)
match the counted bytes, and the Ready footprint (expected 3.8 to 4.0 GiB
above the live run's 78.7 to 81.8 GiB) stays within the charge; and that at
79 GiB `status` warns and the start is refused naming 84 GiB.

## vLLM 0.31.0 verified — 2026-10-14 (branch `feat/verify-vllm-0.31.0`)

Owner decision 2026-10-14: vLLM 0.31.0 joins the verified set (ADR 0018 §1).
AGENTS.md already lists vLLM 0.31.0 on host A.

What changed: `(Engine::Vllm, "0.31.0")` is in `registration::VERIFIED`, so a
0.31.0 installation no longer lists as `CUSTOM yes`. `docs/guide/engines.md`
(custom builds), `docs/guide/install-engines.md` (versions this release knows)
and ADR 0018 §1 (dated note) name it.

Tests: `capyctl-config` `registration.rs` `the_verified_set_marks_custom_builds`
(T22; 0.31.0 verified, 0.31.1 not).

Live evidence:

- Laptop (RTX 4090, 2026-10-10 and 2026-10-14): see
  `docs/changes/vllm-0310-support.md` (launch, chat, `cached_tokens`, deep park,
  stop), and today's wake-canary check (two matching wakes, and a tampered
  reference failing `wake_mismatch` with the engine stopped and memory
  released) and load read (fresh engine samples, `load_source: engine`).
- Host A (GB10, `engine-quick-check.sh --engine vllm --new 0.31.0 --old 0.30.0
  --model qwen3-4b`, origin/main a230cf9a): venv created with the guide's steps;
  engine add; cold start 142.4 s (0.30.0: 99.2 s), warm 71–87 s; streamed
  reasoning request passed (first token 0.186 s), stop and 409 after it;
  aggregate tok/s C1 22.0/22.0, C4 106.2/106.6, C8 207.0/207.3 (0.31.0/0.30.0),
  TTFT p50 within noise; reply lengths differ at C4/C8 as batch-order effects
  allow (no token hash on vLLM). Two harness failures were not CapyCTL's: the
  probe read `/metrics` without the launch key, which CapyCTL's own guard
  refuses on 0.30.0 too (fixed in the quick-check on branch
  `fix/vllm031-metrics-auth`), and the context sweep's calibration could not
  read a 1-token reply from a reasoning model (fixed in capyctl-recipes main).
- Rerun with both harness fixes (host A, 2026-10-14, origin/main `ae6852bf`, warm
  starts): every lifecycle step passed, including the new `engine load via CapyCTL`
  step (12 of 12 load reads during a request carried a fresh engine sample).
  Aggregate tok/s C1 21.9 / 21.9, C4 106.5 / 106.4, C8 207.4 / 207.0 (0.31.0 /
  0.30.0); decode at 2k and 8k within 0.2%, prefill within 2%. The 32k context
  point was refused by both versions (`engine_rejected`, maximum context 29104):
  that is the context CapyCTL fitted for the preset's Qwen3-4B deployment, not a
  difference between versions.

## SGLang saver observation under a long state directory — 2026-10-14 (branch `fix/observation-socket-path`)

Found live 2026-10-14 on host A (GB10, standalone, SGLang 0.5.21, `deep`
deployments, build d4b86843): every `capyctl park deployment` of two SGLang
deployments was refused at once with `park refused before any effect (the
engine is not quiescent)`, and the role's `<state>/observation/` directory
stayed empty while the engines served. The same model and recipe parked on
2026-10-05 (ADR 0014 A17 evidence).

Root cause, confirmed in code and by the tests below. The state directory was
`/home/edurdias/capyctl-0.1.3-live-runs/2026-10-14-1854-b18/state`, so the
launch's socket `<state>/observation/<binding>.sock` was 108 bytes. The
protected entry's `entry_environment` (`runtime/sglang_observation_enrollment.py`)
refused any such path over 107 bytes (the `sun_path` limit) by returning
`False` without a word, so the launch ran without an enrolled saver
observation (SPEC §9.2). With no record in the observation directory, every
embedded park failed SPEC §10 step 4 and was refused as not quiescent. Host
roles share the layout, so any role whose state directory is longer than
63 bytes was affected the same way.

Fix:

- The listener binds, and the host connects, through a descriptor of the
  socket's directory, `/proc/self/fd/<fd>/<binding>.sock`, so the address fits
  `sun_path` whatever the directory's length
  (`sglang_observation_server._Custody.address`,
  `capyctl_launchers::native_observation::connect`). The listener now binds in
  exactly the directory its custody verified. The socket is still a 0600 file
  in the 0700 owner-only directory, with the same peer-credential and key
  checks; no abstract address is used. Paths are bounded at 4095 bytes.
- A scope the entry cannot publish now names why on the engine's log, with a
  closed code and no path: `capyctl_saver_enrollment_refused`, stage `scope`
  (`weight_restore`, `directory_path`, `group`, `directory_mode`, or the error
  class).
- An embedded park refused because the launch's observation never enrolled
  (a readable private directory without the binding's record) now says `the
  engine's saver observation is not enrolled` instead of `the engine is not
  quiescent` (`Quiescence::unproven`, `Unproven::ObservationNotEnrolled`). A
  host's `sglang_park_not_quiescent` log event gains `unenrolled`.

Tests (T22, T20):

- `runtime/tests/test_sglang_observation_enrollment.py`
  `test_a_long_observation_directory_still_enrolls` (a socket path over 108
  bytes enrolls, writes its record and answers a keyed observation) and
  `test_an_unpublishable_scope_names_its_reason`: both failed before (the
  scope was refused, and nothing was logged) and pass after.
- `capyctl-launchers` `native_observation::tests::a_socket_under_a_long_directory_is_observed`
  (the Rust client against the real Python listener under a 108+-byte path):
  failed before, passes after.
- `capyctl-agent` `saver_source::tests::an_embedded_sglang_without_evidence_is_not_quiescent`
  (extended) and `only_a_readable_directory_without_a_record_is_unenrolled`;
  `capyctl-controller` `an_unobserved_engine_park_refusal_says_it_is_not_enrolled`.

CPU tests only; they are not qualification. Live check still needed: a `deep`
SGLang park and wake on a role whose state directory makes the socket path
longer than 107 bytes.

## Engine log redaction keeps model paths

Found in the same live run: SGLang's `server_args` line in the engine log
read `'model_path': '<redacted>.8-27B-NVFP4@482ca0f3...'` and
`'/home/edurdias/.cache/huggingface/hub/models--RadixArk--Qwen3.<redacted>'`.
The suspected URL-userinfo rule was not the cause: `redact_urls` only touches
text after a `scheme://`, and the probe showed it left both lines unchanged.
The long-run rule was the cause, in both the Python scrubber inside the engine
(`runtime/sglang_startup_guards.scrub`) and the log writer
(`capyctl_adapters::vllm::args::redact_text`). A run starting with `/` was
kept as a path, but a `.` in the path (`.cache`, `capyctl-0.1.3`, `Qwen3.6`)
ended the run. The rest of the path then began without `/`, and when it was
48 characters or more it was blanked as a credential. The Python scrubber also
blanked any whole path of 48 characters or more. Now a run that starts with
`/` makes the rest of its word a path, past any `.`, `@` or `:`, until
whitespace, a quote or bracketing punctuation. A path is kept segment by
segment, so a credential-length segment inside one is still blanked. Launch
keys (by value), URL userinfo, query strings and bearer values are redacted
as before.

Tests (T37), failing before and passing after:
`engine_log::tests::model_paths_in_the_argument_echo_are_kept` and
`runtime/tests/test_sglang_startup_guards.py`
`test_scrub_keeps_model_paths_and_still_redacts_keys`. Both use the two live
paths and also check that the keys on the line, a 64-character segment inside
a path, URL credentials and bearer values are still redacted. CPU tests only.
Live check: the `server_args` line of the next SGLang launch shows its model
path.

## Engine quick check reads load through CapyCTL, not unkeyed /metrics — 2026-10-14 (branch `fix/vllm031-metrics-auth`)

The vLLM 0.31.0 quick check on host A failed its `engine /metrics` step: the
probe curled the launch's loopback `/metrics` without a key and got `401
{"error": "Unauthorized"}`. That answer is CapyCTL's own key guard, by design,
and not a CapyCTL defect:

- Under deep parking (default on, ADR 0012, SPEC §9.1 / T21) the vLLM argv
  carries `--middleware capyctl_vllm_guard.RequireEngineKey`
  (`crates/capyctl-adapters/src/vllm/args.rs`) and the per-launch key rides
  `VLLM_API_KEY` in the engine's environment, not `--api-key`. The guard keys
  every path but `/health`, `/metrics` included
  (`runtime/capyctl_vllm_guard.py`, `_INFERENCE_EXACT`), whatever the vLLM
  version, so vLLM 0.30.0 answers the same.
- vLLM's own `AuthenticationMiddleware` guards only `/v1`, `/v2`,
  `/inference` and `/cohere` (`vllm/entrypoints/serve/middleware/authenticate.py`,
  `GUARDED_PREFIX`); `/metrics` is a separate mount
  (`entrypoints/serve/instrumentator/metrics.py`). Read from the vLLM 0.30.0
  sdist and the installed 0.31.0: the auth, middleware registration and
  metrics mount are the same apart from whitespace, and every gauge and
  histogram name CapyCTL reads is in both `v1/metrics/loggers.py`.
- Every CapyCTL reader sends the launch key: the host agent and standalone's
  embedded load (`capyctl_agent::load::keyed_get`, used by
  `capyctl_controller::embedded_load`), and the vLLM adapter's quiescence read
  (`EngineHttp::work_counts`, keyed through `auth`). Their tests already serve
  a fake engine whose `/metrics` refuses an unkeyed read
  (`crates/capyctl-agent/tests/load.rs`, `crates/capyctl-adapters/tests/vllm_http.rs`).

Live on the maintainers' RTX 4090 laptop, vLLM 0.31.0, Qwen2.5-1.5B-Instruct,
an isolated standalone from `origin/main` (057c1a47): the engine argv has the
guard and no `--api-key`; unkeyed `/metrics` and `/v1/models` answer 401,
`/health` 200; during a streamed request every
`GET /management/v1/metrics/load` read carried a fresh sample with
`engine.running` 1 and `waiting` 0, and 0 and 0 once it finished. Cleaned up
afterwards (deployment, profile and role gone, GPU back to its baseline).

The probe (`scripts/live/quick-check/remote.sh` `metrics`) now checks
CapyCTL's contract: during a request, the new `load_check.py` polls the
management load read with the admin token and passes when a fresh sample for
the deployment carries the engine's running and waiting counts. The unkeyed
`/metrics` code and whether the guard is on the argv are kept as notes in
`metrics-notes.txt`. Exercised against the laptop's live role; the quick-check
driver itself has not been rerun on host A.

Seen on the same run, unrelated to the key: standalone's latency view
carries router series only. `EmbeddedLoad::tick` records each report's load
sample but not its engine latency histograms, so `source: engine` series are
absent on standalone for every engine and version.

## TensorFold 0.6.6 verified — 2026-10-14 (branch `feat/verify-tensorfold-0.6.6`)

Owner decision 2026-10-14: TensorFold 0.6.6 joins the verified set (ADR 0023
§1), installed on both lab hosts (host B's venv a byte-identical copy of host
A's); AGENTS.md's engine list also records vLLM 0.31.0 and a llama.cpp build in
the home directory on host A, approved the same day.

Source audit of v0.6.6 (`cb2ebf05`) against v0.6.5: the only upstream change
CapyCTL sees is the new `tensorfold serve --name-priority ID=background`
option (CUDA only), an ordinary extra argument; no environment, HTTP route,
metrics or build change. `--name-priority` names a served name or alias, and
CapyCTL passes no `--name` to a TP2 follower, so it may fail there
(documented in the engine options table). The audit found two gaps that
predate 0.6.6, both fixed here:

- `TENSORFOLD_API_KEY` was not an owned name. TensorFold reads it
  (`server/authentication.py`) and then refuses the unkeyed health and
  metrics routes CapyCTL reads; a profile or deployment `engine_env` entry
  under an approval such as `TENSORFOLD_*` reached the engine through
  `engine_env.values()` → `tensorfold/frozen.rs` `build_env` → the launch
  environment. It is now in `engine_env::OWNED_NAMES`.
- The router's TensorFold mapping read only the MLX statistics
  (`prefill_seconds`, `time_to_first_token`, `tokens_per_second`). The CUDA
  Qwen engine (`families/qwen3_5/cuda/engine.py` `generate`, sent by
  `cuda/http.py`) reports `prefill_s`, `decode_s` and draft counters instead,
  so a CUDA answer had no engine figures beyond cached tokens. A finite
  non-negative `prefill_s` now maps to `prefill_ms` (source `engine`) when
  `prefill_seconds` is absent. `decode_s` is not turned into a rate: the
  first token comes out of prefill and a gated request's statistics cover its
  last run only, so the engine gives no rate CapyCTL can state; time to first
  token and decode rate stay the router's on CUDA. `docs/guide/requests.md`
  says so.

Tests: `capyctl-config` `tensorfold.rs` (0.6.6 verified, 0.6.4 and 0.6.7
not), `capyctl-cli` `engine_cli.rs`
`tensorfold_066_is_verified_and_064_and_067_are_custom` (T07),
`capyctl-config` `engine_env.rs` `owned_names_are_never_settable` (T21;
failed before the owned-name change, passes after),
`capyctl-router` unit `tensorfold_cuda_statistics_map_prefill_only` and
integration `tensorfold_cuda_reports_prefill_only`, streamed and not (T40
T41; both failed before the mapping change, pass after). The old unit fixture
labelled `"engine": "cuda"` carried the MLX shape and is now labelled `mlx`.
CPU and Fake-engine tests are not qualification.

Live on host A, 2026-10-14 (one GB10, `scripts/live/engine-quick-check.sh
--engine tensorfold --new 0.6.6 --old 0.6.5 --host a`, standalone from
origin/main `a230cf9a`; a new `~/tensorfold-0.6.6-venv` at `cb2ebf05` with the
same pins as 0.6.5: Python 3.12.14, torch 2.13.0+cu130, triton 3.7.1, CUDA
13.0). Qwen3.8-27B NVFP4 with the DFlash2 drafter, `--parallel 8`, greedy.
Every lifecycle step passed: `engine add`, cold start 101.5 s (0.6.5: 139.2 s),
warm 13.9 s, a reasoning stream (14 reasoning chunks, content, `stop`, usage,
first token 0.142 s), stop and a 409 after it, `--evict` and wake on request
(15.6 s), the variant without drafts, engine `/health` and `/metrics` (no key).
Benchmark against 0.6.5 (medians, two interleaved passes): aggregate tok/s C1
42.5 / 42.4, C4 122.1 / 120.3, C8 195.0 / 194.2; decode and prefill at 2k, 32k
and 128k within 1%; TTFT at 128k 87.7 / 87.1 s; peak memory equal. C4's TTFT
p50 read 0.269 / 0.187 s with p95 equal: both versions show the same discrete
tiers by queue position (about 0.10, 0.14, 0.21, 0.33 and 0.38 s), and 0.6.5's
own two passes differ the same way. `token_sha` matched on 30 of 30 keys,
between versions and within each. Host B's `~/tensorfold-0.6.6-venv` is a
byte-identical copy of host A's (file digest compared). That run used the
binary from before the CUDA `prefill_s` mapping; the mapping is checked on the
live release checks.

## vLLM 0.31.0 launches under a version-skew-aware reservation — 2026-10-10 (branch `feat/vllm-0.31.0-support`)

Owner decision from the 2026-10-10 engine qualification (RTX 4090 laptop,
vLLM 0.31.0 wheel, Python 3.12.13, torch 2.13.0+cu130; evidence under
`~/projects/crossfunctionalai/engine-qual-2026-10-10/`, `ROWS.txt` indexes
`raw/`): CapyCTL launches vLLM 0.31.0, without changing one rendered byte of a
0.30.0 launch.

**The reservation follows the parser (ADR 0014 §3, ADR 0017 §2).** vLLM 0.31.0
moved logging into the nested `--logging-config` dotted config
(`vllm/config/logging.py`, `LoggingConfig`: `log_level`, `configure_logging`,
`pylogging_config_file`; `--log-level` and legacy `--log-config-file` override
their matching fields, the deprecated alias leaves in 0.33.0). The serve parser
defines the whole surface on one destination, `logging_config` — the
`--logging-config` JSON and every `--logging-config.<field>` resolve to it —
beside legacy flat `--log-level` and `--log-config-file` overrides, each an
argparse `SUPPRESS` destination: absent from a parsed namespace until a token
sets it (`raw/vllm-0310-apiserver-help.txt`). The entry's `RESERVED` list named
the flat 0.29.0/0.30.0 spellings, so `log_config_file` — a name no longer
present in a 0.31.0 namespace — failed every launch closed:
`vllm_startup_failed: effective_args_mismatch` (`status/start-frognano.txt`,
`status/inspect-frognano-failed.json`).

The rule now: the entry reads the destinations the installed parser's own
argparse tree defines (`engine_capabilities.parser_destinations`, the same
probe the capability report uses) and reserves what the build spells. Flat
`RESERVED` names stay unconditional — a build that defines none of them is
drift, never a silent un-reservation (ADR 0014 open issue 4: drift fails
closed); `RESERVED_IF_PRESENT` (rendezvous, scale-out) stays as it was;
`RESERVED_IF_DEFINED` adds the 0.31.0 logging surface (`logging_config`,
`log_level`), reserved wherever the build defines it, nothing reserved on a
build without it. An `SUPPRESS` destination absent from both parses is
agreement; absent from either one alone, or any value difference, is closed
drift. A 0.30.0 launch renders and compares byte-identically: every flat name
is defined with ordinary defaults, and `logging_config`/`log_level` are
reserved on 0.31.0 alone. Deploy-time policy (`engine_policy.rs`) reserves
`--logging-config` and `--log-level` beside `--log-config-file` (checked twice,
ADR 0014 §3), so a user cannot double-set the logging configuration on any
build, in any spelling: the JSON object, any dotted field, `=value`,
underscores, or an abbreviation.

**Cached tokens stay a usage figure (SPEC §17, the metrics contract of #101).**
vLLM 0.31.0's `usage.prompt_tokens_details` is `null` without
`--enable-prompt-tokens-details` (`raw/vllm-manual/chat-repeat-cached.json`,
`raw/vllm-qwen3/cached-tokens-repeat.json`: 418-token repeat prompt, 416/416
prefix-cache hits, `prompt_tokens_details` null), and the per-request `metrics`
object — which gained `speculative_decoding` and `tokens_per_second` — never
carries them (`raw/vllm-manual/chat-stream.sse`, usage chunk). The figure maps
from `usage.prompt_tokens_details.cached_tokens` on both the 0.30.0 and 0.31.0
response shapes — a response-shape-driven rule like the existing per-engine
mapping, since the router cannot know the engine version at request time — and
stays absent, never zero, when the engine reported none
(`crates/capyctl-router/src/engine_metrics.rs`).

Live recheck (laptop, RTX 4090, same `~/vllm-0.31.0-venv`, local machine only;
evidence `~/projects/crossfunctionalai/engine-qual-2026-10-10/recheck/`):
every row PASS through the release build of this branch, driven with
`--config` on the client as well (an earlier attempt without it resolved the
machine's own implicit registry — `vllm` 0.29.0 — and refused with
`profile_exists`; nothing was written there). `engine add`: vLLM 0.31.0
registered, custom, deep park enabled, published. `deploy model` +
`start --wait`: Ready 1/1 (initialize succeeded) — the exact launch the flat
reservation refused before. Chat via the relay (the deployment passes
`--enable-prompt-tokens-details` as an extra): non-streaming and streamed
answers both carry `cached_tokens` `{"source": "engine", "value": 0}` — the
engine measured zero, because FrogNano-4B-2609 is a hybrid (Mamba) model
whose prefix cache does not hit on these prompts (`vllm:prefix_cache_hits_total
0.0` in the engine's own scrape, matching the manual qualification; the dense
Qwen3-4B row in `raw/vllm-qwen3/` hit 416/418); the figure maps from
`usage.prompt_tokens_details.cached_tokens` on the 0.31.0 shape and would stay
absent had the engine reported nothing. Deep-park cycle: park released
9.5 GiB to 727 MiB, one request woke it in 7.5 s with the figures present,
memory returned. `stop`: engines stopped, GPU back at the 111 MiB baseline.
Unlike the CPU and Fake-engine tests, this recheck ran the real engine.

Tests (T14, T21, T40, T41): the entry suite gained a `VersionSkewTests` class
with a fixture parser per shape — the 0.29.0/0.30.0 flat parser unchanged, and
a 0.31.0 parser whose logging actions are `SUPPRESS`-destined dotted
destinations (`fake_parser_0310`) — checking the rendered block launches on
the 0.31.0 shape, the whole 0.31.0 logging surface stays reserved however
spelled, a rendered logging configuration is kept exactly and re-set is
refused on either shape, and a build without a flat reserved name fails closed
(`runtime/tests/test_vllm_entry.py`). Router unit
(`vllm_0310_maps_cached_tokens_from_usage_or_leaves_it_absent`) and integration
(`vllm_0310_maps_cached_tokens_from_usage`, streamed and not, relayed byte for
byte) tests read the raw 0.31.0 response and usage-chunk shapes and hold the
absent-not-zero rule; the 0.30.0 tests are untouched. The adapters' logging
test refuses every 0.31.0 spelling in host-fixed and deployment arguments
(`crates/capyctl-adapters/tests/vllm_args.rs`), and the reserved-parity test
(`crates/capyctl-config/tests/reserved_parity.rs`) folds `RESERVED_IF_DEFINED`
into the one-table check on both sides of the launch. CPU and Fake-engine
tests are not qualification.

## llama.cpp guide and first live rows on a discrete GPU (plan slice L6) — 2026-10-09 (branch `feat/llamacpp-l6-docs-live`)

[ADR 0029](../design/adr/0029-llamacpp-engine.md), plan slice L6
(`docs/plans/2026-10-09-llamacpp-engine.md`). The user guide gains llama.cpp, and
the first live rows ran on a maintainer's laptop (one 16 GiB discrete NVIDIA GPU,
CUDA 13.4 toolkit already installed, nothing installed system-wide) in a standalone
role with its own state directory and ports. No lab host was used.

## Docs

`docs/guide/engines.md` (a llama.cpp section: registration, the typed fields, the
derived memory, restart-only, what is refused and why), `docs/guide/install-engines.md`
(building `llama-server` from the tag with `-DGGML_CUDA=ON -DLLAMA_BUILD_IS_DEV=OFF`),
`docs/guide/engine-flags.md` (the llama.cpp v0.6.0 table, now checked by
`engine_flag_table.rs`, which reads llama.cpp's typed refusal `ReservedField` as
typed), `docs/guide/errors.md` (`engine_config_file`, `effective_args_mismatch`,
llama.cpp in `engine_not_found`, `engine_unsupported`, `capability_missing`),
`docs/guide/requests.md` (`cache_salt`), `docs/operations/install.md` (registering a
`llama-server` binary), `docs/operations/configuration.md` (`local_engine.llamacpp`,
`engine_config.llamacpp`), `docs/examples/deployment-llamacpp.yaml` (validated by
`every_documented_example_passes_validate_config`), the engine lists of the guide
pages, the site's platform list and `capyctl --help`.

## Live rows (laptop, standalone)

Binary: a source build of tag `v0.6.0` (commit `d81235049`) with
`-DGGML_CUDA=ON -DLLAMA_BUILD_IS_DEV=OFF -DCMAKE_CUDA_ARCHITECTURES=89`;
`llama-server --version` prints `version: 0.6.0 (build 1, commit d81235049)` on
standard error. Model: `Qwen/Qwen3-4B-GGUF` revision
`bc640142c66e1fdd12af0bd68f40445458f3869b`, `Qwen3-4B-Q4_K_M.gguf` (2 497 280 256
bytes, sha256 `7485fe6f…fdf5`), deployed with `context_length: 8192` and the default
4 slots, no `resources`.

| Check | Derived or expected | Measured |
|---|---|---|
| LC1 `engine add` | llama.cpp 0.6.0, fingerprint `<v>+<commit>` | `Registered llamacpp (llamacpp 0.6.0)`, `0.6.0+d81235049`, custom `yes`, deep park disabled; the version check took 0.89 s (CUDA initialisation included) |
| Start to Ready, first deploy | within the Initialize window | 7.3 s for `deploy --activate --wait`, the 2.5 GB checkpoint digest included; llama-server listening 1.37 s after its start |
| Start to Ready, `start` after `stop` | a fresh launch | 2.2 s, generation 2 |
| Rendered command | ADR 0029 §5 | `--ctx-size 32768 --parallel 4 --no-kv-unified --gpu-layers all --cache-type-k f16 --cache-type-v f16 --fit off --cache-ram 0 --no-context-shift --metrics --slots --offline --no-webui --cors-origins localhost --no-cors-credentials`; readiness found `n_slots = 4, n_ctx_slot = 8192` |
| KV cache | 4 × 8192 × 36 × 8 × (128 × 2 + 128 × 2) = 4 831 838 208 B = 4608.00 MiB | `llama_kv_cache: CUDA0 KV buffer size = 4608.00 MiB` (equal) |
| GPU memory | derived 8 921 023 708 B (8507 MiB) | 7342 MiB for the process at its peak and while serving (model 2375.91, KV 4608.00, compute 85.01, about 273 CUDA context); 1165 MiB (16 %) under the request |
| Host memory | derived `ram` 4 GiB (provenance `default`) | resident set 877 MiB |
| Stop (SIGTERM) | exit and full release | `stop` returned in 0.15 s, the process exited 0.51 s after the command, GPU back to its 113 MiB baseline |
| Park | refused, restart-only | `capyctl park deployment` exit 5, `unsupported_capability` |
| Chat, non-streaming | answer with `timings` and figures | `content` and `reasoning_content`; after the fix below, `timings` relayed and `capyctl.metrics` `prefill_ms`, `decode_tokens_per_second` (engine), `cached_tokens`, `ttft_ms` |
| Chat, streaming | deltas within `STREAM_DELTA_FIELDS` | deltas `role`, `content`; `timings` on the last chunk; with `stream_options.include_usage` that chunk also has `usage`; the `x-capyctl-metrics` comment carries the engine's `prefill_ms` and decode rate either way |
| Tool call and reasoning | relayed | `tool_calls` (`get_weather`, `{"city": "Lisbon"}`, `finish_reason: tool_calls`) and `reasoning_content`, non-streaming and streaming (deltas `role`, `content`, `reasoning_content`, `tool_calls`) |
| Load during 3 streams | running 3, KV usage from `/slots` | `/metrics` `requests_processing 3`; `/slots` 5081 bytes; after the fix below the management load read shows `running 3, waiting 0, kv_usage_ppm 35950`, fresh |
| Client hang-up mid-decode | `requests_processing` 0 promptly | 0 at the first read after the cut (under 0.1 s) |
| Client hang-up mid-prefill (6.6k-token prompt) | 0 promptly | 0 within 0.5–0.8 s |

The design note's live-check list, item by item: 1, start time — above (laptop
only). 2, peak and steady memory against the request — above: the process holds what
it allocates at start, so peak equals steady; the derived request is 16 % above it
and the `ram` default is about 4.7 times the resident set. The margin and the `ram`
figure are not recomputed from one small model; that waits for a larger model and
the GB10 row. 3, the KV formula — equal to llama.cpp's own line. 4, SIGTERM — above.
5, stream shapes — above. 6, hang-up — above, mid-prefill included. 7, `/slots` KV
usage under load — above. 8, the MTP context's cache with `draft-mtp` and a quantized
V cache with flash attention `auto` — not checked (Qwen3-4B has no MTP heads). 9, a
prebuilt `b11429` binary's version line — not checked (source build). 10, whether the
version check needs CUDA initialisation time — 0.89 s on this CUDA build, inside the
bound.

## Defects found live and fixed

- **A non-streaming answer lost llama.cpp's `timings`.** CapyCTL collects a
  non-streaming request from the engine's stream; `assemble`
  (`capyctl-adapters/src/forward.rs`) kept `tensorfold` and `metrics` from the final
  chunk but not `timings`, so the answer had no `timings` and `capyctl.metrics` had
  no `prefill_ms` and the router's decode rate. `timings` is now kept. Test:
  `a_collected_response_keeps_the_llamacpp_timings` (failed before).
- **Standalone reported no load for llama.cpp.** The embedded load sampler
  (`capyctl-controller/src/embedded_load.rs`) left out a launch with no recorded
  inference key, and an embedded llama.cpp launch has none (ADR 0029 §4), so the
  load read stayed `sample: null`. `LoadTarget.native` is now optional and a keyless
  target is scraped without an `Authorization` header (`capyctl-agent/src/load.rs`);
  a host's ingress targets still always carry their key. Test:
  `a_target_without_a_key_is_scraped_unkeyed` (`crates/capyctl-agent/tests/load.rs`).

## Still open

- `VERIFIED` does not gain `(llamacpp, "0.6.0")`: the plan's acceptance is LC1–LC6
  and two parts did not run on the laptop. LC4 (a llama.cpp and a vLLM deployment
  switching under memory pressure) needs a vLLM environment on the same machine,
  and the laptop has none. LC6's `--spec-type draft-mtp` needs a GGUF model with MTP
  heads. Every llama.cpp build still lists as `custom`.
- The GB10 row (after the owner approves a lab-host build and it is added to
  AGENTS.md's list): start time, peak and steady memory on unified memory, whether the
  default mmap load keeps a page-cache copy beside the device copy, and the prebuilt
  `b11429` arm64 archive's version line.
- The margin and the `ram` figure in `context_fit/llamacpp.rs`, recomputed from the
  GB10 row and a larger model; the live row scripts `scripts/live/matrix/rows/LC1.sh`
  to `LC6.sh` for the lab harness.
- `capyctl park deployment` on llama.cpp says only `unsupported_capability:
  Requested capability is unavailable`; the guide explains why, the message does not.

CPU and Fake-engine tests are not qualification. The laptop rows above are live
evidence for one discrete GPU and one small model only.

## llama.cpp per-request figures and load reports (plan slice L5) — 2026-10-09 (branch `feat/llamacpp-l5-metrics`)

[ADR 0029](../design/adr/0029-llamacpp-engine.md) §11, plan slice L5
(`docs/plans/2026-10-09-llamacpp-engine.md`). The shapes were read from the v0.6.0
tag (`tools/server/server-task.cpp`, `server-common.cpp`, `server-chat.cpp`,
`server-context.cpp`); nothing ran against a llama-server.

- **Per-request figures** (`capyctl-router/src/engine_metrics.rs`). `prefill_ms` from
  `timings.prompt_ms`, `decode_tokens_per_second` from `timings.predicted_per_second`
  (positive only), `cached_tokens` from `usage.prompt_tokens_details.cached_tokens`;
  `ttft_ms` is the router's and `queue_ms` is absent. The router's decode rate counts
  `usage.completion_tokens`, else `timings.predicted_n`. A stream chunk is parsed when
  it carries `timings` (the usage chunk, else the finish chunk); `timings` is relayed
  unchanged.
- **Streamed fields.** llama.cpp's deltas (`role`, `content`, `reasoning_content`,
  `tool_calls`) are already on the relay's list; the table in
  `docs/guide/requests.md` gains its column.
- **Load reports** (`capyctl-agent/src/load.rs`). Running is
  `llamacpp:requests_processing`, waiting `llamacpp:requests_deferred`; the KV usage is
  the tokens of processing slots over the sum of the slots' `n_ctx`, from one keyed,
  bounded `/slots` read per tick. A failed or malformed `/slots` read leaves the engine
  load out. No latency series is forwarded and the sample names no engine family.

Tests (T40, T42, CPU only): `capyctl-router` `engine_metrics` unit tests (answer,
stream with the usage chunk and with the finish chunk only, zero rate absent, the
`predicted_n` fallback), `crates/capyctl-agent/tests/load.rs` (the v0.6.0 `/metrics`
body, the `/slots` ratio, a refused and a malformed `/slots`). Fake-engine tests are not
qualification.

Live checks still open: a captured v0.6.0 answer, stream and `/metrics` body; the
`/slots` body size with long prompts (it carries prompt and generated text, against the
4 MiB scrape bound) and its read time under load.

## llama.cpp GGUF header and sizing (plan slice L4) — 2026-10-09 (branch `feat/llamacpp-l4-sizing`)

[ADR 0029](../design/adr/0029-llamacpp-engine.md) §5, §9, plan slice L4
(`docs/plans/2026-10-09-llamacpp-engine.md`). A llama.cpp deployment no longer has to
state `resources`: CapyCTL reads the GGUF header of the model it renders and derives
the memory request. The format, the keys and the KV layout were read from the v0.6.0
tag (`ggml/src/gguf.cpp`, `src/llama-model.cpp`, `src/llama-hparams.cpp`,
`src/llama-kv-cache.cpp`, `src/llama-context.cpp`, `src/llama-arch.cpp`,
`src/models/*.cpp`, commit `d812350`); nothing ran on a GPU.

- **Header reader** (`capyctl-config/src/gguf.rs`). Magic `GGUF`, version 2 or 3,
  then the metadata key/value pairs; never tensor data. Bounded: 64 MiB of metadata,
  1 048 576 keys, strings up to 1 MiB, arrays up to 1 048 576 entries; a truncated
  file, a duplicate key, an array of arrays or an unknown type is refused. Only
  numbers, booleans, `general.architecture` and short integer arrays are kept;
  every key's name is kept.
- **KV formula** (`capyctl-config/src/context_fit/llamacpp.rs`).
  `slots × pad256(context_length) × Σ_layers head_count_kv × (key_length × bytes(K) +
  value_length × bytes(V))`; `head_count_kv` a number or one per layer (default the
  heads), `key_length`/`value_length` else `embedding_length / head_count`; the MTP
  layers (`nextn_predict_layers`) are not the main context's; the cache types' block
  sizes (`q8_0` 34 bytes per 32 values, `q4_0`/`iq4_nl` 18, `q4_1` 20, `q5_0` 22,
  `q5_1` 24). Without `--flash-attn on` the V cache is counted padded to its widest
  layer, as llama.cpp lays it out then.
- **Derived request.** Weights (the rendered GGUF and all its shards, the projector,
  the draft model and all its shards, ADR 0014 A6) + KV + the family margin
  (unified: ADR 0014 A18; discrete GPU: weights × 1.10 + KV, ADR 0019 §3). The
  startup peak is the request: cold, Ready, parking and wake charge it, parked
  nothing. The margin stays the placeholder until the live rows measure it.
- **Draft model.** A `--model-draft` in the host-fixed or the extra arguments counts
  with every shard llama.cpp loads (a split set named by its first shard), each inside
  `security.approved_paths` once symlinks resolve. A draft that cannot be counted
  (outside the approved paths, a shard missing, a set named by a later shard) is
  refused at resolution, declared budget or not, instead of being left out of it; so
  is a checkpoint with no model to pick.
- **Declared budgets.** llama-server allocates the whole cache its context and slots
  fix (`--fit off`), so where the header makes it calculable (full attention, no
  cache layers off the GPU via `n_gpu_layers` or `--no-kv-offload`) a declared
  `memory.kv_cache`, `memory.request` or `resources` that leaves less for the KV cache
  than that figure is refused naming the field. A draft context only adds to it, so
  the bound stands beside a draft model. Other layouts keep the declared estimate.
- **Refusals.** Derivation is refused, naming `engine_config.memory.kv_cache` or
  `resources`, for a sliding-window header (a key, or an architecture whose model code
  has such layers, `llama4` without a window of 0 included), recurrent (`ssm.*`,
  `wkv.*`), hybrid (`full_attention_interval`, `shortconv.*`, llama.cpp's hybrid
  architectures), MLA (`attention.kv_lora_rank`), other layouts (indexer caches,
  embedding, diffusion and encoder models, a projector), an unreadable header or no
  single model to pick; for `n_gpu_layers` other than `all`, `--override-tensor`,
  `--cpu-moe`, `--n-cpu-moe`, `--n-cpu-ffn`, `--no-kv-offload`, an `mlock` load mode;
  and for a draft context: any `--spec-type draft-*` and a `--model-draft` (judgement
  call: llama.cpp gives the draft model its own context with as many cells as the
  target's, which the ADR does not size; a declared `memory.kv_cache` still derives
  the request with the draft model's weights). A `context_length` above the GGUF's
  `<arch>.context_length` is refused, declared resources or not.
- **Facts.** The host measures them beside the digest (`GgufFacts`: loaded bytes,
  training context, the cache shape or why not); they travel as
  `CheckpointDigestEvidence.gguf` and `SingleLaunchPlan.checkpoint_gguf` (capability
  `checkpoint_gguf`, refused before sending to a host without it), are recorded in the
  revision's `memory.gguf` beside the whole checkpoint's weights (which a plan still
  names and a host verifies), and re-measured before a launch: a remote host and the
  embedded (standalone) host alike refuse a launch whose re-measured weights or GGUF
  facts differ from the revision's (`checkpoint_mismatch`), so a draft model grown
  after the revision was recorded, outside the checkpoint's digest, never launches
  against the old reservation. A llama.cpp revision is provisional until a host
  measured its checkpoint (`CheckpointFacts::provisional()` carries a pending header),
  declared resources included, so the training-context check always applies.

Tests (T42, T26, CPU only): `crates/capyctl-config/tests/llamacpp_sizing.rs`
(reader and bounds, the formula and the plan's 48-layer figure, per-layer
`head_count_kv`, refusals, measured weights with split and refused drafts, a
host-fixed draft, declared budgets against the calculable cache, derived phases on
unified and discrete memory, snapshot re-derivation, training context),
`crates/capyctl-controller/src/checkpoint_digests/tests.rs` (T34 T22: a grown draft
refuses an embedded Initialize), `crates/capyctl-protocol/tests/
checkpoint_digest.rs` (wire round trip and capability), `capyctl-domain` unit tests.
CPU and Fake-engine tests are not qualification; only LC1–LC6 qualify llama.cpp.

Live checks still open: the KV figure against llama.cpp's startup log line, the
margin and the startup peak against measured memory, the draft and MTP contexts'
caches.

## llama.cpp adapter, readiness and restart-only lifecycle (plan slice L3) — 2026-10-09 (branch `feat/llamacpp-l3-adapter`)

[ADR 0029](../design/adr/0029-llamacpp-engine.md) §4, §6, §7, §10, §11, plan slice
L3 (`docs/plans/2026-10-09-llamacpp-engine.md`). A llama.cpp deployment now
launches, becomes ready, serves chat and stops, on a host agent and in standalone
alike. The launch refusals slices L1 and L2 left in place are gone: the host
agent's launch authorization (`resolve_as`), `prepare`, the launching and probe
adapters, and the embedded coordinator's `ProfileBindings::spec`. Shapes were read
from the v0.6.0 tag (`tools/server/server-context.cpp`, `server-http.cpp`,
`server-task.cpp`, `common/arg.cpp`, commit `d812350`).

- **Adapter** (`capyctl-adapters::llamacpp::{adapter,http,initialize}`,
  `AdapterSpec::Llamacpp`). No key: llama-server listens on loopback and is
  reached only through CapyCTL's routed path. Initialize refuses a machine-wide
  `config.ini`, renders through the shared builder, spawns through the director's
  tool, then waits (watching the process) for `/health` 200 (every route answers
  503 `Loading model` until the model is loaded) and for `/v1/models` to list the
  served name. SPEC §8.2's check after the engine's parser: `/props` must report
  `total_slots` equal to the rendered `--parallel` and `endpoint_metrics` and
  `endpoint_slots` true, and `/v1/models` `meta.n_ctx` must equal the rendered
  slot window `pad256(context_length)`, capped at `meta.n_ctx_train` as
  llama-server caps it; any difference fails the launch `effective_args_mismatch`
  before the probe, and the coordinator stops it on its recorded processes. Then
  the bounded chat probe (non-empty `content` or `reasoning_content`). The bound is
  the Initialize window (ADR 0014 A1 and A7). Reads are bounded (5 s, 4 MiB: `/props`
  carries the chat templates). The completion probe uses `POST /v1/completions`;
  llama-server answers its text, without token ids.
- **Restart-only.** Park, Restore and ReloadWeights are unsupported; the Park arms
  of the host agent and the coordinator refuse the kind as before (`residency_tier`
  on a host); a group member never resolves to llama.cpp. A wake is a fresh launch
  of the same frozen contract.
- **Idle gate and quiescence** (ADR 0029 §10, ADR 0023 §6). `idle_before_signal`,
  `engine_quiescent`, `prepare_park` and `observe_work` read `/metrics`:
  `llamacpp:requests_processing` and `llamacpp:requests_deferred` both 0 is idle;
  either one above 0 is busy; nothing listening or 503 is signalled at once; an
  answer without both gauges, an error status or no answer is signalled when the
  bound ends (30 s or the command's remaining time); busy at the bound withholds
  the signal and the cleanup stays uncertain. The wait is the shared
  `tensorfold::wait_idle`, which the embedded coordinator already ran for every
  engine. On the host agent the Terminate gate (`native_execution/idle_gate.rs`,
  moved out of `tensorfold.rs` unchanged for TensorFold) now builds a llama.cpp
  adapter for a llama.cpp launch.
- **Requests.** `cache_salt` is refused `cache_salt_unsupported` before anything is
  sent. SSE `:` comments (llama-server's `--sse-ping-interval` pings) were already
  not progress and are not relayed; a test now pins it for llama.cpp. A served name
  llama-server would split or trim (`--alias` takes a comma list) is refused at
  render, since readiness could never find it listed.
- **Hidden configuration at launch** (ADR 0029 §6). A llama.cpp launch on a host
  where `/etc/llama.cpp/config.ini` exists is refused before any effect with the
  new closed policy refusal `engine_config_file`: in the host agent's admission
  (provisioning, pre-admission and the locked recheck alike, so no key is stored,
  nothing is journaled and the reservation is released on that evidence), and in
  the adapter's Initialize before the spawn (the embedded path, where a `Refused`
  Initialize settles as the installation gate's does). The root is `/`; tests name
  their own (`with_llamacpp_system_root`, `LlamacppAdapter::with_system_root`).
  `engine_config_file` and `effective_args_mismatch` have status hints.
- **Launch-failure summary.** llama-server's `error: invalid argument: --name` is
  read like the other parsers' refusals, so an engine that exits on a bad argument
  is reported `engine_argument_rejected` naming the option, never its value.
- **Development controls.** llama.cpp launches are marked with their
  unauthenticated loopback surface (`/v1`, `/health`, `/metrics`, `/props`,
  `/slots`, `/lora-adapters`, access `inference`), as TensorFold's are.
- **Still refused or not here:** park and group paths (restart-only, multi-rank out
  of scope, ADR 0029); deriving the request from the GGUF header and the
  training-context refusal (slice L4); per-request figures and load reports
  (slice L5); the errors guide, engine guide and live rows (slice L6).

Tests (T42 with T14, T16, T17, T21, T29, T37, T41): `capyctl-adapters/tests/
llamacpp_adapter.rs` against an axum engine with llama-server 0.6.0's shapes
(readiness through 503 and an unlisted model; `effective_args_mismatch` for
`total_slots`, `n_ctx` below and above the window, and `endpoint_metrics`; the
window capped at the training context; a reasoning-only probe; an exit reporting
the rejected argument; `config.ini` refusing before any spawn; the gauges; the
wait at the bound; no park path; `cache_salt`; SSE pings; a hang-up quiescent only
once the gauges read 0), `llamacpp_args.rs` (the served-name refusal),
`launch_failure` and `llamacpp::http` unit tests, `resolve` tests, the agent's
native-execution tests (prepare from local policy, Park refused `residency_tier`,
`engine_config_file` as a terminal refusal with no key and no journal entry, the
Terminate gate on `/metrics`), the controller's `engine_bindings` test (the embedded
spec, no key, the same command on every start, no cache root no launch) and the
embedded stop test `only_an_engine_answering_busy_is_not_signalled`, now run for
llama.cpp's `/metrics` too. A CPU build of the v0.6.0 tag served a 0.5B Qwen2.5
GGUF with the rendered command: `/health`, `/v1/models` (`n_ctx` 512 for
`context_length` 500, `n_ctx_train` 32768), `/props` (`total_slots` 4, both
endpoints on), `/metrics` (processing 1 during a stream, 0 within half a second of
the client hanging up), the completion probe's text and SIGTERM (exit in 0.1 s)
matched what the adapter reads. CPU and Fake-engine tests are not qualification;
only the live rows LC1–LC6 (slice L6) qualify llama.cpp. Still needing a live
check on a GPU: start time to readiness, the SIGTERM exit and memory release, and
the hang-up behaviour mid-prefill.

## llama.cpp option policy, resolution and rendering (plan slice L2) — 2026-10-09 (branch `feat/llamacpp-l2-args`)

[ADR 0029](../design/adr/0029-llamacpp-engine.md) §5, §6, §8, §9, plan slice L2
(`docs/plans/2026-10-09-llamacpp-engine.md`). The fail-closed option arms of slice L1
are replaced by llama.cpp's own policy, a llama.cpp deployment resolves, and the
`llama-server` command and its closed environment are rendered. Flag names were read
from the v0.6.0 tag (`common/arg.cpp`, commit `d812350`) and the rendered command was
checked against the parser of a CPU build of the tag; nothing ran on a GPU.

- **Option policy** (`engine_policy.rs`, tables in `llamacpp.rs`). llama-server
  takes exact names only, so names are compared exactly after `_` → `-`, against
  every long alias and negative form of each option; the other engines' prefix rule
  is not applied (`--cache-reuse` beside the reserved `--cache-ram` is ordinary).
  `--name=value` is refused, saying llama.cpp refuses it. Reserved options are
  ADR 0029 §6's two lists; a reserved option a typed field renders names that field
  (`--ctx-size` → `context_length`, `--parallel` → `max_concurrent_requests`,
  `--cache-type-k`/`-v` → `kv_cache_dtype`, `--gpu-layers` →
  `llamacpp.n_gpu_layers`, `--model` → `llamacpp.gguf_file`, `--mmproj` →
  `llamacpp.mmproj_file`). The sensitive options need approval by name (any
  spelling approves the option); each path a path option names, as llama-server
  splits it (`--lora` and `--control-vector` lists, `<path>:<scale>` lists), lies
  inside `security.approved_paths` lexically at deploy time and through symlinks in
  the launch builder; a quoted list is refused. The name shapes of ADR 0014 open
  issue 5 still apply, except to `--no-host`. Two spellings of one option are a
  duplicate. Host-fixed arguments follow the same exact-name policy.
- **Hidden inputs.** A llama.cpp profile or deployment `env` naming a variable
  llama-server reads in place of a reserved option, its listener, its device or its
  directories is refused `engine_env_reserved:<name>`, whatever `approved_env` says,
  and the launch environment drops it again: `LLAMA_ARG_*`, `LLAMA_SERVER_*` (router
  mode, slot debugging), `AIP_*` (`AIP_MODE=PREDICTION` with `AIP_HTTP_PORT`
  replaces the port), `LLAMA_API_KEY`, `MTMD_BACKEND_DEVICE` (`--mmproj-device`),
  `HF_TOKEN` (`--hf-token`), the ggml device variables (`GGML_CUDA_DEVICES`,
  `GGML_CUDA_ENABLE_UNIFIED_MEMORY`, `GGML_VK_VISIBLE_DEVICES`,
  `GGML_VK_PREFER_HOST_MEMORY`, `GGML_VK_ALLOW_SYSMEM_FALLBACK`,
  `GGML_METAL_DEVICES`, `GGML_OPENCL_PLATFORM`, `GGML_OPENCL_DEVICE`,
  `GGML_HEXAGON_DEVICES`, `ONEAPI_DEVICE_SELECTOR`, `GGML_BACKEND_PATH`),
  `LLAMA_CACHE`, `XDG_CONFIG_HOME` and `HOME`. The list comes from every `getenv` of
  the v0.6.0 tag; other ggml tuning variables stay available through `approved_env`.
  The host's engine cache creates `<state>/engines/llamacpp/{config,cache}` private
  (0700) and refuses a configuration directory that is not empty.
- **Resolution.** `engine_config.llamacpp` (`n_gpu_layers`, a count or `all`,
  default `all`; `gguf_file`; `mmproj_file`, both relative `.gguf` paths inside the
  checkpoint). `context_length` is required; the slots are
  `max_concurrent_requests`, else 4, and `pad256(context_length) × slots` must fit
  llama-server's `int`; `kv_cache_dtype` is one of llama.cpp's nine cache types,
  default `f16`; `dtype`, `quantization`, `cuda_graphs`, `trust_remote_code: true`
  and `language_model_only: true` are refused naming the path; `deep` and
  `host_backed` are `capability_missing` and an undeclared residency is
  `restart_only`; `model.draft` is refused (`--model-draft` is the way). Until the
  request is derived from the GGUF header (slice L4), `resources` are required,
  offline too, and the KV cache is bounded by the declared Ready allocation less
  the measured weights. Defaults are recorded as `capyctl default`, so a snapshot
  re-resolves to itself. Status and `validate config` show the slots under
  `context.streams` with source `declared` or `default`, and they are the load
  read's running limit.
- **GGUF pick** (`checkpoint_layout.rs`, read where the checkpoint is): the one
  `.gguf` that is not a projector, or the first shard of the one complete split
  set, or the file `gguf_file` names (a later shard or a projector is refused);
  several candidates without `gguf_file` are refused naming each.
- **Rendering** (`capyctl-adapters::llamacpp`): `--host 127.0.0.1 --port <p>
  --model <gguf> --alias <served> --ctx-size <pad256(ctx) × slots> --parallel
  <slots> --no-kv-unified --gpu-layers <n|all> --cache-type-k <t> --cache-type-v
  <t> --fit off --cache-ram 0 --no-context-shift --metrics --slots --offline
  --no-webui --cors-origins localhost --no-cors-credentials [--mmproj <file>]`,
  then the host-fixed and the deployment's arguments. The pass-through arguments are
  checked again at render, and the final vector is rechecked: each rendered option
  exactly once, `--mmproj` at most once, no other reserved option, no `=` spelling.
  The environment is closed: `PATH`, the GPU pin, `XDG_CONFIG_HOME`, `LLAMA_CACHE`,
  the engine log and the resolved engine environment less the refused names; no
  `HOME`.
- **Still refused until the later slices land**: the launch (the host agent's launch
  authorization and the embedded coordinator's Initialize refuse the kind, and the
  launch builder, adapter, readiness, idle gate and the launch-time
  `/etc/llama.cpp/config.ini` check are slice L3); deriving the request from the
  GGUF header and the training-context refusal (slice L4, so `resources` stay
  required); per-request figures and load reports (slice L5); park and group paths
  refuse the kind as before.

Tests (T42 with T14, T37): `capyctl-config/tests/llamacpp.rs` (every reserved
spelling and `_` spelling in extra and host-fixed arguments, the fields named, the
`=` spelling, ordinary options, duplicates across spellings, approvals and paths,
options refused even when approved, the tables against llama-server 0.6.0's option
list in `tests/fixtures/llama-server-0.6.0-options.txt`, resolution, slots and their
source, the environment refusals, `gguf_file`/`mmproj_file`, the GGUF pick),
`capyctl-adapters/tests/llamacpp_args.rs` (`--ctx-size 60416 --parallel 4` for 15000
tokens and 4 slots, the reserved block complete and in order, refusals at render,
the recheck, the closed environment, the builder's GGUF and projector, a draft model
outside the approved paths through a symlink) and the `engine_cache` unit test (the
private directories). The engine flag table of `docs/guide/engine-flags.md` and its
test rows land with the guide in slice L6. CPU and Fake-engine tests only; they are
not qualification. Only the live rows LC1–LC6 (slice L6) qualify llama.cpp.

## llama.cpp registration (plan slice L1) — 2026-10-09 (branch `feat/llamacpp-l1-registration`)

[ADR 0029](../design/adr/0029-llamacpp-engine.md) §1–§2, plan slice L1
(`docs/plans/2026-10-09-llamacpp-engine.md`). `llamacpp` is the fourth engine kind
in every closed engine set, and a bare `llama-server` binary registers beside the
dist-info installations. SPEC §20 gains T42 (llama.cpp conformance).

- **Kind.** `Engine::Llamacpp` (serde and CLI name `llamacpp`), in `Engine::ALL`;
  the environment profile `local-llamacpp`. `capyctl-config/src/llamacpp.rs` holds
  the executable name, the version-line parser, the `<version>+<commit>`
  fingerprint, the release tag commits (`0.6.0` → `d812350`) and the
  `/etc/llama.cpp/config.ini` check under a replaceable system root.
- **Detection** finds an executable regular `llama-server` in `PATH` entries,
  `~/llama.cpp/build*/bin`, `/opt/*/bin`, `/usr/local/bin` and under `--path`,
  follows no link out of the scanned root, reads the version from a
  `libllama.so.X.Y.Z` name beside it (or in `<prefix>/lib`), else `unknown`, and
  runs nothing.
- **`engine add`** takes the binary or its directory (a link is refused, naming its
  target), runs the bounded version check reading standard error, parses
  `version: <v> (build <n>, commit <h>)` and writes `build_fingerprint: <v>+<h>`
  (the build number is dropped) with `security.deep_park: disabled` and no
  `cuda_home`. A version line on standard output alone does not parse
  (`engine_unsupported`); `--deep-park enabled` is `capability_missing`; a machine
  with `/etc/llama.cpp/config.ini` is `engine_unsupported` naming the file, before
  the binary runs. Nothing is written for any refusal. Nothing is probed: the
  capability report says deep parking is missing.
- **Digest** (ADR 0008): the binary and every `lib*.so*` entry beside it, and in
  `<prefix>/lib` for `<prefix>/bin/llama-server`; a link counts by its target text.
- **Listing.** `custom` follows the verified set read through the release a build
  counts as: `0.6.0-dev` with the tag's commit counts as `0.6.0`. Listings show a
  llama.cpp profile's fingerprint rather than the library version the host
  reports. The verified set has no llama.cpp entry yet (ruling 2: it lands with
  the live rows), so every build lists as `custom`.
- **The role's own engine.** `local_engine.llamacpp`, `--llamacpp-bin` and
  `CAPYCTL_LLAMACPP_BIN`, with the shared precedence; the profile is `local`, or
  `local-llamacpp` beside another engine. Its version is read from standard error,
  it never parks, and it is refused while the system `config.ini` exists.
- **Until slices L2–L5 land**, a deployment on a llama.cpp profile is refused at
  resolution, every llama.cpp option (host-fixed, extra or rendered) is refused so
  the option policy fails closed, a parking residency on a llama.cpp profile is
  `capability_missing`, and the launch, park and group paths refuse the kind.

Tests (T42 with T01, T03, T07, T14, T21, T22, T37): `capyctl-config/tests/llamacpp.rs`
(kind, version line, listing rule under a test verified table, deep park, options
failing closed, the config file, the resolution refusal), `tests/registration.rs`
(a release before ADR 0029 skips a `llamacpp` profile as an unknown kind; this one
loads it), `engine_settings` unit tests (three ways, profile names, the host's own
llama.cpp), `capyctl-agent/tests/engines.rs` (detection runs nothing, links out of
the root, resolution, the version check on standard error),
`installation` unit tests (the digest over the libraries and `<prefix>/lib`),
`capyctl-cli/tests/engine_cli.rs` (`engine add` and its refusals),
`tests/grammar.rs` (`--llamacpp-bin`) and the standalone provider's own llama.cpp.
CPU and Fake-engine tests only; they are not qualification. Only the live rows
LC1–LC6 (slice L6) qualify llama.cpp.

## llama.cpp engine designed (ADR 0029) — 2026-10-09 (branch `docs/llamacpp-engine-design`)

Owner decision 2026-10-09: llama.cpp's `llama-server` (v0.6.0, tag `v0.6.0`, commit
`d812350`) becomes the fourth engine, restart-only and one model per deployment for
v1, landing as TensorFold did (ADR 0023). Recorded as
[ADR 0029](../design/adr/0029-llamacpp-engine.md), with the design note
`docs/specs/2026-10-09-llamacpp-engine-design.md` and the plan
`docs/plans/2026-10-09-llamacpp-engine.md` (slices L1–L6). SPEC §1 R01 and §9.4
admit llama.cpp; ADR 0018 carries a pointer to the bare-binary registration.

Adopted defaults: a bare-binary installation (version read from `--version` on
standard error, `build_fingerprint` `<version>+<commit>`, digest over the binary and
the shared libraries beside it); qualification on a maintainer's discrete-GPU machine
first, a lab-host build only after owner approval; no engine key and a loopback
listener; `-c` rendered as the window times the slot count with `--no-kv-unified`,
4 slots by default, explicit `-ngl` and cache types, `--fit off`, `--cache-ram 0`;
`--fit*`, `--sleep-idle-seconds`, router-mode options, `--rpc`, `--slot-save-path`,
`--api-key*`, `--tools` and `--agent` reserved; `/etc/llama.cpp/config.ini` and
`LLAMA_ARG_*` closed as hidden inputs; router mode unused; speculative decoding
through approved extra arguments with the draft model counted (ADR 0014 A6); sizing
from the GGUF header with a placeholder margin until live rows; per-request figures
from `timings` and load from `/metrics` and `/slots`; `cache_salt` refused;
template-driven tool and reasoning parsing. A deep tier waits for an upstream
single-model sleep and wake.

The claims were checked against the v0.6.0 sources (`tools/server`, `common/arg.cpp`,
`common/common.*`, `src/llama-context.cpp`, `src/llama-kv-cache.cpp`, the build-info
CMake files and the release workflow) and a CPU-only build of the tag run for
`--version`, `--help` and parser behavior. No GPU run, no lab host. The design
note lists the ten items that need a live check. Documents only: no code, no tests.
CPU and Fake-engine tests are not qualification; the live rows LC1–LC6 are.

## The compile-job count leaves out the model on unified memory — 2026-10-09 (branch `fix/max-jobs-unified-memory`)

Found live on a GB10 (catalog model 8, SGLang 0.5.21, `main` eb214aad): CapyCTL
set the engine's `MAX_JOBS` from `MemAvailable` at launch, 14 jobs there. SGLang
builds its FlashInfer JIT kernels after the model has loaded; on unified memory
the model had taken about 95 GiB of the same pool, about 20 GiB was left, the
parallel `nvcc` jobs took the machine to 116.4 GiB and the first start was
killed for memory twice (also with `--disable-flashinfer-autotune`). With
`MAX_JOBS=2` the first start built and became Ready in 1745 s, inside the
1800 s Initialize bound.

The rule (owner decision 2026-09-25, SPEC §13.3 amendment) now leaves out what
the engine itself holds when the builds run:
`MAX_JOBS = clamp(floor((MemAvailable at launch − unified Ready charge) / 8 GiB),
1, CPU count)`.

- The unified Ready charge is the sum of the deployment's Ready-phase
  allocations on `unified` domains (`engine_env::unified_ready_bytes`,
  `crates/capyctl-adapters/src/engine_env.rs`; SPEC §7.2, ADR 0007
  footprints). The Ready charge, not the startup charge, is used: the builds run
  once the model is resident, and a measured startup peak can itself be the
  build's (ADR 0014: a 111 GiB peak recorded during a 9-minute build). With the
  figures above, 117 GiB less 95 GiB gives 2 jobs.
- On a discrete GPU the model sits in the card's own memory: `device` and
  `distinct` domains add nothing, so a discrete host's count is unchanged
  (ADR 0019).
- The 8 GiB per job stays: each fused-MoE `nvcc`/`cicc` job held 7 to 9 GB on
  GB10 (2026-09-25), and the 2 jobs that the new rule gives here are the count
  that built.
- The SGLang launch (`NativeLaunch::unified_ready_bytes`) and the vLLM and
  TensorFold plans (`unified_ready_bytes`) carry the charge from the frozen
  effective deployment to Initialize. All three engines compile after the
  weights load (TensorFold builds extensions on its first request, ADR 0023
  §6).
- A `MAX_JOBS` in the resolved engine env (the profile's `env` or the
  deployment's `engine_config.env`, ADR 0028 §2.1) still wins; the host log
  now names it `engine env`, and a computed count names the Ready charge it
  left out.
- `docs/operations/install.md`, `docs/operations/configuration.md` and the
  host examples describe the rule and the override.

Tests: `engine_env` unit tests (a 95 GiB charge on 117 GiB gives 2 jobs, a
charge at or above what is available keeps the floor of 1, a set `MAX_JOBS`
wins whatever the charge, only `unified` domains count);
`vllm_frozen.rs` and `device_namespace.rs` check that the vLLM plan and the
SGLang launch carry the Ready charge on unified memory and none on a device
domain. The old rule gave 14 jobs for the 117 GiB case. CPU tests
only; they are not qualification. Live check still needed: a first start of
catalog model 8 on SGLang 0.5.21 on a GB10 with an empty JIT cache, with no
`MAX_JOBS` set, should log `MAX_JOBS=2` and become Ready without a memory kill.

## vLLM's safetensors loader stays `eager` by default while parking — 2026-10-15 (branch `fix/vllm-deep-wake`)

ADR 0014 amendment A21 and its note of 2026-10-15. On 2026-10-09 amendment A21 made a
parking vLLM deployment's undeclared loader follow the host's memory shape (`lazy` on unified
memory, `eager` on a discrete GPU), from a catalog comparison on host A (Qwen3.6-35B NVFP4,
vLLM 0.30, deep park, `eager` vs `lazy`: loading peak 54.2 vs 34.5 GiB, parked charge after
three cycles 28.3 vs 8.3 GB, wake to first token about 51 s vs 81 s). The same release
reverted it on live evidence from host A (GB10, vLLM 0.30.0, Qwen3.8-27B NVFP4, deep park,
run `target/live/0.1.3-backlog/2026-10-14-2311-release-checks/`):

- No declared loader (`lazy` default): cold start 601.6 s; the first wake's in-place weight
  reload took 123 s, beyond the 120 s `reload_weights` bound, so the wake ended uncertain and
  the instance stayed uncertain for 21 minutes until the deployment was deleted.
- Declared `eager`: six wakes of 23-28 s, each with a correct answer, parked charge flat at
  4.8 GiB.
- The `MRotaryEmbedding: Failed to load weights` warning appeared in both runs and is not the
  cause.
- The same branch (change file `vllm-deep-wake.md`) bounds a wake's weight reload by the
  wake window (180 s here) instead of a fixed 120 s, so that 123 s reload would no longer be
  cut; a `lazy` wake of this model has not been rechecked live.

What ships: absent a declared `engine_config.vllm.safetensors_load_strategy`, a parking
deployment resolves `eager` on unified memory and on a discrete GPU alike, as 0.1.2 did, shown
in the effective configuration with `provenance["vllm.safetensors_load_strategy"]` naming it
`capyctl default`. A declared `lazy` or `eager` wins and has no provenance entry. A
non-parking, undeclared deployment still resolves and renders no loader. The startup
placeholder uses `VLLM_EAGER_STARTUP_WEIGHTS_FACTOR` (3.6) exactly where
`--safetensors-load-strategy eager` renders (`VllmLaunchSettings::renders_eager_loader`), which
now includes the defaulted `eager` on unified memory; a declared `lazy` keeps
`STARTUP_WEIGHTS_FACTOR` (2.25).

Tests, failing before and passing after:

- `the_default_load_strategy_is_eager_on_every_memory_shape` (capyctl-adapters, renamed from
  `the_default_load_strategy_follows_the_host_memory_shape`): unified and discrete fixtures
  both resolve `eager` with a `capyctl default` provenance entry and render it; a declared
  `lazy` wins on either shape with no provenance entry.
- `sleep_mode_follows_the_host_deep_park_switch` and
  `the_load_strategy_follows_the_deployment_setting` (capyctl-adapters): the unified fixture's
  undeclared loader renders `eager`.
- `an_undeclared_strategy_stays_unset_without_parking` (capyctl-adapters): unchanged.
- `an_omitted_load_strategy_keeps_existing_identities` (capyctl-config): the unified fixture's
  omitted loader resolves `eager` with `capyctl default`; a declared `lazy` resolves `lazy`
  with no provenance entry; the command fingerprint stays pinned.
- `the_startup_placeholder_follows_the_effective_loader` (capyctl-config): a defaulted parking
  deployment on unified memory and a declared `eager` both use the eager factor; a declared
  `lazy` uses `STARTUP_WEIGHTS_FACTOR`.
- Recipe fingerprints re-pinned where the parking fixture's resolved loader changed
  (`an_absent_drafter_keeps_every_encoding_and_fingerprint`, `declared_resources_are_unchanged`,
  `a_single_host_deployment_is_unchanged`, `effective-vllm-golden.json`).

CPU and Fake-engine tests only; they are not qualification. Live check still needed: on host
A, model 1 on vLLM 0.30, deep park, no declared loader: the argv carries
`--safetensors-load-strategy eager`, the effective configuration shows provenance
`capyctl default`, and three wakes answer correctly.

## Observation server refusal tests tolerate an early close — 2026-10-09 (branch `test/observation-bridge-broken-pipe`)

`test_sglang_observation_server` `test_foreign_peer_never_reaches_bridge` failed
intermittently in the fast `scripts/ci-local.sh` runtime step with
`BrokenPipeError` at the test's `request()` (`sendall`). It passed when run
alone. `test_aliases_and_changed_parent_permissions_deny_connections` failed
once the same way. This was a test fault; the server is unchanged. The server
refuses both connections before it reads any request: the transport
authenticates the peer (`SO_PEERCRED`) before its first read, and the server
checks socket custody before handing the connection over. It then closes the
connection. The test connected and then sent its request. When the server's
thread ran first and closed the connection, the send failed with `EPIPE`,
before the test could see what it asserts: no response. The transport tests
already accept the same early close
(`test_broken_cancel_consumption_poison_disallows_new_requests`). `request()`
now accepts `BrokenPipeError` only for these two connections, which it is told
the server refuses. Every other connection must still send its whole request.

Tests. Every loop ran on 4 cores (`taskset`) beside 8 busy loops pinned to the
same cores.

- `test_foreign_peer_never_reaches_bridge`: before, 192/200, all 8 failures
  `BrokenPipeError`. After, 200/200.
- `test_aliases_and_changed_parent_permissions_deny_connections`: before,
  193/200, all 7 failures `BrokenPipeError`. After, 200/200.
- The whole module: before, 92/100 (9 `BrokenPipeError`s across the two tests,
  once both in one run). After, 100/100.
- A 200 ms pause between the client's connect and its send, which lets the
  server's thread run first, reproduces the failure deterministically. The
  module passed 0/5 before (both refusal tests `BrokenPipeError` every time)
  and 5/5 after.

CPU tests only; they are not qualification. No live check is needed.

## A start refused on available memory gives its figures — 2026-10-09 (branch `admission-figures`)

Found live on host A (standalone, `main` eb214aad): with `managed_limit 110GiB`
and `free_reserve 11GiB`, accepted because together they fit the 121.7 GiB
total, a deployment with a 109.25 GiB cold charge passed placement but was
refused at arm. SPEC §7.2 admits a charge only when the memory available now,
less the charge, leaves the free reserve (118.2 − 109.25 = 8.95 < 11 GiB).
`capyctl start --wait` then reported only `gave up: resource or evidence check
failed: insufficient resources` after three attempts, with no figures.

The rule is unchanged (uncertainty keeps accounting); the refusal now carries
its figures end to end:

- `ResourceError::InsufficientAvailable(AvailableShortfall)`
  (`crates/capyctl-domain/src/resources.rs`) holds the domain, the memory
  available, the candidate's charge, other starts' charges not yet resident,
  the free reserve to keep (on a device domain, the part ADR 0019 §2 does not
  absorb) and the shortfall. `admit_phase`'s free-memory check
  (`crates/capyctl-scheduler/src/residency.rs`) returns it; the managed-limit
  refusal stays `Insufficient`. It reads in capacity_blocked's wording under
  the closed code a host's own refusal uses: `insufficient_memory: needs
  109.2 GiB of system memory, 118.2 GiB available and a 11.0 GiB free reserve
  to keep, 2.1 GiB short` (`insufficient_device_memory` on a GPU). Every place
  that treated `Insufficient` from this check as a free-memory refusal (a
  park's own phase, a switch victim's parking, a grant that allocates nothing
  new) treats the new variant the same way.
- The operation's failure, its journal, `status` (`latest_operation.reason`,
  with the `insufficient_memory` hint) and the CLI carry that text.
  `start --wait` ends a start that gave up for memory with
  `insufficient_resources` (exit 4), as `capacity_blocked` does.
- When a standalone role starts, or an enrolled host publishes its policy, a
  domain whose managed limit plus free reserve exceeds the memory available
  then (plus what deployments already charged there hold) gets a warning with
  the figures in the role log and in `list hosts` (`session.memory_warnings`,
  and a `warning:` line under the table). Nothing is refused for it.
- `docs/operations/configuration.md` (standalone memory limits) explains that
  available memory, not total, must cover the charge plus the reserve;
  `docs/guide/errors.md` shows the new reason.

Tests: the scheduler's free-memory refusals now assert their figures
(`resource_admission.rs`, `resource_sequences.rs`); domain unit tests cover
the wording and when the warning appears; `admission_figures.rs` drives the
shipped CLI against an in-process standalone with 10 GiB of 32 available
(status JSON and text carry the four figures; `list hosts` warns, and does
not with all 32 GiB available); `live_profiles.rs` checks an enrolled
host's publication warns only when its limits exceed what it reports
available; `start_wait_replicas.rs` checks the `--wait` JSON error and exit 4.
CPU and Fake-engine tests only; they are not qualification. Live check still
needed: repeat the host A start and confirm the figures in `start --wait` and
the start-up warning.

## Two load-dependent failures: the subreaper fixture and the port probe — 2026-10-09 (branch `test/fix-two-flakes`)

Two tests failed once each under load and passed when run again.

`subreaper::tests::an_inherited_zombie_is_reaped_and_one_that_cannot_be_is_reported`
(capyctl-launchers, from #88) failed in CI "CPU checks" with
`left: Gone, right: Unknown` at `subreaper/tests.rs:159`. This was a test
fault. The fixture's background children exited on their own (`/bin/true`,
`sleep 0.5`) while their shell might still be a shell. A shell reaps a
background child that has already exited when it next finishes a command,
built-ins included: dash does this after `echo $!`. When the child exited
before that point, the shell reaped it. The "zombie of another live process"
was then already gone (CI's failure). Locally the more common symptom was
`timed out waiting: the child exits`. The children now run `sleep 30` and the
fixture kills them: the orphan only after its shell has exited, and the zombie
only once its parent has exec'd `sleep` (its `/proc/<pid>/comm`), which never
waits. No production code changed for this one.

`group_launch` `a_worker_answers_no_completion_probe` failed in the #97 deep run
with `service_port_in_use:23868`. That port came from the `capyctl_testkit`
pool (#92), and this was the first test of the binary to finish. The earlier
`sglang_worker_ignoring_sigterm_is_escalated` failures with the same signature
(ports 33274, 37834 and 41162) ran before #92. Every FakeHost test takes this
path. The cause was in the agent. `host_checks::port_free` and
`refusal::engine_port_free` probed with `TcpListener::bind`, which leaves the
probe socket listening until it is dropped. The agent forks engines from other
threads (`pre_exec`, so a real `fork`). A child forked while a probe was open
kept a listening copy of it until its `exec`, and the next probe of the same
port was refused as if another program held it. A group Launch probes its port
five times: three binds in `port_free`, then `engine_port_free` in pre-admission
and again under the journal lock. So a free port was refused while sibling
tests were spawning engines. In production this was a spurious, retried
refusal. The probes now go through `host_checks::probe_bind`, which binds with
`SO_REUSEADDR` (as `TcpListener::bind` does) and never listens. A copy of it
in a child refuses no later probe and no listener that sets `SO_REUSEADDR`.
It still refuses whenever anything listens on the address or on an address
overlapping it.

Tests. Every loop ran on 4 cores (`taskset`) beside 8 busy loops pinned to the
same cores.

- Subreaper test alone: before, 46/50 (three `timed out waiting: the child
  exits` and CI's `Gone`/`Unknown`). After, 50/50 and 200/200. Whole
  `capyctl-launchers` lib: before, 18/20 (both failures in this test). After,
  20/20.
- Port probe mechanism, as a standalone program: `port_free`'s three binds ran
  in a loop on a free port while 4 threads spawned `/bin/true` through `fork`.
  The listening probe refused 304,966 of 423,039 probes. The bound probe
  refused 0 of 225,553. Both probes refuse a listener on 127.0.0.1, 0.0.0.0,
  `::`, `::1`, 127.0.0.2 and a LAN address.
- In place: during the Launch of `a_worker_answers_no_completion_probe`, 16
  threads of the test process spawned `/bin/true` through `fork`, as sibling
  tests spawn engines. Before, 94/100, with six `service_port_in_use:<pool
  port>`. After, 100/100. Without that injection the failure is too rare to
  measure here: the whole suite passed 35/35 before at 4 test threads. After
  the fix, the failing test alone passed 50/50, the escalation test alone
  50/50, and the whole suite at 32 test threads 20/20.
- New unit tests `host_checks::tests::a_probe_left_open_holds_nothing` and
  `refusal::tests::a_probe_left_open_is_no_port_conflict` hold a probe's socket
  open, as a forked child would. Both fail with the listening probe and pass
  with the bound one.

Seen while stressing, not changed here: with five instances of the
`group_launch` suite at once on 4 busy cores,
`late_worker_children_are_recorded_and_unrecorded_survivors_stay_uncertain`
failed 50/50 at `group_launch.rs:1426`. Its Launch took longer than the 3 s
after which the fake starts its unrecorded child. Under normal load it
passes.

CPU and Fake-engine tests only; they are not qualification. No live check is
needed: the probe refuses the same holders as before.

## Local git worktrees live under `.worktrees/` — 2026-10-09 (branch `chore/ignore-worktrees`)

Owner decision 2026-10-09. Contributors and agents create git worktrees under
`.worktrees/<name>` inside the checkout instead of as sibling directories.
`.gitignore` now ignores `/.worktrees/`, so they never show up as untracked
files. No code or behavior change.

## Per-request engine metrics on every chat answer — 2026-10-09 (branch `feat/per-request-engine-metrics`)

Owner decision 2026-10-09: every completed chat response carries normalized
per-request engine metrics, and SGLang's cache report is on by default
(SPEC §17). The router adds `capyctl.metrics` to a non-streaming body and one
SSE comment, `: x-capyctl-metrics {...}`, before `data: [DONE]` on a completed
stream (beside the opt-in `x-capyctl-timing` comment; engine chunks are
relayed byte for byte). Figures: `ttft_ms`, `queue_ms`, `prefill_ms`,
`decode_tokens_per_second`, `cached_tokens`, each `{"value", "source"}` with
source `engine` or `router`, and absent when nobody measured it. Mapping
(`crates/capyctl-router/src/engine_metrics.rs`): vLLM 0.30 `metrics`
(`queue_time_ms`; `time_to_first_token_ms`, which runs from scheduling, as
prefill; their sum as TTFT; 1000 / `mean_itl_ms` as decode rate); TensorFold
0.6.x `tensorfold` (`time_to_first_token`, `prefill_seconds`,
`tokens_per_second`, whose 0.0 placeholder counts as unknown); cached tokens
from `usage.prompt_tokens_details.cached_tokens` for every engine. The router
derives TTFT (forward start to the first chunk with generated text) and the
decode rate ((completion tokens − 1) over first text to last chunk) only where
the engine reported none, which is always the case for SGLang 0.5.21. A
collecting forwarder now lends each chunk to the router (`ChatSink::collected`)
so a non-streaming answer is timed the same way, and a collected vLLM answer
keeps the `metrics` object from its usage chunk.

Launch defaults: vLLM renders `--enable-per-request-metrics` in its reserved
block (single-rank and group head; a headless worker has no API server), and
the protected entry compares `enable_per_request_metrics` (present in 0.29.0
and 0.30.0 `FrontendArgs`; read from the published 0.30.0 sdist). SGLang's
`enable_cache_report` is a reserved constant `True` (`fields/serving.py` in
0.5.20 and 0.5.21; 0.5.21 read from the published wheel). Both are in the
deploy-time reserved lists, so extra arguments cannot set or reverse them;
`docs/guide/engine-flags.md` has rows for both and for vLLM's
`--enable-prompt-tokens-details` (extra). `docs/guide/requests.md` documents
the figures and the per-engine table.

Tests (T14, T40, T41): router unit tests per engine on real-shaped bodies
(`vllm_metrics_map_and_nulls_stay_absent`,
`tensorfold_statistics_map_and_placeholders_stay_absent`,
`sglang_reports_cached_tokens_only`, `only_chunks_that_may_report_are_read`,
`attach_leaves_the_engine_fields`); router integration tests
(`crates/capyctl-router/tests/engine_metrics.rs`) per engine, non-streaming
and streaming, with relayed chunks checked byte for byte and absent-not-zero
cases; `a_collected_response_keeps_the_vllm_metrics_object`;
`per_request_metrics_render_by_default_and_are_reserved`; reserved spellings
in `engine_config.rs`, `sglang_args.rs`, `test_vllm_entry.py` and
`test_sglang_server_args.py`. `stream_safety.rs` expectations now include the
metrics comment. CPU and Fake-engine tests are not qualification.

Live check still needed (no lab host was used): on each engine (vLLM 0.30.0,
SGLang 0.5.21, TensorFold 0.6.5), one non-streaming request and one stream
with `stream_options.include_usage`, checking that the vLLM launch is accepted
by the protected entry with the flag, that `capyctl.metrics` and the comment
carry engine figures, and that SGLang reports `cached_tokens` on a repeated
prompt.

## DP attention head check covers P+13 — 2026-10-09 (branch `dp-attention-ports`, stacked on #86)

Owner decision 2026-10-09: extend the R33 pre-launch check (ADR 0028 §7) now, although `enable_dp_attention` is still reserved (path unreachable). Read against SGLang 0.5.21 source (`srt/server_args.py` `PortArgs.init_new`, `srt/managers/data_parallel_controller.py`): with `--dist-init-addr` the head binds `port_base..port_base+5` with `port_base = P+1`, or `P-7` when `P+6 > 65535` (the previous `P+7` trigger was off by one: at `P=65529` SGLang uses `65530..65535`), plus the REP handshake at `P + DP_ATTENTION_HANDSHAKE_PORT_DELTA` (13) with no fallback, plus one `get_zmq_socket_on_host` PUSH per DP rank on an ephemeral port. `host_checks::dp_attention_ports` now returns `P+1..P+6, P+13`, and `None` above 65522 (refused `rendezvous_port_in_use:<P>`); SGLang's own move of the six is therefore never reached. Per-rank ports documented as uncheckable in the guide, ADR 0028 §7 and the spec. Test `dp_attention_probes_the_derived_head_ports` (R33): the new assertions do not hold against the previous range-returning function, pass after. CPU tests are not qualification. Fast `scripts/ci-local.sh` passes; deep pending. Live check pending with MN1 (`ss -ltnp` on a DP-attention head) once DP attention is allowed.

## Group listeners documented — 2026-10-08 (branch `docs/multi-host-listeners`, stacked on #71)

Owner decision 2026-10-08: ADR 0028 decision 3 (trust the network, bind to the direct link where the engine allows, known risk) stands. The several-machines guide gains "Ports a group opens": per engine (vLLM 0.30, SGLang 0.5.21, TensorFold 0.6.5) every listener, its bind (link address, all interfaces, loopback or engine-chosen), no authentication and the pickle risk, and example nftables and ufw rules for the direct link. Sourced from vLLM 0.29 and SGLang 0.5.20 sources, TensorFold 0.6.3, and `crates/capyctl-adapters` group renderers; torch store, NCCL and TensorFold's `--parallel` port are marked inferred. Found from source: SGLang with DP attention also binds `P+13` (handshake) and one ephemeral socket per DP rank on the head's link address, which `Prepare` does not probe. ADR 0028 decision 3 carries the reaffirmation. Docs only; fast `scripts/ci-local.sh` run; deep pending. Live check pending: MN1 records the actual binds with `ss -ltnp` on both hosts.

## Engine options page and a memory-fraction mapping — 2026-10-08 (branch `docs/engine-flag-table`)

Owner decision 2026-10-08: no raw override of SGLang's `--mem-fraction-static`
(it stays reserved, ADR 0014 §3); a documented table and a mapping instead.
`docs/guide/engine-flags.md` has one table per engine (vLLM 0.30, SGLang 0.5.21,
TensorFold 0.6.5) classing common options as typed, extra, approval or reserved,
and a worked mapping from a target SGLang fraction to `memory.request` and
`memory.kv_cache` on `main` (8 GiB margin, 2 GiB static overhead; open PRs #74 and
#75 revisit both). `enable_linear_replayssm_spec` was found declared in the
published SGLang 0.5.21 wheel (`arg_groups/fields/exec_.py`); it is an ordinary
extra argument. `engine_policy::typed_options` is now public for the test.

Tests (T14): `the_engine_option_tables_match_the_policy` runs every tabled option
through `validate_extra_args` with no approvals and checks each row's class and
typed field, and that every policy typed option is tabled; `a_wrong_row_fails_the_check`
proves a wrong class, a wrong field and a missing typed row fail (a deliberately
wrong `--enable-mixed-chunk` row failed the first test before it was reverted).
CPU tests only; the engine option lists were read from the published vLLM 0.30.0
sdist, the SGLang 0.5.21 wheel and TensorFold's `v0.6.5` tag, not from the lab
hosts' environments, and no live run was made.

## A role behaves correctly inside a container — 2026-10-08 (branch `feat/container-readiness`)

Owner decision 2026-10-08: make a host or standalone role correct as a container's main process now; the live container run on a GB10 comes later. Four changes. (1) Memory (SPEC §7.2, T26): `memory::read_host_memory` bounds `/proc/meminfo` by cgroup v2. The process's unified cgroup comes from `/proc/self/cgroup`; for it and every ancestor under `/sys/fs/cgroup` with a numeric `memory.max`, capacity is `min(MemTotal, memory.max)` and availability `min(MemAvailable, memory.max − (memory.current − inactive_file))`, the tightest level winning. Crediting `inactive_file` is a deliberate refinement of the requested `memory.max − memory.current`: without it a model's weights in the page cache read as used. The source travels as `HostMemorySample.source` and in the additive proto field `DomainObservation.memory_source` (`meminfo`, `cgroup_v2:<cgroup>`, `meminfo:cgroup_v1_unread`, `meminfo:cgroup_v2_unreadable`). The server keeps it in `DomainView` (bounded like other names, status only), standalone's host domains report it, and the role logs it at start when it is not plain meminfo. cgroup v1 is out of scope: never read, reported as such. Every consumer of the host reading follows the bound: the unified (GB10) domain, a discrete host's `distinct` system domain, admission refusal, standalone's derived limits and doctor. Engine `MAX_JOBS` sizing (`capyctl-adapters` `engine_env`) still reads the machine's `MemAvailable`; documented. (2) Reaping (SPEC §13.2, T12 T33): new `capyctl_launchers::subreaper`. At start a role sets `PR_SET_CHILD_SUBREAPER` (PID 1 reaps without it) and sweeps once a second, reaping every exited child it did not spawn itself. Its own direct children keep their waiters: a child in CapyCTL's process group, or one spawned through `subreaper::spawn_direct` (ExecLauncher, DurableSpawn, the installation probe and the engine version check; spawn and registration happen under a shared gate the sweep takes exclusively). `process_absence` reads an exited, single-threaded zombie as `Observed::Exited`. When the role is its parent it reaps it on sight, and the next reading proves it gone. Otherwise it is `Unknown` and a recorded set reports the new `GoneProof::Unreaped`, so it is no longer read as alive forever. Termination no longer refuses over such a process (it holds only its pid), and a zombie leader's group is scanned by pgid. PID-reuse protection is unchanged: start ticks are matched before every reap, and only the parent can reap a zombie, so its pid cannot change before the wait. Known limit: an orphan left in CapyCTL's own process group (a grandchild of a plain `Command`) is never reaped. (3) `nvidia-smi` (T26): `gpu_memory::nvidia_smi` tries `/usr/bin` and `/bin`, then each absolute `PATH` directory, and only executable files. Used by the GPU sample (and so the device inventory's corroboration) and by process residency, with the 3 s bound unchanged. (4) Docs: "Running in a container" in `docs/operations/install.md`. CPU tests only (cgroup fixtures, a re-executed subreaper fixture that reaps an inherited zombie and reports an unreapable one, PATH lookup, server and proto round trips); not run in a container or live. The live container run on a GB10 is pending.

## Standalone reports its own engine load — 2026-10-08 (branch `fix/standalone-load-reports`)

Owner decision 2026-10-08 (SPEC §§10, 17, ADR 0013 §10, D9): an external
router needs the engine's running and waiting counts and its real running
limit on a single machine too. Until now the standalone role sampled nothing:
it composed `GET /management/v1/metrics/load` with no load table
(`remote_roles::load_view(.., None)` in `roles.rs`), and its
`CoordinatorLifecycle` had no load to read, so every embedded instance read
`sample: null`, `max_running` stayed derived (`declared`, `default`, ...), and
the router scored embedded instances on its own in-flight count alone
(`load_source: router_in_flight`). The host agent's `LoadReporter` was the only
sampler, and it read its targets only from a host ingress, which standalone
does not have. This supersedes the "Standalone has no host load report" line of
the load-read entry below.

Standalone now samples its engines in process with the host agent's reporter.
`capyctl_agent::load::LoadSource` names what a reporter samples; the host
ingress implements it as before, and `capyctl_controller::embedded_load`
implements it from the store: every Ready embedded launch of the current
session with open dispatch (`local_ready_launches`, which now carries the
instance index), on its recorded loopback endpoint with its sealed inference
key, under the embedded host's published name. Each second it scrapes
`/metrics`, reads SGLang's `/v1/loads?include=core` once per launch, validates
each report (`LoadReport::try_from`) and records it in its own `LoadTable` with
the same fences a host report meets. The management load read and the router
read that table: `CoordinatorLifecycle::with_embedded_load` gives an embedded
instance its fresh sample by its placed host, and `balance::usable_load` now
matches a sample to the remote host, else the placed host. A sample's
`ingress_in_flight` is the router's count for the instance, since standalone
forwards to its engine directly. Engine latency histograms carried on the
samples are not read in standalone; its latency view is unchanged. The guide's
[reading load](../guide/parking.md#reading-load) section no longer says
standalone has no sample.

Tests: `standalone_reports_its_engine_load` (`capyctl-cli`, `roles_f1`) boots
standalone on the Fake installation, starts a deployment and stands an
SGLang-shaped engine at the recorded endpoint that answers keyed `/metrics` and
`/v1/loads`; the load read shows a fresh sample with the engine's gauges and
`max_running` `{"count": 24, "source": "engine"}`, and the router's candidate
uses that engine load. It failed before (`sample: null`, source `default`) and
passes after. `unusable_samples_fall_back_to_router_in_flight` now expects an
embedded instance's sample to be used when it names the placed host, and not
otherwise. CPU and Fake-engine tests are not qualification.

Live check still needed: rerun live check #2 on host A (standalone, SGLang
0.5.21, `max_concurrent_requests: 2`): during an in-flight request the load
read shows a fresh `sample` with `engine.running` and `max_running.source:
engine`, and the router's candidate logs `load_source: engine`.

## Tables an engine keeps on disk count against the models disk — 2026-10-09 (branch `feat/disk-backed-weights`)

Owner decision 2026-10-09 ([ADR 0014 amendment A20](../design/adr/0014-deployment-engine-configuration.md)).
Every safetensors byte counted as memory weights, so Qwen3.8-Flash-Next (126 GiB, 47.7 GiB of
it one n-gram table) never fit a 121.7 GiB GB10. The host now reads the n-gram table shards
(`ple_embedding.ngram_embedding.shard_<n>.{weight,scales,biases}`) from the safetensors headers
in the layout pass and reports them with the digest. When the engine arguments turn on SGLang
0.5.21 `--ple-offload-backend file` or TensorFold 0.6.5 `--ple-on-ssd` (vLLM 0.30 has none),
the memory weights are the rest of the checkpoint (a group member: its share of it) plus the
engine's cache of each table (SGLang 8 GiB, its default budget; TensorFold 1 GiB). The record
is `engine_config.memory.disk_tables`; launch plans carry `checkpoint_tables` (capability
`checkpoint_tables`), which the host checks against its own headers. Without the option or the
tables nothing changes: fingerprints pinned from main. Setting
`SGLANG_QWEN4_PLE_FILE_RSS_BUDGET_GB` beside the file backend is refused.

Tests, failing before (rule disabled: 5 of 7 in `crates/capyctl-config/tests/disk_tables.rs`
fail) and passing after: `flash_next_fits_one_gb10_with_sglang_keeping_the_table_on_disk`,
`flash_next_fits_one_gb10_with_tensorfold_reading_the_table_from_ssd`,
`flash_next_does_not_fit_one_gb10_without_the_option`,
`a_group_member_keeps_its_share_of_the_table_on_disk`,
`without_the_option_the_tables_change_nothing`, `a_changed_sglang_table_budget_is_refused`,
`the_engine_options_are_recognized`; plus `tables_are_found_beside_the_layout`,
`measured_tables_size_an_engine_that_keeps_them_on_disk` (store),
`the_checkpoint_tables_ride_beside_the_weights` (protocol) and the agent's plan check. CPU
tests only; not qualification.

Live check outstanding: model 8 (Qwen3.8-Flash-Next) on one GB10 with SGLang
(`--ple-offload-backend file`) and with TensorFold (`--ple-on-ssd`). The derived Ready phase is
about 108.5 GiB, above the automatic managed limit (97.4 GiB), so the host needs a managed
limit of about 110 GiB and a declared startup peak, or a first start that measures it. SGLang's
file backend also writes its own sparse copy of the table under its cache directory, outside
the model store.

## Wake canary for single-host launches — 2026-10-09 (branch `single-host-wake-canary`)

Owner decision 2026-10-09. Single-host wakes compare the engine's answer with a
reference, as groups do (ADR 0028 §12). At a launch's first readiness the
coordinator (`worker::record_wake_canary`) asks `EngineAdapter::wake_canary` for one
completion of `completion_probe::PROMPT` (`CANARY_TOKENS` = 8, temperature 0,
bounded by `CANARY_DEADLINE`) and stores the token ids and text in the new
`wake_canaries` table (schema v48), keyed by instance, generation and incarnation:
a controller restart keeps it, and a new launch records its own (earlier
generations are dropped). After a restore's own steps succeed and before its
evidence commits, `wake_canary_check` repeats the probe. Token ids are compared
with the group rule (`canary_matches`) when both answers carry them, the text
otherwise (`wake_canary::verdict`); a launch with no reference records one; an
unanswered canary leaves the wake uncertain, as a failed fresh probe does. A
different answer records `wake_mismatch` (the instance's last error and
`mismatch_operation_id`), marks the wake uncertain and accepts an ordinary stop
under `EXIT_PRINCIPAL` (as after an engine exit), retried every scheduler pass from
`wake_mismatch_stops_due`, so a restart retries it too. Every wake of a single
launch is checked, deep and `host_backed` alike.

Remote launches run the probe on the host agent (`MemberAction::Probe` with
`max_tokens`, sent only to hosts declaring `engine_groups`; older hosts keep the
wake checks they had); embedded vLLM and SGLang probe their own endpoint;
TensorFold is restart-only and unaffected. The completion probe now answers
`ProbeAnswer { tokens, text }` (adapters `complete_probe`, renamed from
`complete_token_ids`; new `MemberExecutionResult.probe_text`, at most 4096 bytes),
so an engine that returns no token ids is compared by text; groups still require
token ids. New closed code `wake_mismatch` (CLI codes, errors guide, parking guide).

Tests (new APIs, so they did not build on `main`; all pass after):
`capyctl-controller` `tests::residency::a_matching_wake_canary_completes_the_wake`
(remote and embedded, three cycles), `a_differing_wake_canary_fails_the_wake_and_stops_the_instance`,
`a_new_launch_records_a_new_reference`, `a_controller_restart_keeps_the_reference`,
`wake_canary::tests::*`; `capyctl-store` `wake_canary::tests::*` (including a
reopened store); `capyctl-protocol` `probe_text_answers_a_completion_probe_only`;
adapter `completion_probe` unit and integration tests for the text fallback. CPU
and Fake-engine tests are not qualification.

Live check still needed: model 1 on vLLM, deep park and wake three times on a
GB10, each wake passing the canary. A known-broken wake is refused upstream
(#76), so a mismatch is tested by hand: on a maintainer's local standalone
machine (never a lab server), with a vLLM model ready, stop the role, run
`UPDATE wake_canaries SET tokens_json='[1]', text='x'` on its store, start the
role (it adopts the running engine), then park and wake the model; the wake must
fail `wake_mismatch` and the instance stop, `wake_mismatch` as its last error.

## Draft model source — 2026-10-08 (branch `feat/draft-model-source`)

A deployment may declare `model.draft`, its speculative drafter's own model source (ADR 0008 amendment 2026-10-08): validated and policy-checked like `model.source`, part of the revision fingerprints (absent: unchanged), materialized and verified as a second source row per host (store schema 47, `model_sources` keyed by source), and waited for before activation, digest and launch. Its weights are sized with the checkpoint's without approvals, its `config.json` feeds the draft KV fit, and CapyCTL renders it to SGLang (`speculative_draft_model_path` via the entry), vLLM (`--speculative-config` `model`, merged with the operator's) and TensorFold (`--drafter`). CPU and Fake tests only; a native SGLang or vLLM launch with a materialized drafter on a lab host is still needed.

## Faster checkpoint readiness — 2026-10-08 (branch `feat/digest-readiness`)

Owner decision 2026-10-08 (ADR 0014 §7, amendment of 2026-10-08): option (a) now, option (b) only with host approval. (a) `sources.rs` computes each file's SHA-256 while it streams (beside the git blob id where that is the pin), notes it with the file's stat identity right after the rename, and writes an owner-only `<id>.manifest` at commit; `CheckpointVerifier` adopts it when the walked files and identities match and the small files rehash to it (`fetched`, no weight file read; same canonical manifest, so the same digest). Tar extractions and unnoted files are measured. (b) `checkpoints.trust_declared_digest` (flag `--trust-declared-digest`, `CAPYCTL_TRUST_DECLARED_DIGEST`, YAML; default off) lets a host take a local source's canonical declared digest on first sight, hashing only small files and seeding the stat cache; a changed file forces a full rehash, the policy off forces a measurement, and the store records `declared_trusted` only for the revision's own declaration. Provenance travels as `CheckpointDigestEvidence.provenance` (field 10, additive), is stored in `checkpoint_digests.provenance` (v46; earlier digests `measured`) and shown by status.

Tests (T03 T14 T34): `a_fetched_checkpoint_digest_needs_no_second_read` (reads counted: only small files), `a_changed_or_unpinned_download_is_measured`, `a_declared_digest_is_trusted_only_when_the_host_allows_it`, `a_trusted_declaration_still_catches_a_changed_file`, `digest_checkpoint_trusts_a_declaration_only_by_host_policy`, `the_embedded_host_trusts_a_declared_digest_only_by_policy`, `a_recorded_digest_keeps_where_it_came_from`, `a_recorded_digest_keeps_its_provenance`, `v46_marks_earlier_digests_measured_idempotently`, `declared_digest_trust_follows_flag_env_document_default`, `status_shows_how_a_digest_was_recorded`, protocol validation. CPU tests only; not qualification. A live Hugging Face download and first start on a lab host is pending. Schema v46 and the dated ADR amendment may collide with other open branches that also add v46; renumber on merge.

## Plain HTTP by host approval, download URLs by secret reference — 2026-10-08 (branch `feat/source-extensions`)

Owner approval 2026-10-08, recorded as the ADR 0008 amendment of that date. An `http` model
source may now name a plain `http://` URL; the host fetches it only when it approves plain HTTP
(`model_sources.plain_http: allowed`, `--model-sources-plain-http`,
`CAPYCTL_MODEL_SOURCES_PLAIN_HTTP`; flag over environment over YAML; default `denied`, encoded
only when allowed, so existing host policies and revisions are byte-identical), still inside
`http: allowed` and `allowed_hosts`, still verified against its `sha256`. A fetch that starts on
HTTPS is never redirected down to plain HTTP. An `http` source may instead state
`url_ref: secret://<name>` (a deployment-document field, YAML only like every deployment
field): the host reads the URL from `<state_dir>/secrets/<name>` (owner-only) at the moment of
the fetch and checks it against its own policy first (`denied` otherwise,
`secret_unavailable` for a missing, loose or non-URL secret). The document, revision, ledger,
status and logs hold the reference only; the store directory stays `sources/http/<sha256>`.
The sources store's log lines pass through the new shared
`capyctl_domain::redact::redact_urls` (userinfo, query and fragment become `<redacted>`).

Tests (T14 T37): `plain_http_sources_need_the_hosts_approval`,
`a_secret_url_reference_is_all_a_declaration_holds` (config, runtime failures before the
change), `plain_http_follows_flag_env_document_default`,
`plain_http_and_secret_url_sources_resolve_into_the_store`,
`a_secret_url_is_fetched_verified_and_never_persisted_or_logged` (no stored file, status,
failure or log line holds the URL), `plain_http_sources_are_fetched_only_where_the_host_approves`
(agent, loopback origin), and the `redact` unit tests (domain). CPU tests only; they are not
qualification. A live fetch from a plain-HTTP mirror and from a presigned URL on a lab host is
pending.

## Engine logs kept, redacted as written, with a management tail — 2026-10-08 (branch `engine-log-redaction`)

Owner decision 2026-10-08, SPEC §13.3 amended. Every engine's stdout and stderr now pass through a redacting writer process (the `capyctl` binary re-executed as `/proc/self/exe __engine-log-relay`, own process group, outlives an agent restart, ends at EOF) on the way to the launch's owner-only log (`crates/capyctl-launchers/src/engine_log_relay.rs`, used by `DurableSpawn` and `ExecLauncher`). It replaces the launch's credentials by value (protected-descriptor keys and observation credential, secret-named environment values, handed over on descriptor 3) and by rule URL userinfo, URL query strings and fragments (`capyctl_domain::redact::redact_urls`, shared with the model-source work), secret-named assignments, `hf_` tokens, bearer values and ≥48-character credential runs (`capyctl_adapters::engine_log::LogRedactor`). The log rotates at 16 MiB keeping `.1` and `.2`. SGLang now runs at `log_level`/`log_level_http` `info` (was `error`) and its output is no longer sent to `/dev/null` (`contain_startup_output` removed; the Python scrubber now runs in both modes); `log_requests` stays off, crash dumps stay off, and SGLang's refusal reason is logged in both modes. `--debug-engine-logs` keeps its meaning (SGLang debug level, output written without the writer) and leaves a `<log>.raw` marker. New route `GET /management/v1/deployments/{id}/engine-log?instance=<n>&kib=<1..256>` (admin token; server through the new `ExecuteMember.engine_log_tail` action, capability `engine_log_tail`, host→server message limit 320 KiB; standalone in-process) returns a bounded, re-redacted tail; a raw log is refused `403 forbidden`.

Prompt evidence at default level (source read, not live): SGLang 0.5.21 logs request text only under `log_requests` (`srt/utils/request_logger.py:88-90, 159-163`; OpenAI path `serving_base.py:87-89`), and logs `server_args=` with both keys at info (`srt/entrypoints/engine.py:1123`), which the writer redacts by value; vLLM 0.30.0 logs prompts only with `--enable-log-requests` (default off, `engine/arg_utils.py:3000`; prompts at DEBUG, `serve/utils/request_logger.py:44-61`), reserved by CapyCTL; TensorFold (0.6.3 read; 0.6.5 not available locally) prints request lines and token counts only and has no request-logging switch.

Tests (T21, T34): `crates/capyctl-launchers/tests/engine_log_relay.rs` (`a_launch_log_holds_no_owned_secret`, `a_worker_log_holds_no_observation_credential`, `the_exec_launcher_log_is_redacted_too`: seen failing with redaction disabled), `engine_log_relay::tests::*` (rotation, split reads, overlong lines), `capyctl_adapters::engine_log::tests::*`, `crates/capyctl-cli/tests/engine_log_relay.rs` (the built binary in writer mode), `runtime/tests` (`test_info_is_the_default_level_and_request_logging_stays_off`, `test_default_guard_keeps_output_and_still_rejects_plugins`, `test_output_is_scrubbed_of_credentials`: seen failing on the previous runtime), `vllm_args::request_logging_is_never_rendered_and_cannot_be_enabled` (locks existing behaviour), `crates/capyctl-management/tests/engine_log.rs` (bounds, redaction, raw refusal, auth), protocol `engine_log_tail` tests, `version_skew::an_engine_log_tail_needs_the_capability_and_is_otherwise_answered`, agent `engine_log_tail_serves_a_claimed_launch_bounded_and_redacted`. CPU tests only; not qualification. Live check needed: SGLang 0.5.21 and vLLM 0.30.0 on a GPU host, grep the engine log for the launch's keys and for a driven prompt's text, confirm SGLang's info-level warnings now appear, and read the tail route on server and standalone.

## A park the parked growth bound turns into a stop runs the stop — 2026-10-09 (branch `fix/parked-growth-stop-runs`)

Found live 2026-10-09 on host A (standalone, vLLM 0.30.0 deep, eager
loader). This fixes ADR 0014 amendment A19. A launch's parked charge grew
4.7, 9.6, then 13.1 GiB. Its fourth `capyctl park deployment` became the
growth stop, and status said so. That stop never ran. Its operation stayed
`pending` and its run `queued` for 25 minutes, and the generation-1 engine
kept its 22.6 GiB. An operator's Stop was refused with `lifecycle_conflict`,
and `drain host` asked for reconciliation. A role restart did not clear it.

Cause: `accept_park_command` answers with the stop's operation. It stored the
park's own command receipt (the deployment's `#park` scope, or a sibling
instance's) naming that operation, beside the receipt the stop already has.
The ordinary cleanup's validation (`cleanup::read`) requires exactly one
receipt per cleanup operation. So every read of that stop failed with
`CorruptStoredData`: discovery, arming, status and adoption after a restart.
The coordinator's worker halted with `corrupt stored lifecycle data`. Nothing
was released: the accounting stayed held, with the engine running. The
generation 2 in the ledger is normal: every stop fences its launch to the
next generation. The idle-policy and switch-victim growth stops store only
the stop's own receipt, so they were not affected. Their tests now drive
their stops too.

Fix: the cleanup validation also admits the park command's receipt, and only
that one. It must be under the stop's principal and key, in the deployment's
or this instance's `#park` scope, for a stop journaled `park_growth_stop`.
Any other extra receipt is still corruption. An exact retry of the park
still replays the stop, also after the stop has settled. The stop still
releases only on its gone evidence.

Tests, failing before and passing after:

- `a_park_the_growth_bound_turns_into_a_stop_runs_the_stop` (controller
  scheduler, scripted embedded engine whose host reports growing parked
  residues). Three parks and wakes are measured at 3, 5.5 and 7 GiB. The
  fourth park becomes the stop. Before the fix, the worker read
  `Failed("coordinator service failed: corrupt stored lifecycle data")` with
  the stop `pending` and the instance at generation 2, observed `ready`, as on
  host A. After the fix, the engine is terminated once, the instance is
  released and reads `stopped`, the fourth park is never sent, the worker keeps
  running, and an operator's Stop afterwards is accepted.
- `a_park_after_the_parked_charge_outgrew_its_first_park_is_a_stop` (store).
  The stop is discovered, armed and released on gone evidence, and a retry
  replays it before and after. Before the fix, discovery failed with
  `CorruptStoredData`.

New: `a_growth_stop_refuses_a_park_receipt_it_did_not_answer` (the same
receipt under another principal is still corruption). The idle-policy and
switch-victim growth tests now also drive their stops; they passed before.

CPU and Fake-engine tests only; they are not qualification. Live recheck
still needed: on host A, on a fresh ledger, repeat the four park/wake cycles
on vLLM 0.30.0 deep and confirm the fourth park's stop terminates the
engine, releases it and leaves it stopped.

## Explicit activation — 2026-10-08 (branch `feat/explicit-activation`)

Owner-approved 2026-10-08. `lifecycle.activation: explicit` (deployment
document, or a host document for every deployment that may run on it;
standalone `host.lifecycle.activation`, also `--set` / `CAPYCTL_SET__…`)
makes a deployment move only on an operator's action (SPEC §6.5, §10
amendments). A request while it is stopped or parked is refused at once, 409
`deployment_inactive`, with nothing accepted; it is never a switch victim,
never idled, never reclaimed from the parked set (a park that needs its room
is refused `parked_capacity`). An operator's stop keeps `deployment_stopped`.
Schema v45 adds `deployment_revision_instances.activation` (`on_demand` for
existing rows) and `host_activation_policies`, written with each approved
host publication and at every standalone start. Unset: command identity,
recipe fingerprints and the host's resolution document are unchanged (tests).
Status shows `activation: explicit` (additive).

Tests (failing before, passing after): config `instances`, `remote_roles`,
`setting_overrides`; store `instances_placement`, `host_publication`,
`park_tests`, migration v45; controller `tests_switching`; CLI
`explicit_activation` (HTTP 409 end to end, park and wake by operator).
CPU and Fake-engine tests only; not qualification. Pending: a live check on a
lab host (explicit host, a request for a parked deployment answered 409 with
no wake, then `start` waking it; a second deployment's request not evicting
it). Under a `resource_policy.queue.max_pending_per_deployment` of 0 the
refusal is still 409 `deployment_inactive`, not 429 `queue_full`, since no
activation would start (CLI test
`with_no_waiting_an_inactive_explicit_deployment_is_refused_409`).

## Binary-driven CLI tests no longer read the machine's free memory — 2026-10-08 (branch `test/standalone-memory-independent`)

Owner decision 2026-10-08. The `capyctl-cli` integration tests that spawn the
real binary (standalone and host roles) read this machine's own
`/proc/meminfo`. With about 14 GiB available of 61, every deploy failed with
`insufficient resources`, on clean `main` too: standalone derives a 20% free
reserve, and the golden host states a 16 GiB one. In debug builds only,
`capyctl_agent::memory::read_host_memory` now reads the test-only
`CAPYCTL_TEST_PINNED_HOST_MEMORY` (`<capacity_bytes>:<available_bytes>`) when
it is set. Release builds compile that read out. It is not a user setting and
is not documented as one. `support::capyctl()` pins every role it spawns to the
in-process suite's stated 32 GiB, all of it free. This goes through
`support::pin_host_memory`, which the two helpers that clear the environment
call again. The in-process tests already state their memory
(`support::test_memory`). The real `/proc/meminfo` path keeps its own tests
(`crates/capyctl-agent/tests/memory.rs`). On a discrete machine, GPU memory is
still sampled from `nvidia-smi`.

Tests: the five `role_shutdown` and two `engine_exit` standalone tests named in
the decision failed before with 14 GiB available. With a held allocation keeping
13.5–14.8 GiB available, all 20 binary-driven test files passed 3 times
(163 tests each). `cargo test -p capyctl-cli --all-targets` passed (463 tests)
with 13.7 GiB available. New unit tests check the pinned reading's format.
Still open: in one earlier run,
`a_remote_engine_exit_is_reported_settled_and_relaunched_on_demand` waited out
its 900 s deploy deadline. Its reason was not captured, and 14 later runs
passed, 10 of them on 2 cores. CPU and Fake-engine tests only; they are not
qualification. No live check is needed.

## Change files instead of runbook and release note edits — 2026-10-09 (branch `doc-changes-convention`)

Owner decision 2026-10-09. Every pull request added its entry at the top of
this runbook and to `docs/operations/release-notes-0.1.3.md`, so each merge
made every open pull request conflict. A pull request now adds one file,
`docs/changes/<short-slug>.md`, with `# Status: <title>` (this entry) and
`# Release note: <section>` (or `none`); the format is in
[docs/changes/README.md](../changes/README.md). When a release is cut,
`scripts/assemble-changes.sh [--dry-run] <version>` puts the status entries at
the top of the runbook, newest first by the commit that added each file, and
each release note after the last bullet of its section in
`release-notes-<version>.md` (adding a missing section at the end, or the
file), checks every file first (headings, `../` relative links) and removes
them. AGENTS.md, CONTRIBUTING.md and [Releasing](../operations/releasing.md)
describe the convention; the runbook stays the single status authority and no
existing entry moved. CI and `scripts/ci-local.sh` run
`scripts/test-assemble-changes.sh` (step `changes`) and shellcheck both
scripts.

Tests: `scripts/test-assemble-changes.sh` (dry run changes nothing; runbook and
notes compared byte for byte; second run is a no-op; missing notes file
created; eight refusals leave every file unchanged). It failed before the
script existed and passes after, with mawk and busybox awk. Not added: a check
that a pull request touching code adds a change file (proposed in the pull
request).

## `cache_salt` passed to vLLM and SGLang, refused for TensorFold — 2026-10-08 (branch `feat/cache-salt-passthrough`)

SPEC §10 note (owner decision 2026-10-08): `cache_salt` joins `CHAT_REQUEST_FIELDS` and is forwarded unchanged; `validate_chat_request` bounds it to a non-empty string of at most `MAX_CACHE_SALT_BYTES` (1024, vLLM's own bound; null is absent). `RuntimeEndpoint` now carries the engine family the launch's revision froze (`Store::revision_engine`, `$.profile.engine`), and the router's forwarder honours the salt only for `vllm` and `sglang` (`engine_honours_cache_salt`); for TensorFold, or an unrecorded family (a deployment from before managed configuration), the forwarder refuses before connecting with the new `AdapterError::CacheSaltUnsupported`, answered `400 cache_salt_unsupported` (in-band on a stream) with the lease closed not accepted. The engine adapters' own forwarders follow the same rule. Tests: `cache_salt_reaches_vllm_and_sglang_unchanged`, `cache_salt_is_refused_for_an_engine_that_would_ignore_it`, `a_malformed_cache_salt_is_refused_before_any_engine` (router), `cache_salt_is_allowed_and_its_value_bounded`, `a_forwarder_for_an_engine_ignoring_cache_salt_refuses_it_before_sending` (adapters). CPU tests only. Live check still needed: on a real SGLang (and vLLM 0.30) deployment, an identical long prefix sent twice under one salt reports `cached_tokens` > 0 on the second request, and under a second salt reports 0.

## Load read for external routers and a queue of zero — 2026-10-08 (branch `feat/management-load-route`)

Owner decision 2026-10-08 (SPEC §10 note). `GET /management/v1/metrics/load[?deployment=<id>]`
on the management listener (admin token; an ID naming no deployment is `404 not_found`; two
blocking reads at a time, the rest `429 queue_full`) reports per deployment the router's
in-flight and waiting requests with their bounds, and per instance its router in-flight count,
the latest host sample (time, age, `fresh` under the 3 s stale bound, ingress in-flight, engine
running/waiting/KV ppm or `null`) and `max_running` with its source. The engine's own figure
(`source: engine`) comes from SGLang's loopback `/v1/loads?include=core` (`max_running_requests`
summed over data-parallel ranks), read by the host agent once per launch with the launch's key and
carried on `LoadSample.max_running` (proto field 11, optional, additive both ways); otherwise the
derived value names `declared`, `default`, `state_cache`, `extra_args`, `host_fixed`,
`engine_default` or `on_host` (`context_fit::max_running_for_effective`, as status fits it). vLLM
exposes `max_num_seqs` only on a development route behind the admin key and TensorFold not at all,
so both stay derived. Standalone has no host load report, so its instances read `sample: null`.
`resource_policy.queue.max_pending_per_deployment` now accepts 0 (YAML, `--set`,
`CAPYCTL_SET__…`; tightest host wins): a request beyond the in-flight bound is refused at once with
`429 queue_full` and `Retry-After: 1`; a request for a deployment that is not servable is refused
the same way after starting (or joining) its detached activation. The in-flight bound stays 32:
load samples are routing hints, never admission evidence, so the router does not follow the
engine's reported limit (requests above it wait in the engine and show as its `waiting`).
Recipe fingerprints are unchanged, with or without the setting.

Tests: `a_host_queue_of_zero_lets_no_request_wait` (failed before: `invalid bounded resource
controls`), `with_no_waiting_a_request_is_refused_at_once_but_starts_the_activation` (failed
before: no activation), `with_no_waiting_a_request_beyond_the_in_flight_bound_is_refused_at_once`,
`standalone_queue_of_zero_is_set_three_ways`, `the_running_limit_names_its_source`,
`the_running_limit_of_a_tensorfold_launch_is_its_streams`,
`the_engines_running_limit_travels_with_the_sample`, `the_engines_running_limit_is_kept_with_its_sample`,
`sglangs_running_limit_is_read_from_its_loads`, `an_sglang_launch_reports_its_running_limit_once_read`,
`the_load_read_names_deployments_and_their_instances` (store),
`the_load_read_reports_fresh_then_stale_then_no_sample`, `unknown_load_is_null_never_zero`
(router), `the_load_read_is_authenticated_filtered_and_names_unknown_deployments` and
`the_load_read_serves_fresh_then_stale_samples` (management). CPU and fake-engine tests only; they
are not qualification. Live check still needed: SGLang 0.5.21 on an enrolled host answers
`/v1/loads?include=core` with the launch's inference key and its `max_running_requests` matches the
limit it enforces (a hybrid model capped by its state cache is the interesting case); the fixture
is SGLang 0.5.20's shape.

## Streamed delta fields audited — 2026-10-08 (branch `fix/relay-delta-allowlist`)

The SSE relay's delta allowlist (`STREAM_DELTA_FIELDS`, capyctl-adapters forward.rs) was checked against every `DeltaMessage` field vLLM 0.29.0, SGLang 0.5.20 and TensorFold 0.6.0/0.6.3 serialize on `/v1/chat/completions`: `role`, `content`, `reasoning` (vLLM), `reasoning_content` (SGLang, TensorFold), `tool_calls` — already all listed, so no new key; SGLang `hidden_states` stays excluded (only for the refused `return_hidden_states`), vLLM `citations` is Cohere-endpoint only. Choice fields (`logprobs`, `stop_reason`, `token_ids`, `matched_stop`) and usage details were never key-checked and survive. CPU tests replay real-shaped chunks per engine. Not run live; vLLM 0.30, SGLang 0.5.18/0.5.19/0.5.21 sources were not on hand, so a live stream on each (reasoning parser, tool parser, `include_usage`, cache report) is still needed. Table: docs/guide/requests.md "Streamed fields".

## A launch whose parked charge keeps growing stops instead of parking — 2026-10-07 (branch `park-growth-guard`)

Owner decision 2026-10-07 (catalog finding 2, ADR 0014 amendment A19). On the catalog run
(GB10, vLLM 0.30.0, Qwen3.8-27B NVFP4 sized for 32 GiB) one launch's measured parked charge
grew 4.7, 9.6, 13.4 GiB over three parks while its GPU memory returned to 22.1 GiB on every
wake. Each measured park is now also recorded per launch (store schema v44,
`parked_launch_residues`: first and latest charge per domain). When the latest exceeds the
first by more than the host's `resource_policy.parked_growth_limit` (default `auto`, 100 % of
the first charge; or `N%`, a size, `off`; YAML, `--set` and `CAPYCTL_SET__…`, standalone under
`host.`), the next park of that launch is an ordinary stop: idle (`ready_idle_parked_growth`),
switch victim, and `park deployment` (answered with the stop's operation). The stop releases
only on its own cleanup evidence. Status shows `parked.growth` (`within_limit`, `past_limit`,
`stopped`) and a `Parked` line. A host that states nothing, or `auto`, publishes and stores the
same policy and freezes the same revisions.

CPU and Fake tests only (failing before the change: the outgrown park, idle and switch cases
parked; passing after; within the bound parks as before; unchanged digests; three ways). They are
not qualification. Live check outstanding: the catalog's model 1 deployment, three park and wake
cycles on a GB10 standalone with the default bound; the third park should be a stop
(`ready_idle_parked_growth` or `park_growth_stop`) and the next request a fresh start.

## Group members reserve their share of the weights — 2026-10-08 (branch `feat/group-member-weight-share`)

Owner decision 2026-10-07 (ADR 0028 amendment of that date). A group member whose phases derive from `engine_config.memory` was sized from the whole checkpoint, so a two-host TP2 group reserved its weights twice and a 126 GiB checkpoint could never be admitted on two 121.7 GiB GB10 hosts. Each member now holds `ceil(stage / T) + replicated`, with `replicated = W - sharded` and `stage = sharded` (P = 1) or `min(sharded, ceil(layers / P) x largest_layer)`, from the checkpoint's safetensors headers (`capyctl_config::checkpoint_layout`), or 10 % of `W` kept whole without them; the margin, startup placeholder, graph allowance, SGLang state reserve and `host_backed` copy derive from the share. Declared `resources` and world size 1 are byte-identical (recipe fingerprints pinned from `main`). The share is recorded as `engine_config.memory.member` (topology, whole weights, layout) so snapshots and provisional re-resolution keep it; the host reports the layout in `CheckpointDigestEvidence.layout`, and a member's launch plan names the whole weights (verified by the host) and `checkpoint_layout` (under `engine_groups`), so the host resolves the same share and SGLang's static pool holds it.

Engine evidence (vLLM 0.30.0, SGLang 0.5.21, TensorFold 0.6.5 sources) is in the ADR amendment: vLLM and SGLang shard embeddings and head by vocabulary and keep norms, row-parallel biases, routers and latent low-rank projections whole; TensorFold keeps embeddings whole on both ranks (and GLM-5.3-Flash's head), and has no PP; every engine takes the smallest per-rank KV token capacity. Not modeled: key-value projections replicated when KV heads < TP; the fitted context and hybrid state slot still count the whole model per token (conservative). The placeholder startup (2.25 x share) still exceeds a GB10's 97.4 GiB managed limit for the 126 GiB case; such a deployment declares `memory.startup`.

CPU tests only (config resolution TP2, PP2, TP2xPP2, fallback, provisional re-resolution, Flash-Next-sized fit, declared and single-host fingerprints; store measurement re-resolving both members; protocol plan and evidence validation). Failing first on `main`: the TP2 member charged 107374182400 bytes of weights against an expected 59055800320. Not run live; MN1–MN9 should confirm the per-member grants and each rank's measured peak against them.

## SGLang's static pool on unified memory — 2026-10-07 (branch `fix/sglang-static-fraction-weights`)

Owner decision 5 (b), note on ADR 0014 amendment A14. In the GB10 catalog runs (SGLang
0.5.21) a `memory.kv_cache` of 2 GiB or less failed at start ("Loaded weights leave no GPU
memory for the KV cache") on gpt-oss-20b, Gemma 4 26B-A4B and Qwen3.6-35B-A3B with
`--max-mamba-cache-size 8`. Cause: CapyCTL added its 2 GiB for SGLang's own allocations to
the static pool only when it sized the pools; those launches rendered exactly the weights
on disk and the KV cache, while SGLang 0.5.21 charges the weights as loaded, its load-time
memory and the argument-fixed state against the fraction first. Now every unified launch
with known weights renders weights + KV + state + 3 GiB (`STATIC_OVERHEAD_BYTES`, the
smallest whole GiB above the measured 1.7, 2.2 and about 2.9 GiB of the NVFP4 models),
capped at the request, and an explicit `--max-mamba-cache-size N` counts its `N + 1` slots
(and speculative intermediates). gpt-oss-20b's MXFP4 load (about 5.4 GiB) stays outside it
and keeps a stated request. CPU tests only (capyctl-config `sglang_pool`); CPU and
Fake-engine tests are not qualification. Live check pending (lab hosts reserved): model 2
(Qwen3.6-35B-A3B NVFP4) on SGLang with `memory.kv_cache: 1GiB` and with
`--max-mamba-cache-size 8` at 1536 MiB, Gemma 4 26B-A4B at `kv_cache: 1GiB`, all starting.

## The unified-memory margin grows with the weights — 2026-10-07 (branch `fix/unified-memory-margin`)

Owner decision 4 of 2026-10-07 asked whether the flat 8 GiB margin on unified memory (a
declared `memory.request` less the weights and 8 GiB is the KV cache; SGLang's static pool is
the request less 8 GiB) is intended, since a discrete GPU's has been the weights × 0.10 since
`c5ebc1a` (ADR 0019 A1). Investigation (ADR 0014 amendment A18, measured table there): on
unified memory the margin is the only charge for the engine's CPU-side memory, which a
discrete host charges separately (4 GiB on the system domain); the GB10 recipes measured
3.4 to 5.4 GiB CPU-side for vLLM `restart_only` and 4.2 to 6.0 GiB for SGLang, and up to
6.7 GiB beyond weights and KV for vLLM, so 8 GiB is justified up to 22 GiB of weights and was
not lowered. gpt-oss-120b on vLLM (60.77 GiB of weights) held 12.5 GiB beyond weights and KV
against the 9.25 GiB held (Ready charge 78.0 GiB, in use up to 81.3). New rule: on unified
memory the margin of a vLLM or SGLang request, declared or derived, is
`max(8 GiB, weights × 0.15 + 4 GiB)`; it changes nothing below 26.7 GiB of weights, so no
catalog recipe changes; frozen revisions (recording 8 GiB) re-resolve as they were
(`CheckpointFacts::legacy_family_margin`, which replaces `legacy_device_margin`). The SGLang
gpt-oss-20b recipe's 32 GiB came from SGLang loading the MXFP4 weights as 17.1 GB against
13.8 GB on disk, which is SGLang sizing's (passed to that work), not the margin's. Agreed with
the SGLang static-fraction fix: the margin's value is this change, what SGLang counts inside
its static pool is that one. CPU tests (T14, T26): the boundary at 28,633,115,400 bytes,
gpt-oss-20b unchanged, gpt-oss-120b derived and declared, a frozen revision's exact decode,
and the #68 discrete case unchanged. Not run live; the check to schedule is gpt-oss-120b on
vLLM on a GB10 (kv 8 GiB, startup 90 GiB, 96 GiB managed limit): the Ready charge (83.1 GiB)
should hold the ready footprint.

## vLLM loader under deep parking is a deployment setting — 2026-10-07 (branch `feat/vllm-load-strategy-setting`)

Owner decision 1 of 2026-10-07 (option B), recorded as the note on ADR 0014 §3 and §4.
`engine_config.vllm.safetensors_load_strategy` (`eager` or `lazy`) chooses vLLM's loader;
omitted, a parking deployment still renders `eager` and a non-parking one renders nothing,
and the command fingerprint and resolved configuration are unchanged (pinned). The raw
`--safetensors-load-strategy` in `extra_args` is refused with or without sleep mode. Origin
of the default: commit 785b887 (vLLM 0.29, Qwen3-4B, wake 57 s to 7.5 s); catalog evidence
against it on vLLM 0.30 NVFP4: about 15–17 GiB more held once loaded, wake about 55 s.

CPU tests only (rendering under and outside sleep mode, refusal of the raw option and of
other values and families, snapshot round trip, pinned identity). Not qualification; a live
deep-park run with `lazy` is still needed.

## gpt-oss refuses parking on vLLM and SGLang — 2026-10-07 (branch `gptoss-deep-refusal-wake-bound`)

Owner decision 3 (2026-10-07), from the recipe catalog on host B (CapyCTL 7e50aa9, GB10,
standalone, `openai/gpt-oss-20b`). vLLM 0.30.0 deep wake: the reload logs `OAIAttention:
Failed to load weights` for 24 layers, then `Harmony parser ended in a non-terminal state`;
the request sent to the parked deployment was answered after 778 s with
`{"code":"engine_error","message":"backend completion unverified"}`. SGLang 0.5.21:
`update_weights_from_disk` raises `TypeError: default_weight_loader() got an unexpected
keyword argument 'weight_name'` (answered `activation_uncertain` after 6 s).

`EffectiveDeployment::deep_wake_refusal` (SPEC §§6.2, 9.1, ADR 0010) now also refuses
`deep` and `host_backed` for a checkpoint whose `config.json` names a family in
`DEEP_WAKE_BROKEN_FAMILIES` for the launch's engine (`model_type`, else `architectures`, as
parser defaults match): gpt-oss on vLLM and on SGLang, `capability_missing:deep_park`. The
agent (`launch_capability`, `park_capability`) and standalone's `CapabilityGate` both
apply it before any effect; `restart_only` and SGLang `resident` parks are unaffected. The
effective deployment carries no engine version, so each entry records the versions it was
observed on and is lifted by deleting it after a live park-and-wake on a fixed engine.
A machine that does not see the checkpoint (a server whose host holds it) cannot read the
family; the launching host refuses.

The 778 s was not a request held by a failed wake. The status after it
(`status-woken-1.json`) shows the restore operation succeeded and the instance ready: the
8-token readiness probe after the wake got non-empty output, so the wake counted as
usable, and the request (no `max_tokens`) was forwarded to the mis-reloaded engine
[INFERENCE: it decoded to the 32,768-token context at about 46 tokens/s, ~700 s, and the
relayed stream then failed validation, `Refused::Uncertain`]. A restore that fails or is
retained uncertain already answers joined requests as soon as it is recorded
(`wait_terminal`; the SGLang case above). Live check still owed: on host B, the engine log
timestamps of the restore and of the Harmony line for that run.

Tests (T22 T21), failing before and passing after: `gpt_oss_refuses_both_parking_tiers_on_vllm_and_sglang`
(config), `a_gpt_oss_checkpoint_refuses_deep_on_vllm_and_sglang_but_not_restart_only`
(agent), `a_gpt_oss_launch_is_refused_its_launch_park_and_restore` (controller); plus
`the_gpt_oss_rule_leaves_restart_only_other_families_and_resident_parks`. CPU tests only;
they are not qualification, and no live run was made after the change.

## Status startup figure and a role-shutdown test race — 2026-10-07 (branch `fix/status-startup-provenance-and-shutdown-race`)

Owner decision 5 (a): the deployment's `startup` in status now uses the figure admission freezes into the next start's plan (`frozen_plan_startup`): a peak measured for the revision on its host and installation shows as `measured`, else the weighed placeholder or the revision's own budget as before. Found in the catalog runs: an SGLang deployment kept showing its 55.2 GiB default after a 29.7 GiB peak was measured. Owner decision 5 (d): `role_shutdown::remote_signals_restart_and_drain_host_stops_with_cleanup` deployed once the host was `online`, which the session sets before the host's inventory and resource policy are stored, so under load the deploy got `reconciliation_required`; it now waits for `session.reconciled` and `eligible` (SPEC §4.2). CPU tests only (the T29 measured-peak test failed first on the stale figure; the role test passed 10 of 10 beside a crate build); not qualification. Live check: on the catalog host, `capyctl status deployment <sglang model> --format json` after its first Ready shows `startup.provenance` `measured`.

## Multi-node engine groups — 2026-10-07 (branch `feat/multi-node-groups`)

ADR 0028 is implemented on the feature branch slices through status, closed codes and exits, and the user docs (several-machines guide, settings, network-access risk, engines support table, 0.1.3 release notes). CPU and Fake-engine tests only; this is not qualification. The live rows MN1–MN9 on host A and host B are pending.

## Stored snapshots keep the deployment engine env — 2026-10-06 (branch `fix/snapshot-engine-env`)

`decode_effective_snapshot` rebuilt the deployment without `engine_config.env`, so every revision with a deployment engine env (ADR 0028 §2.1, single host included) failed `CorruptStoredData` at start and load; the deployment entries of `engine_env` are now restated and the exact-equality check proves them. CPU round-trip tests only (deployment env, override of a profile value, profile env and `approved_env`, topology, placement and host `resource_policy.groups`); not run live.

## An unusable checkpoint names its reason — 2026-10-06 (branch `fix/checkpoint-unusable-reason`)

Reported live on the 16 GB discrete-GPU laptop host (0.1.1 standalone, vLLM 0.29, an 8B FP8
checkpoint of 10,605,572,552 bytes, a device domain managing 15,797,762,136 bytes,
`memory.request` 12 GiB and 13 GiB): once measured, every start answered
`checkpoint_mismatch`, including after configuration changes and on a fresh deployment.
The stored row was `unusable` with the generic "does not resolve ... replace the
configuration" text, and `host_effective_revisions` still read `resolved`. Three defects
(ADR 0014 §7): the re-resolution kept its error only for the closed refusals
(`insufficient_device_memory:`, `host_backed_unavailable:`); `admit_start` turned every other
unusable row into `LifecycleError::CheckpointMismatch`; and the failure path wrote no host or
GPU row. Now the resolution's own text (code, field, detail; bounded at 512 characters) is
stored; a start on such a revision is refused `checkpoint_unusable` with it (409, CLI class
`invalid_config`, exit 2), closed refusals keep their codes and exits, and
`checkpoint_mismatch` is only a digest disagreement. In the same transaction each host whose
row does not resolve with the measured weights is refused `does_not_resolve` and its GPU rows
dropped; nothing is reserved, so nothing is released. `status deployment` prints
`Checkpoint  unusable: ... (<reason>)`. The 0.1.1 cause was most likely the 8 GiB family
margin on a declared device request (KV = request − weights − 8 GiB ≤ 0; inferred from the
arithmetic, the real text was not stored); since `c5ebc1a` (0.1.2) the margin is weights × 0.10, and the store
test shows the reported 12 GiB request resolving with those weights while 10 GiB is refused
with "the derived KV cache (request minus weights minus margin) is not positive". A row made
unusable before this fix keeps its generic text (now refused `checkpoint_unusable`, not
`checkpoint_mismatch`) and its host row; a new revision re-resolves. The per-revision row and
the fast re-measure on a fresh deployment (the agent's stat-identity digest cache, ADR 0028
§7) are by design and unchanged.

Tests (T14 T26): `weights_that_do_not_fit_a_declared_request_make_the_revision_unusable`
(previously pinned `CheckpointMismatch`), `a_declared_request_the_measured_weights_do_not_resolve_names_its_reason`,
`an_unusable_checkpoint_is_refused_with_its_reason` (management),
`an_unusable_checkpoint_exits_as_a_configuration_refusal` and
`status_explains_an_unusable_checkpoint` (CLI); the closed-refusal test
`a_derived_request_larger_than_the_card_refuses_the_start_with_its_code` is unchanged and
passes. CPU tests only; they are not qualification, and no live run was made after the fix.

## Standalone parked limit is settable — 2026-10-06 (branch `feat/parked-limit-setting`)

Owner decision 2026-10-06, the follow-up on ADR 0014 amendment A17 and the amendment of
ADR 0025. A standalone's parked limit was fixed at a quarter of the observed memory, below
the 33.4 GiB a parked Qwen3.8-27B with DFlash2 measured on a 128 GB GB10, so its later parks
were refused `park_parked_capacity`. `host.resource_policy.memory.system.parked_limit` now
takes `auto` (the quarter), a size or a whole percentage, by YAML, `--set` and
`CAPYCTL_SET__HOST__RESOURCE_POLICY__MEMORY__SYSTEM__PARKED_LIMIT` (flag over environment over
YAML), and shows in `capyctl config show`. A stated value above the managed limit refuses the
start with both numbers. A document without it publishes the same policy, byte for byte. The
embedded host's published document now carries the stated memory limits, so a discrete
template's `host_backed` default is sized on the same parked room admission uses.

CPU and Fake-engine tests only (precedence, form and boundary checks, unchanged digest, a
park refused `parked_capacity` at a lower limit and admitted at a raised one, and a booted
standalone storing each channel's value). They are not qualification; no live park has run
with a raised limit.

## Standalone refuses a modelopt SGLang park it cannot wake — 2026-10-06 (branch `fix/standalone-deep-wake-refusal`)

Found live on host A (CapyCTL a6b2560, SGLang 0.5.21, GB10, standalone): a `deep` SGLang
deployment with `quantization: modelopt` started and parked; the wake's
`update_weights_from_disk` raised `AttributeError: 'Parameter' object has no attribute
'weight_loader'` and the instance was retained `uncertain`. A host agent refuses that launch
`capability_missing:deep_park` (SPEC §§6.2, 9.1, ADR 0010), but the rule lived in the agent's
`launch_capability`/`park_capability` only, and standalone's embedded host never called it.

The rule is now one decision, `EffectiveDeployment::deep_wake_refusal` (capyctl-config). The
agent calls it as before; standalone's embedded launches and adopted launches pass a
`CapabilityGate` that refuses Initialize, Park and Restore with the same reason before the
engine is asked. The refused start gives up at once (one attempt) and spawns nothing.
Tests: `a_standalone_sglang_modelopt_deep_launch_is_refused_before_any_effect` (spawn_resolved
with the production bindings; before the fix it spawned the engine) and
`a_modelopt_sglang_launch_is_refused_its_launch_park_and_restore`, both T22 T21.

The other refusals in `native_execution/refusal.rs` were checked against standalone. Drift
(`InstallationGate`) and checkpoint mismatch (`CheckpointGate`) already have embedded gates;
runtime integrity is checked when the role resolves its installation, and the SGLang renderer
revalidates the protected entry on every launch;
memory (the coordinator's admission), the engine port (the lease binds it before handing it
out), the residency tier (the store's `parks`) and a wake beside other claims (the residency
admission) are decided where both paths pass. The probe refusals (`capability_missing:core`,
`:deep_park`, `:observation`) stay agent-only by ADR 0008: standalone runs the deep-park probe
at `engine add`, where a missing capability records `deep_park` disabled unless the operator
enables it, and the protected entries probe again at startup. Not run live after the fix; CPU
tests are not qualification.

## Hybrid SGLang runs 8 requests by default — 2026-10-05 (branch `fix/sglang-hybrid-default-8`)

Owner decision 2026-10-05, the note on ADR 0014 amendment A16. A gated-delta-net hybrid
SGLang deployment without `max_concurrent_requests` runs up to 8 requests
(`SGLANG_HYBRID_DEFAULT_RUNNING`, TensorFold's default) instead of up to 32: the state a
derived request reserves, `--max-mamba-cache-size` and `--max-running-requests` all use 8.
The router still admits 32 per deployment; dense SGLang is unchanged. Qwen3.8-27B with
DFlash2 reserves about 16 GiB of state instead of 61 GiB.

Live on the laptop (16 GB GPU, SGLang 0.5.21, FrogNano-4B BF16, standalone, isolated state,
`memory.kv_cache: 1GiB`, where the card holds 10 requests' state): status showed "Running
limited to 8 requests by the state cache", SGLang got `max_running_requests` 8 and
`max_mamba_cache_size` 40, started, and answered 12 concurrent requests. Not run on a GB10.

## SGLang's fraction on a discrete GPU — 2026-10-04 (branch `fix/sglang-discrete-overhead`)

Closes ADR 0014 A14's open item (note on amendment A14). SGLang 0.5.21 takes
`mem_fraction_static` of the GPU memory free at its baseline (after its CUDA context,
without the driver's reserve), not of the card's total CapyCTL renders against, and keeps
100 MiB for a multimodal checkpoint, so its pools fell about 0.65 GiB short on a 16 GB card.
On a discrete GPU whose pools CapyCTL fixes, the closed memory object now carries
`static_allowance_bytes` (1 GiB) and the entry renders the fraction from the static pool
plus it (at most 0.9999). The fixed pools bound what SGLang allocates.

Live on the laptop (16 GB GPU, SGLang 0.5.21, FrogNano-4B BF16, nothing stated, standalone,
isolated state): before, SGLang held 42778 of 63935 KV tokens and refused inputs over 42772
while status showed 63920; after, fraction 0.8352, `max_total_num_tokens=63935`, prompts of
63820 and 63910 tokens answered, 14478 MiB on the card inside the 14.5 GiB charge. CPU and
Fake-engine tests are not qualification.
## Derived SGLang requests hold the hybrid state — 2026-10-04 (branch `feat/sglang-state-facts`)

ADR 0014 amendment A16 closes A14's follow-up. The host that measures a checkpoint also
reads one request slot of an SGLang hybrid model's recurrent state from `config.json`
(`CheckpointDigestEvidence.state_slot_bytes`); the server records it with the digest
(schema v40), re-resolves a provisional revision with it, and every launch carries it
(`checkpoint_state_slot_bytes`, capability `checkpoint_state_slot`), which the host checks.
A derived request then holds the state of `max_concurrent_requests` (or 32), as far as the
domain holds it, and lends nothing more. Explicit requests, declared resources, args that
size the state pool, and revisions without the fact keep A14's sizing.

Live on the laptop (16 GB GPU, SGLang 0.5.21, FrogNano-4B BF16, standalone, isolated state):
the fact was recorded (51511296 bytes); with a 2 GiB KV cache the derived request held 7
requests' state and the KV pool kept the whole 2 GiB (context 65536); with nothing stated
the card had no room for a slot, so A14's sizing stayed (7 requests, 63920 tokens). Not run
on a GB10. CPU and Fake-engine tests are not qualification.

## Short form of `resources`, after 0.1.2 — 2026-10-04 (branch `feat/short-resources`)

Owner decision, recorded in ADR 0023 §4: `resources: {gpu: 11GiB, ram: 2GiB}`
stands for the five phases of a model that restarts. Where the document meets
its host (`deployment_defaults::for_host`) it becomes the long form: cold,
ready, parking and wake charge `gpu` on the device's domain and `ram` on the
system domain (their sum on a unified pool), parked charges zero with no
device, and an undeclared residency is `restart_only`. Refused: a parking
residency, several devices, a missing figure, a zero `gpu`, a mix with phases.
`validate config` shows the phases (short terms offline, by domain with
`--host`). Any engine takes it; the TensorFold example uses it.

Live, standalone on the branch build, one RTX 4090 Laptop GPU, TensorFold
0.6.3, FrogNano-4B-2609 MLX 4-bit: the recipe's long-form file and the same
file with `resources: {gpu: 11GiB, ram: 2GiB}` deployed side by side. Both
started (`initialize succeeded`), both launched TensorFold with
`TENSORFOLD_CUDA_MEMORY_LIMIT_GB=11`, and status showed the same startup
charge (11.0 GiB of gpu0 plus 2.0 GiB of RAM) and the same effective
fingerprint. The short-form deployment answered a chat request (`391`,
`finish_reason: stop`); with it running, starting the long-form one was
refused `capacity_blocked` naming 11.0 GiB of gpu0, as for the long form.

## 0.1.2 release preparation — 2026-10-04

Version bumped to 0.1.2 with release notes in
`docs/operations/release-notes-0.1.2.md` (vLLM 0.30.0, SGLang 0.5.21,
TensorFold 0.6.2, 0.6.3 and 0.6.5, the memory and reliability fixes since
0.1.1, parser defaults, TensorFold `--parallel`, SGLang CUDA graphs on). Four
entries that PRs after 0.1.1 had appended to the 0.1.1 notes moved to the
0.1.2 notes; the 0.1.1 notes are back to their published text. The
verification result is in the release pull request. Nothing is built, tagged
or published yet: next are the manual Release build workflow, the strict local
packaging check, a draft release and the live check from the draft
(docs/operations/releasing.md). The owner publishes.

## TensorFold 0.6.5 verified — 2026-10-04 (branch `feat/verify-tensorfold-0.6.5`)

TensorFold 0.6.5 joins 0.6.0 to 0.6.3 in the verified set (ADR 0023 §1), so
`engine add` shows it `custom no`; 0.6.4, not run live, and later versions stay
custom. Between the v0.6.3 and v0.6.5 tags, the build toolchain and the drafter
checks are unchanged for CapyCTL (`cuda/build.py` only adds a refusal of Flash
Next below sm_120). `cli_args.py` adds `--api-key` (repeatable),
`--api-key-file` and `--metrics-open`: with a key, TensorFold refuses every
route without one, `/metrics` included, and `/health` shrinks to
`{"status":"ok"}`. All three are now reserved, as vLLM's `--api-key` is;
CapyCTL passes no key and its closed environment never carries
`TENSORFOLD_API_KEY`, so `/metrics` stays open on the loopback listener.
`--mtp-confidence` (Nemotron's fixed floor instead of 0.6.5's measured-cost
depth) stays an ordinary option, and the `plan` command is not `serve`.
`server/metrics.py` adds `tensorfold:requests_total` (with keys only) and
`tensorfold:process_footprint_bytes` (macOS only); neither is read. Nemotron-H
on CUDA still runs one request at a time, and the status reason no longer
names a version.

Live on host A (one GB10, standalone from origin/main `9b90a56`, fresh 0700
state and config directories, a new `~/tensorfold-0.6.5-venv` with the same
pins as 0.6.3: torch 2.13.0 cu130, triton 3.7.1, system CUDA 13.0). Qwen3.8-27B
NVFP4 with the DFlash2 drafter, `--parallel 8`, greedy:

| Step | Result |
|---|---|
| `engine add` | `tf065 0.6.5` registered beside `tf063 0.6.3` |
| `start --wait`, cold (empty kernel cache) | 104.8 s (0.6.3: 123.0 s) |
| `start --wait`, warm | 14.5 s |
| stream with reasoning | 14 reasoning chunks, content `391`, `finish_reason: stop`, usage chunk, `[DONE]` |
| request after `stop deployment` | 409 at once |
| wake on request | with 0.6.3 started by `--evict`, a request for 0.6.5 switched and answered in 15.3 s |
| `--no-drafts` | ready in 12.9 s, answered |
| engine `/metrics`, no key | 200, the three gauges CapyCTL reads present |
| shutdown | `drained: true`, `forced: false`, 0 in flight |

Benchmark against 0.6.3 on the same deployment (medians; concurrency 512
tokens, 10 rounds pooled from two interleaved passes; context 128 tokens, 3
runs): aggregate tok/s C1 42.2 / 41.5 (one slower round), C4 121.3 / 121.4,
C8 193.9 / 193.8; prefill at 2k, 32k and 128k within 0.4%; TTFT at 128k 87.2
/ 86.9 s. `token_sha` matched on 24 of 30 keys: every concurrency pair and the
2k runs; the 32k and 128k replies differ, as 0.6.4's fp32 tree-attention fold
(#268) allows, so decode speed there follows different replies. Nemotron 3.5
Lightning, one stream: 147.5 / 152.5 tok/s (+3.4%), the same tokens. The 0.6.5
engine logs name `--no-thinking` at start and warn when a reply hits
`max_tokens` while thinking; the role log has no error or warning lines.

## TensorFold decodes the deployment's requests together — 2026-10-03 (branch `fix/tensorfold-parallel`)

Owner decision 2026-10-03, recorded in ADR 0023 §4. TensorFold serves one request
at a time on CUDA unless started with `--parallel`, and CapyCTL passed none, so a
TensorFold deployment served its requests in turn while vLLM and SGLang ran them
together. `max_concurrent_requests` now renders `--parallel` (8 when undeclared:
TensorFold reserves the drafter's buffers for every stream at start, inside the
declared memory); `--parallel` in the extra or host-fixed arguments wins and is
refused beside a declared count; status shows `Streams ...`; TensorFold's
refusal of a context its cap cannot hold names the fix.

Live, through `capyctl start standalone` on the branch build:

- One GB10, Qwen3.8-27B NVFP4 with DFlash2, 32768-token context, 34 GiB Ready.
  TensorFold 0.6.3 started with `--parallel 8` and logged a 30.68 GiB startup
  estimate within the 34.00 GiB cap and "up to 8 streams". Aggregate decode, 512
  tokens a request, medians of three rounds: 15.8 tokens/s at 1 stream, 17.6 at 2,
  28.9 at 4, 62.2 at 8 (3.9 times one stream). With `--parallel auto` in
  `extra_args` (TensorFold's one at a time) the same machine gave 12.3 at 1 and
  12.8 at 8. TensorFold 0.6.1 gave the same shape (15.6 at 1, 61.3 at 8). This
  machine decodes this model about a third as fast as the recipe's measurement
  for one stream; the ratios are what was checked.
- The same deployment with `max_concurrent_requests: 32` was refused by
  TensorFold at start (49.3 GiB estimated on 0.6.1) and status read "TensorFold's
  memory cap cannot hold context_length beside its streams: lower
  max_concurrent_requests or context_length, or raise the ready allocation in
  resources". `max_concurrent_requests: 4` beside `extra_args: [--parallel, "8"]`
  was refused at deploy, naming both.
- One RTX 4090 Laptop GPU (16 GB), FrogNano-4B-2609 MLX 4-bit on TensorFold
  0.6.3, no drafter, 11 GiB cap: startup estimate 7.29 GiB for 8 streams.
  Aggregate decode 50.4 tokens/s at 1 stream, 93.4 at 2, 176.5 at 4, 319.1 at 8
  (40.3 each); one stream 49.5 to 50.4 tokens/s from 0.5k to 32k tokens of
  context. Eight 28k-token prompts at once all completed, GPU memory at most
  9.9 GiB.

## Standalone memory limits and the TensorFold memory cap — 2026-10-03 (branch `fix/standalone-memory-limit`)

Found in the three-engine comparison (Qwen3.8-27B NVFP4 with DFlash2 on one
GB10, 121.7 GiB) and recorded in ADR 0025.

1. **The standalone managed limit could not be raised.** It was 50 % of memory
   (60.8 GiB), so an 84 GiB TensorFold declaration and a 72 GiB vLLM request were
   refused `capacity_blocked`. `host.resource_policy.memory.system.managed_limit`
   and `free_reserve` now take `auto`, a size or a whole percentage, by YAML,
   `--set` and `CAPYCTL_SET__…`; together they must fit the observed memory. The
   stored policy follows them at every start (before, standalone kept the limits
   it first stored), and `auto` returns to the default.
2. **TensorFold grew past its declaration.** CapyCTL launched it with no cap, so
   TensorFold 0.6.3 sized its CUDA budget from the machine's available memory and
   kept long prompts' states until that ran out (77.9 GiB of machine memory under
   a 58 GiB Ready declaration). The launch now sets
   `TENSORFOLD_CUDA_MEMORY_LIMIT_GB` to the declared Ready allocation.

Live on host A (TensorFold 0.6.3, SGLang 0.5.21), one standalone restarted
with the limit set each way:

- `--set ...managed_limit=90GiB`: a 90.0 GiB limit (policy revision 1). A
  TensorFold deployment declaring 84 GiB cold and 82 GiB Ready, refused under the
  old limit, was admitted and launched with the variable at `82`. A
  254,993-token prompt got its first token at 260 s and completed. Repeated long
  prompts took machine memory from 47 to 67 to 84 GiB, where the growth stopped
  (84.4 GiB, 80.8 GiB above idle, under the 82 GiB cap); a new 250k-token prompt
  was still served.
- `CAPYCTL_SET__…MANAGED_LIMIT=88GiB` on restart: revision 2, 88.0 GiB. The
  comparison's 58 GiB Ready declaration launched with the variable at `58`; five
  prompts (255k, 128k, 255k, 250k, 255k tokens) all completed and machine memory
  peaked at 60.6 GiB (about 56.5 GiB above idle; 77.9 GiB in the comparison).
- `managed_limit: "80%"` in the document on restart: revision 3, 97.4 GiB. An
  SGLang deployment with a 70 GiB request and `--max-mamba-cache-size 40` was
  admitted and started in 153 s; a 254,993-token prompt got its first token at
  367 s and completed (machine memory peak 73.7 GiB).

## A 9 GB model on a 16 GB GPU — 2026-10-03 (branch `fix/single-gpu-16gb`)

Found running FrogNano-4B-2609 (BF16, 9.32 GB) on a 16 GB laptop GPU.

- The device reserve absorbs memory held outside CapyCTL (driver reservation,
  display server) in admission, the switch planner and the launch check; a
  deployment charged up to the card's managed limit starts on an idle card. A
  request declared for a card derives its KV cache as request minus weights ×
  1.10 and its startup peak as the request (ADR 0019 amendment A1).
- STARTUP shows the card's figure with host RAM beside it; a waiting start and
  an unmeasurable checkpoint say why under the status table.
- A local model outside the models directory (a Hugging Face cache snapshot)
  is measured inside its own root and launches; `start --wait` ends at once on
  a digest diagnostic that does not clear (ADR 0014 amendment A11).
- A leased engine port another program listens on refuses the launch
  `port_conflict` before anything starts.
- A failed deployment with nothing retained can be updated, which also lets a
  restart that re-sizes deployments (new port range) re-size it.
- `start --wait` retries a `still_stopping` refusal through the stop window.
- State paths: the lock and state refusals name the path and the rule
  (`invalid_config`); `engine add` refuses a state directory whose socket path
  is too long before writing `engines.yaml`.
- SGLang: its own reason for refusing the server arguments reaches the private
  log under `--debug-engine-logs`; the launch failure hint says SGLang writes
  that log only then.
- Non-streaming responses carry `usage` (the collection asks for it) and no
  null `prompt_text` / `prompt_token_ids`.
- SGLang CUDA graphs stay off by default while parking; measured and
  documented (ADR 0014 note on §4).

Live on the laptop with temporary home, config and state directories (vLLM
0.29.0 and 0.30.0, SGLang 0.5.20 and 0.5.21): plain deployment files started
and served; the redeploy-then-`start --wait`, failed-deployment update,
re-size on restart, HF-snapshot digest, `usage`, SGLang park and wake of a
thinking model, and the debug-log reason were each seen live. The
`port_conflict` refusal itself is covered by a unit test (live, the lease
skipped the busy port). CPU and Fake-engine tests are not qualification.

## Parsers chosen by model family — 2026-10-03 (branch `feat/parser-defaults`)

ADR 0024 (owner decision 2026-10-03). For vLLM and SGLang, CapyCTL reads the
checkpoint's `config.json` and chat template at launch render and picks the
tool-call and reasoning parsers for the Qwen3 family (`hermes` / `qwen25`,
`qwen3`) and the Qwen3.5 family (Qwen3.5, Qwen3.6, Qwen3.8: `qwen3_coder`,
`qwen3`), plus `--enable-auto-tool-choice` on vLLM. An unknown family gets
none. `engine_config.vllm|sglang.tool_call_parser` and `reasoning_parser` take
`auto`, `none` or a name. The same option in `extra_args` or host-fixed args
wins over `auto`. A named parser or `none` beside it is refused. Status and
`validate config` show the choice. Parser names were checked as registered in
vLLM 0.29.0 and 0.30.0 and SGLang 0.5.20 and 0.5.21.

Live on a laptop (RTX 4090 Laptop 16 GB), standalone from this branch with
temporary home, config and state directories. FrogNano-4B-2609
(`qwen3_5`, revision `b90468c1`) ran on vLLM 0.30.0 and SGLang 0.5.21 with no
`extra_args`. The memory was declared by hand (12.5 GiB request, 3 GiB KV
cache, 13 GiB startup) until the 16 GB sizing fix lands. On both engines:
- status printed `Parsers tool calls: qwen3_coder, reasoning: qwen3 (model
  family qwen3_5)`;
- a `tools` request returned structured `tool_calls` with
  `finish_reason: tool_calls`;
- a plain question returned the trace apart from `content`: `reasoning` on
  vLLM, `reasoning_content` on SGLang.

The vLLM command line carried `--tool-call-parser qwen3_coder
--reasoning-parser qwen3 --enable-auto-tool-choice`. CPU and Fake-engine
tests are not qualification.

## Kernel builds left out of the startup peak — 2026-10-02 (branch `fix/startup-peak-excludes-kernel-build`)

ADR 0014 amendment A12 (owner decision 2026-10-02, option A). While an
Initialize runs, its host checks the engine's process group every 500 ms for
a compiler (`ninja`, `nvcc`, `cicc`, `ptxas`, `c++`, `cc1plus` and others). It
reports each span with one as a kernel build, with the readiness result
(`MemberExecutionResult` field 15). The coordinator measures the startup peak
from the availability samples outside those spans. A start that built kernels
throughout records no peak. Peaks recorded before this change stay; a
deployment blocked by one is deleted and redeployed once.

- SGLang and FlashInfer build kernels after weight loading has begun, so a
  start-at-weight-loading signal would still count the builds. The
  process-group signal works for every engine. TensorFold deployments
  declare their resources, so no peak is measured for them.
- Builds inside the engine process (Triton, NVRTC) are not seen, and are
  counted as before.

Tests (CPU and scripted engines, which are not qualification):

- coordinator: a peak sampled during a build is left out (12 GiB recorded
  where the build took 46 GiB); a start that built throughout records none;
  the existing measured-peak tests pass unchanged with no builds reported.
- host journal: the builds are carried in the readiness result and its replay.
- protocol: the field is accepted only on a usable launch, ordered and bounded.
- launchers: a real process group led by a compiler-named process is seen
  building and a `sleep` group is not.
- SGLang adapter: a compiler seen while starting is reported in the step.

Live on host A (GB10, 2026-10-03), Qwen3.8-27B NVFP4 with DFlash2 on SGLang
(`memory: {request: 48GiB}`, local checkpoint). Two standalones were built from
`main` (93ec627) and from this branch (493f8fd), each with its own state and
config directories. Switching the SGLang environment forced the JIT kernel
rebuild (the shared cache names the environment), so both first starts built.
No cache was deleted.

| | `main`, SGLang 0.5.20 | this branch, SGLang 0.5.21 |
|---|---|---|
| First start (with kernel builds) | 624 s | 585 s |
| Lowest MemAvailable during the build | 10.7 GiB | 1.8 GiB |
| Measured startup peak | 106.5 GiB | 44.8 GiB |
| Next start | refused `capacity_blocked` (needs 106.5 GiB, 60.8 GiB limit) | ready in 177 s, no build |

An independent 2 s sampler saw `ninja` and up to 28 compiler processes for
about 6 minutes of each first start.

vLLM 0.30 with Qwen3-4B on this branch started in 67 s. Its compile caches were
warm, so no compiler ran and it measured 20.6 GiB. The 102 GiB vLLM first start
seen in the three-engine run was not reproduced, because doing so needs its
caches cleared:

- FlashInfer's JIT build runs under `ninja`, so it is seen.
- torch.compile's compile workers are Python processes and are not seen
  (the fallback).
- FlashInfer autotuning uses the engine's own GPU memory.

Cleanup: deployments deleted, both standalones shut down drained, every
process and directory the check made removed. The environments, models and
caches are kept.


## Small CLI fixes — 2026-10-02 (branch `fix/small-cli-fixes`)

- `engine remove` with no role running removes the profile from
  `engines.yaml` under its lock and exits 0 (`published: role_not_running`);
  it was refused `agent_unreachable`. A running role keeps the retirement
  path (ADR 0018 amendment A3).
- A role skips an `engines.yaml` profile whose engine kind it does not know
  (written by a newer release), warns at start with the profile and kind, and
  starts; it was refused `invalid_config` at `runtime_profiles.engine`.
  Releases before this one still refuse such a file.
- `capacity_blocked` names, per host short of memory, the domain, the need,
  what is free and the limit.
- A start while new activations wait on an unproven stop is refused
  `still_stopping` (503, retryable, exit 25) instead of
  `reconciliation_required`.
- `a_success_resets_the_attempt_budget` and
  `a_restart_pauses_and_retries_every_adopted_uncertain_launch` no longer
  depend on timing; `shutdown` waits for queued Store jobs.

Live on a laptop with temporary home, config and state directories: a
standalone with only an unknown-kind profile starts with the warning and
publishes nothing, and after it stops `engine remove` edits the file. The
capacity message and `still_stopping` are covered by unit and integration
tests only. CPU and Fake-engine tests are not qualification.

## SGLang 0.5.21 verified — 2026-10-02 (branch `feat/verify-sglang-0.5.21`)

SGLang 0.5.21 joins 0.5.20 in the verified set (ADR 0018), so `engine add`
shows it `custom no`; 0.5.22 and later stay custom. The upgrade keeps torch
2.13.0, FlashInfer 0.6.18, sglang-kernel 0.4.7, transformers 5.12.1 and
torch-memory-saver 0.0.10, and changes nothing CapyCTL drives: the
ServerArgs record and its resolution, `launch_server`'s scheduler target, the
scheduler and tokenizer manager hooks, the release, resume, reload and flush
routes and their replies, and the metric names are the same. Of the 21 new
options, none has a listener, path, configuration or code shape, and the new
`--disaggregation-*` and `--hicache-storage-backend tensorcast` fall in
reserved families. The new routes (`/v1/decisions`, `/v1/systemone`,
`/pd_role_switch`, `/begin_weight_update`, `/end_weight_update`) are not
forwarded. `--kv-cache-dtype` no longer accepts `fp4_e2m1`; CapyCTL never
rendered it. A request-supplied `chat_template` is now refused unless the
server runs with `--trust-request-chat-template`.

One CapyCTL bug found and fixed, on both versions: the SGLang wake probe
(`max_tokens: 8`, exact `OK`) was spent on Qwen3-4B's thinking, so every deep
wake of a thinking model was left uncertain after resume, reload and flush had
succeeded. The probe now sends `chat_template_kwargs: {"enable_thinking":
false}`.

Live on host B (standalone from this branch, fresh 0700 state and config
directories, a new owner-approved `~/sglang-0.5.21-venv` beside the existing
0.5.20 one, both registered in the same standalone). Same prompts on both,
temperature 0, through the CapyCTL endpoint; context sweep 3 requests per
point, decode 3 streams of 512 tokens one at a time.

| | 0.5.20 | 0.5.21 |
|---|---|---|
| Qwen3-4B ready, cold (digest measured) | 99 s | 134 s (first start, kernel builds) |
| Qwen3-4B ready, warm | 59 to 99 s | 74 to 102 s |
| Qwen3-4B TTFT, 1k / 8k prompt | 0.149 / 1.169 s | 0.149 / 1.169 s |
| Qwen3-4B decode, one stream | 22.3 tok/s | 22.3 tok/s |
| Qwen3-4B measured startup peak | 19.4 GiB | 19.7 GiB |
| Qwen3-4B request while parked | 91 s | 47 to 90 s |
| Qwen3.8-27B NVFP4 + DFlash2 ready, new deployment (kernels already built) | 158 s | 169 s |
| Qwen3.8-27B ready, warm | 173 s | 161 s |
| Qwen3.8-27B TTFT, 1k / 8k prompt | 0.453 / 4.155 s | 0.456 / 4.165 s |
| Qwen3.8-27B decode, one stream | 29.1 tok/s (24.4 to 29.3) | 29.1 tok/s (24.5 to 29.2) |
| Qwen3.8-27B measured startup peak | 44.6 GiB | 44.9 GiB |

The 27B deployments set `memory: {request: 48GiB, kv_cache: 16GiB}`. A wake
is mostly the reload from disk (8 GB in 45 to 85 s on this host). On 0.5.21:
park 2 s; the request while parked woke it (resume, reload, flush, probe);
`--reasoning-parser qwen3` streamed 199 `reasoning_content` deltas and woke
the same way; a stream hung up after 2 s and a park right after it settled in
0.6 to 1.1 s; `start --evict` of the 27B parked Qwen3-4B (`released:
parked`); shutdown with one stream in flight drained it (`drained: true`,
`in_flight_at_close: 1`, the stream ended with `[DONE]`).

Open finding, fixed by ADR 0014 amendment A12 (branch
`fix/startup-peak-excludes-kernel-build`): a start that builds kernels records the build's
memory as its startup peak. The first 0.5.21 start of the 27B built FlashInfer
kernels for 9 minutes (`MAX_JOBS` 14 from 117 GiB available) and recorded a
111 GiB peak, and every later start of that deployment was refused
`capacity_blocked`. The two SGLang environments share FlashInfer's JIT cache,
so starting one after the other rebuilt the kernels again (one start took
479 s and recorded 108 GiB). Deleting and redeploying cleared it.

Cleanup: deployments deleted, both profiles removed, drained shutdown, state
removed, no CapyCTL or engine process left; the environments and models are
kept.

## TensorFold 0.6.3 verified — 2026-10-02 (branch `feat/verify-tensorfold-0.6.3`)

TensorFold 0.6.3 joins 0.6.0 to 0.6.2 in the verified set (ADR 0023 §1), so
`engine add` shows it `custom no`; 0.6.4 and later stay custom. Between the
0.6.2 and 0.6.3 tags, `cuda/build.py` and the drafter checks are unchanged.
`cli_args.py` adds `--vision-offload`, `--vision-image-tokens` and a
`control` command; neither option names a path, a listener or code, so both
stay ordinary pass-through options. `server/metrics.py` adds a decode
histogram (`tensorfold:request_decode_seconds`, mirrored as
`tensorfold:request_decode_time_seconds`); the gauges CapyCTL reads are
unchanged and the new histogram is not read. With
`stream_options.include_usage`, 0.6.3 sends usage as its own final chunk with
`choices: []` after the finish chunk that carries the `tensorfold` object;
the stream relay already accepted that chunk, and a unit test now pins the
collected shape.

Live on host A (one GB10, standalone from origin/main `74a9b9b`, fresh 0700
state and config directories, a new `~/tensorfold-0.6.3-venv` with the same
pins as 0.6.2: torch 2.13.0 cu130, system CUDA 13.0). Qwen3.8-27B NVFP4 with
the DFlash2 drafter, `--parallel 8`, greedy:

| Step | Result |
|---|---|
| `engine add` | `tf063 0.6.3` registered beside `tf062 0.6.2` |
| `start --wait`, cold (empty kernel cache) | 103.4 s (0.6.2: 115.1 s) |
| `start --wait`, warm | 13.9 s |
| stream with reasoning | 14 reasoning chunks, content `391`, `finish_reason: stop`, usage chunk, `[DONE]` |
| request after `stop deployment` | 409 at once |
| wake on request | the 0.6.2 deployment stopped, 0.6.3 started, answered in 15.4 s |
| `--no-drafts` | ready in 12.3 s, answered |
| shutdown | `drained: true`, `forced: false`, 0 in flight |

Benchmark against 0.6.2 on the same deployment (medians; concurrency 512
tokens, 10 rounds pooled from two interleaved passes; context 128 tokens, 3
runs): aggregate tok/s C1 42.7 / 42.7, C4 122.6 / 122.7, C8 195.3 / 194.6;
decode tok/s 2k 46.4 / 46.5, 32k 41.2 / 41.3, 128k 30.3 / 30.1; TTFT at 128k
87.1 / 87.0 s. `token_sha` matched on 30 of 30 keys. Usage was counted on
130 of 130 measured requests per version. On Qwen3.8 dense the `/health`
keys are the same as 0.6.2's. No CapyCTL bug found.

## Long prompts, stream bounds and standalone queue settings — 2026-10-02 (branch `fix/stream-idle-cancel`)

Found in the TensorFold 0.6.1 and 0.6.2 context sweep (Qwen3.8-27B NVFP4,
DFlash2, standalone): every 256k-token prompt ended at 120 s with no token.

1. **A prefill was cut by the idle bound.** TensorFold sends the reply's
   `role` chunk as soon as it accepts a request, before it prefills. The
   router counted that chunk as backend progress, so the stream idle bound
   (120 s) applied to the prefill instead of the request deadline (1800 s in
   standalone). A chunk that only opens the reply is now relayed but is not
   progress (`opens_reply_only`, SPEC §10 amendment 2026-10-02), streamed or
   collected.
2. **Standalone refused the queue settings.** `host.resource_policy.queue`
   (every bound a host has, `stream_idle_timeout` included) is now honoured
   in a standalone document, by `--set` and by `CAPYCTL_SET__…`. A restart
   with changed bounds applies them to the stored policy as a new revision;
   before, an existing standalone kept its first bounds silently. The
   persisted memory limits are kept as before.
3. **A cut request stayed uncertain.** A stream or collected response the
   router cuts for its bounds closed the engine connection already; its lease
   is now `cancelling`, as for a client hang-up, and settles on engine
   quiescence.

Root cause of the slow stop: TensorFold 0.6.1 and 0.6.2 (CUDA server) notice a
closed connection only between decode rounds, so an abandoned prefill runs to
its end. The stop waits for `/health` idle by design (ADR 0023 §6), so it lasts
as long as the remaining prefill. CapyCTL cannot shorten that without
signalling a busy engine, which ADR 0023 refuses (owner decision).

Live on host A (standalone, TensorFold 0.6.2, fresh 0700 state and config
directories, 262144-token context):
- Before (commit `74a9b9b`): a 247.5k-token prompt got its `role` chunk at
  0.3 s and was cut at 120.3 s; the engine kept prefilling for 130 s more, and
  the stop took 120.9 s. A start of another deployment during that stop was
  refused `capacity_blocked` at 15 s and `reconciliation_required` at 73 s.
- After, `--set host.resource_policy.queue.stream_idle_timeout=600s`: the
  same prompt (247569 tokens) got its first token at 246.6 s and `[DONE]`.
- After, default bounds: first token at 246.9 s and `[DONE]`.
- After, `CAPYCTL_SET__HOST__RESOURCE_POLICY__QUEUE__REQUEST_DEADLINE=30s` on
  a restart (policy revision 2): a 121k-token prompt was cut at 30.0 s with
  its lease `cancelling`; the stop sent then took 46.7 s, the rest of the
  prefill. A restart without the variable went back to 1800 s (revision 3).

Not fixed (noted for the owner): a start during a stop that waits on a busy
TensorFold is refused `reconciliation_required` ("Current owned state or
resource policy is unavailable") once the cleanup pauses the worker
(`start_scoped`, `CoordinatorError::Stopped`); a clearer retryable answer
needs a new coordinator error. A client that hangs up during a prefill is
noticed only at the next chunk.

CPU and Fake-engine tests are not qualification.

## Draft model memory gaps closed — 2026-10-02 (branch `fix/drafter-memory-gaps`)

Gaps left by the vLLM 0.30 findings (Qwen3.8-27B NVFP4 with DFlash2):

1. **First start above its reservation** (ADR 0014 amendment A8). The derived
   startup placeholder is now `max(request + graphs, weights × 2.25 + margin)`
   for vLLM and SGLang, with a 1.25 GiB graph allowance per captured model.
   Recorded as `startup_graphs_bytes`; older revisions decode unchanged (1.6,
   no allowance).
2. **Draft model KV in the fitted context** (amendment A9).
3. **Hybrid checkpoints on vLLM** (amendment A10, owner decision 2026-10-02).
   vLLM gets `--max-num-seqs` 32, the router's per-deployment bound (one
   shared constant), unless the deployment or installation sets it. The fit
   follows vLLM's recurrent-state block layout for those sequences.
   SGLang is unchanged: with DFlash2 at the default 4 GiB it refuses (state
   cache), so such a deployment declares `memory.kv_cache`.

Live on host B (standalone, fresh 0700 state and config directories, fresh
engine caches), vLLM 0.30, no memory stated (4 GiB KV derived):
- Without DFlash2: context 117600 tokens, reservation 55.19 GiB, first-start
  peak 49.88 GiB, answered Jupiter.
- With DFlash2: context 23072 tokens, reservation 63.25 GiB, peak 49.89 GiB,
  answered Jupiter.

Before the factor change the same starts peaked at 47.86 GiB and 50.49 GiB
against 41.92 GiB and 50.08 GiB.

With `kv_cache: 16GiB` and DFlash2: vLLM peaked at 50.59 GiB and SGLang at
44.63 GiB (both reserved 51.75 GiB then); both answered.

SGLang 0.5.20 with no memory stated: without DFlash2 it started, answering
with running requests capped at 1; with DFlash2 it refused to start.

Cleanup complete. CPU and Fake-engine tests are not qualification.

## TensorFold 0.6.2 verified — 2026-10-02 (branch `feat/tensorfold-0.6.2`)

TensorFold 0.6.2 joins 0.6.0 and 0.6.1 in the verified set (ADR 0023 §1), so
`engine add` shows it `custom no`; 0.6.3 and later stay custom. Between the
0.6.1 and 0.6.2 tags, `cli_args.py`, `server/metrics.py` and `cuda/build.py`
are unchanged, so the reserved, sensitive and typed options, the metric
names and the build toolchain are the same. One kernel source changed
(`gdn.cu`, now `tensorfold_gdn_v2`); every extension builds again on the
first start because CapyCTL keys the extension cache by version. The engine
now prints one line per request and a `done` line per reply (prompt and
reply token counts, a token hash, tok/s, TTFT, accepted/drafted; no prompt
or reply text); successful `GET /health` and `/metrics` are not logged.

Live on host A (standalone from this branch, fresh 0700 state and config
directories, a new `~/tensorfold-0.6.2-venv` with torch 2.13.0 cu130,
owner-approved; system CUDA 13.0): `engine add --approve-option=--drafter
--approve-path <drafters>` registered `tensorfold 0.6.2`, custom no, deep
park disabled. The recipe deployments ran with the models downloaded by
CapyCTL (`model: {hf: ...}`):

| | 0.6.1 (recipes) | 0.6.2 |
|---|---|---|
| Nemotron 3.5 Lightning 30B-A3B 4-bit, ready cold (after download) | 104 s | 101 s |
| Qwen3.8-27B NVFP4 + DFlash2, ready cold (after download) | 119 s | 116 s |
| Nemotron decode, median of 3 streams, 512 tokens | 132 tok/s | 143 tok/s (125 to 149) |
| Qwen3.8-27B DFlash2 decode | 48.2 tok/s | 45.2 tok/s (34.0 to 46.5) |
| Qwen3.8-27B `--no-drafts` decode | 11.9 tok/s | 12.0 tok/s |
| Warm start (`start` after `stop`) | 8 s / 10 s | 8.1 s / 9.7 s |

Every cold start above is from an empty kernel cache. The 0.6.1 ones include
verifying the weights already in the store; the 0.6.2 Nemotron one starts when
its download was verified and the Qwen3.8 one at `start deployment` with the
weights verified, and both include the checkpoint digest, the load and the
kernel builds. Prompts differ from the recipes', so
the medians are not paired; the one
request both runs share (354 of 2355 drafts accepted, the same `token_sha`)
decoded at 33.8 tok/s on 0.6.1 and 34.0 on 0.6.2. Kernel builds from an empty cache: Qwen3.8 builds `nvfp4_ck_v6` at startup
and `qwen_b16_v4`, `gdn_v2`, `prefill_attention_v1` and `qmm_v5` on its first
request (probe TTFT 65 s); Nemotron builds `experts_v7` and `qmm_v5` at
startup and `nemotron_scan_rows` and `prefill_attention_v1` on its first
request (34 s). Both answered Jupiter plain and streaming, with
`reasoning_content` passed through and the `tensorfold` record kept. With
both deployed (32 + 36 GiB over the half-memory standalone limit), each
request for the stopped one switched (`released: stopped`) and answered in
11.1 to 13.3 s. `park deployment` stays refused (ADR 0023 §6); a deployment
without a drafter still fails to start Qwen3.8 dense with the `--drafter`
or `--no-drafts` hint. `/health` and `/metrics` answered on loopback with
the pinned families (the live body is now a parser fixture); standalone has
no host load report, so its latency view shows the router tier only, by
design (`roles.rs`). Deployments deleted, profile removed, drained shutdown,
state removed, no CapyCTL, engine or GPU process left. No CapyCTL bug found.
After rebasing onto the vLLM 0.30 findings fix, a second run from a fresh
state (empty kernel cache) gave the same results: Qwen3.8 DFlash2 44.6 tok/s,
Nemotron 141 tok/s, Nemotron started by a request in 48 s with its builds.

## vLLM 0.30 live findings fixed — 2026-10-02 (branch `fix/vllm-030-findings`)

Four findings from the Qwen3.8-27B NVFP4 runs on vLLM 0.30.0 and SGLang 0.5.20:

1. **vLLM's `reasoning` delta.** vLLM 0.29 and 0.30 stream a trace as
   `delta.reasoning`; the streamed-delta allowlist named only
   `reasoning_content`, so with `--reasoning-parser` the readiness probe failed
   (`chat terminal result unverified`) and the engine was stopped. `reasoning` is
   now relayed unchanged, collected under its own name, and counts as a probe
   answer.
2. **Draft model memory** (ADR 0014 amendment A6). The draft model's weight files
   are counted with the checkpoint's, so the request, KV cache and startup
   placeholder cover it.
3. **First-start Initialize window** (ADR 0014 amendment A7). vLLM and SGLang
   derive Initialize as the load term plus a 480 s first-start allowance.
4. **Engine logs.** Behaviour kept, docs corrected: vLLM and TensorFold output is
   kept owner-only (launch-failure summaries read it); SGLang's is discarded once
   SGLang is imported unless `--debug-engine-logs`, so its file is empty.

Live on host B (standalone, fresh 0700 state and config directories): vLLM 0.30
Qwen3-4B with `--reasoning-parser qwen3` reached Ready and streamed 199
`reasoning` deltas through CapyCTL; the collected response carried `reasoning`.
Derived Initialize 690 s. Qwen3.8-27B NVFP4 with DFlash2: weights recorded
25.77 GB (checkpoint plus draft model), cold 49.25 GiB, first-start peak
50.46 GiB (1.2 GiB above the placeholder, CUDA graphs), second start reserved the
measured peak and peaked at 50.18 GiB, steady 48.97 GiB against a 49.25 GiB
Ready charge; warmup 235 s first, 79 s next; derived Initialize 860 s. SGLang
Qwen3-4B: Ready, log file 0 bytes. Cleanup complete. CPU and Fake-engine tests
are not qualification.

## vLLM 0.30.0 verified — 2026-10-02 (branch `feat/vllm-0.30`)

vLLM 0.30.0 joins 0.29.0 in the verified set (ADR 0018), so `engine add`
shows it `custom no`; 0.30.1 and later stay custom. The upgrade renames or
removes nothing CapyCTL drives: the development routes, the
`VLLM_SERVER_DEV_MODE` switch, every reserved and typed destination and the
metric names are unchanged. New options are covered: `--enable-scale-out`
(registers `/render`, `/derender` and `/inference/v1/generate`) is reserved
on both sides of the launch, compared whenever the installed parser has it;
`--watermark-config` and `--engram-config` need named host approval like
every other `*-config` option; `--load-format ipc_cache` already needs
approval, as any `--load-format` does. The removed environment variables
(`VLLM_PREFIX_CACHE_RETENTION_INTERVAL`, `VLLM_MM_HASHER_ALGORITHM`,
`VLLM_NIXL_EP_MAX_NUM_RANKS`) were never set or documented by CapyCTL.

Live on host B (standalone from this branch, fresh 0700 state and config
directories, a new `~/vllm-0.30-venv` with vLLM 0.30.0 and torch 2.13.0
cu130, owner-approved): `engine add` registered `vllm 0.30.0`, custom no,
deep park enabled. Qwen3-4B (`docs/examples/deployment-vllm.yaml`, checkpoint
already downloaded) was ready in 109 s and answered with Jupiter. Park was a
level-2 sleep (11.9 GiB freed, none backed up in CPU), parked in about 2 s;
MemAvailable went 116.7 GiB idle, 100.5 GiB ready, 112.8 GiB parked. A
request woke it (`wake_up` weights, `collective_rpc` `reload_weights` in
7.3 s, `wake_up` kv_cache) and answered in 9.2 s end to end. A stream hung up
after 2 s and a park right after it settled `parked` in 0.6 s. Deployment
deleted, profile removed, drained shutdown, state removed, no CapyCTL,
engine or GPU process left. No CapyCTL bug found.

## First model on each engine — 2026-10-02 (branch `docs/engine-examples`)

`docs/examples/deployment-{vllm,sglang,tensorfold}.yaml` and the "First model
on each engine" section of `docs/guide/engines.md`, with output captured live
on host B (standalone, published 0.1.1 binary from the one-line installer,
fresh state and config directories): `engine add`, `deploy model`,
`start deployment --wait` and one chat request per engine, run one at a time.
vLLM 0.29 Qwen3-4B ready in 55 s, SGLang 0.5.20 Qwen3-4B in 61 s (checkpoint
already downloaded), TensorFold 0.6.1 Nemotron 3.5 Lightning 30B-A3B 4-bit in
17.5 min including its 18.5 GB download, 10 s on a later restart. Each answered
with Jupiter. Deployments deleted, profiles removed, drained shutdown, no
CapyCTL or engine process left. No CapyCTL bug found.

## 0.1.1 published — 2026-10-02

0.1.1 is published; tag `v0.1.1` is at the #19 merge (`bf13c72`). The
Release build workflow built both architectures, and the strict privacy scan
passed on both with no skips. The live check on host B ran from the release
artifacts: TensorFold 0.6.1, Nemotron ready in about 97 s, and a chat request
answered. A fresh-user curl install worked.

Follow-ups from that check are on `fix/post-0.1.1`: `engine add` names the
control socket it tried when no role answers (a role started with another
`--state-dir` looked absent, so the profile stayed unpublished until a
restart); the install guide says the state directory and its ancestors must
not be group- or world-writable; a refused delete says a stop may still be in
progress.

## 0.1.1 release preparation — 2026-10-01

Version bumped to 0.1.1 with release notes in
`docs/operations/release-notes-0.1.1.md` (TensorFold 0.6.0 and 0.6.1, the live
follow-ups, `ci-local.sh`). The verification result is in the release pull
request. Nothing is built, tagged or published yet: next are the manual
Release build workflow, the strict local packaging check, a draft release and
the live check from the draft (docs/operations/releasing.md). The owner
publishes.

## Live follow-ups — 2026-10-01

The four problems the TensorFold run found are fixed
(`docs/specs/2026-10-01-live-followups-design.md`): Hugging Face link chains
are measured (ADR 0014 A5) and an unmeasurable checkpoint says why;
`validate config` without `--host` checks `resources`; a role starts with no
engine and `engine remove` takes the last profile (ADR 0018 A2); a client
hang-up mid-stream closes the engine connection and the request stays
charged until the engine reports quiescence (SPEC §10). TensorFold 0.6.1 is
verified beside 0.6.0 (ADR 0023).

The live run found two more CapyCTL bugs, both fixed with a failing test
first:

- `091e3a1`: on a remote host, a park or stop accepted right after a
  hang-up never settled the cancelling lease. The quiescence check matched
  the host's load sample only against Ready launches, and an instance stops
  being one once a park or stop is accepted. On vLLM 0.29 on host B, a park
  0.04 s after the close was still `parking` after 150 s (it would have
  waited for its 900 s deadline), and a stop drained its full 30 s. After
  the fix the same park finished 1.35 s after the close and the stop 1.8 s
  after its command.
- `9d47b8f`: `validate config` without `--host` refused the engines guide's
  TensorFold block (`resources.cold.devices[0].sharing` missing) while
  `deploy model` accepted it. Offline, a missing `sharing` now takes the
  deployment's own value for that device, else `exclusive`. The guide-shaped
  file now validates.

`1467b86` fixes the matrix harness: `roles.sh` parsed `join host` text
output as JSON.

Live on host A and host B (GB10), with Nemotron 3.5 Lightning 30B-A3B
4-bit at a 32768-token context, 32 GiB cold and 30 GiB ready:

- TF2 on an unmodified two-hop cache passed on TensorFold 0.6.0 (host B,
  standalone). A fresh `huggingface_hub` snapshot links each file to
  `../../blobs/<sha>`, which links again into `hub/blobs/<xx>/<sha>`. The
  digest was measured (no `unsafe_file`) and the deployment was ready 100 s
  after the start command, including the measurement and the kernel build.
  CapyCTL measured a 26.2 GiB startup peak. A plain request answered `42`
  with the `tensorfold` object (159 tok/s decode), the streaming one `42` in
  50 events, and a 3343-token request took 29.0 s with `nvidia-smi` at most
  18.7 GiB.
- TensorFold 0.6.1 (new venv on host B, an owner exception). TF1 passed:
  `engine add` on the running empty role printed `Registered tensorfold
  (tensorfold 0.6.1)`, with custom `no`, deep park disabled and CUDA
  `/usr/local/cuda`, and published it live. TF2 passed standalone. A new
  deployment was ready 88.4 s after the start (digest and first 0.6.1 kernel
  build), with a 26.4 GiB startup peak. It answered `42` plain (158 tok/s)
  and streaming (50 events), and the 3343-token request took 29.2 s with
  `nvidia-smi` at most 18.7 GiB. TF2 also passed remote, on host B under the
  server: ready in 95.9 s, `42` plain and streaming through the router, and
  the host's load report present for the launch. The router's selection
  read `load_source: engine` with `engine_running: 1` during a long request
  and 0 when idle, so the vLLM-named metric mirrors 0.6.1 adds no longer
  hide the load.
- TF5 passed on both versions. Before, the engine stayed busy until it had
  finished all 2048 tokens and read idle 17.2 s after the close. On 0.6.0,
  standalone, the engine read `busy: false`, `requests_running: 0` 0.07 s
  after the client closed, having generated 32 tokens. The lease was
  acknowledged quiescent 0.21 s and closed 0.40 s after the close. A stop
  sent then finished 1.4 s later, and the process was gone 1.6 s after the
  close. On 0.6.1, standalone: idle 0.08 s after the close (32 tokens),
  lease closed 0.40 s, stopped 0.95 s after the stop command, process gone
  1.2 s after the close. On 0.6.1, remote through host ingress: idle 0.17 s
  after the close, ingress connection to the engine closed with it, lease
  acknowledged 0.48 s, a stop accepted 0.04 s after the close finished
  1.6 s after the close.
- vLLM 0.29 on host B, remote (server and host agent), passed after the
  first fix. Host ingress closed its connection to the engine 0.1 to 0.2 s
  after each hang-up, the engine log showed generation stopping (`Running: 0
  reqs`), and the lease was acknowledged in 0.46 to 0.96 s. A park after the
  lease closed (0.61 s) left the deployment `parked` 1.03 s after the park
  command. A park 0.05 s after the close was acknowledged at 0.60 s and
  parked 1.35 s after the close.
- SGLang 0.5.20 passed, remote, on both hosts. On host A, the lease closed
  0.20 s after the close and a park then took 0.62 s. With the park sent at
  the close, the lease was acknowledged at 0.98 s and the deployment parked
  1.41 s after the close. On host B: lease closed after 1.42 s, park then
  0.57 s; with the park sent at the close, acknowledged 0.85 s, parked 1.32 s.
- Empty role and last profile passed (host B, standalone). A drained
  shutdown reported `drained: true` with 0 in flight. On the restarted role,
  `engine remove tensorfold` removed the last profile (`Published yes`), and
  `status deployment nemotron` printed `Engine  none: run capyctl engine add
  <path>`. The role then booted with no engine, showing the banner `Engines
  none: run capyctl engine add <path>`, and `engine add` of 0.6.1 published
  live.
- M75 passed on both hosts: each booted with no engine, published no
  profiles, started no engine child, and exited 0 in both JSON and text
  modes.

Found and not fixed: a deployment keeps the executable it was resolved with
(ADR 0018: deployments are not re-resolved). After the 0.6.0 profile was
removed and 0.6.1 registered under the same name, starting the existing
deployment ran the 0.6.0 executable, while `status deployment` showed the
0.6.1 installation. When the deployment's executable is no longer
registered, the view falls back to the role's first installation.
Redeploying picked up 0.6.1.

All roles stopped with a drained shutdown. Deployments and profiles were
removed, and no capyctl, engine or GPU process is left on either host.
The venvs, the fresh Hugging Face cache and the model directories are kept.
Local checks: formatting, Clippy with warnings denied (workspace), core
suite 1163 passed / 0 failed, workspace 2398 / 0, site check. CPU and
Fake-engine tests are not qualification.

## TensorFold engine — 2026-10-01

ADR 0023 is implemented: `tensorfold` is the third engine kind, registered with
`capyctl engine add` after a closed-PATH toolchain check, launched restart-only
with a private per-version build cache, ready on `/health` and the model list,
drained on its own counters before a stop, and part of the switch planner like
the other engines. A role's own TensorFold is `local_engine.tensorfold`
(`--tensorfold-bin`, `CAPYCTL_TENSORFOLD_BIN`); a drafter is an approved path
extra argument, as vLLM's and SGLang's draft models are.

Live on host B (GB10, standalone, TensorFold 0.6.0 venv, Nemotron 3.5 Lightning
30B-A3B 4-bit, 32768-token context, 32 GiB cold and 30 GiB ready):

- TF1 passed: `engine add` registered `tensorfold 0.6.0` with deep park
  disabled and the CUDA toolkit found at `/usr/local/cuda`, which the profile
  records as its `cuda_home`; `engine list` showed it. The guide carries the
  captured output.
- TF2 passed after a fix. The first start built the kernels and answered the
  readiness probe about 88 s after the start command (TensorFold reported the
  model loaded in 50.7 s), then failed Ready with `invalid lifecycle input`: the store, the completion check and
  the protocol required a worker process, and TensorFold serves from one.
  Fixed in `7c3a4f3` (a single api process is a group; parked or restored
  claims still need a worker). The next start was ready in 8.1 s. A plain
  request answered `42` with the `tensorfold` object (161 tok/s decode); the
  streaming one answered `42` in 50 events; a 6000-token request ran at
  132 tok/s. Peak memory stayed inside the reservation: CapyCTL measured a
  20.2 GiB startup peak, `nvidia-smi` showed at most 19.3 GiB and
  MemAvailable dropped at most 25.1 GiB (first start, with the build).
- TF3 passed after a fix: a standalone document refused
  `server.lifecycle_defaults` and its coordinator had no idle policy. Fixed in
  `c5ef90c`. With `ready_idle_timeout: 60s` the deployment stopped 56 to 61 s after
  the role restarted with it Ready, its process was gone and the GPU showed no process. One request
  woke it in 8.4 s with the answer (warm start: no kernel build, loaded in
  6.9 s). `park deployment nemotron` was refused `unsupported_capability`
  (exit 5).
- TF4 passed: with Qwen3-4B on vLLM 0.29 (KV cache 26 GiB, 42.7 GiB startup
  reservation; the standalone managed limit is half of the 121.7 GiB) the two
  do not fit together. Three switches each way all answered 200: vLLM
  `released: parked` and TensorFold `released: stopped` in the role's switch
  events, nemotron answered in 9.9 to 11.2 s and Qwen in 18.4 to 22.3 s; the
  largest `nvidia-smi` total was 35.1 GiB (vLLM ready).
- TF5 passed: a stream closed by the client after 2000 bytes left TensorFold
  `busy: true` until it finished the 2048 tokens, and `/health` read
  `busy: false`, `requests_running: 0` 17.2 s after the close, inside the
  30 s drain bound. The router drains a disconnected stream rather than
  cancelling it (SPEC §10: a client disconnect is not proof the engine
  stopped); the same disconnect sent to the engine directly stopped it after
  29 tokens. The release (`start deployment qwen --evict --wait`) came 2.1 s
  after the idle read; the engine stopped answering 0.3 s and exited 0.7 s
  after the release began.

Fixed in the live follow-ups entry above.

All roles stopped with a drained shutdown; deployments deleted; no capyctl or
engine process and no GPU process left on host B. Local checks: formatting,
Clippy with warnings denied, core suite 1139 passed / 0 failed, workspace
2347 / 0, Python runtime 302 OK, site check. Discrete GPUs run TensorFold
unqualified. CPU and Fake-engine tests are not qualification.

## 0.1.0 release candidate — 2026-10-01

Draft release `v0.1.0` (not published) targets `f224320`, the commit both
archives were built from (`dirty: false`, rustc 1.98.1). Each architecture was
built twice with byte-identical archives: x86_64 on a maintainer laptop,
aarch64 on host A. The x86_64 strict packaging check passed with no SKIP lines;
the aarch64 packaging and installer checks passed on host A (42 installer
checks), and its binary has no private-denylist or lab-host match and no old
project name. The draft's six assets match the local builds and
`SHA256SUMS`, and `install.sh --version v0.1.0` installs from the draft with
GitHub authentication.

Installed-binary live check from these archives:

- Laptop (RTX 4090, standalone): the release notes' upgrade steps (old binary
  and shared files removed, fresh state), then vLLM 0.29 Qwen3-4B deployed in
  55 s, served, parked to 1009 MiB and woke on request in 1.4 s; an SGLang
  0.5.20 Qwen2.5-1.5B deployment switched with it both ways. `list hosts` and
  `list engines` work on standalone.
- Laptop server with hosts A (SGLang) and B (vLLM): both joined and online on
  0.1.0; both deployments served through the server, a request without the key
  got 401, both parked and woke on request in 47.3 s (SGLang) and 9.0 s
  (vLLM). A first attempt was refused with `insufficient resources` while
  another workload held the hosts' memory, as intended.

All roles stopped with a drained shutdown; deployments deleted; no capyctl or
engine process and no GPU process left on any machine. The history scan finds
no private names beyond the public owner handle in merge messages and the
public site URL; planning-tool references were removed from the tree (history
keeps them, by owner decision). CPU and Fake-engine tests are not
qualification.

## Earlier entries

Entries up to 2026-09-30 (the September live gates, the 0.1.0 release
candidates, the rename to CapyCTL) and the planning sections written before the
rename (milestones, open questions and owner attention as they stood then) are
archived in [archive/f2-status-2026-09.md](archive/f2-status-2026-09.md).

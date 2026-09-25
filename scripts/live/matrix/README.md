# Two-host matrix harness (plan unit W2)

This harness runs the rows of `docs/plans/2026-09-22-two-host-engine-matrix.md`.
It uses one server on control-host and host roles on host-a and host-b. Results go in
`docs/runbooks/f2-current-status.md` only. The harness records evidence and does not decide
whether a row passed. A dry run, the local rehearsal, CPU tests and Fake-engine tests are not
evidence for any row.

## Order of a run

```bash
M=scripts/live/matrix
$M/sync.sh all                  # snapshot, rsync to both ~/mllm-f2, check digests, build, check runtime
$M/roles.sh up normal           # server + both hosts enrolled and online; writes fixtures
$M/e0.sh static      target/live/matrix/E0-static
$M/e0.sh checkpoints target/live/matrix/E0-checkpoints   # slow; payload digests, equality across hosts only
$M/e0.sh vparity     target/live/matrix/G12-vparity      # read-only host-b vLLM parity (G12)
$M/run_row.sh M01
$M/run_row.sh M16 --tag s92-14 -- s92-14
$M/run_row.sh M73 --tag v92-4 -- v92-4    # engine lifecycle gate; also s92-4
$M/run_row.sh M38 --tag v92-4 -- v92-4    # launch failures close, then recovery
$M/run_row.sh M74 --tag v92-14 -- v92-14  # timeouts.initialize expires, override wins
$M/run_row.sh M75 --no-e0                 # no engine installation, no boot
$M/run_row.sh TC --tag s92-4 -- s92-4 '["--tool-call-parser","qwen25"]'   # tool calls need the engine's parser
$M/run_row.sh REJ --tag v17-4 -- v17-4    # engine 400/413/422 relayed as engine_rejected, leases closed
$M/roles.sh down
```

A row that fails, or exits early, deletes every deployment it deployed with
`delete deployment --stop` (`cleanup_failed_row`); set `KEEP_FAILED=1` to keep them
for inspection.

Put `DRY_RUN=1` in front of any command to print its plan without running it. A dry run
still parses every remote script with `bash -n`. `run_row.sh … --dry-run` writes to
`target/live/matrix/dry-run/<row>/`.

## Files

| File | Purpose |
|---|---|
| `lib.sh` | Hosts, IPs, venvs and paths. Also `x`/`rsh` (logged, dry-run aware), run state and the tree digest |
| `sync.sh` | Snapshot of the worktree without `target .git .superpowers`, `*.log` or bytecode. Then tree digest, rsync with group/other write removed, digest check on each Spark, `bash -lc` build with `~/.local/bin` on PATH, `scripts/check-release-clean.sh` on the control-host and Spark binaries, and runtime sync and check |
| `check_runtime.py` | Runs on the Spark. Checks that the required runtime files exist, that no file or ancestor is group- or other-writable, and prints their SHA-256 |
| `roles.sh` | Preflight, then `init`/`start` the server in tmux on control-host. For each host: `init host`, device probe (`python3 -m runtime.sglang_device`), engine versions, host document, invite, `join host` with an absolute `--join-file`, then `start host` in tmux |
| `templates/host.json`, `gen_host_doc.py` | Host document built from the measured GPU UUID and digest. It holds `sglang` and `vllm` profiles with the host's venvs, host-fixed `args: []`, the D10 budget and ingress on the Tailscale IP |
| `budgets/normal.yaml`, `budgets/tight.yaml`, `gen_budgets.py` | D10 budgets: normal is 80% limit, 10% reserve, `max_parked` 2; tight is 78 GiB, `max_parked` 1. `check` prints the fit table. `suggest` turns a measured MemAvailable drop into a P2 request |
| `models.json`, `gen_deployment.py` | Deployment fixtures `<v\|s><92\|17>-<4\|14\|27\|27f\|30>` with a typed `engine_config` and declared `memory {request, kv_cache}`, so the phases are derived. `--co` writes each model's co-residence variant (`<name>-co`: context 8192, smaller KV cache) sized so q30+q4 and q27f+q14+q4 fit the normal budget together |
| `e0.sh`, `ledger.py` | E0 evidence. Collected read-only through CLI `list`/`status`/`inspect` and SELECTs (`mode=ro`, `query_only`) |
| `identity_probe.py` | I1 probe: greedy, 32 tokens, top-1 logprobs. Modes are `capture`, `check` and `distinct` (a row is void if goldens collide) |
| `infer.py`, `loadgen.py`, `matrixhttp.py` | Routed requests with markers and SSE framing checks. `loadgen.py` runs concurrent streaming and non-streaming load, with long-prompt skew for M57 |
| `fault.sh`, `signal_owned.py`, `memhog.py`, `role_exec.sh` | D6 faults. Signals go only to (pid, start ticks, boot id) identities, never by name. Allocations are at most 40 GiB and 1800 s, with a trap and `timeout` |
| `run_row.sh`, `rowlib.sh`, `rows/M01.sh`, `rows/M05.sh`, `rows/M16.sh` | Row runner. Evidence goes to `target/live/matrix/<row>[-tag]/`, and an earlier directory for the same row is archived first. `SCRATCH_ROWS=<dir>` runs `<dir>/<ROW>.sh` in place of `rows/<ROW>.sh` when it exists. `rowlib.sh` also holds the shared checks: `variant` (a per-row fixture variant), `refused`, `wait_operation`, `host_idle`, `cleanup_check` and `closure_check` |
| `rows/M80.sh`, `bench.py`, `bench_report.py`, `test_bench.py` | M80 performance benchmark through the shipped router (after the current live phase, owner 2026-09-23). `bench.py run` drives streaming cells and records per-chunk arrival times; `once` times one request to its first token (cold start, wake, switch); `promdelta` windows an engine `/metrics` scrape; `latencydelta` windows the server's mllm latency view (router phases, host ingress times and engine histograms marked `source: engine`, read through `status deployment --output json`) and `report` splits the path overhead into client to router, router, router to ingress and ingress to engine. With the server's `observability.timing_header` on, each response also carries its own router timings (`x-mllm-timing` header, and an SSE comment on streams) and `run` summarizes them per cell. `bench_report.py` folds every `M80-*` directory into one table. `test_bench.py` checks the parsing and arithmetic against a fake streaming server; it is not evidence |
| `rows/M48.sh`, `soak.py`, `rows/M49.sh` | Phase G soak (2026-09-24). M48 deploys q4 and q14 fixtures of both engines on both hosts plus the two-instance replica route `qwen3-4b`, with host-b on the tight policy, and runs `soak.py`: a seeded random walk (`SOAK_SEED`, `SOAK_STEPS`, resumable on the deployments it left with `SOAK_RESUME=1 SOAK_FROM_STEP=…` and a new `--tag`) over inference, tool calls, start (with and without `--evict`), stop, park, wake on request, request-driven switching on the tight host (two of its three single-instance deployments Ready, then a request for the third), count-only revisions, instance stop and start, `delete --stop` and redeploy, `drain host`, agent SIGTERM and restart, engine SIGKILL and a short agent SIGSTOP. After every step it waits for the deployments to settle and checks the invariants listed in its docstring (ledger limits, leases, I1 on an 8-token prefix plus one recorded checkpoint digest per model, orphan and GPU processes, state and ledger agreement, identities ended or kept, age of uncertain state). Evidence: `M48/soak/{steps.jsonl,violations.jsonl,summary-*.json,steps/}`. Run it with `KEEP_FAILED=1` so M49 cleans the soak state: `delete --stop` everything, an empty ledger, clean hosts without bytecode, MemAvailable near the pre-soak baseline, then `roles.sh down` with each role logging exit 0 (`role_exec.sh`) |
| `rows/M53.sh`, `rows/M53D.sh`, `rows/M66.sh`, `rows/SGLMO.sh` | Failure and removal rows (2026-09-24). M53: a switch to a target whose engine refuses an argument fails closed with the incumbent intact; the failed target then stops, starts and fails again, and `delete deployment --stop` removes it. M53D: `delete deployment --stop` on a failed launch leaves no residue. M66: `delete deployment --stop` during a stream that outlasts the drain bound (`M66_PROMPT` overrides the prompt). SGLMO: an SGLang modelopt deployment with `residency: deep` is refused `capability_missing:deep_park` with its `restart_only` hint, and the `restart_only` variant serves and stops clean. M53D and M66 judge cleanup after the delete with `residue_check`: the ledger snapshot holds nothing for the deleted id except its tombstone |
| `rows/M38.sh`, `rows/M73.sh`, `rows/M74.sh`, `rows/M75.sh`, `proc_probe.py` | The engine gates that replaced `live_vllm.rs`, `live_sglang.rs` and `run-on-spark.sh` (owner decision 2026-09-22): launch failures and recovery (M38), one lifecycle with argv, access and memory checks (M73), the `timeouts.initialize` deadline (M74) and no-engine refusal (M75). `proc_probe.py` reads an owned process's argv (credential values redacted) and named, non-secret environment variables |

## Rules the harness keeps

- **Secrets.** The API key is read from `server-credentials.json` into `MLLM_API_KEY` only.
  The CLI reads the admin token itself. Invitation files are copied with `scp` and never
  printed. Engine-secret tables and binding payloads are never selected.
- **Faults.** Engine PIDs come from `runtime_bindings.identities_json` for the deployment on
  that host. Role PIDs come from the pid files `role_exec.sh` writes. A PID whose start ticks or
  boot id differ is refused.
- **Policy changes.** Switching a host between normal and tight means `roles.sh host-doc <host>
  tight`, then `host-down` and `host-up`. The host document's fingerprint covers the whole
  document.
- **P2 memory.** Measure each model on its first live run. Pass the M16 `before` and `ready`
  MemAvailable values to `gen_budgets.py suggest`. Record the result in
  `target/live/matrix/run/measured.json` as `{"14": {"request_bytes": …, "kv_cache_bytes": …}}`,
  then run `gen_budgets.py write --measured …` and `roles.sh fixtures`.
- **Checkpoint digests.** `e0.sh checkpoints` hashes each model directory's payload. That is not
  the product's checkpoint manifest digest, so it is evidence of equality across hosts only and is
  never declared as a fixture's `content_fingerprint` (a declared mismatch is refused
  `checkpoint_mismatch` at first placement). Fixtures keep the placeholder fingerprint and the
  host measures and records the real digest.
- **A second worktree.** `MLLM_REMOTE_TREE=$HOME/mllm-soak` syncs, builds and runs from its
  own tree on the Sparks, so two worktrees never replace each other's binary.
- **Goldens.** The first `i1` for a model in a run captures `model@engine` into
  `target/live/matrix/run/goldens.json`. Run `identity_probe.py distinct` before any
  switching row.

## Open assumptions (unproven until a live run)

- The anchor (`qwen3.8-27b-nvfp4`) uses `quantization: modelopt` on SGLang and leaves it for
  vLLM to detect. `27f` adds an FP8 KV cache (`fp8` on vLLM, `fp8_e4m3` on SGLang). Both use
  `language_model_only: true`. No engine has served this checkpoint yet (G17). If an engine
  needs a different value, edit `models.json`.
- The q4 and q14 KV and request values fit resolution (weights + KV ≤ request). They do not
  fit with the 8 GiB placeholder margin added. M16 measures the real values.
- The router is assumed to forward `logprobs`/`top_logprobs`. If it does not, I1 compares
  empty token lists and `capture` fails.
- SGLang fixtures are `deep`, because the matrix hosts keep deep parking on (ADR 0012) and
  the rows exercise park and wake. A `restart_only` SGLang deployment is valid (SPEC §6.2):
  it launches without the memory saver and its park is refused `unchanged`. A host with
  `MLLM_DEEP_PARK=off` refuses `deep` deployments, so these fixtures do not apply there.
- The remote login shell must be bash, because `rsh` sends `bash -lc $'…'`.
- `MATRIX_LOCAL_RSH=1` with `MLLM_REMOTE_HOME=<scratch>` runs the "remote" scripts on this
  machine against fake venvs and models. It exists only to rehearse the harness.

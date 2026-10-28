# Two-host matrix harness (plan unit W2)

This harness runs the rows of `docs/plans/2026-09-22-two-host-engine-matrix.md`.
It uses one server on the control host and host roles on two hosts, A and B. Results go in
`docs/runbooks/f2-current-status.md` only. The harness records evidence and does not decide
whether a row passed. A dry run, the local rehearsal, CPU tests and Fake-engine tests are not
evidence for any row.

## Hosts

The machines come from `hosts.local.env` in this directory, which is gitignored. Copy
`hosts.example.env` to `hosts.local.env` and set `HOST_A`, `HOST_B`, `CONTROL_HOST`, their
addresses, `REMOTE_HOME` and the engine environments. Every script sources it through
`lib.sh` and stops with an error when it is missing or incomplete; `CAPYCTL_MATRIX_HOSTS_ENV`
selects another file. Fixture tags name the host by its code: `a` for `HOST_A`, `b` for
`HOST_B` (`va-4` is vLLM q4 on host A). Host arguments to `roles.sh` and the ENG rows take
the code `a` or `b` (or the configured name).

## Order of a run

```bash
M=scripts/live/matrix
$M/sync.sh all                  # snapshot, rsync to both ~/capyctl-f2, check digests, build, check runtime
$M/roles.sh up normal           # server + both hosts enrolled and online; writes fixtures
$M/e0.sh static      target/live/matrix/E0-static
$M/e0.sh checkpoints target/live/matrix/E0-checkpoints   # slow; payload digests, equality across hosts only
$M/e0.sh vparity     target/live/matrix/G12-vparity      # read-only host B vLLM parity (G12)
$M/run_row.sh M01
$M/run_row.sh M16 --tag sa-14 -- sa-14
$M/run_row.sh M73 --tag va-4 -- va-4    # engine lifecycle gate; also sa-4
$M/run_row.sh M38 --tag va-4 -- va-4    # launch failures close, then recovery
$M/run_row.sh M74 --tag va-14 -- va-14  # timeouts.initialize expires, override wins
$M/run_row.sh M75 --no-e0                 # a role with no engine boots with none and places nothing
$M/run_row.sh TC --tag sa-4 -- sa-4 '["--tool-call-parser","qwen25"]'   # tool calls need the engine's parser
$M/run_row.sh REJ --tag vb-4 -- vb-4    # engine 400/413/422 relayed as engine_rejected, leases closed
$M/run_row.sh ENG1 --tag a -- a va-4 sa-4   # ADR 0018: engine add on a systemd host, then serves
$M/run_row.sh ENG1 --tag b -- b vb-4 sb-4
$M/run_row.sh ENG3 --tag a -- a va-4         # remove refused in use, then --drain
$M/run_row.sh ENG2 --no-e0 -- a                # standalone, both engines, environment profiles
$M/run_row.sh ENG4 --no-e0 -- b                # rc.3 fallbacks; needs CAPYCTL_RC3_LOCAL/CAPYCTL_RC3_REMOTE
$M/run_row.sh LC2 --no-e0 -- a                 # llama.cpp on GB10 (LC1-LC6); first scripts/live/llamacpp-build.sh source model
$M/run_row.sh OT1 --no-e0 -- a                 # ADR 0037 tracing (OT1-OT11, "OpenTelemetry rows" below); the collector binary first
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
| `hosts.example.env`, `lib.sh` | Hosts, addresses, venvs and paths (from `hosts.local.env`). Also `x`/`rsh` (logged, dry-run aware), run state and the tree digest |
| `sync.sh` | Snapshot of the worktree without `target`, `.git`, hidden directories, `*.log` or bytecode. Then tree digest, rsync with group/other write removed, digest check on each host, `bash -lc` build with `~/.local/bin` on PATH, `scripts/check-release-clean.sh` on the control host's and each host's binaries, and runtime sync and check |
| `check_runtime.py` | Runs on the host. Checks that the required runtime files exist, that no file or ancestor is group- or other-writable, and prints their SHA-256 |
| `roles.sh` | Preflight, then `init`/`start` the server in tmux on the control host. For each host: `init host`, device probe (`python3 -m runtime.sglang_device`), engine versions, host document, invite, `join host` with an absolute `--join-file`, then `start host` in tmux. Every role runs under the harness's own tmux server (`tmux -L capyctl-mx`): a session added to an older tmux server inherits that server's limits (found live 2026-10-15: memlock 8 MiB, which broke NCCL at group bring-up). The run server's loopback ports are 7443/8443 unless `CAPYCTL_SERVER_MGMT_PORT`/`CAPYCTL_SERVER_INFER_PORT` name others (when another server on the control host holds them) |
| `templates/host.json`, `gen_host_doc.py` | Host document built from the measured GPU UUID and digest. It holds `sglang` and `vllm` profiles with the host's venvs, host-fixed `args: []`, the D10 budget and ingress on the host's private address |
| `budgets/normal.yaml`, `budgets/tight.yaml`, `gen_budgets.py` | D10 budgets: normal is 80% limit, 10% reserve, `max_parked` 2; tight is 78 GiB, `max_parked` 1. `check` prints the fit table. `suggest` turns a measured MemAvailable drop into a P2 request |
| `models.json`, `gen_deployment.py` | Deployment fixtures `<v\|s><92\|17>-<4\|14\|27\|27f\|30>` with a typed `engine_config` and declared `memory {request, kv_cache}`, so the phases are derived. `--co` writes each model's co-residence variant (`<name>-co`: context 8192, smaller KV cache) sized so q30+q4 and q27f+q14+q4 fit the normal budget together |
| `e0.sh`, `ledger.py` | E0 evidence. Collected read-only through CLI `list`/`status`/`inspect` and SELECTs (`mode=ro`, `query_only`) |
| `identity_probe.py` | I1 probe: greedy, 32 tokens, top-1 logprobs. Modes are `capture`, `check` and `distinct` (a row is void if goldens collide) |
| `infer.py`, `loadgen.py`, `matrixhttp.py` | Routed requests with markers and SSE framing checks. `loadgen.py` runs concurrent streaming and non-streaming load, with long-prompt skew for M57 |
| `fault.sh`, `signal_owned.py`, `memhog.py`, `role_exec.sh` | D6 faults. Signals go only to (pid, start ticks, boot id) identities, never by name. Allocations are at most 40 GiB and 1800 s, with a trap and `timeout` |
| `run_row.sh`, `rowlib.sh`, `rows/M01.sh`, `rows/M05.sh`, `rows/M16.sh` | Row runner. Evidence goes to `target/live/matrix/<row>[-tag]/`, and an earlier directory for the same row is archived first. `SCRATCH_ROWS=<dir>` runs `<dir>/<ROW>.sh` in place of `rows/<ROW>.sh` when it exists. `rowlib.sh` also holds the shared checks: `variant` (a per-row fixture variant), `refused`, `wait_operation`, `host_idle`, `cleanup_check` and `closure_check` |
| `rows/M80.sh`, `bench.py`, `bench_report.py`, `test_bench.py` | M80 performance benchmark through the shipped router (after the current live phase, owner 2026-09-23). `bench.py run` drives streaming cells and records per-chunk arrival times; `once` times one request to its first token (cold start, wake, switch); `promdelta` windows an engine `/metrics` scrape; `latencydelta` windows the server's capyctl latency view (router phases, host ingress times and engine histograms marked `source: engine`, read through `status deployment --format json`) and `report` splits the path overhead into client to router, router, router to ingress and ingress to engine. With the server's `observability.timing_header` on, each response also carries its own router timings (`x-capyctl-timing` header, and an SSE comment on streams) and `run` summarizes them per cell. `bench_report.py` folds every `M80-*` directory into one table. `test_bench.py` checks the parsing and arithmetic against a fake streaming server; it is not evidence |
| `rows/M48.sh`, `soak.py`, `rows/M49.sh` | Phase G soak (2026-09-24). M48 deploys q4 and q14 fixtures of both engines on both hosts plus the two-instance replica route `qwen3-4b`, with host B on the tight policy, and runs `soak.py`: a seeded random walk (`SOAK_SEED`, `SOAK_STEPS`, resumable on the deployments it left with `SOAK_RESUME=1 SOAK_FROM_STEP=…` and a new `--tag`) over inference, tool calls, start (with and without `--evict`), stop, park, wake on request, request-driven switching on the tight host (two of its three single-instance deployments Ready, then a request for the third), count-only revisions, instance stop and start, `delete --stop` and redeploy, `drain host`, agent SIGTERM and restart, engine SIGKILL and a short agent SIGSTOP. After every step it waits for the deployments to settle and checks the invariants listed in its docstring (ledger limits, leases, I1 on an 8-token prefix plus one recorded checkpoint digest per model, orphan and GPU processes, state and ledger agreement, identities ended or kept, age of uncertain state). Evidence: `M48/soak/{steps.jsonl,violations.jsonl,summary-*.json,steps/}`. Run it with `KEEP_FAILED=1` so M49 cleans the soak state: `delete --stop` everything, an empty ledger, clean hosts without bytecode, MemAvailable near the pre-soak baseline, then `roles.sh down` with each role logging exit 0 (`role_exec.sh`) |
| `rows/M53.sh`, `rows/M53D.sh`, `rows/M66.sh`, `rows/SGLMO.sh` | Failure and removal rows (2026-09-24). M53: a switch to a target whose engine refuses an argument fails closed with the incumbent intact; the failed target then stops, starts and fails again, and `delete deployment --stop` removes it. M53D: `delete deployment --stop` on a failed launch leaves no residue. M66: `delete deployment --stop` during a stream that outlasts the drain bound (`M66_PROMPT` overrides the prompt). SGLMO: an SGLang modelopt deployment with `residency: deep` is refused `capability_missing:deep_park` with its `restart_only` hint, and the `restart_only` variant serves and stops clean. M53D and M66 judge cleanup after the delete with `residue_check`: the ledger snapshot holds nothing for the deleted id except its tombstone |
| `rows/M38.sh`, `rows/M73.sh`, `rows/M74.sh`, `rows/M75.sh`, `proc_probe.py` | The engine gates that replaced `live_vllm.rs`, `live_sglang.rs` and an earlier host-specific wrapper script (owner decision 2026-09-22): launch failures and recovery (M38), one lifecycle with argv, access and memory checks (M73), the `timeouts.initialize` deadline (M74) and no-engine boot (M75). `proc_probe.py` reads an owned process's argv (credential values redacted) and named, non-secret environment variables |
| `rows/ENG1.sh`, `rows/ENG2.sh`, `rows/ENG3.sh`, `rows/ENG4.sh` | ADR 0018 engine registration rows, written and first run live 2026-09-25 (results in the status runbook). ENG1: `engine detect`/`add` of the existing vLLM and SGLang venvs on a host running under a transient systemd user unit (`roles.sh host-doc-bare`, `host-up-systemd`), `host.yaml` unchanged, both new profiles serve and stop clean. ENG2: standalone with `CAPYCTL_VLLM_BIN` and `CAPYCTL_SGLANG_BIN` both set publishes `local-vllm`/`local-sglang`; `engine remove` on one is refused (environment profile); a deployment on each serves; a registered `vllm-reg` serves with a stated context length, and the row records what a deployment without one does. ENG3 (sources ENG1.sh): `engine remove` of a profile in use is refused `profile_in_use`, then `--drain` stops the deployment through the ordinary path and removes it only on stop evidence; a start afterwards is refused, and `engine add` republishes it. ENG4 (sources ENG1.sh): the rc.3 version-skew fallbacks — a new CLI against an rc.3 agent (`agent_unreachable`, `engines.yaml` written but never read by the old agent) and a new agent against an rc.3 server (`published=restart_required`), gated on `CAPYCTL_RC3_LOCAL`/`CAPYCTL_RC3_REMOTE` naming existing rc.3 binaries. Only the existing engine venvs named in `hosts.local.env` are used; no venv is created |
| `rows/MN1.sh` … `rows/MN8.sh`, `grouplib.sh`, `gen_group.py`, `rdma_counters.py`, `tests/test_rdma_counters.py` | Multi-node group rows (ADR 0028, design section 17.2; see "Multi-node group rows" below). `gen_group.py` writes the two-rank group fixtures `vg-30`, `sg-30` and `tg-n35` (`topology.tensor_parallel: 2`, `placement.hosts: [HOST_A, HOST_B]`, per-member memory). `grouplib.sh` holds the group checks: the read-only preflight, status and ledger views per member (`ledger.py group`, `ledger.py members`), RDMA transport, and verified cleanup on both hosts. `fault.sh member` signals a member's recorded identity. `rdma_counters.py` reads `/sys/class/infiniband` port counters (and RDMA operation counters where the driver has them) read-only; its tests run with `python3 -m unittest discover -s scripts/live/matrix/tests` |
| `rows/LC1.sh` … `rows/LC6.sh`, `llamacpp_rows.sh`, `llamacpp_probe.py` | ADR 0029 llama.cpp rows for a GB10 host, each in a standalone role with its own state directory while the host's matrix role is down (as ENG2). `scripts/live/llamacpp-build.sh source model` builds llama-server v0.6.0 for sm_121 in the home directory and stages Qwen3-4B Q4_K_M first. LC1 `engine add`; LC2 chat, streams, tool calls, start time and memory (MemAvailable, process RSS, nvidia-smi, the GGUF's page-cache bytes) under the default mmap load and `--load-mode none`; LC3 park refused, stop, start; LC4 llama.cpp and vLLM switching under a lowered managed limit; LC5 hang-up mid-decode and mid-prefill; LC6 per-request figures and the load read under 3 streams (no MTP model). `llamacpp_probe.py` runs on the host from the synced tree. |
| `rows/OT1.sh` … `rows/OT11.sh`, `otel_rows.sh`, `otel_collector.sh`, `templates/otelcol.yaml`, `otel_probe.py`, `otel_spans.py`, `tests/test_otel.py` | ADR 0037 OpenTelemetry rows (see "OpenTelemetry rows" below). `otel_collector.sh` starts the OpenTelemetry Collector release binary the owner placed on the host (`OTELCOL_BIN`) with `templates/otelcol.yaml`: one OTLP/HTTP receiver on loopback and a file exporter into the row's evidence directory on the host; it records the binary's version and SHA-256 and stops it by its recorded identity. `otel_rows.sh` runs the standalone role of every row but OT6 (as the LC rows), registers the approved environments with `engine add` into the row's own `XDG_CONFIG_HOME`, writes short-form deployments and copies the host's evidence directory into `host-<a\|b>/`. `otel_probe.py` runs on the host: requests with or without a caller trace context (sampled, unsampled, six malformed kinds), OT3's sentinels, loads with per-request `x-capyctl-timing`, the tracing options and variable names of engine and role processes, sockets on the collector's ports, the tracing packages of an environment (by files), and stamped log lines. `otel_spans.py` reads the collector's file on the control host: spans per service, keys outside §5's allow-list, each request's span tree against the caller's context, engine spans and their parents, activation traces, span durations beside the timing header, sentinel counts and OT8's cost table. `tests/test_otel.py` checks both helpers' parsing and arithmetic (`python3 -m unittest discover -s scripts/live/matrix/tests`) |

## Rules the harness keeps

- **Secrets.** The API key is read from `server-credentials.json` into `CAPYCTL_API_KEY` only.
  The CLI reads the admin token itself. Invitation files are copied with `scp` and never
  printed. Engine-secret tables and binding payloads are never selected.
- **Faults.** Engine PIDs come from `runtime_bindings.identities_json` for the deployment on
  that host, and a group member's from `group_members.identities_json` for its own host.
  Role PIDs come from the pid files `role_exec.sh` writes. A PID whose start ticks or boot id
  differ is refused.
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
- **A second worktree.** `CAPYCTL_REMOTE_TREE=$REMOTE_HOME/capyctl-soak` syncs, builds and runs from its
  own tree on the hosts, so two worktrees never replace each other's binary.
- **A per-host binary (ADR 0018, row ENG4).** `CAPYCTL_REMOTE_BIN_a` / `CAPYCTL_REMOTE_BIN_b`
  override `RBIN` for host A / host B only (`rbin <host>` in `lib.sh`), so one host can
  run an older or newer agent than the rest of the run. `host_up` and `host_up_systemd` (the
  commands that launch the binary) use it; stopping a role never names it.
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
  `CAPYCTL_DEEP_PARK=off` refuses `deep` deployments, so these fixtures do not apply there.
- The remote login shell must be bash, because `rsh` sends `bash -lc $'…'`.
- `MATRIX_LOCAL_RSH=1` with `CAPYCTL_REMOTE_HOME=<scratch>` runs the "remote" scripts on this
  machine against fake venvs and models. It exists only to rehearse the harness.

## Discrete-GPU rows (DG1-DG7)

`discrete_gpu.sh` runs on the discrete-GPU machine itself, against a standalone role of the
binary under test (`target/release/capyctl` unless `CAPYCTL_BIN` names another). It reads only the
`DGPU_*` values of `hosts.local.env` and keeps its state in `DGPU_STATE` (default
`~/capyctl-dgpu-live`, whose ancestors must be owner-only) and its evidence in
`target/live/dgpu/`.

```bash
M=scripts/live/matrix
$M/discrete_gpu.sh prepare   # capyctl engine add for both environments (idempotent)
$M/discrete_gpu.sh dg1       # vLLM host_backed: A cold, B (A released), A, B, then A parked and woken
$M/discrete_gpu.sh dg2       # the same with residency deep
$M/discrete_gpu.sh dg3       # SGLang, both tiers
$M/discrete_gpu.sh dg4       # A on vLLM, B on SGLang
$M/discrete_gpu.sh dg5       # a derived request larger than the card: exit 4, insufficient_device_memory
$M/discrete_gpu.sh dg6       # the key on DGPU_LISTEN_ADDR, loopback narrowing, upgrade from DGPU_PREVIOUS_BIN
```

The inference listener is loopback or `DGPU_LISTEN_ADDR`, always with the API key; the harness
never serves without the key beyond loopback. SGLang deployments name the Triton attention
backend in their `extra_args` (an SGLang profile carries no host-fixed arguments). On exit the
trap deletes the deployments the run created (with verified cleanup) and stops its role.

## Multi-node group rows (MN1-MN8)

A group here has two tensor-parallel ranks: host A is the head (rank 0, serves the API
through the router) and host B the worker (rank 1), over the direct link. The row
descriptions are in each script's header.

| Row | Engine | What it does |
|---|---|---|
| MN1, MN2, MN3 | vLLM, SGLang, TensorFold | bring-up at TP 2: Ready, 20/20 routed completions, RDMA counters before and after on both hosts (`transport: rdma` or `socket` in `transport.json`), stop with verified cleanup on both hosts |
| MN4 | vLLM, SGLang | five deep park and wake cycles per engine: per-member reservation and MemAvailable on each host, same processes, the wake canary's reference tokens, one `parked` and one `restored` journal entry per cycle (SGLang groups park again, ADR 0028 note of 2026-10-16) |
| MN5, MN6 | each engine | SIGKILL of the worker's (MN5) or head's (MN6) engine from its recorded identity: the Ready plan fails at the killed rank and each host settles its own member; under the fixtures' `recovery: reconcile` the server relaunches the group on a new plan by itself (a start is sent only if it rests failed or stopped); it answers, then stops with verified cleanup on both hosts |
| MN7 | one engine | host B's agent stopped (`roles.sh host-down b`) while Ready: rank 1 `uncertain` and still charged, then settles after `roles.sh host-up b` |
| MN8 | vLLM or SGLang | the group and the single-rank `<v\|s>a-30` on host A alternate with `start --evict`: the evicted group releases on both hosts |

Before MN1, as the owner and not through the harness: `vm.compaction_proactiveness=0`,
unlimited memlock for the service user, read-write `/dev/infiniband/uverbs*` on both
hosts. In `hosts.local.env`: `HOST_A_DIRECT_ADDR`/`HOST_B_DIRECT_ADDR` (each host's
direct-link address, declared as its group peer address by `roles.sh host-doc`), the
engine environments, `TENSORFOLD_VENV_DIR` for MN3 and TensorFold MN5/MN6, and the
checkpoints `GROUP_MODEL_VLLM_SGLANG` and `GROUP_MODEL_TENSORFOLD`, present with the same
files on both hosts. Every row starts with `group_preflight`, which checks read-only that
both hosts are online and declare `engine_groups`, that each host's peer address is its
direct-link address, that the engine's profile has one build fingerprint on both hosts,
and that the checkpoint is on both with the same file list and sizes; it stops the row
before anything is deployed when one is missing, and reports the host tuning.

```bash
M=scripts/live/matrix
$M/sync.sh all && $M/roles.sh up normal
$M/run_row.sh MN1
$M/run_row.sh MN2
$M/run_row.sh MN3
$M/run_row.sh MN4 --tag vllm -- vllm
$M/run_row.sh MN4 --tag sglang -- sglang
$M/run_row.sh MN5 --tag vllm -- vllm          # also sglang, tensorfold
$M/run_row.sh MN6 --tag vllm -- vllm          # also sglang, tensorfold
$M/run_row.sh MN7 --tag vllm -- vllm
$M/run_row.sh MN8 --tag vllm -- vllm
```

## OpenTelemetry rows (OT1-OT11)

ADR 0037's live qualification. The collector is the OpenTelemetry Collector release
binary that the owner placed in the home directory on host A and host B (owner decision
2026-10-27, question 11); no receiver script is part of this tree. Each row starts its own
collector run with `templates/otelcol.yaml` (an OTLP/HTTP receiver on `127.0.0.1:OTELCOL_PORT`,
a file exporter writing `collector-<label>.jsonl`), records `otelcol-binary.txt` (version
and SHA-256, checked against `OTELCOL_SHA256` when it is set) and stops the collector at its
end. On the host, everything a row writes is under `<run root>/otel-evidence/<row>/` and
is copied into the row's evidence as `host-<a|b>/`; the summaries (`spans-<label>.json`,
`sentinels.json`, `cost-*.json`) are written beside it.

Every row but OT6 stops the host's matrix role, runs a standalone role with its own state
and config directories and restarts the matrix role at the end. OT6 uses the matrix roles:
the server's endpoint is a loopback port on the control host (`OT_TUNNEL_PORT`) that an SSH
local forward, started and stopped by the row, carries to host A's collector, so no
collector listens beyond loopback. In `hosts.local.env`: `OTELCOL_BIN`, the approved
environments (`OT_VLLM_VENV_DIR` vLLM 0.32.0, `OT_SGLANG_VENV_DIR` SGLang 0.5.22,
`OT_LLAMACPP_DIR` llama.cpp v0.7.0) and the models (`OT_CHAT_MODEL` Qwen3-4B,
`OT_GGUF_MODEL` its Q4_K_M GGUF, `OT_DECISION_MODEL` Clef-Flash). The rows' headers say
what each records.

| Row | What the script does |
|---|---|
| OT1 | No tracing setting; each engine (and a decision) answers requests with and without a valid traceparent; `ss` on the collector's ports, each engine's tracing options and variable names, the collector's file (expected empty) |
| OT2 | Endpoint on, timing header on; streamed and collected chat with a sampled caller context, a decision, requests without one and with six malformed ones; span trees against the caller's context |
| OT3 | Endpoint and engine tracing on; fresh sentinels in the message, system prompt, tool definition and arguments, `cache_salt`, `X-Request-Id`, an unknown model name, two fields an engine refuses quoting them, and a decision; sentinel counts in the collector's file |
| OT4 | Under a lowered managed limit (default 40GiB): a cold start, a wake after a park, a switch both ways; activation traces and span durations beside `x-capyctl-timing` |
| OT5 | Engine tracing on; vLLM, SGLang and llama.cpp in turn: status, rendered options and variables, engine spans and their parents, `gen_ai.request.id` on SGLang |
| OT6 | Server and host A exporting (host A also with engine tracing); `capyctl.ingress` and engine spans under it, a wake's host step; then host A restarted without an endpoint while the engines keep running |
| OT7 | The collector stopped 20 s into a timed load and restarted after it; every answer, the role log's notices stamped, status's dropped spans, export after the restart |
| OT8 | Per engine, 200 requests per arm: off, sampled, unsampled, engine tracing; on SGLang, direct launches at trace levels 1 and 3 beside one without tracing; cost tables |
| OT9 | Engine tracing on; a `retention: zero` deployment and one without it |
| OT10 | The first environment without the OpenTelemetry packages (approved ones, then `OT_NOSDK_ENV`); its deployment's status and answers |
| OT11 | Start refused for engine tracing with a non-loopback endpoint (standalone and the host role); engine options and `OTEL_*` refused in profiles and deployments |

```bash
M=scripts/live/matrix
$M/run_row.sh OT1 --no-e0 -- a
$M/run_row.sh OT2 --no-e0 -- a                    # chat on vllm (or: -- a sglang)
$M/run_row.sh OT3 --no-e0 -- a
$M/run_row.sh OT4 --no-e0 -- a                    # optional managed limit, default 40GiB
$M/run_row.sh OT5 --no-e0 -- a
$M/run_row.sh OT6 -- a                            # the matrix roles up (roles.sh up)
$M/run_row.sh OT7 --no-e0 -- a                    # optional engine and seconds down (vllm 180)
$M/run_row.sh OT8 --no-e0 --tag vllm -- a vllm    # also sglang, llamacpp
$M/run_row.sh OT9 --no-e0 -- a
$M/run_row.sh OT10 --no-e0 -- a
$M/run_row.sh OT11 --no-e0 -- a
```

The standalone rows take `b` as well (`--tag b -- b`) to run on host B with its collector.

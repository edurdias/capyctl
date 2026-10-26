# Multi-GPU live rows

`run.sh` runs ADR 0035's MG1–MG14 and bandwidth test on one rented x86_64
machine with two identical NVIDIA GPUs. It is a foreground process: launch it
under an independent rental supervisor, not an agent tool's lifetime. It copies
evidence throughout setup and the rows, makes a complete verdict, and exits.
Nothing here touches the lab hosts. Provider deletion belongs to the supervisor,
not to this engine harness.

```bash
scripts/live/multi-gpu/run.sh --dry-run                    # every step, local and remote; contacts nothing
scripts/live/multi-gpu/run.sh --ssh ubuntu@203.0.113.7 --out "$OUT" # foreground; supervisor required
scripts/live/multi-gpu/run.sh --ssh ubuntu@203.0.113.7 --out "$OUT" --only MG1,MG12 --keep
scripts/live/multi-gpu/run.sh --ssh ubuntu@203.0.113.7 --out "$OUT" --skip-setup --only MG8
scripts/live/multi-gpu/run.sh --ssh ubuntu@203.0.113.7 --out "$OUT" --purge-only
```

For an authorized Shadeform instance, the parent launches this wrapper under a
user systemd service (with provider auto-deletion configured independently):

```bash
systemd-run --user --unit="capyctl-mg-$RUN_ID" --collect \
  --working-directory="$PWD" \
  python3 scripts/live/shade_session.py \
  --instance "$INSTANCE_UUID" --out "$OUT" --max-seconds 28800 -- \
  bash scripts/live/multi-gpu/run.sh --ssh "$SSH_TARGET" --out "$OUT" \
  --max-seconds 28200
```

The wrapper records `supervisor.json`, `session.log`, and the provider deletion
receipt. It deletes the rental on success, failure, or deadline; `--keep` does
not override that deletion. A machine or network outage can defeat a local
supervisor, so the provider's spending/deletion cap remains required.

## The machine

What the owner rents (ADR 0035 §14, owner decision 2): x86_64, two RTX A6000
48 GB PCIe cards, a driver whose `nvidia-smi` header reads CUDA 13.0 or higher,
at least 128 GB of RAM (MG6 is SKIP below 120 GiB), 250 GB free in the home
directory, and `git`, `curl`, `python3`, `cc`/`c++`, `make` and `tar`. SSH
access by key. `cmake` and `nvcc` are used when present (`nvcc` at
`/usr/local/cuda/bin/nvcc` or on `PATH`); without them, setup installs the
`cmake` wheel from PyPI and NVIDIA's CUDA 13.0 compiler, runtime and cuBLAS
wheels into the session's directory for the llama.cpp build (built with `c++`
as the CUDA host compiler). The preflight refuses a machine
with fewer than two GPUs, GPUs that differ, an older driver, too little disk, or
an engine already running, before anything is installed.

For MG6, the qualification role explicitly sets system managed and parked
limits to 80%, retaining a 20% free reserve. The 30B TP2 host-backed charge is
about 94.1 GiB; the default 25% parked limit cannot hold it. Admission still
checks available RAM, so the configured budget is not a guarantee of a start.

## What it installs

Only the approved engine versions, fresh x86_64 CUDA builds, in the home
directory (their fingerprints are recorded in `versions.txt`, since they are not
byte copies of the lab hosts' aarch64 environments):

| Engine | Where | How |
|---|---|---|
| vLLM 0.32.0 | `~/vllm-0.32.0-venv` | `uv pip install vllm==0.32.0 --torch-backend=cu130` |
| SGLang 0.5.22 | `~/sglang-0.5.22-venv` | `uv pip install --prerelease=allow sglang==0.5.22` |
| llama.cpp v0.7.0 | `~/llamacpp-v0.7.0` | the tag (commit `620d222`), `llama-server` built with CUDA for the cards' compute capability |

and, under `~/mg`: `uv`, `protoc` 29.3 and Rust (the version
`rust-toolchain.toml` pins) to build CapyCTL there. No system package, driver or
CUDA change. Three CapyCTL binaries are built from the pushed trees:

- `capyctl`: this checkout's commit, as it ships (deep parking at N > 1 off for
  every engine, ADR 0035 §9);
- `capyctl-qualify`: the same commit with `deep_park: true` in vLLM's and
  SGLang's local multi-GPU support data (`local_support` in
  `crates/capyctl-config/src/group_support.rs`), which is exactly the change MG4
  and MG5 make when they pass; the build refuses unless it flips those two
  entries, and `--dry-run` makes the same edit on a copy of this checkout's
  file and fails unless it flips exactly those two;
- `capyctl-old`: the `v0.1.8` tag, the older host of MG11 (no
  `local_tensor_parallel` capability).

Uncommitted product changes are not pushed: commit first. The harness scripts
themselves are copied and their combined SHA-256 is recorded. Setup invalidates
current/qualification binaries when their recorded source commit differs;
`--skip-setup` refuses that mismatch rather than attributing stale binaries to a
new commit. Each attempt gets fresh role state, while approved engine installs
and model downloads can be reused.

MG3 also runs a separate allocation diagnostic after stopping the managed
llama.cpp launch. Version 0.7.0 hides its model-buffer lines at default verbosity.
The diagnostic reuses the live launch's captured arguments and environment,
adding only `--verbosity 4`, sends no requests, and stops the process after it
listens. `MG3-fit-replay.json` identifies both argument lists and the GPU namespace;
`MG3-fit-replay.log` supplies the engine's allocation lines for comparison against
the managed launch's grants. Managed serving, listeners and accounting are still
checked on the ordinary CapyCTL build. Production logging is unchanged.

## Models

- `Qwen/Qwen3-30B-A3B` (BF16, about 61 GB; ADR 0028's bring-up model) and
  `Qwen/Qwen3-4B`, downloaded by CapyCTL from Hugging Face into `~/mg/sources`
  at the commit `main` names when setup runs (recorded in `versions.txt`). The
  first download is the bandwidth test (`BW`): bytes, seconds and MB/s.
- `Qwen/Qwen3-4B-GGUF` `Qwen3-4B-Q4_K_M.gguf` at the revision the laptop rows
  used, for MG3.

## The rows

Each row writes its parts to `results.tsv`. `verdict.md` and `verdict.json`
require every expected part, the supporting artifacts, version fingerprints,
events/journal exports, and engine logs for serving rows. A missing row, skipped
part, malformed record, empty required artifact, or failed part is not a pass.
Fewer than five deep cycles or three host-backed cycles cannot qualify.

| Row | Role | What the script does | Passes when |
|---|---|---|---|
| BW | s1 | deploys the 30B and the 4B model from Hugging Face; times the 30B download | the sources verify; the throughput is recorded |
| MG12 | s1 | TP 3 on two GPUs; a pinned `devices` count other than N | `insufficient_gpus` exit 4; `topology_devices_mismatch` exit 2 |
| MG14 | s1 | the 30B model with `tensor_parallel: 1`; under `auto` with `memory.kv_cache: 40GiB` | `insufficient_device_memory` naming `tensor_parallel: 2`; `insufficient_device_memory` naming the per-GPU charge |
| MG1 | s1 | vLLM, 30B, TP 2, `restart_only`: start, one request through the router | Ready and answering; each GPU's ledger charge covers the launch's bytes on that card and both cards hold part; no recorded process listens off loopback (by `/proc` and by `ss -ltnp`); status shows `collectives: loopback` and `tensor_parallel: 2`; a startup peak is recorded |
| MG7 | s1 | per engine (vLLM, SGLang): a normal stop, then SIGKILL to rank 1 | no new `/dev/shm` name after the stop; after the kill the instance is `failed`, `engine_rank_exited` is recorded, and the charge is released only after every recorded process is gone (`/dev/shm` after the kill is recorded) |
| MG8 | s1 | per engine: SIGSTOP to rank 1, then a streamed request | `tp_stalled` within the stall timeout (`--stall`, 30 s) plus the probe; the instance stops and every process goes |
| MG9 | s1 | per engine: C1, C4, C8 × 256 tokens × 3 rounds through the router, then the same build launched by hand with the arguments the engine logged under CapyCTL and the loopback environment | the router's tokens per second within 10 % of the hand launch at every concurrency |
| MG2 | s1 | SGLang, as MG1 | as MG1 |
| MG3 | s1 | llama.cpp, the GGUF model, `tensor_parallel: 2` (layer split) | as MG1; real, positive CUDA0 and CUDA1 model-buffer log lines each fit the corresponding grant (`MG3-fit.txt` beside `MG3-charge.json`); GPU readings never substitute for missing log lines |
| MG4 | s2 | vLLM TP 2 `deep`: 5 park and wake cycles | every park reaches `parked` with both GPUs' bytes down; every wake answers the canary request with both GPUs' bytes back |
| MG5 | s2 | SGLang TP 2 `deep`: 5 cycles | as MG4 |
| MG6 | s2 | vLLM and SGLang TP 2 `host_backed`: 3 cycles each | as MG4, and nothing of the deployment left in the ledger after each stop |
| MG10 | s2 | the 30B model (SGLang, TP 2, `deep`) parked; the 4B model on gpu0 with a 30 GiB KV cache; a request to the 30B model | the request answers, the 4B model is evicted, and the switch's events name gpu0 and gpu1 |
| MG13 | s2 | `topology: auto`: the 4B model on vLLM (against an explicit single-GPU deployment), then the 30B model on SGLang with the 4B model serving, then park/wake | N = 1 and `TP auto (1)` with equal nonempty arguments; N = 2 and `TP auto (2)`, arguments equal MG5's captured explicit TP 2 deep baseline with identical residency, and the 4B model evicted; the wake preserves N and devices and answers the canary |
| MG11 | srv | a control-plane server, a host of this build and an older host (`v0.1.8`), all on loopback on the machine; MG1's deployment on each | Full MG1 serving evidence on the new host: Ready/chat, owned per-GPU use within reservations, loopback-only listeners and TP 2/loopback collectives status, measured startup peaks; on the older host refused `host_capability_missing:local_tensor_parallel` with no engine started |

Role `s1` runs `capyctl`, role `s2` `capyctl-qualify`; each role has its own
state and config directories under `~/mg`, listens on `127.0.0.1:--port` and
runs one deployment at a time. Rank processes are found in the role's ledger
(the identities each launch recorded, ADR 0035 §6), never by name, and are
signalled only when their PID, start ticks and boot id still match.

MG11 uses the same serving checks as MG1, including when selected alone. Its
commands use the server configuration and its ledger is `state/srv/srv.sqlite3`.
`MG11-owned.jsonl` retains the coordinator's recorded runtime-binding identities;
the enrolled host is this same machine, so the collector can inspect their PIDs
with `/proc`, `ss`, and NVIDIA tooling. Missing identities do not fall back to
other GPU processes and cannot pass charge/listener checks. Remote identity or
startup-measurement publication failures remain qualification failures, not
permission to infer evidence. The strict collector requires the MG11 status,
GPU use, charge, listeners, peaks, identities and deployment exports
alongside chat and refusal outputs. The shared start helper now writes
`start-mg11-new.out` (formerly `mg11-new-start.out`); `chat-mg11.json` is unchanged.
Startup peaks must include positive measured bytes at TP 2 for `system` and a
representative GPU domain, with measured host/device provenance. ADR0035 §3
stores the largest selected GPU peak once and applies it to every identical GPU;
it does not require a measurement record for each GPU. The legacy reservation-table
export is diagnostic only; `MG11-charge.json` captures the authoritative
resource-owner charges.

MG4 and MG5 decide whether each engine's `deep_park` is set in the shipped
support data (owner decision 3); an engine whose row fails keeps `restart_only`
at N > 1. MG1–MG3, MG6–MG9 and MG13 must pass before the release notes say the
feature is qualified.

## Time

On the machine above, about: setup 45 minutes (three CapyCTL builds, two
engine installs, the llama.cpp build), the 30B download at the machine's
bandwidth (61 GB; 10 minutes at 1 Gbit/s), then the rows about 2.5 hours, most
of it engine starts of the 30B model. `--only` with `--keep` and later
`--skip-setup` can split a session while the same supervised rental remains
authorized. An MG13-only attempt also runs MG5 to capture its explicit deep baseline.
`--purge-only` copies existing exported evidence before removing the session.

## Output and cleanup

The session directory defaults to
`<main checkout>/target/live/multi-gpu/<unique-attempt>/`; use the same `--out`
for related reruns. It contains:

- `completion.json`: atomically changes from `state: running` to `state:
  complete` only after final collection and the purge decision. `exit_code`,
  `host_exit`, `final_collection`, `purged`, and `reason` are machine-readable.
  A completed process is not necessarily a successful qualification.
- `verdict.md` and `verdict.json`: all MG1–MG14 plus BW, even for `--only`.
- `attempts/<unique-id>/`: immutable attempt identity and source/settings,
  preflight, transport/purge diagnostics, its verdict, and `raw/` containing
  setup output, progress, results, versions, deployment files, per-start
  output/status/events, per-cycle readings/status, engine logs (including
  rotations), and read-only event and journal exports.

Results are exported on each row part and copied locally every 15 seconds
(`--poll-seconds`), starting before setup. Export uses a text allowlist:
credentials, private state databases, join files and raw process argv are
never copied. Credentials in logs are redacted before transfer; host addresses
and home paths are also removed. Copies use atomic replacement and fsync;
failed copies never delete the previous local evidence.

Reruns append attempts rather than overwriting earlier failures. The latest
selected attempt owns each row, including rows it failed to produce. Combining
rows requires the same source commit, harness hash, qualification settings, and
version/model fingerprints. An earlier pass cannot conceal a missing or failed
rerun, and a different commit starts a new qualification. A bounded `--only`
attempt still exits nonzero until the complete compatible all-row verdict passes.

SSH calls are bounded at 25 seconds, copies at 30 seconds. A lost connection
gets at most 300 seconds (`--disconnect-seconds`); a vanished remote runner is
detected from its PID/start-time identity. The whole run is bounded at eight
hours (`--max-seconds`), including a remote timeout. Final collection gets at
most three attempts, or one after SIGTERM/SIGINT so the supervisor's 120-second
termination grace can accommodate collection. A missing host completion
marker or failed final copy is a nonzero exit, never an idle polling loop.

Purge runs only after a successful final copy and a host completion marker.
It stops session-owned roles and removes the paths `~/mg/created.txt` records:
the session directory, approved engine installs, and caches this session
created. It refuses to purge while engines remain. `--keep` preserves the
remote session, but the provider supervisor still owns the rental's lifetime.
On a failed copy the harness leaves remote evidence intact; the supervisor's
deadline can nevertheless delete the rental, so the incremental local copies
are the recovery record.

## Local regression scenarios

These tests use local transport fixtures, no SSH host, provider, or engine.
Their scratch data lives under `~/.cache/capyctl-agent-tmp/mg-tests`.
They exercise the real foreground runner:

```bash
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover \
  -s scripts/live/multi-gpu -p 'test_*.py'
bash -n scripts/live/multi-gpu/run.sh scripts/live/multi-gpu/remote.sh
scripts/live/multi-gpu/run.sh --dry-run
```

- **Connection interruption:** a snapshot arrives, then SSH fails. The runner
  stops at the reconnect bound, retains the prior local results, does not purge,
  and writes a nonzero completion.
- **Failing row:** the remote process exits zero but MG5 reports failure.
  Collection and purge complete; the runner still exits nonzero.
- **Missing evidence:** all reported row parts pass but MG3's buffer log is
  absent. The complete verdict and process exit fail. A separate fixture makes
  final rsync fail and proves purge is not called.
- **Successful completion:** every part and required artifact is present.
  Final collection precedes purge, completion reports zero, and the runner exits.

Helper regressions cover skip/failure masking, missing parts, malformed records,
JSON server credentials for MG11, typed vLLM argv comparison without persisting
keys, strict CUDA buffer/grant checks, log redaction, and failed rerun precedence.
CPU fixtures establish harness behavior only, never native-engine qualification.

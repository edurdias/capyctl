# Multi-GPU live rows

`run.sh` runs the live rows of ADR 0035 (one model across several GPUs of one
host, MG1–MG14) and a download bandwidth test on one rented x86_64 machine with
two identical NVIDIA GPUs, in one session, and removes everything it created
there at the end, so the machine can be shut down straight afterwards. Nothing
here touches the lab hosts.

```bash
scripts/live/multi-gpu/run.sh --dry-run                    # every step, local and remote; contacts nothing
scripts/live/multi-gpu/run.sh --ssh ubuntu@203.0.113.7     # the whole session
scripts/live/multi-gpu/run.sh --ssh ubuntu@203.0.113.7 --only MG1,MG12 --keep
scripts/live/multi-gpu/run.sh --ssh ubuntu@203.0.113.7 --skip-setup --only MG8
scripts/live/multi-gpu/run.sh --ssh ubuntu@203.0.113.7 --purge-only
```

## The machine

What the owner rents (ADR 0035 §14, owner decision 2): x86_64, two RTX A6000
48 GB PCIe cards, a driver whose `nvidia-smi` header reads CUDA 13.0 or higher,
at least 128 GB of RAM (MG6 is SKIP below 120 GiB), 250 GB free in the home
directory, and `git`, `curl`, `python3`, `cc`/`c++`, `make` and `tar`. SSH
access by key. `cmake` and `nvcc` are used when present (`nvcc` at
`/usr/local/cuda/bin/nvcc` or on `PATH`); without them, setup installs the
`cmake` wheel from PyPI and NVIDIA's CUDA 13.0 compiler wheel into the
session's directory for the llama.cpp build. The preflight refuses a machine
with fewer than two GPUs, GPUs that differ, an older driver, too little disk, or
an engine already running, before anything is installed.

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

Uncommitted changes are not pushed: commit first.

## Models

- `Qwen/Qwen3-30B-A3B` (BF16, about 61 GB; ADR 0028's bring-up model) and
  `Qwen/Qwen3-4B`, downloaded by CapyCTL from Hugging Face into `~/mg/sources`
  at the commit `main` names when setup runs (recorded in `versions.txt`). The
  first download is the bandwidth test (`BW`): bytes, seconds and MB/s.
- `Qwen/Qwen3-4B-GGUF` `Qwen3-4B-Q4_K_M.gguf` at the revision the laptop rows
  used, for MG3.

## The rows

Each row writes one or more lines to `results.tsv` (row, part, pass, fail or
skip, evidence); `verdict.md` tabulates them.

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
| MG3 | s1 | llama.cpp, the GGUF model, `tensor_parallel: 2` (layer split) | as MG1; llama.cpp's log shows model buffers on CUDA0 and CUDA1 (`MG3-fit.txt` beside `MG3-charge.json`) |
| MG4 | s2 | vLLM TP 2 `deep`: 5 park and wake cycles | every park reaches `parked` with both GPUs' bytes down; every wake answers the canary request with both GPUs' bytes back |
| MG5 | s2 | SGLang TP 2 `deep`: 5 cycles | as MG4 |
| MG6 | s2 | vLLM and SGLang TP 2 `host_backed`: 3 cycles each | as MG4, and nothing of the deployment left in the ledger after each stop |
| MG10 | s2 | the 30B model (SGLang, TP 2, `deep`) parked; the 4B model on gpu0 with a 30 GiB KV cache; a request to the 30B model | the request answers, the 4B model is evicted, and the switch's events name gpu0 and gpu1 |
| MG13 | s2 | `topology: auto`: the 4B model on vLLM (against a pinned single-GPU deployment of it), then the 30B model on SGLang with the 4B model serving, then the 30B model parked and woken | N = 1 and `TP auto (1)` with the same vLLM arguments as the pinned deployment; N = 2, `TP auto (2)`, the same SGLang arguments as MG9's launch, and the 4B model evicted; the wake keeps N = 2 on the same GPUs |
| MG11 | srv | a control-plane server, a host of this build and an older host (`v0.1.8`), all on loopback on the machine; MG1's deployment on each | Ready and answering through the server on the new host; on the older host refused `host_capability_missing:local_tensor_parallel` with no engine started |

Role `s1` runs `capyctl`, role `s2` `capyctl-qualify`; each role has its own
state and config directories under `~/mg`, listens on `127.0.0.1:--port` and
runs one deployment at a time. Rank processes are found in the role's ledger
(the identities each launch recorded, ADR 0035 §6), never by name, and are
signalled only when their PID, start ticks and boot id still match.

MG4 and MG5 decide whether each engine's `deep_park` is set in the shipped
support data (owner decision 3); an engine whose row fails keeps `restart_only`
at N > 1. MG1–MG3, MG6–MG9 and MG13 must pass before the release notes say the
feature is qualified.

## Time

On the machine above, about: setup 45 minutes (three CapyCTL builds, two
engine installs, the llama.cpp build), the 30B download at the machine's
bandwidth (61 GB; 10 minutes at 1 Gbit/s), then the rows about 2.5 hours, most
of it engine starts of the 30B model. `--only` with `--keep` and later
`--skip-setup` splits the session; `--purge-only` cleans up at the end.

## Output and cleanup

Results go to `target/live/multi-gpu/<run>/` in the main checkout: `verdict.md`,
`settings.json`, `preflight.txt` and `raw/` (progress log, `results.tsv`,
`versions.txt`, status and ledger reads, engine logs, deployment files). The
machine's address and home directory are replaced in everything copied back.

The host phases run detached (`setsid`), so a dropped connection does not stop
them; rerun with `--skip-setup` to follow up. At the end `remote.sh purge`
stops every role the session started and removes what `~/mg/created.txt`
lists: `~/mg` (sources, builds, Rust, state), the three engine directories, and
the caches under `~` the session created (uv, vLLM, FlashInfer, Triton, ...).
Nothing that existed before the session is removed.

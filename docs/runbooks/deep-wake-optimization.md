# Deep-wake loader qualification — host-a

Date: 2026-09-12. Host: **host-a only**. The owner reserved host-b before
implementation trials; no eager-loader qualification is claimed for host-b.

**Confirmed:** median wake-to-response fell from 57.025s to 7.505s (86.84%).
A fresh original-loader reversion measured 51.395s before the final eager run,
giving an independently reconfirmed reduction of 43.890s (85.40%).

## Recipe and change

Qwen/Qwen3-4B-Instruct-2507 BF16, 7.49 GiB across three safetensors shards,
vLLM 0.29.0, Python 3.12.14, PyTorch 2.13.0+cu130, unified-memory host unified memory,
local EXT4 checkpoint storage. Preserve 16 GiB KV, 4096-token maximum context,
0.10 GPU utilization gate, and localhost port 8150.

The opt-in deep-park lab profile adds:

```text
--safetensors-load-strategy eager
```

Stock/denied profiles keep their prior loader settings. Level-2 park still
discards weights and KV; no CPU weight backup is retained as a wake strategy.
The restore sequence remains wake allocations, reload weights, readiness,
then authenticated routed inference. Policy gates and timeouts are unchanged.

## Measurements

Each row is a fresh engine process with three immediate park/wake cycles.
Latency runs from controller Start request to a fully read and validated,
non-streaming capyctl HTTP completion. This is not request-to-first-token.
The prompt is `Say 'live' and nothing else.`, with `max_tokens: 8`.

| Run | Wake → response samples (s) | Median (s) | Reload samples (s) |
|---|---|---:|---|
| Original baseline | 57.279, 56.384, 57.025 | 57.025 | 56.21, 55.35, 56.00 |
| Eager trial | 8.454, 8.410, 8.618 | 8.454 | 7.41, 7.38, 7.59 |
| Baseline reconfirmation | 54.356, 51.395, 47.221 | 51.395 | 53.40, 50.46, 46.29 |
| Eager reconfirmation | 7.652, 7.127, 7.505 | 7.505 | 6.63, 6.12, 6.49 |

Both eager runs passed all three authenticated routed completions, each returning
`live`, plus stale-generation rejection. Fresh reference and candidate runs
used unchanged timing/test and monitoring code; only loader selection changed.

The first trial reduced the median by 48.571s (85.18%). The pre-trial estimate
was a 40–50s reduction, based on the copy probe below. The original-to-final
reduction of 49.520s supports that estimate. The original baseline runtime is `acbb14c`; timing
instrumentation is `06409bd`. Candidate `roles.rs` SHA-256 is
`99c9f4dc258b561cf8dbc1ce3f2fc9ffdd4104a14200820681442484419e8afa`.

## Memory tradeoff and limits

Host memory was sampled every 250ms, including initialization and brief test
compilation. Peak host use here means MemTotal minus minimum MemAvailable,
not an exact allocation high-water mark. Do not sum process RSS and page cache.

| Run | Minimum available (GiB) | Peak host use (GiB) | Engine-reported park release (GiB) | Swap |
|---|---:|---:|---|---|
| Original baseline | 85.98 | 35.71 | 24.54, 24.78, 24.84 | 0 |
| Eager trial | 77.94 | 43.75 | 24.41, 25.39, 25.39 | 0 |
| Baseline reconfirmation | 85.95 | 35.74 | 24.58, 24.79, 24.83 | 0 |
| Eager reconfirmation | 78.63 | 43.06 | 24.41, 25.43, 25.37 | 0 |

Eager reads whole checkpoint shards into ordinary CPU memory before loading.
The measured temporary headroom cost is about 8 GiB. This is safe for this
isolated lab recipe, not evidence that arbitrary model sizes or overlapping
loads fit. Dynamic admission remains separate work; never resurrect the
superseded 64 GiB KV recipe. The experiment gates require no swap, at least
16 GiB available, and at least 23 GiB engine-reported release.

Sampled available memory returned to roughly 111–112 GiB around the eager
trial's park transitions, comparable to the original loader. Extra loading
memory is temporary; it is not used as a retained CPU weight backup.

These are cache-warm, immediate wakes: initialization has already read the
checkpoint. No cache drop or competing-model pressure was applied. Tail
latency, throughput, TTFT, memory-pressure returns, and NVMe-cold reloads are
not qualified by these small samples.

## Why this configuration was tested

Process counters showed zero storage `read_bytes` and zero major faults
through the sampled baseline reload windows. cProfile then attributed
55.485s of a roughly 57s reload to `torch.Tensor.copy_`; safetensors iteration
took 0.013s. Layer setup was not the dominant cost.

A representative 777,912,320-byte BF16 embedding's first mmap → CUDA copy
took 4.954s; repeated copies took 0.013s. First copies from an ordinary CPU
clone or eager-loaded tensor also took 0.013s. Reading/deserializing the eager
tensor's entire shard took 2.579s. This points to first GPU access to fresh
file-backed mappings, not sustained transfer bandwidth. The exact driver
mechanism was not established.

Nsight captured only startup, ending CUDA activity at about 59s, before reloads.
Its memcpy totals must not be presented as reload attribution. The successful
reload attribution used a temporary additive worker-extension RPC and cProfile;
no installed vLLM files or dependencies were changed.

## Reproduction and evidence

Use the environment in [the environment runbook](vllm-env.md). Run one
engine per host and confirm the prior process has exited. On host-a:

```bash
CAPYCTL_LIVE=1 \
CAPYCTL_VLLM_BIN=$HOME/capyctl-vllm-venv2/bin/vllm \
CAPYCTL_MODEL_PATH=$HOME/models/qwen3-4b-instruct \
CAPYCTL_MODEL_ID=qwen3-4b-instruct CAPYCTL_PORT=8150 \
CAPYCTL_ENGINE_PATH=$HOME/capyctl-vllm-venv2/bin \
cargo test --release -p capyctl-cli --test live_spark live_park_reload -- --exact --nocapture
```

`LIVE-METRIC` records park, wake-to-ready and wake-to-full-response seconds.
The measurement run staged the same frozen test as `live_wake_bench.rs` on
host-a, preserving the host's pre-existing test file. A separate monitor sampled
memory and process counters; the test command alone does not capture those.

Raw baseline: `<temporary-directory>` on host-a.
Raw eager trial: `<temporary-directory>` on host-a.
Raw baseline reconfirmation: `<temporary-directory>` on host-a.
Raw eager reconfirmation: `<temporary-directory>` on host-a.
Reload profile: `<temporary-directory>` on host-a.
The detailed log and scripts were kept locally (gitignored) for audit
only. This tracked report preserves the principal evidence.

Workspace tests and all-targets clippy passed. Two unit tests verify the
opt-in eager flags and the unchanged denied profile. Independent code review
reported no correctness or regression findings. Live results and final
acceptance are backed by the confirmation above.

One implementation variant was tested and retained. Forced prefetch was not
benchmarked: it retains the mmap source backing implicated by the copy probe,
and the baseline was already OS-cache-hot. No further supported configuration
candidate had a measured basis for beating eager within this scope, so the
run stopped without spending the four-variant allowance. No new dependencies
or paid judge evaluations were used. One confirmation launch was rejected
before engine spawn by the conservative port probe during TCP TIME_WAIT;
the unchanged harness passed after normal socket expiry. That attempt produced
no latency sample and was not counted as a failed reload.

The remaining engine-reported reload stage is about 6–8s; its internal cost
breakdown has not been reprofiled. Further gains and pressure/cold-cache
behavior remain unmeasured, not promised follow-up performance.

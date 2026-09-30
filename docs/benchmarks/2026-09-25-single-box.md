# Single-box benchmark through capyctl — 2026-09-25

Owner-approved experiment (2026-09-25). Five models, each on one GB10 host at a
time, single user, 256 prompt tokens and 256 generated tokens, on vLLM and on
SGLang, deployed by capyctl and requested through the capyctl router. Each engine runs
a baseline (no speculation) and the best drafter it supports.

**This is not qualification.** It measures one request shape on one host class.
Passing CPU or Fake-engine tests say nothing about these numbers, and these
numbers say nothing about recipes, shapes or hosts not listed here.

## Results

Decode rate in tokens per second, stream-end based, median of five requests
(256 in, 256 out, one user). "Best" is the fastest speculative run for that
engine. The post's figure is the single-user number in the post that prompted
this run; it used EXL3 builds and drafters for some models, so it is context,
not a like-for-like target.

| Model | vLLM baseline | vLLM best drafter | SGLang baseline | SGLang best drafter | Post |
|---|---:|---:|---:|---:|---:|
| MiniCPM5-2B BF16 | 36.1 | 85.6 (DSpark) | 36.8 | 84.1 (DSpark) | 100.8 |
| Qwen3.6-35B-A3B NVFP4 | 76.5 | 126.2 (DFlash) | 84.6 | 124.8 (MTP) | 89.7 |
| Ling-3.0-flash int4 | not run (`trust_remote_code`) | not run | 23.0 | 48.4 (DSpark) | 69.5 |
| Gemma-4-E2B-it BF16 | 38.3 | 96.2 (assistant) | 39.0 | 99.6 (assistant) | 39.2 |
| Qwen3.8-27B NVFP4 | 10.4 | 27.6 (DFlash2) | 10.7 | 27.1 (DFlash2) | 33.4 |

Every run, in full. TTFT and TTLT are measured at the client through the router;
rates at p10 are the slow tail. capyctl overhead is the client mean minus the
engine's own mean for the same requests (Method). Memory is the drop in host
`MemAvailable` from before deploy to Ready. Startup is `deploy --activate
--wait`.

| Model | Engine | Run | TTFT ms p50 / p90 | TTLT s p50 / p90 | Prefill tok/s p50 / p10 | Decode tok/s p50 / p10 | capyctl overhead ms TTFT / e2e | Memory GiB | Startup s | Post tok/s |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| MiniCPM5-2B BF16 | vLLM | baseline | 79 / 135 | 7.13 / 7.17 | 3258 / 1901 | **36.1** / 36.1 | 60 / 47 | 16.5 | 57 | 100.8 |
|  | vLLM | DSpark | 71 / 85 | 3.07 / 4.08 | 3623 / 3043 | **85.6** / 63.8 | 37 / 47 | 17.7 | 69 |  |
|  | SGLang | baseline | 111 / 133 | 7.03 / 7.05 | 2317 / 1942 | **36.8** / 36.8 | 80 / 46 | 21.6 | 60 |  |
|  | SGLang | DSpark | 103 / 114 | 3.14 / 4.11 | 2489 / 2255 | **84.1** / 64.2 | 64 / 41 | 25.0 | 61 |  |
| Qwen3.6-35B-A3B NVFP4 | vLLM | baseline | 128 / 181 | 3.47 / 3.49 | 2051 / 1522 | **76.5** / 76.1 | 53 / 46 | 32.4 | 226 | 89.7 |
|  | vLLM | MTP | 118 / 125 | 2.20 / 2.29 | 2224 / 2103 | **122.9** / 117.3 | 28 / 30 | 35.1 | 267 |  |
|  | vLLM | DFlash | 132 / 150 | 2.15 / 2.25 | 1987 / 1758 | **126.2** / 121.6 | 37 / 34 | 34.1 | 228 |  |
|  | SGLang | baseline | 175 / 178 | 3.19 / 3.19 | 1507 / 1478 | **84.6** / 83.9 | 56 / 28 | 48.1 | 126 |  |
|  | SGLang | MTP | 200 / 210 | 2.26 / 2.39 | 1316 / 1252 | **124.8** / 115.4 | 70 / 36 | 50.7 | 264 |  |
|  | SGLang | DFlash | 199 / 215 | 2.43 / 2.54 | 1324 / 1225 | **114.5** / 108.9 | 85 / 48 | 51.0 | 120 |  |
| Ling-3.0-flash int4 | vLLM | baseline, MTP | not run: the checkpoint's config needs `trust_remote_code` (see Failures) | | | | | | | 69.5 |
|  | SGLang | baseline | 435 / 476 | 11.52 / 11.58 | 607 / 556 | **23.0** / 22.9 | 58 / 33 | 87.4 | 241 |  |
|  | SGLang | MTP | 432 / 453 | 7.00 / 7.12 | 611 / 583 | **39.0** / 38.0 | 66 / 56 | 88.3 | 241 |  |
|  | SGLang | DSpark | 410 / 434 | 5.70 / 6.34 | 645 / 608 | **48.4** / 43.1 | 57 / 24 | 89.3 | 197 |  |
| Gemma-4-E2B-it BF16 | vLLM | baseline | 74 / 110 | 6.75 / 6.78 | 3541 / 2461 | **38.3** / 38.2 | 45 / 47 | 24.3 | 173 | 39.2 |
|  | vLLM | assistant | 80 / 98 | 2.75 / 3.02 | 3282 / 2694 | **96.2** / 86.7 | 45 / 38 | 24.5 | 185 |  |
|  | SGLang | baseline | 114 / 146 | 6.65 / 6.67 | 2316 / 1804 | **39.0** / 38.9 | 75 / 54 | 30.1 | 81 |  |
|  | SGLang | assistant | 188 / 222 | 2.75 / 2.92 | 1398 / 1202 | **99.6** / 90.5 | 121 / 87 | 28.5 | 77 |  |
| Qwen3.8-27B NVFP4 | vLLM | baseline | 223 / 243 | 24.65 / 24.69 | 1227 / 1123 | **10.4** / 10.4 | 37 / 42 | 35.2 | 228 | 33.4 |
|  | vLLM | MTP | 387 / 411 | 14.58 / 15.03 | 706 / 664 | **17.8** / 17.4 | 37 / 38 | 37.4 | 277 |  |
|  | vLLM | DFlash2 | 365 / 367 | 9.59 / 9.97 | 748 / 744 | **27.6** / 26.6 | 26 / 41 | 41.5 | 462 |  |
|  | SGLang | baseline | 288 / 300 | 24.05 / 24.05 | 948 / 911 | **10.7** / 10.7 | 68 / 34 | 39.1 | 222 |  |
|  | SGLang | MTP | 360 / 383 | 14.48 / 15.81 | 759 / 713 | **18.1** / 16.6 | 74 / 37 | 40.9 | 238 |  |
|  | SGLang | DFlash2 | 321 / 345 | 9.75 / 9.89 | 850 / 792 | **27.1** / 26.5 | 71 / 36 | 49.3 | 427 |  |

Readings:

- Speculation is where the gains are: 2.3× (MiniCPM5 DSpark), 1.5–1.6×
  (Qwen3.6), 2.1× (Ling DSpark on SGLang), 2.5× (Gemma assistant) and 2.6×
  (Qwen3.8-27B DFlash2). Tokens per stream chunk (in `table.json`) track the
  acceptance: 6.1 for MiniCPM5 DSpark, about 4.9 for Qwen3.6 DFlash, 3.5 for
  DFlash2 on the 27B.
- vLLM and SGLang land within a few percent of each other on decode for every
  model both could serve. vLLM prefills faster at this size (lower TTFT);
  SGLang's Qwen3.6 baseline is faster on decode (84.6 against 76.5) with the
  Marlin MoE runner.
- The capyctl path adds about 25–85 ms at first token and 25–55 ms at stream end
  (one outlier: 87 ms for SGLang's Gemma assistant run). Most of it is the
  router-to-host hop over Tailscale (about 15 ms per request, TCP connect floor
  14.5 ms) plus the router's own 3–4 ms; the rest is SSE relay and the gap
  between the engine's histogram boundaries and what the client sees. At these
  lengths that is 1–4% of TTLT.
- Where this run sits below the post: MiniCPM5 (85 against 100.8) and the 27B
  (27.6 against 33.4) used other formats or builds there; Ling's 69.5 was not
  reached with the stock SGLang (48.4 with DSpark) and vLLM could not load the
  checkpoint here. Where it sits above: Qwen3.6 with MTP or DFlash (123–126
  against 89.7). Gemma's 39.2 matches the baseline, not the assistant run.

## Method

- **Hosts.** Two identical GB10 hosts (128 GB unified memory, aarch64, sm_121,
  driver 580.173.02, CUDA 13.0, kernel 6.17). vLLM ran on host A and SGLang on
  host B, in parallel, one deployment per host at a time. The control-plane host
  ran the capyctl server, the router and the load generator.
- **Engines.** Existing environments, unchanged: vLLM 0.29.0 (torch 2.13.0,
  FlashInfer 0.6.18, transformers 5.17.0) on host A; SGLang 0.5.20 (torch
  2.13.0, FlashInfer 0.6.18, sglang-kernel 0.4.7, transformers 5.12.1) on host B.
  No custom engine build was needed (see "Custom builds").
- **capyctl.** `main` at `a1298c4` plus the fixes on this branch (see "Product
  fixes"); the branch merged later `main` (`be780b9`) after the run, and the
  live binaries predate that merge. Built from a snapshot of the worktree and run as a server plus two
  enrolled hosts by the live matrix harness (`scripts/live/matrix`). The server
  had `observability.timing_header` on. Host B's role ran with
  `--debug-engine-logs` from 14:14 UTC on, to diagnose one launch failure.
- **Deployments.** One deployment per model and variant, from
  `scripts/live/matrix/models.json` (keys `mc2`, `q36`, `ling`, `g2`, `27b`; a
  trailing `m` is built-in MTP, `d` the external drafter). All use
  `residency: restart_only`, CUDA graphs on, context 16384,
  `max_concurrent_requests` 4, and a declared memory request (planning values,
  listed per run in the evidence). Speculative options go through
  `engine_config.extra_args` with `accept_extra_args: true`; the benchmark host
  documents approve `--speculative-config` (vLLM) and
  `--speculative-draft-model-path` (SGLang) with the model store as the approved
  directory.
- **Measurement.** Row M80 (`scripts/live/matrix/rows/M80.sh`, `bench.py`),
  bench phase only: `M80_PROMPTS=256 M80_CONC=1 M80_REPEATS=5 M80_WARMUP=1
  M80_MAX_TOKENS=256`. Each run deploys `<fixture>-bn` with `--activate --wait`
  (the startup time), sends one warmup request (excluded; it calibrates the
  synthetic prompt to 256 tokens and confirms `ignore_eos`), then five measured
  streaming chat completions, one at a time, `temperature 0`, `ignore_eos`, 256
  output tokens. The prompt is a synthetic word list with a "count upward"
  instruction; the chat template adds 1 to 17 tokens, so the prompt column shows
  what the engine counted.
- **Metrics.** TTFT: client send to first content chunk. TTLT: client send to the
  end of the stream (`[DONE]`). Prefill: prompt tokens / TTFT. Decode:
  (completion tokens − 1) / (TTLT − TTFT). Median and p90 over the five requests;
  for rates the low tail (p10) is shown, since that is the slow side. Memory used:
  host `MemAvailable` before deploy minus at Ready (weights, KV cache, CUDA graphs
  and runtime; page cache is excluded because it counts as available). Startup:
  wall time of `deploy --activate --wait`, including checkpoint digest
  measurement, engine start, weight load, compile or graph capture and
  readiness.
- **capyctl overhead.** Client mean minus the engine's own mean from the same
  requests: SGLang's `/metrics` histograms (scraped on loopback before and after
  the cell), and for vLLM (which keys `/metrics`) the engine histograms the host
  agent forwards in the capyctl latency view. "TTFT" compares time to first token;
  "e2e" compares client stream end with the engine's end-to-end latency. The
  latency view splits the path: client to router about 1 ms, router pre-forward
  about 3 to 4 ms, router to host ingress about 15 ms (Tailscale; TCP connect
  floor 14.5 ms), ingress to engine about 2 to 3 ms.
- **Stream-end correction.** With `ignore_eos`, tokens generated after the
  model's own end of sequence often detokenize to no text. `bench.py` originally
  timed TTLT and decode from the last *text* chunk, which for MiniCPM5 came at
  3.6 s of a 7.1 s stream and nearly doubled the apparent decode rate. The table
  uses the stream end for every run (recomputed from `records.jsonl`), and
  `bench.py` now records it (`e2e_s`, `decode_e2e_tps`).

## Checkpoints and flags

Draft paths are `<store>/sources/huggingface/<owner>--<name>@<sha>`; `<store>` is
the host's model store. Every run also sets the typed fields above.

| Model (checkpoint, commit) | Variant | vLLM 0.29.0 | SGLang 0.5.20 |
|---|---|---|---|
| `openbmb/MiniCPM5-2B` BF16 (`12a3808a`) | base | — | — |
| | DSpark `openbmb/MiniCPM5-2B-DSpark` (`114a20fd`) | `--speculative-config {"method":"dspark","model":<draft>,"num_speculative_tokens":7}` | `--speculative-algorithm DSPARK --speculative-draft-model-path <draft> --speculative-dspark-block-size 7` |
| `nvidia/Qwen3.6-35B-A3B-NVFP4` (`1355db6a`), `language_model_only` on vLLM | base | — | `--moe-runner-backend marlin` |
| | MTP (built in) | `{"method":"mtp","num_speculative_tokens":3,"moe_backend":"triton"}` | `--moe-runner-backend marlin --speculative-moe-runner-backend triton --speculative-algorithm NEXTN --speculative-num-steps 3 --speculative-eagle-topk 1 --speculative-num-draft-tokens 4` |
| | DFlash `z-lab/Qwen3.6-35B-A3B-DFlash` (`f181eece`) | `{"method":"dflash","model":<draft>,"num_speculative_tokens":15}` | `--moe-runner-backend marlin --speculative-algorithm DFLASH --speculative-draft-model-path <draft>` |
| `inclusionAI/Ling-3.0-flash-int4` W4A16 (`7a27e9eb`) | base | — | — |
| | MTP (built in) | `{"method":"mtp","num_speculative_tokens":1}` | `--speculative-algorithm NEXTN --speculative-num-steps 1 --speculative-eagle-topk 1 --speculative-num-draft-tokens 2` |
| | DSpark `inclusionAI/Ling-3.0-flash-dspark` (`8e5d9988`) | not registered in vLLM | `--speculative-algorithm DSPARK --speculative-draft-model-path <draft>` |
| `google/gemma-4-E2B-it` BF16 (`3e22461f`) | base | — | — |
| | assistant `google/gemma-4-E2B-it-assistant` (`2d874ef7`) | `{"model":<draft>,"num_speculative_tokens":2}` | `--speculative-algorithm NEXTN --speculative-draft-model-path <draft> --speculative-num-steps 5 --speculative-num-draft-tokens 6 --speculative-eagle-topk 1` |
| Qwen3.8-27B NVFP4, local build; FP8 KV cache; `quantization: modelopt` on SGLang; `language_model_only` on vLLM | base | — | — |
| | MTP (built in) | `{"method":"mtp","num_speculative_tokens":3}` | `--speculative-algorithm NEXTN --speculative-num-steps 3 --speculative-eagle-topk 1 --speculative-num-draft-tokens 4` |
| | DFlash2 `z-lab/Qwen3.8-27B-DFlash2` (`50307d4c`) | `{"method":"dflash","model":<draft>,"num_speculative_tokens":7}` | `--speculative-algorithm DFLASH --speculative-draft-model-path <draft> --speculative-num-draft-tokens 8 --speculative-draft-model-quantization unquant` |

Memory requests (planning values): MiniCPM5 24 GiB (28 with the drafter),
Qwen3.6 48 GiB (52), Ling 92 GiB (94–95, with a declared startup peak equal to
the request), Gemma 4 E2B 32 GiB (34), Qwen3.8-27B 40 GiB (46–48); KV cache
8 GiB (6 GiB for Ling).

## Hugging Face download through capyctl (first live use)

Every checkpoint except the local Qwen3.8-27B NVFP4 came from a pinned
`model.source: {type: huggingface, repo, revision}` materialized by the host into
its model store (`~/models/sources/huggingface/<owner>--<name>@<sha>`), with
the host document opting in (`model_sources: {huggingface: allowed, max_bytes:
1TiB}`). Drafters were declared the same way on deployments that were never
activated, so the host downloaded them, and the speculative options point at the
store path.

**Verdict: it works.** Nothing fell back to `huggingface-cli`; the hosts have no
Hugging Face token and none was needed (every repository here is public). What
was observed:

- 17 sources (8 on host A, 9 on host B; about 123 GB per host) downloaded
  concurrently from 12:53 UTC. The link, not capyctl, set the pace: both hosts sit
  on Wi-Fi at about 9 MB/s aggregate each (a single `curl` of the same file got
  1.2 MB/s from a host and 2.8 MB/s from the control-plane host). Small drafters
  verified within minutes; MiniCPM5 (5 GB) at 13:50–13:55; Gemma 4 E2B (10 GB)
  at 14:15; Qwen3.6-35B-A3B NVFP4 (23.5 GB) at 15:15 (host B) and 15:21 (host A); Ling-3.0-flash
  int4 (77 GB) at 17:29 (host A) and 17:30 (host B).
- Resume works: each host role was restarted three times mid-download (binary
  fixes and a host-document change). Partial files and reservations stayed in
  `sources/.capyctl/`, and every download continued from where it stopped. Right
  after a restart, `status` showed `bytes_done: 0` for a few seconds before the
  host's first progress answer; cosmetic.
- Status reports `pending`, `downloading` with bytes done and total, and
  `verified` with the measured checkpoint digest, per host.
- A second deployment of a model already downloaded was refused
  `model_source_pending` on `deploy --activate` until the supervisor asked the
  host again (product fix 3 below).
- There is no "download only" verb: to fetch a drafter, a deployment naming it is
  created and never activated. The copies stay after `delete deployment`
  (SPEC §6.3); `capyctl prune sources` is the explicit reclaim.
- The store layout is `sources/huggingface/<owner>--<name>@<sha>`, not
  `~/models/<name>`. Drafter paths in `extra_args` name the store path.
- During the run, the first deploy of a new source with `--activate --wait`
  returned `model_source_pending` at once rather than waiting for the download.
  The run therefore deployed each source first without activation, and started
  the benchmark deployment once status showed it verified. Fixed after the run
  (product fix 5).

## Product fixes (this branch)

Each fix has a regression test. The tests are CPU-only and are not
qualification. Fixes 1 to 3 were exercised live in this run, as noted. Fixes 4
and 5 follow the owner's decisions on the pull request and were made after the
run, so they were not run live.

1. **vLLM engine PATH lacked the CUDA toolkit.** vLLM treats FlashInfer as
   absent unless `nvcc` is on PATH, because the installation has no pre-built
   cubins. With CUDA graphs on and an FP8 KV cache, vLLM picked the FlashInfer
   attention backend and then failed at graph capture ("FlashInfer backend is
   not available").
   - During the run, a hard-coded `/usr/local/cuda/bin` on the engine PATH
     fixed it. Live: Qwen3.8-27B NVFP4 on vLLM failed before, served after.
   - Owner decision: that was replaced by an optional runtime-profile field,
     `cuda_home` (SPEC §13.3 amendment), in `engines.yaml`, `host.yaml` or
     `CAPYCTL_CUDA_HOME` for the standalone environment installation. capyctl
     prepends `<cuda_home>/bin` after the engine's own `bin` and sets
     `CUDA_HOME`; without it the PATH stays minimal. `capyctl engine add` detects
     it: `CUDA_HOME` if it holds `bin/nvcc`, else `/usr/local/cuda` if it does.
     The harness passes it with `CAPYCTL_HOST_CUDA_HOME`.
2. **vLLM `--speculative-config` could never be approved**
   (`crates/capyctl-config/src/engine_policy.rs`, `runtime/extra_args_policy.py`).
   It was classed as a filesystem path, and its value is a JSON object, so every
   vLLM speculative deployment was refused at deploy time ("names a path outside
   the host's security.approved_paths") and would have been refused again at
   launch. It now has its own check at both gates: named approval, a JSON object
   whose keys are all in a closed list (`method`, `model`,
   `num_speculative_tokens`, `draft_tensor_parallel_size`, `prompt_lookup_max`,
   `prompt_lookup_min`, `draft_sample_method`, `moe_backend`), scalar values,
   and the draft `model` inside an approved directory. Live: every vLLM
   drafter run below. The owner accepted it as ADR 0014 Amendment A3.
3. **A verified source copy was not reused** (`crates/capyctl-store/src/model_sources.rs`).
   A new deployment of a model already verified on its host started `pending`,
   so `deploy --activate` was refused `model_source_pending` until the next
   supervisor round trip. A revision now starts verified when a deployment that
   still exists holds the same store key verified on the same host. Live: every
   `-bn` benchmark deployment after 13:57 UTC.
4. **JIT compile parallelism is bounded** (owner decision). capyctl sets
   `MAX_JOBS` to `clamp(floor(MemAvailable at launch / 8 GiB), 1, CPU count)`
   and `FLASHINFER_NVCC_THREADS=1` in both engines' environments, and logs the
   choice at launch. A profile's `env` may override either one with a positive
   integer.
   - `MAX_JOBS` is honoured by FlashInfer's ninja JIT, torch `cpp_extension`
     and `tvm_ffi`.
   - `FLASHINFER_NVCC_THREADS` sets the threads inside each FlashInfer `nvcc`;
     1 is FlashInfer's own default.
   - vLLM's `NVCC_THREADS` applies only when vLLM itself is built, so capyctl does
     not set it.
5. **`deploy --activate --wait` waits for a downloading source** (owner
   decision). `deploy model --activate` and `start --wait` already waited for a
   pending checkpoint digest. They now also wait while no host holds the
   declared source verified, within the same Initialize window. A failed source
   ends the wait.

Harness changes (`scripts/live/matrix`): benchmark models and drafter variants in
`models.json` (with `hf` sources, `residency`, chained `extends`);
`gen_host_doc.py --hf-max-bytes`, `--approve-speculation` and `--cuda-home`; p90 in `bench.py`
distributions; the stream-end metrics above; a `MemAvailable` sample at Ready in
M80.

## Failures and their reasons

- **Ling-3.0-flash on vLLM: not run.** vLLM 0.29.0 has the Bailing V3 model
  but not its configuration class, and transformers 5.17.0 has no
  `bailing_hybrid` either, so loading the checkpoint's config needs
  `trust_remote_code` ("contains custom code which must be executed"). The
  benchmark host documents keep `security.trust_remote_code: false`, and
  enabling checkpoint code was not authorized for this run. SGLang 0.5.20 ships
  its own `bailing_hybrid` configuration and served the model. vLLM has no
  registered Ling DSpark drafter either.
- **Both hosts ran out of memory once (15:26 and 15:32 UTC).** FlashInfer's CUTLASS
  fused-MoE kernels for sm_120 are JIT-compiled at engine start with one `nvcc`
  job per core; each `cicc` held 7 to 9 GB. It happened for SGLang's
  `flashinfer_cutlass` MoE runner on Qwen3.6 NVFP4 and for vLLM's MTP head
  (an unquantized MoE) on the same model. The kernel OOM killer took the user
  session with the host role and its tmux; the hosts stayed up and nothing was
  rebooted. The runs switched to the prebuilt Marlin runner (SGLang) and
  `moe_backend: triton` for the vLLM draft. capyctl could not bound JIT
  parallelism then; product fix 4 now sets `MAX_JOBS` from free memory. It was
  not re-run live.
- **Engine and recipe findings, each fixed in the fixture:** SGLang refuses
  deep parking for the modelopt 27B (`capability_missing:deep_park`; all runs
  use `restart_only`); SGLang's DFlash drafter inherits `modelopt` from the
  target and then asks for the `accelerate` package
  (`--speculative-draft-model-quantization unquant`); SGLang refuses the
  default FlashInfer TRT-LLM MoE runner for NVFP4 Qwen3.6 on sm_121.
- **Harness findings.** SGLang engine output is not captured without
  `--debug-engine-logs` (the private engine log files stay empty), which hid the
  DFlash cause until host B was restarted with it. `cleanup_check` counts
  request leases server-wide, so a run on the other host can fail it: two
  kept rows (Gemma baseline on SGLang, Qwen3.6 MTP on vLLM) have `rc=1` from
  that check alone, with five of five requests served and the host
  clean. Evidence directories for rows that failed and were rerun are kept as
  `*.prev-<time>`.
- **Self-inflicted:** one SGLang DFlash2 launch died when its host role was
  restarted for a fix; two deployments were left behind by the OOM and
  collided with their reruns (`route_conflict`) until the rows' own cleanup
  deleted them.

## Dropped models (owner decision)

- **DeepSeek-V4-Flash.** Does not fit one box: 159.6 GB native (FP4 experts plus
  FP8), about 168 GB as NVFP4. The only one-box build found is a forked vLLM with
  a custom 2-bit checkpoint (about 1.75 tok/s). Two-host TP2 is parked.
- **Tinfield-1 at 2 bits.** Published only as GGUF (llama.cpp) and EXL3
  (exllamav3); neither vLLM nor SGLang loads either, and the BF16 checkpoint
  (about 330 GB) does not fit.
- **Qwen3.8-Flash-Next.** About 124 GiB as NVFP4, of which 47.7 GiB is an n-gram
  embedding table that every one-box recipe keeps on NVMe: SGLang rewrites it on
  every restart (about 55 minutes), and vLLM needs an out-of-tree patch.

## Custom builds

None. The stock environments ship every architecture and drafter this run
used: vLLM 0.29.0 registers `Qwen3DSparkModel` (the MiniCPM5 DSpark pairing
that vLLM issue #57013 calls unvalidated ran fine), `DFlash2DraftModel`, the
Gemma 4 assistant and Qwen3.5-family MTP; SGLang 0.5.20 ships
`bailing_moe_v3` with its configuration, DSpark (`Qwen3DSparkModel`,
`LingDSparkModel`), DFlash, NEXTN and the Gemma 4 assistant. The one model a
stock engine could not load (Ling on vLLM) needs checkpoint code, not a newer
build. No new environment was created, so `capyctl engine add` was not exercised
live in this run.

## Evidence

Local to the control-plane host, under `target/live/bench/`: one `M80-<fixture>/`
directory per run (fixture, `records.jsonl` with per-chunk arrival times,
`cells/`, engine `/metrics` scrapes, capyctl latency views, page-cache samples, E0
snapshots, cleanup checks, `bench.json`, `summary.md`), `table.json` (the table
above), and the sync, role and download logs. Run `matrix-20260925T125244Z`.

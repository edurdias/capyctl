# Status: llama.cpp GB10 rows LC1–LC6 run live on host A, all pass — 2026-10-15 (branch `test/live-rows-0.1.3`)

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

# Release note: none

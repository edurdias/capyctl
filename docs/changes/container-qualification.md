# Status: Containers run live on a GB10 — 2026-10-20 (branch `docs/container-qualification`)

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

# Release note: none

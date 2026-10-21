# Status: Containers run live on a discrete GPU — 2026-10-21 (branch `docs/container-qualification-discrete`)

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

# Release note: Operations

- **Containers run on a discrete GPU.** On an RTX 4090 Laptop GPU with
  Docker and CDI, a standalone role running as an unprivileged user started,
  parked, woke and stopped vLLM 0.31.0 and started and stopped TensorFold
  0.6.6; per-process GPU memory and the card's limits work as on a host. A
  TensorFold image needs `ninja`, and a new container started on existing
  state needs the same `--hostname` as the one before. See
  [Running in a container](../operations/install.md#running-in-a-container).

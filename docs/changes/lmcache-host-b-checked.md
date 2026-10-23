# Status: LMCache environments on host B import and see the GPU — 2026-10-23 (branch `docs/lmcache-host-b-checked`)

The owner freed host B for a short check on 2026-10-23. Both LMCache environments copied
there on 2026-10-22 (see "LMCache 0.5.6 environments on host B") import and see the GPU:

- `~/vllm-0.32.0-lmcache-venv`: vLLM 0.32.0, LMCache 0.5.6, torch 2.13.0+cu130; CUDA
  available, NVIDIA GB10.
- `~/sglang-0.5.22-lmcache-venv`: SGLang 0.5.22, LMCache 0.5.6, torch 2.14.1+cu130; CUDA
  available, NVIDIA GB10.

The check imported each engine and `lmcache` and asked torch for the device; it started
no engine, role or LMCache server, and left no process on the GPU. The LC rows have not
been run on host B; every row in 0.1.7 ran on host A.

# Release note: none

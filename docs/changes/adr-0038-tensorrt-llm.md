# Status: ADR 0038 accepted: TensorRT-LLM as a native engine — 2026-10-27 (branch `docs/adr-tensorrt-llm`)

ADR 0038 (Accepted 2026-10-27; target 0.2.0) designs TensorRT-LLM as a native engine,
following the owner's decisions of 2026-10-25 and the owner's answers of 2026-10-27 to
the ADR's nine open questions. The base version is 1.2.1: a virtual environment on host A,
`~/tensorrt-llm-1.2.1-venv`, built from NVIDIA's aarch64 wheel (pinned by URL and SHA-256)
on the CUDA 13.1 stack with torch 2.9.1, chat qualified `restart_only`. Evidence was read
from the v1.2.1 tag, the wheel itself (its metadata and all of its Python, over HTTP range
requests, nothing installed) and NVIDIA's documentation; no host was used.

Three answers changed the design:

- **System packages on host A** (question 4): the owner approved installing
  `libopenmpi-dev` with apt and the CUDA 13.1 toolkit on host A. The recipe installs them
  first, `CUDA_HOME` names the toolkit, and the MPI pins expect Ubuntu's Open MPI 4.1.6,
  which TR1 confirms.
- **More versions** (question 7): the 1.3 and 1.4 pre-releases are also qualified in
  0.2.0, each in its own environment on host A with its own live rows (TR1a–TR11a,
  TR1b–TR11b). 1.4 needs CUDA 13.4 and another torch, renders the backend in its own
  spelling and stays restart-only, with `--enable_sleep_mode` reserved. What else differs
  is read from each tag at implementation and established by the rows. Every other
  version runs as an uncertified custom setup and is never refused for its version.
- **Kind name** (question 8): the kind is `tensorrt-llm` (profile default,
  `local_engine.tensorrt-llm`, `--tensorrt-llm-bin`, `CAPYCTL_TENSORRT_LLM_BIN`,
  `engine_config.tensorrt-llm.llm_api_options`); the engine's command stays `trtllm-serve`.

The other answers kept the recommendations: one process with MPI pinned to an isolated
singleton; no idle signal, so cut requests settle at the next verified stop with ADR
0032's restart when they fill a deployment; a per-launch link named after the served name;
`max_batch_size` 32 by default; a deployment mapping of LLM API options with reserved keys
refused; FlashInfer's kernel downloads allowed and documented.

Registration reads metadata only and never imports the package, because importing it
initializes MPI and CUDA. CapyCTL runs `trtllm-serve serve` on `127.0.0.1` and a leased
port, with the PyTorch backend, one GPU and a configuration file CapyCTL writes whole. The
KV pool is bound by `kv_cache_config.max_tokens` from the grant. Every launch checks that
no descendant left the process group and runs the loopback listener check. Decisions,
embeddings, LMCache, several GPUs and groups are refused.

The ADR adds conformance row T49 and 33 live rows on host A with Qwen3-4B. Nothing is
implemented; no code or tests change. CPU and Fake-engine tests are not qualification, and
the TR rows are the live check still needed.

# Release note: none

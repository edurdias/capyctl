# Status: ADR 0038 proposed: TensorRT-LLM as a native engine — 2026-10-25 (branch `docs/adr-tensorrt-llm`)

ADR 0038 (Proposed, for the owner to decide; target 0.2.0) designs TensorRT-LLM 1.2.1 as a
native engine, following the owner's decisions of 2026-10-25: a virtual environment on
host A, `~/tensorrt-llm-1.2.1-venv`, built from NVIDIA's aarch64 wheel (pinned by URL and
SHA-256) on the CUDA 13.1 stack with torch 2.9.1, chat qualified `restart_only`. Evidence
was read from the v1.2.1 tag, the wheel itself (its metadata and all of its Python, over
HTTP range requests, nothing installed) and NVIDIA's documentation; no host was used.

The engine kind is `trtllm`. Registration reads metadata only and never imports the
package, because importing it initializes MPI and CUDA. CapyCTL runs `trtllm-serve serve`
on a per-launch link named after the served name (the engine serves under the model
directory's name and has no served-name option), on `127.0.0.1` and a leased port, with
the PyTorch backend, one GPU and a configuration file CapyCTL writes whole, since that file
overrides the command line. The KV pool is bound by `kv_cache_config.max_tokens` from the
grant; the free-memory fraction is only a ceiling, because on GB10 TensorRT-LLM's own
measure counts other processes' memory. Readiness is `/health`, `/version`, `/v1/models`,
CapyCTL's probe and a check of the pool the engine built.

`/metrics` is consumed on read and writes nothing while idle, so TensorRT-LLM reports no
idle signal: cut requests settle at the next verified stop (ADR 0031 §6), with ADR 0032's
restart when they fill a deployment. The engine aborts a request only when its client
disconnects. Sleep in 1.2.1 is a prototype the server cannot reach, so the engine stays
`restart_only`; the ADR states what would qualify a parking tier later. The model runs in
the server process (`TLLM_WORKER_USE_SINGLE_PROCESS=1`) with MPI pinned to an isolated
singleton: by default Open MPI 4.1.6 forks a daemon that leaves the process group and
binds transports on every interface. Every launch checks that no descendant left the
process group and runs the loopback listener check. No usage reporting was found;
`DO_NOT_TRACK` follows the host's `engine_telemetry`. Decisions, embeddings, LMCache,
several GPUs and groups are refused.

The ADR adds conformance row T49, live rows TR1–TR11 on host A with Qwen3-4B, and nine
open questions with a recommendation each. Nothing is implemented; no code or tests
change. CPU and Fake-engine tests are not qualification, and the TR rows are the live
check still needed.

# Release note: none

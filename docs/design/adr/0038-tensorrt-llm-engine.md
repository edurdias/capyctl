# ADR 0038 — TensorRT-LLM (`trtllm-serve`) as a native engine

**Status:** Accepted (2026-10-27). Proposed 2026-10-25; the owner answered every open
question on 2026-10-27, and the answers are recorded below ("Owner decisions of
2026-10-27"). Three answers differ from the recommendations: Open MPI and the CUDA 13.1
toolkit are installed system-wide on host A (decision 4), the 1.3 and 1.4 pre-releases are
qualified with their own rows in 0.2.0 (decision 7, §2a), and the engine kind is
`tensorrt-llm` (decision 8). Target release 0.2.0 (retargeted from 0.1.9 on 2026-10-27,
after 0.1.9 shipped with ADR 0035 only). Nothing is implemented; the SPEC, guide and
operations texts this ADR lists change with the implementation, as for ADRs 0029 and 0033.
**Amends (with the implementation):** `SPEC.md` §1.1 (R01, supported engines), §9.4 (later
backends), §20 (T49, live rows TR1–TR11 for 1.2.1, TR1a–TR11a for the 1.3 pre-release and
TR1b–TR11b for the 1.4 pre-release); ADR 0018 §1 (a sixth installation kind: detection, the
version check that never imports the engine, the fingerprint); ADR 0033 §2 (the
supported-engine table gains a TensorRT-LLM row, off); ADR 0035 §10 (TensorRT-LLM has no
local multi-GPU entry).
**Related:** ADR 0008 (installations and fingerprints), ADR 0012 (deep parking and its
protections), ADR 0014 (§2 typed parameters, §3 reserved settings, §5 memory request, §6
extra arguments, §8 sensitive options, A1, A2, A8, A12, A18), ADR 0017 (version skew and
the verified set), ADR 0019 (device selection), ADR 0024 (parsers by model family), ADR
0027 (engine processes and helpers), ADR 0028 (groups), ADR 0029 (llama.cpp, the landing
shape this follows), ADR 0030 (decision models), ADR 0031 §6 and §7 (engines without an
idle signal; the loopback listener check), ADR 0032 (cancelled-request settlement), ADR
0033 and ADR 0034 (LMCache), ADR 0035 (several GPUs on one host), ADR 0036 (embeddings),
ADR 0037 (trace context).

## Context

### The owner's decisions of 2026-10-25

For 0.2.0 the owner chose TensorRT-LLM **1.2.1**, the current stable release, as a native
engine. It runs from a virtual environment on host A, `~/tensorrt-llm-1.2.1-venv`, built
from NVIDIA's aarch64 wheel on the CUDA 13.1 stack with torch 2.9.1. Chat is to be
qualified `restart_only`: TensorRT-LLM's sleep mode is a prototype. This ADR designs that
integration. No host is reachable while it is written, so nothing below was run; every
fact about the engine is read from its source, its wheel or its documentation, and the live
rows at the end are what qualifies it.

On 2026-10-27 the owner answered this ADR's open questions (recorded at the end) and widened
the version scope: beside 1.2.1, the 1.3 and 1.4 pre-releases are qualified in 0.2.0, each
in its own environment on host A with its own rows (decision 7, §2a). Every other version
runs as an uncertified custom setup and is never refused for being uncertified (the owner's
rule of 2026-10-27).

### What was read

- TensorRT-LLM [v1.2.1](https://github.com/NVIDIA/TensorRT-LLM/tree/v1.2.1), commit
  `376f7e1`, released 2026-04-20 (not a pre-release). Paths below are under
  `tensorrt_llm/` at that tag unless stated, and line numbers are the tag's.
- The wheel the owner named,
  [`tensorrt_llm-1.2.1-cp312-cp312-linux_aarch64.whl`](https://pypi.nvidia.com/tensorrt-llm/)
  on NVIDIA's package index: 1,631,156,104 bytes, SHA-256
  `4c4cf7c00e0e54fe1038ffeb4247b66664d585d054c67810ce04d1c7a018f57a` (the index's own
  hash). Its central directory, `METADATA`, `entry_points.txt` and every one of its 853
  Python files under `tensorrt_llm/` and `triton_kernels/` were read over HTTP range
  requests, without installing it. The fourteen source files cited most below are
  byte-identical in the wheel and at the tag.
- The 1.2.1 documentation: [installation](https://nvidia.github.io/TensorRT-LLM/1.2.1/installation/linux.html),
  [supported hardware](https://nvidia.github.io/TensorRT-LLM/1.2.1/supported-hardware.html),
  [`trtllm-serve`](https://nvidia.github.io/TensorRT-LLM/1.2.1/commands/trtllm-serve/trtllm-serve.html)
  and [KV cache](https://nvidia.github.io/TensorRT-LLM/1.2.1/features/kvcache.html).
- NVIDIA's [DGX Spark known issues](https://docs.nvidia.com/dgx/dgx-spark/known-issues.html)
  (updated 2026-10-21), for unified-memory reporting.
- Open MPI [v4.1.6](https://github.com/open-mpi/ompi/tree/v4.1.6), the version Ubuntu 24.04
  ships (`openmpi 4.1.6-7ubuntu2`), for what the MPI runtime TensorRT-LLM loads does at
  start. The owner approved installing it on host A with `libopenmpi-dev` (owner decision
  2026-10-27, question 4); TR1 confirms that mpi4py loads it.
- FlashInfer [v0.6.4](https://github.com/flashinfer-ai/flashinfer/tree/v0.6.4), the version
  the wheel pins, for its kernel downloads.
- For the 1.3 and 1.4 pre-releases, nothing beyond the 1.4.0rc1 facts cited in evidence §8
  and under "Alternatives considered" was read. What else differs from 1.2.1 is established
  by reading their tags at implementation and by their own live rows (§2a).

## Evidence

### 1. Distribution and installation

- **Where the wheel is.** PyPI's 1.2.1 entry is a 10,133-byte source archive whose
  `pyproject.toml` builds with `wheel-stub` and `index_url = "https://pypi.nvidia.com/"`:
  `pip install tensorrt_llm` on PyPI downloads NVIDIA's wheel at install time. NVIDIA's
  index lists three 1.2.1 wheels: `cp310` and `cp312` for x86-64, and `cp312` for aarch64,
  the one above.
- **Prerequisites** ([installation guide](https://nvidia.github.io/TensorRT-LLM/1.2.1/installation/linux.html),
  "tested on Ubuntu 24.04", Python 3.12): the CUDA Toolkit 13.1 with `CUDA_HOME` set;
  `pip3 install torch==2.9.1 torchvision --index-url https://download.pytorch.org/whl/cu130`
  ("By default, PyTorch CUDA 12.8 package is installed"); `sudo apt-get -y install
  libopenmpi-dev`; then the wheel. The guide warns that pip may replace a cu130 torch with
  a CUDA 12.8 build and recommends a constraints file pinning torch.
- **The wheel's requirements** (`METADATA`): `torch>=2.9.1,<=2.10.0a0`, `tensorrt~=10.14.1`,
  `cuda-python>=13`, `nvidia-nccl-cu13>=2.27.7,<=2.28.9`, `flashinfer-python==0.6.4`,
  `triton==3.5.1`, `transformers==4.57.3`, `numpy<2`, and `mpi4py` without a version.
  mpi4py needs an MPI library at run time; PyPI also publishes `openmpi` 5.0.11 and `mpich`
  5.0.2 wheels for aarch64 [INFERENCE: whether mpi4py's binary wheel would load one of
  them before the system's library was not checked].
- **Layout.** 1,835 entries, 2,619,911,400 bytes unpacked, of which `tensorrt_llm/` is
  2,618,405,851 (its `libs/` holds `libtensorrt_llm.so`, 935 MB, `libnvinfer_plugin_tensorrt_llm.so`,
  `libth_common.so` and the attention libraries). `top_level.txt` names `tensorrt_llm`,
  `triton_kernels` and `examples` (twelve example scripts installed as a top-level package).
  Console scripts: `trtllm-serve`, `trtllm-bench`, `trtllm-build`, `trtllm-eval`,
  `trtllm-prune`, `trtllm-refit`; `trtllm-llmapi-launch` is a data script. The version is
  `version.py`'s `__version__ = "1.2.1"`, exported as `tensorrt_llm.__version__`.
- **Importing the package starts MPI.** `_utils.py:37` is `from mpi4py import MPI`, which
  initializes MPI on import, and `__init__.py:117-120` imports it. Running `trtllm-serve`,
  even for `--help`, or `python -c "import tensorrt_llm"` therefore initializes MPI and CUDA
  (§6 below for what that starts).
- **Hardware.** The 1.2.1 hardware page lists DGX Spark under Blackwell. That is a platform
  statement; which kernels exist for a given model, dtype and quantization on SM 12.1 is
  established only by running it.

### 2. The command line and the configuration file

`trtllm-serve` is a Click group whose default command is `serve`: an argument that is not a
subcommand name is taken as the model (`commands/serve.py:949-965`), so a model directory
named `disaggregated` would select another command. `serve`'s options
(`commands/serve.py:271-474`):

- `MODEL` (positional: "model name | HF checkpoint path | TensorRT engine path"),
  `--host` (default `localhost`), `--port` (8000), `--backend` (`pytorch` default,
  `tensorrt`/`trt`, `_autodeploy`), `--log_level` (`info`).
- Sizing: `--max_beam_width`, `--max_batch_size` (2048), `--max_num_tokens` (8192, the
  per-iteration token budget), `--max_seq_len` (from the model when unset),
  `--free_gpu_memory_fraction` alias `--kv_cache_free_gpu_memory_fraction` (0.9),
  `--enable_chunked_prefill`.
- Parallelism: `--tensor_parallel_size`/`--tp_size`, `--pipeline_parallel_size`/`--pp_size`,
  `--context_parallel_size`/`--cp_size`, `--moe_expert_parallel_size`/`--ep_size`,
  `--moe_cluster_parallel_size`/`--cluster_size`, `--gpus_per_node` (detected).
- Code and sources: `--tokenizer`, `--custom_tokenizer` (an alias or "Python import path"),
  `--custom_module_dirs` (directories it imports), `--trust_remote_code`, `--revision`,
  `--chat_template` ("a file path or one-liner template string"), `--reasoning_parser`,
  `--tool_parser`, `--media_io_kwargs`.
- Disaggregated serving and tracing: `--metadata_server_config_file` (an etcd client,
  `serve/metadata_server.py`), `--server_role`, `--disagg_cluster_uri`,
  `--otlp_traces_endpoint`, `--num_postprocess_workers`,
  `--fail_fast_on_attention_window_too_large`.
- `--config`, alias `--extra_llm_api_options`: a YAML file of LLM API arguments. It is read
  after the command line and **overrides** it (`commands/serve.py:529-533`, and the option's
  help: "a YAML file that overwrites the parameters specified by trtllm-serve").

There is no `--served_model_name`, no API key option, no gRPC server and no `--version`.
The served model ID is the model directory's last path component when the argument is a
directory, else the argument itself (`serve/openai_server.py:128-132`), and the chat handler
never compares a request's `model` with it (`openai_chat`, `serve/openai_server.py:485-616`).
Click takes exact option names (no abbreviation) and accepts `--name=value`.

The YAML reaches every field of `TorchLlmArgs` (`llmapi/llm_args.py:2768` onward), a
strict model that refuses unknown fields. Among them: `kv_cache_config` (§7),
`enable_iter_perf_stats` (default false, `llm_args.py:2825-2828`), `return_perf_metrics`
(`llm_args.py:2125-2127`), `cuda_graph_config` (on by default, `llm_args.py:2777-2778`),
`orchestrator_type` (`rpc` or `ray`, `llm_args.py:2129`), `kv_connector_config` (prototype,
`llm_args.py:2938-2942`), `enable_sleep` (prototype, `llm_args.py:2964-2969`),
`speculative_config`, `lora_config`, `load_format` (`auto` or `dummy`) and
`checkpoint_format`. A search of the wheel's Python finds at least 126 environment
variables read, 66 of them `TLLM_*` or `TRTLLM_*`.

### 3. Routes and readiness

Routes (`serve/openai_server.py:254-295`): `GET /health`, `/health_generate`, `/version`,
`/v1/models`, `/metrics`, `/perf_metrics`, `/steady_clock_offset`; `POST
/steady_clock_offset`, `/kv_cache_events`, `/v1/completions`, `/v1/chat/completions`,
`/v1/responses`, `/release_memory`, `/resume_memory`, `/update_weights`; `GET` and `DELETE
/v1/responses/{id}`; and `/prometheus/metrics`, mounted only with `return_perf_metrics`.
None needs a key.

- `/health` answers an empty 200 while the executor exists and is not shut down, else 503
  (`serve/openai_server.py:340-344`, `llmapi/llm.py:842-849`). It runs no generation.
- `/health_generate` runs a one-token chat through the same handler and answers 200 or 500
  (`serve/openai_server.py:346-376`).
- `/version` answers `{"version": "1.2.1"}` (`serve/openai_server.py:378-380`); `/v1/models`
  lists the one served ID (`:382-384`).
- `/v1/responses` keeps conversation history in the server's memory unless
  `TRTLLM_RESPONSES_API_DISABLE_STORE` is set (`serve/openai_server.py:124-126`).
- `trtllm-serve` binds its HTTP socket to `--host` before it builds the model
  (`commands/serve.py:189-197`), then constructs `LLM` (PyTorch backend) and serves it
  with uvicorn on that socket (`:199-229`, `serve/openai_server.py:1129-1139`).

### 4. Metrics

- **`/metrics` is iteration statistics, not a gauge.** The handler collects
  `llm.get_stats_async(2)` into a JSON array (`serve/openai_server.py:386-390`; the route's
  own comment: "the metrics endpoint only reports iteration stats", `:259`). The executor
  records one entry per scheduler iteration only when `enable_iter_perf_stats` is on, and a
  read takes every entry recorded since the previous read and empties the buffer
  (`_torch/pyexecutor/py_executor.py:532-544`, `:874-881`). Fields (the C++ serializer,
  `cpp/tensorrt_llm/executor/jsonSerialization.cpp`): `timestamp`, `iter`,
  `numActiveRequests`, `numQueuedRequests`, `numCompletedRequests`,
  `inflightBatchingStats` (scheduled, context, generation and paused requests),
  `kvCacheStats` (`maxNumBlocks`, `freeNumBlocks`, `usedNumBlocks`, `tokensPerBlock`,
  reuse counters), `gpuMemUsage`.
- **An idle engine writes nothing.** `numActiveRequests` is the active list at the start of
  an iteration (`py_executor.py:693-700`), so the last entry before a lull counts the
  request that finished in it. With no active and no waiting requests the executor blocks
  on its input queue with no timeout (`_torch/pyexecutor/executor_request_queue.py:297-317`;
  a 1200 s timeout applies only with MPI disabled) and records no entry. No entry ever reads
  0 running and 0 waiting, and an empty read means "no iteration since the last read" or a
  failed fetch (the fetch has a 2 s bound), which `/metrics` does not distinguish.
  `numQueuedRequests` is the input queue's size only (`executor_request_queue.py:870-871`),
  not requests fetched and waiting for KV space.
- **`gpuMemUsage` is the device's, not the process's.** It is `total − free` from
  `torch.cuda.mem_get_info()` (`py_executor.py:787-788`).
- **`/prometheus/metrics`** exports `trtllm_request_success_total{finished_reason}` and
  four latency histograms (`metrics/collector.py:27-106`), all written when a response
  finishes through the HTTP handler (`serve/openai_server.py:465-469`, called after a
  stream's `[DONE]`, `:510-511`). A stream cut by its client never reaches that call, so
  cancelled requests are not counted. There is no running, waiting or in-flight gauge.

### 5. Cancellation

- For every chat request the handler starts `await_disconnected`, which polls the client
  connection once a second and, when it is gone and the request is unfinished, calls
  `promise.abort()` and logs `<client> is disconnected, abort <id>`
  (`serve/openai_server.py:212-220`, started at `:598`).
- The abort is queued, not applied: `GenerationResult.abort()` calls the executor's
  `abort_request` (`executor/result.py:734-738`); the proxy puts a `CancellingRequest` on
  its queue with the comment "it may take a while for the request to be cancelled in the
  worker" (`executor/proxy.py:149-158`); the worker calls `cancel_request`
  (`executor/base_worker.py:341-350`), which enqueues the ID
  (`py_executor.py:487-493`). The scheduler drops it from the waiting queue, or marks an
  active request finished with reason `CANCELLED` and frees its KV through the normal
  path, at its next iteration (`py_executor.py:2528-2558`). The final output says
  `finish_reason: "cancelled"` (`executor/result.py:316-318`), but the client that would
  read it is gone.
- There is no abort by an ID CapyCTL assigns and no route that lists requests. The only
  abort is triggered by the disconnect itself, so CapyCTL cannot keep reading a response
  after asking for its abort, which is what ADR 0032 settles SGLang on.

### 6. Processes, MPI and listeners

- **Default process model.** On Linux with one GPU, `LLM` creates a
  `GenerationExecutorProxy` (`executor/executor.py:533-552`), which starts the model worker
  through an `MpiPoolSession` (`executor/proxy.py:64-72`), that is an mpi4py
  `MPIPoolExecutor` spawning one worker process with `MPI_Comm_spawn`
  (`llmapi/mpi_session.py:170-174`). The proxy and worker talk over ZeroMQ, bound by
  default to `tcp://127.0.0.1:*` with a random 32-byte HMAC key over pickled messages
  (`executor/ipc.py:52-53`, `:97-100`), and over a stats RPC socket at
  `ipc://<tempdir>/rpc_<uuid>` (`executor/proxy.py:95-97`, `llmapi/mpi_session.py:544-548`;
  the directory is Python's `tempfile.gettempdir()`, so `TMPDIR` or `/tmp`).
- **The single-process switch.** With `TLLM_WORKER_USE_SINGLE_PROCESS=1`
  (`llmapi/utils.py:358-361`) a one-GPU `LLM` runs the worker in the server process
  instead (`executor/executor.py:509-531`), warning "this may hurt streaming generation
  performance". `TLLM_DISABLE_MPI=1` (`_utils.py:535-537`) is the Ray path, which opens its
  own listeners.
- **What Open MPI 4.1.6 starts.** A process that initializes MPI without a launcher is a
  singleton. Unless `ess_singleton_isolated` is set, the singleton forks an `orted`
  daemon at `MPI_Init` (`orte/mca/ess/singleton/ess_singleton_module.c:163-171`), and that
  daemon is started with `--hnp --set-sid`, "to get out of our process group" (`:579-585`).
  Workers it spawns for `MPI_Comm_spawn` are its children. The TCP transport binds its
  listening socket to `INADDR_ANY` (`opal/mca/btl/tcp/btl_tcp_component.c:926`, `:965`),
  as does the daemon's out-of-band channel (`orte/mca/oob/tcp/oob_tcp_listener.c:282`,
  `:535`); `if_include` chooses which addresses are used and advertised, not what is bound.
  [INFERENCE: not run. With the default process model on host A, the GPU-holding worker
  would run outside the engine's process group, under a daemon that left it, and some MPI
  listener would be on every interface. Open MPI 5 and MPICH behave differently, which is
  why TR1 records which library is loaded.]
- **HTTP.** The API socket is bound to `--host` alone (`commands/serve.py:193`). The
  helpers `get_free_port` and `find_free_port` bind `('', 0)` briefly to pick a port
  (`_utils.py:488`, `llmapi/mpi_session.py:540`); in this tag they serve the disaggregated
  and Ray paths [INFERENCE from their callers].

### 7. KV cache sizing, and GB10

- `KvCacheConfig` (`llmapi/llm_args.py:1628-1659`, `:1700`): `free_gpu_memory_fraction`
  (default 0.9), `max_tokens` (none), `host_cache_size` (bytes, none), `enable_block_reuse`
  (true), `dtype` (`auto`). "If both `max_tokens` and `free_gpu_memory_fraction` are
  specified, memory corresponding to the minimum will be used."
- How the pool is sized (`_torch/pyexecutor/_util.py:325-437`): after the weights load,
  TensorRT-LLM starts the executor with a small KV cache, runs dummy requests to profile
  the peak, shuts it down, and computes `available = (total − peak + estimation KV) ×
  fraction` (`:116-132`), where `total` comes from `torch.cuda.mem_get_info()` and `peak`
  is torch's own peak plus `max(device used − torch used, 0)` (`:373-385`). Then `max_tokens`,
  if given, caps it (`:419-437`), and the executor is started again with the final pool.
- **On unified memory that "peak" includes the machine.** `mem_get_info` on GB10 reports
  the shared pool, so memory held by every other process counts as this engine's "memory
  used outside torch", and the fraction becomes a fraction of what the machine has free at
  that instant. NVIDIA's known issues add that on DGX Spark "the memory size reported by
  `cudaMemGetInfo` may be smaller than the actual allocatable memory", because it does not
  count what the kernel could reclaim. A pool sized by the fraction alone therefore depends
  on the other tenants and on the page cache at start.
- The engine's KV bytes per token are `_get_kv_size_per_token()` (layers × KV heads × head
  dimension × 2 × the KV dtype's bytes for full attention; variable-window and draft
  models change it, `_util.py:409-437`).

### 8. Sleep

`enable_sleep` is a prototype field ("requires extra setup that may slowdown model
loading", `llmapi/llm_args.py:2964-2969`). The server registers `/release_memory`,
`/resume_memory` and `/update_weights` as "RL-only endpoints" (`serve/openai_server.py:283-292`),
and each begins with `assert isinstance(self.llm, AsyncLLM)` (`:1114-1127`). `trtllm-serve`
constructs `LLM`, never `AsyncLLM` (`commands/serve.py:199-210`), so 1.2.1's server has no
usable sleep or wake. (The 1.4.0rc1 pre-release adds `--enable_sleep_mode` with an
HMAC-authenticated control API; 1.4 is qualified restart-only all the same, §2a.)

### 9. Usage reporting and outbound connections

- No usage reporting in TensorRT-LLM's Python: the wheel has no usage or telemetry module,
  and a search of all 853 Python files for usage statistics, telemetry, do-not-track
  switches and NVIDIA URLs finds only OpenTelemetry tracing, which needs an explicit
  `--otlp_traces_endpoint` (`llmapi/llm.py:243-250`) and whose packages are only in the
  wheel's `devel` extra. The native libraries were not examined.
- Hugging Face Hub, which the pinned Transformers 4.57.3 requires at `>=0.34.0,<1.0`,
  turns its telemetry off when `DO_NOT_TRACK` is set (for example
  [v0.36.0 `constants.py:172-175`](https://github.com/huggingface/huggingface_hub/blob/v0.36.0/src/huggingface_hub/constants.py#L172-L175)).
- FlashInfer 0.6.4 downloads precompiled kernels from
  `https://edge.urm.nvidia.com/artifactory/...` when one it needs is not cached
  (`flashinfer/jit/cubin_loader.py:32-34`, `:202-208`). That is not usage reporting, but it
  is an outbound connection at run time [INFERENCE: whether TensorRT-LLM's PyTorch backend
  reaches that path for Qwen3-4B BF16 on SM 12.1 is not known].

### 10. Other surfaces

- **Embeddings.** No `/v1/embeddings` route and no pooling mode (§3); `mm_embedding_serve`
  is a multimodal encoder server answering on `/v1/chat/completions`
  (`serve/openai_server.py:320-338`).
- **Decisions.** No ADR 0030 decision path exists for this engine.
- **`cache_salt`.** Accepted on chat requests and passed to `generate_async`
  (`serve/openai_protocol.py:640`, `serve/openai_server.py:595`); "KV cache will be salted
  with the provided string to limit the kv cache reuse to the requests with the same
  string" (`llmapi/llm.py:379`).
- **Usage.** Chat answers carry `usage.prompt_tokens_details.cached_tokens`
  (`serve/postprocess_handlers.py:182-183`). There is no per-request timing object.
- **Parsers.** Reasoning: `deepseek-r1`, `qwen3`, `nano-v3`
  (`llmapi/reasoning_parser.py:109-113`). Tools: `qwen3`, `qwen3_coder`, `kimi_k2`,
  `deepseek_v3`, `deepseek_v31`, `deepseek_v32` and more
  (`serve/tool_parser/tool_parser_factory.py:13-19`).
- **KV connectors.** `kv_connector_config` is the prototype connector API LMCache 0.5.6's
  TensorRT-LLM integration plugs into ([LMCache v0.5.6](https://github.com/LMCache/LMCache/blob/v0.5.6/lmcache/integration/tensorrt_llm/__init__.py)).

### Summary

| Question | TensorRT-LLM 1.2.1 | Consequence for CapyCTL |
|---|---|---|
| Install | NVIDIA's wheel, torch 2.9.1 cu130, CUDA 13.1, an MPI library | registration reads metadata only; importing starts MPI |
| Served name | the model directory's last component | CapyCTL names the directory |
| Hidden configuration | `--config` YAML overrides the command line; 66+ `TLLM_*`/`TRTLLM_*` variables | CapyCTL writes the YAML; closed environment |
| Readiness | `/health` without generation; `/v1/models`; `/version` | plus CapyCTL's probe |
| Idle signal | none: `/metrics` is consumed on read and silent when idle | ADR 0031 §6 rule |
| Per-request settlement | abort only on disconnect, no abort by ID | not available |
| Sleep | prototype; routes need `AsyncLLM`, which the server never builds | `restart_only` |
| Processes | MPI pool by default; single process by switch | single process |
| Listeners | HTTP on `--host`; ZeroMQ on loopback; MPI transports may bind every interface | listener check on every launch |
| KV sizing | min(fraction of free memory, `max_tokens`) | `max_tokens` from the grant |
| Usage reporting | none found; HF Hub honours `DO_NOT_TRACK` | general convention |
| Embeddings, decisions | no | refused |
| LMCache | prototype connector API | off |

## Decision

### 1. Scope: `tensorrt-llm`, chat, restart-only, one GPU

`tensorrt-llm` joins `vllm`, `sglang`, `tensorfold`, `llamacpp` and `custom` in every closed
engine set. The kind is spelled after the product, not after its command (owner decision
2026-10-27, question 8); the engine's command stays `trtllm-serve`. A deployment runs one
`trtllm-serve` process serving one Hugging Face checkpoint on the PyTorch backend, on one
GPU of one host, for `api: chat`. Its residency is `restart_only`; `deep` or `host_backed`
fails resolution with `capability_missing:deep_park`. The versions qualified in 0.2.0 are
1.2.1 and the 1.3 and 1.4 pre-releases (§2a). Out of scope in 0.2.0: the TensorRT engine
backend and `trtllm-build`, AutoDeploy, multimodal encoders, disaggregated serving, LoRA,
speculative decoding, beam search, sleep in any version, groups and several GPUs (§13).

### 2. Installation and registration

The installation is a Python virtual environment holding the `tensorrt_llm` distribution.

- **The recipe** (recorded in the operations install guide and run by TR1; making the
  environment on host A is the owner's approval of 2026-10-25, the system packages of step
  1 are the owner's approval of 2026-10-27 (question 4), and AGENTS.md gains both entries
  when they are made):
  1. on host A, once: `sudo apt-get -y install libopenmpi-dev` (Ubuntu 24.04's Open MPI
     4.1.6, which mpi4py loads at run time) and the CUDA 13.1 toolkit from NVIDIA's
     packages, as the 1.2.1 installation guide asks; `CUDA_HOME` is the toolkit's
     directory, which TR1 records;
  2. `python3.12 -m venv ~/tensorrt-llm-1.2.1-venv`;
  3. `pip install torch==2.9.1 torchvision --index-url https://download.pytorch.org/whl/cu130`,
     then a constraints file pinning the installed `torch==2.9.1+cu130`;
  4. the wheel by URL with its hash, so nothing else can be substituted:
     `pip install -c constraints.txt "tensorrt_llm @ https://pypi.nvidia.com/tensorrt-llm/tensorrt_llm-1.2.1-cp312-cp312-linux_aarch64.whl#sha256=4c4cf7c00e0e54fe1038ffeb4247b66664d585d054c67810ce04d1c7a018f57a"`,
     with `--extra-index-url https://pypi.nvidia.com/` for its NVIDIA dependencies
     (`tensorrt`, `nvidia-*`).

  The pre-releases' environments follow the same shape with their own guides (§2a).
- **Detect** reads `tensorrt_llm-*.dist-info` under a `site-packages` directory with ADR
  0018 §1's roots and bounds, and executes nothing.
- **Add** resolves the environment lexically and reads the name and version from the
  dist-info. The bounded version check (ADR 0018 §1: 60 s, 4 KiB, cleared environment, own
  process group) runs the environment's interpreter in isolated mode on
  `importlib.metadata.version("tensorrt_llm")`, as for SGLang, and **never imports
  `tensorrt_llm`** or runs `trtllm-serve`: either would initialize MPI and CUDA (§1 of the
  evidence) and, with Open MPI's default, fork a daemon outside the check's process group.
  The same query reads the versions of `torch`, `tensorrt`, `flashinfer-python`,
  `transformers` and `mpi4py`, which `engine list` shows. The entry point is
  `<env>/bin/trtllm-serve`; `build_fingerprint` is the checked version. The default
  profile name is `tensorrt-llm`; the role's own engine is `local_engine.tensorrt-llm`,
  `--tensorrt-llm-bin`, `CAPYCTL_TENSORRT_LLM_BIN`, with the shared precedence.
- **Fingerprint** (ADR 0008): the canonical manifest over the distribution's import
  packages, `tensorrt_llm/` and `triton_kernels/` (its compiled libraries included),
  excluding bytecode, as the other Python engines' package trees. `examples/` is not
  imported by the server and is left out. torch, TensorRT, FlashInfer and the MPI library
  are outside the digest, as vLLM's dependencies are; their versions are recorded instead,
  and a change shows in `engine list`.
- **Verified set** (ADR 0017): `(tensorrt-llm, "1.2.1")` with `torch` `2.9.1+cu130`, once
  TR1–TR11 pass, and one entry for each pre-release, with its own torch build, once its own
  rows pass (§2a). Any other TensorRT-LLM version, or a qualified version beside another
  torch build, registers, lists as `custom` and runs as an uncertified custom setup: it is
  never refused for being uncertified (the owner's rule of 2026-10-27, question 7), and
  `engine list` and status show it uncertified. The safety refusals of §3, §9 and §16 apply
  to it unchanged.
- **Deep park:** the profile is written `security.deep_park: disabled`, and `--deep-park
  enabled` is refused `capability_missing`. Nothing about the toolchain is checked at add;
  the first start's kernel builds are handled as ADR 0014 A12 handles them.

### 2a. Versions: 1.2.1, and the 1.3 and 1.4 pre-releases (owner decision 2026-10-27, question 7)

1.2.1 stays the version this ADR was designed from and the stable one. The owner also
qualifies the 1.3 and 1.4 pre-releases in 0.2.0, each with its own rows; the decision
approves an environment for each on host A.

- **Which builds.** For 1.4 that is 1.4.0rc1, the pre-release this ADR cites. The 1.3
  pre-release was not read; its exact version, wheel URL and SHA-256 are fixed when its
  environment is made and recorded by TR1a, as 1.4's are by TR1b. A later pre-release or
  release of either line is another version: it runs as an uncertified custom setup until
  its own rows pass.
- **Environments.** `~/tensorrt-llm-<version>-venv` on host A, built with §2's recipe
  shape (torch first, a constraints file, the wheel by URL and hash) from the version's own
  installation guide; AGENTS.md gains each entry when it is made.
- **What is known to differ in 1.4** (evidence §8 and the reading behind "Alternatives
  considered"): it needs CUDA 13.4 instead of 13.1 and a different torch, it changes the
  backend option, and it adds `--enable_sleep_mode` with an HMAC-authenticated control
  API. So: the 1.4 environment uses the CUDA 13.4 toolkit, installed beside 13.1 under the
  approval of question 7 [INFERENCE: that 1.4's guide asks for the full toolkit, as
  1.2.1's does, was not read; TR1b records what it needed]; the verified-set entry names
  1.4's torch build as TR1b records it; the backend that §3 renders as `--backend pytorch`
  is rendered in 1.4's spelling, read from its `commands/serve.py` at implementation and
  checked by TR2b; `--enable_sleep_mode` and every option or key of its control API are
  reserved, never rendered, and 1.4 stays `restart_only`, because §8 asks for sleep in a
  stable release with its own ADR and rows.
- **What is not known for 1.3, nor for the rest of 1.4.** Its options and YAML fields, the
  environment variables it reads, its process model and MPI start, `/metrics`, cancellation
  and kernel downloads. The implementation reads each version's tag as it reads 1.2.1's
  (the option, key and environment tables of §3 are kept per qualified version) and the
  version's live rows establish the rest. Where a row shows a difference from 1.2.1's
  design, that version's table or check changes, not 1.2.1's; a difference that breaks a
  rule of §3–§13 (a listener off loopback, a process outside the group, no way to bound
  the pool) fails the row and the version stays uncertified.
- **An uncertified version** is rendered with the tables of the qualified version of the
  same minor line, else 1.2.1's; the readiness checks of §5 (`/version`, `/v1/models`, the
  built pool) and TensorRT-LLM's strict YAML models catch what moved.

### 3. Launch

**The command.** CapyCTL runs

```
<env>/bin/trtllm-serve serve <launch dir>/model/<served name>
    --host 127.0.0.1 --port <leased port> --backend pytorch
    --max_seq_len <context_length> --max_batch_size <slots>
    --tp_size 1 --pp_size 1 --gpus_per_node 1
    --kv_cache_free_gpu_memory_fraction 0.9
    --log_level info
    --config <launch dir>/llm_api_options.yaml
    [--reasoning_parser <p>] [--tool_parser <p>] [--trust_remote_code]
    [ordinary extra arguments]
```

- `serve` is always named, so no model path is read as a subcommand.
- `<launch dir>` is a CapyCTL-owned directory per launch, mode 0700, under the role's
  runtime directory. `model/<served name>` is a symbolic link to the verified checkpoint
  directory, so the served ID TensorRT-LLM lists is the name CapyCTL routes (evidence §2).
  A served name that is not one path component (empty, `.`, `..`, containing `/` or NUL,
  or longer than 255 bytes) is refused at resolution, `served_name_unsupported` (owner
  decision 2026-10-27, question 3).
- `<slots>` is `max_concurrent_requests`, 32 when undeclared (owner decision 2026-10-27,
  question 5).
- `--backend pytorch` is 1.2.1's spelling; each pre-release renders the spelling read from
  its own tag (§2a).
- The fraction is rendered explicitly so no engine default can move; on its own it is not
  the pool's size (§4).

**The configuration file CapyCTL writes**, `llm_api_options.yaml` (mode 0600), and the
only one the engine reads. Because the file overrides the command line, CapyCTL owns it
whole:

```yaml
kv_cache_config:
  max_tokens: <from the grant, §4>
  free_gpu_memory_fraction: 0.9
  dtype: <kv_cache_dtype, or auto>
enable_iter_perf_stats: true
iter_stats_max_iterations: 1000
return_perf_metrics: false
enable_sleep: false
dtype: <dtype, or auto>
# cuda_graph_config: null       when cuda_graphs: false
# <the deployment's llm_api_options, merged under the reserved keys>
```

A deployment may add LLM API arguments as `engine_config.tensorrt-llm.llm_api_options`, a
mapping (owner decision 2026-10-27, question 6; the key follows the kind's name, question
8). CapyCTL merges it under its own keys and refuses, at deploy time and again when it
renders the file, a mapping that names a reserved key:

- **Reserved, rendered by CapyCTL:** `model`, `backend`, `max_seq_len`, `max_batch_size`,
  `tensor_parallel_size`, `pipeline_parallel_size`, `gpus_per_node`,
  `kv_cache_config.max_tokens`, `.free_gpu_memory_fraction`, `.dtype` (typed),
  `.host_cache_size` (never set: no engine-owned host cache, SPEC §12),
  `enable_iter_perf_stats`, `enable_iter_req_stats`, `iter_stats_max_iterations`,
  `return_perf_metrics`, `perf_metrics_max_requests`, `enable_sleep`, `dtype` (typed),
  `trust_remote_code` (typed, host approval).
- **Reserved, never rendered:** `context_parallel_size`, `moe_expert_parallel_size`,
  `moe_cluster_parallel_size`, `max_beam_width` (CapyCTL's chat contract has no beam
  search, and beams multiply a request's KV), `tokenizer`, `custom_tokenizer`, `revision`,
  `skip_tokenizer_init`, `load_format` (`dummy` serves random weights), `checkpoint_format`
  and `checkpoint_loader`, `orchestrator_type` and every `ray_*` field, `kv_connector_config`
  (§12), `cache_transceiver_config` and every disaggregation field, `otlp_traces_endpoint`
  (until ADR 0037 is decided), `num_postprocess_workers` and `postprocess_tokenizer_dir`,
  `mm_encoder_only`, `enable_attention_dp`, `speculative_config`, `lora_config` and
  `peft_cache_config` (out of scope, §1).
- **Ordinary:** everything else, such as `enable_chunked_prefill`, `max_num_tokens`,
  `kv_cache_config.enable_block_reuse`, `cuda_graph_config` (when `cuda_graphs` is
  undeclared), `scheduler_config`, `sampler_type`, `attn_backend`, `moe_config`,
  `guided_decoding_backend`, `torch_compile_config`, `enable_autotuner`, `stream_interval`.
  TensorRT-LLM's strict models refuse unknown keys at start.

**Command-line options** (ADR 0014 §6). Extra and host-fixed arguments are compared by
exact name after splitting a `--name=value` spelling, covering every alias:

- **Reserved, rendered:** the positional model, `--host`, `--port`, `--backend`,
  `--max_seq_len`, `--max_batch_size`, `--tensor_parallel_size`/`--tp_size`,
  `--pipeline_parallel_size`/`--pp_size`, `--gpus_per_node`,
  `--free_gpu_memory_fraction`/`--kv_cache_free_gpu_memory_fraction`,
  `--config`/`--extra_llm_api_options`, `--log_level`, `--trust_remote_code`,
  `--reasoning_parser`, `--tool_parser`.
- **Reserved, never rendered:** `--context_parallel_size`/`--cp_size`,
  `--moe_expert_parallel_size`/`--ep_size`, `--moe_cluster_parallel_size`/`--cluster_size`,
  `--max_beam_width`, `--tokenizer`, `--custom_tokenizer` and `--custom_module_dirs` (both
  import code), `--revision`, `--metadata_server_config_file`, `--server_role`,
  `--disagg_cluster_uri`, `--otlp_traces_endpoint`, `--num_postprocess_workers`, and every
  subcommand other than `serve`.
- **Sensitive, host approval by name (ADR 0014 §8):** `--chat_template`, with a file value
  inside `security.approved_paths`.
- **Ordinary:** `--max_num_tokens`, `--enable_chunked_prefill`,
  `--fail_fast_on_attention_window_too_large`, `--media_io_kwargs`.

A typed field and its native spelling together are a duplicate and refused. The common
field `quantization` is refused on a TensorRT-LLM profile (the checkpoint carries it);
`language_model_only` is refused (no switch in 1.2.1).

**The closed environment** (SPEC §13.3): `PATH` (the environment's `bin` first, then the
fixed system directories), `HOME`, `CUDA_VISIBLE_DEVICES` (ADR 0019 §6), `CUDA_HOME` (the
toolkit TR1 recorded for the version: 13.1 for 1.2.1, owner decision 2026-10-27, question
4), `TMPDIR=<launch dir>/tmp` (so the stats socket and every temporary file are private to
the launch), `HF_HUB_OFFLINE=1` and `TRANSFORMERS_OFFLINE=1` (the checkpoint is local),
`TRTLLM_RESPONSES_API_DISABLE_STORE=1` (the engine keeps no conversation history, §10),
`TLLM_WORKER_USE_SINGLE_PROCESS=1` and the MPI settings of §9,
`DO_NOT_TRACK=1` under the host's `engine_telemetry: off` (§10), and the log metadata. A
profile or deployment environment may not name `TLLM_*`, `TRTLLM_*`, `OMPI_*`, `PMIX_*`,
`PRTE_*`, `UCX_*`, `NCCL_*`, `MPI4PY_*`, `HF_*`, `TRANSFORMERS_*`, `FLASHINFER_*`,
`CUDA_VISIBLE_DEVICES`, `TMPDIR` or `HOME`, even when `approved_env` lists it
(`engine_env_reserved:<name>`); the implementation reads every environment lookup of the
tag, as ADR 0029 §6 did, and adds any other name that moves a listener, a device, a
directory or a reserved value.

### 4. Memory: the KV cache from the grant

- **Request.** ADR 0014 §5 unchanged: weights (the checkpoint's weight files), plus the KV
  cache, plus the family margin. Until TR3 measures TensorRT-LLM, the margin is the
  unified-memory placeholder `max(8 GiB, weights × 0.15 + 4 GiB)` (A18) and, on a discrete
  GPU, the weights × 0.10 (ADR 0019 §3). The startup placeholder (A2, A8) holds the
  weights, the final KV cache and the profiling run's small cache and activations, which
  TensorRT-LLM allocates before it builds the final pool (evidence §7); TR3 replaces it
  with the measured peak.
- **KV tokens.** `memory.kv_cache` (declared, or derived as request − weights − margin) is
  rendered as `kv_cache_config.max_tokens = floor(kv_bytes / bytes_per_token)`, with
  `bytes_per_token = 2 × num_hidden_layers × num_key_value_heads × head_dim ×
  bytes(kv dtype)` from the checkpoint's `config.json` (`head_dim` else `hidden_size /
  num_attention_heads`). For Qwen3-4B in BF16 that is 2 × 36 × 8 × 128 × 2 = 147,456 bytes,
  so a 4 GiB cache renders `max_tokens: 29127` (arithmetic, not measured).
- **The fraction is only a ceiling.** Rendered at 0.9, it can only make the pool smaller
  than `max_tokens`, when the machine is short at start (evidence §7). It can never make it
  larger, so the grant holds whatever the other tenants hold.
- **Geometry CapyCTL cannot compute** is refused at resolution, `kv_geometry_unsupported`:
  per-layer or sliding attention windows (`sliding_window`, `layer_types`,
  `max_window_layers`), multi-head latent attention (`kv_lora_rank`), recurrent or hybrid
  layers, and encoder-decoder models. Each can be added with its own formula and row.
- **Checked after start** (§5): the pool the engine built, read from the readiness
  probe's iteration entry as `kvCacheStats.maxNumBlocks × tokensPerBlock`, must hold at
  least one request's window (`max_seq_len`), else the launch is stopped and fails
  `kv_cache_short`, naming both figures; and must not exceed `max_tokens` by more than one
  block, else `effective_args_mismatch`. A pool between the two is accepted and status
  shows it beside the rendered figure.
- **Readings.** CapyCTL never uses the engine's `gpuMemUsage` (evidence §4). On GB10 the
  instance's use is the per-process reading of its processes, which the driver lists
  there ("per-process GPU memory is listed", DGX Spark known issues).

### 5. Readiness

Ready is, in order, each bounded by `timeouts.initialize` (ADR 0014 A1 and A7 until TR3
measures TensorRT-LLM's start): the port accepts a connection; `/health` answers 200;
`/version` reports the profile's version, else `installation_drift`; `/v1/models` lists
exactly the served name, else `effective_args_mismatch`; then the existing bounded chat
probe ("Say ready.", 8 tokens, temperature 0), where non-empty `content` or
`reasoning_content` is an answer; then one `/metrics` read, which must return at least one
iteration entry (proving iteration statistics are on) and pass §4's KV check; then §9's
process and listener checks. `/health_generate` is not used: it is a one-token generation
through the same handler the probe already exercises.

### 6. Metrics and load

The host is the only reader of `/metrics` (a read consumes it). It reads at its load
sampling interval and keeps the newest entry:

- **Load reports:** running is the newest entry's `numActiveRequests` and waiting its
  `numQueuedRequests`, from a read that returned entries; a read that returned none
  reports both as unknown, never 0. KV usage is `usedNumBlocks / maxNumBlocks`.
- **Never idle evidence.** No entry reads 0 and 0, and an empty read is ambiguous
  (evidence §4), so `/metrics` is not an idle signal (§7). Routing and admission keep
  using CapyCTL's own lease counts.
- **Per-request figures** (`capyctl.metrics`, SPEC §17): `cached_tokens` from
  `usage.prompt_tokens_details.cached_tokens`, the completion count from `usage`; the
  engine gives no time to first token, prefill time or queue time, so `ttft_ms` is the
  router's and the others are absent.
- **`/prometheus/metrics` stays off** (`return_perf_metrics: false`): its counter misses
  cancelled requests and its histograms repeat what the router measures; turning it on
  also makes the engine keep per-request metrics in memory.

### 7. Cancellation and settlement

The router's behaviour on a cut is today's: a client hang-up, a request deadline cut, a
stream idle cut or an engine error answer closes the engine connection, and the lease
becomes `cancelling`. TensorRT-LLM notices the closed connection within about a second and
queues an abort that its scheduler applies at its next iteration (evidence §5).

TensorRT-LLM 1.2.1 has **no idle signal** CapyCTL accepts and **no per-request
acknowledgement**, so ADR 0031 §6's rule for engines without an idle signal applies
unchanged:

1. A cancelling lease on a TensorRT-LLM instance is never settled while the engine runs. It
   closes on controlled cleanup (SPEC §10): the verified exit of the instance's process
   group at its next stop, idle stop, switch release or restart, with
   `request_cancellation_acknowledged` journaled and a receipt naming that stop.
2. Draining does not wait for cancelling leases; it waits for leases still being answered,
   then stops. Memory is never freed before the verified exit.
3. When such requests alone fill the deployment's limit of 32, ADR 0032 item 7's restart
   runs (`instance_restarted_for_held_requests`).
4. Status shows `cancelling: N (settles at the next stop or restart: the engine reports no
   idle signal)`, and `engine list` shows `idle signal: none` for the profile.

Why nothing weaker is accepted: the engine's abort log line and the closed socket prove
dispatch of an abort, not its effect (ADR 0032 §5); `/metrics` cannot show 0 running and 0
waiting (evidence §4); the Prometheus counter never sees a cut stream. The owner kept this
for 0.2.0 (owner decision 2026-10-27, question 2); TR5 records the quiet-interval data a
later amendment would need before such a reading could become an idle signal.

### 8. Parking

`restart_only`: parking is the stop with verified exit, and waking is a fresh launch of
the same pinned effective contract (ADR 0029 §10); `capyctl park deployment` uses the
restart-only release. `enable_sleep` is rendered `false` and reserved; the RL routes are
never called. The ADR 0012 protections are not required, since there is no engine control
path; the engine listens on loopback without a key (§9), the known limitation of llama.cpp
and TensorFold, shown as `engine key: none`.

**What would qualify a parking tier later.** A deep-park ADR for TensorRT-LLM needs, in a
stable release: sleep and wake on `trtllm-serve` itself (not only `AsyncLLM`) that answer
only after the memory is released or restored, engine-wide; a key per launch on those
routes (1.4.0rc1's HMAC control key is a candidate), behind CapyCTL's key guard on
loopback; a release that discards weights and KV rather than copying weights to host
memory, since on GB10 a host copy frees nothing (SPEC §7.2), with the wake reloading from
the verified checkpoint; a refusal while requests are active, or a defined drain; and live
rows on host A: five park and wake cycles with the measured residue per cycle and no growth
(ADR 0014 A13, A19), the wake canary matching, and a cancelled request across a park. A
host copy could still qualify `host_backed` on a discrete GPU, with its own rows.

### 9. Ownership, MPI and listeners

- **One process** (owner decision 2026-10-27, question 1). Every launch sets
  `TLLM_WORKER_USE_SINGLE_PROCESS=1`, so the model runs in the `trtllm-serve` process
  (evidence §6), and pins the MPI runtime to a single local process. For Open MPI that is
  `OMPI_MCA_ess_singleton_isolated=1` (no daemon, `ess_singleton_module.c:163-166`) and, as
  proposed, `OMPI_MCA_btl=self` with `OMPI_MCA_pml=ob1` (no network transport)
  [INFERENCE: the transport settings are read from Open MPI's parameters, not run; TR1 and
  TR9 confirm them]. The expected library is Ubuntu's Open MPI 4.1.6, installed by owner
  decision 2026-10-27, question 4; the exact set follows TR1's record of the library mpi4py
  loads (TR1a's and TR1b's for the pre-releases). A launch whose library is not the one its
  version's TR1 qualified still starts, and the checks below decide. The process is `api`
  (ADR 0027); compilers and compile workers that JIT kernels are helpers.
- **No process outside the group.** At readiness and at every re-proof the host lists the
  leader's descendants (`/proc/<pid>/stat` parent links). Any descendant whose process
  group differs from the launch's (as an Open MPI daemon started with `--set-sid` would)
  stops the launch, with gone evidence required for the group and for that process, and
  fails it `engine_process_escaped`, naming the process. A group stop cannot prove such a
  process gone, so it is never accepted.
- **Listeners.** ADR 0031 §7's check runs on **every** TensorRT-LLM launch, not only with
  several GPUs as ADR 0035 §5 runs it for vLLM and SGLang: once readiness passes and
  whenever a host re-proves the launch, a TCP listener of an owned process on any address
  other than `127.0.0.1` or `::1` stops the launch and fails it `engine_listener_exposed`.
  Expected: the API on `127.0.0.1:<port>`. As in ADR 0031, a listener opened and closed
  before the check, or opened later, is not seen.
- **Loopback and local users.** The API has no key; any local process can reach it,
  including routes CapyCTL never uses (`/v1/completions`, `/v1/responses`,
  `/kv_cache_events`, `/steady_clock_offset`). SPEC §9.1's line holds: controls never leave
  loopback, and local users are not a boundary CapyCTL claims.

### 10. Usage reporting, outbound connections and data

TensorRT-LLM has no usage reporting of its own (evidence §9), so it takes the general
convention of #188 (`engine_telemetry::DO_NOT_TRACK`): `DO_NOT_TRACK=1` under the host's
`engine_telemetry: off`, which also covers the Hugging Face Hub it imports; nothing under
`allow`. `HF_HUB_OFFLINE` and `TRANSFORMERS_OFFLINE` are set under both, because the
checkpoint is always local. FlashInfer's kernel downloads are allowed and documented in
0.2.0 (owner decision 2026-10-27, question 9); TR8 records whether a start reaches them.
The data-retention page gains: the engine keeps no conversation history
(`TRTLLM_RESPONSES_API_DISABLE_STORE=1`); its KV block reuse is partitioned by the
effective salt.

### 11. APIs

- **Chat** only, through the existing router surface. `cache_salt` follows ADR 0033 §5: the
  router sends the effective salt, which TensorRT-LLM uses to partition block reuse.
  Responses pass through unchanged.
- **Decisions:** `api: decisions` is refused `capability_missing:decisions` (ADR 0030 §6).
- **Embeddings:** refused `capability_missing:embeddings` (no route; ADR 0036 when it lands).

### 12. LMCache: off

ADR 0033 §2's table gains a row: TensorRT-LLM, no, `engine_unsupported`: "LMCache is not
qualified with TensorRT-LLM". Under ADR 0034's default an `auto` deployment shows LMCache
off with that reason; `lmcache: required` is refused. `kv_connector_config` stays
reserved. Turning it on needs LMCache's TensorRT-LLM connector in multiprocess mode
rendered into CapyCTL's YAML, the effective salt reaching LMCache, and its own LC rows.

### 13. Several GPUs and groups: refused

Tensor parallelism needs the MPI pool, one spawned worker per rank (evidence §6), which is
exactly the process model §9 refuses. So TensorRT-LLM has no ADR 0035 §10 support entry: a
deployment with `topology` on one host is refused `multi_gpu_unsupported` (exit 5), with
the reason "TensorRT-LLM runs on one GPU in this version"; `topology: auto` resolves N = 1.
It has no ADR 0028 §10 group entry either, so a group is refused as for any engine without
one. Qualifying `--tp_size` later needs a launcher that keeps every rank in the launch's
process group (CapyCTL running `mpirun` itself, or Open MPI 5's daemonless spawn), every
MPI and NCCL transport pinned to loopback with the listener check passing, per-GPU KV
sizing, and the ADR 0035 §14 rental session.

### 14. Parsers

ADR 0024's family table gains a TensorRT-LLM column: Qwen3 renders `--reasoning_parser
qwen3 --tool_parser qwen3`; Qwen3-Coder `--tool_parser qwen3_coder`; DeepSeek-R1
`--reasoning_parser deepseek-r1`; Nemotron Nano 3 `--reasoning_parser nano-v3`; DeepSeek
V3, V3.1 and V3.2 and Kimi K2 their tool parsers. The typed `tool_call_parser` and
`reasoning_parser` fields override the family default with TensorRT-LLM's names.

### 15. GB10 specifics, together

- One `system` domain: weights, KV, activations, CUDA graphs and the process's host memory
  are charged once (SPEC §7.2).
- The pool is bound by `max_tokens`, because TensorRT-LLM's own measure counts other
  tenants and under-reports reclaimable memory (§4).
- Loading reads safetensors through the page cache while the copy lands in the same pool;
  the startup peak is measured by TR3, not assumed.
- BF16 is what TR rows qualify. FP8 and NVFP4 checkpoints run unqualified until a row
  shows their kernels on SM 12.1.
- A CPU copy of weights is not capacity (§8).

### 16. Codes

| Where | Code | When |
|---|---|---|
| resolution | `capability_missing:deep_park` | `deep` or `host_backed` |
| resolution | `capability_missing:decisions`, `capability_missing:embeddings` | §11 |
| resolution | `multi_gpu_unsupported` (existing) | `topology` (§13) |
| resolution | `served_name_unsupported` (new) | a served name that is not one path component |
| resolution | `kv_geometry_unsupported` (new) | §4 |
| deploy | reserved option or key (existing ADR 0014 §6 refusal) | §3 |
| deploy | `engine_unsupported` (LMCache, existing) | `lmcache: required` |
| launch | `engine_env_reserved:<name>` (existing) | §3 |
| launch | `installation_drift`, `effective_args_mismatch` (existing) | §5, §4 |
| launch | `kv_cache_short` (new) | the built pool holds less than one window |
| launch | `engine_process_escaped` (new) | a descendant outside the process group |
| launch | `engine_listener_exposed` (ADR 0035) | a listener off loopback |

### 17. Texts that change with the implementation

- **SPEC** §1.1 R01: "... llama.cpp's `llama-server` fourth (ADR 0029), TensorRT-LLM's
  `trtllm-serve` (ADR 0038, kind `tensorrt-llm`), restart-only on one GPU." §9.4: a note
  that TensorRT-LLM begins restart-only (its 1.2.1 sleep is a prototype the server cannot
  reach, and the 1.4 pre-release's sleep is not qualified), reports no idle signal (ADR
  0031 §6 settlement), and runs one process with its MPI runtime pinned. §20: T49 and the
  live rows TR1–TR11, TR1a–TR11a and TR1b–TR11b.
- **ADR 0018** §1, **ADR 0033** §2, **ADR 0035** §10: the amendments in the header.
- **Guide**: `engines.md` and `install-engines.md` (the recipe with its system packages,
  the three qualified versions and the verified set, uncertified versions running as
  custom setups, `engine key: none`, no idle signal, one GPU, FlashInfer's kernel
  downloads), `engine-flags.md` (the option and key tables), `parking.md` (restart only),
  `errors.md` (the new codes).
- **Operations**: `install.md` (`libopenmpi-dev`, the CUDA toolkits and the environments),
  `configuration.md` (`engine_config.tensorrt-llm.llm_api_options`,
  `local_engine.tensorrt-llm`), `data-retention.md` (§10).

## Consequences

- A sixth engine kind in every closed set, a Python installation path whose version check
  must never import the engine, a CapyCTL-written engine configuration file, and two new
  per-launch checks (descendants in the group, listeners) on the built-in adapter.
- TensorRT-LLM deployments settle cut requests only at a stop. Under many hang-ups the
  deployment restarts when they fill its limit; each restart costs a full TensorRT-LLM
  start (two executor starts and, on a first start, kernel builds), which TR3 measures.
- Running the worker in the server process may cost streaming throughput (NVIDIA's own
  warning); TR10 measures it. In exchange the process tree is one process CapyCTL owns.
- The pool can be smaller than granted on a crowded machine, never larger; status shows
  the difference.
- Host A gains, per qualified version, a 2.6 GB engine package (1.2.1's size; the
  pre-releases' are recorded by their TR1 rows) plus torch and TensorRT in its own
  environment, three environments in all; and system-wide `libopenmpi-dev` and the CUDA
  13.1 toolkit (owner decision 2026-10-27, question 4), plus the CUDA 13.4 toolkit for 1.4
  (§2a). It is the first engine whose installation depends on an MPI library and on system
  packages.
- Three versions are qualified, each by its own eleven rows, so a live session for
  TensorRT-LLM runs 33 rows; the option, key and environment tables are kept per version.
- Out of scope: deep and host-backed residency, several GPUs, groups, the TensorRT engine
  backend, LoRA, speculative decoding, embeddings, decisions, LMCache, disaggregated
  serving.

## Alternatives considered

- **A custom engine (ADR 0031) instead of an integration.** Possible today, but CapyCTL
  could not render the KV pool from the grant, own the YAML that overrides the command
  line, pin MPI or name the served model; the owner chose a native engine.
- **The NGC container** (`nvcr.io/nvidia/tensorrt-llm/release:1.2.1`), NVIDIA's documented
  easy path. CapyCTL launches native installations; container launchers are a later
  delivery (SPEC R03). Rejected for 0.2.0.
- **The default MPI pool.** Matches NVIDIA's streaming advice, but with Open MPI 4.1.6 the
  daemon leaves the process group and its transports bind every interface (evidence §6),
  so a stop could not prove the GPU-holding process gone. Rejected (owner decision
  2026-10-27, question 1).
- **Sizing by `--kv_cache_free_gpu_memory_fraction` alone**, computing a fraction from the
  grant. On GB10 the fraction applies to what the machine has free at start, other tenants
  included, so the same fraction gives different pools; `max_tokens` gives the granted one.
- **Letting the deployment pass its own `--config` file.** It overrides every reserved
  value CapyCTL renders, and CapyCTL could check it only by re-implementing the merge.
  CapyCTL writes the file and merges the deployment's mapping into it instead.
- **An idle signal from `/metrics`**, or from the Prometheus counter compared with CapyCTL's
  dispatch count. The first never reads 0 and 0; the second misses cancelled streams and
  counts other local clients' requests. Owner decision 2026-10-27, question 2 keeps the
  quiet-interval reading for later.
- **A protected Python entry** (as `runtime/vllm_entry.py`) that parses the command line and
  YAML with the installed parser and checks the reserved values before serving. Stronger
  against hidden defaults, but it imports TensorRT-LLM in a CapyCTL script and must track
  `commands/serve.py`'s internals. The readiness checks of §5 (`/version`, `/v1/models`,
  the built KV pool) verify the values that matter after the engine's own parsing.
- **1.4.0rc1 instead of 1.2.1**, for its sleep API. A pre-release with a different CUDA
  (13.4) and torch; the owner kept the stable release as the base and qualifies 1.4 beside
  it, restart-only (§2a).
- **Only 1.2.1 qualified, every other version custom** (the proposal's recommendation for
  question 7). Overruled by owner decision 2026-10-27, question 7: the 1.3 and 1.4
  pre-releases get their own rows in 0.2.0.
- **Refusing versions other than the qualified ones.** Rejected by the owner's rule of
  2026-10-27: an uncertified version runs as a custom setup and is never refused for being
  uncertified.
- **The MPI library inside the environment, and no system toolkit** (the proposal's
  recommendation for question 4, PyPI's `openmpi` or `mpich` wheel and the CUDA runtime
  from the wheels). Overruled by owner decision 2026-10-27, question 4: NVIDIA's documented
  prerequisites are installed on host A instead.
- **`trtllm` as the kind name** (the proposal's recommendation for question 8), matching
  the engine's commands. Overruled by owner decision 2026-10-27, question 8:
  `tensorrt-llm`.

## Implementation plan (target 0.2.0)

1. **Configuration** (`capyctl-config`): the `tensorrt-llm` kind in every closed set and in
   `engine_policy.rs`; `engine_config.tensorrt-llm` (`llm_api_options`); the option, key
   and environment policies of §3, per qualified version (§2a); the KV token derivation and
   the geometry refusal (§4); the served-name check; the refusals of §1, §11–§13; the ADR
   0024 column.
2. **Registration** (`capyctl-agent` engines, `capyctl-cli` engine): detection of
   `tensorrt_llm-*.dist-info`; the metadata-only version check with the companion versions;
   the fingerprint over `tensorrt_llm/` and `triton_kernels/`; the verified set's entry per
   qualified version and the uncertified marking (§2); `engine list` showing companions,
   `idle signal: none` and `engine key: none`.
3. **Adapter** (`capyctl-adapters/src/tensorrt_llm/`, the Rust module spelling of the
   kind): rendering of the command per qualified version, the launch directory, the
   served-name link and the YAML; the closed environment with the MPI pins; readiness (§5);
   the `/metrics` reader and load mapping (§6); `cancel_work` answering `Uncertain` and the
   adapter declaring no idle signal, so ADR 0031 §6's path applies; stop by SIGTERM to the
   group, then the bound, then SIGKILL, then verified exit.
4. **Ownership** (`capyctl-launchers`, `capyctl-adapters`): the descendant check
   (`engine_process_escaped`) and the listener check (`custom/listeners.rs`, reused) on
   every TensorRT-LLM launch and re-proof.
5. **Memory** (`capyctl-config::derive_resources`): placeholders of §4, replaced by TR3's
   measurements in the same release.
6. **Docs:** §17's texts and one change file.

Tests, tagged T49 (with T14, T17, T21 and T37 where they apply), against stub engines:
detection executes nothing; `add` never imports the package (the check's command line is
asserted); version and companion parsing; every reserved option, alias and `=` spelling
refused, and every reserved YAML key, nested ones included; the rendered command and YAML
for a minimal deployment byte for byte, for each qualified version's tables (1.4's
`--enable_sleep_mode` refused); an unlisted version registered as `custom`, shown
uncertified and launched, never refused for its version; `max_tokens` from the grant for
Qwen3-4B's geometry and the geometry refusals; the served-name link and refusals; readiness
failing on each step, including a `/metrics` read with no entry and a built pool below one
window or above `max_tokens`; an empty `/metrics` read reported unknown, never idle; a
cancelling lease never settled while the stub runs and settled by the verified stop; the
restart when held requests fill the limit; a descendant outside the group and a listener
off loopback each failing the launch; `DO_NOT_TRACK` under `off` only and the offline
switches under both; the refusals of deep residency, decisions, embeddings, `topology` and
`lmcache: required`. CPU and Fake-engine tests are not qualification.

## Live qualification (host A, TensorRT-LLM 1.2.1 and the 1.3 and 1.4 pre-releases)

Run in a live session the owner schedules (one live session on the hosts at a time), in
`~/tensorrt-llm-1.2.1-venv` once it exists, with Qwen3-4B in BF16 from the host's model
store. Signals go only to CapyCTL-owned processes. The rows below qualify 1.2.1; the
pre-releases' rows follow the table.

| Row | What | Passes when |
|---|---|---|
| TR1 | Environment and registration: `libopenmpi-dev` and the CUDA 13.1 toolkit installed on host A (owner decision 2026-10-27, question 4); the environment made by §2's recipe; `engine detect`; `engine add`; then, by hand, `python -c "from mpi4py import MPI; print(MPI.Get_library_version())"` | the wheel's hash matches; torch is `2.9.1+cu130`; detect lists it and executes nothing; add records 1.2.1, the digest and the companion versions, and no `orted` or CUDA context appears during add; mpi4py loads Ubuntu's Open MPI 4.1.6, else the library it loads is recorded and §9's MPI settings are fixed from it before TR2; the CUDA toolkit's version and `CUDA_HOME` are recorded |
| TR2 | Start and readiness: a Qwen3-4B deployment with `memory.kv_cache: 4GiB` | the rendered command and YAML match the plan; readiness passes each step of §5; `/v1/models` lists the served name; `/version` is 1.2.1; a chat with a tool and with reasoning returns `tool_calls` and `reasoning_content` through the Qwen3 parsers |
| TR3 | Memory and start time: cold start (empty kernel caches) and three warm starts, then 30 minutes of mixed traffic | the per-process peak during start, the steady figure, the weights as loaded and the built pool are recorded; peak minus weights minus KV sets the margin and the startup figure (§4); the built pool equals `max_tokens` rounded to blocks; time to ready is recorded for both start kinds and sets the derived Initialize window; repeated with a 20 GiB resident allocation beside it, the pool is unchanged |
| TR4 | Metrics: a burst of concurrent streams, then a lull | load reports show running, waiting and KV usage during the burst; reads during the lull return no entries and report unknown; a second reader is never started |
| TR5 | Cancellation: a client hang-up mid-stream under steady traffic; a deadline cut before output; then 32 requests cut before output on an otherwise idle deployment | the engine logs the abort within about a second and its next entries show the request gone; the leases stay `cancelling` and status says they settle at the next stop; a stop does not wait for them and its verified exit settles them; the 32 trigger one restart, journaled, whose exit settles all 32. Also recorded, as data for a later idle-signal amendment (owner decision 2026-10-27, question 2): across the run, every interval of two empty `/metrics` reads at least 2 s apart, and whether any request was active in it |
| TR6 | Park and wake, restart only: five idle parks and wakes | each park is a stop with verified exit and the instance's per-process memory reads gone; each wake is a fresh launch that passes readiness; `deep` is refused `capability_missing:deep_park` |
| TR7 | Refusals: `deep`; `api: decisions`; `api: embeddings`; `topology: {tensor_parallel: 2}`; `lmcache: required`; extra arguments `--host`, `--config=x.yaml`, `--tp_size 2`, `--kv_cache_free_gpu_memory_fraction 0.5`, `--custom_module_dirs`; `llm_api_options` with `enable_sleep`, `orchestrator_type`, `kv_connector_config`, `kv_cache_config.max_tokens`; a served name with `/` | each is refused before anything starts, with its code |
| TR8 | Usage reporting and outbound connections: a cold start and a run under `engine_telemetry: off`, the engine's sockets sampled every second | the environment has `DO_NOT_TRACK=1` and the offline switches; any outbound connection of an owned process is recorded with its destination (FlashInfer's kernel host or none), and the guide's note on kernel downloads (owner decision 2026-10-27, question 9) states what was found |
| TR9 | Ownership and listeners: during TR2 and after a SIGKILL of the engine process | the launch is one process (helpers only while compiling), no descendant outside the group, the only listener is `127.0.0.1:<port>` (the check and `ss -ltnp`); after the SIGKILL the instance fails, nothing is released before gone evidence, and the next start serves |
| TR10 | Single process versus the MPI pool: decode tokens per second and time to first token at 1, 8 and 32 concurrent streams under CapyCTL, then the same command without the single-process switch run by hand outside CapyCTL | both throughputs are recorded; the by-hand run's process tree, process groups and listeners are recorded (whether an `orted` with its own session and listeners on every interface appears), as evidence for the process model chosen by owner decision 2026-10-27, question 1 |
| TR11 | Enrolled host: host A as a host of the control-plane server | TR2's deployment serves through the host's ingress, and a hang-up there settles as in TR5 |

A pass adds `(tensorrt-llm, "1.2.1")` with `torch` `2.9.1+cu130` to the verified set and
records the measured margin, startup figure and Initialize window. A discrete-GPU row, and
any model other than Qwen3-4B BF16, run as uncertified setups until their own rows.

### Rows for the pre-releases (owner decision 2026-10-27, question 7)

Rows TR1a–TR11a qualify the 1.3 pre-release and TR1b–TR11b the 1.4 pre-release (1.4.0rc1).
Each is the row of the same number above, run in that version's own environment
(`~/tensorrt-llm-<version>-venv`, §2a) with the same model and deployment, and passes on
the same terms with the version's own figures, except:

- **TR1a, TR1b:** the environment follows the version's installation guide; the wheel's
  URL and hash, the torch build, the CUDA toolkit (13.4 for 1.4, installed beside 13.1) and
  the MPI library mpi4py loads are recorded, and the version's MPI settings (§9) are fixed
  from them; add records the version.
- **TR2a, TR2b:** `/version` reports the version; the rendered command and YAML match the
  version's tables, read from its tag at implementation; for 1.4 that includes the backend
  in its own spelling.
- **TR3a, TR3b:** the margin, startup figure and Initialize window are the version's own,
  not 1.2.1's.
- **TR6b, TR7b:** 1.4 is restart-only like the others; TR7b also refuses
  `--enable_sleep_mode` and the options and keys of its control API.
- **TR10a, TR10b:** the by-hand MPI pool run records whether the version's process model and
  MPI start still match evidence §6; a difference is recorded for its §9 settings, and the
  single-process launch must still pass TR9a or TR9b.

A version whose eleven rows pass adds `(tensorrt-llm, "<version>")` with its torch build to
the verified set and records its own margin, startup figure and Initialize window. Until
then, and if any row fails, the version runs as an uncertified custom setup; it is not
refused.

## Owner decisions of 2026-10-27

The proposal of 2026-10-25 asked nine questions; the owner answered all of them on
2026-10-27. The decision sections above already read as decided. Throughout, the owner's
rule of 2026-10-27 applies: certified engine, version and checkpoint combinations are
advertised as certified; anything else runs as an uncertified custom setup and is never
refused for being uncertified. Safety refusals (reserved options and keys, loopback-only
controls, malformed requests) are unaffected.

1. **Process model:** one process, as proposed: `TLLM_WORKER_USE_SINGLE_PROCESS=1` with
   MPI pinned to an isolated singleton (§9). TR10 measures what it costs.
2. **Idle signal:** none in 0.2.0, as proposed. Cut requests settle at the next verified
   stop, with ADR 0032's restart when they fill a deployment (§7); TR5 records the
   quiet-interval data for a later amendment.
3. **Served name:** a per-launch symbolic link named after the served name, as proposed;
   names that are not one path component are refused `served_name_unsupported` (§3).
4. **MPI library and CUDA toolkit on host A (changed from the recommendation):** the owner
   approves installing `libopenmpi-dev` with apt and the CUDA 13.1 toolkit on host A. The
   recipe installs them first (§2), `CUDA_HOME` names the toolkit (§3), §9's MPI settings
   expect Ubuntu's Open MPI 4.1.6, and TR1 installs and records both.
5. **Default concurrency:** `max_batch_size` 32 when `max_concurrent_requests` is
   undeclared, as proposed (§3).
6. **LLM API options:** a deployment mapping merged into CapyCTL's file with the reserved
   keys refused, as proposed (§3). Under decision 8 its key is
   `engine_config.tensorrt-llm.llm_api_options`.
7. **Other versions (changed from the recommendation):** the 1.3 and 1.4 pre-releases are
   also qualified in 0.2.0, each with its own rows (TR1a–TR11a, TR1b–TR11b) and its own
   environment on host A, which this decision approves (§2a). Every other version runs as
   an uncertified custom setup under the owner's rule (§2); none is refused for its
   version.
8. **Kind name (changed from the recommendation):** `tensorrt-llm`. The kind, the default
   profile name, `local_engine.tensorrt-llm`, `--tensorrt-llm-bin`,
   `CAPYCTL_TENSORRT_LLM_BIN`, `engine_config.tensorrt-llm` and the verified-set entries use
   it (§1, §2, §3); the engine's own command stays `trtllm-serve`.
9. **Kernel downloads at run time:** allowed and documented, as proposed; TR8 records
   whether a start reaches FlashInfer's kernel host (§10).

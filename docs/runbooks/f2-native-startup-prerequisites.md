# F2 native startup prerequisites

Status: native startup remains closed. The protected candidate handoff is implemented
through `49bed60` and its consolidated review has no remaining Critical or Important
findings. This does not complete F2B or qualify a native engine.

## Remaining work

- The import-safe `sglang_server_args` mapper now checks 100 explicit native
  fields plus private/dynamic inputs and resolved graph backends after a guarded
  constructor call. Wire it only after the constructor's plugin, environment,
  logging, model/config, and GPU-discovery effects are guarded; remaining
  auto-resolved backend/page/chunk settings need effective-recipe checks.
  The frozen descriptor now carries the reviewed host ID,
  hardware fingerprint, logical device selector, and memory domain. Resolve and
  corroborate that selection against the observed physical GPU before any model
  load; a logical selector is not a CUDA index or observed UUID.
- Install the wrapper and its local helpers under a protected absolute service path.
  The current checkout has group-writable ancestors and cannot be that installation.
  Bind installed source and helper identities to the reviewed runtime recipe.
- Wire the production clock, checkpoint preflight, and credential resolver into
  `NativeCandidateService`; the current interface alone is not production composition.
- Guard the actual memory-saver implementation, collect complete worker identities,
  and compare attributed allocations with the retained grant.
- Typed single-effect controls are implemented in `6c78908`, with deterministic
  tests only; production observations and coordinator persistence remain open.
  Complete forwarding, coordinator/API/CLI integration, and F2C
  single-engine and mixed-engine live qualification on host-a.

These are implementation dependencies. No new owner decision is currently needed.
Existing authorization covers the isolated SGLang environment and reviewed observer
patch on host-a. Existing environments and drivers remain outside that change.

## Pinned source inspection

The local `runtime/sglang_source_preflight.py` now verifies seven selected source
files and revalidates retained identities without importing the engine. Its
synthetic CPU tests and the full 111-test runtime suite pass. Production installed
root selection and consumption by the guarded startup still remain open; selected
files do not attest the complete import graph or compiled package.

Source commit: `fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1`. Inspection on September 14,
2026 did not import an engine or execute a model.

The fetched `server_args.py` SHA-256 is
`e04556de6d99ba8a76b91fffa49aa70ea9d65cd99f09d0da5d4cfe1228e28500`, matching the
earlier recorded comparison against the isolated installation on host-a.

[ServerArgs](https://github.com/sgl-project/sglang/blob/fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1/python/sglang/srt/server_args.py)
defines `tp_size`, `dp_size`, `tokenizer_worker_num`, `max_running_requests`,
`max_total_tokens`, and `mem_fraction_static`. Graph controls are
`disable_prefill_cuda_graph` and `disable_decode_cuda_graph`;
`disable_cuda_graph` exists internally but has no CLI option. Saver fields are
`enable_memory_saver` and `enable_weights_cpu_backup`. Device selection includes
`device`, `base_gpu_id`, and `gpu_id_step`. These findings identify fields, not a
complete approved mapping or effective allocation guarantee.

The remaining closed-feature fields located in that source include `dtype`,
`kv_cache_dtype`, `context_length`, `trust_remote_code`, `speculative_algorithm`,
`enable_lora`, `disaggregation_mode`, `enable_hierarchical_cache`,
`hicache_storage_backend`, `enable_lmcache`, `cpu_offload_gb`, `grpc_port`,
`grpc_mode`, and `smg_grpc_mode`. Singleton topology also needs explicit
`nnodes`, `node_rank`, `pp_size`, `ep_size`, `detokenizer_worker_num`, and
`use_ray` constraints. Automatic warmup (`skip_server_warmup`, `warmups`),
request logging (`log_requests`), compiler settings, and plugin discovery require
explicit treatment in the effective recipe. Do not infer their safety from TP=1.

[Saver adapter](https://github.com/sgl-project/sglang/blob/fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1/python/sglang/srt/utils/torch_memory_saver_adapter.py)
selects its real implementation when enabled, raises a recorded import failure when
enabled but unavailable, and selects its no-op implementation when disabled. The real
adapter's `enabled` property also checks the underlying saver. A configuration flag
alone cannot establish functioning memory release.

[HTTP startup](https://github.com/sgl-project/sglang/blob/fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1/python/sglang/srt/entrypoints/http_server.py)
accepts scheduler, detokenizer, tokenizer, and warmup hooks. Its server composition
installs the independent inference/admin authentication middleware. A protected health
gate must remain effective after that middleware is added, including OPTIONS.

The pinned `/flush_cache` handler returns a plain-text success response and status
200, or status 400 on failure. It does not return JSON null. Release and resume
handlers return implicitly on success (JSON null). Disk reload returns a JSON object
containing `success`, `message`, and `num_paused_requests`. Deterministic control
fixtures must reproduce these actual shapes before their tests can support native
integration.

[Engine startup](https://github.com/sgl-project/sglang/blob/fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1/python/sglang/srt/entrypoints/engine.py)
logs server arguments inside `_launch_subprocesses`, configures logging, and loads
plugins. Avoiding the public CLI does not by itself avoid that path. Startup must
have a tested secret-safe logging contract and a closed plugin policy. The source
also retains scheduler process handles and detokenizer PIDs; those are useful
enrollment inputs, but require independent start-identity corroboration.

## Verification prerequisite

The complete runtime suite needs `TMS_SOURCE_ARCHIVE` set to the existing pinned
archive, `<saver-source-archive>`. The suite validates its
SHA-256 before extraction. The initial unset-variable failure was resolved with
that local archive: all 97 runtime tests passed after the device-descriptor addition.
No owner action is required.

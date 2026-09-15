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
  load. The `sglang_device` collector now correlates bounded proc/sysfs UUID and
  PCI observations against an explicit trusted service mapping and a full-UUID
  inherited CUDA namespace. Provision/freeze that mapping against policy and
  establish the namespace before native imports; a logical selector is not a
  CUDA index or observed UUID by itself.
- The committed wrapper and local helpers are installed at the protected versioned
  path recorded below. Bind their identities and installed engine sources to the
  reviewed runtime recipe before use; installation alone grants no launch authority.
- Startup output containment and closed external-plugin checks are implemented
  and tested in CPU subprocesses. They still need composition before imports in
  the API and spawned interpreters. Source preflight now covers both pinned
  plugin/platform initializers. Trusted package metadata/search paths and disabled
  alternate native logging channels remain required; these helpers are not a sandbox.
- Wire the production clock, checkpoint preflight, and credential resolver into
  `NativeCandidateService`; the current interface alone is not production composition.
  `OwnedCoordinatorState` now composes the lifetime lock before opening SQLite
  or starting a session. It derives fixed state paths, rejects unsafe existing
  database/sidecar files, and retains the lock beyond the connection lifetime.
  Seven ownership tests and fifteen runtime-binding tests pass. Wire this owner
  into the production worker at joint cutover; the legacy entrypoint is unchanged.
- Guard the actual memory-saver implementation, collect complete worker identities,
  and compare attributed allocations with the retained grant.
  `sglang_saver_binding` now checks the existing scheduler singleton chain,
  initialized pool, enrolled process identity, and mapped library backing-file
  provenance around one snapshot call. Its fourteen CPU tests pass. Attach the
  hook to the actual scheduler and install the reviewed binary before native use;
  these checks do not prove allocator routing or whole-process residency.
  The process-local scheduler bridge now observes after the original
  `process_input_requests` returns, with one retained request and bounded deadlines.
  Its thirteen CPU tests pass. The accepted-connection Unix transport now checks
  exact controller/scheduler process identities and kernel peer credentials, with
  1-KiB requests, 64-KiB responses, one active request, and a two-second socket
  deadline. Its twenty-one CPU tests pass. This is not a GPU synchronization point;
  the Rust client in `afe5b2b` now pins protected socket custody and exact scheduler
  peer identity, validates the closed response and same-namespace monotonic time,
  and requires terminal EOF within one deadline. All 39 launcher tests pass,
  including actual Python transport interoperability with synthetic saver facts.
  The scheduler-side listener now creates only a fresh protected 0600 Unix socket,
  retaining one thread and transport instance. Eight CPU tests cover custody and
  shutdown; all 208 runtime tests pass, and the Rust interoperability test uses
  this listener. Trusted path provisioning, startup attachment, complete enrollment,
  and durable consumption remain open. No transport result grants lifecycle authority.
- Typed single-effect controls are implemented in `6c78908`, with deterministic
  tests only; production observations and coordinator persistence remain open.
  Complete forwarding, coordinator/API/CLI integration, and F2C
  single-engine and mixed-engine live qualification on host-a.

These are implementation dependencies. No new owner decision is currently needed.
Existing authorization covers the isolated SGLang environment and reviewed observer
patch on host-a. Existing environments and drivers remain outside that change.

## Pinned source inspection

### Installed read-only preflight — September 15, 2026

On the authorized host host-a, `git archive bf4b209 runtime` was extracted
into a new mode-0700 directory, `$HOME/mllm-sglang-f2-runtime-bf4b209`.
The install refused an existing destination. No existing engine environment,
checkpoint, driver, or service was modified.

Using the isolated SGLang environment's Python with `-I -B`, the committed
helpers verified all nine selected installed sources and all ten checkpoint
artifacts (398 tensors, 8,044,936,192 payload bytes). External-plugin metadata
checks passed. The fixed proc/sysfs collector observed one physical GPU;
the boot-scoped inventory digest was
`2124d5550ed2316a62493cd335399bea795ffa074e07207c8b1d2f3a729387dd`.
An explicit module check confirmed no `sglang`, `torch`, or `transformers`
imports. There was no ServerArgs construction, native startup, or model load.

This is read-only prerequisite evidence, not device-policy provisioning,
whole-package attestation, live qualification, or durable launch authority.
The native entrypoint remains closed.

A second immutable helper install, `$HOME/mllm-sglang-f2-runtime-8356c51`,
contains the committed saver-source and scheduler-bridge helpers. It used the same
new-directory/no-overwrite procedure. Import-free preflight verified and revalidated
all nine saver Python files against release `0.0.9.post1` and source archive SHA-256
`25fd4b691ed3242c3a18b2bef0dbe9de84d2e7068b96a37686a923d55c274f43`.
All nine selected SGLang sources and closed external-plugin checks also passed.
The check imported none of `sglang`, `torch`, `transformers`, or `torch_memory_saver`.
The observer binary patch is still not built/installed, and no model was loaded.

### Source contract

The local `runtime/sglang_source_preflight.py` now verifies nine selected source
files and revalidates retained identities without importing the engine. Its
synthetic CPU tests and the full 200-test runtime suite pass. Production installed
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

# Engine options

Engines take many options. CapyCTL treats each one in one of four ways:

- **typed**: a field of `engine_config` says it. The same option in
  `extra_args` is refused, because the field is the one way to set it.
- **extra**: pass it in `engine_config.extra_args` (with
  `accept_extra_args: true`). CapyCTL checks its shape, not its meaning.
- **approval**: pass it in `extra_args` only if the installation lists it in
  `security.approved_options`. Options that take a path also need the path to
  be inside `security.approved_paths`. These options load code, read files, or
  open connections.
- **reserved**: CapyCTL sets it, or keeps it off, and refuses it in any
  spelling, abbreviations included. The table names the setting to use
  instead.

These rules cover vLLM, SGLang, TensorFold, llama.cpp and TensorRT-LLM. A
[custom engine](engines.md#custom-engines) takes its options from its
manifest, and CapyCTL checks none of them; a deployment on it cannot add any.

The rules come from `crates/capyctl-config/src/engine_policy.rs` and the typed fields from the `engine_config`
schema (`crates/capyctl-config/src/schema.rs`). A test
(`crates/capyctl-config/tests/engine_flag_table.rs`) runs every option in the
tables below through CapyCTL's checks and fails if a row's class is wrong,
or if a typed field is missing from its table. Options not listed follow the
same rules: try `capyctl validate config` to see how one is treated, or print
the full table with `capyctl engine flags`.

An option that is not typed reaches the engine as written. CapyCTL does not
count memory it makes the engine use, such as a CPU KV offload. Size
`memory.request` to include it.

## The ownership table

`capyctl engine flags <family>` prints every option and environment name the
checks know for an engine family (`vllm`, `sglang`, `tensorfold`, `llamacpp`
or `tensorrt-llm`), with its class and the reason. It is built from the same tables
the checks read, so it cannot drift from them. It prints text by default;
`--format json` (or `--json`) prints one object. `--version V` records the
engine version in the output; the policy is the same for every version of a
family this release supports.

```sh
capyctl engine flags sglang --version 0.5.22 --format json
```

```json
{
  "engine": "sglang",
  "version": "0.5.22",
  "flags": [
    {"name": "--watermark-config", "match": "name", "class": "reserved", "reason": "..."},
    {"name": "--debug-tensor-dump-", "match": "prefix", "class": "reserved", "reason": "..."},
    {"name": "--port", "match": "name", "class": "owned", "reason": "..."},
    {"name": "--chat-template", "match": "name", "class": "sensitive", "approval": "path_or_checkpoint", "reason": "..."},
    {"name": "--reasoning-parser", "match": "name", "class": "ordinary", "reason": "..."}
  ],
  "env": [
    {"name": "DUMPER_", "match": "prefix", "class": "reserved", "reason": "..."}
  ],
  "unlisted": {"flags": "...", "env": "..."}
}
```

The classes in the output:

- **owned**: CapyCTL renders or closes the setting, or a typed field says it
  (the **typed** and **reserved** rows of the tables below). Refused.
- **reserved**: nobody may set it, whatever the host approves: a
  configuration file, and options or variables that write requests, tensors
  or debug data to disk. Refused.
- **sensitive**: needs the host's named approval; `approval` names the kind
  (`code`, `path`, `path_or_checkpoint`, `listener_or_egress`, `config`,
  `speculative_config`).
- **ordinary**: passes unchanged.

`match` is `name` for one option (every engine but llama.cpp also refuses
its abbreviations, `_` spellings, `--no-` negations and `=value` forms) or
`prefix` for every option starting with the name. Environment names are
compared upper-cased, so `dumper_enable` is refused like `DUMPER_ENABLE`, with
`engine_env_reserved:<name>`.

Every `LMCACHE_*` variable is reserved on every engine: LMCache's
vLLM helper reads its configuration from the environment after any file, and
CapyCTL renders LMCache's settings itself.

## vLLM 0.30 and 0.31

| Need | Option | Class | CapyCTL |
|---|---|---|---|
| Speculative decoding: EAGLE, EAGLE3, MTP, n-gram, a draft model | `--speculative-config` | approval | A JSON object. Only the keys `method`, `model`, `num_speculative_tokens`, `draft_tensor_parallel_size`, `prompt_lookup_max`, `prompt_lookup_min`, `draft_sample_method` and `moe_backend` are accepted. `method` is one of vLLM's names (`eagle`, `eagle3`, `mtp`, `ngram`, `draft_model`, ...). `model`, the draft directory, must be inside `security.approved_paths`. |
| Reasoning parser | `--reasoning-parser` | extra | Usually set with `engine_config.vllm.reasoning_parser` (`auto` picks one by model family). Use the field or the option, not both. |
| Tool-call parser | `--tool-call-parser`, `--enable-auto-tool-choice` | extra | Usually set with `engine_config.vllm.tool_call_parser`, which also adds `--enable-auto-tool-choice`. Use the field or the options, not both. |
| Parser plugins | `--tool-parser-plugin`, `--reasoning-parser-plugin` | approval | They load code. |
| Context length | `--max-model-len` | typed | `engine_config.context_length` |
| Max running requests | `--max-num-seqs` | typed | `engine_config.max_concurrent_requests` |
| Batched tokens per step | `--max-num-batched-tokens` | typed | `engine_config.vllm.max_num_batched_tokens` |
| Mamba / state cache | `--mamba-cache-dtype`, `--mamba-ssm-cache-dtype`, `--mamba-cache-mode`, `--mamba-block-size` | extra | vLLM has no state slot count; it takes the state from the KV cache budget (`memory.kv_cache`). |
| GPU memory fraction | `--gpu-memory-utilization` | reserved | `engine_config.memory.request`. CapyCTL computes the fraction from it. |
| KV cache size | `--kv-cache-memory-bytes` | reserved | `engine_config.memory.kv_cache` |
| Parallel sizes, executor | `--tensor-parallel-size`, `--pipeline-parallel-size`, `--data-parallel-size`, `--distributed-executor-backend` | reserved | `topology`. On one machine CapyCTL renders `--tensor-parallel-size N --distributed-executor-backend mp` ([One model on several GPUs](deploy.md#one-model-on-several-gpus)); across machines, the group's settings. |
| CPU weight offload, swap | `--cpu-offload-gb`, `--swap-space` | reserved | Not supported: CapyCTL owns where the weights live and counts them in `memory.request`. |
| Sleep mode | `--enable-sleep-mode` | reserved | `residency` (parking) |
| KV cache dtype | `--kv-cache-dtype` | typed | `engine_config.kv_cache_dtype` |
| KV block size | `--block-size` | typed | `engine_config.vllm.block_size_tokens` |
| Chunked prefill | `--enable-chunked-prefill` | extra | |
| CUDA graphs | `--enforce-eager` | typed | `engine_config.cuda_graphs` |
| Weight dtype | `--dtype` | typed | `engine_config.dtype` |
| Quantization | `--quantization` | typed | `engine_config.quantization` |
| Weight loading | `--safetensors-load-strategy` | typed | `engine_config.vllm.safetensors_load_strategy` (`eager` or `lazy`; `eager` while parking when not set) |
| Text only | `--language-model-only` | typed | `engine_config.language_model_only` |
| Remote code | `--trust-remote-code` | typed | `engine_config.trust_remote_code` (the installation must allow it: `capyctl engine add --allow-remote-code`) |
| Log level | `--uvicorn-log-level`, `--log-level`, `--log-config-file`, `--logging-config` | reserved | CapyCTL owns the server's logging. vLLM's own level is the `VLLM_LOGGING_LEVEL` variable: set it in `engine_config.env` once the installation lists it in `security.approved_env`. 0.31 moved its logging options under `--logging-config`; every spelling is reserved. |
| Request and stats logging | `--enable-log-requests`, `--disable-log-requests`, `--enable-log-outputs`, `--disable-log-stats` | reserved | CapyCTL starts vLLM with request and output logging off (`--no-enable-log-requests`, `--no-enable-log-outputs`), so prompts and completions never reach the engine log. It logs requests itself and reads the engine's stats for load reports. |
| Per-request metrics | `--enable-per-request-metrics` | reserved | Always on: each answer carries vLLM's timings for the request, which CapyCTL adds to its [per-request metrics](requests.md#per-request-metrics). |
| Cached-token count | `--enable-prompt-tokens-details` | extra | Off unless you pass it. With it, usage reports `prompt_tokens_details.cached_tokens` and the per-request metrics show `cached_tokens`. |
| KV offload, other connectors | `--kv-transfer-config` | approval | Any KV connector other than LMCache's. It can reach off the host. A deployment that passes one runs without LMCache (`kv_transfer_config_set`), since the engine takes one connector configuration. A value naming an LMCache connector (`LMCacheConnectorV1`, `LMCacheMPConnector`, a `kv_connector_module_path` under `lmcache.`, or either inside a `MultiConnector`) is refused in every spelling, whatever the host approves: CapyCTL renders LMCache's connector itself on vLLM 0.32.0 or newer, from the deployment's `lmcache` ([LMCache](engines.md#lmcache)); `capyctl engine flags` lists it as owned. |
| CPU KV offload | `--kv-offloading-backend`, `--kv-offloading-size` | extra | Uses host memory that CapyCTL does not count. |
| Plugins | `--io-processor-plugin`, `--worker-extension-cls`, `--logits-processors` | approval | They load code. `VLLM_PLUGINS` is set by CapyCTL and cannot be changed. |
| Compilation settings | `--compilation-config` | approval | A JSON value that no check reads. |

## SGLang 0.5.21

| Need | Option | Class | CapyCTL |
|---|---|---|---|
| Speculative algorithm | `--speculative-algorithm` | extra | SGLang's names: `EAGLE`, `EAGLE3`, `NEXTN`, `STANDALONE`, `NGRAM`, `DFLASH`, `DSPARK`, `UNO`. A checkpoint's own MTP heads use `NEXTN` or `EAGLE`; SGLang has no `MTP` name. |
| Draft model | `--speculative-draft-model-path`, `--speculative-draft-model` | approval | The draft directory must be inside `security.approved_paths`. |
| Draft steps and tokens | `--speculative-num-steps`, `--speculative-eagle-topk`, `--speculative-num-draft-tokens` | extra | |
| Linear-attention spec verify | `--enable-linear-replayssm-spec` | extra | See the note below the table. |
| Reasoning parser | `--reasoning-parser` | extra | Usually set with `engine_config.sglang.reasoning_parser` (`auto` picks one by model family). Use the field or the option, not both. |
| Tool-call parser | `--tool-call-parser` | extra | Usually set with `engine_config.sglang.tool_call_parser`. Use the field or the option, not both. |
| Context length | `--context-length` | typed | `engine_config.context_length` |
| Max running requests | `--max-running-requests` | typed | `engine_config.max_concurrent_requests` |
| KV pool in tokens | `--max-total-tokens` | typed | `engine_config.sglang.max_total_tokens`. Without it CapyCTL computes it from `memory.kv_cache`. |
| Mamba / state cache | `--max-mamba-cache-size`, `--mamba-full-memory-ratio`, `--mamba-ssm-dtype` | extra | On a gated-delta-net hybrid CapyCTL sets `--max-mamba-cache-size` itself from `max_concurrent_requests`. If you pass either of the first two, it leaves the state pool to you, and `memory.request` must hold it. A stated `memory.request` that cannot hold the state `--max-mamba-cache-size` fixes, with the weights, the KV cache, SGLang's own allocations and the margin, is refused before the start, naming the request that does. |
| Static memory fraction | `--mem-fraction-static` | reserved | `engine_config.memory.request` and `memory.kv_cache`. See [Reaching a memory fraction](#reaching-an-sglang-memory-fraction). |
| Parallel sizes, GPU choice | `--tp-size`, `--pp-size`, `--dp-size`, `--ep-size`, `--enable-dp-attention`, `--base-gpu-id`, `--gpu-id-step` | reserved | `topology`. On one machine CapyCTL sets `tp_size` N with `base_gpu_id` 0 and `gpu_id_step` 1 ([One model on several GPUs](deploy.md#one-model-on-several-gpus)); data parallel, expert parallel and DP attention are not supported. |
| CPU offload, weight backup | `--cpu-offload-gb`, `--enable-weights-cpu-backup` | reserved | `residency`: CapyCTL chooses how weights come back after parking. |
| Memory saver | `--enable-memory-saver` | reserved | `residency` (parking) |
| KV cache dtype | `--kv-cache-dtype` | typed | `engine_config.kv_cache_dtype` |
| Chunked prefill | `--chunked-prefill-size` | typed | `engine_config.sglang.chunked_prefill_size` (`-1` turns it off) |
| Mixed chunks | `--enable-mixed-chunk` | extra | |
| CUDA graphs | `--disable-cuda-graph`, `--disable-prefill-cuda-graph`, `--disable-decode-cuda-graph`, `--cuda-graph-backend-prefill`, `--cuda-graph-backend-decode` | typed | `engine_config.cuda_graphs` |
| CUDA graph batch size | `--cuda-graph-max-bs` | extra | |
| Weight dtype | `--dtype` | typed | `engine_config.dtype` |
| Quantization | `--quantization` | typed | `engine_config.quantization` |
| Quantize at load | `--quantize-and-serve`, `--modelopt-quant` | reserved | Not supported. Serve a quantized checkpoint instead. |
| Tokenizer workers | `--tokenizer-worker-num` | typed | `engine_config.sglang.tokenizer_workers` |
| Text only | `--language-model-only` | typed | `engine_config.language_model_only` |
| Remote code | `--trust-remote-code` | typed | `engine_config.trust_remote_code` (the installation must allow it: `capyctl engine add --allow-remote-code`) |
| Log level | `--log-level`, `--log-level-http` | reserved | CapyCTL sets them and reads the engine's log. |
| Request logging | `--log-requests`, `--log-requests-target` | reserved | CapyCTL logs requests itself. |
| Metrics | `--enable-metrics` | reserved | Always on: CapyCTL reads them for load reports. |
| Cached-token report | `--enable-cache-report` | reserved | Always on: usage reports `prompt_tokens_details.cached_tokens`, which the [per-request metrics](requests.md#per-request-metrics) show. |
| LMCache | `--enable-lmcache`, `--lmcache-config-file`, every `--lmcache-*` option | reserved | CapyCTL renders them itself on SGLang 0.5.22 or newer, from the deployment's `lmcache` ([LMCache](engines.md#lmcache)); `capyctl engine flags` lists them as owned. |
| KV offload | `--enable-hierarchical-cache`, `--hicache-storage-backend`, `--enable-flexkv` | reserved | Not supported: these caches hold memory and files outside CapyCTL's accounting. Every `--hicache-storage-*` and `--flexkv-*` option is reserved too. |
| Plugins, custom code | `--custom-weight-loader`, `--enable-custom-logit-processor` | approval | They load code. `SGLANG_PLUGINS` is set by CapyCTL and cannot be changed: SGLang loads no plugin. |
| Loader settings | `--model-loader-extra-config` | approval | A JSON value that no check reads. |
| Dumps to disk, watermark key | `--watermark-config`, `--debug-tensor-dump-output-folder`, `--debug-tensor-dump-layers`, `--debug-tensor-dump-input-file`, `--msprobe-dump-config`, `--export-metrics-to-file`, `--export-metrics-to-file-dir`, `--spec-trace-dir` | reserved | Not supported, whatever the host approves. They write requests, tensors or debug data to disk, or (0.5.22) read the key that marks every answer. Every `--debug-tensor-dump-*` option is reserved too, and so are the `DUMPER_*`, `SGLANG_DUMP*`, `SGLANG_KERNEL_API_*` and `SGLANG_CUDA_COREDUMP*` variables and `SGLANG_DSPARK_DEBUG_DUMP`, `SGLANG_DSPARK_ENABLE_SPS_RECORD`, `SGLANG_EXPERT_DISTRIBUTION_RECORDER_DIR` and `SGLANG_TORCH_PROFILER_DIR`. |

**`--enable-linear-replayssm-spec`.** This option exists in SGLang 0.5.21.
It was checked in the published wheel
(`sglang-0.5.21-cp312-cp312-manylinux_2_34_x86_64.whl`, SHA-256
`ac300998eb6b1afcb2971b31317b66ef12891003bdf9d1a5c7e34f9d4e3dcf0d`):
`sglang/srt/arg_groups/fields/exec_.py` declares `enable_linear_replayssm_spec`
(default off), `field_order.py` lists it, and `attention_hook.py` checks it. It
is not one of the speculative options. It changes how a gated-delta-net or
KDA hybrid checks draft tokens, works only with a linear draft chain
(`--speculative-eagle-topk` unset or 1), and cannot be combined with
`--enable-linear-replayssm`. CapyCTL passes it as an ordinary extra argument.
An engine built from other sources may differ; `<engine python> -m
sglang.launch_server --help` lists what an installation accepts.

The speculative options in that wheel (`arg_groups/fields/spec.py`) are
`--speculative-algorithm`, `--speculative-draft-model-path` (alias
`--speculative-draft-model`), `--speculative-draft-model-revision`,
`--speculative-draft-load-format`, `--speculative-num-steps`,
`--speculative-eagle-topk`, `--speculative-num-draft-tokens`,
`--speculative-accept-threshold-single`, `--speculative-accept-threshold-acc`,
`--speculative-use-rejection-sampling`, `--speculative-token-map`,
`--speculative-attention-mode`, `--speculative-draft-attention-backend`,
`--speculative-draft-kv-cache-dtype`, `--speculative-draft-window-size`,
`--speculative-draft-sink-size`, `--speculative-moe-runner-backend`,
`--speculative-draft-model-quantization`, `--speculative-skip-dp-mlp-sync`,
`--enable-multi-layer-eagle`, `--speculative-adaptive`,
`--speculative-adaptive-config`, `--spec-trace-dir`, the
`--speculative-dflash-*`, `--speculative-dspark-*` and
`--speculative-ngram-*` families, and `--uno-lora-path`. Options ending in
`-path`, `-dir` or `-config` need approval, like the draft model path;
`--spec-trace-dir` is reserved, because it writes traces to disk.

## TensorFold 0.6.6

The options of `tensorfold serve` (`tensorfold/cli_args.py` at v0.6.6).
TensorFold has no option for a tool-call parser, quantization, CUDA graphs, a
log level, request logging, KV offload to another store, or plugins: it
chooses parsers by model family and takes the quantization from the
checkpoint.

| Need | Option | Class | CapyCTL |
|---|---|---|---|
| Speculative decoding: a draft model | `--drafter` | approval | The draft directory must be inside `security.approved_paths`. Without `--drafter` or `--no-drafts`, CapyCTL passes `--drafter none`, so TensorFold never picks one itself. |
| No drafts | `--no-drafts` | extra | |
| Draft settings | `--drafter-bits` | extra | |
| MTP drafts | `--mtp-drafts`, `--mtp-confidence` | extra | |
| Reasoning | `--thinking` | typed | `engine_config.tensorfold.thinking` |
| Reasoning budget | `--thinking-budget`, `--reasoning-effort` | extra | |
| Context length | `--context` | reserved | `engine_config.context_length`. CapyCTL always renders it. |
| Max tokens a reply | `--max-tokens` | typed | `engine_config.tensorfold.max_tokens` |
| Max running requests | `--parallel` | extra | Or `engine_config.max_concurrent_requests`, which renders it. Not both. |
| KV cache dtype | `--kv-dtype` | typed | `engine_config.kv_cache_dtype` |
| Prefix caches | `--prompt-cache-gib`, `--checkpoint-slots`, `--spill-gib`, `--max-snapshots` | extra | `--spill-gib` writes to disk. Count the memory in the deployment's `resources`. |
| Snapshot directory | `--snapshot-dir` | reserved | A directory outside CapyCTL's control. |
| Memory limit | none | — | CapyCTL sets `TENSORFOLD_CUDA_MEMORY_LIMIT_GB` to the declared Ready memory (`resources`). The variable cannot be changed. |
| Prefill precision | `--prefill-fp8`, `--precision` | extra | |
| Custom kernels | `--lane-kernels` | approval | It loads code. |
| Vision from URLs | `--vision-urls` | approval | It reaches off the host. |
| Two-GPU groups | `--tp`, `--rank`, `--master`, `--master-port` | reserved | `topology`. CapyCTL renders them for each group member. |
| Background priority by served name | `--name-priority` | extra | 0.6.6 and later, CUDA only: `ID=background`, where `ID` is the served name or an alias. CapyCTL passes no `--name` to the follower of a two-GPU group, so the option may fail there. |
| Authentication | `--api-key`, `--api-key-file`, `--metrics-open` | reserved | CapyCTL owns authentication. The `TENSORFOLD_API_KEY` variable is reserved too. |

## llama.cpp v0.6.0

The options of `llama-server` at the v0.6.0 tag (`common/arg.cpp`). llama-server
matches option names exactly, so CapyCTL does too: every long spelling of an
option (its aliases, its `--no-` form and `_` in place of `-`) is the same
option, and `--name=value` is refused because llama-server takes only
`--name value`. Short options are refused for every engine.

| Need | Option | Class | CapyCTL |
|---|---|---|---|
| Context length | `--ctx-size` | typed | `engine_config.context_length`, each request's window. CapyCTL renders the window times the slots. |
| Max running requests | `--parallel` | typed | `engine_config.max_concurrent_requests`, 4 when not set. It sizes the KV cache, so it has one source. |
| KV cache dtype | `--cache-type-k`, `--cache-type-v` | typed | `engine_config.kv_cache_dtype`, both caches. |
| Layers on the GPU | `--gpu-layers`, `--n-gpu-layers` | typed | `engine_config.llamacpp.n_gpu_layers`, `all` when not set. |
| The model file | `--model` | typed | `engine_config.llamacpp.gguf_file`, a GGUF file inside the model directory. |
| Vision projector | `--mmproj` | typed | `engine_config.llamacpp.mmproj_file`. |
| Flash attention | `--flash-attn` | extra | |
| Batch sizes | `--batch-size`, `--ubatch-size` | extra | |
| Threads | `--threads`, `--threads-batch` | extra | |
| Prompt cache reuse | `--cache-reuse` | extra | |
| Reasoning | `--reasoning-budget`, `--reasoning-format` | extra | |
| Sampling defaults | `--temp`, `--top-p`, `--top-k` | extra | |
| Speculative decoding: settings | `--spec-type`, `--draft-max`, `--draft-min` | extra | A draft context (`--spec-type draft-*`) needs `engine_config.memory.kv_cache` or `resources`. |
| Speculative decoding: a draft model | `--model-draft` | approval | The file must be inside `security.approved_paths`. Its weights are counted. |
| Adapters | `--lora`, `--lora-scaled`, `--control-vector`, `--control-vector-scaled` | approval | Each path must be inside `security.approved_paths`. |
| Templates and grammars from files | `--chat-template-file`, `--grammar-file`, `--json-schema-file` | approval | The file must be inside `security.approved_paths`. |
| Video decoding | `--video-ffmpeg-dir` | approval | It names programs llama-server runs. |
| Listener | `--host`, `--port`, `--reuse-port`, `--path`, `--api-prefix`, `--ssl-key-file`, `--ssl-cert-file` | reserved | CapyCTL listens for the engine on loopback. |
| Authentication | `--api-key`, `--api-key-file` | reserved | CapyCTL owns authentication. |
| Served name | `--alias` | reserved | The deployment's name. |
| Slot layout and self-sizing | `--kv-unified`, `--fit`, `--fit-target`, `--fit-ctx`, `--cache-ram` | reserved | CapyCTL renders `--no-kv-unified`, `--fit off` and `--cache-ram 0` so the memory matches what it derived. |
| Context shift | `--context-shift` | reserved | Rendered off: a request never silently drops its start. |
| Status pages | `--metrics`, `--slots`, `--props` | reserved | CapyCTL turns `/metrics` and `/slots` on and reads them. |
| Web UI and CORS | `--webui`, `--cors-origins`, `--cors-credentials` | reserved | Off. |
| Idle unload, router mode | `--sleep-idle-seconds`, `--models-dir`, `--models-preset`, `--models-max` | reserved | CapyCTL stops and starts the model itself. |
| Downloads | `--offline`, `--hf-repo`, `--hf-file`, `--model-url` | reserved | CapyCTL fetches models. |
| Saved slots, RPC | `--slot-save-path`, `--rpc` | reserved | They write files or open connections. |
| Logs | `--log-file`, `--verbose`, `--log-verbosity` | reserved | CapyCTL owns the engine log. |
| Devices | `--device`, `--split-mode`, `--tensor-split`, `--main-gpu` | reserved | `devices` and `topology`. On several GPUs CapyCTL renders `--split-mode layer`, `--tensor-split` and `--main-gpu 0`; `row` and `tensor` are not supported. |
| Built-in tools and agent | `--tools`, `--agent` | reserved | They run code on the host. |
| Embeddings and reranking | `--embedding`, `--rerank`, `--pooling` | reserved | A llama.cpp deployment serves chat. |

## TensorRT-LLM 1.2.1, 1.3.0rc29 and 1.4.0rc1

The options of `trtllm-serve serve` and the fields of its LLM API
configuration, read from three tags: v1.2.1, v1.3.0rc29 and v1.4.0rc1
(`crates/capyctl-config/src/tensorrt_llm.rs`). An option or key
reserved on any of the three is refused on all three; `trtllm-serve` refuses
an option its version lacks anyway. A version outside the three uses the
tables of the qualified version of its minor line, else 1.2.1's.

`trtllm-serve` matches option names exactly, so CapyCTL does too: no
abbreviation stands for an option, `_` and `-` are the same, every alias is
the same option, and `--name=value` is compared by its name. Short options and
positional arguments (a model path, a subcommand other than `serve`) are
refused.

| Need | Option | Class | CapyCTL |
|---|---|---|---|
| Context length | `--max_seq_len` | typed | `engine_config.context_length` |
| Max running requests | `--max_batch_size` | typed | `engine_config.max_concurrent_requests`, 32 when not set. |
| KV cache dtype | `--kv_cache_dtype` | typed | `engine_config.kv_cache_dtype` (`auto` or `fp8`), rendered as `kv_cache_config.dtype`. The option exists on 1.3 and 1.4. |
| Remote code | `--trust_remote_code` | typed | `engine_config.trust_remote_code` (the installation must allow it). |
| Reasoning parser | `--reasoning_parser` | typed | `engine_config.tensorrt-llm.reasoning_parser`. The model family picks one when not set ([Tool calls and reasoning](engines.md#tool-calls-and-reasoning)). |
| Tool-call parser | `--tool_parser` | typed | `engine_config.tensorrt-llm.tool_call_parser`. The model family picks one when not set. |
| Weight dtype | none | — | `engine_config.dtype`, rendered as `dtype` in the configuration file, `auto` when not set. |
| CUDA graphs | none | — | `engine_config.cuda_graphs`: `false` renders `cuda_graph_config: null`. Not set, `cuda_graph_config` is an ordinary key of `llm_api_options`. |
| Quantization, text only | none | — | `engine_config.quantization` and `engine_config.language_model_only` are refused on a TensorRT-LLM profile: the checkpoint carries its quantization, and 1.2.1 has no text-only switch. |
| Batched tokens per step | `--max_num_tokens` | extra | |
| Chunked prefill | `--enable_chunked_prefill` | extra | |
| Attention window check | `--fail_fast_on_attention_window_too_large` | extra | |
| Media loading | `--media_io_kwargs` | extra | |
| Generation defaults | `--generation-config` | extra | 1.3 and later. It takes `auto` or `trtllm`, not a file. |
| Chat template | `--chat_template` | approval | A file value must be inside `security.approved_paths`. |
| Listener | `--host`, `--port` | reserved | CapyCTL listens for the engine on `127.0.0.1` and a leased port. |
| Backend | `--backend` | reserved | CapyCTL renders `--backend pytorch`. |
| Parallel sizes | `--tensor_parallel_size`, `--tp_size`, `--pipeline_parallel_size`, `--pp_size`, `--gpus_per_node`, `--context_parallel_size`, `--cp_size`, `--moe_expert_parallel_size`, `--ep_size`, `--moe_cluster_parallel_size`, `--cluster_size`, `--enable_attention_dp` | reserved | One GPU: CapyCTL renders `--tp_size 1 --pp_size 1 --gpus_per_node 1`. A deployment with `topology` is refused `multi_gpu_unsupported`. |
| GPU memory fraction | `--free_gpu_memory_fraction`, `--kv_cache_free_gpu_memory_fraction` | reserved | Rendered at 0.9, a ceiling only. The KV pool is `kv_cache_config.max_tokens`, from `engine_config.memory` ([Memory](#the-configuration-file)). |
| Configuration file | `--config`, `--extra_llm_api_options`, `--set` | reserved | CapyCTL writes the only configuration file the engine reads. Use `engine_config.tensorrt-llm.llm_api_options`. `--set` is 1.4's. |
| Log level | `--log_level` | reserved | Rendered `info`. |
| Beam search | `--max_beam_width` | reserved | CapyCTL's chat contract has no beam search. |
| Tokenizers and code | `--tokenizer`, `--custom_tokenizer`, `--custom_module_dirs`, `--post_processor_hook`, `--middleware` | reserved | They load code or another tokenizer than the checkpoint's. |
| Revision | `--revision`, `--hf_revision` | reserved | The checkpoint is local. |
| Served name | `--served_model_name` | reserved | CapyCTL links the checkpoint under the served name, which the engine then lists. |
| Disaggregated serving, tracing | `--metadata_server_config_file`, `--server_role`, `--disagg_cluster_uri`, `--otlp_traces_endpoint` | reserved | Out of scope. |
| Post-processing, frontends | `--num_postprocess_workers`, `--num_serve_frontends` | reserved | More processes CapyCTL does not own. |
| Usage reporting | `--telemetry`, `--no-telemetry` | reserved | The host's `engine_telemetry` decides (1.3 and later). |
| Other listeners | `--grpc`, `--grpc-protocol`, `--openengine-enable-load-metrics`, `--report_addr` | reserved | 1.3 and later. CapyCTL owns the one listener. |
| Visual generation, agent scheduling | `--enable_visual_gen`, `--visual_gen_args`, `--agent_percentage`, `--agent_types` | reserved | 1.3 and later. Out of scope; the agent scheduler reaches the search services it names. |
| Client chat templates | `--allow_request_chat_template` | reserved | 1.3 and later. |
| Sleep | `--enable_sleep_mode` | reserved | 1.4. TensorRT-LLM is restart-only ([Parking](parking.md)). |

### The configuration file

CapyCTL writes `llm_api_options.yaml` (mode 0600) into a private directory for
each launch and passes it with `--config`. The file overrides the command
line, so CapyCTL owns it whole:

```yaml
kv_cache_config:
  max_tokens: <from the grant>
  free_gpu_memory_fraction: 0.9
  dtype: <kv_cache_dtype, or auto>
enable_iter_perf_stats: true
iter_stats_max_iterations: 1000
return_perf_metrics: false
enable_sleep: false            # 1.2 only; 1.3 and 1.4 removed the field
dtype: <dtype, or auto>
# cuda_graph_config: null      when cuda_graphs: false
```

`max_tokens` is `memory.kv_cache` divided by the bytes of KV one token takes,
`2 × layers × KV heads × head dimension × bytes of the KV dtype`, from the
checkpoint's `config.json`. A model whose KV CapyCTL cannot size this way
(sliding or per-layer attention windows, multi-head latent attention,
recurrent or hybrid layers, encoder-decoder models) is refused
`kv_geometry_unsupported`.

A deployment adds LLM API arguments as
`engine_config.tensorrt-llm.llm_api_options`, a mapping of at most 64 KiB as
JSON whose keys are lower-case field names. CapyCTL merges it under its own
keys (one level deep for `kv_cache_config`) and refuses, at deploy time and
again when it writes the file, a mapping that names a reserved key, nested
keys included. Keys it does not reserve reach the engine, whose strict models
refuse unknown ones at start.

| Keys | Class | Why |
|---|---|---|
| `model`, `backend`, `max_seq_len`, `max_batch_size`, `tensor_parallel_size`, `pipeline_parallel_size`, `gpus_per_node`, `kv_cache_config.max_tokens`, `kv_cache_config.free_gpu_memory_fraction`, `kv_cache_config.dtype`, `enable_iter_perf_stats`, `enable_iter_req_stats`, `iter_stats_max_iterations`, `return_perf_metrics`, `perf_metrics_max_requests`, `enable_sleep`, `dtype`, `trust_remote_code`, `reasoning_parser` | reserved | CapyCTL renders them, or a typed field does. |
| `cuda_graph_config` | reserved when `engine_config.cuda_graphs` is set | The typed field renders it. |
| `kv_cache_config.host_cache_size`, `kv_cache_config.max_gpu_total_bytes`, `kv_cache_config.use_uvm`, `kv_cache_config.disk_cache_path`, `kv_cache_config.disk_cache_size`, `kv_cache_config.disk_prefetch_num_reqs`, `kv_cache_config.cross_kv_cache_fraction`, `kv_cache_config.iteration_stats_interval`, `kv_cache_config.max_attention_window`, `kv_cache_config.sink_token_length` | reserved | They move the pool off the GPU or change the geometry CapyCTL sized it from. |
| `context_parallel_size`, `cp_config`, `moe_expert_parallel_size`, `moe_cluster_parallel_size`, `moe_tensor_parallel_size`, `enable_lm_head_tp_in_adp`, `attention_dp_config`, `enable_attention_dp`, `pp_partition`, `orchestrator_type`, `mpi_session`, every `ray_*` key | reserved | One GPU, one process. |
| `kv_connector_config`, `cache_transceiver_config`, every `disagg*` key | reserved | LMCache is off; disaggregated serving is out of scope. |
| `max_beam_width`, `speculative_config`, `lora_config`, `enable_lora`, `peft_cache_config`, `mm_encoder_only`, `encode_only` | reserved | Out of scope in this release. |
| `tokenizer`, `custom_tokenizer`, `tokenizer_revision`, `revision`, `skip_tokenizer_init`, `load_format`, `checkpoint_format`, `checkpoint_loader`, `model_kwargs` | reserved | The checkpoint is the verified local one; `load_format: dummy` serves random weights. |
| `env_overrides`, `post_processor_hook`, `num_serve_frontends`, `num_postprocess_workers`, `postprocess_tokenizer_dir` | reserved | They set the engine's environment, load code or start processes. |
| `otlp_traces_endpoint`, `telemetry_config`, `prometheus_metrics_config`, `perf_metrics_output_dir`, `iter_perf_stats_interval`, `mx_config`, `gms_config`, `dwdp_config`, `sleep_config` | reserved | Connections, files, the statistics CapyCTL reads, or sleep. |
| Everything else, such as `enable_chunked_prefill`, `max_num_tokens`, `kv_cache_config.enable_block_reuse`, `kv_cache_config.tokens_per_block`, `scheduler_config`, `sampler_type`, `attn_backend`, `moe_config`, `guided_decoding_backend`, `torch_compile_config`, `enable_autotuner`, `stream_interval` | ordinary | Passed unchanged. |

The engine's environment is closed too: a profile or deployment `env` may not
name a `TLLM_`, `TRTLLM_`, `TRT_LLM_`, `OMPI_`, `PMIX_`, `PRTE_`, `UCX_`,
`NCCL_`, `MPI4PY_`, `HF_`, `TRANSFORMERS_`, `FLASHINFER_`, `RAY_` or `SLURM_`
variable, nor `CUDA_VISIBLE_DEVICES`, `NVIDIA_VISIBLE_DEVICES`,
`DISABLE_TORCH_DEVICE_SET`, `CUDA_MPS_PIPE_DIRECTORY`, `TMPDIR`, `HOME`,
`RANK`, `LOCAL_RANK`, `WORLD_SIZE`, `MASTER_ADDR`, `MASTER_PORT`,
`PROMETHEUS_MULTIPROC_DIR`, `DG_JIT_CACHE_DIR`, `MX_TRANSFER_LOG_DIR`,
`MODEL_EXPRESS_URL`, `TELEMETRY_DISABLED` or `DO_NOT_TRACK`, even when
`security.approved_env` lists it (`engine_env_reserved:<name>`).

## Reaching an SGLang memory fraction

SGLang sizes its memory from `--mem-fraction-static`, a share of the memory
free when it starts. CapyCTL reserves that option and computes it from
`engine_config.memory`. This section shows how to aim for a given fraction.
It describes `main`. Two open changes (pull requests #74 and #75) revisit the
8 GiB margin and the 2 GiB overhead used below; if they merge, use the
constants they set.

### The formula

On a unified-memory machine (one pool for CPU and GPU, such as a 128 GB
GB10), with `R` = `memory.request`, `K` = `memory.kv_cache`, `W` = the
checkpoint's weights, `S` = the state pool of a hybrid model (0 otherwise),
and `A` = `MemAvailable` from `/proc/meminfo` when the engine starts:

1. The margin is 8 GiB (`SGLANG_OVERHEAD_MARGIN_BYTES`,
   `crates/capyctl-config/src/effective/engine_config.rs`).
2. The static pool is `R − 8 GiB`, but never less than `K` and never more
   than `R` (`static_pool_bytes`,
   `crates/capyctl-config/src/context_fit/sglang_pool.rs`).
3. When CapyCTL sizes SGLang's pools (a full-attention model or a
   gated-delta-net hybrid; others are left to SGLang), the pool grows to at
   least `W + K + S + 2 GiB`, up to `R` (`grown_static` and
   `STATIC_OVERHEAD_BYTES` in the same file).
4. The fraction is the static pool divided by `A`, rounded down to four
   decimal places, and must be between 0.0001 and 0.9999 (`static_fraction`
   in `runtime/sglang_server_args.py`).

So, when `R − 8 GiB` is at least `W + K + S + 2 GiB`:

```text
fraction = floor(10000 × (R − 8 GiB) / A) / 10000
so, for a target fraction f:  R = f × A + 8 GiB
```

`A` is not the machine's total. It is what is free when the engine starts,
so it is lower while other models are loaded. A fraction computed from `A`
therefore gives the same bytes each time, not the same share of the machine.

### Worked example

A 128 GB machine with `A` = 120 GiB at launch, a model with 30 GiB of
weights, no state pool:

| Target fraction | Static pool `f × A` | `memory.request` | Largest `memory.kv_cache` (`R − 8 GiB − W − 2 GiB`) |
|---|---|---|---|
| 0.50 | 60 GiB | 68GiB | 28GiB |
| 0.65 | 78 GiB | 86GiB | 46GiB |
| 0.80 | 96 GiB | 104GiB | 64GiB |

```yaml
engine_config:
  memory:
    request: 86GiB   # 0.65 of 120 GiB, plus the 8 GiB margin
    kv_cache: 46GiB
```

Things to know:

- With a smaller `memory.kv_cache`, the fraction stays the same, but CapyCTL
  also passes `--max-total-tokens` (`memory.kv_cache` divided by the KV bytes
  per token). SGLang then keeps no more KV cache than that, and the rest of
  the static pool stays unused. To use a larger KV cache, raise
  `memory.kv_cache`; to set the token count yourself, use
  `engine_config.sglang.max_total_tokens`.
- The whole `memory.request` is reserved on the machine, so it must fit the
  memory limit (`host.resource_policy.memory.system.managed_limit`).
- On a discrete GPU the margin is `(R − K) / 11` instead of 8 GiB, `A` is the
  card's total memory, and the fraction gets 1 GiB more when CapyCTL sizes
  the pools (`DISCRETE_BASELINE_ALLOWANCE_BYTES`).

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

The rules come from `crates/capyctl-config/src/engine_policy.rs` and the typed fields from the `engine_config`
schema (`crates/capyctl-config/src/schema.rs`). A test
(`crates/capyctl-config/tests/engine_flag_table.rs`) runs every option in the
tables below through CapyCTL's checks and fails if a row's class is wrong,
or if a typed field is missing from its table. Options not listed follow the
same rules: try `capyctl validate config` to see how one is treated.

An option that is not typed reaches the engine as written. CapyCTL does not
count memory it makes the engine use, such as a CPU KV offload. Size
`memory.request` to include it.

## vLLM 0.30

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
| CPU weight offload, swap | `--cpu-offload-gb`, `--swap-space` | reserved | Not supported: CapyCTL owns where the weights live and counts them in `memory.request`. |
| Sleep mode | `--enable-sleep-mode` | reserved | `residency` (parking) |
| KV cache dtype | `--kv-cache-dtype` | typed | `engine_config.kv_cache_dtype` |
| KV block size | `--block-size` | typed | `engine_config.vllm.block_size_tokens` |
| Chunked prefill | `--enable-chunked-prefill` | extra | |
| CUDA graphs | `--enforce-eager` | typed | `engine_config.cuda_graphs` |
| Weight dtype | `--dtype` | typed | `engine_config.dtype` |
| Quantization | `--quantization` | typed | `engine_config.quantization` |
| Text only | `--language-model-only` | typed | `engine_config.language_model_only` |
| Remote code | `--trust-remote-code` | typed | `engine_config.trust_remote_code` (the installation must allow it) |
| Log level | `--uvicorn-log-level` | reserved | CapyCTL owns the server's logging. vLLM's own level is the `VLLM_LOGGING_LEVEL` variable: set it in `engine_config.env` once the installation lists it in `security.approved_env`. |
| Request and stats logging | `--enable-log-requests`, `--disable-log-requests`, `--disable-log-stats` | reserved | CapyCTL logs requests itself and reads the engine's stats for load reports. |
| KV offload, LMCache | `--kv-transfer-config` | approval | vLLM's KV connectors (LMCache among them) are set here. It can reach off the host. |
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
| Mamba / state cache | `--max-mamba-cache-size`, `--mamba-full-memory-ratio`, `--mamba-ssm-dtype` | extra | On a gated-delta-net hybrid CapyCTL sets `--max-mamba-cache-size` itself from `max_concurrent_requests`. If you pass either of the first two, it leaves the state pool to you, and `memory.request` must hold it. |
| Static memory fraction | `--mem-fraction-static` | reserved | `engine_config.memory.request` and `memory.kv_cache`. See [Reaching a memory fraction](#reaching-an-sglang-memory-fraction). |
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
| Remote code | `--trust-remote-code` | typed | `engine_config.trust_remote_code` (the installation must allow it) |
| Log level | `--log-level`, `--log-level-http` | reserved | CapyCTL sets them and reads the engine's log. |
| Request logging | `--log-requests`, `--log-requests-target` | reserved | CapyCTL logs requests itself. |
| Metrics | `--enable-metrics` | reserved | Always on: CapyCTL reads them for load reports. |
| KV offload, LMCache | `--enable-lmcache`, `--enable-hierarchical-cache`, `--hicache-storage-backend`, `--enable-flexkv` | reserved | Not supported: these caches hold memory and files outside CapyCTL's accounting. Every `--lmcache-*`, `--hicache-storage-*` and `--flexkv-*` option is reserved too. |
| Plugins, custom code | `--custom-weight-loader`, `--enable-custom-logit-processor` | approval | They load code. |
| Loader settings | `--model-loader-extra-config` | approval | A JSON value that no check reads. |

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
`-path`, `-dir` or `-config` need approval, like the draft model path.

## TensorFold 0.6.5

The options of `tensorfold serve` (`tensorfold/cli_args.py` at v0.6.5).
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
| Authentication | `--api-key`, `--api-key-file`, `--metrics-open` | reserved | CapyCTL owns authentication. |

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

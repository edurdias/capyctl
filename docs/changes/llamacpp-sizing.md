# Status: llama.cpp GGUF header and sizing (plan slice L4) — 2026-10-09 (branch `feat/llamacpp-l4-sizing`)

[ADR 0029](../design/adr/0029-llamacpp-engine.md) §5, §9, plan slice L4
(`docs/plans/2026-10-09-llamacpp-engine.md`). A llama.cpp deployment no longer has to
state `resources`: CapyCTL reads the GGUF header of the model it renders and derives
the memory request. The format, the keys and the KV layout were read from the v0.6.0
tag (`ggml/src/gguf.cpp`, `src/llama-model.cpp`, `src/llama-hparams.cpp`,
`src/llama-kv-cache.cpp`, `src/llama-context.cpp`, `src/llama-arch.cpp`,
`src/models/*.cpp`, commit `d812350`); nothing ran on a GPU.

- **Header reader** (`capyctl-config/src/gguf.rs`). Magic `GGUF`, version 2 or 3,
  then the metadata key/value pairs; never tensor data. Bounded: 64 MiB of metadata,
  1 048 576 keys, strings up to 1 MiB, arrays up to 1 048 576 entries; a truncated
  file, a duplicate key, an array of arrays or an unknown type is refused. Only
  numbers, booleans, `general.architecture` and short integer arrays are kept;
  every key's name is kept.
- **KV formula** (`capyctl-config/src/context_fit/llamacpp.rs`).
  `slots × pad256(context_length) × Σ_layers head_count_kv × (key_length × bytes(K) +
  value_length × bytes(V))`; `head_count_kv` a number or one per layer (default the
  heads), `key_length`/`value_length` else `embedding_length / head_count`; the MTP
  layers (`nextn_predict_layers`) are not the main context's; the cache types' block
  sizes (`q8_0` 34 bytes per 32 values, `q4_0`/`iq4_nl` 18, `q4_1` 20, `q5_0` 22,
  `q5_1` 24). Without `--flash-attn on` the V cache is counted padded to its widest
  layer, as llama.cpp lays it out then.
- **Derived request.** Weights (the rendered GGUF and all its shards, the projector,
  the draft model and all its shards, ADR 0014 A6) + KV + the family margin
  (unified: ADR 0014 A18; discrete GPU: weights × 1.10 + KV, ADR 0019 §3). The
  startup peak is the request: cold, Ready, parking and wake charge it, parked
  nothing. The margin stays the placeholder until the live rows measure it.
- **Draft model.** A `--model-draft` in the host-fixed or the extra arguments counts
  with every shard llama.cpp loads (a split set named by its first shard), each inside
  `security.approved_paths` once symlinks resolve. A draft that cannot be counted
  (outside the approved paths, a shard missing, a set named by a later shard) is
  refused at resolution, declared budget or not, instead of being left out of it; so
  is a checkpoint with no model to pick.
- **Declared budgets.** llama-server allocates the whole cache its context and slots
  fix (`--fit off`), so where the header makes it calculable (full attention, no
  cache layers off the GPU via `n_gpu_layers` or `--no-kv-offload`) a declared
  `memory.kv_cache`, `memory.request` or `resources` that leaves less for the KV cache
  than that figure is refused naming the field. A draft context only adds to it, so
  the bound stands beside a draft model. Other layouts keep the declared estimate.
- **Refusals.** Derivation is refused, naming `engine_config.memory.kv_cache` or
  `resources`, for a sliding-window header (a key, or an architecture whose model code
  has such layers, `llama4` without a window of 0 included), recurrent (`ssm.*`,
  `wkv.*`), hybrid (`full_attention_interval`, `shortconv.*`, llama.cpp's hybrid
  architectures), MLA (`attention.kv_lora_rank`), other layouts (indexer caches,
  embedding, diffusion and encoder models, a projector), an unreadable header or no
  single model to pick; for `n_gpu_layers` other than `all`, `--override-tensor`,
  `--cpu-moe`, `--n-cpu-moe`, `--n-cpu-ffn`, `--no-kv-offload`, an `mlock` load mode;
  and for a draft context: any `--spec-type draft-*` and a `--model-draft` (judgement
  call: llama.cpp gives the draft model its own context with as many cells as the
  target's, which the ADR does not size; a declared `memory.kv_cache` still derives
  the request with the draft model's weights). A `context_length` above the GGUF's
  `<arch>.context_length` is refused, declared resources or not.
- **Facts.** The host measures them beside the digest (`GgufFacts`: loaded bytes,
  training context, the cache shape or why not); they travel as
  `CheckpointDigestEvidence.gguf` and `SingleLaunchPlan.checkpoint_gguf` (capability
  `checkpoint_gguf`, refused before sending to a host without it), are recorded in the
  revision's `memory.gguf` beside the whole checkpoint's weights (which a plan still
  names and a host verifies), and re-measured before a launch: a remote host and the
  embedded (standalone) host alike refuse a launch whose re-measured weights or GGUF
  facts differ from the revision's (`checkpoint_mismatch`), so a draft model grown
  after the revision was recorded, outside the checkpoint's digest, never launches
  against the old reservation. A llama.cpp revision is provisional until a host
  measured its checkpoint (`CheckpointFacts::provisional()` carries a pending header),
  declared resources included, so the training-context check always applies.

Tests (T42, T26, CPU only): `crates/capyctl-config/tests/llamacpp_sizing.rs`
(reader and bounds, the formula and the plan's 48-layer figure, per-layer
`head_count_kv`, refusals, measured weights with split and refused drafts, a
host-fixed draft, declared budgets against the calculable cache, derived phases on
unified and discrete memory, snapshot re-derivation, training context),
`crates/capyctl-controller/src/checkpoint_digests/tests.rs` (T34 T22: a grown draft
refuses an embedded Initialize), `crates/capyctl-protocol/tests/
checkpoint_digest.rs` (wire round trip and capability), `capyctl-domain` unit tests.
CPU and Fake-engine tests are not qualification; only LC1–LC6 qualify llama.cpp.

Live checks still open: the KV figure against llama.cpp's startup log line, the
margin and the startup peak against measured memory, the draft and MTP contexts'
caches.

Release note: none.

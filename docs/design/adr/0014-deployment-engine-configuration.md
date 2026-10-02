# ADR 0014 — Deployment engine configuration and checkpoint identity

**Status:** Accepted (owner decisions E1 and P2, 2026-09-22). Owner answers the same day
confirm the text as written: Q9 the checkpoint re-verification of §7 (open issue 1), Q10
extra arguments allowed unless the host denies them with sensitive options always needing
named approval (§6, §8), and Q11 the protected vLLM entry `runtime/vllm_entry.py` (§6).
The SPEC amendments below are applied to `SPEC.md` §8.2 and §16.3.
**Amends:** `SPEC.md` §8.2 (parameter ownership) and §16.3 (single-host example). It
implements ADR 0008's "a deployment owns its `engine_config`" and applies ADR 0011's rule
that mllm validates a recipe's shape and capacity while the user owns whether it works.
**Amended by:** Amendment A3 below (owner decision 2026-09-25): vLLM
`--speculative-config` is approved key by key (§8).
**Unit:** WE of `docs/plans/2026-09-22-two-host-control-plane-plan.md`.

## Context

Owner decision E1: every engine must be able to serve any model. The host declares its
engine installations; the deployment chooses the model, the installation and its
parameters. Deployments carry typed common parameters plus ordinary engine arguments that
pass through under SPEC §8.2 and §13.3 operator policy, behind an explicit flag. Settings
mllm owns are always reserved. Security-sensitive options need host-policy approval.
Checkpoint identity is a digest recorded at deploy time and re-verified at launch. The
single-checkpoint SGLang recipe pin and the narrow vLLM flag list are over-restrictions to
remove.

What the code does today:

- **Tuning lives on the host.** Engine tuning is in the host profile's
  `launch_settings` (`crates/mllm-config/src/schema.rs`, `LAUNCH_SETTINGS`), not in the
  deployment, which contradicts ADR 0008.
- **SGLang serves one checkpoint.** `normalize_launch`
  (`crates/mllm-config/src/effective.rs`, around lines 824–966) refuses any recipe but
  `qwen3_4b_instruct2507_tp1_dp1_bf16_disk_reload_v1` and pins dtype, context (4096),
  running requests (8), total tokens (4096) and nine feature switches. The same pin is
  repeated in `crates/mllm-adapters/src/sglang/args.rs` (`validate_settings` and a
  Qwen3-4B geometry KV lower bound), `sglang/frozen.rs` (recipe, source and checkpoint
  revision constants from `effective/sglang.rs`), `runtime/sglang_entry.py` (`_SETTINGS`,
  `_RECIPE`, `_SOURCE`, `_CHECKPOINT`, `_MINIMUM_KV`) and `runtime/sglang_server_args.py`
  (`_FIXED`, about 110 ServerArgs fields with exact values and types).
- **The checkpoint is one pinned manifest.** `runtime/checkpoint_manifest.py` compiles in
  ten SHA-256 values and the Qwen3-4B geometry; `runtime/checkpoint_preflight.py` checks
  config fields, tensor names, shapes and storage against it.
- **The SGLang build is one pinned source tree.** `runtime/sglang_source_preflight.py`
  compiles in revision `94602c9c…` and an 86-file source inventory.
- **vLLM accepts seven flags.** `engine_policy.rs:41-49` allowlists `--max-model-len`,
  `--trust-remote-code`, `--dtype`, `--enforce-eager`, `--max-num-seqs`,
  `--max-num-batched-tokens` and `--tokenizer-mode`, from the host profile's `args` only;
  SGLang profiles may carry no arguments at all.
- **Checkpoint identity is declared, not measured.** `model.content_fingerprint` is a
  string the agent compares with the plan (`crates/mllm-agent/src/native_execution.rs`
  around line 144); standalone writes `sha256:<name>`.

## Decision

### 1. Ownership split

| Where | What |
|---|---|
| Host installation (profile) | Executable, runtime type, build fingerprint, environment, security policy (deep park, trust remote code, approved sensitive options and paths, extra-argument policy), host-fixed arguments. |
| Deployment `engine_config` | Typed common parameters, engine-specific typed parameters, memory request, extra arguments and the flag that accepts them. |
| mllm (reserved) | Everything in §3, rendered from the grant, placement and binding. |

The profile's tuning fields (`kv_cache_dtype`, `block_size_tokens`, `cpu_offload_bytes`,
`requested_budget`, `recipe` and every SGLang recipe field) move to the deployment. A host
document still naming them is refused with an error naming the new location (SPEC §15.3
strictness). Standalone templates and live fixtures move in the same unit.

### 2. Typed parameters

```yaml
engine_config:
  dtype: bfloat16                 # auto | bfloat16 | float16 | float32
  quantization: modelopt_fp4      # engine's own name; omitted = checkpoint default
  kv_cache_dtype: fp8_e4m3        # omitted = engine default
  context_length: 32768
  max_concurrent_requests: 16
  cuda_graphs: true               # omitted = mllm safe default (§4)
  language_model_only: true       # serve a multimodal checkpoint as text-only
  trust_remote_code: false        # needs host approval (§3)
  memory:
    request: 40GiB                # per instance (P2); see §5
    kv_cache: 8GiB
  vllm: {block_size_tokens: 16, max_num_batched_tokens: 8192}
  sglang: {max_total_tokens: 65536, chunked_prefill_size: 4096}
  accept_extra_args: true
  extra_args: ["--reasoning-parser", "qwen3"]
```

| Typed field | vLLM rendering | SGLang ServerArgs field |
|---|---|---|
| `dtype` | `--dtype` | `dtype` |
| `quantization` | `--quantization` | `quantization` |
| `kv_cache_dtype` | `--kv-cache-dtype` | `kv_cache_dtype` |
| `context_length` | `--max-model-len` | `context_length` |
| `max_concurrent_requests` | `--max-num-seqs` | `max_running_requests` |
| `cuda_graphs: false` | `--enforce-eager` | `disable_cuda_graph` (prefill and decode) |
| `language_model_only` | the installation's text-only switch | the installation's text-only switch |
| `trust_remote_code` | `--trust-remote-code` | `trust_remote_code` |
| `memory.kv_cache` | `--kv-cache-memory` bytes (reserved rendering) | `max_total_tokens` derived from geometry (open issue 2) |

Values are passed through as the engine spells them; mllm validates type and range, not
whether the engine supports a value on this checkpoint (ADR 0011). `language_model_only`
maps to whichever switch the installed build accepts; a build that has none refuses the
field at launch with a named error. A typed field and its native spelling in `extra_args`
together are a duplicate and refused.

### 3. Reserved settings (always refused, rendered by mllm)

**vLLM** keeps `VLLM_RESERVED_FLAGS` (`engine_policy.rs:13-40`: host, port, model,
served model name, device, tensor and pipeline parallel size, GPU memory utilization, CPU
offload, swap space, KV cache bytes and memory, sleep mode, API key, middleware, request
and stats logging, log config, uvicorn log level and access log) and adds:
`--safetensors-load-strategy` while sleep mode is on, `--data-parallel-size`,
`--data-parallel-address`, `--data-parallel-rpc-port`, `--distributed-executor-backend`,
`--headless`, `--api-server-count`, `--uds`, `--root-path`, `--ssl-keyfile`,
`--ssl-certfile`, `--ssl-ca-certs`, `--revision`, `--code-revision`, `--config` (a
configuration file would hide values, SPEC §8.2) and `--disable-log-stats` (W8 needs the
metrics). `--kv-cache-dtype` and `--block-size` leave the reserved list and become typed.

**SGLang** reserves this subset of today's `_FIXED` plus the fields
`construct_server_args` sets: `host`, `port`, `api_key`, `admin_api_key`, `model_path`,
`tokenizer_path`, `served_model_name`, `revision`, `device`, `base_gpu_id`,
`gpu_id_step`, `tp_size`, `dp_size`, `pp_size`, `ep_size`, `dcp_size`, `attn_cp_size`,
`moe_dp_size`, `nnodes`, `node_rank`, `dist_init_addr`, `use_ray`, `enable_dp_attention`,
`mem_fraction_static` (rendered from the grant), `cpu_offload_gb`,
`enable_memory_saver`, `enable_weights_cpu_backup` and
`enable_draft_weights_cpu_backup` (derived from residency, ADR 0010), `grpc_port`,
`grpc_mode`, `smg_grpc_mode`, `sidecar`, `sidecar_args`, `smg_http_sidecar_port`,
`fastapi_root_path`, `enable_http2`, every `ssl_*` field and `enable_ssl_refresh`,
`log_level`, `log_level_http`, `log_requests`, `log_requests_target`,
`crash_dump_folder`, `enable_metrics` (on, owned by mllm for W8), `skip_server_warmup`,
`disaggregation_mode`, `enable_hierarchical_cache`, `hicache_storage_backend` and its
extra config, `enable_lmcache`, `lmcache_config_file`, `enable_flexkv`,
`flexkv_config_file` (cache integrations are owned under SPEC §12 until designed), and
`quantize_and_serve` with the `modelopt_*_path` fields (quantization is out of scope,
SPEC §1.2).

The reserved lists are checked twice: at deploy time in Rust (normalized names, exact
match or any unambiguous prefix of a reserved name, since both engines' argument parsers
accept abbreviations), and at launch by the engine's own parser (§6).

### 4. mllm safe defaults

Some of today's pins encode live findings rather than a checkpoint. They become defaults
the deployment may override, shown with provenance `mllm default` in the effective
configuration (T14): SGLang CUDA graphs off while the memory saver is on, SGLang
tokenizer and detokenizer workers 1, vLLM
`--safetensors-load-strategy eager` whenever sleep mode is on (reserved there). Every
other `_FIXED` value is dropped and the engine's own default applies.

### 5. Memory request (P2)

`engine_config.memory.request` is the per-instance reservation. When omitted it is
derived: weights bytes (sum of weight-file sizes in the checkpoint manifest, §7) plus
`memory.kv_cache` plus the engine family's overhead margin. When `kv_cache` is omitted it
is derived as request minus weights minus margin and must be positive. At least one of
the two is required: an engine's own default takes all free memory, which SPEC §7.1
forbids. Initial margins are conservative placeholders (proposed 8 GiB per family) until
M16 measures peak minus weights minus KV for each model and engine; matrix budgets are
then recomputed from those measurements. Derived phases: cold, ready, parking and wake
equal the request; parked equals the engine's residual floor, also measured. An explicit
`resources:` phase block still overrides derivation. Reservations always use the declared
or derived request, never a sampled value (SPEC §7.3).

### 6. Extra arguments

`extra_args` is an ordered token list of long options and their values. It is accepted
only when the deployment sets `accept_extra_args: true` and the installation's policy is
not `security.extra_args: denied` (default `allowed`). Deploy-time checks refuse short
options, positional tokens that do not follow an option, reserved names and their
prefixes, duplicates, typed-field duplicates, configuration-file options, and
security-sensitive options the host has not approved (§8). mllm does not check arity or
meaning; the engine's parser does.

At launch the engine's own parser sees the complete argument set and mllm compares the
resolved reserved fields with the values it rendered; any difference is a closed
`effective_args_mismatch` failure before the engine starts serving:

- **SGLang.** `sglang_server_args.py` parses `extra_args` with the installed
  `ServerArgs` CLI parser, merges typed and extra values under the reserved ones, runs
  `resolve_once`, and revalidates only the reserved subset (today's `revalidate`, narrowed
  from `_FIXED`).
- **vLLM.** A new protected entry `runtime/vllm_entry.py` parses the argument vector with
  the installed vLLM parser, checks the reserved fields, and starts the server in-process
  with mllm's key guard as today.

### 7. Checkpoint identity

The checkpoint digest is `sha256:` over a canonical manifest: every regular file under
the model directory, sorted by relative path, with size and SHA-256. Symbolic links, and chains of up to 8 links (A5), are
followed only when every hop resolves inside the host's model store (Hugging Face snapshot
layout). The manifest also supplies weights bytes for §5.

- **Recorded at deploy.** A declared `model.content_fingerprint` is the expectation.
  Otherwise the deploy is accepted durably with the condition
  `checkpoint_digest_pending` (R12), a host holding the checkpoint computes the digest,
  and the server records it on the deployment revision. Activation waits for it.
- **Per host.** Before a host first receives an instance, its agent computes the digest
  and must match the recorded one; the result is cached with each file's stat identity
  (device, inode, size, modification and change time).
- **Re-verified at launch and wake.** The agent checks every file's stat identity against
  its cache and rehashes the small files (configuration, tokenizer, index); any change
  forces a full rehash, and a mismatch refuses the launch with the reservation released
  on evidence that nothing started. Wake is included because deep parking reloads weights
  from disk (SPEC §9.1); a checkpoint changed under a parked engine is silent model
  substitution (SPEC §10).

Remote hosts need a new `DigestCheckpoint` member action and result, a protocol
addition after W3.

### 8. Security-sensitive options (host approval)

Refused unless the installation lists the option in `security.approved_options`, and
for path values the path is inside `security.approved_paths`:

- remote or custom code: `trust_remote_code`; vLLM `--worker-cls`,
  `--worker-extension-cls`, `--logits-processors`, `--tool-parser-plugin`; SGLang
  `enable_custom_logit_processor`;
- paths: download and cache directories, a tokenizer or chat-template file outside the
  checkpoint, LoRA and draft-model paths, generation-config directories, allowed local
  media paths;
- listeners and egress: any extra port or socket, tracing endpoints, remote load formats,
  media URL fetching domains, tokens for remote hubs.

`trust_remote_code` keeps its existing host switch (`security.trust_remote_code`).
Options not on any list are ordinary. vLLM `--speculative-config` has its own rule
(Amendment A3).

### 9. What stays and what goes

| Artifact | Decision |
|---|---|
| Installation build fingerprint | Stays; recorded per host and shown per instance. |
| `sglang_source_preflight.py` | Retired (ADR 0008 amendment, owner decision 2026-09-23), with `saver_source_preflight.py`. No installation file is compared to hashes and no permission rule applies to installation files. Registration records an installation fingerprint (package version and a digest over its files) and later drift is flagged; the internals the entry depends on are probed by shape at launch (`runtime/engine_capabilities.py`). An SGLang upgrade or custom build needs neither a code change nor a new manifest. |
| `effective/sglang.rs` constants | Removed; frozen metadata carries build fingerprint and checkpoint digest instead. |
| `normalize_launch` SGLang recipe check and pins | Removed; residency-derived memory saver, CPU weight backup and weight restore stay. |
| vLLM approved-flag list | Removed; reserved and sensitive lists plus §6 replace it. |
| `sglang/args.rs` `validate_settings`, Qwen3-4B KV bound | Removed; a geometry bound is computed from the checkpoint's configuration when derivable. |
| `sglang_entry.py` `_SETTINGS`, `_RECIPE`, `_SOURCE`, `_CHECKPOINT`, `_MINIMUM_KV` | Removed; public settings are validated against the typed schema and reserved subset. |
| `sglang_server_args.py` `_FIXED` | Split into reserved (§3) and safe defaults (§4); the rest is dropped. |
| `checkpoint_manifest.py` | Removed. |
| `checkpoint_preflight.py` | Retired. The agent verifies the digest in Rust for both engines (§7), porting its descriptor-based open chain so no path component is re-resolved between check and hash; exact configuration, geometry and tensor checks go (ADR 0011). The SGLang entry stops calling it. |

## SPEC amendments (exact text)

- **§8.2**, append: "A deployment's `engine_config` carries typed common parameters per
  engine family and, only when the deployment sets `accept_extra_args: true` and host
  policy allows it, ordinary engine arguments passed through unchanged. Reserved settings
  are refused at deployment and verified again after the engine's own parser resolves
  them. Code-loading, path, listener and egress options require the host installation
  to approve them by name. A checkpoint is identified by a content digest recorded when
  the deployment is accepted and re-verified before every launch and wake (ADR 0014)."
- **§16.3**, replace the `engine_args:` block with:
  ```yaml
  engine_config:
    context_length: 65536
    memory:
      kv_cache: "8GiB"
  ```
  and append to the paragraph after the example: "`engine_config` is validated for shape;
  whether the engine supports the combination on this checkpoint is the user's
  responsibility (ADR 0011)."

## Consequences

- The four D5 models, including the NVFP4 anchor, become deployable on either engine
  without code changes; whether each engine build serves each one is found by live runs.
- A recipe that does not work fails as launch attempts and ends terminal `Failed` after
  the ADR 0011 budget; mllm no longer prevents it in advance.
- Launch time grows by stat checks on every launch and a full hash on first placement per
  host (roughly 30–60 s for a 60 GB checkpoint at 1–2 GB/s; to be measured).

## Open issues

1. **Re-verification cost.** Resolved by owner decision Q9: a full hash on first
   placement on a host and whenever any file's size, modification time or inode changes;
   every launch and wake re-checks that metadata and rehashes the small files. This
   trusts the filesystem's change time; a full rehash on every launch would be stronger
   but costs about a minute per large model.
2. **SGLang KV bytes.** SGLang sizes KV by `mem_fraction_static` and `max_total_tokens`,
   not bytes. `max_total_tokens` can be derived from configuration geometry for standard
   attention; hybrid and MLA architectures (the qwen3.8 anchor is hybrid) need an explicit
   `sglang.max_total_tokens` together with `memory.request`. `mem_fraction_static` from
   the grant on unified memory is unverified live.
3. **Time-of-check gap.** Files can change between verification and the engine's read.
   The mitigation is a model store not writable by other users; it is not closed.
4. **Parser drift.** Reserved ServerArgs field names and vLLM flag names change between
   engine versions. The launch check fails closed when a reserved name is missing, which
   makes drift visible, not silent.
5. **Sensitive-option lists are initial.** New engine versions add options; an unlisted
   dangerous option passes as ordinary until the list is updated. The launch-time reserved
   check does not cover it.
6. **Evidence.** None of this is qualified; each model and engine pair needs a live run.

## Amendment A1 — deployment timeouts (owner decision 2026-09-22, open issue 1)

A deployment may declare how long its Initialize and its wake may take. This replaces
the fixed 900 s window the CLI gave every start and stop.

```yaml
request_deadline: 30m
timeouts:
  initialize: 20m   # omitted = derived
  wake: 5m          # omitted = derived
```

- **Placement.** `timeouts` is a top-level deployment field beside `request_deadline`,
  in the ADR 0013 style for deployment-level operational settings. It is not part of
  `engine_config`: it bounds lifecycle operations and changes nothing the engine is
  given, so it is not in the recipe fingerprint. It is part of the deploy command's
  identity (normalized to milliseconds) only when declared.
- **Derivation (placeholder).** When a field is omitted, it is derived from the
  checkpoint's weights bytes (§7), counted in decimal GB and rounded up:
  Initialize = 120 s + 10 s per GB (plus a first-start allowance for vLLM and SGLang,
  amendment A7), capped at 1800 s; wake = 60 s + 5 s per GB, capped
  at 900 s. While the digest is pending (including the zero-weight placeholder a
  provisional revision is frozen with), Initialize is 900 s and wake 900 s. The
  formula is recomputed after the M16 measurements.
- **Bounds.** The store refuses any Initialize or Stop whose deadline lies beyond the
  revision's request deadline (SPEC §6). A declared timeout above the request deadline
  is therefore refused at resolution, and so is an Initialize below 30 s or a wake
  below 10 s. A derived value is lowered to the request deadline.
- **Provenance.** The effective configuration records `timeouts.initialize_ms`,
  `timeouts.wake_ms`, a `provenance` of `declared` or `derived` per field and, when
  anything is derived, the `basis` (`checkpoint_weights` or `checkpoint_digest_pending`)
  (T14). A revision frozen before this amendment has no `timeouts`; it still decodes,
  and its values are derived again from the same facts.
- **Use.** A start that names no deadline (CLI `start deployment`, `start instance`,
  `deploy model --activate`, and router activation) is given the resolved Initialize
  timeout; a stop is given 900 s lowered to the request deadline. Both are lowered to
  the smallest request deadline any host resolved the revision with, so every
  acceptance invariant holds. `--initialize-timeout <duration>` on those start
  commands wins over the deployment's value, up to the request deadline. The
  coordinator's own `initialize_timeout` becomes a 3600 s ceiling; the step deadline
  bounds each Initialize. Status reports the windows under each deployment's
  `timeouts`, and `validate config` checks the block (with `--host`, it shows the
  resolved values).
- **Not yet consumed.** No coordinator operation wakes a parked engine yet, so
  `timeouts.wake` is resolved, validated and shown but bounds nothing until one does.
  A non-provisional revision accepted while its digest was pending keeps the pending
  values until its next revision; the frozen revision is not rewritten under launches
  that may reference it.

## Amendment A2 — startup memory budget (owner decision 2026-09-23)

§5's derived cold phase is no longer the request. `engine_config.memory.startup` declares
the per-instance startup peak (at least `memory.request`; refused beside a declared
`resources:` block, whose cold phase states it). Undeclared, the derived cold phase is the
placeholder `max(request, weights × 1.6 + margin)` (the request while the weights are
unknown), with provenance `derived` under `memory.startup`, until a first run on a host
measures the peak; the store then reserves the measured peak for later starts of that
revision on that host and installation. Ready, parking and wake still equal the request. A
revision resolved before this amendment records no startup peak; it still decodes and its
cold phase stays the request. The launch plan carries the reserved peak
(`SingleLaunchPlan.startup_bytes`) so the host charges the same peak for a starting launch.
`validate config` checks the field and shows `startup` with its provenance; status shows
it per deployment (with every measurement) and per starting instance. See ADR 0015's
2026-09-23 amendment for the per-host activation gate. The placeholder factor is not a
measurement; nothing here is qualified live.

## Amendment A3 — vLLM `--speculative-config` approved key by key (owner decision 2026-09-25)

Found live in the 2026-09-25 single-box benchmark (`docs/benchmarks/2026-09-25-single-box.md`).
§8 listed `--speculative-config` among the path options. Its value is a JSON object, so no
value could lie inside `security.approved_paths`. Every vLLM speculative deployment was
refused at deploy time. The launch-time gate refuses any structured value named as a
path, so it would have been refused at launch too. The owner accepted the fix below.

The option keeps named approval: `security.approved_options` must list it. When it is
approved, its value must be a JSON object that meets three conditions:

- every key is on a closed list: `method`, `model`, `num_speculative_tokens`,
  `draft_tensor_parallel_size`, `prompt_lookup_max`, `prompt_lookup_min`,
  `draft_sample_method`, `moe_backend`;
- every value is a string, number or boolean (no nested object or list);
- the draft `model`, when named, is an absolute path inside
  `security.approved_paths`, checked lexically at deploy time and, at launch,
  through every existing symlink (as for path options).

Any other key is refused, for example a tokenizer, a revision, a quantization or a
nested draft configuration. This closes every path the value could carry. Both gates
apply the same rule: `engine_policy.rs` (`Sensitivity::SpeculativeConfig`) at deploy
time, and `runtime/extra_args_policy.py` on the parsed destination at launch. A new key
that vLLM adds needs this list amended; until then it is refused.

Evidence: `speculative_config_is_admitted_key_by_key` (mllm-config) and
`test_speculative_config_is_checked_key_by_key` (runtime). These are CPU tests only.
Live, MTP, DFlash, DFlash2, DSpark and the Gemma 4 assistant ran through this gate on
vLLM 0.29.0. That is not qualification of any recipe.

## Amendment A4 — the minimal deployment file (owner decision 2026-09-25)

A deployment document needs three fields:

```yaml
name: my-model
engine: vllm                 # the runtime profile; `runtime_profile` stays the long form
model: Qwen3-4B              # or an absolute path, ~/models/Qwen3-4B, or {hf: owner/repo[@commit]}
```

Every other field is optional and, when stated, means exactly what it meant before; a
full document resolves byte for byte as it did. Absent fields are defaulted in shared
code (`mllm_config::deployment_defaults`), the same for server deployments and
standalone (a server plus one host):

- **From the document alone**, inside the strict deployment parse, so the CLI, the
  management API, the store and the agent read the same completed document:
  `schema_version: 1`, `kind: deployment`, `routes: [<name>]`,
  `runtime_profile: <engine>`, `recipe: standard`, `recovery: reconcile`,
  `model.revision: "1"`, and `model.content_fingerprint: measured`. That value is a
  label, not a digest: mllm measures the checkpoint's digest on the host (§7) and
  records it. A stated `sha256:<64 hex>` stays an expectation the measurement must
  match (a different measurement is recorded `mismatch`).
- **Model shorthands.** `model: <path>` is a local path: relative to the host's models
  directory, or absolute. A leading `~/` is expanded by the CLI against the home of
  the user who runs it; a document that still carries one is refused.
  `model: {hf: owner/repo@<commit>}` is a pinned Hugging Face source (ADR 0008).
  Without a commit (or with a branch or tag after `@`), `mllm deploy model --file`
  pins it to the commit the reference names now, asking the Hugging Face API at
  `HF_ENDPOINT` (default `https://huggingface.co`), so the server only ever stores a
  pinned source. `mllm validate config` never contacts the network and refuses an
  unpinned reference with the way to pin it. Private repositories are pinned by hand.
- **From the host**, where the deployment becomes one host's document
  (`instances::assign_devices`, which acceptance and `validate config --host` run):
  an engine family name (`vllm`, `sglang`) that is not a profile name stands for the
  host's one profile of that family (standalone publishes its installation as
  `local`); `runtime_profile_revision` is the revision the host publishes; `devices`
  is the lowest-index GPU with the sharing the host states. On a host whose GPUs are
  device domains, acceptance resolves an undeclared deployment once per GPU and
  placement picks one (discrete GPU design §7); the default is the host's own row.
- **From the host and the checkpoint**, at resolution: `residency` is `restart_only`
  when the profile opted out of deep parking (ADR 0012), `deep` on a unified host,
  and on a discrete host `host_backed` when the weights plus the engine's host
  overhead fit what the system domain holds parked, otherwise `deep` (discrete GPU
  design §5). A deployment that states no memory and no `resources` gets
  `memory.kv_cache = min(4 GiB, managed_limit / 4)` of the domain it runs in, and its
  request derives from the weights (§5), sized for the card on a discrete host (the
  standalone template's rule). Both are named `mllm default` in the engine
  configuration's provenance (`residency`, `memory.kv_cache`), so a provisional
  revision re-resolved with the measured weights chooses them again (§7) and a
  snapshot of it decodes exactly.

`mllm validate config` prints the completed document (`document`); with `--host` it
prints the host's document and the resolved profile revision, devices, residency,
memory and provenance. SPEC §16 notes that its examples state every field and that
three are required.

Consequences: a minimal deployment is always provisional at acceptance, since its
request derives from weights not yet measured, and activation waits for the digest
(`checkpoint_digest_pending`). Tests are CPU and Fake-engine tests; nothing here
qualifies an engine recipe.

## Amendment A5: Hugging Face link chains (owner decision 2026-10-01)

Problem: recent `huggingface_hub` versions store a snapshot file as a link to a shared
blob, which can itself be a link, so a two-hop chain was refused as `unsafe_file` and the
model could not be measured.

Rule: a link chain of at most 8 hops is followed. Each hop is resolved lexically against
the directory holding the current link and opened from the store descriptor with
`O_NOFOLLOW`. Every hop must stay inside the store and the chain must end at a regular
file. A directory, an escape from the store, a loop or a ninth hop is `unsafe_file`.

Manifest identity: an entry records the first link's path and the final file's identity
(size and SHA-256), so digests recorded before this amendment are unchanged.

Gate wording: a checkpoint that cannot be read or resolved reports `could not be measured
(<reason>)`; `does not match its recorded digest` is kept for a measured digest that
differs from the recorded one.

## Amendment A6: draft model weights count with the checkpoint's (2026-10-02)

Problem: found live on 2026-10-02 with vLLM 0.30, Qwen3.8-27B NVFP4 and the DFlash2
draft model. §5 derives the memory request, the KV cache and the startup placeholder
from the checkpoint's weights. A speculative deployment also loads a draft model, and
its weights were counted nowhere: the engine was given a KV cache sized as if the
draft model took no memory, and its peak (53.0 GiB) went above its 49.2 GiB
reservation.

Rule: the weights a revision is sized with are the checkpoint's weight files plus the
draft model's: the directory named by vLLM's `--speculative-config` `model`, SGLang's
`--speculative-draft-model-path` or TensorFold's `--drafter`, in the profile's or the
deployment's arguments (the last one named wins). The draft model must already lie
inside `security.approved_paths` (§8); it is sized with the same confined stat walk as
the checkpoint, inside the approved directory that holds it. Both the embedded host
(standalone) and a remote host count it, whether sizing or measuring. The digest stays
the checkpoint's own; the draft model is not digested. MTP heads live in the checkpoint
and add nothing.

Everything §5 and amendment A2 derive from the weights follows: a derived request grows
by the draft model's weights, a derived KV cache (declared request) shrinks by them, so
the engine's explicit KV budget (`--kv-cache-memory-bytes`, SGLang's static fraction)
leaves room for the draft model, and the startup placeholder grows with them. A
deployment that declares both `memory.request` and `memory.kv_cache` states its own
budget and must leave room for the draft model itself. A host launching a revision
whose plan names the checkpoint's weights alone (recorded before this amendment)
accepts it, as it was sized then.

Live (2026-10-02, vLLM 0.30, Qwen3.8-27B NVFP4 with DFlash2, `kv_cache: 16GiB`): the
recorded weights were 25.77 GB (21.92 GB checkpoint plus 3.85 GB draft model), the
derived request 48.0 GiB and the derived cold phase 49.25 GiB. The first start peaked
at 50.46 GiB (status' measured peak), 1.2 GiB above the placeholder; the CUDA graphs
alone took 1.64 GiB against the 1.25 GiB overhead placeholder. As amendment A2
provides, the next start reserved the measured 50.46 GiB and peaked at 50.18 GiB;
steady use was 48.97 GiB against the 49.25 GiB Ready charge. Without the draft model
counted, every phase would have been 3.58 GiB smaller.

## Amendment A7: first-start allowance in the derived Initialize window (2026-10-02)

Problem: amendment A1 derives Initialize as 120 s plus 10 s per GB of weights. vLLM 0.30
warms up on a first start (torch.compile, FlashInfer autotuning, CUDA graph capture) and
caches the result: its first start of Qwen3.8-27B NVFP4 (22 GB) spent 236 s warming up
after loading, past the 340 s the formula gave, while recipes declared 900 s. Rerun on
2026-10-02 with DFlash2: warmup 235 s (41.6 s of it compilation) on the first start,
79 s on the next.

Rule: for vLLM and SGLang the derived Initialize window is the load term plus a
first-start allowance of 480 s (about twice that warmup), capped at 1800 s and lowered
to the request deadline as before. The pending value (900 s) and TensorFold's
first-build bound (ADR 0023 §4) are unchanged. A later start, with the compile cache
warm, is shorter; the longer window only delays noticing an engine that is alive but
stuck, never one that exited. A revision frozen before this amendment keeps the
window it was frozen with (it still decodes exactly).

## Amendment A8: first-start graph allowance in the startup placeholder (2026-10-02)

Problem: found live on 2026-10-02 (amendment A6's run). The first start of vLLM 0.30
with Qwen3.8-27B NVFP4 and DFlash2 peaked at 50.46 GiB against a 49.25 GiB cold phase.
Amendment A2's placeholder is `max(request, weights × 1.6 + margin)`; with a 16 GiB KV
cache the request (48.0 GiB) was the larger term, so the cold phase was the request plus
the 1.25 GiB CUDA context and graph charge (ADR 0019). The CUDA graphs alone took
1.64 GiB, the draft model's included, and they are captured after the KV cache is
allocated, so they sit on top of the request, not inside the load term. A start above
what CapyCTL reserved is what the startup budget exists to prevent.

Rule: for vLLM and SGLang the derived placeholder startup is
`max(request + graphs, weights × 1.6 + margin)`, where `graphs` is a first-start graph
allowance of 1.25 GiB per model whose graphs the engine captures: the checkpoint, plus
the draft model when the arguments name one (amendment A6's options). TensorFold declares
its resources and is unchanged. The cold phase is that startup plus the CUDA context and
graph charge, as before; Ready, parking and wake are unchanged. The allowance is recorded
as `startup_graphs_bytes` beside `startup_bytes`, so a snapshot re-derives it; a revision
frozen before this amendment records none and re-derives without it. A declared
`memory.startup`, a declared `resources:` block and a device request (discrete GPU design
§3) carry no allowance. As before, the first measured peak replaces the placeholder for
later starts of the revision on that host and installation.

For the A6 deployment the cold phase becomes 51.75 GiB (48.0 + 2 × 1.25 + 1.25), 1.29 GiB
above the measured first-start peak; without a draft model it is 1.25 GiB larger than
before. The allowance is a placeholder, not a measurement.

## Amendment A9: the fitted context counts the draft model's KV (2026-10-02)

Problem: found live on 2026-10-02. With no memory stated, the Qwen3.8-27B NVFP4 and
DFlash2 deployment got the default 4 GiB KV cache and a context fitted to 32752 tokens
from the checkpoint's layers alone, and vLLM 0.30 refused it ("4.2 GiB KV cache is
needed, which is larger than the available KV cache memory (3.98 GiB)"). The draft
model's KV layers share the engine's pool: vLLM groups them with the checkpoint's, and
SGLang adds the draft pool's bytes per token to the target's.

Rule: when the arguments name a draft model (amendment A6's options) on vLLM or SGLang,
the fit (§5, owner decision 2026-09-25) reads the draft model's `config.json` where the
checkpoint is read, counts its layers the same conservative way, and adds its KV per token
to the checkpoint's, with the deployment's KV dtype for both. A draft model whose
configuration cannot be read falls back to 4096 tokens with the reason. A declared
`context_length` always wins; the fit warns about a declared one only when both shapes
are exact. Status names the draft model in the fit's reason.

Open issue (owner decision needed): on a hybrid checkpoint (linear-attention or Mamba
layers) vLLM also needs, beyond the KV per token, one recurrent-state block per sequence
(`max_num_seqs`, 256 by default) and, per request, 2 + speculative-tokens state blocks
per recurrent layer group. Neither is sized from the KV per token. The default 4 GiB KV
cache is too small for Qwen3.8-27B NVFP4 on vLLM 0.30 with or without a draft model
("max_num_seqs (256) exceeds available Mamba cache blocks": 83 without DFlash2, 254 with
it). A deployment of such a model declares `memory.kv_cache` (16 GiB works) until the
default is decided.

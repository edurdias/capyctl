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
configuration (T14): SGLang CUDA graphs off while the memory saver is on (until A13), SGLang
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

## Amendment A8: the startup placeholder covers graphs and the load transient (owner decision 2026-10-02)

Problem: found live on 2026-10-02. A first start must never exceed what CapyCTL
reserved, and two placeholders did, both for Qwen3.8-27B NVFP4 on vLLM 0.30.

- **Graphs above the request.** With DFlash2 and a 16 GiB KV cache, the first start
  peaked at 50.46 GiB against a 49.25 GiB cold phase. Amendment A2's placeholder,
  `max(request, weights × 1.6 + margin)`, was the request (48.0 GiB). The CUDA graphs
  (1.64 GiB, the draft model's included) are captured after the KV cache is allocated,
  so they sit on top of the request, beyond the 1.25 GiB context and graph charge
  (ADR 0019).
- **Load transient above 1.6 × weights.** With the default 4 GiB KV cache the load term
  was the larger one, and loading dropped MemAvailable further. Without a draft model
  (20.42 GiB of weights) the peak was 47.86 GiB against 41.92 GiB, and 50.49 GiB
  (2.08 × weights plus 8 GiB) on a rerun. With DFlash2 (24.0 GiB) it was 49.12 GiB
  against 47.65 GiB.

Rule:

- For vLLM and SGLang the derived placeholder startup is
  `max(request + graphs, weights × 2.25 + margin)`.
- `graphs` is a first-start graph allowance of 1.25 GiB per model whose graphs the
  engine captures: the checkpoint, plus the draft model when the arguments name one
  (amendment A6's options).
- TensorFold declares its resources and is unchanged.
- The cold phase is that startup plus the CUDA context and graph charge, as before;
  Ready, parking and wake are unchanged.
- The allowance is recorded as `startup_graphs_bytes` beside `startup_bytes`, so a
  snapshot re-derives it. A revision frozen before this amendment records none and
  re-derives exactly as it was (no allowance, factor 1.6).
- A declared `memory.startup`, a declared `resources:` block and a device request
  (discrete GPU design §3) carry no allowance.
- As before, the first measured peak replaces the placeholder for later starts of the
  revision on that host and installation.
- Both terms are placeholders, not measurements.
- Owner decision 2026-10-02: new revisions use the 2.25 factor; revisions frozen
  before this amendment keep 1.6.

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

## Amendment A10: vLLM sequences and hybrid checkpoints (owner decision 2026-10-02)

Problem: found live on 2026-10-02 after amendment A9. On a hybrid checkpoint (attention
beside gated-delta-net layers) vLLM keeps attention KV and recurrent state in one pool of
uniform pages. It needs one state block per sequence (`max_num_seqs`, 256 by default) and,
per request, 2 + speculative state blocks per recurrent layer group. With the default
4 GiB KV cache, Qwen3.8-27B NVFP4 on vLLM 0.30 refused to start: "max_num_seqs (256)
exceeds available Mamba cache blocks", 83 blocks without DFlash2 and 254 with it. With
DFlash2 it also held at most 26368 tokens, where the per-token fit gave 32752 (30384 with
the draft KV of A9).

Rule:

- vLLM is started with `--max-num-seqs` equal to CapyCTL's per-deployment in-flight
  bound (32, `capyctl_domain::launch::MAX_REQUESTS_PER_DEPLOYMENT`, the same constant
  the router enforces), unless the deployment sets `max_concurrent_requests` or the
  installation's host-fixed arguments set `--max-num-seqs`.
- For a gated-delta-net hybrid on vLLM, the fit follows vLLM's layout (vLLM 0.29 and
  0.30, `_get_kv_cache_groups_uniform_page_size`), read from the configuration:
  - the attention block is the recurrent state's size in attention pages, in 16-token
    steps (1568 tokens for this model, 1648 with 7 speculative tokens);
  - the group size is the smallest layer kind's count, or the largest when it is under
    1.5 times the smallest;
  - a block is one page of every layer of a group.
- The grant must hold one block per sequence plus two left unused. When it does not, the
  fit falls back with a reason naming the KV cache those sequences need.
- The context is then the largest whole number of blocks that leaves each recurrent
  group 2 + 2 × (speculative tokens + 1) state blocks. A sliding-window draft layer is
  counted as holding the whole context.
- Any other shape keeps the per-token fit.
- At 4 GiB this fits 117600 tokens without DFlash2 and 23072 with it. Live: both started,
  answered, and stayed inside their reservations (amendment A8).

SGLang is unchanged. SGLang 0.5.20 sizes its recurrent-state pool from its memory budget
and then caps its running requests to what that pool holds, so a `--max-running-requests`
default would not change what it can hold. Live at the default 4 GiB KV cache:

- Without DFlash2 it started, with its running requests capped at 1.
- With DFlash2 it refused to start ("Not enough GPU memory for hybrid (mamba/linear-attention)
  state cache": 2.30 GB left, 146.81 MB of state per request, kept for every draft token).

A hybrid SGLang deployment with a draft model declares `memory.kv_cache` (16 GiB works).

## Amendment A11: a checkpoint outside the model store (2026-10-03)

Problem: found live on 2026-10-03. The guides name an absolute path as a model, and
resolution accepts one, but §7 measured a checkpoint only inside the host's model store.
An absolute path to a Hugging Face cache snapshot got the digest diagnostic `invalid_root`;
`start --wait` then waited out its whole Initialize window and the status table showed no
error. The launch check refused the path too.

Rule:

- A local model named by a path outside the model store is measured inside its own root:
  the hub directory of a Hugging Face cache snapshot (`<hub>/models--<org>--<name>/snapshots/<commit>`,
  whose files link to the repository's blobs, and the large ones on to the hub's shared
  blobs), otherwise its parent directory. Links follow the A5 chain rules inside that root.
  A root other users may write is refused `invalid_root` (open issue 3: the time-of-check
  mitigation is a directory other users cannot write). The launch accepts such a local
  path; a downloaded source stays inside the sources store, and a draft model inside its
  approved root (A6).
- `start --wait` and `deploy --activate --wait` end at once, nothing started, on a digest
  diagnostic that does not clear by itself (`invalid_root`, `unsafe_file`, `too_large`,
  `unauthorized`, `not_materializable`), saying what it means; `status deployment` shows it
  under the table.

## Note on §4: SGLang CUDA graphs while parking (2026-10-03)

The `cuda_graphs: false` default while the memory saver is on came from the first SGLang
recipe (docs/plans/2026-09-12-f2b-sglang-adapter.md §2: "disable prefill/decode CUDA graphs
for the first recipe"), not from a failure. Measured live on 2026-10-03 with SGLang 0.5.21
and FrogNano-4B-2609 on a 16 GB laptop GPU, `deep` residency, no memory stated: 34.7 to
36.1 tokens/s decoding with the default and 61.1 with `cuda_graphs: true`; with graphs on,
start, park, wake (10 s to the first answer) and decoding after the wake all worked. The
parked engine kept 1.57 GiB of the card with graphs on and 0.88 GiB without, above the
1 GiB parked device residue placeholder (ADR 0019 §3), because SGLang's park does not
release graph memory. The default is unchanged: turning it on needs a parked residue that
covers the graphs, recorded per revision so frozen revisions re-derive unchanged. Until
then a deployment that wants the speed states `cuda_graphs: true`. Amendment A13 records the
residue and turns the default on.

## Amendment A12: kernel builds are not part of the startup peak (owner decision 2026-10-02)

Problem: found live on 2026-10-02 while verifying SGLang 0.5.21 on a GB10 (unified
memory). A first start with an empty JIT cache built FlashInfer kernels for about
9 minutes (`MAX_JOBS` 14) and recorded a measured startup peak of 111 GiB, where the
same deployment normally peaks at about 45 GiB. The compilers' memory comes from the
pool the peak is measured in, so the measurement was the build's. Peaks merge with
`MAX` (amendment A2), so every later start of that deployment was refused
`capacity_blocked`. Starting the other SGLang environment rebuilt the kernels, because
the shared JIT cache's build files name the environment's path, and recorded 108 GiB.
vLLM, which builds FlashInfer kernels, can do the same. TensorFold builds its CUDA
extensions on its first start (ADR 0023 §4), but a TensorFold deployment declares its
`resources`, so no peak is measured for it.

Decision (owner, option A): the startup peak records the engine's own memory, not the
memory of a kernel build.

- **Where builds happen.** The owner's option A was phrased as "start recording when
  the engine begins loading weights". On the engines CapyCTL runs, that point does not
  separate builds from loading. SGLang and FlashInfer build each kernel the first time
  it is used, which is after weight loading has begun (SGLang's KV cache, rotary and
  speculative sampling kernels, FlashInfer's attention and GEMM modules). TensorFold
  builds at its first start or request, and vLLM builds FlashInfer kernels during
  warmup. A log line such
  as SGLang's `Load weight begin` would still count the builds, and every engine
  version words it differently.
- **Signal.** Every one of these builds runs as child processes of the engine, in the
  process group CapyCTL launched it in: `ninja` with `nvcc`, `cicc`, `ptxas`, `c++` or
  `cc1plus` under it. The signal is the same for every engine: a compiler in the
  engine's process group. While an Initialize runs, its host checks the group every
  500 ms (`capyctl-launchers` `group_building`, which reads `/proc/<pid>/stat`). It
  reports each span with a compiler as a kernel build, in the host's clock, with the
  readiness result (`EffectObservation.kernel_builds`; `MemberExecutionResult` field
  15, at most 64 spans).
  - A span starts at the last check that saw no compiler and ends at the first check
    that sees none again, so it brackets the build.
  - `ninja` runs for the whole build, so the short compiler steps between two checks
    are covered.
- **Rule.** The coordinator keeps the availability samples of an Initialize and
  measures the peak from the samples outside every reported build. Samples before
  and after a build count as before. A start whose every sample fell inside a build
  records no peak, so the next start keeps the placeholder (A8) and measures then. A
  measurement still merges with `MAX`. If the engine's own peak happened during a
  build and was left out, the next start, which builds nothing, measures it and
  raises the stored peak.
- **Fallback.** A build this signal cannot see is still counted, as before this
  amendment. That covers compilation inside the engine process (Triton, NVRTC,
  torch.compile in process), a build shorter than one check between two checks, a
  build whose compiler leaves the engine's process group, a launcher that cannot read
  `/proc`, and an older host, which reports no builds. In those cases an inflated peak
  is cleared by deleting and redeploying the deployment. A reset command (option C)
  was not chosen.
- **Stored peaks.** A peak recorded before this amendment is not changed: the store
  cannot tell which of them came from a start that built kernels. A deployment that
  is now `capacity_blocked` because of one is deleted and redeployed once (release
  notes). After that, a start that builds records no build memory, so nothing new
  needs healing.

Nothing here changes admission, the placeholder or a declared peak. Only the
measurement changes.

Live (2026-10-03, GB10, Qwen3.8-27B NVFP4 with DFlash2 on SGLang): a first start that
rebuilt the JIT kernels for about 6 minutes recorded 106.5 GiB without this amendment, and
the next start was refused `capacity_blocked`. With it, a first start that rebuilt the
same kernels (availability fell to 1.8 GiB during the build) recorded 44.8 GiB, and the
next start was ready in 177 s.

## Amendment A13: the parked charge is measured; SGLang CUDA graphs stay on (owner decision 2026-10-03)

Problem: the parked phase charged a fixed placeholder, 1 GiB of the card on a discrete GPU
(ADR 0019 §3) and 2 GiB of the pool on unified memory (§5). SGLang's park does not release
its CUDA graphs, so with graphs on a parked engine held more than the placeholder (1.57 GiB
on a 16 GB card, note on §4), and CapyCTL kept the graphs off beside the memory saver,
costing about 40 % of the decoding speed.

Rule:

- When a park completes, the coordinator samples the host. The memory the parked processes
  hold (matched by the process identities the store recorded, as for resident floors, ADR
  0007) is recorded per revision, host, engine installation and memory domain, the largest
  kept, like the startup peak (A2). Only a sample taken after the park counts. Only the
  domains that hold the engine's device memory are measured: a `device` domain (GPU bytes)
  or a `unified` pool (GPU bytes and anonymous pages). The host RAM charged on a discrete
  host's system domain keeps its placeholder.
- A park of that revision on that host and installation is charged the measured residue,
  never below the placeholder and never above the Ready charge on that domain. The host
  re-checks co-residence with the revision's own parked phase (SPEC §3.1), so the
  controller never charges less than the host does. Parked owners of the revision are moved
  to the new charge in the transaction that records it.
- A deployment that declares `resources:` keeps its declared parked phase; nothing is
  measured for a restart-only one.
- Status shows `parked`: the charge on the measured domains, `placeholder` or `measured`,
  and every residue recorded.
- SGLang's CUDA graphs are no longer turned off beside the memory saver: `cuda_graphs` is
  left to SGLang (on) unless the deployment states it. `cuda_graphs: false` still turns
  them off. A revision frozen with the old default (`cuda_graphs: false`, provenance
  `capyctl default`) re-resolves with it unchanged.
- Until the first park of a revision is measured, its first park is charged the
  placeholder. A start admitted beside it on that charge is still checked against the
  card's observed free memory at launch, so an undercharge refuses or waits rather than
  overcommitting the card; the measurement follows the park within one sample.

Found live 2026-10-03 on a 16 GB laptop GPU with SGLang 0.5.21 and FrogNano-4B-2609, no
memory stated, `deep` residency: graphs on by default (captured at start), 62 tokens/s;
the park left 1564 MiB on the card and the ledger charged 1.53 GiB (status `measured`); a
wake on request answered in 13 s and decoded at 62 tokens/s again; a vLLM deployment of
11.5 GiB started beside the parked engine (14.28 GiB charged on the card's 14.71 GiB); one
of 12.25 GiB, which the 1 GiB placeholder would have admitted beside it, reclaimed the
parked engine instead (15.03 GiB with the measured residue).

Found live 2026-10-03 on a GB10 (unified memory) with SGLang 0.5.21: Qwen3-4B, nothing
stated, graphs captured at start, 22 tokens/s at one stream; its park was measured at
6.30 GiB (3.06 GiB of GPU memory and 3.2 GiB of anonymous pages; MemAvailable dropped by the
same amount), against the 2 GiB unified placeholder, so the placeholder undercharged a
parked engine on that host too. Qwen3.8-27B (NVFP4, DFlash draft, request 48 GiB) then
started beside the parked engine on the measured charge.

Recorded with it: the 27B SGLang deployment with CUDA graphs on stops a few minutes after
Ready, with or without requests (the "exits at 2 streams" of the 2026-10-03 three-engine
benchmark, where it ran `restart_only`). Not a memory or graph failure: with graphs on, the
scheduler starts a torch inductor compile-worker pool (21 processes) before Ready; the
launch records them in its process group, the pool's workers exit when idle, and the
embedded exit watcher (`engine_exit.rs`) reads any recorded member gone as the engine
exiting and stops it. With graphs off only the pool's parent is recorded and the engine
stays up. Graphs gave that deployment almost nothing (21.3 against 20.9 tokens/s at one
stream, 39.4 against 38.1 at two). The exit watcher has since stopped reading a helper's exit
as the engine's, so graphs stay on for such a deployment too.
Checked live after that fix (GB10, request 55 GiB, KV cache 16 GiB, two concurrent requests,
graphs on by default): the compile workers exited (23 processes down to 3) and the engine
stayed ready through 10 idle minutes, then served 21.3 tokens/s at one stream and 39.4 at
two. Its park was refused `the engine is not quiescent` with graphs on and with them off
alike, so that refusal is not about graphs; Qwen3-4B on the same build parked and was
measured at 6.30 GiB again. Closed by amendment A15: the refusal came from the saver
observation, not from the engine's gauges, and a speculative SGLang deployment no longer
parks.

## Amendment A14: `memory.kv_cache` is SGLang's KV pool (owner decision 2026-10-03)

Problem: found live on 2026-10-03 with SGLang 0.5.21, Qwen3.8-27B NVFP4, DFlash2 (8 draft
tokens), fp8 KV, `memory: {request: 48GiB, kv_cache: 16GiB}` and
`max_concurrent_requests: 8`. §5 gave SGLang a static pool (`--mem-fraction-static`) of the
request less the margin, and the KV cache only bounded it from below. SGLang loaded the
weights (21.2 GB, and 3.3 GB of draft model) and split what was left between the hybrid
model's recurrent-state pool and the KV pool, about 0.9 to 1. The KV pool held 211k tokens
where vLLM, given the same 16 GiB, held 322k. The state pool held 14 slots at 5 per request,
so SGLang capped the running requests at 2 ("max_running_requests is capped to 2 by the
mamba state cache").

Rule (vLLM is unchanged; it is already given the KV cache in bytes):

- The KV pool is `memory.kv_cache` divided by SGLang's KV bytes per token, passed as
  `--max-total-tokens`. The bytes per token are the KV dtype (else the deployment's dtype,
  else the checkpoint's) times key and value heads for each layer that keeps KV. A
  gated-delta-net hybrid keeps KV for its full-attention layers only. A draft model's layers
  are added in full, in the same KV dtype or `--speculative-draft-kv-cache-dtype` (amendment
  A9; SGLang's `dflash_draft_cell_size_per_token`). The fitted context (§5) of such a hybrid
  on SGLang counts the same layers.
- On a gated-delta-net hybrid, the recurrent-state pool is passed as
  `--max-mamba-cache-size`: 5 slots per running request, SGLang 0.5.20 and 0.5.21's most
  (3, plus 2 with the radix cache's extra buffer and overlap scheduling, their defaults).
  A slot is one request's state: per linear-attention layer, a bfloat16 convolution state
  of `linear_conv_kernel_dim - 1` positions and a temporal state in `mamba_ssm_dtype`
  (float32 by default, or `--mamba-ssm-dtype`).
- The running requests are the deployment's `max_concurrent_requests`. Undeclared, they are
  the most, up to CapyCTL's in-flight bound (32, `MAX_REQUESTS_PER_DEPLOYMENT`), whose state
  fits, passed as `--max-running-requests`.
- The static pool rendered is the weights (with the draft model's, amendment A6), the KV
  cache, the state and 2 GiB of SGLang's own allocations (CUDA context, workspaces, load
  buffers), never less than the request less the margin nor more than the request; what it
  takes beyond the request less the margin comes out of the margin. Found live on
  2026-10-03: a static pool of exactly weights, KV cache and state held 356793 of the
  399457 KV tokens passed, 1.7 GiB short. A discrete device keeps its static pool. The
  state must fit the request less the margin, the weights and the KV cache. The state is
  `(slots + 1) × slot`, plus `(running + 1) × draft tokens × slot` of intermediate states
  with speculative decoding (`--speculative-num-draft-tokens`), as SGLang reserves them.
- The weights are the revision's recorded weights. A revision that declares both its
  request and its KV cache records none (it is not re-resolved with the measurement), so
  the launch sums the weight files it reads: the checkpoint's and the draft model's
  (found live 2026-10-03).
- With an explicit `memory.request`, sizing is strict. When a declared
  `max_concurrent_requests` does not fit, or one running request does not, the launch is
  refused before anything starts. The refusal names the state's bytes and the memory
  request that would hold it. Where the checkpoint is read locally, the status JSON
  carries the same text as the context's `warning`.
- With a derived request (owner decision 2026-10-03), CapyCTL fits what it can. A derived
  request holds the weights, the KV cache and the margin, and nothing for the state, so on
  unified memory the state may take up to half of the margin; the other half stays for
  SGLang's runtime outside its static pool. The static pool grows by what the state takes.
  The running requests are the most, up to the declared count (or 32), that fit, and the
  launch is refused only when one request does not fit. A discrete device lends nothing
  from its margin.
- On a discrete device (found live 2026-10-04, FrogNano-4B BF16 on a 16 GB laptop GPU), a
  derived request is the weights x 1.10 plus the KV cache, its static pool exactly the
  weights and the KV cache, and its margin lends nothing, so every hybrid deployment that
  stated no memory was refused for one running request. CapyCTL chose that KV cache as
  well, so the state takes up to half of it: the running requests are the most, up to the
  declared count (or 32), whose state fits half the KV cache, the KV pool
  (`--max-total-tokens`) is the KV cache less that state, and the fitted context is held to
  that pool. FrogNano-4B there runs 7 requests at 63920 tokens of context (it fitted 120512
  and was refused). The launch is refused only when one request's state exceeds half the KV
  cache. A declared `max_total_tokens` keeps its pool and lends nothing. Open: SGLang's own
  allocations inside a discrete static pool are not modelled (it keeps its static pool), so
  that start held 42741 of the 63935 KV tokens passed and answered a longer input with a
  400 naming the limit. Closed by the note on amendment A14 below.
- `status` sizes a deployment on a device domain as the launch does, before a card total is
  observed, so its warning and a refused start name the same memory request.
- When the state holds fewer running requests than the declared count (or 32),
  `status deployment` says "Running limited to N requests by the state cache", and the
  status JSON carries `context.running_limit` (standalone; a remote host decides at launch).
- Arguments that size the state pool themselves (`--max-mamba-cache-size`,
  `--mamba-full-memory-ratio`, in the deployment's or the installation's arguments) win:
  CapyCTL then passes neither the state pool nor the running requests, and still passes
  the KV pool. A declared `max_total_tokens` is the deployment's own KV pool.
- What this does not model is left to SGLang's own sizing, as before, with the reason: a
  sliding-window or latent-attention checkpoint, another hybrid kind, an unreadable
  configuration, an unknown KV dtype, speculative decoding with no draft-token count. When
  the weights are unknown, only a declared count's state is passed.
- Dense models keep their memory request; only `--max-total-tokens` is new for them.

Consequence: an explicit memory request for a hybrid deployment has to hold its state. The
recipe above needs about 16.2 GiB more for 8 running requests with DFlash2 (113 slots of
146.81 MiB). Without a stated request, Qwen3.8-27B at the default 4 GiB KV cache runs 5
requests (it ran 1 before), and with DFlash2 at that default it runs 1 (it was
refused before). Amendment A10's note that SGLang sizes its state itself no longer applies.

Follow-up: a derived request should include the state for its running requests. That needs
the state's bytes as a checkpoint fact beside the weights (measured by the host that reads
the checkpoint, sent with the measurement, recorded with the revision), since resolution
reads no checkpoint file. Until then the margin lends it.

## Note on amendment A14: SGLang's fraction on a discrete GPU (2026-10-04)

Problem: the open item of amendment A14. On a 16 GB laptop GPU, FrogNano-4B BF16 with no
memory stated showed a context of 63920 tokens, but SGLang 0.5.21 held 42778 KV tokens of
the 63935 passed and refused any input over 42772 tokens. With `memory.kv_cache: 2GiB` it
held 44328 of 65536. The gap was the same, about 0.65 GiB, at both sizes.

Cause, read in SGLang 0.5.21 (`KVCacheConfigurator._profile_available_bytes`): SGLang sizes
its pools from `mem_fraction_static` times the GPU memory free when its scheduler starts
(after its own CUDA context; the driver's reserve and other processes' memory are not in
it), less what is already allocated (the weights) and a multimodal reservation
(`SGLANG_VLM_CACHE_SIZE_MB`, 100 MiB, for a multimodal checkpoint such as Qwen3.5's). CapyCTL
renders the fraction against the card's total (ADR 0019). Measured: the card totals 15.99
GiB, SGLang's baseline was 15.24 GiB, so a fraction of 0.7727 gave its pools
0.7727 × 0.75 GiB less than the static pool, plus the 100 MiB. CUDA graphs, the sampler and
the chunked-prefill activations are not part of it: they are allocated after the pools,
outside the static fraction.

Rule: on a discrete GPU, when CapyCTL fixes SGLang's pools (the KV pool in tokens, and on a
hybrid model the state pool in slots, by CapyCTL or `--max-mamba-cache-size`), the
fraction is rendered from the static pool plus 1 GiB (`DISCRETE_BASELINE_ALLOWANCE_BYTES`,
the closed memory object's `static_allowance_bytes`), never above 0.9999 of the card. SGLang
allocates only the pools it is told, so the allowance lets its profile reach them and takes
nothing more. The status context and the KV pool are unchanged. Unified memory keeps its
static pool with SGLang's own allocations (amendment A14), a pool left to SGLang's sizing
(an unmodelled shape, a state pool sized by `--mamba-full-memory-ratio`) gets no allowance,
and so does vLLM.

Live (2026-10-04, the same laptop, SGLang 0.5.21, FrogNano-4B BF16, nothing stated): the
fraction rose from 0.7727 to 0.8352, SGLang held all 63935 KV tokens passed
(`max_total_num_tokens=63935`), status showed a context of 63920 and 7 running requests, a
63820-token prompt (the context less 100) and a 63910-token one were answered, and the card
held 14478 MiB, inside the 14.5 GiB CapyCTL charged. SGLang's longest input is the context
less 6 (`max_req_input_len`).

Not covered in full: other processes' memory on the card when SGLang starts (a parked
engine's residue, for one) is outside its baseline too. The allowance covers the fraction
of about 1 GiB in all; beyond that SGLang's KV pool is short again by the fraction of the
rest, as before this note.

## Amendment A15: SGLang does not park a speculative deployment (owner decision 2026-10-03)

Problem: the open issue of amendment A13. Qwen3.8-27B (NVFP4, DFlash2) on SGLang 0.5.21 was
refused every park with `the engine is not quiescent` while SGLang's running and waiting
gauges read 0. The embedded check before a park needs both the gauges at zero and an
enrolled memory saver that is real and fully mapped, and reports either failing as not
quiescent. The saver half failed: the scheduler observation reads only the single-rank
topology the recipe renders, and that topology has no speculative algorithm, so the
DFlash2 scheduler failed the check when it enrolled. The enrollment then stopped without a
word, no record was written, and the host had no saver evidence. Qwen3-4B, with no draft,
enrolled and parked.

The topology check is right to refuse it. Read in SGLang 0.5.21's source: a park pauses the
draft model's weights with the target's (the same `weights` region), and a `deep` wake
sends `update_weights_from_disk` with the target's checkpoint to every weight runner, the
draft's included, so the draft would wake without its weights. The draft's host-RAM backup
(`--enable-draft-weights-cpu-backup`) is not part of the recipe either.

Rule:

- An SGLang deployment whose arguments (the installation's or the deployment's
  `extra_args`) name `--speculative-algorithm` defaults to `restart_only`, as TensorFold
  does (ADR 0023 §6). The default is named in the provenance, so an existing revision
  re-resolves to it.
- Such a deployment that states `deep` or `host_backed` is refused when it is resolved, as
  an invalid configuration (`unsupported combination at residency`) whose message gives
  the reason and `restart_only` as the way out.
- vLLM's speculative deployments are unchanged.
- An enrollment that stops part way writes one line to the engine's log,
  `capyctl_saver_enrollment_refused`, with a fixed stage word (`arguments`, `library`,
  `install`, `listen`, `record`), the exception's class name and a binding error's closed
  code (`topology` here), never its message, a path or a credential. A launch outside an
  observation scope, or without the memory saver, logs nothing.

Evidence: `crates/capyctl-config/tests/sglang_speculative_residency.rs` and
`test_a_refused_enrollment_names_its_stage` (runtime).

Live (2026-10-03, GB10, SGLang 0.5.21, Qwen3.8-27B NVFP4 with DFlash2, request 55 GiB, KV
cache 16 GiB, two running requests):

- Before: the deployment resolved `deep` and started with the memory saver on and
  `speculative_algorithm` `DFLASH`. The host's observation directory stayed empty. Both
  gauges read 0.0, and the park was refused with the host log line
  `saver_observation_unavailable` / `record_unreadable`.
- Started with this change's runtime, the engine log carried
  `capyctl_saver_enrollment_refused` with stage `install`, error `BridgeError` and code
  `topology`.
- After: the same file resolved `restart_only` (memory saver off, no refusal line), served
  requests, and stopped and started again (258 s to ready). A park request is answered
  `unsupported_capability`, as for any `restart_only` deployment. The same file with
  `residency: deep` was refused at deploy with the reason.

Follow-up: parking a speculative SGLang deployment. It needs the topology check to admit the
speculative algorithm, a wake that restores the draft's weights (its own path on a disk
reload, or the draft's host-RAM backup), and a live check that drafts are still accepted
after a wake. SGLang 0.5.20 also could not reload ModelOpt (NVFP4) weights from disk (ADR
0019 §5), so for this checkpoint the wake has to be proven on the installed build.

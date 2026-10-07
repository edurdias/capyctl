# Current implementation and launch status

## A launch whose parked charge keeps growing stops instead of parking — 2026-10-07 (branch `park-growth-guard`)

Owner decision 2026-10-07 (catalog finding 2, ADR 0014 amendment A18). On the catalog run
(GB10, vLLM 0.30.0, Qwen3.8-27B NVFP4 sized for 32 GiB) one launch's measured parked charge
grew 4.7, 9.6, 13.4 GiB over three parks while its GPU memory returned to 22.1 GiB on every
wake. Each measured park is now also recorded per launch (store schema v44,
`parked_launch_residues`: first and latest charge per domain). When the latest exceeds the
first by more than the host's `resource_policy.parked_growth_limit` (default `auto`, 100 % of
the first charge; or `N%`, a size, `off`; YAML, `--set` and `CAPYCTL_SET__…`, standalone under
`host.`), the next park of that launch is an ordinary stop: idle (`ready_idle_parked_growth`),
switch victim, and `park deployment` (answered with the stop's operation). The stop releases
only on its own cleanup evidence. Status shows `parked.growth` (`within_limit`, `past_limit`,
`stopped`) and a `Parked` line. A host that states nothing, or `auto`, publishes and stores the
same policy and freezes the same revisions.

CPU and Fake tests only (failing before the change: the outgrown park, idle and switch cases
parked; passing after; within the bound parks as before; unchanged digests; three ways). They are
not qualification. Live check outstanding: the catalog's model 1 deployment, three park and wake
cycles on a GB10 standalone with the default bound; the third park should be a stop
(`ready_idle_parked_growth` or `park_growth_stop`) and the next request a fresh start.

## Group members reserve their share of the weights — 2026-10-08 (branch `feat/group-member-weight-share`)

Owner decision 2026-10-07 (ADR 0028 amendment of that date). A group member whose phases derive from `engine_config.memory` was sized from the whole checkpoint, so a two-host TP2 group reserved its weights twice and a 126 GiB checkpoint could never be admitted on two 121.7 GiB GB10 hosts. Each member now holds `ceil(stage / T) + replicated`, with `replicated = W - sharded` and `stage = sharded` (P = 1) or `min(sharded, ceil(layers / P) x largest_layer)`, from the checkpoint's safetensors headers (`capyctl_config::checkpoint_layout`), or 10 % of `W` kept whole without them; the margin, startup placeholder, graph allowance, SGLang state reserve and `host_backed` copy derive from the share. Declared `resources` and world size 1 are byte-identical (recipe fingerprints pinned from `main`). The share is recorded as `engine_config.memory.member` (topology, whole weights, layout) so snapshots and provisional re-resolution keep it; the host reports the layout in `CheckpointDigestEvidence.layout`, and a member's launch plan names the whole weights (verified by the host) and `checkpoint_layout` (under `engine_groups`), so the host resolves the same share and SGLang's static pool holds it.

Engine evidence (vLLM 0.30.0, SGLang 0.5.21, TensorFold 0.6.5 sources) is in the ADR amendment: vLLM and SGLang shard embeddings and head by vocabulary and keep norms, row-parallel biases, routers and latent low-rank projections whole; TensorFold keeps embeddings whole on both ranks (and GLM-5.3-Flash's head), and has no PP; every engine takes the smallest per-rank KV token capacity. Not modeled: key-value projections replicated when KV heads < TP; the fitted context and hybrid state slot still count the whole model per token (conservative). The placeholder startup (2.25 x share) still exceeds a GB10's 97.4 GiB managed limit for the 126 GiB case; such a deployment declares `memory.startup`.

CPU tests only (config resolution TP2, PP2, TP2xPP2, fallback, provisional re-resolution, Flash-Next-sized fit, declared and single-host fingerprints; store measurement re-resolving both members; protocol plan and evidence validation). Failing first on `main`: the TP2 member charged 107374182400 bytes of weights against an expected 59055800320. Not run live; MN1–MN9 should confirm the per-member grants and each rank's measured peak against them.

## SGLang's static pool on unified memory — 2026-10-07 (branch `fix/sglang-static-fraction-weights`)

Owner decision 5 (b), note on ADR 0014 amendment A14. In the GB10 catalog runs (SGLang
0.5.21) a `memory.kv_cache` of 2 GiB or less failed at start ("Loaded weights leave no GPU
memory for the KV cache") on gpt-oss-20b, Gemma 4 26B-A4B and Qwen3.6-35B-A3B with
`--max-mamba-cache-size 8`. Cause: CapyCTL added its 2 GiB for SGLang's own allocations to
the static pool only when it sized the pools; those launches rendered exactly the weights
on disk and the KV cache, while SGLang 0.5.21 charges the weights as loaded, its load-time
memory and the argument-fixed state against the fraction first. Now every unified launch
with known weights renders weights + KV + state + 3 GiB (`STATIC_OVERHEAD_BYTES`, the
smallest whole GiB above the measured 1.7, 2.2 and about 2.9 GiB of the NVFP4 models),
capped at the request, and an explicit `--max-mamba-cache-size N` counts its `N + 1` slots
(and speculative intermediates). gpt-oss-20b's MXFP4 load (about 5.4 GiB) stays outside it
and keeps a stated request. CPU tests only (capyctl-config `sglang_pool`); CPU and
Fake-engine tests are not qualification. Live check pending (lab hosts reserved): model 2
(Qwen3.6-35B-A3B NVFP4) on SGLang with `memory.kv_cache: 1GiB` and with
`--max-mamba-cache-size 8` at 1536 MiB, Gemma 4 26B-A4B at `kv_cache: 1GiB`, all starting.

## The unified-memory margin grows with the weights — 2026-10-07 (branch `fix/unified-memory-margin`)

Owner decision 4 of 2026-10-07 asked whether the flat 8 GiB margin on unified memory (a
declared `memory.request` less the weights and 8 GiB is the KV cache; SGLang's static pool is
the request less 8 GiB) is intended, since a discrete GPU's has been the weights × 0.10 since
`c5ebc1a` (ADR 0019 A1). Investigation (ADR 0014 amendment A18, measured table there): on
unified memory the margin is the only charge for the engine's CPU-side memory, which a
discrete host charges separately (4 GiB on the system domain); the GB10 recipes measured
3.4 to 5.4 GiB CPU-side for vLLM `restart_only` and 4.2 to 6.0 GiB for SGLang, and up to
6.7 GiB beyond weights and KV for vLLM, so 8 GiB is justified up to 22 GiB of weights and was
not lowered. gpt-oss-120b on vLLM (60.77 GiB of weights) held 12.5 GiB beyond weights and KV
against the 9.25 GiB held (Ready charge 78.0 GiB, in use up to 81.3). New rule: on unified
memory the margin of a vLLM or SGLang request, declared or derived, is
`max(8 GiB, weights × 0.15 + 4 GiB)`; it changes nothing below 26.7 GiB of weights, so no
catalog recipe changes; frozen revisions (recording 8 GiB) re-resolve as they were
(`CheckpointFacts::legacy_family_margin`, which replaces `legacy_device_margin`). The SGLang
gpt-oss-20b recipe's 32 GiB came from SGLang loading the MXFP4 weights as 17.1 GB against
13.8 GB on disk, which is SGLang sizing's (passed to that work), not the margin's. Agreed with
the SGLang static-fraction fix: the margin's value is this change, what SGLang counts inside
its static pool is that one. CPU tests (T14, T26): the boundary at 28,633,115,400 bytes,
gpt-oss-20b unchanged, gpt-oss-120b derived and declared, a frozen revision's exact decode,
and the #68 discrete case unchanged. Not run live; the check to schedule is gpt-oss-120b on
vLLM on a GB10 (kv 8 GiB, startup 90 GiB, 96 GiB managed limit): the Ready charge (83.1 GiB)
should hold the ready footprint.

## vLLM loader under deep parking is a deployment setting — 2026-10-07 (branch `feat/vllm-load-strategy-setting`)

Owner decision 1 of 2026-10-07 (option B), recorded as the note on ADR 0014 §3 and §4.
`engine_config.vllm.safetensors_load_strategy` (`eager` or `lazy`) chooses vLLM's loader;
omitted, a parking deployment still renders `eager` and a non-parking one renders nothing,
and the command fingerprint and resolved configuration are unchanged (pinned). The raw
`--safetensors-load-strategy` in `extra_args` is refused with or without sleep mode. Origin
of the default: commit 785b887 (vLLM 0.29, Qwen3-4B, wake 57 s to 7.5 s); catalog evidence
against it on vLLM 0.30 NVFP4: about 15–17 GiB more held once loaded, wake about 55 s.

CPU tests only (rendering under and outside sleep mode, refusal of the raw option and of
other values and families, snapshot round trip, pinned identity). Not qualification; a live
deep-park run with `lazy` is still needed.

## gpt-oss refuses parking on vLLM and SGLang — 2026-10-07 (branch `gptoss-deep-refusal-wake-bound`)

Owner decision 3 (2026-10-07), from the recipe catalog on host B (CapyCTL 7e50aa9, GB10,
standalone, `openai/gpt-oss-20b`). vLLM 0.30.0 deep wake: the reload logs `OAIAttention:
Failed to load weights` for 24 layers, then `Harmony parser ended in a non-terminal state`;
the request sent to the parked deployment was answered after 778 s with
`{"code":"engine_error","message":"backend completion unverified"}`. SGLang 0.5.21:
`update_weights_from_disk` raises `TypeError: default_weight_loader() got an unexpected
keyword argument 'weight_name'` (answered `activation_uncertain` after 6 s).

`EffectiveDeployment::deep_wake_refusal` (SPEC §§6.2, 9.1, ADR 0010) now also refuses
`deep` and `host_backed` for a checkpoint whose `config.json` names a family in
`DEEP_WAKE_BROKEN_FAMILIES` for the launch's engine (`model_type`, else `architectures`, as
parser defaults match): gpt-oss on vLLM and on SGLang, `capability_missing:deep_park`. The
agent (`launch_capability`, `park_capability`) and standalone's `CapabilityGate` both
apply it before any effect; `restart_only` and SGLang `resident` parks are unaffected. The
effective deployment carries no engine version, so each entry records the versions it was
observed on and is lifted by deleting it after a live park-and-wake on a fixed engine.
A machine that does not see the checkpoint (a server whose host holds it) cannot read the
family; the launching host refuses.

The 778 s was not a request held by a failed wake. The status after it
(`status-woken-1.json`) shows the restore operation succeeded and the instance ready: the
8-token readiness probe after the wake got non-empty output, so the wake counted as
usable, and the request (no `max_tokens`) was forwarded to the mis-reloaded engine
[INFERENCE: it decoded to the 32,768-token context at about 46 tokens/s, ~700 s, and the
relayed stream then failed validation, `Refused::Uncertain`]. A restore that fails or is
retained uncertain already answers joined requests as soon as it is recorded
(`wait_terminal`; the SGLang case above). Live check still owed: on host B, the engine log
timestamps of the restore and of the Harmony line for that run.

Tests (T22 T21), failing before and passing after: `gpt_oss_refuses_both_parking_tiers_on_vllm_and_sglang`
(config), `a_gpt_oss_checkpoint_refuses_deep_on_vllm_and_sglang_but_not_restart_only`
(agent), `a_gpt_oss_launch_is_refused_its_launch_park_and_restore` (controller); plus
`the_gpt_oss_rule_leaves_restart_only_other_families_and_resident_parks`. CPU tests only;
they are not qualification, and no live run was made after the change.

## Status startup figure and a role-shutdown test race — 2026-10-07 (branch `fix/status-startup-provenance-and-shutdown-race`)

Owner decision 5 (a): the deployment's `startup` in status now uses the figure admission freezes into the next start's plan (`frozen_plan_startup`): a peak measured for the revision on its host and installation shows as `measured`, else the weighed placeholder or the revision's own budget as before. Found in the catalog runs: an SGLang deployment kept showing its 55.2 GiB default after a 29.7 GiB peak was measured. Owner decision 5 (d): `role_shutdown::remote_signals_restart_and_drain_host_stops_with_cleanup` deployed once the host was `online`, which the session sets before the host's inventory and resource policy are stored, so under load the deploy got `reconciliation_required`; it now waits for `session.reconciled` and `eligible` (SPEC §4.2). CPU tests only (the T29 measured-peak test failed first on the stale figure; the role test passed 10 of 10 beside a crate build); not qualification. Live check: on the catalog host, `capyctl status deployment <sglang model> --format json` after its first Ready shows `startup.provenance` `measured`.

## Multi-node engine groups — 2026-10-07 (branch `feat/multi-node-groups`)

ADR 0028 is implemented on the feature branch slices through status, closed codes and exits, and the user docs (several-machines guide, settings, network-access risk, engines support table, 0.1.3 release notes). CPU and Fake-engine tests only; this is not qualification. The live rows MN1–MN9 on host A and host B are pending.

## Stored snapshots keep the deployment engine env — 2026-10-06 (branch `fix/snapshot-engine-env`)

`decode_effective_snapshot` rebuilt the deployment without `engine_config.env`, so every revision with a deployment engine env (ADR 0028 §2.1, single host included) failed `CorruptStoredData` at start and load; the deployment entries of `engine_env` are now restated and the exact-equality check proves them. CPU round-trip tests only (deployment env, override of a profile value, profile env and `approved_env`, topology, placement and host `resource_policy.groups`); not run live.

## An unusable checkpoint names its reason — 2026-10-06 (branch `fix/checkpoint-unusable-reason`)

Reported live on the 16 GB discrete-GPU laptop host (0.1.1 standalone, vLLM 0.29, an 8B FP8
checkpoint of 10,605,572,552 bytes, a device domain managing 15,797,762,136 bytes,
`memory.request` 12 GiB and 13 GiB): once measured, every start answered
`checkpoint_mismatch`, including after configuration changes and on a fresh deployment.
The stored row was `unusable` with the generic "does not resolve ... replace the
configuration" text, and `host_effective_revisions` still read `resolved`. Three defects
(ADR 0014 §7): the re-resolution kept its error only for the closed refusals
(`insufficient_device_memory:`, `host_backed_unavailable:`); `admit_start` turned every other
unusable row into `LifecycleError::CheckpointMismatch`; and the failure path wrote no host or
GPU row. Now the resolution's own text (code, field, detail; bounded at 512 characters) is
stored; a start on such a revision is refused `checkpoint_unusable` with it (409, CLI class
`invalid_config`, exit 2), closed refusals keep their codes and exits, and
`checkpoint_mismatch` is only a digest disagreement. In the same transaction each host whose
row does not resolve with the measured weights is refused `does_not_resolve` and its GPU rows
dropped; nothing is reserved, so nothing is released. `status deployment` prints
`Checkpoint  unusable: ... (<reason>)`. The 0.1.1 cause was most likely the 8 GiB family
margin on a declared device request (KV = request − weights − 8 GiB ≤ 0; inferred from the
arithmetic, the real text was not stored); since `c5ebc1a` (0.1.2) the margin is weights × 0.10, and the store
test shows the reported 12 GiB request resolving with those weights while 10 GiB is refused
with "the derived KV cache (request minus weights minus margin) is not positive". A row made
unusable before this fix keeps its generic text (now refused `checkpoint_unusable`, not
`checkpoint_mismatch`) and its host row; a new revision re-resolves. The per-revision row and
the fast re-measure on a fresh deployment (the agent's stat-identity digest cache, ADR 0028
§7) are by design and unchanged.

Tests (T14 T26): `weights_that_do_not_fit_a_declared_request_make_the_revision_unusable`
(previously pinned `CheckpointMismatch`), `a_declared_request_the_measured_weights_do_not_resolve_names_its_reason`,
`an_unusable_checkpoint_is_refused_with_its_reason` (management),
`an_unusable_checkpoint_exits_as_a_configuration_refusal` and
`status_explains_an_unusable_checkpoint` (CLI); the closed-refusal test
`a_derived_request_larger_than_the_card_refuses_the_start_with_its_code` is unchanged and
passes. CPU tests only; they are not qualification, and no live run was made after the fix.

## Standalone parked limit is settable — 2026-10-06 (branch `feat/parked-limit-setting`)

Owner decision 2026-10-06, the follow-up on ADR 0014 amendment A17 and the amendment of
ADR 0025. A standalone's parked limit was fixed at a quarter of the observed memory, below
the 33.4 GiB a parked Qwen3.8-27B with DFlash2 measured on a 128 GB GB10, so its later parks
were refused `park_parked_capacity`. `host.resource_policy.memory.system.parked_limit` now
takes `auto` (the quarter), a size or a whole percentage, by YAML, `--set` and
`CAPYCTL_SET__HOST__RESOURCE_POLICY__MEMORY__SYSTEM__PARKED_LIMIT` (flag over environment over
YAML), and shows in `capyctl config show`. A stated value above the managed limit refuses the
start with both numbers. A document without it publishes the same policy, byte for byte. The
embedded host's published document now carries the stated memory limits, so a discrete
template's `host_backed` default is sized on the same parked room admission uses.

CPU and Fake-engine tests only (precedence, form and boundary checks, unchanged digest, a
park refused `parked_capacity` at a lower limit and admitted at a raised one, and a booted
standalone storing each channel's value). They are not qualification; no live park has run
with a raised limit.

## Standalone refuses a modelopt SGLang park it cannot wake — 2026-10-06 (branch `fix/standalone-deep-wake-refusal`)

Found live on host A (CapyCTL a6b2560, SGLang 0.5.21, GB10, standalone): a `deep` SGLang
deployment with `quantization: modelopt` started and parked; the wake's
`update_weights_from_disk` raised `AttributeError: 'Parameter' object has no attribute
'weight_loader'` and the instance was retained `uncertain`. A host agent refuses that launch
`capability_missing:deep_park` (SPEC §§6.2, 9.1, ADR 0010), but the rule lived in the agent's
`launch_capability`/`park_capability` only, and standalone's embedded host never called it.

The rule is now one decision, `EffectiveDeployment::deep_wake_refusal` (capyctl-config). The
agent calls it as before; standalone's embedded launches and adopted launches pass a
`CapabilityGate` that refuses Initialize, Park and Restore with the same reason before the
engine is asked. The refused start gives up at once (one attempt) and spawns nothing.
Tests: `a_standalone_sglang_modelopt_deep_launch_is_refused_before_any_effect` (spawn_resolved
with the production bindings; before the fix it spawned the engine) and
`a_modelopt_sglang_launch_is_refused_its_launch_park_and_restore`, both T22 T21.

The other refusals in `native_execution/refusal.rs` were checked against standalone. Drift
(`InstallationGate`) and checkpoint mismatch (`CheckpointGate`) already have embedded gates;
runtime integrity is checked when the role resolves its installation, and the SGLang renderer
revalidates the protected entry on every launch;
memory (the coordinator's admission), the engine port (the lease binds it before handing it
out), the residency tier (the store's `parks`) and a wake beside other claims (the residency
admission) are decided where both paths pass. The probe refusals (`capability_missing:core`,
`:deep_park`, `:observation`) stay agent-only by ADR 0008: standalone runs the deep-park probe
at `engine add`, where a missing capability records `deep_park` disabled unless the operator
enables it, and the protected entries probe again at startup. Not run live after the fix; CPU
tests are not qualification.

## Hybrid SGLang runs 8 requests by default — 2026-10-05 (branch `fix/sglang-hybrid-default-8`)

Owner decision 2026-10-05, the note on ADR 0014 amendment A16. A gated-delta-net hybrid
SGLang deployment without `max_concurrent_requests` runs up to 8 requests
(`SGLANG_HYBRID_DEFAULT_RUNNING`, TensorFold's default) instead of up to 32: the state a
derived request reserves, `--max-mamba-cache-size` and `--max-running-requests` all use 8.
The router still admits 32 per deployment; dense SGLang is unchanged. Qwen3.8-27B with
DFlash2 reserves about 16 GiB of state instead of 61 GiB.

Live on the laptop (16 GB GPU, SGLang 0.5.21, FrogNano-4B BF16, standalone, isolated state,
`memory.kv_cache: 1GiB`, where the card holds 10 requests' state): status showed "Running
limited to 8 requests by the state cache", SGLang got `max_running_requests` 8 and
`max_mamba_cache_size` 40, started, and answered 12 concurrent requests. Not run on a GB10.

## SGLang's fraction on a discrete GPU — 2026-10-04 (branch `fix/sglang-discrete-overhead`)

Closes ADR 0014 A14's open item (note on amendment A14). SGLang 0.5.21 takes
`mem_fraction_static` of the GPU memory free at its baseline (after its CUDA context,
without the driver's reserve), not of the card's total CapyCTL renders against, and keeps
100 MiB for a multimodal checkpoint, so its pools fell about 0.65 GiB short on a 16 GB card.
On a discrete GPU whose pools CapyCTL fixes, the closed memory object now carries
`static_allowance_bytes` (1 GiB) and the entry renders the fraction from the static pool
plus it (at most 0.9999). The fixed pools bound what SGLang allocates.

Live on the laptop (16 GB GPU, SGLang 0.5.21, FrogNano-4B BF16, nothing stated, standalone,
isolated state): before, SGLang held 42778 of 63935 KV tokens and refused inputs over 42772
while status showed 63920; after, fraction 0.8352, `max_total_num_tokens=63935`, prompts of
63820 and 63910 tokens answered, 14478 MiB on the card inside the 14.5 GiB charge. CPU and
Fake-engine tests are not qualification.
## Derived SGLang requests hold the hybrid state — 2026-10-04 (branch `feat/sglang-state-facts`)

ADR 0014 amendment A16 closes A14's follow-up. The host that measures a checkpoint also
reads one request slot of an SGLang hybrid model's recurrent state from `config.json`
(`CheckpointDigestEvidence.state_slot_bytes`); the server records it with the digest
(schema v40), re-resolves a provisional revision with it, and every launch carries it
(`checkpoint_state_slot_bytes`, capability `checkpoint_state_slot`), which the host checks.
A derived request then holds the state of `max_concurrent_requests` (or 32), as far as the
domain holds it, and lends nothing more. Explicit requests, declared resources, args that
size the state pool, and revisions without the fact keep A14's sizing.

Live on the laptop (16 GB GPU, SGLang 0.5.21, FrogNano-4B BF16, standalone, isolated state):
the fact was recorded (51511296 bytes); with a 2 GiB KV cache the derived request held 7
requests' state and the KV pool kept the whole 2 GiB (context 65536); with nothing stated
the card had no room for a slot, so A14's sizing stayed (7 requests, 63920 tokens). Not run
on a GB10. CPU and Fake-engine tests are not qualification.

## Short form of `resources`, after 0.1.2 — 2026-10-04 (branch `feat/short-resources`)

Owner decision, recorded in ADR 0023 §4: `resources: {gpu: 11GiB, ram: 2GiB}`
stands for the five phases of a model that restarts. Where the document meets
its host (`deployment_defaults::for_host`) it becomes the long form: cold,
ready, parking and wake charge `gpu` on the device's domain and `ram` on the
system domain (their sum on a unified pool), parked charges zero with no
device, and an undeclared residency is `restart_only`. Refused: a parking
residency, several devices, a missing figure, a zero `gpu`, a mix with phases.
`validate config` shows the phases (short terms offline, by domain with
`--host`). Any engine takes it; the TensorFold example uses it.

Live, standalone on the branch build, one RTX 4090 Laptop GPU, TensorFold
0.6.3, FrogNano-4B-2609 MLX 4-bit: the recipe's long-form file and the same
file with `resources: {gpu: 11GiB, ram: 2GiB}` deployed side by side. Both
started (`initialize succeeded`), both launched TensorFold with
`TENSORFOLD_CUDA_MEMORY_LIMIT_GB=11`, and status showed the same startup
charge (11.0 GiB of gpu0 plus 2.0 GiB of RAM) and the same effective
fingerprint. The short-form deployment answered a chat request (`391`,
`finish_reason: stop`); with it running, starting the long-form one was
refused `capacity_blocked` naming 11.0 GiB of gpu0, as for the long form.

## 0.1.2 release preparation — 2026-10-04

Version bumped to 0.1.2 with release notes in
`docs/operations/release-notes-0.1.2.md` (vLLM 0.30.0, SGLang 0.5.21,
TensorFold 0.6.2, 0.6.3 and 0.6.5, the memory and reliability fixes since
0.1.1, parser defaults, TensorFold `--parallel`, SGLang CUDA graphs on). Four
entries that PRs after 0.1.1 had appended to the 0.1.1 notes moved to the
0.1.2 notes; the 0.1.1 notes are back to their published text. The
verification result is in the release pull request. Nothing is built, tagged
or published yet: next are the manual Release build workflow, the strict local
packaging check, a draft release and the live check from the draft
(docs/operations/releasing.md). The owner publishes.

## TensorFold 0.6.5 verified — 2026-10-04 (branch `feat/verify-tensorfold-0.6.5`)

TensorFold 0.6.5 joins 0.6.0 to 0.6.3 in the verified set (ADR 0023 §1), so
`engine add` shows it `custom no`; 0.6.4, not run live, and later versions stay
custom. Between the v0.6.3 and v0.6.5 tags, the build toolchain and the drafter
checks are unchanged for CapyCTL (`cuda/build.py` only adds a refusal of Flash
Next below sm_120). `cli_args.py` adds `--api-key` (repeatable),
`--api-key-file` and `--metrics-open`: with a key, TensorFold refuses every
route without one, `/metrics` included, and `/health` shrinks to
`{"status":"ok"}`. All three are now reserved, as vLLM's `--api-key` is;
CapyCTL passes no key and its closed environment never carries
`TENSORFOLD_API_KEY`, so `/metrics` stays open on the loopback listener.
`--mtp-confidence` (Nemotron's fixed floor instead of 0.6.5's measured-cost
depth) stays an ordinary option, and the `plan` command is not `serve`.
`server/metrics.py` adds `tensorfold:requests_total` (with keys only) and
`tensorfold:process_footprint_bytes` (macOS only); neither is read. Nemotron-H
on CUDA still runs one request at a time, and the status reason no longer
names a version.

Live on host A (one GB10, standalone from origin/main `9b90a56`, fresh 0700
state and config directories, a new `~/tensorfold-0.6.5-venv` with the same
pins as 0.6.3: torch 2.13.0 cu130, triton 3.7.1, system CUDA 13.0). Qwen3.8-27B
NVFP4 with the DFlash2 drafter, `--parallel 8`, greedy:

| Step | Result |
|---|---|
| `engine add` | `tf065 0.6.5` registered beside `tf063 0.6.3` |
| `start --wait`, cold (empty kernel cache) | 104.8 s (0.6.3: 123.0 s) |
| `start --wait`, warm | 14.5 s |
| stream with reasoning | 14 reasoning chunks, content `391`, `finish_reason: stop`, usage chunk, `[DONE]` |
| request after `stop deployment` | 409 at once |
| wake on request | with 0.6.3 started by `--evict`, a request for 0.6.5 switched and answered in 15.3 s |
| `--no-drafts` | ready in 12.9 s, answered |
| engine `/metrics`, no key | 200, the three gauges CapyCTL reads present |
| shutdown | `drained: true`, `forced: false`, 0 in flight |

Benchmark against 0.6.3 on the same deployment (medians; concurrency 512
tokens, 10 rounds pooled from two interleaved passes; context 128 tokens, 3
runs): aggregate tok/s C1 42.2 / 41.5 (one slower round), C4 121.3 / 121.4,
C8 193.9 / 193.8; prefill at 2k, 32k and 128k within 0.4%; TTFT at 128k 87.2
/ 86.9 s. `token_sha` matched on 24 of 30 keys: every concurrency pair and the
2k runs; the 32k and 128k replies differ, as 0.6.4's fp32 tree-attention fold
(#268) allows, so decode speed there follows different replies. Nemotron 3.5
Lightning, one stream: 147.5 / 152.5 tok/s (+3.4%), the same tokens. The 0.6.5
engine logs name `--no-thinking` at start and warn when a reply hits
`max_tokens` while thinking; the role log has no error or warning lines.

## TensorFold decodes the deployment's requests together — 2026-10-03 (branch `fix/tensorfold-parallel`)

Owner decision 2026-10-03, recorded in ADR 0023 §4. TensorFold serves one request
at a time on CUDA unless started with `--parallel`, and CapyCTL passed none, so a
TensorFold deployment served its requests in turn while vLLM and SGLang ran them
together. `max_concurrent_requests` now renders `--parallel` (8 when undeclared:
TensorFold reserves the drafter's buffers for every stream at start, inside the
declared memory); `--parallel` in the extra or host-fixed arguments wins and is
refused beside a declared count; status shows `Streams ...`; TensorFold's
refusal of a context its cap cannot hold names the fix.

Live, through `capyctl start standalone` on the branch build:

- One GB10, Qwen3.8-27B NVFP4 with DFlash2, 32768-token context, 34 GiB Ready.
  TensorFold 0.6.3 started with `--parallel 8` and logged a 30.68 GiB startup
  estimate within the 34.00 GiB cap and "up to 8 streams". Aggregate decode, 512
  tokens a request, medians of three rounds: 15.8 tokens/s at 1 stream, 17.6 at 2,
  28.9 at 4, 62.2 at 8 (3.9 times one stream). With `--parallel auto` in
  `extra_args` (TensorFold's one at a time) the same machine gave 12.3 at 1 and
  12.8 at 8. TensorFold 0.6.1 gave the same shape (15.6 at 1, 61.3 at 8). This
  machine decodes this model about a third as fast as the recipe's measurement
  for one stream; the ratios are what was checked.
- The same deployment with `max_concurrent_requests: 32` was refused by
  TensorFold at start (49.3 GiB estimated on 0.6.1) and status read "TensorFold's
  memory cap cannot hold context_length beside its streams: lower
  max_concurrent_requests or context_length, or raise the ready allocation in
  resources". `max_concurrent_requests: 4` beside `extra_args: [--parallel, "8"]`
  was refused at deploy, naming both.
- One RTX 4090 Laptop GPU (16 GB), FrogNano-4B-2609 MLX 4-bit on TensorFold
  0.6.3, no drafter, 11 GiB cap: startup estimate 7.29 GiB for 8 streams.
  Aggregate decode 50.4 tokens/s at 1 stream, 93.4 at 2, 176.5 at 4, 319.1 at 8
  (40.3 each); one stream 49.5 to 50.4 tokens/s from 0.5k to 32k tokens of
  context. Eight 28k-token prompts at once all completed, GPU memory at most
  9.9 GiB.

## Standalone memory limits and the TensorFold memory cap — 2026-10-03 (branch `fix/standalone-memory-limit`)

Found in the three-engine comparison (Qwen3.8-27B NVFP4 with DFlash2 on one
GB10, 121.7 GiB) and recorded in ADR 0025.

1. **The standalone managed limit could not be raised.** It was 50 % of memory
   (60.8 GiB), so an 84 GiB TensorFold declaration and a 72 GiB vLLM request were
   refused `capacity_blocked`. `host.resource_policy.memory.system.managed_limit`
   and `free_reserve` now take `auto`, a size or a whole percentage, by YAML,
   `--set` and `CAPYCTL_SET__…`; together they must fit the observed memory. The
   stored policy follows them at every start (before, standalone kept the limits
   it first stored), and `auto` returns to the default.
2. **TensorFold grew past its declaration.** CapyCTL launched it with no cap, so
   TensorFold 0.6.3 sized its CUDA budget from the machine's available memory and
   kept long prompts' states until that ran out (77.9 GiB of machine memory under
   a 58 GiB Ready declaration). The launch now sets
   `TENSORFOLD_CUDA_MEMORY_LIMIT_GB` to the declared Ready allocation.

Live on host A (TensorFold 0.6.3, SGLang 0.5.21), one standalone restarted
with the limit set each way:

- `--set ...managed_limit=90GiB`: a 90.0 GiB limit (policy revision 1). A
  TensorFold deployment declaring 84 GiB cold and 82 GiB Ready, refused under the
  old limit, was admitted and launched with the variable at `82`. A
  254,993-token prompt got its first token at 260 s and completed. Repeated long
  prompts took machine memory from 47 to 67 to 84 GiB, where the growth stopped
  (84.4 GiB, 80.8 GiB above idle, under the 82 GiB cap); a new 250k-token prompt
  was still served.
- `CAPYCTL_SET__…MANAGED_LIMIT=88GiB` on restart: revision 2, 88.0 GiB. The
  comparison's 58 GiB Ready declaration launched with the variable at `58`; five
  prompts (255k, 128k, 255k, 250k, 255k tokens) all completed and machine memory
  peaked at 60.6 GiB (about 56.5 GiB above idle; 77.9 GiB in the comparison).
- `managed_limit: "80%"` in the document on restart: revision 3, 97.4 GiB. An
  SGLang deployment with a 70 GiB request and `--max-mamba-cache-size 40` was
  admitted and started in 153 s; a 254,993-token prompt got its first token at
  367 s and completed (machine memory peak 73.7 GiB).

## A 9 GB model on a 16 GB GPU — 2026-10-03 (branch `fix/single-gpu-16gb`)

Found running FrogNano-4B-2609 (BF16, 9.32 GB) on a 16 GB laptop GPU.

- The device reserve absorbs memory held outside CapyCTL (driver reservation,
  display server) in admission, the switch planner and the launch check; a
  deployment charged up to the card's managed limit starts on an idle card. A
  request declared for a card derives its KV cache as request minus weights ×
  1.10 and its startup peak as the request (ADR 0019 amendment A1).
- STARTUP shows the card's figure with host RAM beside it; a waiting start and
  an unmeasurable checkpoint say why under the status table.
- A local model outside the models directory (a Hugging Face cache snapshot)
  is measured inside its own root and launches; `start --wait` ends at once on
  a digest diagnostic that does not clear (ADR 0014 amendment A11).
- A leased engine port another program listens on refuses the launch
  `port_conflict` before anything starts.
- A failed deployment with nothing retained can be updated, which also lets a
  restart that re-sizes deployments (new port range) re-size it.
- `start --wait` retries a `still_stopping` refusal through the stop window.
- State paths: the lock and state refusals name the path and the rule
  (`invalid_config`); `engine add` refuses a state directory whose socket path
  is too long before writing `engines.yaml`.
- SGLang: its own reason for refusing the server arguments reaches the private
  log under `--debug-engine-logs`; the launch failure hint says SGLang writes
  that log only then.
- Non-streaming responses carry `usage` (the collection asks for it) and no
  null `prompt_text` / `prompt_token_ids`.
- SGLang CUDA graphs stay off by default while parking; measured and
  documented (ADR 0014 note on §4).

Live on the laptop with temporary home, config and state directories (vLLM
0.29.0 and 0.30.0, SGLang 0.5.20 and 0.5.21): plain deployment files started
and served; the redeploy-then-`start --wait`, failed-deployment update,
re-size on restart, HF-snapshot digest, `usage`, SGLang park and wake of a
thinking model, and the debug-log reason were each seen live. The
`port_conflict` refusal itself is covered by a unit test (live, the lease
skipped the busy port). CPU and Fake-engine tests are not qualification.

## Parsers chosen by model family — 2026-10-03 (branch `feat/parser-defaults`)

ADR 0024 (owner decision 2026-10-03). For vLLM and SGLang, CapyCTL reads the
checkpoint's `config.json` and chat template at launch render and picks the
tool-call and reasoning parsers for the Qwen3 family (`hermes` / `qwen25`,
`qwen3`) and the Qwen3.5 family (Qwen3.5, Qwen3.6, Qwen3.8: `qwen3_coder`,
`qwen3`), plus `--enable-auto-tool-choice` on vLLM. An unknown family gets
none. `engine_config.vllm|sglang.tool_call_parser` and `reasoning_parser` take
`auto`, `none` or a name. The same option in `extra_args` or host-fixed args
wins over `auto`. A named parser or `none` beside it is refused. Status and
`validate config` show the choice. Parser names were checked as registered in
vLLM 0.29.0 and 0.30.0 and SGLang 0.5.20 and 0.5.21.

Live on a laptop (RTX 4090 Laptop 16 GB), standalone from this branch with
temporary home, config and state directories. FrogNano-4B-2609
(`qwen3_5`, revision `b90468c1`) ran on vLLM 0.30.0 and SGLang 0.5.21 with no
`extra_args`. The memory was declared by hand (12.5 GiB request, 3 GiB KV
cache, 13 GiB startup) until the 16 GB sizing fix lands. On both engines:
- status printed `Parsers tool calls: qwen3_coder, reasoning: qwen3 (model
  family qwen3_5)`;
- a `tools` request returned structured `tool_calls` with
  `finish_reason: tool_calls`;
- a plain question returned the trace apart from `content`: `reasoning` on
  vLLM, `reasoning_content` on SGLang.

The vLLM command line carried `--tool-call-parser qwen3_coder
--reasoning-parser qwen3 --enable-auto-tool-choice`. CPU and Fake-engine
tests are not qualification.

## Kernel builds left out of the startup peak — 2026-10-02 (branch `fix/startup-peak-excludes-kernel-build`)

ADR 0014 amendment A12 (owner decision 2026-10-02, option A). While an
Initialize runs, its host checks the engine's process group every 500 ms for
a compiler (`ninja`, `nvcc`, `cicc`, `ptxas`, `c++`, `cc1plus` and others). It
reports each span with one as a kernel build, with the readiness result
(`MemberExecutionResult` field 15). The coordinator measures the startup peak
from the availability samples outside those spans. A start that built kernels
throughout records no peak. Peaks recorded before this change stay; a
deployment blocked by one is deleted and redeployed once.

- SGLang and FlashInfer build kernels after weight loading has begun, so a
  start-at-weight-loading signal would still count the builds. The
  process-group signal works for every engine. TensorFold deployments
  declare their resources, so no peak is measured for them.
- Builds inside the engine process (Triton, NVRTC) are not seen, and are
  counted as before.

Tests (CPU and scripted engines, which are not qualification):

- coordinator: a peak sampled during a build is left out (12 GiB recorded
  where the build took 46 GiB); a start that built throughout records none;
  the existing measured-peak tests pass unchanged with no builds reported.
- host journal: the builds are carried in the readiness result and its replay.
- protocol: the field is accepted only on a usable launch, ordered and bounded.
- launchers: a real process group led by a compiler-named process is seen
  building and a `sleep` group is not.
- SGLang adapter: a compiler seen while starting is reported in the step.

Live on host A (GB10, 2026-10-03), Qwen3.8-27B NVFP4 with DFlash2 on SGLang
(`memory: {request: 48GiB}`, local checkpoint). Two standalones were built from
`main` (93ec627) and from this branch (493f8fd), each with its own state and
config directories. Switching the SGLang environment forced the JIT kernel
rebuild (the shared cache names the environment), so both first starts built.
No cache was deleted.

| | `main`, SGLang 0.5.20 | this branch, SGLang 0.5.21 |
|---|---|---|
| First start (with kernel builds) | 624 s | 585 s |
| Lowest MemAvailable during the build | 10.7 GiB | 1.8 GiB |
| Measured startup peak | 106.5 GiB | 44.8 GiB |
| Next start | refused `capacity_blocked` (needs 106.5 GiB, 60.8 GiB limit) | ready in 177 s, no build |

An independent 2 s sampler saw `ninja` and up to 28 compiler processes for
about 6 minutes of each first start.

vLLM 0.30 with Qwen3-4B on this branch started in 67 s. Its compile caches were
warm, so no compiler ran and it measured 20.6 GiB. The 102 GiB vLLM first start
seen in the three-engine run was not reproduced, because doing so needs its
caches cleared:

- FlashInfer's JIT build runs under `ninja`, so it is seen.
- torch.compile's compile workers are Python processes and are not seen
  (the fallback).
- FlashInfer autotuning uses the engine's own GPU memory.

Cleanup: deployments deleted, both standalones shut down drained, every
process and directory the check made removed. The environments, models and
caches are kept.


## Small CLI fixes — 2026-10-02 (branch `fix/small-cli-fixes`)

- `engine remove` with no role running removes the profile from
  `engines.yaml` under its lock and exits 0 (`published: role_not_running`);
  it was refused `agent_unreachable`. A running role keeps the retirement
  path (ADR 0018 amendment A3).
- A role skips an `engines.yaml` profile whose engine kind it does not know
  (written by a newer release), warns at start with the profile and kind, and
  starts; it was refused `invalid_config` at `runtime_profiles.engine`.
  Releases before this one still refuse such a file.
- `capacity_blocked` names, per host short of memory, the domain, the need,
  what is free and the limit.
- A start while new activations wait on an unproven stop is refused
  `still_stopping` (503, retryable, exit 25) instead of
  `reconciliation_required`.
- `a_success_resets_the_attempt_budget` and
  `a_restart_pauses_and_retries_every_adopted_uncertain_launch` no longer
  depend on timing; `shutdown` waits for queued Store jobs.

Live on a laptop with temporary home, config and state directories: a
standalone with only an unknown-kind profile starts with the warning and
publishes nothing, and after it stops `engine remove` edits the file. The
capacity message and `still_stopping` are covered by unit and integration
tests only. CPU and Fake-engine tests are not qualification.

## SGLang 0.5.21 verified — 2026-10-02 (branch `feat/verify-sglang-0.5.21`)

SGLang 0.5.21 joins 0.5.20 in the verified set (ADR 0018), so `engine add`
shows it `custom no`; 0.5.22 and later stay custom. The upgrade keeps torch
2.13.0, FlashInfer 0.6.18, sglang-kernel 0.4.7, transformers 5.12.1 and
torch-memory-saver 0.0.10, and changes nothing CapyCTL drives: the
ServerArgs record and its resolution, `launch_server`'s scheduler target, the
scheduler and tokenizer manager hooks, the release, resume, reload and flush
routes and their replies, and the metric names are the same. Of the 21 new
options, none has a listener, path, configuration or code shape, and the new
`--disaggregation-*` and `--hicache-storage-backend tensorcast` fall in
reserved families. The new routes (`/v1/decisions`, `/v1/systemone`,
`/pd_role_switch`, `/begin_weight_update`, `/end_weight_update`) are not
forwarded. `--kv-cache-dtype` no longer accepts `fp4_e2m1`; CapyCTL never
rendered it. A request-supplied `chat_template` is now refused unless the
server runs with `--trust-request-chat-template`.

One CapyCTL bug found and fixed, on both versions: the SGLang wake probe
(`max_tokens: 8`, exact `OK`) was spent on Qwen3-4B's thinking, so every deep
wake of a thinking model was left uncertain after resume, reload and flush had
succeeded. The probe now sends `chat_template_kwargs: {"enable_thinking":
false}`.

Live on host B (standalone from this branch, fresh 0700 state and config
directories, a new owner-approved `~/sglang-0.5.21-venv` beside the existing
0.5.20 one, both registered in the same standalone). Same prompts on both,
temperature 0, through the CapyCTL endpoint; context sweep 3 requests per
point, decode 3 streams of 512 tokens one at a time.

| | 0.5.20 | 0.5.21 |
|---|---|---|
| Qwen3-4B ready, cold (digest measured) | 99 s | 134 s (first start, kernel builds) |
| Qwen3-4B ready, warm | 59 to 99 s | 74 to 102 s |
| Qwen3-4B TTFT, 1k / 8k prompt | 0.149 / 1.169 s | 0.149 / 1.169 s |
| Qwen3-4B decode, one stream | 22.3 tok/s | 22.3 tok/s |
| Qwen3-4B measured startup peak | 19.4 GiB | 19.7 GiB |
| Qwen3-4B request while parked | 91 s | 47 to 90 s |
| Qwen3.8-27B NVFP4 + DFlash2 ready, new deployment (kernels already built) | 158 s | 169 s |
| Qwen3.8-27B ready, warm | 173 s | 161 s |
| Qwen3.8-27B TTFT, 1k / 8k prompt | 0.453 / 4.155 s | 0.456 / 4.165 s |
| Qwen3.8-27B decode, one stream | 29.1 tok/s (24.4 to 29.3) | 29.1 tok/s (24.5 to 29.2) |
| Qwen3.8-27B measured startup peak | 44.6 GiB | 44.9 GiB |

The 27B deployments set `memory: {request: 48GiB, kv_cache: 16GiB}`. A wake
is mostly the reload from disk (8 GB in 45 to 85 s on this host). On 0.5.21:
park 2 s; the request while parked woke it (resume, reload, flush, probe);
`--reasoning-parser qwen3` streamed 199 `reasoning_content` deltas and woke
the same way; a stream hung up after 2 s and a park right after it settled in
0.6 to 1.1 s; `start --evict` of the 27B parked Qwen3-4B (`released:
parked`); shutdown with one stream in flight drained it (`drained: true`,
`in_flight_at_close: 1`, the stream ended with `[DONE]`).

Open finding, fixed by ADR 0014 amendment A12 (branch
`fix/startup-peak-excludes-kernel-build`): a start that builds kernels records the build's
memory as its startup peak. The first 0.5.21 start of the 27B built FlashInfer
kernels for 9 minutes (`MAX_JOBS` 14 from 117 GiB available) and recorded a
111 GiB peak, and every later start of that deployment was refused
`capacity_blocked`. The two SGLang environments share FlashInfer's JIT cache,
so starting one after the other rebuilt the kernels again (one start took
479 s and recorded 108 GiB). Deleting and redeploying cleared it.

Cleanup: deployments deleted, both profiles removed, drained shutdown, state
removed, no CapyCTL or engine process left; the environments and models are
kept.

## TensorFold 0.6.3 verified — 2026-10-02 (branch `feat/verify-tensorfold-0.6.3`)

TensorFold 0.6.3 joins 0.6.0 to 0.6.2 in the verified set (ADR 0023 §1), so
`engine add` shows it `custom no`; 0.6.4 and later stay custom. Between the
0.6.2 and 0.6.3 tags, `cuda/build.py` and the drafter checks are unchanged.
`cli_args.py` adds `--vision-offload`, `--vision-image-tokens` and a
`control` command; neither option names a path, a listener or code, so both
stay ordinary pass-through options. `server/metrics.py` adds a decode
histogram (`tensorfold:request_decode_seconds`, mirrored as
`tensorfold:request_decode_time_seconds`); the gauges CapyCTL reads are
unchanged and the new histogram is not read. With
`stream_options.include_usage`, 0.6.3 sends usage as its own final chunk with
`choices: []` after the finish chunk that carries the `tensorfold` object;
the stream relay already accepted that chunk, and a unit test now pins the
collected shape.

Live on host A (one GB10, standalone from origin/main `74a9b9b`, fresh 0700
state and config directories, a new `~/tensorfold-0.6.3-venv` with the same
pins as 0.6.2: torch 2.13.0 cu130, system CUDA 13.0). Qwen3.8-27B NVFP4 with
the DFlash2 drafter, `--parallel 8`, greedy:

| Step | Result |
|---|---|
| `engine add` | `tf063 0.6.3` registered beside `tf062 0.6.2` |
| `start --wait`, cold (empty kernel cache) | 103.4 s (0.6.2: 115.1 s) |
| `start --wait`, warm | 13.9 s |
| stream with reasoning | 14 reasoning chunks, content `391`, `finish_reason: stop`, usage chunk, `[DONE]` |
| request after `stop deployment` | 409 at once |
| wake on request | the 0.6.2 deployment stopped, 0.6.3 started, answered in 15.4 s |
| `--no-drafts` | ready in 12.3 s, answered |
| shutdown | `drained: true`, `forced: false`, 0 in flight |

Benchmark against 0.6.2 on the same deployment (medians; concurrency 512
tokens, 10 rounds pooled from two interleaved passes; context 128 tokens, 3
runs): aggregate tok/s C1 42.7 / 42.7, C4 122.6 / 122.7, C8 195.3 / 194.6;
decode tok/s 2k 46.4 / 46.5, 32k 41.2 / 41.3, 128k 30.3 / 30.1; TTFT at 128k
87.1 / 87.0 s. `token_sha` matched on 30 of 30 keys. Usage was counted on
130 of 130 measured requests per version. On Qwen3.8 dense the `/health`
keys are the same as 0.6.2's. No CapyCTL bug found.

## Long prompts, stream bounds and standalone queue settings — 2026-10-02 (branch `fix/stream-idle-cancel`)

Found in the TensorFold 0.6.1 and 0.6.2 context sweep (Qwen3.8-27B NVFP4,
DFlash2, standalone): every 256k-token prompt ended at 120 s with no token.

1. **A prefill was cut by the idle bound.** TensorFold sends the reply's
   `role` chunk as soon as it accepts a request, before it prefills. The
   router counted that chunk as backend progress, so the stream idle bound
   (120 s) applied to the prefill instead of the request deadline (1800 s in
   standalone). A chunk that only opens the reply is now relayed but is not
   progress (`opens_reply_only`, SPEC §10 amendment 2026-10-02), streamed or
   collected.
2. **Standalone refused the queue settings.** `host.resource_policy.queue`
   (every bound a host has, `stream_idle_timeout` included) is now honoured
   in a standalone document, by `--set` and by `CAPYCTL_SET__…`. A restart
   with changed bounds applies them to the stored policy as a new revision;
   before, an existing standalone kept its first bounds silently. The
   persisted memory limits are kept as before.
3. **A cut request stayed uncertain.** A stream or collected response the
   router cuts for its bounds closed the engine connection already; its lease
   is now `cancelling`, as for a client hang-up, and settles on engine
   quiescence.

Root cause of the slow stop: TensorFold 0.6.1 and 0.6.2 (CUDA server) notice a
closed connection only between decode rounds, so an abandoned prefill runs to
its end. The stop waits for `/health` idle by design (ADR 0023 §6), so it lasts
as long as the remaining prefill. CapyCTL cannot shorten that without
signalling a busy engine, which ADR 0023 refuses (owner decision).

Live on host A (standalone, TensorFold 0.6.2, fresh 0700 state and config
directories, 262144-token context):
- Before (commit `74a9b9b`): a 247.5k-token prompt got its `role` chunk at
  0.3 s and was cut at 120.3 s; the engine kept prefilling for 130 s more, and
  the stop took 120.9 s. A start of another deployment during that stop was
  refused `capacity_blocked` at 15 s and `reconciliation_required` at 73 s.
- After, `--set host.resource_policy.queue.stream_idle_timeout=600s`: the
  same prompt (247569 tokens) got its first token at 246.6 s and `[DONE]`.
- After, default bounds: first token at 246.9 s and `[DONE]`.
- After, `CAPYCTL_SET__HOST__RESOURCE_POLICY__QUEUE__REQUEST_DEADLINE=30s` on
  a restart (policy revision 2): a 121k-token prompt was cut at 30.0 s with
  its lease `cancelling`; the stop sent then took 46.7 s, the rest of the
  prefill. A restart without the variable went back to 1800 s (revision 3).

Not fixed (noted for the owner): a start during a stop that waits on a busy
TensorFold is refused `reconciliation_required` ("Current owned state or
resource policy is unavailable") once the cleanup pauses the worker
(`start_scoped`, `CoordinatorError::Stopped`); a clearer retryable answer
needs a new coordinator error. A client that hangs up during a prefill is
noticed only at the next chunk.

CPU and Fake-engine tests are not qualification.

## Draft model memory gaps closed — 2026-10-02 (branch `fix/drafter-memory-gaps`)

Gaps left by the vLLM 0.30 findings (Qwen3.8-27B NVFP4 with DFlash2):

1. **First start above its reservation** (ADR 0014 amendment A8). The derived
   startup placeholder is now `max(request + graphs, weights × 2.25 + margin)`
   for vLLM and SGLang, with a 1.25 GiB graph allowance per captured model.
   Recorded as `startup_graphs_bytes`; older revisions decode unchanged (1.6,
   no allowance).
2. **Draft model KV in the fitted context** (amendment A9).
3. **Hybrid checkpoints on vLLM** (amendment A10, owner decision 2026-10-02).
   vLLM gets `--max-num-seqs` 32, the router's per-deployment bound (one
   shared constant), unless the deployment or installation sets it. The fit
   follows vLLM's recurrent-state block layout for those sequences.
   SGLang is unchanged: with DFlash2 at the default 4 GiB it refuses (state
   cache), so such a deployment declares `memory.kv_cache`.

Live on host B (standalone, fresh 0700 state and config directories, fresh
engine caches), vLLM 0.30, no memory stated (4 GiB KV derived):
- Without DFlash2: context 117600 tokens, reservation 55.19 GiB, first-start
  peak 49.88 GiB, answered Jupiter.
- With DFlash2: context 23072 tokens, reservation 63.25 GiB, peak 49.89 GiB,
  answered Jupiter.

Before the factor change the same starts peaked at 47.86 GiB and 50.49 GiB
against 41.92 GiB and 50.08 GiB.

With `kv_cache: 16GiB` and DFlash2: vLLM peaked at 50.59 GiB and SGLang at
44.63 GiB (both reserved 51.75 GiB then); both answered.

SGLang 0.5.20 with no memory stated: without DFlash2 it started, answering
with running requests capped at 1; with DFlash2 it refused to start.

Cleanup complete. CPU and Fake-engine tests are not qualification.

## TensorFold 0.6.2 verified — 2026-10-02 (branch `feat/tensorfold-0.6.2`)

TensorFold 0.6.2 joins 0.6.0 and 0.6.1 in the verified set (ADR 0023 §1), so
`engine add` shows it `custom no`; 0.6.3 and later stay custom. Between the
0.6.1 and 0.6.2 tags, `cli_args.py`, `server/metrics.py` and `cuda/build.py`
are unchanged, so the reserved, sensitive and typed options, the metric
names and the build toolchain are the same. One kernel source changed
(`gdn.cu`, now `tensorfold_gdn_v2`); every extension builds again on the
first start because CapyCTL keys the extension cache by version. The engine
now prints one line per request and a `done` line per reply (prompt and
reply token counts, a token hash, tok/s, TTFT, accepted/drafted; no prompt
or reply text); successful `GET /health` and `/metrics` are not logged.

Live on host A (standalone from this branch, fresh 0700 state and config
directories, a new `~/tensorfold-0.6.2-venv` with torch 2.13.0 cu130,
owner-approved; system CUDA 13.0): `engine add --approve-option=--drafter
--approve-path <drafters>` registered `tensorfold 0.6.2`, custom no, deep
park disabled. The recipe deployments ran with the models downloaded by
CapyCTL (`model: {hf: ...}`):

| | 0.6.1 (recipes) | 0.6.2 |
|---|---|---|
| Nemotron 3.5 Lightning 30B-A3B 4-bit, ready cold (after download) | 104 s | 101 s |
| Qwen3.8-27B NVFP4 + DFlash2, ready cold (after download) | 119 s | 116 s |
| Nemotron decode, median of 3 streams, 512 tokens | 132 tok/s | 143 tok/s (125 to 149) |
| Qwen3.8-27B DFlash2 decode | 48.2 tok/s | 45.2 tok/s (34.0 to 46.5) |
| Qwen3.8-27B `--no-drafts` decode | 11.9 tok/s | 12.0 tok/s |
| Warm start (`start` after `stop`) | 8 s / 10 s | 8.1 s / 9.7 s |

Every cold start above is from an empty kernel cache. The 0.6.1 ones include
verifying the weights already in the store; the 0.6.2 Nemotron one starts when
its download was verified and the Qwen3.8 one at `start deployment` with the
weights verified, and both include the checkpoint digest, the load and the
kernel builds. Prompts differ from the recipes', so
the medians are not paired; the one
request both runs share (354 of 2355 drafts accepted, the same `token_sha`)
decoded at 33.8 tok/s on 0.6.1 and 34.0 on 0.6.2. Kernel builds from an empty cache: Qwen3.8 builds `nvfp4_ck_v6` at startup
and `qwen_b16_v4`, `gdn_v2`, `prefill_attention_v1` and `qmm_v5` on its first
request (probe TTFT 65 s); Nemotron builds `experts_v7` and `qmm_v5` at
startup and `nemotron_scan_rows` and `prefill_attention_v1` on its first
request (34 s). Both answered Jupiter plain and streaming, with
`reasoning_content` passed through and the `tensorfold` record kept. With
both deployed (32 + 36 GiB over the half-memory standalone limit), each
request for the stopped one switched (`released: stopped`) and answered in
11.1 to 13.3 s. `park deployment` stays refused (ADR 0023 §6); a deployment
without a drafter still fails to start Qwen3.8 dense with the `--drafter`
or `--no-drafts` hint. `/health` and `/metrics` answered on loopback with
the pinned families (the live body is now a parser fixture); standalone has
no host load report, so its latency view shows the router tier only, by
design (`roles.rs`). Deployments deleted, profile removed, drained shutdown,
state removed, no CapyCTL, engine or GPU process left. No CapyCTL bug found.
After rebasing onto the vLLM 0.30 findings fix, a second run from a fresh
state (empty kernel cache) gave the same results: Qwen3.8 DFlash2 44.6 tok/s,
Nemotron 141 tok/s, Nemotron started by a request in 48 s with its builds.

## vLLM 0.30 live findings fixed — 2026-10-02 (branch `fix/vllm-030-findings`)

Four findings from the Qwen3.8-27B NVFP4 runs on vLLM 0.30.0 and SGLang 0.5.20:

1. **vLLM's `reasoning` delta.** vLLM 0.29 and 0.30 stream a trace as
   `delta.reasoning`; the streamed-delta allowlist named only
   `reasoning_content`, so with `--reasoning-parser` the readiness probe failed
   (`chat terminal result unverified`) and the engine was stopped. `reasoning` is
   now relayed unchanged, collected under its own name, and counts as a probe
   answer.
2. **Draft model memory** (ADR 0014 amendment A6). The draft model's weight files
   are counted with the checkpoint's, so the request, KV cache and startup
   placeholder cover it.
3. **First-start Initialize window** (ADR 0014 amendment A7). vLLM and SGLang
   derive Initialize as the load term plus a 480 s first-start allowance.
4. **Engine logs.** Behaviour kept, docs corrected: vLLM and TensorFold output is
   kept owner-only (launch-failure summaries read it); SGLang's is discarded once
   SGLang is imported unless `--debug-engine-logs`, so its file is empty.

Live on host B (standalone, fresh 0700 state and config directories): vLLM 0.30
Qwen3-4B with `--reasoning-parser qwen3` reached Ready and streamed 199
`reasoning` deltas through CapyCTL; the collected response carried `reasoning`.
Derived Initialize 690 s. Qwen3.8-27B NVFP4 with DFlash2: weights recorded
25.77 GB (checkpoint plus draft model), cold 49.25 GiB, first-start peak
50.46 GiB (1.2 GiB above the placeholder, CUDA graphs), second start reserved the
measured peak and peaked at 50.18 GiB, steady 48.97 GiB against a 49.25 GiB
Ready charge; warmup 235 s first, 79 s next; derived Initialize 860 s. SGLang
Qwen3-4B: Ready, log file 0 bytes. Cleanup complete. CPU and Fake-engine tests
are not qualification.

## vLLM 0.30.0 verified — 2026-10-02 (branch `feat/vllm-0.30`)

vLLM 0.30.0 joins 0.29.0 in the verified set (ADR 0018), so `engine add`
shows it `custom no`; 0.30.1 and later stay custom. The upgrade renames or
removes nothing CapyCTL drives: the development routes, the
`VLLM_SERVER_DEV_MODE` switch, every reserved and typed destination and the
metric names are unchanged. New options are covered: `--enable-scale-out`
(registers `/render`, `/derender` and `/inference/v1/generate`) is reserved
on both sides of the launch, compared whenever the installed parser has it;
`--watermark-config` and `--engram-config` need named host approval like
every other `*-config` option; `--load-format ipc_cache` already needs
approval, as any `--load-format` does. The removed environment variables
(`VLLM_PREFIX_CACHE_RETENTION_INTERVAL`, `VLLM_MM_HASHER_ALGORITHM`,
`VLLM_NIXL_EP_MAX_NUM_RANKS`) were never set or documented by CapyCTL.

Live on host B (standalone from this branch, fresh 0700 state and config
directories, a new `~/vllm-0.30-venv` with vLLM 0.30.0 and torch 2.13.0
cu130, owner-approved): `engine add` registered `vllm 0.30.0`, custom no,
deep park enabled. Qwen3-4B (`docs/examples/deployment-vllm.yaml`, checkpoint
already downloaded) was ready in 109 s and answered with Jupiter. Park was a
level-2 sleep (11.9 GiB freed, none backed up in CPU), parked in about 2 s;
MemAvailable went 116.7 GiB idle, 100.5 GiB ready, 112.8 GiB parked. A
request woke it (`wake_up` weights, `collective_rpc` `reload_weights` in
7.3 s, `wake_up` kv_cache) and answered in 9.2 s end to end. A stream hung up
after 2 s and a park right after it settled `parked` in 0.6 s. Deployment
deleted, profile removed, drained shutdown, state removed, no CapyCTL,
engine or GPU process left. No CapyCTL bug found.

## First model on each engine — 2026-10-02 (branch `docs/engine-examples`)

`docs/examples/deployment-{vllm,sglang,tensorfold}.yaml` and the "First model
on each engine" section of `docs/guide/engines.md`, with output captured live
on host B (standalone, published 0.1.1 binary from the one-line installer,
fresh state and config directories): `engine add`, `deploy model`,
`start deployment --wait` and one chat request per engine, run one at a time.
vLLM 0.29 Qwen3-4B ready in 55 s, SGLang 0.5.20 Qwen3-4B in 61 s (checkpoint
already downloaded), TensorFold 0.6.1 Nemotron 3.5 Lightning 30B-A3B 4-bit in
17.5 min including its 18.5 GB download, 10 s on a later restart. Each answered
with Jupiter. Deployments deleted, profiles removed, drained shutdown, no
CapyCTL or engine process left. No CapyCTL bug found.

## 0.1.1 published — 2026-10-02

0.1.1 is published; tag `v0.1.1` is at the #19 merge (`bf13c72`). The
Release build workflow built both architectures, and the strict privacy scan
passed on both with no skips. The live check on host B ran from the release
artifacts: TensorFold 0.6.1, Nemotron ready in about 97 s, and a chat request
answered. A fresh-user curl install worked.

Follow-ups from that check are on `fix/post-0.1.1`: `engine add` names the
control socket it tried when no role answers (a role started with another
`--state-dir` looked absent, so the profile stayed unpublished until a
restart); the install guide says the state directory and its ancestors must
not be group- or world-writable; a refused delete says a stop may still be in
progress.

## 0.1.1 release preparation — 2026-10-01

Version bumped to 0.1.1 with release notes in
`docs/operations/release-notes-0.1.1.md` (TensorFold 0.6.0 and 0.6.1, the live
follow-ups, `ci-local.sh`). The verification result is in the release pull
request. Nothing is built, tagged or published yet: next are the manual
Release build workflow, the strict local packaging check, a draft release and
the live check from the draft (docs/operations/releasing.md). The owner
publishes.

## Live follow-ups — 2026-10-01

The four problems the TensorFold run found are fixed
(`docs/specs/2026-10-01-live-followups-design.md`): Hugging Face link chains
are measured (ADR 0014 A5) and an unmeasurable checkpoint says why;
`validate config` without `--host` checks `resources`; a role starts with no
engine and `engine remove` takes the last profile (ADR 0018 A2); a client
hang-up mid-stream closes the engine connection and the request stays
charged until the engine reports quiescence (SPEC §10). TensorFold 0.6.1 is
verified beside 0.6.0 (ADR 0023).

The live run found two more CapyCTL bugs, both fixed with a failing test
first:

- `091e3a1`: on a remote host, a park or stop accepted right after a
  hang-up never settled the cancelling lease. The quiescence check matched
  the host's load sample only against Ready launches, and an instance stops
  being one once a park or stop is accepted. On vLLM 0.29 on host B, a park
  0.04 s after the close was still `parking` after 150 s (it would have
  waited for its 900 s deadline), and a stop drained its full 30 s. After
  the fix the same park finished 1.35 s after the close and the stop 1.8 s
  after its command.
- `9d47b8f`: `validate config` without `--host` refused the engines guide's
  TensorFold block (`resources.cold.devices[0].sharing` missing) while
  `deploy model` accepted it. Offline, a missing `sharing` now takes the
  deployment's own value for that device, else `exclusive`. The guide-shaped
  file now validates.

`1467b86` fixes the matrix harness: `roles.sh` parsed `join host` text
output as JSON.

Live on host A and host B (GB10), with Nemotron 3.5 Lightning 30B-A3B
4-bit at a 32768-token context, 32 GiB cold and 30 GiB ready:

- TF2 on an unmodified two-hop cache passed on TensorFold 0.6.0 (host B,
  standalone). A fresh `huggingface_hub` snapshot links each file to
  `../../blobs/<sha>`, which links again into `hub/blobs/<xx>/<sha>`. The
  digest was measured (no `unsafe_file`) and the deployment was ready 100 s
  after the start command, including the measurement and the kernel build.
  CapyCTL measured a 26.2 GiB startup peak. A plain request answered `42`
  with the `tensorfold` object (159 tok/s decode), the streaming one `42` in
  50 events, and a 3343-token request took 29.0 s with `nvidia-smi` at most
  18.7 GiB.
- TensorFold 0.6.1 (new venv on host B, an owner exception). TF1 passed:
  `engine add` on the running empty role printed `Registered tensorfold
  (tensorfold 0.6.1)`, with custom `no`, deep park disabled and CUDA
  `/usr/local/cuda`, and published it live. TF2 passed standalone. A new
  deployment was ready 88.4 s after the start (digest and first 0.6.1 kernel
  build), with a 26.4 GiB startup peak. It answered `42` plain (158 tok/s)
  and streaming (50 events), and the 3343-token request took 29.2 s with
  `nvidia-smi` at most 18.7 GiB. TF2 also passed remote, on host B under the
  server: ready in 95.9 s, `42` plain and streaming through the router, and
  the host's load report present for the launch. The router's selection
  read `load_source: engine` with `engine_running: 1` during a long request
  and 0 when idle, so the vLLM-named metric mirrors 0.6.1 adds no longer
  hide the load.
- TF5 passed on both versions. Before, the engine stayed busy until it had
  finished all 2048 tokens and read idle 17.2 s after the close. On 0.6.0,
  standalone, the engine read `busy: false`, `requests_running: 0` 0.07 s
  after the client closed, having generated 32 tokens. The lease was
  acknowledged quiescent 0.21 s and closed 0.40 s after the close. A stop
  sent then finished 1.4 s later, and the process was gone 1.6 s after the
  close. On 0.6.1, standalone: idle 0.08 s after the close (32 tokens),
  lease closed 0.40 s, stopped 0.95 s after the stop command, process gone
  1.2 s after the close. On 0.6.1, remote through host ingress: idle 0.17 s
  after the close, ingress connection to the engine closed with it, lease
  acknowledged 0.48 s, a stop accepted 0.04 s after the close finished
  1.6 s after the close.
- vLLM 0.29 on host B, remote (server and host agent), passed after the
  first fix. Host ingress closed its connection to the engine 0.1 to 0.2 s
  after each hang-up, the engine log showed generation stopping (`Running: 0
  reqs`), and the lease was acknowledged in 0.46 to 0.96 s. A park after the
  lease closed (0.61 s) left the deployment `parked` 1.03 s after the park
  command. A park 0.05 s after the close was acknowledged at 0.60 s and
  parked 1.35 s after the close.
- SGLang 0.5.20 passed, remote, on both hosts. On host A, the lease closed
  0.20 s after the close and a park then took 0.62 s. With the park sent at
  the close, the lease was acknowledged at 0.98 s and the deployment parked
  1.41 s after the close. On host B: lease closed after 1.42 s, park then
  0.57 s; with the park sent at the close, acknowledged 0.85 s, parked 1.32 s.
- Empty role and last profile passed (host B, standalone). A drained
  shutdown reported `drained: true` with 0 in flight. On the restarted role,
  `engine remove tensorfold` removed the last profile (`Published yes`), and
  `status deployment nemotron` printed `Engine  none: run capyctl engine add
  <path>`. The role then booted with no engine, showing the banner `Engines
  none: run capyctl engine add <path>`, and `engine add` of 0.6.1 published
  live.
- M75 passed on both hosts: each booted with no engine, published no
  profiles, started no engine child, and exited 0 in both JSON and text
  modes.

Found and not fixed: a deployment keeps the executable it was resolved with
(ADR 0018: deployments are not re-resolved). After the 0.6.0 profile was
removed and 0.6.1 registered under the same name, starting the existing
deployment ran the 0.6.0 executable, while `status deployment` showed the
0.6.1 installation. When the deployment's executable is no longer
registered, the view falls back to the role's first installation.
Redeploying picked up 0.6.1.

All roles stopped with a drained shutdown. Deployments and profiles were
removed, and no capyctl, engine or GPU process is left on either host.
The venvs, the fresh Hugging Face cache and the model directories are kept.
Local checks: formatting, Clippy with warnings denied (workspace), core
suite 1163 passed / 0 failed, workspace 2398 / 0, site check. CPU and
Fake-engine tests are not qualification.

## TensorFold engine — 2026-10-01

ADR 0023 is implemented: `tensorfold` is the third engine kind, registered with
`capyctl engine add` after a closed-PATH toolchain check, launched restart-only
with a private per-version build cache, ready on `/health` and the model list,
drained on its own counters before a stop, and part of the switch planner like
the other engines. A role's own TensorFold is `local_engine.tensorfold`
(`--tensorfold-bin`, `CAPYCTL_TENSORFOLD_BIN`); a drafter is an approved path
extra argument, as vLLM's and SGLang's draft models are.

Live on host B (GB10, standalone, TensorFold 0.6.0 venv, Nemotron 3.5 Lightning
30B-A3B 4-bit, 32768-token context, 32 GiB cold and 30 GiB ready):

- TF1 passed: `engine add` registered `tensorfold 0.6.0` with deep park
  disabled and the CUDA toolkit found at `/usr/local/cuda`, which the profile
  records as its `cuda_home`; `engine list` showed it. The guide carries the
  captured output.
- TF2 passed after a fix. The first start built the kernels and answered the
  readiness probe about 88 s after the start command (TensorFold reported the
  model loaded in 50.7 s), then failed Ready with `invalid lifecycle input`: the store, the completion check and
  the protocol required a worker process, and TensorFold serves from one.
  Fixed in `7c3a4f3` (a single api process is a group; parked or restored
  claims still need a worker). The next start was ready in 8.1 s. A plain
  request answered `42` with the `tensorfold` object (161 tok/s decode); the
  streaming one answered `42` in 50 events; a 6000-token request ran at
  132 tok/s. Peak memory stayed inside the reservation: CapyCTL measured a
  20.2 GiB startup peak, `nvidia-smi` showed at most 19.3 GiB and
  MemAvailable dropped at most 25.1 GiB (first start, with the build).
- TF3 passed after a fix: a standalone document refused
  `server.lifecycle_defaults` and its coordinator had no idle policy. Fixed in
  `c5ef90c`. With `ready_idle_timeout: 60s` the deployment stopped 56 to 61 s after
  the role restarted with it Ready, its process was gone and the GPU showed no process. One request
  woke it in 8.4 s with the answer (warm start: no kernel build, loaded in
  6.9 s). `park deployment nemotron` was refused `unsupported_capability`
  (exit 5).
- TF4 passed: with Qwen3-4B on vLLM 0.29 (KV cache 26 GiB, 42.7 GiB startup
  reservation; the standalone managed limit is half of the 121.7 GiB) the two
  do not fit together. Three switches each way all answered 200: vLLM
  `released: parked` and TensorFold `released: stopped` in the role's switch
  events, nemotron answered in 9.9 to 11.2 s and Qwen in 18.4 to 22.3 s; the
  largest `nvidia-smi` total was 35.1 GiB (vLLM ready).
- TF5 passed: a stream closed by the client after 2000 bytes left TensorFold
  `busy: true` until it finished the 2048 tokens, and `/health` read
  `busy: false`, `requests_running: 0` 17.2 s after the close, inside the
  30 s drain bound. The router drains a disconnected stream rather than
  cancelling it (SPEC §10: a client disconnect is not proof the engine
  stopped); the same disconnect sent to the engine directly stopped it after
  29 tokens. The release (`start deployment qwen --evict --wait`) came 2.1 s
  after the idle read; the engine stopped answering 0.3 s and exited 0.7 s
  after the release began.

Fixed in the live follow-ups entry above.

All roles stopped with a drained shutdown; deployments deleted; no capyctl or
engine process and no GPU process left on host B. Local checks: formatting,
Clippy with warnings denied, core suite 1139 passed / 0 failed, workspace
2347 / 0, Python runtime 302 OK, site check. Discrete GPUs run TensorFold
unqualified. CPU and Fake-engine tests are not qualification.

## 0.1.0 release candidate — 2026-10-01

Draft release `v0.1.0` (not published) targets `f224320`, the commit both
archives were built from (`dirty: false`, rustc 1.98.1). Each architecture was
built twice with byte-identical archives: x86_64 on a maintainer laptop,
aarch64 on host A. The x86_64 strict packaging check passed with no SKIP lines;
the aarch64 packaging and installer checks passed on host A (42 installer
checks), and its binary has no private-denylist or lab-host match and no old
project name. The draft's six assets match the local builds and
`SHA256SUMS`, and `install.sh --version v0.1.0` installs from the draft with
GitHub authentication.

Installed-binary live check from these archives:

- Laptop (RTX 4090, standalone): the release notes' upgrade steps (old binary
  and shared files removed, fresh state), then vLLM 0.29 Qwen3-4B deployed in
  55 s, served, parked to 1009 MiB and woke on request in 1.4 s; an SGLang
  0.5.20 Qwen2.5-1.5B deployment switched with it both ways. `list hosts` and
  `list engines` work on standalone.
- Laptop server with hosts A (SGLang) and B (vLLM): both joined and online on
  0.1.0; both deployments served through the server, a request without the key
  got 401, both parked and woke on request in 47.3 s (SGLang) and 9.0 s
  (vLLM). A first attempt was refused with `insufficient resources` while
  another workload held the hosts' memory, as intended.

All roles stopped with a drained shutdown; deployments deleted; no capyctl or
engine process and no GPU process left on any machine. The history scan finds
no private names beyond the public owner handle in merge messages and the
public site URL; planning-tool references were removed from the tree (history
keeps them, by owner decision). CPU and Fake-engine tests are not
qualification.

## Rename to CapyCTL — 2026-09-30

ADR 0022: the project, binary and repository are CapyCTL (`capyctl`), with no
aliases for the old names. Crates, variables, paths, units, archives, the
installer, the site and the docs use the new name; earlier entries below keep
the old one. `scripts/check-name.sh` keeps it out of everything else (file
contents and paths) and runs in packaging verification and CI.

State written by earlier releases is not reused and earlier roles cannot talk
to 0.1.0 roles; the 0.1.0 release notes give the clean-up steps. The live
rows that drive a previous release (DG6 migration, ENG4) refuse to run until
a previous capyctl release exists.

Local checks on the renamed tree: formatting, Clippy with warnings denied,
core suite 1115 passed / 0 failed, workspace 2285 / 0, Python runtime 301 OK,
packaging with no SKIP lines (`capyctl-0.1.0-linux-x86_64.tar.gz`), installer
tests and the site check. Live on a laptop (release build, fresh state):
`capyctl --version`, `engine add`, the text banner and stop summary in a
terminal, the JSON banner when redirected, and `list hosts`. CPU and
Fake-engine tests are not qualification.

## Terminal output — 2026-09-29

ADR 0021 is implemented: commands print tables or a summary with key-value
details unless `--json` is given, and roles print text on a terminal and one
JSON object per line to the journal, files and pipes. JSON results and event
fields are unchanged; tests and the live harness ask for JSON where they
parse it. The guides show output from a real run. CPU and Fake-engine tests
are not qualification.

## 0.1.0 release check and first-run fixes — 2026-09-29

The readiness pull request (CI, release build, community files) merged at
`ca95932`. Before the merge, the CI workflow failed validation because
`runner.temp` is not available in job-level `env`; the fixture path is now set
in its step, and actionlint passes over every workflow. The site's name check
had never run: `site/voice-denylist.local.txt` was absent. With a local copy of
the private denylist it passes.

`v0.1.0` was built from `ca95932` with Rust 1.98.1 on both architectures
(x86_64 on a maintainer laptop, aarch64 on host A). Each archive was
byte-identical to a repeat packaging run. The x86_64 strict packaging check
passed with no SKIP lines. The aarch64 packaging and installer checks passed on
host A, and its binary had no private-denylist or lab-host match in a local scan.

Installed-binary live check from those archives, through `install.sh`:

- One machine (laptop RTX 4090, standalone): vLLM 0.29 Qwen3-4B deployed in
  52 s, served, parked to 1009 MiB and woke on request in 3.0 s. An SGLang
  0.5.20 Qwen2.5-1.5B deployment switched with it both ways. SGLang's first
  request took 96.9 s (a one-time kernel build), then 0.16–0.26 s.
- Several machines (laptop server, SGLang on host A, vLLM on host B): both
  deployments served through the server, a request without the key got 401,
  both parked in 2 s, and they woke on request in 48.0 s (SGLang) and 8.6 s
  (vLLM).

All roles stopped with a drained shutdown, and every GPU was left idle.

The check found first-run problems, fixed before 0.1.0:

- The one-time listener migration widened an explicit `127.0.0.1:8443` bind on
  fresh state to `0.0.0.0:8443`. The owner removed the migration (2026-09-29):
  a stated bind is always honoured (SPEC §15.1, ADR 0019).
- An identity directory under `/tmp` was refused with "unsafe or already in
  use". The message now names the path and the failing check.
- A listener whose port was taken exited with a generic
  `management_unavailable`. It now names the listener, the address and the cause.
- A join file copied as 0644 was refused without naming it. The message now
  gives the path and the `chmod 600` to run.
- The requests guide now warns that an engine's first request can be slow.

The fixes change the binary, so both architectures are rebuilt and checked
again before the draft. CPU and Fake-engine tests are not qualification.

## External build-cache path removal — 2026-09-27

The release scan caught a generated Rust source path from a shared build cache
outside the rewritten checkout. The broad home-directory remap had removed the
username but retained the rest of that private path. Packaging now canonicalizes
and explicitly remaps `CARGO_TARGET_DIR` to the portable build prefix, including
when the cache is outside the checkout.

The packaging verifier checks both the original cache path and its partially
home-remapped form. Its binary and symbol scans now consume complete command
output: an early-closing quiet grep could otherwise make a real match appear
absent under `pipefail`. The previously leaking archive is rejected by the new
check; the rebuilt package passes strict packaging verification, all 42
installer checks and the independent privacy scan with zero findings.

Application code is unchanged. The rewritten application's additional native
run passed five park/wake cycles and all 55 observations in 35–45 ms. Its
deployment and role were stopped with verified cleanup. The final release
must incorporate this packaging fix and rebuild both architectures before
publication. The new repository is still private and no release is published.

## Release license and history gate — 2026-09-27

The Apache-2.0 license selected in ADR 0006 is now present in `LICENSE`, shipped
in release archives, and retained by the installer. The installer still accepts
older archives without it. The README links the license directly. Strict
packaging verification, all 42 installer checks and the site publishing checks
pass for this change; the previously recorded runtime verification is unchanged.

The local history scanner now fails on prohibited names and machine-specific
paths across every retained blob, tree filename, commit/tag message, attribution
header and ref name. It preserves the approved public repository URLs, human
author identities and generic test paths. Scanner fixtures cover old content
that is absent from the current tree, annotated tags and branch names. A first
rewrite of all 715 main-branch commits passed with zero prohibited references
across 10,177 objects. The final rewrite must include this license addition
before either release architecture is built. Old development branches and
release-candidate tags will not be copied into the new release repository.

## SGLang observation fairness fix — 2026-09-27

The scheduler now yields for 1 ms only while its native observation listener
handles an accepted connection. This covers custody, authentication and reply
transport as well as the snapshot. Ordinary scheduler ticks do not sleep.
The listener clears the signal after success, rejection, disconnection or
custody failure. Authentication, two-second observation deadlines, restoration
checks and conservative accounting are unchanged.

A clean installed release built from `d7d7fec` passed five consecutive
host-backed park/wake cycles with Qwen2.5-1.5B-Instruct and SGLang 0.5.20.
All 55 observations succeeded in 35–63 ms (median 40 ms); wakes took
1.898–2.110 seconds. The same GPU process survived all cycles, held 966 MiB
while parked, and returned to about 7.5 GiB after waking. The deployment was
deleted with verified stop, the role was stopped, and no local GPU compute
process remained. Evidence is local and untracked under
`target/live/sglang-fixed/` and `target/live/sglang-fairness-release/`.

All 301 Python runtime tests pass with the pinned saver-source fixture.
The core suite passed 1113 distinct tests (1114 reported) and the workspace
suite passed 2257 distinct tests (2258 reported), with zero failures. Clippy
with warnings denied across the core crates, formatting, strict packaging
verification, 42 installer checks and the site publishing checks pass.
CPU and Fake-engine tests are not native recipe qualification; the native
result above applies to the selected recipe. Review found no remaining
correctness or security issue in this scoped change.

The site workflow now explicitly waits for a public repository before building
and deploying Pages, matching its documented release sequence. Next: rewrite
history with the stronger privacy gate into the new release repository, verify
the rewritten tree, rebuild both architectures from its commit, and prepare
the release. The earlier timeout investigation below is resolved by this fix.

## Public release destination — 2026-09-27

The owner selected `edurdias/mllm` for the public release. The source repository
remains private; the new sanitized repository will be created directly at the
selected destination, so the previous same-namespace repository rename sequence
is no longer needed. The installer defaults to that repository, and the site
and installation guides use `https://edurdias.github.io/mllm/`.

Before rewriting history, update the local sanitization rules to preserve the
approved public repository slug and URLs while still stripping private paths
and machine identifiers. The older blanket owner-name replacement must not
rewrite the new destination. Rebuild both release architectures from the
rewritten release commit, prepare the release draft, and verify public
installation and Pages after publication. The SGLang timeout fix and its
installed-release live pass remain pending. This destination change does not
publish a release or create the public repository.

## Installed-release timeout investigation — 2026-09-27

Both release architectures were rebuilt from merged `c4e132c` with clean
BUILDINFO records. Their archives matched the previous build byte for byte;
packaging and the 42 installer checks passed. Strict packaging verification
passed on the control host; the ARM host skipped unavailable ShellCheck.
The installed binaries on all three machines contain the exact-marker fix.
Publication and the sanitized-history transition remain pending.

A fresh installed-binary run of Qwen2.5-1.5B-Instruct on SGLang 0.5.20 with
host-backed residency passed its first wake but failed its second: three native
observations exceeded the two-second receive deadline. A fresh diagnostic run
with private engine logging reproduced this on wake three. Dispatch stayed
closed and the reservation remained accounted for. This is separate from the
previous exact-marker prompt failure; the timeout prevented the resume call.

The scheduler snapshot itself took 33–40 ms, with no wait for the safe-point
hook. Successful transport calls took 100–1132 ms (median 600 ms), much of it
before requesting the snapshot or after receiving it. One failed connection
had already stored a valid snapshot but could not return it in time. The
scheduler was consuming a CPU core while otherwise idle. Its busy event loop
and the observation transport share the scheduler process's Python threads.

Two controlled diagnostic runtime copies kept the installed binary, engine
installation, model, effective recipe, deadlines and authentication unchanged:

- Reducing Python's thread-switch interval to 1 ms did not resolve the delay:
  successful calls had a median of 778 ms and parking failed on cycle four.
- Yielding for 1 ms after each scheduler observation hook passed five consecutive
  park/wake cycles. All 55 observations completed in 34–49 ms (median 39 ms),
  with snapshot work taking 33–43 ms and at most 1 ms before requesting it.

This supports scheduler-loop starvation of the observation transport as the
cause on this recipe. The always-yield diagnostic is not a production change
or a general performance qualification. Next is a narrowly scoped yield while
an observation is active, preserving all custody, authentication, deadline and
accounting rules, followed by regression checks and another installed-release
live pass. Do not increase the timeout on this evidence alone.

The diagnostic deployments were deleted with verified stop, their roles were
stopped, and no local GPU compute process remained. No engine environment,
driver or network configuration changed. Local, untracked evidence is under
`target/live/release-fresh-c4e132c/`, `target/live/sglang-timeout/`,
`target/live/sglang-fairness/` and `target/live/sglang-yield/`.
An earlier interrupted test's uncertain ledger was retained after its orphaned
processes were stopped from verified ownership evidence; it was not rewritten.
CPU and Fake-engine tests are not native recipe qualification.

The new custom-engine model test and multi-host model placement remain deferred
until the project release work is complete.

## SGLang exact-marker prompt — 2026-09-27

The wake probe now explicitly requests the two letters `OK` without punctuation.
The exact response check, eight-token budget, temperature, restoration checks
and uncertainty accounting are unchanged. No new setting or fallback is added.

A controlled native test on Qwen2.5-1.5B-Instruct with SGLang 0.5.20 and
host-backed residency reproduced the old prompt's `OK.` response before parking
and in the actual wake probe. That wake stayed uncertain with dispatch closed
and its reservation retained. In a diagnostic build of `1d32d4e`, changing only
the prompt's wording (plus response classification instrumentation) produced
exact `OK` and passed three consecutive park/wake cycles. The same GPU process
released memory to 966 MiB while parked and recovered to about 7.5 GiB after
wake. Eighteen routed before/after samples consistently distinguished the two
prompts. All test deployments and roles were stopped with verified cleanup.

All 25 SGLang control tests pass, including strict rejection of `OK.` and other
incorrect markers. The core suite passed 1113 distinct tests (1114 reported),
and the workspace suite passed 2257 distinct tests (2258 reported), with zero
failures. Clippy with warnings denied across the core crates, formatting and
diff checks pass. CPU and Fake-engine tests are not native recipe qualification.

This is evidence for that selected recipe, not every SGLang model or build.
The diagnostic evidence is local and untracked under
`target/live/sglang-probe-review/`. Release assets now contain this prompt change; the newer installed-release
timeout investigation above records the remaining small-model limitation.

## Inventory collector trust check — 2026-09-27

The shared boot collector now checks the runtime tree and its ancestors using
the existing native-launch integrity rules before running Python. Both remote
and standalone roles pass the selected runtime directory directly. Isolated
Python imports use that directory, including custom directory names, without
ambient package or site hooks. An unsafe runtime publishes no inventory.

Two regression tests cover refusal before execution for writable modules,
helpers, directories and ancestors or a symlink, plus successful isolated
collection from a trusted custom directory. All 12 inventory tests pass, and
the actual collector ran successfully under isolated Python on the control
host. These checks are not native-engine qualification; the selected-recipe
evidence below remains the live record.

The core suite passed 1113 distinct tests (1114 reported) and the workspace
suite passed 2257 distinct tests (2258 reported), with zero failures. Clippy
with warnings denied across the core crates and CLI, formatting and diff
checks pass. The release assets must be rebuilt from the merged fix before
publication.

## 0.1.0 installed-binary guide validation — 2026-09-27

Selected-recipe guide validation is complete with the limitation below. It
began at merged commit `bca166c`; the fixes and evidence are in the commit
containing this entry. The full release remains an unpublished draft. Public installation remains pending the repository
transition, owner publication and site deployment. The historical F2 records
below are retained as evidence; their branch and pending-work statements apply
to their dated entries.

Both release architectures built from `bca166c` with clean BUILDINFO records.
Each architecture produced identical archives on repeated builds, passed the
42 installer checks, checksum and embedded-runtime checks, and the private
name/path scan. The control host passed strict packaging verification; the ARM
host skipped ShellCheck because it is absent there (the same scripts passed it
on the control host). The public installer URL still returns 404 before launch.

The guide walk found these changes:

- An immediate `start --evict` after deploying a checkpoint whose sizing is
  pending stopped a serving victim and then refused the target start. The
  preflight now reuses the store's checkpoint and source admission checks
  before eviction. A regression test reproduced the stop before the fix and
  verifies both deployment and instance starts leave the victim serving.
- A fresh remote host omitted the device inventory digest and physical GPU
  UUID required by SGLang, so the downloaded checkpoint was refused at the
  native placement gate. Remote startup now collects the same physical
  evidence as standalone and keeps it across engine registration. Explicit
  policy pins are preserved. An enrolled alias may differ from the kernel
  hostname: the frozen inventory digest still binds the physical host, boot
  and devices, and the namespace and UUID checks remain mandatory. Tests
  reject a different physical host even when the logical alias matches.
- `park` returns when accepted; the guide now distinguishes `parking` from
  completion. The switch example uses `--evict --wait`, which waits for
  checkpoint measurement and readiness, and distinguishes a pressure stop
  from an operator stop.
- Plain `scp` copied an invitation with mode 0644 and enrollment refused it.
  The guide now preserves permissions and explicitly sets the invitation to
  0600 on the receiving host.

Live evidence is local and untracked under `target/live/release-010/` in the
validation checkout. The installed baseline binary passed standalone vLLM
chat, streaming, key rejection and host-backed park/wake, and remote vLLM chat,
streaming and deep park/wake through the server. Both remote hosts enrolled
and published their existing vLLM and SGLang installations. The second host
materialized and verified the guide's Hugging Face source (8,060,917,568 bytes,
revision `cdbee75f17c01a7cc42f958dc650907174af0554`). With the host-inventory fix,
SGLang served chat and streaming, parked deep, and answered correctly after a
59.76-second wake while retaining the same two GPU processes. Adding another
engine live preserved the placement evidence. An authenticated request from
another machine reached the server; the same request without a key returned
401. These are selected-recipe checks, not a claim about every model or engine
build.

The optional small SGLang checkpoint served and parked on the discrete GPU,
but wake became uncertain on two attempts. Its answer to the adapter's probe
prompt was `OK.` rather than the required exact `OK`; the adapter's strict
probe remains unchanged. This installed baseline recipe is not validated for wake.
Reservations and closed dispatch were retained, and an explicit stop verified cleanup.
The guide's larger checkpoint passed on the remote host as recorded above.

The patched installed x86-64 binary also passed the live eviction regression:
a pending checkpoint start was refused, the serving model retained the same
GPU process, no switch began, and the next inference request answered correctly.

The eviction regression and all eight switch-management tests pass. The core
suite passed 1113 distinct tests (1114 reported, including the nested owned-state
summary), with zero failures. These CPU and Fake-engine tests are not native
engine qualification. The final workspace suite passed 2255 distinct tests
(2256 reported), with zero failures. All 111 focused Python placement, entry,
composition and startup-gate tests pass. Clippy with warnings denied (the core
crates plus CLI, config and agent), formatting, site publishing checks with the
private-name scan, and both updated packages' installer/content checks pass.

All test deployments were stopped with verified cleanup and deleted from the
remote server. No test role or GPU compute process remains on any of the three
machines. The control host's pre-existing identity state was restored; generated
validation state and logs were archived locally, not deleted. The downloaded
model remains in the second host's model store.

Next: review the unpublished draft, run the sanitized-history transition, rebuild
from the rewritten release commit, and have the owner publish. The public
installer and Pages deployment must then be checked against that public release.
Multi-host tensor/pipeline parallelism remains deferred.

## A command uses the role on its machine; saved contexts removed — 2026-09-26 (branch `chore/remove-contexts`)

Owner decision 2026-09-26: the user should not have to set the configuration
every time, and a command run on a machine knows the role running there. The
saved contexts added on `fix/first-run-ux-2` (the `mllm context` commands,
`--context`, `MLLM_CONTEXT`, `MLLM_CONTEXT_KEY` and the files under
`~/.config/mllm/`) are removed with their tests and documentation.

- **Which role a command uses.** `--config` > `MLLM_CONFIG` > the role running
  on this machine, found under the state root: a server's or standalone
  role's credentials and recorded management address, and the document a
  server or host was started or enrolled with when one was named
  (`<state root>/run/server-document`, `<state root>/run/host-document`, 0600),
  so the packaged system units are found without `--config`.
- **Host machines.** `mllm engine` and `mllm config show` use the host's
  document (recorded, else `<state root>/config/host.yaml`), and `engine add`
  writes the `engines.yaml` the host reads. A command that needs the server is
  refused with "This machine is an mllm host; run this command on the server".
- **More than one role.** Server or standalone with a host: the server or
  standalone role. Server and standalone: the one that answers, else refused
  naming both and the `--config` that chooses. `config show` with more than
  one role is refused, naming them (`--role` or `--config` chooses).
- CPU tests only (`crates/mllm-cli/tests/local_role.rs`): detection of each
  role, recorded server and host documents, precedence, the host refusal, and
  that no context command or flag remains. Not qualification of any engine.

## First-run UX, second pass — 2026-09-26 (branch `fix/first-run-ux-2`)

Fixes from a first-run walk with the real binary, each with a regression test
that failed first, plus the owner's 2026-09-26 decision on client commands.
CPU tests, fake installations and scripted management APIs only; none of this
is qualification of an engine recipe. The host session fix was also checked
by hand on this machine with fake engine environments (a server and five
hosts, one declaring its discrete GPU as a device domain).

- **`status` names a failed launch.** LAST ERROR falls back to the failed
  latest operation's code and first message line instead of `-`.
- **`start` right after `stop`.** `start --wait` waits, within the start's
  window, for the stop to settle and then starts; a plain start refused
  `runtime_retained` while stopping says so and exits 25 (`still_stopping`)
  instead of 2.
- **`start host`** prints a ready line like standalone's (state directory,
  ingress listener, identity file). A fresh host's first control session was
  refused ("host inventory publication refused") and reconnected: its first
  inventory raced the GPU collector on a host declaring a device domain, so the
  device was published unobserved. A host with no executor (no ingress) sent
  the startup snapshot, older than the observation TTL once installations had
  been measured, and was refused on every session. The first inventory of a
  session now waits (bounded, off the session loop) for a device sample, and a
  host with no executor measures its domains again per session.
- **`init host`** leaves the models directory to the shared default
  (`~/models`, downloads in `~/models/sources`, allowed) and writes the
  resource policy standalone derives from the same machine, so the generated
  document validates as written.
- **Plain runtime wording.** Warnings and validation errors no longer cite
  requirement sections or decision records; the wording gate now scans string
  literals in the CLI and configuration crates.
- **`--output` help** is shown at the top level and on `init` and `invite`
  only; the flag still parses anywhere.
- **Client commands without `--config`** (owner decision 2026-09-26). They
  use the role running on the machine; the saved contexts this branch first
  added were removed afterwards (see the section above).

## Discrete GPU live re-check after the fix wave — 2026-09-26 (branch `feat/discrete-gpu-network`)

Live re-check of the rows the final review left owed, with the branch's
release build (commits `9193a71..` the commit that records this section; the
two fixes below are `da51752` and `513bade`). The 16 GB discrete-GPU laptop
host ran the standalone role with vLLM 0.29.0 and SGLang 0.5.20 registered by
`mllm engine add`, models Qwen3-4B-Instruct (A) and Qwen2.5-1.5B-Instruct (B),
minimal deployment files; other programs held about 30 GiB of its 61 GiB of
RAM throughout. Host A and host B each ran the same aarch64 build as a
standalone role (engines registered by `mllm engine add`, ports by `--set`),
vLLM and SGLang both on Qwen3-4B-Instruct with `residency: deep` and a 40 GiB
memory request so the two cannot be resident together. Evidence is local and
untracked (`target/live/dgpu2/` on the control-plane host). CPU and
Fake-engine tests are not qualification; these rows are.

| Row | Result |
|---|---|
| DG6 upgrade from 0.1.0-rc.4 | Pass. rc.4 from its release tarball made fresh state on the laptop host (its generated `unified` policy) and served a deployment. After the rc.4 role stopped (engine kept running), the branch build started on the same state: the listener notice once, the document moved to `0.0.0.0:8443` with `standalone.yaml.pre-0.1.0` beside it, "resource policy ... replaced (revision 2): domains [unified] are now [gpu0, system]; stopped with verified cleanup first", "re-sized ...". The rc.4 engine was stopped; the deployment came back as revision 2, stopped, and the first request started it cold (55.6 s, HTTP 200). On the machine's LAN address `/v1/models` gave 200 with the key and 401 without or with a wrong one. A second start printed no notice and re-attached the running engine. |
| DG1 vLLM `host_backed`, observed free RAM | Pass. Every A↔B switch planned the victim a stop up front (`park_does_not_fit`, no failed park), because host RAM could not take the copy: about 29 GiB available less the 12.2 GiB reserve left about 17 GiB, and the other model's system charge (4 GiB process placeholder plus 1.5 × its weights) left less than the victim's copy (A 11.2 GiB, B 4.3 GiB). Cold switches 39–49 s. A alone: card 13 281 MiB in use; explicit park 5.3 s, card → 1 009 MiB (A's process 868 MiB), A's host memory 1.9 → 12.5 GiB, available RAM 29.4 → 18.5 GiB after the wake (vLLM keeps the pinned copy); wake 1.39 s (request included), same process. |
| DG3 SGLang parking | Pass after a fix. The saver library check now warns (`mllm_saver_library_permissions`, `group_undetermined`, `warned`, in the engine log; visible only with `--debug-engine-logs`). Parks then reached the saver observation, and the busy scheduler answered in 0.9–1.6 s against the 1.5 s bound: a late answer after the release left the park `uncertain` (accounting kept), one before it refused the park. After the fix (below): B `deep` park 3.7 s, card 7 110 → 410 MiB, wake 10.5 s; B `host_backed` park 4.8 s, card 7 110 → 410 MiB, process host memory 2.0 → 5.1 GiB, wake 15.0 s; A `host_backed` park 6.9 s (card 11 506 → 494 MiB, host 2.0 → 9.9 GiB), wake 9.7 s; A `deep` switches with B: A parked (released: parked) and woke in 12.7–16.9 s, same processes. |
| M12 retry after a failed launch | Pass. A vLLM deployment with a malformed engine argument: three requests in a row each started a new activation and each answered 500 `activation_failed` ("operation ... failed with code launch_failed") in 10.3 s, with a new operation id each time; none was 409. |
| vLLM memory within its reservation | Pass. A ready: the ledger charges `gpu0` 14 220 787 655 B (13.24 GiB, request plus the 1.25 GiB context charge); the engine process holds 13 160 MiB (12.85 GiB), the card 13 281 MiB in all. |
| Unified boot and UUID cross-check, host A and host B | Pass on both. The policy has one `unified` domain, `gpu0` maps to it, no device domain; `gpu0` carries `physical_gpu_uuid` equal to `nvidia-smi`'s UUID for the device at `0000000F:01:00.0` on each host. |
| Unified switching vLLM ↔ SGLang, deep, host A | Pass. Every request answered 42, no refusal. Ready charge 41.25 GiB (40 GiB request plus 1.25 GiB context), parked 2.0 GiB, managed limit 60.84 GiB. Each switch parked the victim (`released: parked`) and reused the same two engine processes. vLLM park about 1.1 s and released 33.2 of the 38.6 GiB its Ready state took (86 %, MemAvailable); vLLM wake 6.6–7.1 s alone, 12.0 s while parking SGLang; SGLang wake 61.5 s (weights reloaded from disk). rc.4 on this host: park about 2 s, 80–82 % of 25.2 GiB, wake 8.8 s. |
| Unified switching, host B | Pass, same shape. vLLM park about 1.1 s, released 32.7 of 38.1 GiB (86 %); vLLM wake 7.6–9.5 s; SGLang wake 59.4 s; same processes throughout; no refusal. |

Fixes found live (each with a CPU regression test that failed before):

- `da51752`: an SGLang saver read that misses its bound is read again. The
  enrolled source uses the protocol's largest bound (2 s) and makes up to three
  reads, each with a fresh request id; only a whole, bound answer is evidence.
  Test `a_saver_read_that_misses_its_bound_is_read_again` (fixture `stall`
  command). Live: the DG3 parks above.
- `513bade`: a victim stopped because its parked footprint did not fit is
  reported `released: stopped (no room to park)`. It was "host RAM full" even
  for a `deep` SGLang victim whose residue did not fit on the card (seen live
  in DG3). ADR 0019 and the install guide updated.

Verification after both fixes: core suite 1112 passed, 0 failed; `mllm-agent`
all targets 0 failed; Python runtime suite 283 passed plus the known
`TMS_SOURCE_ARCHIVE` fixture error; clippy (workspace, warnings denied) and
`cargo fmt --check` clean.

Open, not fixed here:

- The SGLang saver warning is written only to the engine log, which is empty
  unless the role runs with `--debug-engine-logs`; an operator never sees it
  by default.
- On the laptop host the SGLang scheduler still answers saver reads in
  0.7–2.0 s (it spins its main thread); the retry covers it, but a read that
  misses three times leaves the park uncertain.
- `status deployment` after a failed launch shows the instance's LAST ERROR as
  `-` while LAST OPERATION says `initialize failed (launch_failed)`.

Host state afterwards: the laptop GPU idle (113 MiB, no compute process), and
the rc.4 and branch state roots, `~/.config/mllm` and the extracted rc.4
tarball removed. Host A and host B: no mllm role, engine or GPU compute
process, no tmux session; the state roots, `~/.config/mllm`, the build tree
and binaries removed.

## Discrete GPU live rows DG1–DG7 — 2026-09-26 (branch `feat/discrete-gpu-network`)

Live on the 16 GB discrete-GPU laptop host (one 16 GB card, 61 GiB of RAM of
which other programs held about 30 GiB throughout), standalone role of the
branch's release build, vLLM 0.29.0 and SGLang 0.5.20 registered with
`mllm engine add`, models Qwen3-4B-Instruct (A, 8.04 GB of weights) and
Qwen2.5-1.5B-Instruct (B, 3.09 GB), minimal deployment files (name, engine,
model; residency set per row). Harness `scripts/live/matrix/discrete_gpu.sh`;
evidence in `target/live/dgpu/` on that host (untracked). Commits `ba70c21..`
the commit that records this section (fixes `12719ba`, `f9dd21f`, `aaa0382`,
`1d67977`, `d935dc7`, `d50444b`). Core suite 1104 passed, 0 failed; config,
scheduler, agent, domain and protocol 639 / 0; CLI 304 / 0; clippy clean. CPU and Fake-engine
tests are not qualification; these rows are. The multi-GPU picker has no live
evidence in the repository (one GPU here). The unified-host behaviour is not
testable on this host and was not rerun; the GB10 regression row is pending.

| Row | Result |
|---|---|
| DG1 vLLM `host_backed` | Pass with a host-RAM caveat. A cold 45 s (card 13.2 GiB used). Park 5.4 s: card 13 281 → 1 009 MiB, A's process 868 MiB; A's host memory 1.9 → 12.2 GiB (the pinned copy, 11.1 GB = 1.38 × the weights). Wake 1.4 s (request included), same process. B parked: 778 MiB on the card, 4.1 GB pinned; B's wake 0.8 s. In an A↔B switch A parks, then B's start reclaims (stops) it: with the RAM other programs hold, A's copy plus B's charge does not fit above the 20 % system reserve, so each switch is a cold start (72–78 s). No stall. |
| DG2 vLLM `deep` | Pass. A and B switch both ways reusing their processes: A's wake 11.5–14.6 s, B's 7.4 s, A's park 0.6 s; residues 868 / 778 MiB. |
| DG3 SGLang, both tiers | Serves A (66 s cold) and B. Every park is refused before any effect (`park_refused`, "the engine is not quiescent"): the saver binding refuses the SGLang environment's `torch_memory_saver` library (`unsafe_library`), because the environment's files are group-writable and this host's account database (`sss`) cannot prove the group private. The switch then stops the victim (cold switches 26–38 s). Environment, not product: needs `chmod -R g-w` on that environment by the owner (not done; engine environments are not changed). |
| DG4 vLLM A, SGLang B | Pass for switching both ways (A parked then reclaimed, B's park refused as in DG3, so stopped). A's explicit park 5.3 s, wake 1.4 s. |
| DG5 refusal | Pass after a fix: A with a 12 GiB KV cache derives a 21.7 GB request against 15.8 GB managed; `deploy` accepts it provisionally (exit 0, digest measured after), `start --wait` exits 4 `insufficient_device_memory` 0.6 s after the deploy; no engine started. |
| DG6 network | Key: on this machine's tailnet address `/v1/models` is 200 with the key, 401 without or with a wrong one; bound there, loopback is refused; bound to loopback, the tailnet address is refused. A keyless run beyond loopback was not made (owner rule for this host). Not tested from another machine. `config show` lists each setting with its source (`default`, `yaml`, `set`). Upgrade from 0.1.0-rc.4: the notice appears once, the document holds `0.0.0.0:8443`, `standalone.yaml.pre-0.1.0` sits beside it. **Blocker:** the upgraded standalone then refuses to start (`resource policy revision conflict`, exit 1 `internal`): rc.4 stored a unified policy for this machine and this build derives `system` + `gpu0`; see "Owner attention". |
| DG7 remote host | Not run (optional for 0.1.0); pending. |
| Extra | `restart_only`: every release is a stop, every return a cold start. GPU pin: the engine is started with `CUDA_VISIBLE_DEVICES` set to the card's UUID. vLLM serves A with a fitted context of 26 752 tokens after the context fix. |

Measured, to replace the placeholders: parked device residue 868 MiB (4B) and
778 MiB (1.5B) for vLLM against the 1 GiB placeholder; engine host memory
(anonymous plus shared) 1.8–1.9 GiB for vLLM and 2.0 GiB for SGLang when ready,
against the 4 GiB placeholder; vLLM's pinned `host_backed` copy 1.37 × the
weights, kept after the wake; vLLM's own process holds 13.2 GiB of the card
against a 12.0 GiB reservation (weights, 3.7 GiB of KV cache and about 1.5 GiB of
CUDA graphs and context).

Product fixes found live (each with a CPU regression test; "live" means the
row above exercised the fix):

- vLLM refused a fitted context by one block (its null block): the fit leaves
  one block to vLLM (live).
- A park was charged its whole parking footprint against free memory, so a
  `host_backed` park on a small card was held until its deadline; a park or
  wake is now charged only what it adds beyond the owner's own charge on
  `device` and `distinct` domains, and a domain it adds nothing to is not
  judged on free memory (live).
- A settled parked owner is credited on `device` and `distinct` domains (its
  residue and its copy are in use); `unified` unchanged (live). Reclaiming such
  an owner in a forecast removes its floors with it (CPU).
- A switch park host memory cannot take is refused `parked_capacity` so the
  switch stops the victim, instead of holding the request for 10 minutes (live).
- The `host_backed` copy is charged at 1.5 × the weights in every phase for both
  engines, and the process sample counts shared resident memory, where the
  pinned copy lives (live measurements; ADR 0019 and the operator guide
  amended).
- Admission sampled GPU processes from a cache that was always past its age
  bound, so it credited nothing; each observation takes a fresh sample (live).
- A failed on-demand activation left every later request answered 409
  `idempotency key identifies a different command`; the key names the latest
  operation (CPU; found live).
- A derived request larger than the card answered `checkpoint_mismatch` (exit 2);
  it is `insufficient_device_memory` (exit 4) (live).

Open, not fixed here:

- An SGLang profile takes no host-fixed arguments, so the Triton attention
  backend goes in each SGLang deployment's `extra_args` (`engine add --arg`
  is refused for SGLang).

### Final review fix wave — 2026-09-26 (commits `6766cbd..8781ef0`)

The whole-branch review (`2fbcc48`) found 1 critical and 14 important
issues. Fixed on CPU, each with a regression test that failed before; CPU and
Fake-engine tests are not qualification, so the live rows below are still
owed:

- **Upgrade of a generated policy (C1, the DG6 blocker).** Standalone replaces
  its generated resource policy on the first start that observes another
  machine shape: engines charged under the old policy are stopped by the
  ordinary Stop first (only verified cleanup releases them), the policy, keys
  and epoch change in one transaction, deployments are re-sized from their
  stored documents (one that names the old domain is listed with what to do),
  and a one-time notice says so. A hand-written host policy is never replaced;
  a changed shape is refused with the recorded and declared domains and the
  recovery steps. ADR 0019 §10a. Live: pass (re-check section above).
- **Planner and memory (I3, I4, M9).** The device domain is charged the
  request plus the engine's CUDA context and graphs (1.25 GiB placeholder;
  vLLM held 13.2 GiB against 12.0), so vLLM now needs a card of about 10 GiB
  or more; a smaller card boots and refuses each vLLM deployment. Park or
  stop is decided from the host's fresh observation as well as the ledger, so
  DG1's host_backed parks that host RAM cannot take are planned stops; a park
  refused for memory is reported `stopped (host RAM full)` (now
  `stopped (no room to park)`). Live: DG1 pass (re-check section above).
- **One rule for every host shape (I5, owner rule 2026-09-26).** Resident
  crediting (parked owners, a park's own charge, `RssShmem`) and the
  switch-park refusal are the same on unified and discrete hosts. Live: the
  unified switching row passed on each lab host (re-check section above).
- **SGLang saver permissions (I2).** A library that fails the owner-only rule
  is observed with one warning in the engine log instead of refused. Live:
  DG3 parks after a further fix (re-check section above).
- **Configuration and network (I1, I8, I9, I10, I11, I12, I13).** A
  read-only document keeps the new inference default while the migration is
  pending; the server takes `--management-listen` / `MLLM_MANAGEMENT_ADDR`,
  clients find a role by its recorded management address, a disagreeing
  state root is refused, `join host --set` works; `devices: [{id: gpu1}]`
  parses; `validate config` runs the start's standalone checks and reports
  unknown weights as unknown; ready lines name the credentials file; the
  tarball ships the linked guides with a link check; help and shipped
  documents carry no process wording (a gate enforces it).
- **Heterogeneous GPUs (I7).** A GPU too small for a model is excluded for
  that deployment; the host is refused only when no GPU fits. No multi-GPU
  live row exists.
- **Minor (M4, M10, M11, M12, M19) and tests (I14).** `device_unobserved` has
  a hint; status names the deployment's installation; a request while the
  checkpoint is measured is a retryable "starting"; concurrent arrivals join
  one activation; the flaky native vLLM tests take ports outside the ephemeral
  range; every CLI test runs against an isolated home.
- **Not done here (I6).** The GB10 PCI/UUID cross-check has a fixture with
  both collectors' formats; live: the unified standalone boot on each lab
  host published the matching UUID (re-check section above).

## Single-box benchmark through mllm — 2026-09-25 (branch `test/model-benchmark`)

Owner-approved experiment: five models, 256 in / 256 out, one user, vLLM 0.29.0
on host A and SGLang 0.5.20 on host B, deployed by mllm and requested through
the router (row M80, bench phase, 1 warmup + 5 measured). Full method, flags,
results and failures: `docs/benchmarks/2026-09-25-single-box.md`. Evidence:
`target/live/bench/` on the control-plane host, run `matrix-20260925T125244Z`.
Not qualification.

- **Live results (decode tok/s, median; baseline → best drafter).** MiniCPM5-2B
  36 → 85 (DSpark, both engines); Qwen3.6-35B-A3B NVFP4 77 → 126 (vLLM DFlash),
  85 → 125 (SGLang MTP); Ling-3.0-flash int4 23 → 48 (SGLang DSpark; vLLM not
  run, needs `trust_remote_code`); Gemma-4-E2B 38 → 96–100 (assistant);
  Qwen3.8-27B NVFP4 10.5 → 27.6 (DFlash2). mllm path overhead 25–85 ms at first
  token, 25–55 ms at stream end.
- **Hugging Face sources worked live** (first use): 17 sources, about 123 GB
  per host, resumed across three host-role restarts; the Wi-Fi link (about
  9 MB/s per host) set the pace.
- **Product fixes, each with CPU regression tests:**
  - Exercised live: vLLM `--speculative-config` is admitted key by key under
    host approval, at deploy time and at launch; it was classed as a path, so
    no vLLM speculation could deploy. Accepted as ADR 0014 Amendment A3, with
    an "Amended by" note in SPEC §8.2. A source copy already verified on the
    host is reused by a new deployment; activation had been refused
    `model_source_pending`.
  - After the owner's decisions, not run live: vLLM needs `nvcc` on PATH to use
    FlashInfer. The CUDA PATH that fixed this live is now an optional,
    host-approved profile field, `cuda_home`: `engine add` detects it, and
    standalone takes `MLLM_CUDA_HOME` (SPEC §13.3 amendment).
  - After the owner's decisions, not run live: mllm sets
    `MAX_JOBS = clamp(floor(MemAvailable / 8 GiB), 1, CPUs)` and
    `FLASHINFER_NVCC_THREADS=1` at launch, logs the choice, and a profile's
    `env` may override either one.
  - After the owner's decisions, not run live: `deploy --activate` and
    `start --wait` wait for a model source that is still downloading, within
    the Initialize window.
- **Still open:** Ling on vLLM needs checkpoint code (`trust_remote_code`),
  which stayed off. SGLang engine output is only captured with
  `--debug-engine-logs`. The CUTLASS fused-MoE JIT ran both hosts out of memory
  once during the run; the new `MAX_JOBS` bound has not been exercised live.

## First-run friction from the guide walk — 2026-09-25 (branch `fix/first-run-friction`)

Four fixes before 0.1.0, found by walking the user guides with the real
binary. Regression tests drive the `mllm` binary (`tests/first_run.rs`,
`tests/validate_config.rs`); CPU, fake installations and a scripted role and
management API only, so none of this is qualification. No live run was made.

- **`engine add` with no role running exits 0** (ADR 0018 §3). No control
  socket, or a stale one refusing connections, means no role runs (the normal
  first run: standalone refuses to start with no engine). The profile is saved
  and the command prints `saved to <engines.yaml> (revision N); start mllm
  (…) to use it` with `published: role_not_running`. A role that is running
  but does not take or answer the request still exits 22.
- **`deploy model --activate` waits for the checkpoint digest** (ADR 0014
  §7). `--activate` (and `start deployment|instance --wait`) poll status while
  the revision's digest is pending, bounded by the start's Initialize window,
  then start. Nothing is started if the bound passes. A plain deploy stays
  asynchronous and names `mllm start deployment <name> --wait`; a start
  refused `checkpoint_digest_pending` says the same.
- **`validate config --host` runs deploy's per-host checks** (ADR 0013 §2–3,
  ADR 0018 §7). It merges the `engines.yaml` beside the host document, and
  refuses a placement selector the host's labels do not match, a host with no
  runtime profile, and a profile the host does not declare
  (`profile_not_published`, exit 24). It also resolves the scoped documents as
  the server does. The result's `requires_server` lists what only a running
  server checks.
- **Relative `--config` and `$MLLM_CONFIG` are made absolute** (ADR 0018 §2)
  once, before any command or role uses them. A bare `host.yaml` had put
  `engines.yaml` at an empty parent directory, whose sync failed after the
  write, so the profile was saved but never published.

## Context fitted to the KV grant; standalone rendezvous root — 2026-09-25 (branch `fix/context-fit-standalone-rdzv`)

Two owner decisions of 2026-09-25. CPU tests only; live proof on real engines
is pending (no live run was made: the hosts were busy with a benchmark).

- **Context fitted to the KV grant** (ADR 0014 §5). With no
  `engine_config.context_length`, the shared launch builders compute the
  largest context the KV cache grant holds from the checkpoint's `config.json`
  (layers, KV heads, head dim; KV element width from `kv_cache_dtype`, then
  `dtype`, then the checkpoint's, fp8 at one byte), cap it at
  `max_position_embeddings`, round it down to a 16-token block (or
  `vllm.block_size_tokens`) and pass it as vLLM `--max-model-len` or SGLang
  `--context-length`, for every profile on both engines. Sliding-window and
  hybrid layers count as full attention; MLA, missing fields, an unknown KV
  dtype or a missing `config.json` fall back to 4096 with the reason. An
  explicit value wins (with a warning when the grant provably cannot hold
  it); a host-fixed `--max-model-len` in `MLLM_ENGINE_ARGS` is kept. The
  standalone `--max-model-len 4096` environment default is removed. The fit
  runs where the checkpoint is (embedded host or host agent) and is not part
  of the effective configuration. `validate config` shows `effective.context`;
  `status` shows each deployment's `context` (`declared`, `host_fixed`,
  `fitted`, `fallback`, or `on_host` for a remote host's revision).
- **Standalone rendezvous root** (SPEC §8.2 / T21). Standalone creates
  `<state>/rendezvous` (0700, refused if not owned/0700, as a host) at start,
  names each SGLang launch's rendezvous directory in it, removes it once the
  launch's processes are proved gone, and at start sweeps directories no
  retained binding owns (never through a symlink, never outside the root).
  The directory name matches the host's (`rendezvous`), not `rdzv`.

Live checks still owed: a vLLM and an SGLang standalone deployment with no
`context_length` start and report the fitted value; an SGLang stop leaves no
directory in `<state>/rendezvous` and none in `/tmp`.

## Table output for record views — 2026-09-25 (branch `feat/cli-table-output`)

Owner decision 2026-09-25 (recorded in SPEC §14): like the docker CLI, commands
that read records print an aligned table by default, terminal or not: `list
hosts`, `list deployments`, `list engines`, `status deployment` (the
deployment, then its instances), `engine list` and `engine detect`. Upper-case
headers, host names resolved from the host inventory (the id when a host has
none, or the inventory cannot be read), memory in GiB, timeouts in seconds;
nested detail stays in the JSON. An empty result prints the headers only.
`--format json` (or `--json`) prints the JSON result byte for byte as before
and reports errors as JSON, exactly as `--output json` did; `--output json`
is still accepted. Mutations, `inspect`, `validate`, `prune`, `drain` and
`revoke` print JSON as before; exit codes are unchanged.

- Rendering: `crates/mllm-cli/src/table.rs` (unit tests: alignment, empty
  results, host-name resolution and fallback, units, host states, status
  sections, engine views). Binary tests: `management_cli` (T10: `list
  deployments` and `status deployment` tables; `--format json`, `--json` and
  `--output json` print identical bytes), `engine_cli` (T37: `engine detect`
  and `engine list`), `host_recovery` (`list hosts` names a revoked host).
- Every CLI test that parses JSON passes `--format json`.
- Live matrix: every script passes `--format json`. `lib.sh`'s `cli` probes
  the binary once and translates the flag to `--output json` for a release
  from before this change (ENG4's rc.3 binaries, release validation).
- Verified locally: workspace and core suites, clippy with warnings denied,
  `cargo fmt --check`, `scripts/test-install.sh`, `scripts/verify-packaging.sh`,
  harness dry-runs of M73, M08 and ENG1 to ENG4 (M54's dry-run fails the same
  way on `main`). CPU and Fake-engine tests only; no live run, and nothing
  here qualifies an engine recipe.

## Engine registration — 2026-09-25 (branch `feat/engine-registration`)

ADR 0018 (amends SPEC §4.2, §15.1): `mllm engine detect|add|list|remove` and
`mllm list engines`, live publication (`live_profile_update`), two-phase
removal, standalone `local-vllm`/`local-sglang`. Commits: `deed9ae..HEAD`.

Evidence: CPU and Fake-engine tests only (`crates/*/tests/{registration,engines,
control_socket,live_profiles,engine_cli,standalone_engines}.rs`, plus a
hard-link hardening regression pair added in `registration.rs` this pass).
These are not qualification. Live rows ENG1–ENG4 ran on 2026-09-25; see
"Live rows ENG1–ENG4" below.

Owner decisions of 2026-09-25 are recorded in the plan and ADR 0018; the answer
on the exit code for `profile_not_published` (plan item 20): fail-fast, HTTP
409, CLI exit 24 — the next free exit number, following the accepted 16–23
pattern for the other engine-registration codes.

Final review (local review notes,
range `612ed0d..dc1a426`): 1 Critical and 5 Important findings, plus 12
minors; verdict was not ready until fixed, plus live ENG1–ENG4. A fix wave
(`dc1a426..e7565c1`) addressed the Critical — the role no longer writes
`engines.yaml`; the CLI writes it only after the server confirms the
retirement, running as root under `sudo` for the packaged system units — and
all five Important findings: retirement idempotency so a retry resumes
instead of conflicting, standalone expiry and placement exclusion via store
v36, and an unanswered remove reported as an unknown outcome instead of
"nothing was removed", plus formatting drift and flaky environment-variable
tests. A re-review
(local review notes) read the
fix diff against every finding and ruling and confirmed C1 and I1–I4 fixed
with no new Critical or Important breakage; I5 (the live rows) stays open by
ruling. This session's pass fixed the re-review's Minor 1: a root CLI opening
`engines.yaml.lock` followed a hard link and could `fchown` the file it
pointed at; the lock and the engines file are now opened `O_NOFOLLOW` and
refused unless they are a regular file with one link owned by root or the
state-dir owner, checked before any `fchown`, with a regression test that
fails against the prior code.

Decisions from the plan's local decision record:

- No pre-flight cross-task conflict scan; the owner removed per-task review for
  speed, and the plan's self-review checked type consistency.
- Implementers run the task's own tests plus a build of touched crates; the
  core suite, workspace tests, clippy and fmt run before the final review.
- `profile_not_published` is HTTP 409, CLI exit 24 (fail-fast; see above).
- `expire_profile_retirements` expires only rows in state `retiring`; a
  confirmed retirement stays until the host's re-publication drops the
  profile, so a stale confirmed row cannot block re-adding the same name
  forever.
- An uncertain or unfinished stop keeps a retirement open until it expires
  unconfirmed — the spec never confirms on a guess.
- Any accepted host publication, startup or live, clears that host's
  confirmed retirement rows for profiles the publication no longer lists, so a
  host restart cannot strand a confirmed row.
- The drain poll in the session relay has no bound of its own; the retirement
  service returns Holding (unconfirmed) once the 900 s bound passes, so a
  drained removal cannot poll forever.
- The role binding the control socket must first check its parent state
  directory is owned by the running user, mode 0700, and refuse to bind
  otherwise, closing the bind-to-chmod umask window.
- `engine add` works before any role has ever started (the first-run path):
  the state root is created owner-only (0700) if missing, `engines.yaml` is
  written, and the command reports `agent_unreachable`.
- Roles resolve `engines.yaml` with the same rule as the CLI, including
  `$MLLM_CONFIG` when `--config` is absent, so a role and `mllm engine` always
  agree on the file.
- (Outside this plan) A standalone vLLM deployment generated by the template
  never parked, because residency ignored the deep-park switch regardless of
  it — contradicting the owner rule that standalone must not differ from
  server mode; fixed on a separate branch and live-verified with rc.4.
- (C1) The CLI is the only writer of `engines.yaml`; the running role never
  writes it. Removal: the CLI asks the role to retire, waits for the
  confirmation, then writes the file and asks the role to reload. Under the
  system units the operator runs `sudo mllm engine … --config
  /etc/mllm/host.yaml`; the CLI keeps an existing file's owner and mode, and
  creates a new one for the role's service user, mode 0600, so
  `ProtectSystem=strict` never blocks the role. A crash between the
  confirmation and the CLI's write leaves the profile in the file while the
  server keeps it out of placement; running `remove` again finishes it.
- (I1) A profile retirement's idempotency key is stable per (host, profile
  name) until the retirement is cleared, so a retried `remove` resumes the
  same retirement instead of conflicting with it; an accepted publication
  that no longer lists a profile clears its confirmed row, on both the
  startup and the live path.
- (I2, I3) Standalone runs the same expiry and the same placement exclusion
  as the server, including a publication row (store v36), so a removed
  profile can never start again and a crash mid-drain does not wedge the
  name.
- (I4) When the CLI loses the connection or times out during `remove`, it
  reports the outcome as unknown, not "nothing was removed", and tells the
  operator to run `mllm engine list`; the client's wait bound carries a
  margin over the role's 960 s plus the drain timeout.
- (I5) Live rows ENG1–ENG4 ran on 2026-09-25 (below).

Live rows ENG1–ENG4, 2026-09-25. Host A is the first host, host B the
second; the server runs on the control-plane host. The branch was built from
the synced tree on all three machines, and only the existing vLLM 0.29.0 and
SGLang 0.5.20 environments were used. Evidence (local, not committed):
`target/live/engreg/`.

| Row | Live verdict |
|---|---|
| ENG1, host A and host B | pass on both. `engine detect` with no `--path` listed both home-level environments from metadata. The host ran under a transient systemd user unit with a document that declares no profiles. `engine add` of each environment exited 0, `published`, `custom: false`: vLLM in 12.2 s and 12.8 s, SGLang in 7.3 s. These times are the whole command, including the bounded version check and the deep-park probe; the plan's "within 10 s" is not met for vLLM, whose version check alone takes several seconds. `host.yaml` was byte-identical afterwards (`sha256sum -c` OK), and `engines.yaml` reached revision 2. `list engines` on the server showed both profiles. v\*-4 and s\*-4 on the new profiles reached Ready, answered 42, stopped with verified cleanup and were deleted |
| ENG3, host A | pass. With va-4 Ready, `engine remove vllm` was refused `profile_in_use`, naming va-4; the deployment stayed Ready and answered. `engine remove vllm --drain` returned in 1.2 s (`removed: vllm`, revision 2); the deployment was stopped with verified cleanup. `list engines` no longer showed vllm. `start --wait` was refused `host_ineligible` ("no approved configuration carries a runtime profile whose build it reported"). `engine add` published the profile again (revision 3) |
| ENG2, host A (standalone, both variables set) | pass after the fix below. `engine list` shows `local-vllm` and `local-sglang`, `published`, `source: environment`. `engine remove local-vllm` is refused `invalid_config` (environment profile). A deployment on `local-vllm` served: its argv carries the host-fixed `--max-model-len 4096`. A deployment that also states `context_length` is refused `invalid_config` ("the installation's host-fixed args already set `--max-model-len`"). A deployment on `local-sglang` served. `engine add --name vllm-reg` of the same vLLM environment was published beside them. See also the registered-profile check below |
| ENG4, host B | pass. An rc.3 agent was online (`supported`) with the new server. The new CLI's `engine add` exited `agent_unreachable` and wrote `engines.yaml` (revision 1). After a restart the rc.3 agent still published nothing from that file. After an upgrade to the new binary, the host published `vllm` at start. Against an rc.3 server, a new agent's `engine add` answered `published: restart_required`, and after a host restart the rc.3 server listed `vllm` for the host |

Registered vLLM profile without a `--max-model-len` default (ENG2). A profile
added with `engine add` carries no arguments, so the environment profile's
`--max-model-len 4096` default does not apply. A deployment on it that states
`context_length: 16384` passes `--max-model-len 16384` and serves. A deployment
that states no context length launches with the model's own maximum.
qwen3-4b-instruct has a 262144-token context, so vLLM refused at start:
"To serve at least one request with the model's max seq len (262144), 36.0 GiB
KV cache is needed, which is larger than the available KV cache memory
(4.0 GiB)". The operation failed as `launch failed: the engine exited before
readiness`, with the engine_config hint, and cleanup was clean. This is
vLLM's own limit, not an mllm defect. An operator who registers a vLLM
environment must state `context_length` in each deployment, or add the
profile with `--arg --max-model-len --arg N`. Whether the documentation
should say so, or a registered vLLM profile should carry the same default
as the environment profile, is an owner decision.

Found and fixed during the live rows:

1. **Product: `engine list` did not show standalone environment profiles.**
   The first ENG2 run returned an empty list. `list` read only `engines.yaml`
   and the role document, and dropped the profiles the role reported as
   accepted from `MLLM_VLLM_BIN`/`MLLM_SGLANG_BIN`. It now appends every
   profile the role accepted that neither file names, with
   `source: environment`. Regression test (T16):
   `engine_cli.rs::list_shows_the_roles_environment_profiles`, which fails
   without the fix. The rerun of ENG2 shows both profiles (live proof).
2. Harness: `roles.sh` `server_init` copied the snapshot build over
   `MLLM_LOCAL_BIN`, so ENG4's "rc.3 server" binary was replaced by the new
   one. A named binary is now run as given. ENG4 also read `hosts.json` for
   `"vllm"` anywhere, which the other host's full document matched; it now
   checks the named host's entry only. Its rc.3-server phase now reloads the
   new run's variables and runs in a subshell.
3. Harness: `host_clean` hid leftover rendezvous directories. `ls -d A B &&
   echo LEFTOVER_RDZV` exits non-zero when one pattern matches nothing, so
   the directories were listed but never flagged. It now pipes through
   `grep .`.

Found, not fixed (product, open): **a standalone SGLang launch leaves
`/tmp/mllm-rdzv-*` behind.** Both ENG2 runs left one owner-only
`/tmp/mllm-rdzv-*` directory, holding its `store` file, per `local-sglang`
launch after `delete --stop`. The directories were created at the SGLang
launch times, and they were removed by hand after the run. The host role
names each launch's rendezvous directory inside
`<state_dir>/rendezvous/<incarnation>` and removes it on gone evidence (fixed
2026-09-23). Standalone never sets a rendezvous root (`ProfileBindings` has
none), so the entry falls back to its own temporary directory, and a
signalled stop never runs the entry's exit handler. This predates engine
registration (same path on `main`) and breaks the rule that standalone must
not differ from server mode. A fix needs standalone to own a rendezvous root
and to retire each launch's directory where its local cleanup proves the
group gone (standalone does not retire saver enrollments there either). That
is a design choice in the coordinator's local cleanup, so it is left for the
owner. With the `host_clean` fix, ENG2's idle check now fails on this until
it is fixed.

Local verification of the fix (CPU only, not qualification): `mllm-cli`
all-targets 208 passed; Clippy on `mllm-cli` clean with warnings denied;
`cargo fmt --check` clean.

Host state after the rows: every role was stopped (`roles.sh down`, each
role signalled by its recorded identity). The branch's trees, run directories
and the rc.3 binary copy were removed from both hosts. Neither host has an
mllm, engine or GPU compute process, a tmux session or a rendezvous directory.
`~/.config/mllm` is absent.

Open items:

- Standalone SGLang rendezvous directories (above).
- Registered vLLM profile and context length (above): owner decision.
- Version skew on removal: an older CLI's `remove` can still reach a newer
  role and get back a confirmed retirement, but does not know to write
  `engines.yaml` on this path. The retirement keeps the profile out of
  placement, but it lingers in the file until a current CLI runs `remove`
  again. Worth a release-notes callout.
- Minor findings left unfixed this round: `final-review.md` minors 2–12, and
  `final-rereview.md` minors 2–6 (a `standing`-map entry that outlives a
  resumed-then-cleared poll; standalone `reload` committing the embedded
  publication and `host.replace` separately; `publish_at_start` collapsing
  every store error into one message; a rename race in the control socket's
  directory-owner read; a loose `"1 passed"` substring match in an isolated
  test). `final-rereview.md` Minor 1, the `engines.yaml.lock` hard link, is
  fixed as of this session's commit.

## Release candidate 0.1.0-rc.4 — build and live pass, 2026-09-25 (branch `docs/rc4-live-evidence`)

Host names in this section: host A is the first host, host B the second (the
tight-policy host); the control-plane host runs the server. Evidence (local,
not committed): `target/live/rc4/`.

**Cleanup of the observation debug run.** The roles left from run
`matrix-20260925T021319Z` (both host agents in tmux and the server) were
stopped with `roles.sh down` (each role signalled by its recorded identity and
exited). The debug tree `~/mllm-obsfail` and the run directory were removed
from both hosts. Afterwards neither host had a tmux session, an mllm, engine or
GPU compute process, or a rendezvous directory.

**Release.** Draft pre-release `v0.1.0-rc.4` (unpublished; the owner
publishes) now targets `main` at 612ed0d (PR #28 drain fix and PR #29
standalone vLLM deep park). Both tarballs were rebuilt from 612ed0d: x86_64 on
the control-plane host, aarch64 natively on host A (nice 19). Each passed
`scripts/verify-packaging.sh` on its own architecture (shellcheck is not
installed there, so that check was skipped). `BUILDINFO` records commit
612ed0d, not dirty, and runtime manifest `dec9dca0…91561`. The assets were
replaced and the notes updated to add #29.

| Asset | sha256 |
|---|---|
| `mllm-0.1.0-rc.4-linux-x86_64.tar.gz` | `69d3e9ce61a45c72243134257d7319e0ded8634483ce0c7a26b5993257b69a24` |
| `mllm-0.1.0-rc.4-linux-aarch64.tar.gz` | `df08d2b2da12c99126936ad9d8c917d4d5d869aea2093023b9f69a65a37d8b63` |
| `install.sh` | `a83b56109bba710a84fa3dfb5919c1caebc5c8c980c12cd81c8a3a0fae2812d0` |
| `SHA256SUMS` | `bcb272df7eac00b1ad04c1f220d08d8e206196bd1a8f5c1dcde8d00fdb33b823` |

**Live pass, installed binaries only.** Every role ran from binaries installed
by `install.sh`, fed from a `file://` mirror of the draft's assets (downloaded
back from the draft and checked against its `SHA256SUMS`). The roles ran under
the packaged systemd user units with fresh state: the server's state in its
own directory, the hosts in the default layout (`~/.local/state/mllm/host`,
document at `~/.config/mllm/host.yaml`, managed runtime). Only the matrix
harness scripts were copied to the hosts, with no build or runtime tree. Host A
used the normal budget, host B the tight one (managed limit 84 GiB,
`max_parked` 1). vLLM 0.29.0 and SGLang 0.5.20 ran from the existing
environments.

| Check | Live verdict |
|---|---|
| a. M73 va-4, sa-4 | pass. Both engines launched from the managed runtime, listening on loopback only (refused from off-host). Unkeyed engine and control routes returned 401; the router refused unkeyed callers (401) and has no `/metrics` path (404). The answer was correct and the stream well-formed. Stop cleaned up with verification. The restart made a new binding with no reused PID. MemAvailable came back within 0.31 GiB |
| b. Standalone vLLM, generated deployment (#29) | pass on host A, `mllm start standalone` under the packaged standalone unit. The deployment document came from the product's own template (`standalone_config::deployment_document`, deep parking on), which gave `residency: deep` and `--enable-sleep-mode`. Two park/wake cycles: each park settled `parked` in about 2 s and released 20.2–20.5 GiB of the 25.2 GiB Ready drop (80–82%, MemAvailable). The same two processes kept their PIDs and start ticks. A routed request woke the deployment in 8.8 s with the right answer, still at generation 1. A `systemctl --user stop`/`start` of the unit re-attached the running engine. `delete deployment --stop` cleaned up |
| c. `drain host` with `request_deadline: 600s` (#28) | pass on host B. Status showed `request_deadline_ms` 600000. The drain returned `drained: true` with the instance `stopped` and cleanup `verified`, in 1.1 s with nothing refused. Cleanup was verified with no engine, GPU process or port left. The next request reactivated the deployment on demand (20 s, answer correct). Before #28 this drain was refused `LifecycleConflict` |
| d. Refusals (#18) | pass. After an operator stop, a routed request got 409 `deployment_stopped`, and the message names `mllm start deployment <id>`. Host B then ran rc.1 under its unit on the rc.4-written state, and the server listed it `upgrade_required` ("reports no version"). `start deployment`, `start --evict` and `start --wait` were each refused `host_ineligible`, CLI exit 15. The message names the host, "host version unreported, server version 0.1.0-rc.4" and the reason. The deployment stayed stopped. Reinstalling rc.4 made host B `supported`, and the start served |
| e. Two-instance `start --evict --wait` on tight host B (#18) | pass. Incumbent vb-14 (46 GiB) was Ready and vb-4 with 2 instances (2 × 24 GiB) was deployed. `start --evict --wait` exited 0 in 22.6 s with both instances Ready. Its receipt names the one victim (`vb-14/0`), which was parked, not stopped: the tight host allows one parked deployment. Both requests were answered. An earlier run first tried a plain `start --wait`: instance 0 fit next to the incumbent, instance 1 did not, and after the 900 s start deadline it exited 4 (`insufficient_resources` … "the start is partial") with instance 0 left Ready. That matches SPEC §14 |
| f. `revoke host` under the packaged unit (#17) | pass on host A with va-4 Ready. The revoke answered `engines: retained`. The agent logged one `error [host_revoked]` line naming both recovery commands. systemd recorded `status=14`, `Result=exit-code`, `NRestarts=0`, and the unit was still failed 20 s later (not restarted). All three engine identities stayed alive, and dispatch returned 503. Then `invite host <id> --recover` and `join host --recover` (same host id, `recovered: true`) and a unit start: online, Ready, the same three PIDs and start ticks (re-proven, not relaunched), and served |
| g. M28 sa-14 SGLang park/wake ×3 | pass 3 of 3. Each run released 88.7–88.9% of the Ready drop with the same four processes, woke on request in 182–219 s (disk reload), and matched I1 exactly (max logprob delta 0.0). The host journal for the run has 0 `native_observation_*` or `saver_observation_*` lines, including the new `native_observation_slow` |

Scratch rows used for c–f (`DRAIN600`, `EVICT2`, `EVICT2D`, `SKEWD`, `SKEWX`,
`REVOKE`) are kept with the evidence under `target/live/rc4/rows/`, and run
through `SCRATCH_ROWS`. The first `SKEWD` run shows rc=1 because its exit-code
wrapper checked the wrong status; `SKEWX` repeated the refusals and recorded
exit 15 from the CLI itself.

No product bug was found. CPU and Fake-engine tests are not qualification. The
rows above prove the named behaviours only for the q4 and q14 fixtures on
these two hosts.

Host state after the pass: every deployment was deleted and every unit stopped
and uninstalled. `~/.local/state/mllm`, `~/.config/mllm` and the copied
harness and mirrors were removed from both hosts. Neither host has an mllm,
engine or GPU compute process, a tmux session or a rendezvous directory. The
server state stays on the control-plane host under `~/mllm-rc4-server`.

## Standalone vLLM deep parking — 2026-09-25 (branch `fix/standalone-vllm-deep-park`)

Live-proven with rc.4 (see the rc.4 section above, check b). The original
evidence was CPU and Fake-engine tests only.

- Defect: the generated standalone deployment declared vLLM `restart_only`
  whatever the deep-park switch said, so a standalone vLLM deployment launched
  without sleep mode and never parked; idle eviction and switching stopped it
  cold. Server mode deep-parks the same engine (live-proven earlier: 77% of its
  memory released with the same processes). Owner rule: standalone must not
  differ from server mode.
- Fix: the template's residency follows the host's switch for every engine
  (ADR 0012, SPEC §6.2): `deep` when deep parking is on (`MLLM_DEEP_PARK` unset
  or `on`), `restart_only` when the host opts out. A build whose probe finds deep
  parking missing is refused `capability_missing:deep_park` by the protected
  entry, as elsewhere; the host then opts out and gets `restart_only`.
- Tests: a standalone boot with deep parking on parks its generated vLLM
  deployment and wakes the same launch (same endpoint, per-launch key and live
  processes); an opted-out boot's deployment is `restart_only` and its park is
  refused. The Fake gained an opt-in mode that reports real processes as its
  group and follows the embedded vLLM residency contract; the default Fake is
  unchanged. Workspace 1788 passed, 1 ignored; core suite 1012 passed; Clippy
  clean.

## Flaky parallel tests and leaked stand-in engines — 2026-09-25 (branch `fix/flaky-parallel-tests`)

CPU-only test fix; nothing here is live-proven or qualifies an engine recipe.

- Root cause of the intermittent failures (launcher group observation, agent
  `journal` and `native_vllm` readiness): `observe_process_group` failed with
  `Visibility` whenever any unrelated process on the host exited between the
  `/proc` listing and its `stat` read. Under a parallel test run that was nearly
  every scan (a churn regression test failed 200 of 200 observations). It now
  skips a pid that no longer exists, as `scan_group_by_pgid` already did; any
  other unreadable process still fails closed, and a member leaving between the
  two snapshots is still `Changed`. The same failure applied to production
  observations on a busy host.
- Leak: `ready_deep_park` asserted readiness before its callers installed the
  reap guard, so a failed readiness left the stand-in vLLM running (orphans found
  on control-host). The guard is now part of the test `Host`, and the stand-in ends its
  own group if the test process or its fixture directory disappears.
- Evidence: workspace at default threads failed 2 of 10 runs before the fix;
  launchers + agent at 32 threads failed 3 of 10 before (each failure leaked an
  engine) and 0 of 20 after, with no new leaks.

Owner decisions 2026-09-25, both implemented; CPU and Fake-engine tests only, not
live-proven on the hosts.

- A start that places nothing because no allowed host is eligible (drain-only
  after version skew, draining, revoked, offline, reconciling) is refused
  `host_ineligible` (HTTP 503, CLI exit 15), naming each host and why, with the
  host's and the server's versions for a drain-only host. It was
  `capacity_blocked`. `start --evict` checks this before releasing anyone.
- An inference request for an operator-stopped deployment is 409
  `deployment_stopped` (message names `mllm start deployment <id>`). It was 429
  `insufficient_resources`. New code, added to SPEC §10 in the same change;
  `host_ineligible`, the `--evict` coverage and the `--wait` rule are recorded
  in SPEC §14.
- `start deployment --evict` plans every instance the start activates before
  releasing anyone (each against the ledger the earlier ones leave, each victim
  set minimal) and releases per host. If one instance cannot be placed even with
  eviction, nothing is released and the refusal names the instance and the
  host's need, free and evictable memory (`capacity_blocked`, exit 4).
- `start deployment --wait` succeeds only once every instance has been Ready; an
  instance not placed before the start's deadline exits 4, a failed launch 13.

Regression tests (each failed on `main` before the fix): management `evict.rs`
(every replica evicted for, refusal before eviction, `host_ineligible`), router
`router_core.rs` and CLI `stop_intent.rs` (409 `deployment_stopped`), CLI
`start_wait_replicas.rs` (partial start exits 4). Pending: a live run of a
two-instance `start --evict --wait` on a tight host and of a start against a
drain-only host.
## A revoked host exits instead of retrying — 2026-09-24 (branch `fix/revoked-host-exit`)

Owner decision 2026-09-24, fixing the rc.3 observation that a revoked host agent
kept retrying its session a few times a minute. The controller now answers a revoked
certificate's session, and closes its live session, with a typed refusal
(`PermissionDenied`, exactly `host_certificate_revoked`), but only when the
certificate presented over mutual TLS is one it revoked (store
`certificate_revoked`, by fingerprint or with its host); every other failure stays the
generic refusal. The agent acts on that exact answer alone: `run_session*` return
`HostRevoked`, and `mllm start host` logs one line (`error [host_revoked]: ...`
naming `mllm invite host <id> --recover --output FILE` and
`mllm join host --join-file FILE --recover`) and exits with the new code 14
(`ExitCode::HOST_REVOKED`). Nothing is stopped or signalled, so engines stay for
`join --recover` to re-prove (ADR 0016, amended in Consequences). An unreachable or
restarting server, a version refusal, a generic or look-alike refusal, and an
impostor endpoint (certificate not from the pinned CA) keep the existing backoff.
Both host units add 14 to `RestartPreventExitStatus`; `scripts/verify-packaging.sh`
now checks 2, 3, 5 on every unit and 14 on the host units. The standalone role has no
enrolled host to revoke, so its units are unchanged. Exit codes are documented in
`docs/operations/install.md` ("Exit codes the units do not restart").

Tests (T05, T06): store `certificate_revoked_names_only_revoked_certificates`; agent
`only_the_exact_revocation_refusal_stops_reconnecting`; mTLS integration
`revocation_closes_the_session_and_refuses_reconnect_and_commands` (the agent now
returns `HostRevoked`), `only_the_controllers_exact_revocation_answer_stops_the_agent`,
`an_impostors_revocation_answer_never_reaches_the_agent`; CLI
`a_revoked_host_exits_with_its_own_code_and_the_recovery_commands`. With the old retry
behaviour restored, three of the integration tests fail. Local only: core 1007 passed;
workspace all-targets 1772 passed, 1 ignored (`--test-threads=4`; at the default
thread count four load-sensitive tests in unchanged code, `mllm-launchers` process
visibility and `native_vllm` readiness, failed once and pass on rerun); Clippy clean
with warnings denied; `scripts/verify-packaging.sh` passed (shellcheck not installed,
skipped); `scripts/test-install.sh` passed. CPU and mTLS tests are not qualification:
not live-proven. Pending: a live revoke on a host under the packaged unit (exit 14,
unit not restarted, engines alive), then `join --recover` re-proving them.

## SGLang saver observation failure at `receive_header` — 2026-09-25 (branch `fix/sglang-observation-failed`)

The rc.2 M28 sa-14 refusal (`park_refused` after `native_observation_failed` at
`receive_header`) is still **not root-caused**. This branch adds diagnostics only;
it changes no park, wake or observation outcome.

What the rc.2 evidence shows. The host journal on host-a has exactly two
lines at 19:32:56.146 local time (23:32:56.146Z): `native_observation_failed`
`receive_header`, then `saver_observation_unavailable` `observe`. There is no
`sglang_park_not_quiescent` line, so the quiescence read and the
`saver_mapped_before` read passed. The failure was the third saver read in the
park (the precondition read in `Run::run`), 2.53 s after the server accepted the
park. A `receive_header` failure means the socket gave no 4-byte header before
the 1500 ms budget ran out, or the engine closed the connection without a frame.
The bare stage could not tell these apart. The engine's own stderr goes to
`/dev/null` without `--debug-engine-logs`, and the rc.2 host directory has since
been removed.

Code finding (not a behaviour change). When the scheduler does not reach a safe
point in time, the engine never sends its `uncertain` frame. The bridge's slot
expires at the same deadline the transport sends by, and the host's budget starts
before the engine's. So on the host a slow scheduler always looks like
`receive_header`, never `response_status`. Tightening the budgets so an
`uncertain` frame always arrives in time is a possible follow-up. It is not in
this branch.

Diagnostics added:
- Host (`crates/mllm-launchers/src/native_observation.rs`): each socket-stage
  failure (`send`, `receive_header`, `receive_body`, `receive_eof`) logs one line
  with `cause` (`deadline`, `eof`, or `error` plus its errno), the bytes received
  of those expected, `elapsed_ms` against `timeout_ms`, and whether the enrolled
  scheduler is still alive (`owner_alive`). The send and trailing-EOF paths used
  to fail without any stage line. A success that took a third of its budget or
  more logs `native_observation_slow`, so near misses show up in the host
  journal without engine logs.
- Engine (`runtime/sglang_observation_transport.py`,
  `runtime/sglang_scheduler_observer.py`): each served connection logs one
  `mllm_observation_served` line. It carries the outcome, the stage where a
  connection without a frame stopped, and whether a frame went out. It also
  carries the time from accept to when the bridge was asked and answered, the
  bridge's wait for a safe point, the snapshot duration, safe points seen, lock
  contention, and whether the result reached the slot. This line is visible
  only with `--debug-engine-logs`.

Live (host-a, SGLang 0.5.20, qwen3-14b, fixture sa-14, run
`matrix-20260925T021319Z`; evidence `target/live/obsfail/`; the scratch loop row
is `target/live/obsfail/rows/OBS.sh`):

| Run | Result |
|---|---|
| `OBS-sa-14`, 20 park/wake cycles, engine logs off (the rc.2 setting) | 20 of 20 parked and 20 of 20 woke on request. 88.8–88.9% of the Ready drop was released each time. Wake took 185–225 s (disk reload). I1: one capture and 19 passes (max logprob delta 0.0). The same four engine identities stayed alive to the end, and cleanup was clean. **0** `native_observation_*` or `saver_observation_*` lines in the host log |
| `OBS-sa-14-dbg`, 10 cycles, `--debug-engine-logs` | 10 of 10 parked and 10 of 10 woke, with 88.8% released. All 160 served observations came back `observed`. Engine time from accept to close: p50 18 ms, p99 271 ms, max 288 ms. Wait for a safe point: 0 ms every time (the first tick). Snapshot: 13–15 ms. Contention: 0. The 100–290 ms outliers came from the transport thread's own work, not the scheduler. One cycle was high throughout. That fits GIL hand-offs against the busy-spinning scheduler, but it is not proven |

So the failure did not reproduce in 30 cycles, 0 of 30 (rc.2 1 of 3, rc.3 0 of 3).
The measured margin is 18 ms typical and 288 ms worst against a 1500 ms budget.
Hitting `receive_header` needs a stall more than five times the worst one seen.
Candidates the evidence cannot separate: a Python GC pause in the scheduler
(SGLang freezes GC only with CUDA graphs on, and the recipe has them off), a
procfs stall in the snapshot's `/proc/self/maps` read, or a GIL stall of the
transport thread. The new lines will name the stage, cause and timings the next
time it happens.

The live runs used the build before the last two diagnostic additions: the
host's `native_observation_slow` line and the engine line's
`request_ms`/`result_ms`. Those two are CPU-tested only. A third live run
stopped when Tailscale SSH asked for owner re-authentication on both hosts.
It was not retried.

Host state after the stop: both deployments in these runs were deleted. The
last cleanup check found no engine process, no GPU compute process and no
rendezvous directory on host-a. Still running or present, and needing a
reachable host to remove: the host roles in tmux (`mx-host-matrix-20260925T021319Z`
on both hosts) and the control-host server (`mx-srv-matrix-20260925T021319Z`). Also
present: `~/mllm-obsfail` (with `target/`) and `~/mllm-runs/matrix-20260925T021319Z`
on both hosts. The host-a tree was being overwritten by rsync when the SSH
check started, so its runtime files may be a mix of two snapshots. After
re-authentication, run `MLLM_MATRIX_LIVE=<repo>/target/live/obsfail
MLLM_REMOTE_TREE=$HOME/mllm-obsfail scripts/live/matrix/roles.sh down`
(this stops the hosts, then the server), then remove both trees and the run
directories on the hosts.

Local verification (CPU only, not qualification): the core suite passed 1004.
Workspace all-targets passed 1769, with 1 ignored. Clippy is clean with warnings
denied. The `runtime/tests` `test_sglang_*` suite is OK (181).

## Version skew policy and capability gating — 2026-09-24 (branch `feat/version-skew`)

Owner decision 2026-09-24: a SemVer skew policy between server and hosts (ADR 0017,
amending SPEC §13.1). The host sends its release version (`Connect.binary_version`,
field 7) and every post-baseline protocol feature it implements
(`Connect.capabilities`, field 8). Same `major.minor` line is supported; N-1 is
supported with `upgrade_recommended`; older, another major, or no/unparseable version
is drain-only (`upgrade_required`: only Inspect, Terminate, CloseIngress and Probe are
sent; not a placement candidate); a newer host is refused with "upgrade the server
first" and keeps reconnecting. Fourteen post-baseline features are catalogued
(`mllm_protocol::capabilities`); the send path refuses any command needing one the
host did not declare, typed and before anything is sent
(`host_capability_missing:<name>`, `host_upgrade_required`), launch/park/wake preflight
the same gate, placement requires `checkpoint_digest`, `startup_bytes` and
`restore_checkpoint_digest`, and a Terminate carries recorded identities only to a
host that declared them. `model_source_unsupported` became
`host_capability_missing:model_sources`. Versions and verdicts are recorded (schema
v34 `host_versions`) and shown in `list hosts` / `inspect host` and per allowed host
in deployment status. Upgrade order in `docs/operations/install.md`: server first,
then hosts one at a time. Consequence: hosts on releases before this one report no
version and are drain-only against an upgraded server until they are upgraded.

Local verification only: core 1070 reported, workspace 1764 passed (1 ignored), Clippy
clean with warnings denied across the workspace. CPU and mTLS transport tests are not
qualification; no mixed-version fleet has run on the hosts. Pending: a live rolling
upgrade (server first, then host-a, then host-b) once a release carries this change.
## Release candidate 0.1.0-rc.3 — build and live pass, 2026-09-24 (branch `docs/rc3-live`)

PR #13 (the rc.2 live findings) merged as 603ab7f and PR #14 (version bump,
plus the pre-release install docs and installer message) as 17469dd. Draft
pre-release `v0.1.0-rc.3` (unpublished; the owner publishes; the rc.2 draft is
untouched) targets `main` at 17469dd: `mllm-0.1.0-rc.3-linux-x86_64.tar.gz`
(control-host, sha256 `4853c80e…d370`), `mllm-0.1.0-rc.3-linux-aarch64.tar.gz` (built
natively on host-a, nice 19, sha256 `fb516655…20f0`), `install.sh`
(`43dd6183…2879`) and `SHA256SUMS`. Both passed `scripts/verify-packaging.sh`
on their own architecture; `BUILDINFO` commit 17469dd, not dirty, runtime
manifest `80044870…ddee0`.

Live pass with the installed release binaries only (`install.sh` from a
`file://` mirror of the draft, systemd user units, fresh state, no
`runtime_dir`). This time the host state used the documented default layout:
`~/.local/state/mllm/host`, with the host document at
`~/.config/mllm/host.yaml` (the unit's default `MLLM_CONFIG`, no env file).
Evidence: `target/live/rc3/`.

| Check | Live verdict |
|---|---|
| User state root (fix 1 of #13) | pass on both hosts: `~/.config/mllm` created first, no `~/.local/state/mllm`; `install.sh --systemd host` printed `created ~/.local/state/mllm (0700)`; after `init`, `join` and the unit's start it is still a real 0700 directory holding `host/`, `tmp/` and the engine runtime; no new "compatibility symlink" journal line |
| Refused park on a drain-only host (fix 2 of #13) | pass: host-b on rc.1 against the rc.3 server showed `upgrade_required`; vb-4 kept serving; `park deployment vb-4` was refused `park_refused` (`host_upgrade_required`, before any effect); the deployment was `reconciling` for one sample and `ready`, dispatch open, within about 2 s, serving 42, same engine PIDs and start ticks, no host session loss or agent restart. rc.2 reproduced a permanent 503 here |
| Upgrade host-b back to rc.3 | pass: `supported`, state root still a real directory |
| M73 va-4, sa-4 | pass: launched from `~/.local/state/mllm/host/runtime`, loopback only, unkeyed 401, stop and restart clean |
| M31 vb-4 ↔ sb-4, tight host-b, 1 cycle deep | pass: park and wake on the same processes |
| M28 sa-14, three runs | pass 3 of 3 (89% of the Ready drop released each time); `native_observation_failed` did not recur (0 lines in the host journal). The single rc.2 occurrence stays unexplained |

Local (CPU only) for #14: core 1004, workspace all-targets 1767, Clippy clean
with warnings denied, `scripts/test-install.sh` passed including the new
pre-release case. The CPU tests prove the fixes' logic; only the rows above
prove them on the hosts, and none of this qualifies an engine recipe beyond
the q4/q14 fixtures exercised.

After the pass every role and unit was stopped and uninstalled. Both hosts
have no engine, role or GPU compute process, no rendezvous directory, no
`~/.local/state/mllm`, `~/.config/mllm` or `~/mllm-rc3-*`; the server state
stays on control-host under `~/mllm-rc3-server` (invitation files removed).

## Release candidate 0.1.0-rc.2 — live validation, 2026-09-24 (branch `fix/rc2-live-findings`)

PR #10 (soak fixes and harness) and PR #11 (version skew, ADR 0017) merged,
then PR #12 bumped the version. Draft release `v0.1.0-rc.2` (pre-release,
unpublished) targets `main` at 45f91af: `mllm-0.1.0-rc.2-linux-x86_64.tar.gz`
(control-host, sha256 `e99f57d9…d0c`), `mllm-0.1.0-rc.2-linux-aarch64.tar.gz` (built
natively on host-a, nice 19, sha256 `f7a5d598…bc1`), `install.sh`
(`ba339733…404a`) and `SHA256SUMS`. Both passed `scripts/verify-packaging.sh` on
their own architecture; `BUILDINFO` commit 45f91af, not dirty, runtime manifest
`80044870…ddee0`.

Live, with release binaries only: `install.sh` from a `file://` mirror of the
draft assets on control-host (`--systemd server`) and both hosts (`--systemd host`),
fresh state under `~/mllm-rc2-*`, host documents without `runtime_dir` (the
managed runtime the binary writes to `<state_dir>/runtime`), roles run by the
installed systemd user units (`~/.config/mllm/<role>.env` naming the document).
No repository build or synced tree on the hosts; the harness ran from its
scripts only, through the new `MLLM_LOCAL_BIN`, `MLLM_REMOTE_BIN`,
`MLLM_*_RUN_ROOT` overrides and `gen_host_doc.py --managed-runtime`. Evidence:
`target/live/rc2/`.

| Row | Verdict |
|---|---|
| M75 (no engine, both hosts) | pass: `invalid_config` "no engine installation", exit 2, nothing left |
| M73 va-4, sa-4 | pass: both engines launched from the managed runtime (`~/mllm-rc2-host/host/runtime/vllm_entry.py`, `sglang_entry.py`), loopback-only, unkeyed engine calls 401, stop with verified cleanup, restart at a new binding |
| M08 | pass: router and host ingress serve no engine or control path (404), engines loopback only and refused from control-host, control routes 401 unkeyed, vLLM marked `exposed`/not production safe, SGLang not exposed |
| M29 va-4 (vLLM park/wake) | pass: 77% of the Ready drop released, same processes, wake on request |
| M28 sa-14 (SGLang park/wake) | pass on 2 of 3 runs (89% released). The first run's park was refused `park_refused` after the host's saver observation failed (`native_observation_failed` at `receive_header`); the engine kept serving (fail closed). Not reproduced; open |
| M31 vb-4 ↔ sb-4, tight host-b, 2 cycles deep | pass: switches park and wake the same processes, reservations settle |
| M64 (`delete --stop`, drain) | pass |
| TC sa-4 (`qwen25`), vb-4 (`hermes`) | pass: 4 of 4 each (vLLM named choice finishes `stop` with the tool call) |
| `systemctl --user restart mllm-host` | pass: same engine PIDs and start ticks, new agent PID, reconciled, serves |
| M45 revoke with va-4 Ready | pass: `engines: retained`, dispatch 503, reconnect refused, engine alive |
| Recovery (`invite host --recover`, `join host --recover`) | pass: same host id, same engine processes at generation 1 (re-proven, not relaunched), serves |
| Mixed version (host-b on rc.1, server rc.2) | `upgrade_required` with its reason; start refused; a Ready engine keeps serving; stop and drain work; reinstalling rc.2 gives `supported` and a start succeeds. Found bug 2 below |

Bugs found and fixed on `fix/rc2-live-findings` (CPU-verified with failing-first
regression tests; the fixes themselves have not run live):

1. **User units and systemd ≥ 254.** `~/.config/mllm` (where the user units read
   `<role>.env`) existed before the first start, so systemd 255 on the hosts
   made `~/.local/state/mllm` a compatibility symlink to it; the unit's state
   and `TMPDIR` landed in the configuration directory. `install.sh --systemd`
   (user scope) now creates an empty 0700 `~/.local/state/mllm` and warns about
   an existing link; `install.md` documents it; `scripts/test-install.sh`
   covers both.
2. **A park refused before sending closed dispatch for good.** On a drain-only
   host the park preflight refuses `host_upgrade_required`, the coordinator
   settles it leaving the remote launch's dispatch closed until a fresh probe
   reopens it, but the readiness proof was kept, so no probe was sent: vb-4
   stayed `reconciling` with dispatch closed (503) while its engine ran.
   `RemoteEngine::residency` now forgets the proof on every park refusal
   (host `unchanged`, preflight, gate refusal), so the supervisor re-probes.
   Test: `version_skew.rs` `a_park_refused_before_sending_forgets_readiness_so_a_probe_reopens_dispatch` (T16 T34).

Harness fix: `M27.sh` checked the tight policy on host-a regardless of the
fixture's host.

Local on `fix/rc2-live-findings`: core 1004, workspace all-targets 1767,
Clippy clean with warnings denied, `scripts/test-install.sh` passed. CPU and
Fake-engine tests are not qualification. After the run every role was stopped,
the units and binaries uninstalled, and both hosts left with no engine, role
or GPU compute process, no rendezvous directory and no `~/mllm-rc2-*` state;
the server state stays on control-host under `~/mllm-rc2-server`.

Observations, not changed: a start refused because the only allowed host is
drain-only reports `capacity_blocked` ("capacity is unavailable") although the
scheduler's diagnostic is `host_ineligible`; a revoked host agent keeps
retrying its session (a few refusals a minute) instead of exiting.

## Soak M48–M50 — 2026-09-24 (branch `test/soak-m48-m50`, stopped by the owner)

M48 is not passed: the owner stopped the soak after 119 walked steps, short of
the 200 the matrix asks for, and M50 was not run. Harness: `rows/M48.sh`,
`soak.py`, `rows/M49.sh` (see `scripts/live/matrix/README.md`). Seed 20260924.
Deployments: va-4, sa-14, sb-4, vb-4 and vb-14 (every engine launched with
its tool parser) and the two-instance replica route `qwen3-4b` (sa-4-rep);
host-b on the tight policy, so two of its three single-instance deployments
fit and a request for the third switches.

- Segment 1 (commit `323e80d` binary, runs `M48` and `M48-r69`): 84 steps. Two
  invariant hits, both harness false positives (a stopped replica instance was
  credited with the binding of its sibling placed on the same host). It found
  two product defects, both fixed with regression tests and a failing-first check:
  1. `081e849`: a first placement whose checkpoint did not measure to the
     declared `content_fingerprint` failed as "runtime ownership is uncertain";
     it is now refused `checkpoint_mismatch` before any effect, as a wake is.
     (Found because `e0.sh checkpoints` fed its payload digest, which is not the
     product's manifest digest, to fixtures; the harness no longer does.)
  2. `227feb9`: a request for an operator-stopped deployment made room by
     switching before the operator-stop refusal, so on the tight host it parked
     a Ready incumbent and was then refused 429 (steps 77 and 80). The refusal
     now runs before any switch round.
- M49 on segment 1: every deployment deleted with verified cleanup and no residue
  by id, empty ledger, both hosts clean, MemAvailable within 0.5 GiB of the
  pre-soak baseline, roles exited 0. Before that, a server and both host roles
  were SIGTERMed and restarted with engines retained: they re-attached, and the
  invariant check and I1 passed on the re-attached engines.
- Segment 2 (commit `3585adf` binary, runs `M48-final` and `M48-final-r9`):
  35 steps, no invariant violation, then stopped by the owner mid-step. M49 on
  it: same clean outcome (MemAvailable within 0.1 GiB, roles exit 0).
- Coverage over both segments: routed inference and streams, tool calls, operator
  start with and without `--evict`, stop, park, wake on request, request-driven
  switching on the tight host, count-only revisions, instance stop and start,
  delete `--stop` and redeploy, drain host, host agent SIGTERM and restart,
  engine SIGKILL and agent SIGSTOP/SIGCONT. Refusals seen and judged expected:
  requests for operator-stopped deployments (429; its code is
  `insufficient_resources`, which reads oddly for an operator stop), and an
  activation that does not fit the tight host without `--evict`.
- Observations, not changed: a second replica instance that cannot be placed
  without eviction waits `queued` until its deadline (about 15 minutes), also
  after `start --evict --wait` returned; vLLM q4 greedy output flips a near tie
  at token 10 once the probe prompt is prefix-cached, so the soak compares an
  8-token I1 prefix.
- Local: core 982 reported, workspace 1983 reported, Clippy clean with warnings
  denied. CPU and Fake-engine tests are not qualification.

Remaining: M48 needs a full ≥200-step walk on the final binary, then M49 and
M50 (M73 on both engines and M08). Both hosts were left with no engine, role
or GPU compute process and no rendezvous directory.

## Distribution: one binary, GitHub Releases, install.sh — 2026-09-24 (branch `feat/distribution`)

Owner decision 2026-09-24: one self-contained binary, GitHub Releases and
`install.sh`; Homebrew deferred. Verified locally only (CPU tests and a
file:// installer fixture; not qualification of any engine recipe).

1. The runtime helpers are embedded. `crates/mllm-agent/build.rs` compiles
   every `runtime/*.py` (not `runtime/tests`) into the binary with a SHA-256
   manifest; `embedded_runtime::materialize` writes them to the managed
   `<state_dir>/runtime` (0700, files 0600, marker `.mllm-managed-runtime`,
   staged and renamed into place). `mllm init host`, `mllm start host`
   (document without `runtime_dir`) and `mllm start standalone` (no
   `MLLM_RUNTIME_DIR`) materialize it; a different manifest refreshes it, a
   changed managed tree is restored with a warning, an unmarked directory is
   refused, and a declared `runtime_dir` / `MLLM_RUNTIME_DIR` is never written.
   The server has no runtime (it launches no engine). Standalone no longer
   falls back to the checkout's `runtime/`.
2. Releases. The workspace version is `0.1.0-rc.1`. `packaging/release.sh`
   ships `bin/mllm`, units and docs (no `runtime/`), records the runtime
   manifest in `BUILDINFO`, copies `install.sh` and writes the release
   `SHA256SUMS`; `--sums DIR` rewrites it after gathering both architectures.
   The units run `/usr/local/bin/mllm` (user: `~/.local/bin/mllm`) and the
   standalone units no longer set `MLLM_RUNTIME_DIR`.
3. `packaging/install.sh` (POSIX sh, shellcheck-clean): gh, GitHub API with
   `GITHUB_TOKEN`, public URL or `MLLM_INSTALL_BASE_URL`; verifies the tarball
   against `SHA256SUMS` and every file against the archive's own sums, refuses
   on mismatch; `--system`, `--systemd <role>` (installed, never enabled),
   `--version`, `--uninstall`. `scripts/test-install.sh` exercises it under
   sh, dash and `bash --posix`; `scripts/verify-packaging.sh` runs it against
   the built tarball.

Draft release `v0.1.0-rc.1` (pre-release, unpublished; owner reviews before
publishing) was rebuilt after PR #8 merged and targets `main` at 6d7bf36:
`mllm-0.1.0-rc.1-linux-x86_64.tar.gz` (built on control-host, sha256
`606f7702…dbc2`), `mllm-0.1.0-rc.1-linux-aarch64.tar.gz` (built natively on
host-a in `~/mllm-release-build`, nice 19, sha256 `26dda1d9…5783`),
`install.sh` (`e09a327f…e43b`) and `SHA256SUMS`. Both passed
`scripts/verify-packaging.sh` on their own architecture; both binaries carry
runtime manifest `80044870…ddee0` and `BUILDINFO` commit 6d7bf36, not dirty.
The API and `gh` download paths of `install.sh` resolve published releases
only, so they work once the draft is published.

Not established: no release binary has run a role on a host, and the matrix
harness still declares `runtime_dir` (synced tree), so the managed runtime has
not launched a live engine yet.

## Model sources — 2026-09-24 (branch `feat/model-sources`)

Declared `huggingface` and `http` model sources are materialized by the host into
`<store>/sources/...` through the additive `MaterializeSource` action (ADR 0008
amendment 2026-09-24): pinned revisions and digests only, host opt-in
(`model_sources`, denied by default), a store reservation against `max_bytes`
before any byte is written, per-file verification, atomic commit, then the WE3
digest. Activation waits (`model_source_pending`) and status shows per-host state
and bytes. `mllm prune sources` reclaims unreferenced copies explicitly. Local
verification only, against a fake hub and origin: core 992 reported, workspace
1741, Clippy clean (schema v33, `ExecuteMember` field 14). Pending: a live Hugging Face download on a host; standalone
support; disk in the server's placement plan. CPU and fake-origin tests are not
qualification.

## Revoked host recovery — 2026-09-24 (branch `feat/host-recovery`)

Owner decision 2026-09-24: a revoked host recovers by re-enrolling under the
**same** identity (ADR 0016, amending SPEC §4.1). Implemented on
`feat/host-recovery`, rebased on `origin/main` `4decb9e` (after PR #3 host
revocation, PR #4 packaging and PR #5 schema downgrade guard; store v32 lands
after the guard).

- `mllm invite host <name|id> --recover --output FILE`: an explicit, single-use
  recovery invitation, 15 minutes by default (at most one hour), bound to the
  revoked host's id and journaled (`host_recovery_invited`). A host that is not
  revoked is refused `host_not_revoked` (409); an unknown host is `not_found`.
  An ordinary invitation for a revoked name is still refused.
- `mllm join host --join-file FILE --recover`: the host keeps its state and
  journal (or starts from fresh identity files if they were lost), always
  generates a new key, and gets a new certificate for the same host id
  (`host_recovered`). An ordinary join refuses a recovery invitation and
  `--recover` refuses an ordinary one. A retained identity for another host or
  controller is refused, never adopted.
- Revocation is now per certificate as well as per host (store v32): the old
  certificate stays refused after recovery; certificates of hosts revoked before
  v32 are carried in as revoked.
- On reconnect the existing reconciliation runs: a still-owned Ready engine is
  re-proven by a fresh probe against its recorded identities before dispatch
  reopens (not relaunched). A host that lost its journal re-proves nothing; its
  engines stay closed and charged until an operator stop settles them on gone
  evidence. For that, a Terminate now carries the server's recorded process
  identities (additive protocol field); a host with no record of the launch only
  observes and reports them, never signals them, so the launch is never released
  while one is alive.

Tests (CPU and Fake-engine only; not qualification): `mllm-cli`
`host_recovery` drives the real server and host binaries over mutual TLS (deploy
a fake engine, revoke, dispatch closed and reconnect refused, recovery of an
active host refused, recover by name, join `--recover`, same host id, engine
re-proven and served without relaunch, old certificate refused, invitation
single-use; and a lost-journal variant with an expired invitation refused,
dispatch closed and accounting retained while the engine runs, and an operator
stop issued while the host was away completing only on gone evidence by
identity). Also store, controller (real mTLS session), agent journal and
enrollment, protocol, management and grammar tests (T05 T06 T33 T34).
Local verification after the rebase on `4decb9e`: core 985 reported (984
distinct), workspace all-targets 1710, all passing; Clippy clean with warnings
denied. The
lost-journal test was shown to fail (stop never settles) with the recorded
identities removed from the Terminate. One early run hit a transient
`revoke host` request-journal refusal that did not recur in five later runs.
Pending: live rows M45 (revocation) and a live recovery row on the hosts.

## Schema downgrade guard and standalone `--config` — 2026-09-24 (branch `fix/schema-guard-standalone-config`)

Closes the two gaps the packaging guide found, verified locally only (CPU
tests; not qualification of any engine recipe).

1. An older binary now refuses state written by a newer one (SPEC §13.2, T33).
   `migrations::apply` returns `StoreError::FromNewerVersion { found, supported }`
   when the recorded schema version exceeds the binary's latest, before writing
   anything; the host journal returns `JournalError::FromNewerVersion` the same
   way. Roles report `store_from_newer_version` with a restore-backup or
   use-newer-binary hint and exit 5, which the units do not restart. Guard logic
   only: no schema version was added or renumbered.
2. `mllm start standalone --config <file>` is implemented (SPEC §15.2, R13). The
   explicit document is the one honoured; a missing or invalid one refuses with
   exit 2 and is never replaced by the generated or implicit document. The state
   root still comes from `MLLM_STATE_DIR`, a pristine root gets its credentials
   once, and a served root that lost them refuses. The packaged units keep
   starting without `--config`; `docs/operations/install.md` documents the
   precedence and the drop-in for an explicit document.

## Service packaging — 2026-09-24 (branch `feat/service-packaging`)

SPEC §4.3 service definitions and an F5-direction release tarball, verified
locally only. `packaging/systemd/{system,user}/` hold server, host and
standalone units: `Type=simple` foreground, `KillMode=process` on host and
standalone so engines survive a restart and are re-attached (server:
`mixed`), no draining `ExecStop=`, `TimeoutStopSec=90s` (drain_timeout + 60s),
`Restart=on-failure` except exit codes 2, 3 and 5, `OOMPolicy=continue`, and
engine-compatible hardening (no `PrivateTmp`, `PrivateDevices` or syscall
filter on host and standalone). `packaging/release.sh` builds a stripped,
reproducible tarball of git-tracked files with an owner-only `runtime/`;
`scripts/verify-packaging.sh` checks the unit invariants, runs
`systemd-analyze verify`, builds the tarball twice and checks its entries,
modes and digests. Operator guide: `docs/operations/install.md`.

Not established: no unit has run on a host. Whether the host unit's
hardening lets vLLM and SGLang start, park and wake, and whether engines
survive `systemctl restart mllm-host` and are re-attached, needs a live run.
Found while writing the guide: an older binary did not refuse a state store
migrated by a newer one, and `mllm start standalone --config` was refused as
not implemented; both are closed on `fix/schema-guard-standalone-config`.
Rollback across a schema change still needs a state backup.

## Post-merge live smoke — 2026-09-24 (branch `fix/live-smoke-2026-09-24`)

PR #1 (`edurdias/mllm`) merged into `main` as `eb33deb` after local
verification (no CI minutes available). A live smoke on both hosts then passed
M75, M73 on both engines, M08, vLLM and SGLang park and wake (M29, M28), a
cross-engine switch with warm processes (M31), M47, M53, M65, M66, M54, a sustained
frozen-agent run (M58: 2400 of 2400 requests, 5.4 s suspension, no replay), M64 and
M36, plus vLLM tool calls with a tool parser. It found two defects, both fixed on
`fix/live-smoke-2026-09-24` (from `origin/main`):

1. An engine's invalid-request rejection became a 500 and held an uncertain lease.
   The owner decided on 2026-09-24 that a complete engine response with status 400,
   413 or 422 and a JSON body is completion evidence, so the client receives the
   engine's status and message (`engine_rejected`) and the lease closes, while every
   other status stays uncertain (SPEC §10 note).
2. SGLang tool calls failed at the router with a 500, streaming or not (the adapter
   always streams from the engine). The cause was not the tool-call index: SGLang
   0.5.20 serializes each tool-call delta through pydantic without dropping unset
   fields, so the delta carries `"role": null`, which the strict delta check read
   as a role other than `assistant`. Read-only inspection of the 0.5.20 source on
   host-a (`serving_chat._process_tool_call_stream`, `ToolCallItem.tool_index:
   int`) shows the index is always an integer, so index validation stays strict. A
   null role is now an absent role; a non-null role other than `assistant` is still
   uncertain, and a chunk's `"usage": null` no longer overwrites collected usage. The
   regression test replays SGLang's exact bytes (T19).

Tool calls need the engine's own tool parser, passed through
`engine_config.extra_args` with `accept_extra_args: true` (vLLM
`--enable-auto-tool-choice --tool-call-parser hermes`, SGLang
`--tool-call-parser qwen25`); mllm relays, never parses (SPEC §10 note).

Harness: a row that fails or exits early now deletes what it deployed
(`cleanup_failed_row`, `KEEP_FAILED=1` keeps it), so a failed deploy no longer
leaves a route that makes the next row fail `route_conflict`. M64 judges cleanup
after `delete --stop` by deployment id. M53 now expects the failed target's
restart to be refused `startup_requires_empty_host` and checks ledger residue by
id. New rows: `TC` (tool calls, named and auto, streamed and not), `REJ` (engine
rejections), `M58` (sustained frozen agent); `M31` takes `SWITCH_MEMORY_JSON` for
the q4 pair; `M08` records tool-call behaviour without a parser (not gating).

Live after the fixes (2026-09-24, run `matrix-20260924T185326Z`): TC on SGLang
sa-4 with `qwen25` returned `get_weather` tool calls for named and auto, streamed
and not (4 of 4, finish `tool_calls`, well-formed SSE); TC on vLLM vb-4 with
`hermes` 4 of 4; REJ 40 of 40 rejections relayed 400 `engine_rejected`, the
streamed rejection an `engine_rejected` error event, no lease held, and the route
then served; M08 passed. The failure-cleanup trap was exercised with a scratch row.
Both hosts were left with no engine or role process and no GPU compute process.
Local: core 974 reported (973 distinct), workspace 1678 (one run had a load-timed
failure in `a_success_resets_the_attempt_budget`, 0 of 30 in isolation), Clippy
clean, runtime Python 276. CPU and Fake-engine tests are not qualification.

Open: the keyed vLLM admin-key probe was not run because the permission classifier
refused an agent reading engine keys from process environments, even with the
owner's relayed approval.

## Consolidated review round — 2026-09-24 (uncommitted)

The owner's single end-of-work review ran as four read-only reviewers (store;
controller and scheduler; agent, protocol, adapters and runtime with a security
focus; router, management, CLI, config and test hygiene), followed by fix agents
per area. Security: the sensitive-option gate now decides on the destination the
engine's own parser resolves (closing bind and path bypasses such as SGLang
`--decoupled-spec-bind` and vLLM `--master-ad`), code-loading options need host
approval, vLLM control traffic uses a separate admin key with no proxy or redirects
(remote and embedded), engines start from a closed environment with plugins off,
bytecode is neither written nor loaded, the whole runtime tree is integrity-checked,
chat bodies are allowlisted while tool calls, structured output, reasoning and
multimodal content still pass (SPEC §10), and secrets are redacted from debug
output. Controller: an unproven cleanup pauses only its own binding and retries when
the host returns instead of halting every lane; leaked lease grants close; uncertain
leases no longer block switching; stale results no longer end host sessions; pauses
apply immediately. Store: one instance's failure closes only that instance; parks
and restores never reopen a gate another reason closed; deferred stops stop ready
siblings at once; embedded cleanup never releases on empty evidence; switch
closures are cleared; checkpoint digests are accepted only from resolved hosts.
Router and management: uncertain ends no longer leak in-memory slots; waiting
requests are served first-in first-out within their deadline; the configured body
bound applies; the events stream projects every event kind; replays cannot undo an
operator stop; `start --evict` validates before evicting; a request deadline under
30 s is refused. Drains record their intent before any stop (store schema v30), and
a legacy generated `standalone.yaml` with the old `tls` block starts with a warning
instead of being refused. Test roles can no longer be orphaned and fixed-port
collisions are gone. Latest local verification: core 972, workspace 1671 and
Clippy pass on CPU and fake engines; not yet re-run live.

## Embedded vLLM separate admin key — 2026-09-24 (uncommitted)

SPEC §9.1 / T21, ADR 0012. Embedded (standalone) vLLM now uses a separate admin key,
as the remote path already does. `ProfileBindings` issues a fresh admin key beside the
inference key (`AdapterSpec::Vllm.admin_key`). The resolved-spawn factory seals both
roles before the builder runs and refuses the launch if the two keys are equal. The
engine gets it as `MLLM_VLLM_ADMIN_KEY`. `runtime/mllm_vllm_guard.py` then admits the
inference key only on `/v1`-family paths and `/metrics`. The adapter presents the admin
key on `/sleep`, `/wake_up`, `/is_sleeping`, `/collective_rpc` and `/reset_prefix_cache`.
Ingress and the router still read only the inference role. The standalone host policy
now names `admin_credential_ref: secret://admin-key` for vLLM too. This changes the
standalone vLLM recipe fingerprint.

Migration: a launch recorded before this change sealed one key. A restarted coordinator
adopts it with that key only (`local_adoption`) and never mints an admin key for a
running engine. That engine keeps the single-key guard until it next launches. Every
new launch seals both roles.

Tests (T21 T37): the spec carries distinct fresh keys, an embedded start seals both
roles and the runtime endpoint carries only the inference key, adoption with and without
a recorded admin key, and HTTP key routing against a keyed-guard mock (with and without
an admin key). The standalone profile names both references. Core, workspace and
clippy logs are `target/orch-logs/key-*.log`. These are CPU and Fake-engine tests only,
not native qualification: no host run has exercised the two-key embedded guard.

## Status reasons, solo-first-start switching and launch-failure reasons — 2026-09-24 (uncommitted)

Three local fixes from the M53/M53D/M66 live findings. They were not run on the hosts.

1. SPEC §6.4. Status now shows `latest_operation {id, kind, state, error_code, reason, hint}`
   for each deployment and each instance, and `error_code` on each `operations[]` entry.
   All of these fields are additive. The reason is the latest journal evidence for an
   operation that did not succeed. It is cut to one line of at most 512 bytes, with no
   engine log tail, and it is withheld if it might quote a credential
   (`mllm_domain::diagnostics`). An error code is shown only when it is a closed code.
   The hint is fixed text for the closed category. `deploy --wait`, and the new
   `start deployment|instance --wait`, print the reason and hint when they fail.
2. When a switch target needs a solo first start (a whole-host startup footprint),
   the plan now releases every other charge on the host up front. Those victims stop
   instead of parking (`accept_switch_release(.., may_park)`). Live M53 showed the old
   order: the victim parked, the first arm failed on insufficient resources, the start
   sat out a 30 s retry cooldown, and only then was the parked residual reclaimed.
3. An engine that exits before readiness is now a launch failure, not ownership
   uncertainty. The host sends `MemberExecutionResult.launch_failure`, a new proto field
   (13). It is one printable line of at most 256 bytes: the exit code or signal, and the
   names of any options the engine refused, never their values. The controller maps a
   result that reports launched, not usable, all processes gone to
   `RuntimeError::LaunchFailed`. The launch is still released only on the host's
   verified gone evidence.

Harness: `rows/M53.sh` (the recheck extension), `M53D.sh`, `M66.sh` (a long count prompt,
checked with `residue_check` after the delete) and `SGLMO.sh` come from the session
scratchpad. `run_row.sh` gains `SCRATCH_ROWS`. `residue_check` ignores the deleted id's
tombstone; the earlier inline check wrongly failed M53D and M66 on it.
The core, workspace and clippy logs are `target/orch-logs/fix8-*.log`.
These are CPU and Fake-engine tests only, not native qualification.

## SGLang 0.5.20 standalone product gate passed — 2026-09-21

The final objective is a full multi-node test: control-host controls host-a and
host-b. Standalone on host-a is the first gate, not completion. The
server/agent transport and distributed group support must be verified before
claiming multi-node success; independent SSH launches do not meet that goal.

The owner explicitly authorized a clean SGLang 0.5.20 installation on host-a,
then required fixes in product code and validation through the shipped CLI.
This authorization permits the SGLang environment migration; drivers, reboots,
and unrelated environments remain out of scope.

The clean private environment is `~/mllm-sglang-0.5.20-venv`. Its installed
SGLang source matches release commit `94602c9c2b7cbdb8efd5c52802dac6a1c180089e`;
the runtime now checks 86 source files, including the new argument groups.
CUDA allocation passed with PyTorch 2.13.0 / CUDA 13.0. The package checker
reports a cuSPARSELt wheel metadata incompatibility (`manylinux2014_sbsa`
inside the aarch64 wheel); its ELF architecture is AArch64. No installed
package code or metadata was patched. Old helper directories were moved to
`~/mllm-archive/sglang-before-0.5.20/`; the old environment is now archived there too. Restore it to its original
`~/mllm-sglang-f2-venv` path before attempting rollback.

Uncommitted product fixes adapt ServerArgs resolution to 0.5.20, share the
LaunchSpec type across script/import execution, publish the observed host name,
and preserve physical GPU UUIDs when applying persisted resource controls.
The owner requested an explicit `start standalone --debug-engine-logs` flag;
it retains full private native logs, may include secrets, and keeps raw logs
out of management errors. Without that flag raw native output is suppressed.
The UUID regression reproduced loss before the fix and passes afterward.
The CLI now connects deploy/start/stop/status to the authenticated management
API, with a separate loopback listener and admin credential. A binary-level
Fake-engine test passes; that is not native qualification.

Live validation uses `target/release/mllm start standalone` and
`mllm deploy model --file ... --activate --wait` on host-a. Evidence and
private application state are under `~/mllm-runs/sglang-0.5.20-product/`.
The first product deployment exposed lost GPU identity and failed closed at
placement. The second (`qwen3-4b-v2`) passed placement and loaded weights, but its first
inference failed because the guarded environment omitted the venv tool path:
FlashInfer could not execute `ninja`. The launcher now builds PATH from the
selected interpreter's bin directory plus fixed system directories. It never
inherits the caller's shell PATH. Explicit retry of a verified-clean failed
launch is also fixed; it creates fresh operation and binding identities while
preserving every retained-state guard. The corrected product passed twice:
with debug logging and with the default suppressed-output mode. Each run reached
Ready with dispatch enabled and answered authenticated inference through
`127.0.0.1:8443` with HTTP 200 (`2 + 2 → 4`, then `3 + 4 → 7`).
Each CLI stop settled stopped with admission and dispatch disabled. Both owned
process groups disappeared and nvidia-smi showed no compute processes.
After the default-mode stop, MemAvailable was 123,863,424 kB.
The final deployment generation is 3; no park/wake or multi-node success is claimed.
SQLite was queried read-only for diagnosis; no state rows were edited.

Checklist:
- [x] Clean 0.5.20 environment; preserve rollback and previous failure evidence.
- [x] Native Ready, authenticated routed inference, CLI stop, owned-group cleanup.
- [x] Default-off full debug log flag, exercised in both modes through the binary.
- [x] Complete final Rust integration check (635 distinct core tests).
- [ ] Complete consolidated code review.
- [ ] Implement server/agent enrollment, transport, reconciliation and remote lifecycle.
- [ ] Implement and validate distributed group launch/accounting on both hosts.
- [ ] Pass the full multi-node gate; standalone success is only its prerequisite.

Non-secret live evidence is copied to `target/live/sglang-0.5.20-product/`.
Full debug logs remain in private files on host-a and were not copied into
the repository or management journal.
Core verification after the retry fix passes 635 distinct tests (636 reported,
including the owned-state child summary). A subsequent tool-path change passes
the full SGLang Initialize target (10 tests). Core Clippy and targeted CLI Clippy
pass with warnings denied. The complete runtime suite now passes 259/259,
including the additional debug argument mapping check. Its pinned saver source
fixture is supplied through `TMS_SOURCE_ARCHIVE`; these are CPU checks.
The native start/inference/stop gate passed. This does not qualify deep park,
wake, switching, or distributed operation.

Read-only multi-node preparation confirms both hosts are reachable and use
aarch64 unified-memory host / driver 580.173.02. host-b has the required checkpoint but no
SGLang environment was listed. Product `start server`, `start host`, enrollment and authenticated AgentControl
sessions now pass local binary tests. Remote engine execution and native
two-host validation remain pending.

Implementation follows
`docs/plans/2026-09-21-1831-feat-two-host-sglang-plan.md`.
Host-scoped ownership, typed command contracts and the additive namespace
migration are implemented. U1 focused domain/protocol/store checks pass 177
tests; the integrated core run passes 639 distinct tests (640 reported,
including the owned-state child summary), and core Clippy passes with warnings
denied. These CPU/Fake checks do not qualify native multi-node operation.
U2 enrollment, U3 durable execution and U4 remote roles/sessions are implemented.
U5 remote lifecycle and private ingress are in progress, followed by group launch.
The owner reaffirmed on 2026-09-21 that the acceptance target is the full two-node
run, not standalone. The registry-backed configuration test now verifies host
selection, disjoint resource identities, durable original configuration, and exact
request replay. Private ingress tests cover generation fencing, forwarded header
restrictions, streaming request accounting, and persistent separate credentials.
A frozen ingress binding rejects endpoint reassignment and missing remote authority.
The integrated core run now passes 651 distinct tests (652 reported, excluding
the nested owned-state summary); core, agent, config and CLI Clippy pass with
warnings denied. These are local tests, not native qualification. The owner's
Tailscale SSH reauthentication completed on 2026-09-22; read-only SSH checks then
succeeded on both hosts with no GPU compute process on either. The native remote
gate on host-a is therefore unblocked but not yet run: no source sync, host build,
server/host deployment or remote native launch has happened since. host-b
had no known matching SGLang 0.5.20 environment; on 2026-09-22 the owner granted
a scoped exception to create a clean SGLang 0.5.20 virtual environment on host-b
mirroring host-a (same source commit, same wheels), with no driver, system package or
reboot changes and existing vLLM environments left untouched. The owner also
directed that the old host-a standalone service, if still alive, be stopped through
the shipped CLI and that work proceed until blocked or a live milestone is proven.
Later on 2026-09-22 both hosts were synchronized to the current worktree and built
`target/release/mllm` under `~/mllm-f2` (builds over non-interactive SSH need
`~/.local/bin` on PATH for `protoc`). The host-b environment now matches host-a
byte-for-byte (206 PyPI wheels, identical RECORD digests, same `uv pip check`
cuSPARSELt metadata complaint); an import and CUDA smoke passed without loading a
model, which is parity, not qualification. The old host-a standalone service (PID
158346, ports 7443/8443) had no running engine; the CLI has no `stop standalone`
verb and the standalone role installs no SIGTERM handler, so it was ended with
SIGTERM. That missing graceful stop is an open product gap. Its state directory
still records deployment `qwen3-4b` with desired `ready` and observed `stopped`.

Owner direction on 2026-09-22 changes the sequence. Multi-node work proceeds first
with single-rank recipes: one control-host control plane managing both hosts, each host
running multiple vLLM and SGLang single-rank deployments, serving, switching models
and parking as needed. The end goal is a two-host control plane supporting both
engines in all meaningful permutations, mapped by an explicit test matrix. Two-rank
(TP2) group work, previously U6/U7, is deferred until after that matrix passes; its
open design questions (residency, peer exposure, NCCL transport, rank readiness,
compensation, owner granularity, placement shape, rendezvous ports) are parked.
The test matrix is `docs/plans/2026-09-22-two-host-engine-matrix.md`
(scenarios M01–M72, gaps G01–G17 plus U5-G1…G4, decisions D1–D11) and the work
plan is `docs/plans/2026-09-22-two-host-control-plane-plan.md` (units
W0–W13 in waves). Decision E1 (below) supersedes the plan's per-model recipe
approach for G17. Owner decisions so far:
D1 deep parking is enabled by default and a host opts out (the SPEC §9.1/T21
text that says opt-in is to be amended to match); D2 remote vLLM is built now, in
parallel; D3 park/wake is built once through the coordinator with remote
Park/Restore actions; D4 automatic request-driven switching is required now;
D5 the model set is qwen3.8-27b (NVFP4 build as the catalog anchor), qwen3-30b-a3b,
qwen3-14b and qwen3-4b-instruct, mirrored from host-a to host-b with SHA-256
verification and no internet download; D6 fault injection may use signals on
mllm-owned processes and one bounded external memory allocation, never firewall,
interface or reboot changes; D7 "host restart" means restarting the host agent
process, and reboot recovery stays untested; D8 a server restart must fully
re-attach live remote engines through fresh probes, keeping failures charged and
closed; D9 one route may be served by replicas on both hosts, with the router
load-balancing on its own in-flight counts combined with engine metrics that
each host agent scrapes on loopback and reports, and failing over on host loss;
D10 the normal per-host budget is an 80% managed limit with a 10% free reserve,
and a tight budget admits exactly one of the two largest models; D11 undeploy and
graceful role shutdown (server, host and standalone SIGTERM handling) are built
in this phase. The request path stays layered as the owner stated it: controller
(router) to host agent to engines; clients never reach an agent or engine directly.
E1 (same day): every engine must be able to serve any model. The host declares its
engine runtimes; the deployment chooses the model, the runtime and its parameters.
Deployments carry typed common parameters (dtype, quantization, KV-cache dtype,
context, concurrency and similar) plus ordinary engine arguments that pass through
per SPEC §8.2 and §13.3 operator policy, behind an explicit flag that accepts extra
parameters. Settings mllm owns (device, ports, bind addresses, memory grants, keys,
ranks) are always reserved and can never be overridden. Security-sensitive options
(remote code, plugin or code paths, extra listeners) need host-policy approval.
Checkpoint identity is a digest recorded at deploy time and re-verified at launch,
replacing pre-pinned per-model hashes. The single-checkpoint SGLang recipe pin and
the narrow vLLM flag list are over-restrictions to remove. This matches accepted
ADR 0008 (a deployment owns its `engine_config` from its engine family's schema)
and ADR 0011 (mllm validates a recipe's shape and capacity; whether it works is the
user's responsibility).

P1 (same day, after reviewing SPEC §2, §3, §6, §10 and ADR 0008 with the owner):
load-balanced replicas live inside one deployment. A deployment declares a count
of instances (one instance is one engine group) plus optional placement constraints
(allowed hosts or selector, spread or pack, maximum per host); instances may share
a host when capacity allows. The server's scheduler places instances from live
capacity and reservations at activation, records each placement durably, and the
router balances across that deployment's ready instances. Pinning an instance to a
host remains possible through the selector; changing the count is a revision of the
deployment. This replaces the plan's separate replica-route design (W7/W9).
P2: each deployment declares its per-instance memory request; when omitted, mllm
derives it from checkpoint size, requested KV and a per-engine overhead margin.
The first live run of each model measures actual peak use, and matrix budgets are
recomputed from those measurements. Reservations always use the declared or
derived request. P3: stopping or signalling any role, standalone included, is a
service restart: admission closes, in-flight streams finish or cancel within a
bound, engines stay running and owned, and the next start re-attaches them through
the fresh-probe path. A separate explicit drain (`drain host`, `stop standalone
--drain`) stops every engine with verified cleanup and leaves deployments eligible
for on-demand activation. P4: vLLM development-mode exposure under default-on deep
parking is accepted for this phase with the mandatory mitigations, provided
status and inspect mark every deployment and host profile that exposes those
controls, and park rows run only after the security row (M08) passes live.

Designs: ADR 0013 (deployment instances and placement) and ADR 0014 (deployment
engine configuration, E1 and P2) are written; the plan is revised into waves.
Owner answers to their open questions on 2026-09-22: Q5 on-demand activation
starts one instance, and the rest start only where they fit without eviction,
while explicit `start deployment` brings up all instances; Q6 mixed-engine
load balancing is tested as two deployments on two routes, since one deployment
names one engine installation; Q7 per-instance `stop instance` and `start
instance` verbs are added now (this amends ADR 0013, which proposed none); Q8 a
non-count revision stops all instances and restarts on the new revision, with
rolling replacement designed later; Q9 the checkpoint is fully hashed on first
placement on a host and whenever any file's size, mtime or inode changes, and each
launch or wake re-checks that metadata and rehashes the small files; Q10 ordinary
extra engine arguments are allowed unless the host denies them, and
security-sensitive options always need named host approval; Q11 vLLM launches
through a new `runtime/vllm_entry.py` wrapper that runs vLLM's own parser and
refuses reserved fields however they are spelled or supplied.

W4 landed locally: the host agent executes Park and Restore. vLLM parks with
`sleep?level=2` and restores with weight wake, `reload_weights`, KV wake and a
prefix-cache reset, followed by a fresh model probe before the gate reopens;
quiescence requires zero in-flight ingress work and zero running and waiting
engine gauges. An engine failure after dispatch leaves the launch uncertain and
quarantined until Terminate. SGLang park is wired through the adapter but refused
with no effect until a memory-saver observation source exists in production.
CPU and fake-engine tests only; no native engine has parked.

W11 landed locally. Stopping a role (server, host or standalone) is a signal, not a
command: new inference gets 503 `shutting_down`, admitted requests and streams get
up to the drain bound (default 30 s; now `shutdown.drain_timeout` in role
configuration, see below), engines are left running and owned,
and the next start re-attaches them. Standalone gained a SIGTERM handler and its own
re-attach path: a restarted standalone adopts its Ready embedded launches and
reopens dispatch only after matching process identity and an authenticated model
check. The explicit drain commands are `mllm drain host <name|id>` and, confirmed by
the owner, `mllm drain standalone`; both issue ordinary stops with verified cleanup
and leave deployments eligible for on-demand activation. Core suite 686 passing and
Clippy clean; fake-engine role tests only, not live. Known limits: standalone adopts
only Ready launches (not uncertain or parked ones); adoption on any role is refused
while request leases from the dead session remain, which a crash with requests in
flight leaves behind; the drain bound is environment-only; draining an offline host
waits for its deadline.

W8 landed locally: each host agent scrapes open, handle-bound engines'
`/metrics` on loopback with the per-launch key every second (vLLM running,
waiting and KV-usage gauges; SGLang equivalents once WE2 enables its metrics) and
sends bounded `ReportLoad` frames; the controller keeps the latest sample per
deployment generation, stale after 3 s and dropped when the host session ends, for
the router's instance selection (I3). Metrics stay unreachable through ingress.
Fake-engine and mTLS session tests only, not live.

W14 landed locally: deployment status, inspect and list views, and the host
inventory, carry a derived `development_controls` field marking every vLLM launch
with deep parking on and a parking residency as `exposed`, listing the reachable
surface and the mitigations and stating `production_safe: false`; text output
prints a notice on stderr. Nothing in configuration can set or clear the mark, and
unreadable state reports `unknown`, never safe. It found that before WE1 a
`restart_only` vLLM deployment could still launch in development mode; WE1's
derived sleep mode fixes that and a drift test guards it. Per-instance marking
waits for I1.

WE1 landed locally (ADR 0014, first slice). A deployment now carries
`engine_config`: typed common fields (dtype, quantization, KV-cache dtype, context
length, concurrency, CUDA graphs, language-model-only, trust-remote-code), a memory
request and KV size, per-family fields, and `extra_args` behind
`accept_extra_args`. Host profiles keep only host-fixed `args`; host `security`
gains `extra_args` (allowed by default), `approved_options` and `approved_paths`.
Reserved options are refused however spelled (abbreviation, negation, `=value`,
dotted keys, `--config`), and security-sensitive options need named host approval.
The memory request is declared or derived as weights plus KV plus a placeholder
8 GiB per-engine margin. vLLM sleep mode is now derived from deep parking and a
parking residency. Rendering of the new fields is WE2: until then SGLang stays on
its interim single pinned recipe and vLLM refuses typed fields it cannot yet render.
Core suite 708 reported (707 distinct), agent/config/protocol/domain/testkit 230,
CLI explicit targets and Clippy all pass; CPU and fake-engine only. WE1 changed the
stored format, so state written before it fails to load. The owner decided on
2026-09-22 that upgrades must migrate old state forward: pre-E1 effective
revisions and host documents are rewritten into the new shape, only unmappable
records are refused, and accounting for anything running is kept.

W12 landed locally. Host `eligible` is derived: online and reconciled, an accepted
approved configuration, and at least one reported profile matching an approved
profile's build fingerprint; a reported qualification grants nothing (ADR 0011).
Adoption after a crash now carries request leases on the same fence; dispatch
stays closed until leases from the dead session are closed on evidence (a fresh
probe plus a later quiescence observation from the same process group), never on
a timer. A restart also adopts a Stop the dead session left planned or uncertain
and completes it on gone evidence. `join host` accepts a relative `--join-file`.
Core suite 715 passing; Clippy clean. W12 found that production routers never write
`request_leases` (only tests call `grant_dispatch`), so durable in-flight accounting
does not exist in practice yet. The owner decided on 2026-09-22 that the router
writes a durable lease per dispatch and closes it on completion or cancellation
acknowledgement, with batched bounded writes, as SPEC §10 accounting requires.

Phase B passed live on 2026-09-22 with one server on control-host controlling both hosts
(evidence `target/live/phase-b/`, all rows on a pre-WE1 source snapshot, qwen3-4b
only). SGLang ran natively on host-b for the first time (Ready in 127 s; answer, stream,
stop and verified cleanup). vLLM 0.29.0 ran remotely on host-a for the first time
(Ready in 29 s), and the development-control security check held: nothing
reachable from control-host or through ingress, every engine path keyed on loopback except
unkeyed `/health`, all engine sockets on loopback, and status marking the
deployment `exposed`. Both hosts then served concurrently with each route answered
by its own engine, and stopping one did not disturb 40 requests and a stream on
the other. Server SIGTERM during a stream finished the stream, answered new
requests 503 `shutting_down`, and re-attached the same engine after restart;
`drain host` stopped the engine with verified cleanup, and the next request
reactivated it on demand. Host agent SIGTERM kept the stream and engine and
re-attached, but new requests during the host's drain got 500 instead of 503
(open). vLLM on host-b was skipped: its `~/mllm-vllm-venv2` differs from host-a's
(`hf-transfer` extra, `jiter` 0.16.0 vs 0.17.0, a differently built
`instanttensor`), and no exception covers changing it. Three product bugs were fixed
with regression tests: admission compared the whole ledger against one host's
limits, so any charge on the other host blocked admission; a start accepted but
never armed before a server restart was orphaned and blocked all later commands;
and an operator start did not lift an earlier explicit stop, so drain looked like
an explicit stop. Open: host drain should suspend dispatch before closing ingress;
the same whole-ledger check remains in the policy-update overcommit and park/switch
admission paths, and `max_parked` counts across hosts; endpoint port leases are
global rather than per host; status shows `stopped` while a start is queued.

The pre-E1 state migration landed locally as store schema v19. It rewrites stored
effective revisions, retained sources, receipts and host publications into the
`engine_config` shape through WE1's own resolver, keeps binding identity on the
recorded legacy fingerprint so running launches stay recognised, and refuses
unmappable revisions with their bytes, bindings and reservations retained, a journal
entry, and an `operator_action` in status. Retained pre-E1 host journal commands
still decode, and Probe, Park and Restore of such a launch resolve against today's
approved document without re-signing. CPU tests only.

The owner granted a second scoped exception on 2026-09-22: create a separate
`~/mllm-vllm-0.29-venv` on host-b byte-identical to host-a's `~/mllm-vllm-venv2`, leaving
host-b's existing vLLM environments untouched and changing no driver or system
package. It was created the same day: the freeze (196 packages) and every
site-packages file match host-a by SHA-256, apart from venv-path shebangs and their
RECORD lines; the locally built `instanttensor` was copied as installed. `vllm
--version` reports 0.29.0 and CUDA imports work; no model was loaded.

WE2 landed locally. SGLang's single pinned recipe is gone: typed settings and extra
arguments flow into `ServerArgs`, only the reserved subset stays fixed,
`mem_fraction_static` is rendered from the memory grant, and `/metrics` is enabled
(loopback only; SGLang exempts it from its key). The SGLang entry re-parses extra
arguments with the installed `ServerArgs` parser and, after SGLang's own resolve,
refuses any change to a reserved field. vLLM now launches through
`runtime/vllm_entry.py`, which refuses `--config` and reserved fields however they
are spelled using the installed vLLM parser, then serves in-process; typed fields
render to their native flags. Every host runtime directory needs the new
`vllm_entry.py` before any vLLM launch. Until WE3 lands, SGLang launches verify no
checkpoint identity at all. Core suite 723 reported (722 distinct), runtime Python
281 and Clippy pass; CPU and fake engines only.

W2 landed: the live matrix harness in `scripts/live/matrix/` (snapshot, sync and
build; role bring-up with generated host documents and budgets; 20 deployment
fixtures across four models, both engines and both hosts; E0 evidence capture;
the I1 greedy-logprob identity probe; ownership-checked fault injection and a
bounded memory allocator; a load generator; and a row runner). It passed
shellcheck, syntax checks, config resolution through the real `mllm-config`
parsers, a fake-engine rehearsal and a dry run; nothing ran live. Gaps it found:
`mllm validate config` is still unimplemented; a `restart_only` SGLang launch was
reported refused, but that proved historical (see the policy-refusal paragraph
below); whether the
router forwards `logprobs` is unverified; the 8 GiB placeholder margin pushes the
4B and 14B fixtures past their declared requests until M16 measures real use.

Durable request leases and the Phase B fixes landed locally (store schema v21).
The router opens a durable lease before a request reaches the engine and closes it
on completion or proven non-acceptance; errors and timeouts leave it `uncertain`
and held, and streams hold it until the backend stream ends. A single group-commit
writer adds about 3.6 ms p50 and 8 ms p99 per dispatch under 32 concurrent
dispatchers (debug build). A host starting graceful shutdown now announces it; the
controller suspends that host's dispatch before its ingress closes, so new requests
get 503 `shutting_down` with `retryable: true`, and only the exact role-gate refusal
counts as not accepted. Policy-update overcommit and `max_parked` are scoped per
host. Endpoint port leases are keyed per host. Status derives `queued`, `starting`,
`stopping` and `reconciling` instead of reporting `stopped` or `ready`. Core suite
746 passing; CPU and fake engines only.

WE3 landed locally (store schema v20). A checkpoint's identity is the SHA-256 of a
sorted manifest of every file's path, size and hash, host-independent, computed
with a no-follow walk confined to the host's model store (in-store symlinks to
regular files only). Each accepted revision starts `pending`; the digest is
measured on the host by a new `DigestCheckpoint` action (or in-process for
standalone) and recorded before first launch, and a revision whose memory request
depends on weights stays provisional until then. Every launch and wake re-checks
file metadata and rehashes files up to 64 MiB, rehashing everything when any file
changes; a mismatch refuses launch, or leaves a parked launch parked. The old
pinned checkpoint manifest and preflight are removed, and runtime directories need
the new `runtime/pinned_file_observation.py`. Core suite 746 passing, runtime
Python 231; CPU and fake engines only. Open: a host refusing a launch for a digest
mismatch ends its session and the controller redelivers until the Initialize
deadline; a standalone model outside `MLLM_MODELS_ROOT` is now refused.

Policy refusals are now terminal answers (local, CPU and fake engines only). A
host that refuses a launch before any effect (checkpoint mismatch or unverified,
insufficient memory, residency tier, unauthorized) returns a typed refusal with a
closed reason instead of ending its session; the controller settles the launch at
once with that reason, and a refused Park or Restore answers `unchanged`. A
`restart_only` SGLang deployment resolves and renders without the memory saver,
and a Park of it is refused `unchanged`; the reported refusal of such launches came
from the pre-ADR 0014 launch shape. Standalone still forces SGLang to `deep`, so
`MLLM_DEEP_PARK=off` with SGLang was expected to be refused; it now falls back to
`restart_only` (`deployment_document` gained a `deep_park` argument). On
2026-09-22 the owner confirmed that `crates/mllm-cli/tests/live_interactive.rs` and
a local Task 2 implementation report, which
AGENTS.md had excluded as the owner's, are leftovers from earlier work. The
exclusion is removed, and they are deleted if no longer needed: validation goes
through the shipped product, not hardcoded scripts. `live_interactive.rs` was an
in-process vLLM park/wake lab that never ran the shipped binary; it is deleted,
and earlier mentions of it in this runbook are historical. The task-2 report,
whose unit committed long ago, moved to that slice's `archive/` directory. The
owner also decided that the remaining engine tests that bypass the shipped product
(`crates/mllm-cli/tests/live_vllm.rs`, `live_sglang.rs` and
`scripts/live/run-on-spark.sh`) become matrix rows driven through the product CLI
and roles, then are deleted; the temporary `repro_sglang_unarmed.rs` is already
deleted. That conversion is done: every scenario now maps to a product-driven
matrix row (M73 launch, inference, access control, stop and restart and memory
return for both engines; M38 empty model directory, recovery and an engine that
exits at once; M74 readiness deadline under `timeouts.initialize`; M75 standalone
refusing to boot without an engine, plus `check-release-clean.sh` in `sync.sh
build`), and the three files are deleted. None of the new rows has run live.

Owner decisions on open issues, 2026-09-22: (1) deployments gain
`timeouts.initialize` and `timeouts.wake`, defaulting to a value derived from
checkpoint size, with a per-command CLI override, replacing the fixed 900 s;
(2) the shutdown drain bound becomes a `shutdown.drain_timeout` field in server,
host and standalone configuration (default 30 s, at most 600 s), replacing
`MLLM_SHUTDOWN_DRAIN_SECS`; (3) SGLang's unauthenticated `/metrics` is accepted
because it is loopback-only read-only counters, and status marks it like the
development controls; (4) `drain host` on an offline host returns at once with
pending stops that complete with gone evidence on reconnect, `--wait` still
waits, and the host takes no new placements while a drain is pending; (5) waking a
launch parked before checkpoint digests existed first measures and records the
digest, then wakes. A Tailscale SSH re-authentication prompt briefly blocked live
runs; the owner cleared it the same day.

Fixes landed locally (CPU and fake engines only): `mllm validate config` validates
server, host, standalone and deployment files offline through the product's own
parsers, optionally resolving a deployment against a host document; non-stream
responses no longer drop `logprobs` (the request body was already forwarded
untouched); remote bindings are no longer test-bound on the controller; hosts
accept `load_report_interval` (250 ms to 5 s); the host agent and standalone refuse
to launch from a runtime directory or module that is a symlink, not owned by the
running user, or group/other-writable (`runtime_integrity`); `shutdown.drain_timeout`
replaces the environment variable; SGLang status marks the unauthenticated
loopback `/metrics`; and ADR 0014 is Accepted with SPEC §8.2 and §16.3 amended.
Known consequences: a checkout whose `runtime/*.py` files are group-writable (0664,
as on control-host) can no longer boot standalone from that checkout, and every
`docs/examples/*.yaml` file fails the product parsers because they are stale
sketches. The owner decided the runtime check should relax to owner-only: group
write is allowed when the group is the owning user's private group (umask 002
style), and remains refused otherwise.

Offline drain and legacy wake landed locally (store schema v24, `host_drains`). A
planned cleanup for an offline host is deferred rather than armed, so it no longer
halts the coordinator after the 30 s protocol timeout, and other hosts' cleanups
are not blocked behind it. `drain host` on an offline host returns at once with
`host_state: "offline"`, `stops: "pending"` and operation ids; `--wait` polls. A host
with a pending drain is excluded from eligible hosts. Waking a pre-digest parked
launch measures the digest first; a Restore now carries the recorded digest, and a
mismatch refuses without any engine call. Limit: a drain completes only if the
host reconnects within the Stop deadline (request time plus 900 s); after that the
Stop stays planned, the engine stays charged and the host stays ineligible. The
owner decided that on reconnect the server closes such expired, never-armed stops
as `expired` and issues fresh stops with new deadlines, keeping accounting until
gone evidence, so drain intent survives an outage of any length.

Live M16 (per-model smoke) started 2026-09-23 on both hosts. Its first run found a
product bug, fixed with a regression test: a host agent ended its control session
when a launch failed before readiness, so the controller waited the full Initialize
deadline and tore down the host's other effects; the failure is now a journaled
result. The engine error behind it was SGLang with no memory left for KV cache
under a 16 GiB request minus the 8 GiB placeholder margin, so harness requests rose
to 20 GiB (4B) and 42 GiB (14B). It also showed that a failed deployment's name
cannot be reused until undeploy exists (W6, now in progress). Further gaps from
the same run: a failed deployment cannot be stopped (`Lifecycle state does not
permit this action`); a memory request smaller than weights plus KV plus margin
still resolves and then fails inside the engine; and W8 load samples appear in no
CLI status view.

W6 undeploy landed locally. `mllm undeploy model <name|id>` is refused with 409
`undeploy_requires_cleanup` while any instance holds a runtime, reservation, lease,
open step or operation; once everything is released it removes routes, instances
and checkpoint digest rows in one transaction, turns the deployment into a
tombstone so the name can be redeployed under a new ID, keeps all history, and
never touches model files. The router answers 404 for the removed model at once.
Replays by request id return the original receipt. Core suite 795 passing; not
live. The owner decided on 2026-09-23 to rename the command `mllm delete
deployment <name|id>` (dropping `undeploy model`, with SPEC §6.3 and §14 amended)
and to add `--stop`, which stops every instance, waits for verified cleanup and
then deletes, durably and replayably, reporting `pending` if cleanup cannot yet be
proven.

Remote co-residence landed locally (host journal v4, store v25). An enrolled host
now holds one launch claim per instance incarnation and advertises
`launch_claims: per_launch`; before each launch it re-checks, against its own
approved policy, that the new launch fits beside its claimed launches (typed
refusals `insufficient_memory`, `device_conflict`, `port_conflict`), charging each
claim by its durable phase and charging an unresolvable retained claim the whole
budget. Readiness authority and gates are per launch. Different deployments now
co-reside on one remote host; a fake-engine end-to-end run served two deployments
from one enrolled host with independent stops. Core suite 798 passing; not live.
Open: two instances of the same deployment still cannot share a remote host,
because the host fences commands per deployment by generation, contrary to the P1
decision that instances may share a host; a host does not re-check wake growth
beside other claims.

I3 landed locally. The router balances each request across a deployment's
instances whose gates are open (remote ones also need a live host session). Score
= max(router in-flight, engine running + waiting) when a fresh matching W8 sample
exists, otherwise router in-flight, plus a penalty of up to 8 above 80% KV use;
ties rotate deterministically, and every choice is logged as a `router_selection`
line. The durable lease is fenced on the chosen instance's generation in the grant
transaction, closing the earlier lease-versus-forwarder race. Failover happens only
before the engine accepts the request (at most four attempts; streams before the
first byte); anything else stays uncertain and is never replayed. A binary test
with a real server and two host agents spread a burst across hosts, steered away
from a host reporting high load, did not replay a request whose host agent was
killed, and rejoined the adopted engine. Core suite 798; not live. Open: a frozen
(SIGSTOPped) host agent still accepts connections until its control session is
declared lost, so requests routed there in that window hang until the 300 s
forward timeout. The owner decided on 2026-09-23: server and agent exchange
heartbeats every second on the control session; after 5 s of silence the server
suspends dispatch to that host (accounting kept, nothing released), and after 30 s
it treats the session as lost; both values are server configuration.

Runtime-integrity relaxation and drain re-issue landed locally. The host agent's
runtime check now allows group write only when the group is verifiably the owning
user's private group (name, primary gid, no members, no other account using it),
refusing on any failed lookup; other write, symlinks and foreign owners stay
refused. Other group-write checks were not relaxed: the SGLang entry path check in
`crates/mllm-adapters/src/sglang/args.rs`, launcher ownership and observation
checks, and the runtime Python checks. An expired, provably never-sent drain stop
for a reconnected host is now closed as `expired` and re-issued with a fresh
deadline in one transaction, keeping binding, lease and reservation until gone
evidence. Core suite passing; CPU and fake engines only.

Owner decision on 2026-09-23, after reviewing which files the permission checks
guard: mllm's private state (identity, credentials, locks, observation sockets)
stays strict; mllm's own runtime helper scripts use the owner-only rule everywhere
(including the SGLang entry path check); and engine installation files get no
hard-coded hashes and no permission rule. Instead an installation's fingerprint
(version plus a digest of its files) is recorded at registration with drift
flagged later, and mllm's SGLang hooks probe the internals they need at launch
(API shape, not file hashes), refusing only the dependent feature, such as deep
parking, when a build lacks them. The pinned SGLang 0.5.20 source audit, which
refused any custom or patched SGLang build, is replaced accordingly (ADR 0008).

Deployment timeouts landed locally. `timeouts.initialize` and `timeouts.wake` sit
beside `request_deadline`, outside the recipe fingerprint. Derived placeholders:
initialize = min(120 s + 10 s per GB of weights, 1800 s) and wake = min(60 s + 5 s
per GB, 900 s), or 900 s while the digest is pending, never beyond the request
deadline. Effective configuration and status record values and provenance;
`--initialize-timeout` overrides per command; start and stop windows replace the
fixed CLI 900 s. `timeouts.wake` bounds nothing until a coordinator wake exists.

`mllm delete deployment <name|id> [--stop]` replaced `undeploy model` locally,
with SPEC §4.3, §6.3 and §14, ADR 0013, the plan and the matrix updated. `--stop`
issues an administrative stop (so on-demand activation cannot restart it before
deletion), waits for cleanup, then deletes; it is journaled in two steps and
resumable by request id, and returns `deleted:false, cleanup:"pending"` (exit 0)
when a host is offline or cleanup is not yet proven. Core suite 798; CLI 113.

Per-instance host fencing landed locally (host journal v5, store v26, additive
`CommandIdentity.instance_index`, capability `launch_claims: per_instance`). The
host fences each instance of a deployment against its own last assignment, so two
instances of one deployment now co-reside on one enrolled host; a fake end-to-end
run brought both to Ready, stopped them independently, and admitted an instance
restarted below its sibling's generation. Older hosts keep the same-deployment
refusal and now draw a fresh generation for a returning instance. The host also
re-checks a wake beside its other claims and refuses `insufficient_memory` with
the launch left parked. Core suite 800; not live.

Engine installation fingerprints and capability probes landed locally. The pinned
SGLang source audit and the saver source audit are deleted. `runtime/engine_capabilities.py`
probes by API shape (`core`, `deep_park`, `metrics`, and `observation` for SGLang)
against the real SGLang 0.5.20 and vLLM 0.29.0 layouts; a missing `deep_park` refuses
only `deep` launches and Park (`capability_missing:deep_park`, suggesting
`restart_only`), and a missing `core` refuses every launch. The host agent records
each installation's version and a file digest at start, re-measures at launch,
flags drift in status and the journal, and refuses `installation_drift` only when
the profile sets `security.installation_drift: refuse` (default `warn`). mllm's
helper scripts share one owner-only rule (Rust and a Python mirror); private state
stays strict. SPEC §8.1, §9.2, §13.3, T22 and T37 and ADRs 0008 and 0014 are updated.
Core suite 832; runtime Python 225; CPU and fake engines only. Remaining small
items: standalone records no installation fingerprint yet; `engine_capabilities.py`
is not yet a required runtime file; the SGLang descriptor still carries the old
`source_revision` token; the probe's 120 s limit is unverified on a host; and the
`roles_f1` tests collide on port 8100 when run in parallel.

Control-session heartbeats landed locally (additive protocol, negotiated so older
peers are never suspended for silence). Server and agent heartbeat every second;
after `control.heartbeat_suspend_after` (default 5 s) of silence the server marks
the host unresponsive, forgets its readiness proofs and suspends its dispatch and
placement eligibility without releasing anything; hearing it again requires a
fresh probe before dispatch reopens; after `control.heartbeat_lost_after` (default
30 s) the session is lost. An agent that stops hearing the controller reconnects
without touching engines. In a binary test a SIGSTOPped host agent was suspended
after about 4.8 s, new requests went to the other host, the in-flight request kept
its lease and completed once without replay, and after SIGCONT the same engine
served again; a frozen server made both agents reconnect with engines kept. Core
suite 832; not live.

W5 landed locally: park, wake and preinitialize run through the coordinator as
durable operations on an instance's retained binding, so a parked instance always
wakes on its own host with the same binding and generation. Each transition is
budgeted at its peak before arming and settled on evidence; park drains to zero
request leases first; refused transitions keep state and footprint; uncertain ones
keep peak reservation, claim and closed gate until a stop settles them on gone
evidence. `max_parked` and parked budgets are enforced by stopping the least
recently parked instances on that host, never ready work. `park deployment`,
`start deployment` (wakes parked instances first), on-demand wake (concurrent
requests join one restore), `preinitialize deployment` (one instance at a time)
and controller-owned idle timers (`lifecycle_defaults.ready_idle_timeout` and
`parked_idle_timeout`, off when omitted) are wired; restarts adopt parked launches.
Core suite 833; scripted hosts only, no engine parked. Open: requests arriving
while an instance is parking or waking get a retryable refusal, whereas SPEC §6.1
says PARKING and WAKING queue; SGLang park stays refused until a memory-saver
observation source exists; standalone has no idle configuration. The owner
decided on 2026-09-23 that idle timers stay off unless configured.

M16 (per-model smoke) passed live on 2026-09-23 for all ten model and engine
combinations across both hosts (run `matrix-20260923T034935Z`, snapshot `f5d793ea`,
evidence `target/live/matrix/M16-*`), five of them only after a variant or rerun.
Every row answered 3/3 prompts, forwarded logprobs and left zero request leases.
Ready times ranged from 29 s (vLLM 4B) to 585 s (SGLang 27B BF16); checkpoint
digests took 7 to 32 s on first measurement and under 1 s after. Measured
suggested requests: 4B 19–24 GiB, 14B 42–46 GiB, 30B-A3B 74–82 GiB, 27B BF16
71–78 GiB, 27B NVFP4 about 39 GiB steady. Engine and recipe findings: vLLM 0.29 on
qwen3-30b-a3b ran the whole host out of memory during FlashInfer MoE JIT compilation
(the kernel OOM killer also killed the host role) and passed with
`--moe-backend triton` through extra arguments; SGLang 0.5.20 refuses
`--language-model-only` for `Qwen3_5ForConditionalGeneration`, so 27B on SGLang
passes without it; both NVFP4 rows briefly drove MemAvailable to about 0.5–6 GiB
during startup JIT before settling near 35 GiB, so a request sized from the
transient peak (about 130 GiB) is misleading. The I1 identity check was void for
one pair: 27B BF16 on SGLang and 27B NVFP4 on vLLM produced identical greedy text
with different logprobs. Product findings: a host result reporting a launched but
dead engine was discarded, so the controller waited the full Initialize deadline
(fixed locally with a regression test; not yet live); and the coordinator runs one
worker loop for all hosts, so a slow or dead activation on one host delayed stops
and activations on the other by up to 15 minutes, causing both cleanup-timing
failures and five `Endpoint capacity is unavailable` rejections.

W13 landed locally: the launcher records each spawned child's exit by exact
identity; the host agent watches Ready launches every 250 ms, closes the gate and
sends `MemberExit` (repeated every 5 s until settled); the controller closes
dispatch at once, journals `engine_exited` and issues an ordinary stop that
terminates any surviving group members and releases only on gone evidence; status
shows `failed`; the next request relaunches on demand. In end-to-end fake runs
dispatch closed within 2 s of SIGKILL. The cleanup pass also landed: standalone
records installation fingerprints and drift (`MLLM_INSTALLATION_DRIFT`),
`engine_capabilities.py` is a required runtime file where used, the SGLang
`source_revision` token is removed (binary and runtime directory must now be
updated together on each host), standalone engine ports are configurable
(`MLLM_STANDALONE_ENGINE_PORTS`, which also removed the CLI test port collisions),
and `docs/examples/*.yaml` are rewritten and validated by a test. Core suite 836.

Owner decisions on 2026-09-23 from M16: lifecycle work becomes per-deployment
concurrent, so waiting on one deployment's load never blocks another deployment's
start or stop, while admission and reservations stay serialized through store
transactions; and startup gets its own memory budget, declared or measured on first
run, reserved until the instance is Ready and then dropped to the steady request,
with launches on one host serialized through their startup phase whenever their
peaks do not fit together (ADR 0007 phase-aware admission).

The SGLang memory-saver observation source landed locally, so the earlier notes
that SGLang park stays refused are superseded (pending live proof). Read-only
inspection of host-a showed that torch-memory-saver 0.0.10 exports no snapshot
API and that SGLang 0.5.20 `ServerArgs` is a msgspec struct, so the source reads
the saver's per-tag memory pools and asks the CUDA driver whether each segment is
still mapped. The engine enrolls observation from inside its scheduler process
(owner-only record and socket in a 0700 directory, requests authenticated with a
key derived from the launch's admin key, so a restarted host can still observe its
launch). Park counts as released only when every `kv_cache` and `weights`
allocation is observed unmapped; partial observations are refused before an engine
call and uncertain after one. Quiescence needs zero in-flight ingress plus zero
SGLang running and queued gauges. Embedded standalone uses the same observer. Core
suite 847; runtime Python 238; CPU and fakes only. The harness now uses M16's
measured requests and working recipe variants. Live questions remain: whether the
driver reports paused segments as unmapped on unified-memory host, segment counts for the large
models, and that CUDA-graph memory is neither observed nor released by SGLang park.

With M16's measured requests (4B 24 GiB, 14B 46, 30B-A3B 82, 27B BF16 78, 27B NVFP4
40) the planned co-residence pairs no longer fit the 80% managed limit (about
97 GiB). The owner decided on 2026-09-23 to keep 80% and give co-residence
fixtures a smaller declared KV cache and context, while single-model rows keep
full KV. Engines preallocate their KV pool, so a smaller pool trades concurrency
and maximum context for density without making accounting uncertain; the
remaining uncertainty is startup peaks (covered by the startup budget), the
placeholder per-engine margin, and memory outside the pool (absorbed by the free
reserve and the host's published available memory).

Per-instance concurrent lifecycle landed locally (ADR 0015). A scheduler discovers
work and runs each effect (initialize, cleanup, park or restore, settlement) as its
own task per instance lane, with separate bounded pools for activations and
cleanups (`max_concurrent_effects`, default 8), so stops never wait behind loads
and a hung load on one host no longer blocks others; admission and ledger stay
serialized through store transactions, retry cooldowns hold only their own start,
and shutdown joins every task. Because host ingress is keyed by deployment and
member rather than instance, at most one Initialize per deployment and host runs
at a time. Core suite 847, CLI 128 (including the two-host tests) and Clippy pass;
not live.

W10 request-driven switching landed locally. A request for a deployment with no
open instance now waits in a bounded queue (per-deployment and total counts,
buffered bytes, deadline) and joins one activation instead of being refused, also
while an instance is starting, waking, draining, parking or stopping. When the
on-demand start is refused for capacity, the switcher plans on the host needing
the fewest evictions, takes that host's first-come turn, keeps a busy last-ready
victim admitting for the non-resetting admission window, closes its gate, waits for
its request leases to drain (the switch fails and the gate reopens on drain
timeout; nothing is killed), parks deep victims or stops `restart_only` ones and
waits for verified release, then activates the target. Victims serving elsewhere
go first, then least recently used. Switch events and journal entries record each
step. Core suite 860; fake engines only. Gaps: status does not show a switch in
progress; queue limits and drain timeout use built-in defaults rather than host
queue policy and configuration; warm-residency commitments (SPEC §6.5) are not
excluded from victims; on a single-claim host the planner ignores host occupancy;
and a failed switch can reopen a gate that a host-loss closure closed during the
drain window, which must be fixed. The owner decided on 2026-09-23 that an
explicit `start deployment` never evicts unless given `--evict`, which runs the
same switch plan and reports the victims.

The startup budget, per-instance host ingress and co-residence fixtures landed
locally (store schema v27). Deployments may declare `engine_config.memory.startup`;
otherwise a first-run measurement per revision, host and installation (recorded
only when no other launch was on the host) is reused, or a placeholder of
max(request, weights × 1.6 + 8 GiB) applies. Admission reserves the startup peak as
the cold phase until Ready, then the steady request. A per-host activation gate
holds a start whose peak does not fit beside in-flight peaks but would once they
are Ready. Host ingress is keyed per instance, so two instances of one deployment
on one host start concurrently. Co-residence fixtures (`--co`: 8192 context, small
KV) fit the planned pairs within 97.35 GiB. Core suite 860, CLI 130 and Clippy
pass; fake engines only. Problem: the placeholder startup peak for the 30B-A3B
model (99 GiB) exceeds the managed limit, so it can never be admitted to be
measured. The owner decided on 2026-09-23 that an unmeasured model whose estimate
exceeds the limit may start only alone on its host (emptied by the normal switch
rules if needed), reserving the whole managed limit; that run is measured and
later starts use the real peak.

That fix pass landed locally (store schema v28). A solo first start reserves the
whole managed limit, is refused with `startup_requires_empty_host` while any other
engine holds a charge, is made room for by request-driven switching or by an
explicit `start … --evict`, and records its measured peak. `start deployment` and
`start instance` accept `--evict`, journaled and replayable, reporting victims and
the switch id; default start never evicts. Gate closures now record their reason
(`switch`, `host_session`, `engine_exit`), so a failed switch reopens only its own
closure and a passing host probe does not reopen a gate a switch holds. Switch
drain timeout (`switching.drain_timeout`) and queue limits come from configuration,
status shows a switch in progress, and a new `lifecycle.warm: true` flag exempts a
deployment from switch eviction, idle policy and parked reclamation (ADR 0013
amendment). Single-claim hosts are freed by releasing their occupant. The
standalone lab entry points and deprecated park-policy aliases are removed. Core
suite 869, CLI 126 and Clippy pass; fake engines only.

The owner asked on 2026-09-23 for a dedicated performance benchmark row (M80),
driven through the shipped router: per-request time to first token, time to last
token, prefill and decode rates and inter-token latency percentiles across prompt
lengths and concurrency for every model on both engines, router and ingress
overhead against direct engine calls, and the lifecycle latencies users feel (cold
start, wake from park and switch, each measured to first token). It runs after the
current live phase. M80 (`scripts/live/matrix/bench.py`, `rows/M80.sh`,
`bench_report.py`) is built and validated against a fake streaming server; it
never reads engine secrets. The owner decided the same day to track whatever the
engines provide and otherwise measure at the mllm level: mllm's own router and
host ingress record per-request timings (queue wait, activation wait, forwarding,
upstream first byte, total), and the host agent forwards engine latency histograms
from the metrics it already scrapes where an engine exposes them, so the path
overhead can be separated from engine time for both engines through the product.
That instrumentation landed locally: the router records ten per-request phases
(queue wait, activation wait, selection, lease grant, forwarding, upstream first
byte, first content, last chunk, total) per deployment, instance, generation and
engine; host ingress records time to headers, first and last byte; the host agent
forwards bounded deltas of the engines' own latency histograms (vLLM 0.29: TTFT,
end-to-end, queue, prefill, decode, inter-token; SGLang 0.5.20: TTFT, end-to-end,
queue, inter-token) with its load reports; `GET /management/v1/metrics/latency` and
`status`/`inspect deployment` expose each series with its tier and source; and
`observability.timing_header` (off by default) adds per-request timings. M80 reads
them through the CLI. Core suite 877 and Clippy pass; fake engines only; not live.

The two-host live matrix ran on 2026-09-23 (evidence `target/live/matrix/`). Passed
live: M75 and M73 on both engines (launch argv, loopback-only listeners, keyed
engine routes, restart as a new generation, memory return); M05 and M08
(development controls marked `exposed`, SGLang `/metrics` marked, router never
serves engine paths, ingress refuses unkeyed calls); M29 vLLM park and wake on both
hosts (sleep level 2, weight wake, reload, KV wake and prefix reset with the same
processes; about 90% of the ready footprint released; wake on request in 60–70 s);
M28 SGLang park and wake (saver mapping observed going from 39 GB to 0, same
processes, 87.5% released, wake 182 s); M30 park waiting for a stream (SGLang); M32
`max_parked` stopping the least recently parked; M33 preinitialize (SGLang); M34
refusals for `restart_only`, `host_backed` and opted-out hosts; co-residence of vLLM
and SGLang on one host (M19/M22); two instances of one deployment on one host and
across both hosts, including `stop instance`/`start instance` and count 2→1→2 via the
new `deploy model --revision`; balancing (M54 10/10 split, M56 31/33 at 32
concurrent); a frozen host agent suspended after about 5.2 s with no replay (M58)
and rejoining after a fresh probe (M60); engine SIGKILL settled in 1.4 s and
relaunched on demand (M36, M37); the readiness deadline (M74); M38; host agent and
server restarts re-attaching the same engines (M40, M42); `delete deployment
--stop` and `drain host` (M64); request-driven switching same-engine and
cross-engine with correct models at every step (M27, M31) and `start --evict`.
The full workspace suite passed 1486 on host-a; on control-host `a1_gate` and three
standalone tests fail only because that machine's small free memory cannot admit
the fake engine under standalone's 50% policy. Eight product bugs were fixed with
regression tests: an SGLang start beside another loading launch (starts are now
serialized through startup when SGLang is involved), `--mllm-` extra arguments
passing resolution, the SGLang saver library refused as a hard link, SGLang disk
reload renaming the served model, a republished host policy being ignored, a
memory-neutral park blocked by the free-memory check, a fixed 300 s SGLang reload
cap, and the missing revision-aware CLI update. Open findings: the SGLang scheduler's
torch distributed store listens on all interfaces and accepted a connection from
control-host (security); the router's fixed 300 s stream cap cut a long stream and left
its lease uncertain, so a park never armed (M30 vLLM); switching reclaims its own
parked target under `max_parked 1`, so every switch was a cold restart (M31); a vLLM
wake beside a ready SGLang 14B was refused for resources despite fitting (M33); the
solo first start never triggers while a digest is pending; `queue_full` answers
413; `deploy` hides refusal reasons; `inspect deployment --effective-config` is
unsupported on the server role; M57 and M59 steering was not demonstrated. Not
run: M35, M39, M41, M43–M47, M51–M53, M55, M61–M63, M65–M72, M80.

All nine open findings were then fixed locally with regression tests (store schema
v29). Security: torch 2.13's `TCPStore` listens on every interface regardless of
host, so SGLang now uses a file rendezvous in a 0700 directory with Gloo and NCCL
pinned to `lo`, `nccl_port` is reserved, and vLLM pins its host IP and interfaces to
loopback as defence in depth; M08 rerun live on host-a showed only 127.0.0.1
listeners. Streams are no longer cut at fixed wall-clock caps: a stream ends only
when its first event misses the request deadline or a later gap exceeds
`resource_policy.queue.stream_idle_timeout` (default 120 s). A switch no longer
reclaims its own parked target. The M33 refusal came from admission charging
resident engines twice (published free memory already excluded them); hosts now
report per-process resident memory keyed by process identity, and admission credits
Ready owners' verified resident memory, bounded and never beyond their reservation
(ADR 0007). Weights are sized by a stat walk before the full digest, so the startup
estimate and the solo first start work while the digest is pending. `queue_full`
answers 429 with `Retry-After`; `deploy` names its refusal reason;
`GET /management/v1/deployments/{id}/effective-config` and `inspect deployment
--effective-config` work on the server with secrets redacted; and CLI tests no
longer depend on the machine's free memory. Workspace 1527 tests, core 886,
runtime Python 253 and Clippy pass. Still to prove live: warm switching (M31), the
M33 wake, the solo first start and long vLLM streams (M30). A signalled SGLang stop
leaves its rendezvous directory behind (open).

Live reruns and the benchmark on 2026-09-24 (run `matrix-20260923T234659Z`,
evidence `target/live/matrix/`) proved those fixes: M08 showed only loopback
listeners and no rendezvous directory left after any SGLang stop (the host agent
now owns `<state_dir>/rendezvous/<incarnation>` and removes it on gone evidence);
M33 admitted the vLLM wake beside a ready SGLang 14B; the solo first start refused a
plain start, evicted with `--evict`, reserved the whole host and recorded a
measured 82.69 GB peak; a 3000-token vLLM stream ran 379 s uncut and the park armed
0.9 s after it; warm switching kept the same processes across three cycles (M31);
long-prompt skew (M57) and an engine stall (M59) steered new work away from the
loaded or stalled instance with no replay; M54, M56, M58, M60, M43, M68 and M69
passed; M41 settled a launch whose agent died at spawn only at the Initialize
deadline. Product failures still open: stop does not drain request leases before
terminating, so retiring a replica by count (M65) or `delete --stop` (M66) cut
in-flight requests, although SPEC §6.3 says stop drains; stop during Initialize is
refused (M47); a failed deployment cannot be stopped (M53, fix in progress); the
per-request timing header labels the engine `model`; and SGLang 0.5.20 cannot
reload the modelopt NVFP4 checkpoint from disk, so every deep wake of that recipe
fails (left uncertain with its reservation until stop proved it gone). Two
latency-view bugs found during the run were fixed. M61 (mixed-engine replicas of one
route) is not expressible by design (Q6). Not run: M35, M44, M45, M46, M51, M52,
M62, M67, M70–M72.

M80 results (2048-token prompt, one request, through the router): decode rate
tracks model bytes on the unified-memory host, about 21 tokens/s for 4B, 8 for 14B, 4.4 for 27B
BF16, 10 for 27B NVFP4 and 30 for 30B-A3B; time to first token 0.3–2.2 s. vLLM cold
starts are much faster than SGLang (4B 22 s against 68 s; 30B 85 s against 378 s);
vLLM wakes from deep park in 8–81 s, while SGLang's disk-reload wake is close to a
cold start for large models. mllm's path adds about 20–60 ms (vLLM) and 40–100 ms
(SGLang) to time to first token, most of it router-to-ingress and ingress-to-engine
time growing with prompt length; selection is under 1 ms. Reports:
`target/live/matrix/M80-report.md` and `M80-overhead.md`.

The stop-related failures were then fixed locally with regression tests. An operator
stop is always accepted: with nothing held it is recorded at once (stop from a
failed or never-started deployment now succeeds, replacing the old rule that stop
from stopped is illegal); with a runtime held it runs ordinary cleanup; during an
unassociated Initialize it is deferred and issued once the launch settles. Every
ordinary cleanup now drains first, waiting for the instance's in-flight request
leases up to `switching.drain_timeout` (default 30 s) before terminating, which
covers count-decrease retirement, `delete --stop` and drain host. The timing header
names the host-reported engine. SGLang with a modelopt or NVFP4 quantization is
refused `deep` residency (`capability_missing:deep_park`, suggesting
`restart_only`) because SGLang 0.5.20 cannot reload it from disk. Workspace 1545,
core 896 and Clippy pass; fake engines only, pending live recheck of M47, M53, M65
and M66. The live recheck on 2026-09-24 (run `matrix-20260924T121812Z`) passed:
stop during Initialize was deferred and completed on both engines (M47); a plain
stop of a failed deployment was recorded at once and `delete --stop` worked (M53);
reducing the instance count under 12-way load returned 616 of 616 requests with the
retiring instance drained first (M65); `delete --stop` during a long stream waited
the full 30 s drain bound on both engines (M66); SGLang NVFP4 `deep` was refused
before any process started while `restart_only` served; and M73 passed on both
engines. Open: the refusal reason reaches only the server journal (status shows
`failed` without it, although SPEC §6.4 requires status to expose the latest error);
a switch to a target that needs an empty host parks the incumbent, waits 30 s and
then stops it, with a misleading uncertainty message.

I2 landed locally (store schema v23; new `mllm-scheduler` placement). Each
instance carries its own revision, generation and state; a count-only revision
leaves running instances untouched, adds instances without eviction and retires
surplus ones with verified cleanup; any other revision stops each instance and
restarts it on the new revision, durably. Placement runs in the start transaction
with spread or pack, `max_per_host`, deterministic ties, host-label selectors
(`resource_policy.labels`) and per-host ledgers; unplaceable instances defer with a
diagnostic. Deploy resolves against every allowed host and records refusals.
On-demand activation starts the lowest instance not operator-stopped; explicit start
brings up all. Status aggregates instances (`ready` if any is ready, `failed` only if
all failed) and lists hosts and per-instance errors. Core suite 785 passing, Clippy
clean on all crates, and a two-host fake end-to-end test passes; not live. Open: a
remote host still holds one launch claim in its journal, so a second instance on
the same enrolled host is refused `host_occupied` (co-residence works only on the
embedded host); the router still picks the lowest ready binding rather than
balancing (I3); a parked instance's placement is not sticky (W5). Core suite 760 reported (759 distinct); runtime Python 231
(the drop from 281 is WE3's removal of the old checkpoint preflight tests).

I1 landed locally (store schema v22; ADR 0013 accepted with the Q7 amendment; SPEC
§1.1 R06, §2, §10 and §16.4 amended). Deployments accept `instances` (default 1,
at most 64) and `placement` (`hosts`, `selector`, `strategy: spread|pack`,
`max_per_host`), with `host` as shorthand; unplaceable or contradictory shapes are
refused by name. The store records instances, per-host effective revisions and
per-instance bindings, runs, claims, leases and owners; existing deployments
migrate to instance 0 with their accounting intact. Status reports desired and
ready instances, a `degraded` condition and per-instance state with its own
development-controls mark. `stop instance <deployment>/<n>` and `start instance
<deployment>/<n>` exist in CLI and API. The lifecycle still realizes only instance 0:
placement of further instances, resolution against every allowed host, count
changes while running, Q8 stop-all-then-start orchestration, per-instance
on-demand choice, host label matching and per-instance status derivation are I2.
Core suite 760 reported; CPU and fake engines only.

W1 landed locally: deep parking is enabled by default and a host opts out with
`security.deep_park: disabled` (standalone: `MLLM_DEEP_PARK=off`). SPEC §9.1, §16.2,
§18, T21 and AGENTS.md now say so, with ADR 0012 recording the decision. This is not
a production-safety claim: vLLM development-mode controls stay loopback-only behind
the per-launch key guard and are never reachable through ingress or the router.
A `restart_only` deployment never receives a park policy. Status does not yet mark
profiles that expose these controls (open). W3 landed locally: session protocol
version 2 adds Park/Restore member actions, residency evidence, bounded load
reports and member-exit reports; command encoding stays at version 1 so retained
host journals remain readable. After both, the core suite reports 675 (674
distinct) passing and Clippy is clean. None of this is live-verified.

Recovery gaps U5-G1…G3 are fixed locally (not yet live): an uncertain remote launch
is settled by an authenticated host Terminate with gone evidence or stays uncertain
with accounting retained; operator stop accepts it; a restarted controller adopts
such launches. Host session loss suspends remote dispatch (router answers 503),
and a new `Probe` action re-proves readiness with a fresh native model probe
against the identical owned processes before dispatch reopens; server restart uses
the same path. Status now reports `uncertain` rather than `stopped`. The core suite
(673 reported) plus agent/config/protocol tests and Clippy pass. Known remaining
limits: a launch whose host agent dies mid-Initialize waits for the Initialize
deadline before settlement; adoption refuses deployments with in-flight request
leases or a prior-session cleanup step; host journal history is never compacted.

U5 remote single-host SGLang passed live on 2026-09-22. The shipped roles ran with
the server on control-host and an enrolled host on host-a: init, invite, join, deploy
with an explicit `host:` selector, activation, routed authenticated inference
through the host's private ingress, stop, and verified cleanup across three
generations of deployment `01M356HDG005QA16Q7EA617KZA` (answers 42, 63, 42;
streaming returned 200). Readiness came from the host's native model probe. Each
stop left no engine process group, no GPU compute process, and zero reservations,
leases and claims. Unauthenticated router calls got 401, direct ingress without
the gate key 403, inference after explicit stop 429 without autoactivation, and a
replayed stop request returned the original operation. Non-secret evidence is in
`target/live/u5-remote-host-a/`. The first live run found two product bugs, both
fixed with a regression test (T09/T33/T38): controller command redelivery every
500 ms spawned duplicate host effects until the session was torn down mid-launch,
and every reconnect republished the stale startup inventory, which publication
refused after its 2 s freshness window. Controller (237) and agent (40) tests and
their Clippy pass; the full core suite was not rerun by that unit. Open U5 gaps:
G1 a remote launch that goes uncertain cannot be stopped or settled (the first run
ended with an abandoned uncertain reservation in its isolated state directory,
after the engine group it had started was terminated by signal); G2 after a host
restart, a Ready remote deployment stays ready at the controller while the host
gate returns 500; G3 status can report `stopped` while an uncertain engine runs;
G4 host `eligible` is hard-coded false. Controller restart with a live remote
deployment, stream interruption and CLI crash recovery were not exercised.

The D5 model set is in place on both hosts with identical payload SHA-256:
`~/models/{qwen3-4b-instruct, qwen3-14b, qwen3-30b-a3b, qwen3.8-27b,
qwen3.8-27b-nvfp4}`. The first four were copied from host-a over the direct link;
`qwen3.8-27b-nvfp4` was materialized on each host from the complete Hugging Face
snapshot `009632fef96dd349150baa780c984e62e70e91fe` of
`RadixArk/Qwen3.8-27B-NVFP4-BF16-LMHead`. The anchor is a hybrid multimodal
`Qwen3_5ForConditionalGeneration` checkpoint; the NVFP4 build is a modelopt mixed
NVFP4/FP8 quantization with an FP8 KV-cache scheme and is 23.75 GB on disk, not
the catalog's 32 GB. Whether the installed SGLang 0.5.20 and vLLM builds can serve
this architecture and quantization has not been checked by any engine run.

The U5 recovery fixes passed live on 2026-09-22 on native SGLang 0.5.20 (server
control-host, host host-a, deployment `01M35ARNS85PT8D1WYHXK7EFDC`; evidence in
`target/live/recovery-host-a/`). A killed host agent made the router answer 503
within 1 ms, and the restarted agent re-proved the same engine processes and
resumed serving in under 1 s. When the engine had died meanwhile, dispatch stayed
closed and stop cleaned up. A server restart adopted the Ready launch and resumed
serving the same engine processes within 2 s. A launch whose agent died during
weight loading settled at its deadline; an uncertain launch accepted operator stop
and cleaned up once the agent returned; and a server restart during uncertainty
adopted the launch and settled it automatically. The run found three product bugs,
each fixed with a regression test: a 12 ms host clock lead made publication
refuse every inventory and made the controller drop every host result (now
admitted within a 500 ms lead and recorded on the controller clock), and a
terminated launch's ingress entry blocked a same-generation retry (now retired on
proven termination). Status still misleads in two cases: `ready` with dispatch
disabled after the engine died, and `stopped` while an Initialize is in flight
with its agent down. Also open: `join host` fails with a relative `--join-file`,
and there is no deployment-level Initialize deadline (the CLI fixes 900 s).

Remote vLLM (matrix gap G01) is implemented locally: the host agent now selects
the adapter by engine and reuses the S1 vLLM plan builder, extracted to
`crates/mllm-adapters/src/vllm/frozen.rs`. Affected-crate tests (507), the core
suite and Clippy pass. No live remote vLLM run has happened yet.
The five binary role startup/enrollment/
reconnect tests also pass. A targeted recovery regression confirms that replayed
historical native evidence retains ownership but does not refresh readiness or
reopen ingress. Full restart readiness recovery remains required for U8. U5 still
needs the complete remote inference lifecycle and native host-a gate; it is not done.
The final affected integration run passes 178 tests after adding session-loss gate
closure. Ready publication and disconnect share a lock; a threaded race regression
proves that late completion cannot reopen a disconnected session's gate. Reconnect
preserves ownership but does not promote a historical probe to fresh readiness.
The server also refuses forwarding for revoked enrolled hosts. Agent/controller
and Store Clippy remain clean. These checks do not qualify native execution.

U1 preserves local ledger keys and immutable
receipts; group reservation and remote execution remain separate pending work.
The requested end-to-end two-host inference/recovery/cleanup test is distinct
from the broader F4 residency, switching and cache qualification. Those
capabilities remain unqualified until their own evidence is complete.
Plan review covered coherence, feasibility, scope, security and adversarial
assumptions; it corrected an overbroad completion condition that had made all
F4 cache and switching work a prerequisite for this task.

Read-only SHA-256 comparison on both hosts confirms identical checkpoint
configuration, weight index, all three safetensors shards and tokenizer files
under `~/models/qwen3-4b-instruct`. No model or engine was launched for that check.
Both hosts report 200 Gb/s on their two direct interfaces. Bidirectional ICMP
on `192.0.2.10`/`192.0.2.11` succeeds; this is connectivity evidence,
not measured throughput or NCCL qualification.
Enrollment certificate primitives now reject forged/malformed requests,
strip requested CA/server privileges, and preserve the host's public key;
four focused tests and agent Clippy pass. This alone does not establish
completed enrollment or remote transport.
Protected atomic identity storage now passes seven focused tests and Clippy.
It preserves existing files, rejects unsafe/partial state, holds an exclusive
local lock, and permits only one concurrent replacement of a given identity
revision. Typed enrollment persistence, the additive v17 registry and TLS/API
integration now pass U2 verification: 645 distinct core tests, 24 agent/protocol
tests, then all 51 management tests after its JSON error-envelope correction.
Core and affected-crate Clippy pass with warnings denied. Tests prove exact
enrollment and renewal replay, concurrent redemption, expiry and hostname
collision denial, zero bootstrap RPCs to an untrusted server, denial of a trusted
but unregistered certificate, and revocation rejection on an existing TLS
connection. U4 must still close actual AgentControl streams on revocation/expiry
and reauthorize commands. These transport tests do not qualify native multi-node
operation.

U3's canonical typed command digest binds every identity field and action,
normalizes group member ordering, and revalidates typed shape. Five focused
protocol execution tests pass. U3 now adds durable acceptance, session and
assignment fencing, permanent replay tombstones, gated process creation and
owned-process cleanup. Fourteen journal tests cover lost acknowledgements,
restart, cross-journal ticket rejection, missing databases, delayed deadlines
and retained uncertainty. Integration passes 646 distinct core tests and 100
reported agent/launcher/protocol tests (including a nested child summary);
Clippy passes with warnings denied. A reused process-group leader can no longer
be mistaken for verified cleanup. Actual authenticated session wiring and
resource/profile authorization remain U4/U5. These CPU and controlled-child
checks do not qualify native multi-node SGLang.

U4 now exposes strict server/host configuration, atomic role initialization,
private invitation files, join, host inventory views and foreground role startup.
The shipped binary test starts a GPU-free server, enrolls an unprepared host,
reports it online but ineligible, and restarts both roles without changing host
identity. Five product role tests pass, including competing initialization and
explicit missing-config denial. Real TLS session tests cover claimed-identity
mismatch, session replacement, revocation, certificate expiry and bounded queues.
Full integration passes 648 distinct core tests; config/agent/protocol tests pass
128 checks. CLI library/grammar tests and core/CLI Clippy pass with warnings denied.
Host full engine logs require the local `--debug-engine-logs` flag, default off.
U4 rejects execution until U5 supplies approved local resource/profile authority;
received history summaries alone cannot settle ownership or readiness. No native
remote or multi-node qualification is claimed.

## Prior SGLang 0.5.19 gate failure — 2026-09-21

Launch/configuration fixes are committed as `047007a`; the scoped runner and
failure-evidence retention are committed as `5d07f11`. Unrelated working-tree
changes remain uncommitted. The subsequent live run used that working tree.

The authorized host-a run reached the protected wrapper, then failed with
`source_revalidation_failed` in 4.43 seconds. The selected SGLang 0.5.19
installation has group-writable package files/directories, and seven of ten
audited source files disagree with the recipe's pinned hashes. The gate remains
closed. At that point environment changes were prohibited. The owner subsequently
authorized the 0.5.20 migration described above. That earlier attempt changed
no installation or driver and rebooted no host.

The deployment settled stopped with admission and dispatch disabled; no engine
processes remained in the post-run check. Evidence is under
`target/live/20260921T213446Z/`, with details in `live-f2.md`. Private state
is retained on host-a at `$HOME/.tmphQyl3M`. This is not native
qualification. Final S3 review and S2 remain pending behind the live gate.

## Local SGLang launch validation — 2026-09-21

At HEAD `b9b33af`, the uncommitted launch fixes pass the local diagnostic:
standalone reaches the wrapper and reports `launch_failed`, with journal evidence
`sglang_startup_failed: artifact_mismatch`, using `/usr/bin/python3` and the stub
checkpoint. This replaces the prior never-armed failure in this local reproduction;
it does not demonstrate native model readiness. The diagnostic state is retained
in a machine-local temporary directory. Running it inside a sandboxed environment
first failed controller ownership checks because sandbox ancestor UIDs appeared as
`nobody`; the successful run used real host filesystem ownership without weakening
the checks.

The required five-crate core command passes 634 distinct tests (635 reported,
including the owned-state child-process duplicate). Configuration tests pass;
the `roles_f1` and `standalone_lifecycle` CLI targets pass 5/5 with one test thread.
All-target Clippy passes with warnings denied for the five core crates. These are
CPU/Fake checks, not native qualification. The excluded owner files were not read,
edited, formatted, tested or staged. No commit or live host run was performed.

The two configuration concerns are fixed in the working tree. The pinned SGLang
recipe rejects `trust_remote_code: true` during configuration normalization, even
when the host security switch permits remote code. Omitted host `deep_park`
policy now means disabled, and standalone requires `MLLM_DEEP_PARK=on` to enable
it; missing, empty, `off`, and unrecognized values do not grant permission.
SPEC §9.1 and T21 now reflect the current working agreement rather than the older
default-on decision. The SGLang standalone template requests deep residency, so
its next native run requires explicit opt-in. The local diagnostic above predates
this default change and was not rerun in this session.

Focused configuration tests, 14 standalone configuration unit tests, and the five
CLI lifecycle tests pass. Regression tests prove default denial, explicit opt-in,
and early rejection of unsupported remote code. The controller launch regression
also verifies omitted policy renders no sleep flags and sets
`VLLM_SERVER_DEV_MODE=0`. Core and affected-library Clippy pass with warnings
denied. The checkout contains broad pre-existing changes, including formatting,
beyond these fixes; they remain uncommitted and must not be bundled blindly.
The final core run passes all 634 distinct tests (635 reported). An earlier run
hit `AddrInUse` in the SGLang stub-engine test; the full isolated retry passed.
The sandboxed attempt was blocked by home-directory ownership and write checks,
so integration verification used the real host filesystem.
Native rerun and the final S3 review remain pending.

Current host authorization comes from the working agreement: both host-a and
host-b are authorized. The owner authorized the SGLang 0.5.20 environment
migration on host-a; driver changes, reboots, and unrelated environment
changes remain prohibited. Older authorization and closed-entrypoint statements below are
historical and do not override that agreement or the S3 composed startup gate.

## Recent committed work

- `dc5a3c1`: a success resets the attempt budget.
- `0a0a3ee`: retry a failed deployment three times with a 30 s doubling cooldown,
  added through `CoordinatorOptions`.
- `49f8bde`: the ordinary path's types, methods, operation kind and event kinds
  lose the qualified prefix.
- `0556375`: the Fake engine's lifecycle simulation renamed under its own name
  (`fake/lifecycle.rs`, `FakeFault`).
- `9530811`: retired `qualification_id` — the host YAML key, profile field and
  token, and bindings renamed to `identity_id` and `recipe_fingerprint`.
- `92fdeee`: deleted the store/config/domain qualification modules; schema v13
  drops ten tables and removes the negative identity guards.
- `9c8f68a`: deleted the candidate lanes, management routes and the CLI qualify
  verb.
- `1f13d4f`: a closed deployment offers no work; a planned step still expires.
- `f42e84d`: an ordinary test fixture that never runs a candidate suite
  (`tests/support/fixture.rs`).
- `bf4b042`: `NativeLaunchHandoff` is sourced through a `NativeLaunchSource`
  trait.
- `6b2e073`: moved `ArmResult`, cleanup types, SGLang pins and native launch
  types out of the candidate module.
- `57247ae`: the domain park contract as pure rules (`mllm-domain/src/park.rs`).
- `df19a46`: qualification is not an mllm concept (ADR 0011 decision 2; SPEC
  §8.4 withdrawn).
- `1761f07`: a failed deployment closes its own admission, not the host's.
- `c6915fd`: the A1 gate on the Fake engine — deploy, start, and one inference
  served through the router, with the coordinator as the sole authority.
- `d2a6117`: a start binding is identified by what it is, not by one spelling, so
  a restart-only deployment can be started at all.
- `8c9a17a`: associated candidate Cleanup through the original retained runtime,
  with clock-free discovery and verified atomic release.
- `ec05bd8`: owned candidate Abort with retained accounting and strict SSE replay.
- `c6da962`: deadline-bound owned Finish with preserved V3 catalog history.
- `8a1fbec`: owned candidate Park/Restore through the original retained Fake.
- `67c0678`: authenticated candidate-run creation using the owned Store/session,
  shared bounded command capacity, exact durable retries, and no runtime effects.
- `1612945`: ordinary owned cleanup acceptance, arming and verified completion;
  generation fencing and atomic release only after exact cleanup evidence.
- `153f6d9`: application-owned bounded local pressure monitor with independent
  stale-read watchdog and cancellation-safe shutdown.
- `744d542`: pinned detokenizer source added to startup verification; all ten
  selected files match the isolated installation and upstream pin without imports.
- `2c2b491`: bounded coherent historical operation lookup.
- `8f238c9`: bounded F2C latency summaries with separate failures and timeouts.
- `d430074`: owned ordinary Fake cleanup through verified durable release.
- `2aff45e`: scoped Start receipts preserved across cleanup and replacement.
- `949609b`: versioned private native launch scope from persisted execution.
- `ae919db`: isolated Python 3.12.3 decoder verification on host-a.
- `7d139a8`: owned Start admission serialized with shutdown and fatal closure.
- `9de1fa8`: no-site isolated interpreter startup before protected native guards.
- `14c2923`: authenticated owned Start and Stop submission.
- `f3a2684`: bounded exact-marker correctness validation for the future F2C runner.
- `3e4bd89`: expired never-armed Initialize terminalization without runtime effects.
- `e4e2e95`: separate monotonic request timing validation.
- `f40abd1`: explicit Stop for never-armed Initialize without runtime cleanup.
- `c0a07dd`: bounded metadata-only request journals.
- `e11c3de`: private descriptor-relative artifact storage with a shared byte cap.
- `861a3ad`: bounded collected JSON marker response validation.
- `eb8e572`: bounded streamed marker data-event validation.

Expired, never-armed ordinary Fake Initialize requests now terminalize atomically.
The worker proves absence of execution, grants, ownership and runtime identities
before releasing unused endpoint and binding reservations. No memory release,
cleanup evidence or ledger epoch is invented. Armed uncertainty remains retained.
Expiry events replay through the management SSE stream.

Explicit Stop before Initialize arms also passes root integration. Acceptance
fences generation and retains reservations; the owned worker releases only after
the prior task exits and a separate atomic no-effect proof succeeds. It records
distinct Stop history and SSE events without cleanup evidence or a memory epoch.
Associated runtime cleanup remains unchanged. Armed unassociated work is retained.

F2C request journals now serialize only closed outcome codes, numeric corpus
ordinals and validated monotonic durations. Private artifact storage creates
exclusive mode-0700 run directories and mode-0600 fixed files relative to a trusted
parent descriptor. Writers share an at-most100-MiB payload cap and stop on failure;
partial evidence is retained. Each file requires explicit sync. These helpers do
not validate a run manifest, prove route identity, authorize effects or complete
the API-driven runner.

Collected JSON and decoded streaming data events now have bounded marker checks.
They validate the served model, one choice, exact ordered content and natural stop;
streaming also requires one terminal event. Malformed envelopes and alternative
output fail closed without response text in diagnostics. Streaming checks accept
already-decoded data events, not raw SSE bytes, and reject usage-only events.
A bounded LF/CRLF framing helper now feeds those events across arbitrary network
byte splits, including split UTF-8. It supports comments and multiline data but
rejects other SSE fields, lone CR and incomplete frames. HTTP status/content type,
clean transport completion and binding provenance remain runner obligations.

The F2C phase-margin calculation now uses checked integer arithmetic for
`peak + max(2 GiB, ceil(peak/4))`. It rejects overflow instead of saturating or
wrapping. This numerical helper does not establish attribution, verify a recipe
or reduce any reservation; missing attribution keeps the conservative grant.
Its pressure-case helper selects the smallest whole-GiB ceiling covering every
supplied intermediate charged demand while denying direct wake. It rejects an
empty or unsafe interval. Actual verified attribution, complete ledger totals
and planner feasibility remain caller obligations, not numerical assumptions.

The candidate pipeline these paragraphs used to describe in detail — Initialize,
run-scoped inference, Park/Restore, Finish, Abort, Cleanup, and the qualification
ceremony that gated them — is deleted (ADR 0011; Tasks 7–9 of the qualification-
removal plan, commits `9c8f68a`, `92fdeee`). Narrating its internal mechanics here
would describe code that no longer exists; see "Recent committed work" above for
the deletion commits and the A1b entry below for what replaced it.

Authenticated Start and Stop HTTP submission now passes root integration
verification. The optional lifecycle router shares the existing owned state,
trusted principal and bounded command capacity. Stop resolves generation in its
acceptance transaction after historical receipt lookup. Exact retries survive
cleanup, replacement and worker shutdown; accepted responses do not claim Ready
or cleanup completion. Narrower routers gain no lifecycle authority. No listener,
native lifecycle support or additional cleanup/recovery path is introduced.

Scoped Start command receipts now pass root integration verification. Exact
retries preserve the original operation, joined value and deadline after Ready,
verified cleanup, replacement and valid session rotation. Historical reads grant
no execution authority; current resource-policy gates still govern new acceptance.
The owned worker now provides bounded command handles. Exact receipt history is
read before current admission flags; fresh commands serialize with shutdown,
Drop, initialization pause and fatal closure. The HTTP adapter maps typed Store
errors to fixed public categories. Retained handles keep the owned state/process lock
for historical reads but cannot restart execution.

The owned Fake cleanup worker passes root integration verification. It retains
the original instance, waits for Initialize to exit,
supports explicit same-session cleanup after associated uncertainty, and sends
only after a new durable cleanup arm. Unverified outcomes retain authority.

## Remaining implementation and verification

1. Complete ordinary warm lifecycle, sequence/preinitialization, no-spawn
   terminalization, missing-association cleanup and restart reconciliation.
   Expiry and explicit Stop for never-armed Initialize are implemented. Other
   no-spawn states and missing-association/restart recovery remain open.
2. Ordinary park (drain, park, parked accounting, wake) is not designed; the
   contract it must satisfy is `mllm-domain/src/park.rs`. The ordinary native
   launch is designed and its launch path is landing on this branch;
   `NativeLaunchHandoff` waits on a `NativeLaunchSource` implementation.
   Native parking is blocked on both engines regardless: `VllmAdapter` has no
   `execute_persisted`.

## Milestones and review gates

Work follows [ADR 0009](../design/adr/0009-proof-carrying-reconciliation.md). Review
happens at a milestone boundary, not after each task. Units inside a milestone are
verified by focused TDD plus the core suite and carry no separate review pass.

Each milestone leaves a working system and is independently reversible. The order is
deliberate: the largest deletion is last, because doing it first would restructure the
most intricate logic in the project against tests that have never run in production.

### A1 — Production cutover

The cutover is done on the Fake engine and the gate is met there. A native engine
still cannot be started, for the reason recorded below.

- [x] Engine family to adapter resolution (`mllm-adapters/src/resolve.rs`).
- [x] Proof that recorded processes are gone, from identities rather than a live
      handle, so it survives the restart that destroys handles.
- [x] Engine-generic driver factory (`spawn_resolved`): read the declared engine,
      build an `AdapterSpec`, resolve, prove cleanup with `observed_gone`.
- [x] Wire the coordinator into `roles.rs`; retire the handle map; resolve adapters
      per binding (`23e3f35`, `ffe6af6`, `8996065`).
- [x] Accept an ordinary Start for a restart-only deployment (`d2a6117`). The start
      validator asserted the fake-engine fixture's shape, so every Start was refused
      as corrupt stored data and nothing could run at all.
- [x] Drive an accepted Start to Ready and serve through the router (`c6915fd`).
      Four fixture assumptions blocked it: the observation source reported the
      agent's `system` label rather than the host's declared domains; `AdapterSpec::Fake`
      resolved to a bare `FakeEngine` whose `execute_persisted` answers `Unsupported`;
      the Fake engine recognised an ordinary initialize by a `qualified:` id prefix;
      and the router read routes only from the legacy `route_model_id` column, which
      managed configuration clears.
- [ ] Remove the legacy authorities together, as the A2d plan requires: synthetic
      admission, empty-ledger checks, old reservation writers, router-owned eviction
      and in-memory release guards. Never two authorities at once.

**Gate:** deploy, start and serve one inference through the router with the
coordinator as the sole lifecycle authority, on a real engine.

`crates/mllm-cli/tests/a1_gate.rs` is that gate as one test and it passes **on the
embedded Fake engine**, which is what standalone declares when no live profile is
configured. That is the first end-to-end evidence the project has, and it is not
verification of a native recipe (SPEC §18).

The native half of the gate is still open. The owner confirmed on 2026-09-16 that
**both** engines are required, not one:

- vLLM is done: `VllmAdapter::execute_persisted` launches from the frozen profile,
  records process identities and probes readiness, and it is live-green (S1, run 6
  below).
- SGLang: the native entrypoint denial was **composed open** on 2026-09-19
  (owner-authorized; S3 plan
  `docs/plans/2026-09-19-sglang-launch.md`, commits `393b197..17b6ffc`).
  `sglang_entry._verified_native_contract` runs the audited gates (source
  revalidation, plugin closure, placement, checkpoint) and the guarded engine
  import follows when the contract holds; the ordinary descriptor carries
  `sglang_private_launch`/`sglang_launch` kinds and the route-name served token.
  The mllm side is complete: `ProfileBindings` builds the SGLang runtime, seals
  inference + admin roles (`engine_secrets` v15), the adapter spawns through
  protected descriptors (v2 launch scope), readiness is real, and the Rust and
  Python validators are ASCII-printable-parity. **Not yet live-verified:** a live
  launch still needs the host to publish `device_inventory_digest` and the
  guarded launcher to set the child's `CUDA_VISIBLE_DEVICES`; until then the
  audited argument mapper fails closed (`placement_mismatch`). Residual
  placement risk: any single GPU in the verified inventory satisfies placement,
  because `device_id` is not yet bound to a physical UUID
  (`DevicePolicy.physical_gpu_uuid` is the hardening follow-up). Task 10 of the
  plan is the live gate on host-a; nothing here qualifies the native recipe.

Both are read from the code, not from an observed run: no native SGLang start has
been attempted since the cutover.

Park was originally part of this gate and has moved to A1b. The ordinary lifecycle
has no park at all; the candidate path that formerly had one is deleted (ADR 0011).
Ordinary stop returned with `b52f729`, which split the suspension predicate;
park has not.

The owner confirmed on 2026-09-16 that parking is the product's premise, not an
option: **one model parked while another serves, switching between them
automatically, is the reason the box holds more than one model.** Anything that
reduces eviction to stop-and-restart misses the point of the project.

### A1b — Implement eviction in the authority

Pressure-driven switching is not implemented in the product. `SwitchEngine` is the
only implementation of drain-release-wake, it lives in the router, and production
never constructs it: `mllm-cli/src/roles.rs` wires `WakeJoin` and `auto_activate`
instead. Before the port extraction, `ready_deployments_excluding` had exactly one
caller, `switch.rs`. `request_transition_inner` handles suspension flags and the
preinitialize contract and never looks at another deployment.

So when a request arrives for a deployment while another holds the exclusive pool,
nothing releases the incumbent. The engines can perform the switch — that was
measured on 2026-09-16 — but mllm has no way to ask for it. This is why the F2 exit
gate's warm-switching criterion could only be demonstrated engine-direct.

- [ ] Implement drain, release and wake in the lifecycle authority, taking
      `SwitchEngine`'s semantics as the contract: close admission, bounded drain
      grace, quiescence through the adapter, park or stop by declared tier, and on
      failure reopen the incumbent unsuspended and journal the failed switch.
- [ ] Move the activation join to the authority so simultaneous arrivals collapse to
      one operation, keyed by deployment, revision and generation.
- [ ] Delete `SwitchEngine` and, with it, the two writes the router currently makes
      through the port.
- [x] Give the ordinary stop an intent (`b52f729`). Schema v11 adds `admin_stopped`,
      carrying the operator's intent alone; `suspended` keeps its nine eligibility
      readers untouched. This is what the earlier attempt could not do by writing
      `suspended`, from either side of acceptance.
- [x] Make the park tier declarable and host-validated (ADR 0010). Residency names
      the tier (`restart_only`, `host_backed`, `deep`); a host declares each domain's
      memory topology; a host-backed park is refused at configuration time on a
      one-pool domain; SGLang's startup flags follow the declared tier. This makes
      the choice expressible and checkable. It does not implement park.
- [x] Remove qualification (ADR 0011). mllm guards the host; the user owns the
      recipe. The candidate and qualification subsystem is deleted, schema v13
      drops its tables, the park contract survives as pure domain rules, a
      failed deployment closes its own admission and is retried three times
      with a doubling cooldown before it is given up on, within the start
      command's deadline, and an uncertain attempt still resolves through the
      gone-proof first — an explicit Stop drives that cleanup and the Start
      that follows is a new generation with a fresh budget. CPU and Fake tests
      are not verification of any native recipe.
- [x] S1 — native launch (vLLM), live-green on host-a on 2026-09-18 (run 5
      at `000b832`, six of six scenarios, evidence entry in
      `docs/runbooks/live-f2.md`). The coordinator directs a native
      builder to launch a real vLLM 0.29 engine: cold start to Ready in 27 s,
      inference through the router, loopback-only listening with the guard
      middleware refusing unkeyed control routes, a stop that proves the group
      gone in 1.2 s, a restart under a new incarnation, a bad model source
      closed in 5 s with the next start on the same controller reaching Ready,
      an executable that exits at once closed with no leftovers, and memory
      returning after stop. Landed on the way: encrypted per-launch engine keys;
      a launch that fails after arm is terminated, proven gone and released with
      evidence; configuration for `deep_park`, the model store and the model
      source; the Fake engine moved out of the product into `mllm-testkit` as a
      test fixture; the router's per-deployment forwarder keyed on what the
      coordinator recorded. Runs 1 to 4 each found a defect the CPU suite could
      not see because its fixtures did not have the launch path's real shape
      (pre-flight self-match, api-only identity refused as corrupt, adapter
      probing without its key, zero start ticks on this kernel failing every
      process scan, re-admission gap after a closure); each is recorded with
      its fix in the live runbook. Open items: post-launch retry stays deferred
      to SPEC §6; state directories written before commit `e4dcd20` must be
      recreated, because the model-source shape changed the recipe fingerprint;
      vLLM 0.29 authenticates only `/v1`, `/v2`, `/inference` and `/cohere`, so
      `runtime/mllm_vllm_guard.py` covers the remaining development routes
      itself and L3 holds that true; the manifest-hash fingerprint for a
      `local` model source is deferred to S1b, so standalone still writes the
      placeholder `sha256:<name>` and the spec is amended to say so; whether a
      parking residency with `enable_sleep_mode` false and `deep_park` enabled
      should be refused or defined as restart-only parking is a question for
      the S2 ADR, since such a profile resolves today and S2's park would call
      `/sleep` on an engine started without `--enable-sleep-mode`; S1r (restart
      re-attach) is next. What
      this establishes is vLLM 0.29 with qwen3-4b-instruct on this host and
      nothing about parking, SGLang, re-attach or other builds. CPU and
      Fake-engine tests here are a pre-check, never the claim that a native
      engine recipe works live. Confirmed again after the whole-branch
      review's fix wave (`a7e72ff`..`44a3a42`) and the three re-review items
      (`74aa941`): run 6 on 2026-09-19, six of six, 137 s, no leftovers
      (evidence under `target/live/20260919T152154Z/`).
- [ ] Implement ordinary park. The ordinary lifecycle has no park at all; the
      candidate path that formerly had one is deleted. This is the premise of
      the product and the largest remaining piece of A1b.

Ported faithfully first, keeping the existing T16 and T19 tests as the contract. The
semantics were written against F1's assumptions and deserve revisiting against the
proof-carrying model, but that belongs in A3 rather than here, where it would rewrite
the tests that define correct behaviour.

**Gate:** a request for a deployment whose pool is held by another causes the
authority to release the incumbent and serve the request, with no router involvement
beyond asking.

### A2 — Extract the domain

`mllm-store` is larger than the controller, management, adapters, router and
scheduler combined because workflow logic followed the transaction into it.

- [ ] Create `mllm-domain` as a pure crate: planner, policies, resource algebra,
      proof rules. No async runtime, no clock, no engine knowledge.
- [ ] Move rules out of `lifecycle`, `progression`, `initialize`, `security`, `warm`
      and `cleanup` with no behaviour change. The store keeps its tables.

**Gate:** `mllm-domain` compiles without an async runtime and its tests run with no
database and no network. A rule that cannot be tested that way is in the wrong layer.

### A3 — Capabilities and proofs as data

- [ ] Engines declare the actions they perform and the facts they can prove instead
      of failing when called.
- [ ] The domain permits a transition when its required proofs are a subset of what
      the installation proves.
- [ ] vLLM reaches restart-only parking by mechanism rather than by special case,
      which is the fallback `SPEC.md` §6.2 already describes.

**Gate:** adding an engine family requires publishing two sets and no coordinator
edit. The proof set gates the commit, not the call.

### A4 — Collapse the second lifecycle

Discharged by deletion (ADR 0011).

## Tracked for later: naming and engine resolution

[ADR 0008](../design/adr/0008-engine-installations-and-runtime-types.md) makes
"engine installation" the term of record, but internal type names still say runtime
profile. Rename `RuntimeProfile` and its configuration key, and keep one name per
concept on every new surface in the meantime. The mockups additionally use "runtime"
for three different things — start mechanism, Python version and CUDA version — and
only the first is the runtime type; the others are build metadata that
`build_fingerprint` already covers.

Add the engine-family to adapter resolution layer. Adapters are currently selected
at hardcoded construction sites, which is what blocks both a third engine family and
the construction of `SglangAdapter` on the runtime-binding path.

Two mockup behaviours conflict with the spec and should not be implemented as drawn.
Raw engine flags include `--served-model-name`, which `engine_policy.rs` reserves,
and the interface warns that a raw flag overrides a structured setting; T14 requires
conflicts to fail with provenance instead. The CLI grammar is also resource-first
(`mllm hosts list`), where R11 requires action-first (`mllm list hosts`).

## Earlier multi-node constraints, updated for the current two-host work

These constraints were originally deferred beyond the standalone F2 recipe.
The owner's current two-host instruction and the plan linked above now govern
this work. The standalone recipe remains TP=1, DP=1; distributed qualification
requires its own recipe and evidence under SPEC §11.

1. Local completion now admits one `api` plus contiguous `worker-0..N` identities
   sharing a boot identity. U1 adds a separate host/member-scoped group contract,
   so equal PIDs on different hosts are valid. Group lifecycle settlement is
   still pending; the local completion path does not establish it.
2. General tensor, pipeline, data and expert layouts remain unqualified. U1's
   group contract permits only two distinct hosts with one device each and ranks
   0/1. U7 still needs to connect that topology to configuration normalization
   and the distributed native recipe under T27.
3. U1 adds `mllm-domain::group` with member identities, TP2 ranks, peer addresses
   and rendezvous data. Production group reservation, dispatch and recovery
   remain pending under U6; validated shape alone grants no launch authority.
4. Multi-rank release and resume acknowledgement is unverified. Per the F2B plan,
   SGLang's release and resume await their communicators, and a success reply from
   the tokenizer manager does not prove every rank released. A partial release that
   reads as success would be exactly the unevidenced release `SPEC.md` §6.1 forbids.
   The 2026-09-16 verification proved the single-rank path only.
5. `NativeResidencyObserver` now fails closed when allocations span more than one
   device, because summing mapped bytes across devices cannot distinguish a fully
   restored group from one restored rank. Per-rank evidence, and cross-host
   aggregation for a multi-node group, remain unimplemented.
6. Hardware: host-a has a single unified-memory host, so no parallel topology can be verified
   there. Tensor parallelism needs a multi-device host; multi-node needs two hosts.

## Open questions

1. A caller waits the full 600s bound to learn an operation is uncertain.
   When `drive` fails after arming, the worker marks the lifecycle run `uncertain`
   and pauses, holding the retained binding until an explicit Stop and a verified
   cleanup. That is the proof-carrying model working as intended. But the
   `operations` row stays `running`, and `CoordinatorLifecycle::classify` reads only
   that row, so `wait_terminal` polls for the whole `TERMINAL_WAIT` (600s) before
   reporting `Uncertain`. Observed on 2026-09-16: a regression run took 600.15s to
   fail. The run state is durable and says `uncertain` immediately, so the caller
   could be told at once. Changing it alters what the router does with a request
   that triggers activation — it would fail fast rather than hold the client — so it
   is the owner's call, not a repair to make unattended.

2. Stopping a deployment that was never started reports a conflict.
   `accept_ordinary_cleanup_in_transaction` finds no unreleased runtime binding and
   returns `LifecycleError::Conflict`, which reaches the caller as
   `LifecycleFault::Conflict` — "your view is stale, re-read and retry". Re-reading
   will not help: nothing was ever started, so this is an illegal transition and the
   honest answer is a refusal. `LifecycleError` has no variant for that today;
   `Disabled` is the closest and means something else. Pinned by
   `standalone_lifecycle::stop_is_illegal_from_stopped` so a change is deliberate.

3. A resource policy written before ADR 0010 cannot be read back.
   `StoredPolicy.version` stayed at `1` when `StoredDomain` gained a required
   `memory` field, so a policy row written before that change now fails to decode as
   `CorruptStoredPolicy`, and `import_resource_policy` does not overwrite it — it
   reads the existing row first and propagates the error. Failing closed is correct:
   the old row genuinely lacks the topology fact and ADR 0010 forbids inferring it.
   The defect is the diagnosis, which says "corrupt" for what is merely a superseded
   shape. No persisted policy exists on this machine. **If standalone refuses to boot
   against a state directory created before 2026-09-16, delete the directory** — the
   host policy is republished at every boot.

4. Retry cooldown sleeps inside the single worker loop. When a deployment fails
   and is waiting out its doubling cooldown before the next attempt, the worker
   sleeps in place, which blocks it from discovering and advancing any other
   deployment for up to 30 s per wait. This is accepted for A1b standalone,
   where one worker and one deployment are the common case, but it will not
   scale past that. The fix shape is a not-before time read from
   `deployment_attempts.last_attempt_ms` and checked at poll time instead of a
   blocking sleep, so the worker keeps discovering other deployments while one
   waits out its cooldown.

5. The give-up reason is recorded in the journal but not in the deployment's
   own state. Every counted attempt and the give-up itself now write a
   `journal_entries` row naming the deployment and the reason, in the same
   owned transaction that counts the attempt or closes the admission, and the
   observer reports a planned step of a closed deployment as `Closed` rather
   than `Superseded`. What is still missing is a failure category or
   last-error text on the deployment row itself, so a caller reading only
   `deployments` still cannot tell a budget exhaustion from any other reason
   admission might be closed.

6. Stored kind strings of the ordinary path were renamed in the same change
   without a data migration; a v12 state directory that holds lifecycle
   history is recreated, as the design keeps no compatibility. `operations.kind`
   went from `qualified_initialize` to `initialize`, the owned-launch
   association tag from `qualified_owned_launch` to `owned_launch`, the
   management event kinds from `qualified_*` to `initialize_*`, and the plan's
   own tag with them. Schema v13 drops tables and rewrites none of these, so a
   pre-v13 directory carrying lifecycle rows fails to decode rather than
   upgrading. **Delete such a directory**; the host policy is republished at
   every boot.

7. Fingerprint drift between an effective configuration snapshot and the
   current host is not checked at deployment start. The refusals that used to
   catch a stale or mismatched recipe came from the deleted qualification
   catalog and judged the recipe, not host capacity; nothing replaced that
   check when the catalog was removed, so a start can proceed against a
   snapshot that no longer matches the host it targets.

7. The dispatch seam — grant, close, finish and pending dispatch, and
   `request_leases` — has no production issuer. Router dispatch ownership (the
   F2A2c plan) is the intended one and has not landed. Ordinary tests use the
   seam directly today to exercise the request-lease guards, which is useful
   coverage but not evidence that anything in production calls it.

8. Two SGLang wire kinds, `sglang_launch` (public) and `sglang_private_launch`
   (schema version 2 private descriptor), plus the served-name rule "the
   deployment's route name" (`work.effective().routes.first()`), are the
   contract the 2026-09-19 ordinary native launch rename produced. The former
   `sglang_candidate_*` kinds and the `candidate-{binding_id}` rule are gone;
   both validators reject them.

9. `crates/mllm-controller/src/sequence.rs`'s planner still keeps a
   `qualified_park`/`qualified_restore`/`qualified_initialize` eligibility
   vocabulary inherited from the legacy F1 `Controller` lineage. This plan did
   not touch it; renaming it is work for the park ADR that wires ordinary park
   against the `mllm-domain/src/park.rs` contract. The legacy
   `crates/mllm-controller/src/operations.rs` `Controller` itself is untouched
   by this plan and remains slated for retirement at the A2d gate, per the
   milestones section above.

## Owner attention

- **Live rows owed by the final review fix wave (2026-09-26):** DG6 upgrade
  from 0.1.0-rc.4 (policy migration), DG1 switching (observed-memory park or
  stop), DG3 SGLang parking (saver permission warning), and one unified
  switching row plus the unified standalone boot on each lab host (one
  crediting rule; GB10 UUID cross-check).

Two items from S1, 2026-09-18. `crates/mllm-cli/tests/live_interactive.rs`
(the owner's, excluded from agent edits) uses `ParkPolicy::ExperimentalAllowed`,
which is now a deprecated alias of `ParkPolicy::Enabled`; workspace-wide clippy
with warnings denied fails on that one line, and every other crate passes. The
alias constants and the `start_standalone_with_policy` and
`LiveVllmProfile::from_env` shims exist only for that file and can go once it is
updated. Its test `lab_http_auth_status_and_busy_controls` also fails, because
it boots standalone without declaring an engine installation, which S1 made a
refusal (`NoEngineInstallation`). Separately, `roles_f1.rs` is flaky under
parallel test threads; this predates S1 and the live runner uses one thread.


Execution capacity item: after Cleanup committed, fresh-worker creation for the
queued trusted response-capture unit failed with `agent thread limit reached`.
The visible workers are complete and no Cargo process remains. The available
tools expose no worker close/release operation. The current execution skill keeps
capacity-limited work queued and forbids reusing a completed worker for another
unit. Continue from a fresh session with worker capacity, or explicitly direct
inline implementation. No response-capture worker launched or changed source.

Root verification for the associated candidate Cleanup slice: Store 204,
controller 243 and management 67 tests pass (514 distinct core tests). The same
full five-crate run also passes all 88 adapter and 74 harness tests, for 676
distinct tests. Initial integration exposed an unnecessary clock sample during
idle Cleanup discovery. The narrow repair preserves every action clock fence and
the existing two-sample assertion; the complete rerun passes on final code. Two existing
caller-timeout tests failed during the earlier
unarmed Stop worker's concurrent fixture runs, then passed unchanged in isolated
reruns and subsequent bounded full core runs. No timeout or evidence-freshness
limit was changed.
Tests used four threads to bound concurrent fixture load; internal race tests
remain enabled. Separate no-site verification passed 10 renderer, 15 runtime-binding
and 218 Python runtime tests; all16 launch-decoder tests also pass on isolated host
Python 3.12.3.
The full root run also passes all 74 harness tests, including five phase-bound/ceiling, six exact-marker,
seven collected JSON, eight streamed-data, five SSE framing, five timing, seven journal and nine
protected-storage tests.
The full Cargo harness count includes the three pressure-ceiling tests.
All-target Clippy also passes for these five crates with warnings denied.
None is native verification evidence.

One existing item remains for the owner's inspection: check the untracked
`crates/mllm-cli/tests/live_interactive.rs` for formatting from the earlier
workspace-formatter incident. There is no original baseline for that file, so
this work cannot certify or restore it. It remains excluded from reading,
editing, formatting, tests and staging. The separately modified local Task 2 report
also remains excluded and untouched by this continuation.

Host access item: RESOLVED. The 2026-09-15 SSH timeouts no longer reproduce.
A read-only check on 2026-09-16 connected successfully; `host-a.tailnet.ts.net`
resolves to `100.64.0.10` over Tailscale and port 22 is open.

Kernel and GPU driver item: OPEN, owner action required. host-a rebooted at
2026-09-15 22:53 into kernel `7.0.0-1019-nvidia`, which has no GPU driver module.
`modprobe -n -v nvidia` reports `FATAL: Module nvidia not found in directory
/lib/modules/7.0.0-1019-nvidia`. No nvidia modules are loaded and no `/dev/nvidia*`
nodes exist, so `nvidia-smi` fails. The previously booted kernel
`6.17.0-1031-nvidia` still carries the complete stack: `nvidia.ko`, `nvidia-uvm.ko`,
`nvidia-drm.ko`, `nvidia-modeset.ko` and `nvidia-peermem.ko`. Driver packages
`nvidia-driver-580-open 580.173.02` remain installed. `GRUB_DEFAULT=0` selects the
newest kernel, so the upgrade silently changed the boot target.

Two owner options. Booting `6.17.0-1031-nvidia` and pinning it is the faster and
more reversible one; building the 580-open driver for `7.0.0-1019-nvidia` through
DKMS is the forward fix. Either way, pin the boot entry so a future kernel upgrade
cannot silently remove GPU access again. The agent did not change drivers, modules,
boot configuration or power state; all checks were read-only.

No new approval is required for the current bounded implementation. Only
host-a is authorized. The approved isolated SGLang environment and reviewed
observer patch do not authorize changing existing engine environments, drivers,
rebooting, or accessing host-b. Both native entrypoint denials remain closed;
there has been no model load or native verification in these slices. Build and
live-effect gates remain explicit rather than inferred from passing CPU tests.

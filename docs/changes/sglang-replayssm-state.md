# Status: SGLang ReplaySSM spec-verify and MTP drafts sized as SGLang allocates them — 2026-10-14 (branch `fix/sglang-replayssm-state`)

Reported for a Qwen3.8-27B NVFP4 deployment with EAGLE (4 draft tokens,
`--speculative-eagle-topk 1`), `--enable-linear-replayssm-spec`,
`--max-mamba-cache-size 110`, `max_concurrent_requests: 22` and
`memory: {request: 79GiB, kv_cache: 29GiB}`: CapyCTL counted
`(22 + 1) × 4` intermediate recurrent states (13.2 GiB) that SGLang does not
keep under that flag, so the static pool it rendered was the whole request
and the margin was 0 (ADR 0014 amendment A14 and its note on SGLang's static
pool on unified memory).

SGLang 0.5.21 and 0.5.20 (both define the flag; their sizing is the same)
read from the published wheels: with the flag on a gated-delta-net or KDA
hybrid, `_handle_max_mamba_cache` (`kv_cache_configurator.py`) reserves no
intermediate states and charges a ReplaySSM ring for
`max_running_requests + 1` requests instead, and `MambaPool.__init__`
(`memory_pool.py`) allocates no `intermediate_ssm`, but allocates that ring
(per linear-attention layer and record, bfloat16 compact d and k with their
low parts and a float32 gate; 16 records unless `--linear-replayssm-cache-len`
says otherwise) and still allocates the deduplicated conv windows
(`draft tokens + kernel - 2` columns) for each running request plus one. The
flag also makes the temporal state float32 unless `--mamba-ssm-dtype` names
another dtype (`handle_linear_attn_backend`).

`crates/capyctl-config/src/context_fit/sglang_pool.rs` now counts that: the
state the arguments fix (`argument_state`) and the state CapyCTL sizes itself
hold the slots plus, for each running request plus one, either the
intermediate states (no flag, unchanged) or the ring and the conv windows
(flag), and the temporal state is float32 under the flag. For the reported
deployment that is 0.67 GiB instead of 13.2 GiB, and the static pool is the
request less its 8 GiB margin. The flag is recognised as SGLang's parser takes
it: the full name or an abbreviation that is not `--enable-linear-replayssm`
(the decode ring, another option), in the deployment's or the host-fixed
arguments; it is a `store_true` flag, so `=true` (which SGLang refuses)
counts nothing. A derived request, whose resolution reads no checkpoint,
still reserves the intermediate states under the flag; they exceed the ring
at its default length, and the launch sizes what SGLang keeps within that
reservation. Deployments without the flag size and fingerprint as before.
ADR 0014's state formula notes the flag.

Tests (`sglang_pool/tests.rs`, T14): the reported deployment without the flag
keeps its figures (475136 KV tokens, a 79 GiB static pool) and with it
renders a 71 GiB one; at a tighter request the static pool is exactly the
111 slots and 23 ring-and-window sets; an abbreviation, the host-fixed
arguments and a longer ring are counted, and `--enable-linear-replayssm` or
`=true` are not; CapyCTL's own sizing at 79 GiB was refused and now holds 22
running requests; the flag's float32 temporal state. Those three tests
failed before the change and pass after; a derived request's reservation
under the flag is pinned unchanged, and the existing pool, recipe-fingerprint
and golden effective-revision tests pass unchanged.

## An EAGLE draft from the checkpoint's own MTP head

Found live on host A on 2026-10-14 (SGLang 0.5.21, a standalone built from
this branch's first commit, d4b86843): the deployment above as
RadixArk/Qwen3.8-27B-NVFP4-BF16-LMHead at 009632fe (23749332688 bytes of
measured weights) with `kv_cache_dtype: fp8_e4m3`, EAGLE 3/1/4 drafting with
the checkpoint's MTP layer and `--enable-mixed-chunk`. CapyCTL passed 950272
KV tokens (29 GiB at 32 KiB) and a 71 GiB static pool; SGLang ran the 22
requests but held 770984 and 776670 KV tokens, and the process took 78.7 to
81.8 GiB at Ready against the 79 GiB request (an 80 GiB Ready charge with
the engine overhead). The engine log's `server_args` were not in the evidence;
the rendered figures are CapyCTL's sizing at d4b86843.

Cause, read in SGLang 0.5.21 and 0.5.20 (the same code): without
`--speculative-draft-model-path`, the draft worker loads the checkpoint as
`Qwen3_5ForCausalLMMTP`. It is built before the target's KV pool
(`Scheduler.init_model_worker`: the draft worker, then `init_memory_pools`),
so the target's profile (`_profile_available_bytes`) charges against the
static pool its MTP layer (in the measured weights) and its own BF16
embedding and LM head, 2 × 248320 × 5120 × 2 bytes = 4.74 GiB, which it frees
only after the target's pool exists (`init_lm_head`). Its KV pool, one
full-attention layer (`_build_hybrid_linear_kv_pool`, layer `[0]`) for the
target's token count in the target's KV dtype, is allocated after the
profile and outside it: the target's `eagle_draft_num_layers` comes from the
target configuration, which names no `num_nextn_predict_layers`. Modelled,
the 71 GiB static pool left the target 804866 tokens; the live figures sit
0.86 to 1.03 GiB below, the draft runner's own allocations, which no figure
here models.

CapyCTL now counts that draft (`EAGLE`, `EAGLE3` or `NEXTN` on a
gated-delta-net hybrid whose configuration names an MTP head, with no
separate draft model): its KV layer shares the KV cache, as a draft model's
layers do (amendment A9), so the tokens passed are 29 GiB / (32768 + 2048) =
894373, and the static pool holds its embedding and LM head (one table when
they are tied; `--speculative-draft-kv-cache-dtype` and `--dtype` apply), 75.44
GiB here, which leaves SGLang's profile 0.7 GiB beyond the target's pool even
with the GiB it was short live. When CapyCTL sizes the state itself the tables
count with the weights, so that deployment without `--max-mamba-cache-size`
is refused at 79 GiB, naming them, and holds its 22 requests at 82 GiB. The
memory request and charge are unchanged: the tables are freed at start-up, so
the margin gets them back. ADR 0014's rule for the static pool on unified
memory states it. A launch without speculative decoding, with a separate draft
model, or of a checkpoint without an MTP head sizes as before.

Tests (`sglang_pool/tests.rs`, T14):
`an_mtp_draft_keeps_its_kv_layer_and_tables_in_the_static_pool` reproduces the
live deployment from the checkpoint's configuration and safetensors headers
(the old figures and SGLang's profile of them, the new 894373 tokens and
static pool, `NEXTN`, `EAGLE3`, a draft KV dtype, the refusal and 82 GiB);
`other_launches_of_an_mtp_checkpoint_size_as_before` pins the unchanged cases
and the tied tables. Both failed before the change and pass after.

CPU and Fake-engine tests only; they are not qualification. Live check still
needed: launch the MTP deployment on SGLang 0.5.21 and confirm the engine log
shows `--max-total-tokens 894373` and `max_total_num_tokens` 894373, the
"Mamba Cache is allocated" and "GDN ReplaySSM ring buffers allocated" lines
(no `intermediate_ssm_state_cache`) match the counted bytes, and the Ready
footprint. With 29 GiB of KV now actually allocated the Ready footprint is
expected to grow by 3.8 to 4.0 GiB over the live run's, above the 79 GiB
request: that deployment needs a larger request or a smaller KV cache.

# Release note: Memory

- **SGLang hybrid models with speculative decoding get the memory they are
  sized for.** A hybrid model (Qwen3.5 and later) launched with
  `--enable-linear-replayssm-spec` no longer has per-draft-token recurrent
  states counted against its memory request, and one drafting with EAGLE from
  its own MTP head now has that draft's KV layer counted in its KV cache and
  its start-up copy of the embedding and LM head in SGLang's static pool, so
  SGLang holds the KV pool CapyCTL passes instead of a smaller one.

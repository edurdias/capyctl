# Status: SGLang ReplaySSM spec-verify no longer counts intermediate states — 2026-10-14 (branch `fix/sglang-replayssm-state`)

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
and golden effective-revision tests pass unchanged. CPU and Fake-engine tests
only; they are not qualification. Live check still needed: launch the
reported deployment on SGLang 0.5.21 and confirm the rendered
`--mem-fraction-static` leaves the 8 GiB margin, SGLang's "GDN ReplaySSM ring
buffers allocated" and "Mamba Cache is allocated" log lines (no
`intermediate_ssm_state_cache`) match the counted bytes, and the KV pool holds
the 475136 tokens passed.

# Release note: Memory

- **SGLang's ReplaySSM spec-verify gets its margin back.** A hybrid model
  (Qwen3.5 and later) launched with `--enable-linear-replayssm-spec` and
  speculative decoding no longer has per-draft-token recurrent states counted
  against its memory request: CapyCTL counts the much smaller replay buffers
  SGLang keeps instead, so the static pool no longer takes the whole request.

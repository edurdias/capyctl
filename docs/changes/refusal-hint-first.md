# Status: A sizing refusal leads with the request to raise to — 2026-10-15 (branch `fix/refusal-hint-first`)

Found live on host A (B18, Qwen3.8-27B with an EAGLE MTP draft and
`--max-mamba-cache-size 60` at a 65 GiB request): the SGLang sizing refusal
(owner decision 2026-10-14) reached `start --wait` and status cut off at
`... more than the 69793218560-byte memory request; raise engin`. Status bounds
an operation's reason to 512 bytes (SPEC §6.4,
`capyctl_domain::diagnostics::MAX_REASON_BYTES`), and the refusal named the
request to raise to last, so the bound removed the one part the operator acts
on.

- Both SGLang sizing refusals (`crates/capyctl-config/src/context_fit/sglang_pool.rs`:
  arguments that fix the state pool, and a hybrid model's recurrent state
  that does not fit) now lead with the request to raise to, then give every
  figure: `raise engine_config.memory.request to "75GiB" (80530636800 bytes),
  keeping memory.kv_cache at 31138512896 bytes: SGLang's static pool holds ...`.
  The live case names 75 GiB (71163366912 + 8589934592 = 79753301504 bytes,
  rounded up to whole GiB).
- A bounded reason is cut on a character boundary and ends in a visible `…`,
  within its bound (`diagnostics::bound_bytes`, `bound_chars`): status's
  operation reason, the unusable-checkpoint diagnostic, a parked instance's
  release reason, a switch's detail, and the live-profile and retirement
  refusals sent to a host. Status picks an operation's hint from the whole
  reason before it bounds it (`diagnostics::public_text`), so a closed code
  past the bound still gives the hint.

Tests (failing before, passing after):
`the_live_eagle_refusal_leads_with_the_request_to_raise` (capyctl-config, the
live figures; the request named is the smallest whole GiB that holds them),
`a_refusal_names_the_smallest_request_that_fits` (now asserts the request
leads), `a_cut_reason_ends_in_an_ellipsis_and_keeps_its_lead`
(capyctl-domain), `a_bounded_reason_keeps_the_request_to_raise_and_shows_the_cut`
(capyctl-store: status keeps the request, ends in `…` within 512 bytes, and
classifies a code past the bound). `wait_ends_at_once_when_the_start_gave_up`
(capyctl-cli) and `a_deterministic_refusal_gives_up_without_retrying`
(capyctl-controller) use the new order. CPU and Fake-engine tests only; they
are not qualification. Live check still needed: repeat the B18 start on host A
and confirm `start --wait` and status name `75GiB`.

# Release note: Fixes

- **A start refused for its memory request says what to raise it to first.**
  SGLang's sizing refusals now begin with `raise engine_config.memory.request
  to "<size>"` and give their figures after it, so status and
  `capyctl start --wait`, which shorten long reasons, never cut it off. A
  shortened reason ends in `…`.

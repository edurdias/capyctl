# Status: TensorFold group readiness reads TensorFold's token ids — 2026-10-15 (branch `fix/tensorfold-group-probe`)

Found live 2026-10-15 (MN3, MN5, MN6): with libnccl fixed, both ranks of a
TensorFold 0.6.5 TP2 group loaded, then activation failed `group_member_failed:
rank 0: the completion probe through the head failed: the head generated no
token`. TensorFold 0.6.5 (609ca419) and 0.6.6 (cb2ebf05) honor
`return_token_ids` on `/v1/completions`, but answer the ids in the response's own
`tensorfold` block (`tensorfold.token_ids`, `src/tensorfold/cuda/server.py:558`,
`cuda/http.py:240-242`), never in `choices[0].token_ids`, where CapyCTL read
them. The head answered text with no ids found, and a group probe needs ids
(ADR 0028 §9).

The `TypeError: cannot unpack non-iterable NoneType object` lines in the rows'
error captures are unrelated to the probe. Each one ends a process's `main()`
(`raise SystemExit(main())`). The capture greps the whole run directory, so its
lines come from every row run there so far, carry no timestamps, and also
appear in the vLLM and SGLang rows' captures (MN4-vllm and MN4-sglang of the
07:13 run). A probe request cannot produce such a trace: TensorFold's request
handler turns any exception into a 500 answer and logs
`[tensorfold] request error`. Neither appeared, and the probe got an answer
(text without ids). No request field CapyCTL sends causes the crash, so
`return_token_ids` stays in the request.

Owner decision 2026-10-15, ADR 0028 §9 amended:

- The completion probe reads TensorFold's ids from `tensorfold.token_ids`,
  falling back to `choices[0].token_ids` (`completion_probe::tensorfold_answer`,
  the one reader of TensorFold completion ids: group readiness, the
  request-stall check and the single-launch `complete_probe` all use it).
  TensorFold group readiness is now judged on token ids, as vLLM's and SGLang's
  is.
- If a TensorFold head's answer carries no ids in either place (a build without
  them in its block), the group readiness, stall and re-proof checks accept its
  non-empty generated text (`group_activation::HeadEvidence::Text`). The
  controller logs a notice naming the deployment when it does. A vLLM or SGLang
  head without ids still fails as before. The wake canary compares ids only
  (`HeadEvidence::into_tokens`): a canary answered with text alone fails the
  wake and records no reference. TensorFold groups are restart-only and never
  wake.

Tests: adapters `tensorfold_asks_for_token_ids_without_a_key` (TensorFold's
0.6.5/0.6.6 response shape, ids in the block) fails before (`tokens: []`) and
passes after; the new unit test `tensorfold_ids_come_from_its_own_block` pins
both shapes (with the block's ids, and without them, which answers text only)
plus the choice fallback and malformed ids. Controller
`tensorfold_readiness_passes_on_text_without_token_ids` (a TensorFold-shaped
fake head answering text and no ids) fails before (the activation never reaches
Ready) and passes after. `vllm_and_sglang_readiness_still_needs_token_ids`
passes before and after. CPU and Fake-engine tests are not qualification. Live
check still needed: the MN3, MN5 and MN6 tensorfold rows must reach Ready, with
no TensorFold text-evidence notice in the controller log (the 0.6.5 head
answers ids).

# Release note: Multi-node groups

- **TensorFold groups become ready.** CapyCTL now reads the token ids a
  TensorFold head returns where TensorFold puts them, so a TensorFold
  multi-node group passes its readiness check instead of failing with
  `the head generated no token`.

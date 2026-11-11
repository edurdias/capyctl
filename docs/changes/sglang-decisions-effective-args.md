# Status: SGLang decision heads retain their native execution mode — 2026-11-10 (branch `fix/sglang-decisions-effective-args`)

The RTX PRO 6000 qualification's Clef-Flash start failed with
`effective_args_mismatch`: CapyCTL pinned `is_embedding=False` on every launch
without `api: embeddings`, but SGLang 0.5.23 resolves Clef joint heads and Kev
pointer heads to `is_embedding=True` for their prefill-only decision path. This
normalization is checkpoint-driven, not a Blackwell backend choice.

Following ADR 0030 §6 and ADR 0036 §2, CapyCTL now renders and pins
`is_embedding=True` only for embeddings deployments, leaving the native mode
at SGLang's checkpoint-selected default otherwise. Decision heads may select
embedding mode, while ordinary chat checkpoints serving label-scored decisions
retain their native mode. Extra arguments still cannot select `is_embedding`,
and an explicit embeddings deployment still refuses resolution out of embedding
mode. API routing and the fixed decision readiness probe are unchanged.

The decision-head regression failed before the fix with the same closed error;
all 51 focused ServerArgs tests pass afterward, including reserved-extra refusal
and embeddings-mode drift. CPU and Fake-engine tests are not qualification. A
live Clef-Flash start and fixed `/v1/systemone` request on SGLang 0.5.23 still
need to be rerun by the live-session owner.

# Release note: Fixes

- Fix SGLang decision-model startup failing with `effective_args_mismatch` when
  Clef or Kev legitimately selects its native embedding execution mode. The
  deployment continues to serve decisions, not the embeddings API.

# Status: vLLM 0.32 qualified for chat; decisions stay refused — 2026-10-21 (branch `feat/vllm-0.32-decisions`)

Owner decision 2026-10-21. ADR 0018 §1: vLLM 0.32.0 joins the verified set for chat.
ADR 0030 §6 amendment: `api: decisions` stays refused on vLLM on every version
(`capability_missing:decisions`), now with the measured reason: vLLM 0.32.0 serves
`POST /v1/systemone` but answers Clef and Clef-Flash from their text output instead of
their decision head; vLLM is admitted once a release uses these models' decision heads.
vLLM 0.32.0's `--grpc-port` (a second listener) is reserved (SPEC §8.2, T21). The vLLM
adapter takes the deployment's API and, for a decisions launch, proves readiness, the
post-wake probe and the wake canary with the fixed decision (ADR 0030 §§3, 4), never a
chat completion; no resolved deployment reaches it while the refusal stands. The host A
venv `~/vllm-0.32.0-venv` was made with `uv venv --python 3.12 --seed --managed-python`
and `uv pip install vllm==0.32.0 --torch-backend=cu130` (torch 2.13.0+cu130).

Tests: T43 (`capyctl-config` decisions: vLLM 0.31.0, 0.32.0, 0.33.0 and a versionless
build are refused with the reason; `vllm_initialize`: a decisions launch is ready only on
the fixed decision, no chat probe, the canary is a decision; `vllm_residency`: a woken
decisions launch is proven by the decision), registration (0.32.0 verified), T14/T21
(`runtime/tests/test_vllm_entry.py`: the 0.32.0 parser launches, `--grpc-port` reserved
however spelled). CPU and Fake-engine tests are not qualification.

Live on host A (GB10), isolated standalone, managed limit 108 GiB:

- Qwen3-4B chat with deep park on vLLM 0.32.0: start 96 s (digest included), chat and a
  stream answered, park freed about 12 GiB, two wakes on request (8.3 s and 9.3 s) passed
  the wake canary (no mismatch recorded), stop 5 s.
- Decisions, measured with a build that let vLLM resolve `api: decisions`: Clef-Flash and
  Clef (BF16) became ready on the fixed decision, answered through the router, and were
  refused `api_mismatch` on chat and `invalid_request` on a malformed body. capyctl-bench
  (Clef-Flash; results in capyctl `target/live/vllm-032/`): API-Bank refused on all 508
  requests ("at most 26 options for this model"), Home appliances 0.0%, GPQA Diamond
  44.4%, Typed Decisions 69.5% with ECE 0.243, AG News 90.2%, BoolQ 89.8%; 1 question
  about a 2k-token text p50 517 ms. On SGLang 0.5.22 the same model scores API-Bank
  93.1%, Home appliances 97.7%, GPQA 51.0%, Typed 70.2% with ECE 0.012. Clef on vLLM
  scored 43.2% on Home appliances (SGLang 79.5%).
- Parking, for the later ADR 0030 §7.2 decision: Clef-Flash run directly on vLLM 0.32.0
  with sleep mode slept at level 2, woke, reloaded its weights and answered the fixed
  decision with identical probabilities.

Live check still needed: none for chat; decisions on vLLM once a release reads these
models with their decision heads.

# Release note: Engines

- **vLLM 0.32.0 is supported for chat**, deep parking included. Decision models
  (`api: decisions`) stay on SGLang 0.5.22 and llama.cpp 0.7.0: vLLM 0.32.0 answers Clef
  and Clef-Flash from their text output instead of their decision head, so CapyCTL
  refuses vLLM for them with `capability_missing:decisions`
  ([Decision models](../guide/deploy.md#decision-models)).

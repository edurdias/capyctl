# Status: TensorRT-LLM stream compatibility — 2026-11-10 (branch `fix/trtllm-probe-terminal`)

**Availability amended 2026-11-11:** this compatibility work is retained for
0.2.x, not TensorRT-LLM support in 0.2.0. ADR 0038's accepted release holdout
refuses it with `engine_unavailable_in_release:tensorrt_llm` (R01, T49).

ADR 0038 §5 and §11: TensorRT-LLM's startup probe and launch-resolved router forwarder accept its native streaming shape without weakening other engines' checks. TensorRT-LLM 1.2.1 omits null `finish_reason` fields and generates an independent ID for each chunk. An omitted finish reason is non-terminal; completion still requires a valid explicit finish reason followed by `[DONE]`. Chunk IDs and the other JSON fields retain their bytes except for the existing model-name remapping; existing SSE padding handling is unchanged. The host ingress already relays the stream unchanged and needs no production change.

The previous TensorRT-LLM test double incorrectly used a constant ID and explicit null finish reasons. A source-derived 1.2.1 fixture now covers the opening role, content, terminal output and usage events, with deterministic IDs, timestamp and text. The readiness test failed before the fix with `engine listed the model but did not answer: chat terminal result unverified`. Coverage also checks collected and streamed forwarding, terminal evidence, malformed chunks and strict behavior for other engine families. TensorRT-LLM 1.4.0rc3 now reuses one response ID, but still omits null finish reasons; its source therefore still needs the absent-field compatibility.

Verification passed under the shared Cargo lock with an isolated target directory: 161 adapter unit tests, 21 engine-contract tests, 11 TensorRT-LLM adapter tests, 55 router-core tests and the workspace formatting check. The fixture's byte-preservation assertion was adjusted to exclude SSE padding outside the JSON, matching existing relay behavior.

CPU and Fake-engine tests are not qualification. The rented GPU was not modified or rerun. Live TR3 remains necessary; its separate explicit 32,768-token context with a 4 GiB Qwen3-4B KV grant cannot satisfy the existing one-window readiness invariant. That configuration must be corrected without relaxing the invariant or silently reducing an explicitly requested production context.

# Release note: Fixes

- **Future TensorRT-LLM readiness and chat streams (0.2.x).** Retained compatibility work accepts omitted non-terminal finish reasons and 1.2.1's per-chunk response IDs. TensorRT-LLM remains unavailable in 0.2.0.

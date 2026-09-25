# F2B Pinned SGLang Adapter Implementation Plan

**Goal:** Implement SGLang through the shared lifecycle, resource, security, and routing contracts, including a mandatory qualified warm-parking recipe.

**Architecture:** Bind one adapter to one immutable runtime. Use authenticated private HTTP controls under durable coordinator steps. Allocation restoration, checkpoint reload, cache validation, and model usability remain separate milestones.

**Tech Stack:** Existing Rust adapters/reqwest/Axum test servers; a small protected Python entrypoint for the pinned SGLang server; existing launcher supervision. No engine installation in deterministic implementation tasks.

**Spec:** [F2 design](../design/milestones/f2-sglang-design.md), §§3/6 and Q11; [A2d coordinator](2026-09-12-f2a2d-coordinator-integration.md), [A3 management](2026-09-12-f2a3-management-and-configuration.md), and F2C live qualification.

## Global Constraints

- “Parking and restoration must be qualified for at least one selected recipe for each engine to close F2.”
- “Security permission and technical qualification are separate checks.”
- “SGLang must not inherit vLLM numeric sleep-level semantics.”
- “No blind repetition of a possibly applied engine operation is allowed.”
- “Readiness stays closed through allocation restoration, weight restoration, required cache invalidation/reset, and the qualified model-usability check.”
- Only host-a is authorized for live qualification. Never access host-b; never change system drivers or an existing engine environment implicitly.

---

## 1. Selected source pin and protocol findings

Plan against SGLang **v0.5.16**, peeled commit
`fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1`. `git ls-remote` resolves its annotated
tag object as `d21f3c3a10606ba3c7bf43f981496da0a7d620cd`; use the peeled commit
for source identity. This is a selected research pin, not a claim that it is
installed on host-a or that its dependencies work there. Record installed package,
Torch/CUDA, memory-saver, kernel, driver, and architecture fingerprints separately.

Pinned source evidence:

- [HTTP controls](https://github.com/sgl-project/sglang/blob/fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1/python/sglang/srt/entrypoints/http_server.py): release/resume await the tokenizer manager. Disk update returns a success field and HTTP failure on unsuccessful update. Flush reports failure when it cannot run. These endpoints use `ADMIN_OPTIONAL` authentication.
- [Worker release/restore](https://github.com/sgl-project/sglang/blob/fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1/python/sglang/srt/managers/scheduler_components/weight_updater.py): release asserts full idleness; KV release flushes cache; weight release preserves static buffers; resume restores allocations/static buffers. This does not establish checkpoint-weight contents without a selected backup or reload policy.
- [Tokenizer control fan-out](https://github.com/sgl-project/sglang/blob/fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1/python/sglang/srt/managers/tokenizer_control_mixin.py): release/resume await their communicators. The first recipe fixes TP=1 and DP=1; multi-rank acknowledgement qualification is not inferred from these calls.
- [Memory-saver adapter](https://github.com/sgl-project/sglang/blob/fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1/python/sglang/srt/utils/torch_memory_saver_adapter.py): a no-op implementation exists when the saver is unavailable. Endpoint success without proving the real saver is enabled cannot qualify release.
- [Authentication](https://github.com/sgl-project/sglang/blob/fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1/python/sglang/srt/utils/auth.py): with both keys configured, admin-marked endpoints require the admin key. Health/metrics prefixes bypass the default key check; protect or disable inference-producing health paths in the launch wrapper.
- [Server arguments](https://github.com/sgl-project/sglang/blob/fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1/python/sglang/srt/server_args.py): memory saver and CPU weight backup are separate flags. `disable_cuda_graph` is not a CLI argument at this pin; use supported per-phase disable flags. Explicit max token-pool sizing is distinct from the static-memory fraction.
- [Control inputs](https://github.com/sgl-project/sglang/blob/fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1/python/sglang/srt/managers/io_struct.py): disk reload supports synchronous operation, non-aborting behavior, and cache flush. Freeze those fields explicitly rather than relying on defaults.

No strong NVIDIA catalog skill matched this custom coordinator/adapter protocol.
Catalog discovery installed no skill and changed no hardware.

## 2. First qualification recipe

Use the existing local **Qwen3-4B-Instruct-2507** checkpoint, independently fingerprinted
at qualification time. TP=1, DP=1, one tokenizer worker, BF16, context 4096, maximum
running requests 8, explicit token-pool cap 4096. Disable prefill/decode CUDA graphs
for the first recipe. Enable memory saver; disable CPU weight backup. No speculative
decoding, LoRA, remote code, disaggregation, external cache, CPU KV offload, or native
gRPC listener in this recipe. These combinations remain explicitly unqualified,
not silently advertised as supported. Native private KV limits and radix invalidation
are included. Additional recipes require their own bounds and evidence.

The token-pool cap is not a byte guarantee. Derive bytes from the pinned checkpoint
architecture, KV dtype, layers, KV heads/head dimension, allocator alignment, and
runtime overhead; compare the effective engine allocation with the committed grant.
Reject an unbounded or mismatched allocator result. Static memory fraction must be
derived from a safe granted cap; never inherit a default that consumes most GPU RAM.

Park sequence: close coordinator dispatch; drain all registered backend work;
persist release intent; `POST /release_memory_occupation` with
`{"tags":["kv_cache","weights"]}`; require acknowledged completion, real saver,
unchanged API/worker identities, and qualified retained bound. Lost reply remains
uncertain. Do not resend release: tags/state are not an idempotent public guarantee.

Wake sequence: reserve full wake peak; persist resume intent;
`POST /resume_memory_occupation` with the same tags; persist a separate reload intent;
`POST /update_weights_from_disk` with this body, using the frozen checkpoint path:

```json
{"model_path":"/qualified/local/checkpoint","load_format":"auto","abort_all_requests":false,"is_async":false,"keep_pause":false,"recapture_cuda_graph":false,"flush_cache":true}
```

Then require reload `success:true`, successful `POST /flush_cache?timeout=0`, exact
runtime identities, and an accounted deterministic model probe before Ready.
No readiness during intermediate HTTP success. This is warm runtime restoration
with disk reread, not a promise to avoid disk I/O or outperform cold initialization.

The parked endpoint remains private and leased. Public status never calls
`/health_generate` or another inference-producing health probe. Model probes run
under the transition grant with dispatch closed and no external request leases.

## 3. Tasks

### Task 1: Strict SGLang launch recipe and private credentials

**Files:** Create `crates/mllm-adapters/src/sglang/{mod.rs,args.rs}`,
`crates/mllm-adapters/tests/sglang_args.rs`, `runtime/sglang_entry.py`,
`runtime/tests/test_sglang_entry.py`; modify adapter `lib.rs` and A3 profile resolver.
**Interfaces:** `SglangLaunch` freezes public launch arguments, binding ID,
checkpoint identity, token/KV grant, and credential references; rendering returns
existing `RenderedCommand`. Actual secrets never enter argv or its recorded DTO.

- [ ] Write launch tests: two bindings differ in endpoint/served-name/secret
  references; all reserved flag aliases are rejected; missing qualification
  prevents ordinary warm policy. Only A3's exact candidate-run authorization
  permits qualification controls; missing real memory saver rejects both modes.
  Run `cargo test -p mllm-adapters
  --test sglang_args`; expect RED.
- [ ] Render the protected entrypoint, not a shell-concatenated command. Load
  credentials from protected files/descriptors, construct `ServerArgs` in Python,
  then call pinned `http_server.launch_server`. Disable argument dumps that could
  include secrets; sanitize startup logging. Restrict to one tokenizer process
  because this pin rejects key authentication with multiple tokenizer workers.
- [ ] Add wrapper auth for inference-producing health routes using this pure gate:

```python
import hmac
import unittest

def private_health_allowed(path: str, authorization: str, inference_key: str) -> bool:
    if not path.startswith("/health"):
        return True
    expected = "Bearer " + inference_key
    return bool(inference_key) and hmac.compare_digest(authorization, expected)

class PrivateHealthTests(unittest.TestCase):
    def test_health_generation_requires_private_key(self):
        self.assertFalse(private_health_allowed("/health_generate", "", "secret"))
        self.assertTrue(private_health_allowed("/health_generate", "Bearer secret", "secret"))
```

  Integrate this gate as middleware, including normalized health paths; real
  inference/admin authentication remains the pinned server's two-key middleware.
  Test HTTP unauthorized calls, not just the pure helper. Do not expose OPTIONS
  as an engine-operation bypass. No public proxy route reaches engine controls.
- [ ] Fail startup if real memory saver is absent for a warm recipe. Keep a
  restart-only binding possible only when explicitly configured and reported.
  Place the helper in `runtime/sglang_entry.py` and import it in the test module;
  importing the entrypoint must not import/start SGLang before the guarded main.
  Run Rust argument tests and `python -m unittest discover -s runtime/tests
  -p 'test_sglang_entry.py' -v` to GREEN. Assert nonzero discovered test count.
- [ ] Commit `feat: render bounded private SGLang runtime recipes`.

### Task 2: Typed control calls and conservative outcomes

**Files:** Create `sglang/http.rs`, `sglang/adapter.rs`,
`crates/mllm-adapters/tests/sglang_control.rs`.
**Interfaces:** Implement A2d's evolved `EngineAdapter` in adapters `traits.rs`.
Its `RuntimeAction`, `RuntimeCommand`, and `RuntimeError` already live there;
controller runtime already re-exports shared types. No move or second lifecycle
trait. Shared completion types live in domain; adapters never depend on
controller/store. Candidate-run permission is coordinator-validated, never an
adapter-wide switch disabling qualification checks.

- [ ] Run an in-process fake HTTP server recording method/path/body and barriers.
  Assert exactly one release call, exact tags, admin credential selection, reload
  false/500/malformed-body rejection, and no Ready before the probe. Run the named
  control test target RED before writing adapter methods.
- [ ] Implement release/resume expecting the pinned successful null/empty JSON
  response shape; reject error objects even under 2xx. Disk reload requires an
  explicit true success. Cap response bodies at 64 KiB and never log raw error
  bodies containing paths/credentials. Disable automatic control retries and
  redirects. Control deadlines: release 60 s, resume 60 s, reload 300 s,
  flush 10 s, model probe 30 s, all capped by operation deadline.
- [ ] Emit ordered shared milestones only after their actual work succeeds.
  Caller timeout, lost acknowledgement, identity mismatch, saver uncertainty,
  or failed model probe returns Uncertain and leaves the peak/gate unchanged.
  `observe_work` may return Unknown; it must never infer Idle from missing metrics.
- [ ] Separate compound Restore into persisted resume/reload/flush/probe substeps
  in coordinator storage. After a lost response, recovery inspects; it never
  blindly repeats a possibly applied substep. Drain all-work completion requires
  a qualified barrier or confirmed terminal results for every request.
- [ ] Test release reply loss, resume reply loss, successful allocation resume
  followed by reload failure, cache-flush failure, duplicate resume rejection,
  worker replacement, and stale operation completion. Run to GREEN; commit
  `feat: implement evidence-gated SGLang park and restore controls`.

### Task 3: Forwarding, capability status, and cross-engine contract tests

**Files:** Create `sglang/forward.rs`, `crates/mllm-adapters/tests/engine_contract.rs`;
modify common terminal parser and management inventory DTO assembly.

- [ ] Run the same terminal-aware forwarding contract from A2d for vLLM and
  SGLang: model identity, request body mapping, ordered stream chunks, malformed
  SSE, premature close, cancellation uncertainty, and nonstream collection.
- [ ] Forward only to immutable binding endpoint with its inference credential;
  rewrite backend model name to the qualified served-name when required while
  preserving public route identity. Do not forward arbitrary upstream paths.
- [ ] Publish four eligibility states (`qualified`, `unknown`, `unsupported`,
  `disabled`) with reason and evidence reference. Require both qualification and
  host security permission for parking. Source pin or presence of an endpoint
  does not set Qualified. Store qualification against all effective fingerprints.
- [ ] Assert changing checkpoint, arguments, saver/Torch/CUDA, or hardware identity
  invalidates evidence. Same-engine and mixed-engine tests retain separate
  bindings and use one scheduler. Run adapter and management tests to GREEN.
- [ ] Commit `feat: expose SGLang through shared serving and capability contracts`.

### Task 4: Publish adapter readiness and consume F2C qualification evidence

**Files:** Create `docs/runbooks/f2-sglang-qualification.md`. F2C alone executes
live runs and owns versioned evidence under `.context/f2-qualification/`.

- [ ] Document adapter prerequisites and deterministic readiness results: protected
  wrapper, actual discovered Python tests, typed controls, evidence milestones,
  conservative grants, and candidate-run authorization. No GPU launch in B.
- [ ] Link F2C Tasks 3–5 as the sole executor of preflight, single-engine checks,
  mixed-engine preparation/coexistence, warm switching, and recovery. Missing
  software or memory saver becomes its explicit blocker, never an implicit install.
- [ ] After F2C completes, consume its existing case IDs and evidence references;
  do not repeat the runs or copy raw artifacts. Only the exact passing recipe
  becomes Qualified through A3. Keep F2 open if either warm recipe lacks evidence.
- [ ] Commit the readiness runbook independently; update evidence references
  only after F2C verification. Never commit checkpoints, credentials, or bodies.

## 4. Remaining qualification uncertainty

Pinned source establishes candidate protocol behavior, not ABI compatibility,
allocator bounds, deep-release efficacy, or post-wake correctness on host-a.
Those are explicit live gates. A failed selected recipe may require another
reviewed pin or a separately accounted backup strategy; do not silently switch
policy or label an allocation-resume response as successful warm restoration.

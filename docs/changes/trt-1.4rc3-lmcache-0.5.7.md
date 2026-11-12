# Status: TensorRT-LLM rc3 and LMCache 0.5.7 compatibility — 2026-11-10 (branch `engines/trt-1.4rc3-lmcache-0.5.7`)

**Availability amended 2026-11-11:** TensorRT-LLM compatibility is retained
for 0.2.x, not support in 0.2.0. ADR 0038's accepted holdout refuses it with
`engine_unavailable_in_release:tensorrt_llm` (R01, T49). LMCache is unaffected.

ADR 0038's 1.4 audit now targets v1.4.0rc3 (`9e334fb`), retaining
1.2.1 and the 1.3 pre-release tables. The renderer owns and rechecks one
serving frontend on 1.3/1.4, omits the absent option on 1.2.1, and reserves
the new OpenEngine metrics listeners and frontend-affinity port stride.
The install guide includes the separate rc3 torch/cu132/toolkit tuple and
the pinned pip-MPI recipe. Wheel inspection found that mpi4py 4.1.2 searches
the venv's `lib` directly and Open MPI 4.1.8 uses origin-relative RUNPATH;
no inherited loader-variable permission was added.

ADR 0034's installation-shape check accepts LMCache 0.5.7's identity-backed
salt and SGLang key factory without accepting a dropped salt. Versioned
real-source fixtures cover 0.5.6 and 0.5.7. The ADR distinguishes 0.5.7's
typed engine-driven tensor envelope from 0.5.6's pickle transport and
requires separate tag-source builds against each engine's installed torch;
0.5.7 is not published on PyPI.

Targeted checks passed: config TensorRT unit tests (9), config TensorRT
fixtures (9), adapter TensorRT argument fixtures (9), LMCache installation
fixtures (21), LMCache shape unit tests (4), and `cargo fmt --all -- --check`
(after correcting formatting). They cover version tables, owned frontend
rendering/rechecks, reserved surfaces, and salt-preservation source shapes.
CPU and Fake-engine tests are not qualification. Live checks still owed:
TensorRT-LLM 1.2.1 and rc3 loader/MPI identity, singleton process ownership,
loopback listeners, startup/readiness and KV-pool proof, cancellation and
restart-only settlement; LMCache 0.5.7 with both engine/torch tuples,
applicable transfer modes, salt isolation, cache reuse after restart, and
repeated deep/host-backed park/wake correctness and memory release.
No qualification was marked passed and no verified entry was added.

# Release note: Fixes

- **Engine source compatibility.** TensorRT-LLM 1.4.0rc3 frontend work is
  retained for 0.2.x; TensorRT-LLM is unavailable in 0.2.0. LMCache 0.5.7's
  salt-preserving source layout passes installation checks. Neither native
  stack is qualified by these changes.

# Status: Re-prove retained custom extraction engines — 2026-11-24 (branch `fix/custom-extraction-readoption`)

SPEC §§4.3, 6.1 and 13.2, ADR 0031 §5 and ADR 0041 §4 require a retained custom extraction engine to pass a fresh extraction probe before dispatch reopens after a standalone role restart. The standalone readiness supervisor rebuilt its adapter without the frozen manifest's accepted input kinds. The probe therefore refused the empty input set before sending a request, leaving the engine owned and charged with dispatch closed. Re-proof now uses the manifest's effective inputs, as initial launch and host reconnection already do; no operator resume is required.

The T33/T44/T50 regression uses a Fake engine on loopback to exercise standalone re-proof with text-only, image-only, PDF-only and omitted input declarations. It checks the launch key, health-before-extraction order and exact fixed request, and rejects a malformed extraction answer. CPU and Fake-engine tests are not qualification. A native role-restart qualification run is still required; no live run was made for this fix.

Before the fix, the regression failed with the same readiness refusal as the retained engine: `the engine did not answer the fixed extraction probe: the engine does not implement this capability`. After the fix, all 10 local-readiness tests passed.

# Release note: Fixes

- **Custom extraction engines recover after a standalone role restart.** Retained engines can pass their fresh readiness probe and resume dispatch without being stopped and relaunched. The probe uses only an input kind accepted by the engine's frozen manifest.

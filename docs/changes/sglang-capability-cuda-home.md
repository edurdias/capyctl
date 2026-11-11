# Status: Probe SGLang with its selected CUDA toolkit — 2026-11-10 (branch `fix/sglang-deep-capability`)

ADR 0008, ADR 0018 and SPEC §9.1: registration and launch/Park capability probes now receive the same selected `CUDA_HOME` that the profile records and its launch renders. The probe still clears the ambient environment and runs Python with `-I -S -B`; no inherited loader paths, preload libraries or Python hooks are admitted.

On the rented RTX PRO 6000 (sm_120), SGLang 0.5.22/0.5.23 imports DeepGEMM while loading its HTTP routes. DeepGEMM asserts when it cannot find a CUDA toolkit. The probe had discarded the selected wheel-installed toolkit, so it incorrectly reported all four deep-park routes missing even though torch-memory-saver 0.0.10 imported successfully. SGLang's sm_80 branch skips that import, explaining why the A100 P3 rows passed. The A100 EM9 functional cycles passed, but its overall result remained a skip because parked-memory measurements were not implemented.

The registration regression reproduced `deep_park: disabled` / `deep_park_probe: capability_missing` before the fix for the recorded-toolkit scenario. CPU regressions cover both SGLang versions, the admission registry and the cleared environment. A read-only RTX import control changed only `CUDA_HOME` and restored all four capabilities (`core`, `deep_park`, `metrics`, `observation`). CPU and Fake-engine tests, and an import-only control, are not native lifecycle qualification; the P3 and EM9 lifecycle rows still need a rerun with the corrected binary and re-registered profiles.

# Release note: Fixes

- **SGLang deep parking with a wheel-installed CUDA toolkit.** Engine registration and parking admission now probe with the profile's selected CUDA toolkit, avoiding false missing-capability reports on GPUs whose SGLang imports require it, including Blackwell sm_120.

# Status: Nemotron 3.5 Super VL is not parked on vLLM — 2026-10-16 (branch `fix/supervl-deep-wake-refusal`)

In the recipe runs for Nemotron 3.5 Super VL (NVFP4) on vLLM 0.31.0, every deep park's wake
failed CapyCTL's wake canary (`wake_mismatch`), with and without a drafter, so the model was
stopped after each wake. vLLM alone reproduces it on host A, with no CapyCTL: served with
`--enable-sleep-mode` (8k context, one sequence), a greedy prompt answered the same prose twice;
after `/sleep?level=2`, `/wake_up` and `reload_weights` (the deep park and wake CapyCTL performs)
the same prompt repeated `</think>` to the token limit, twice. The defect is in vLLM's sleep and
reload of this model, not in CapyCTL; the canary caught it as designed.

The checkpoint's family (`model_type: nemotron_h_omni`, architecture
`NemotronH_Omni_Reasoning_V3`) joins gpt-oss in `DEEP_WAKE_BROKEN_FAMILIES` for vLLM, so a
parking deployment of it is refused at deploy with `capability_missing:deep_park` instead of
failing at its first wake. `residency: restart_only` serves it, as the recipe does. The
text-only Nemotron H family (`nemotron_h`) is not affected by the rule. The parking guide now
lists the models CapyCTL refuses to park.

Test (T21, T22): `nemotron_super_vl_refuses_parking_on_vllm_only` in
`crates/capyctl-config/tests/deep_wake_refusal.rs` (fails before, passes after). CPU tests only;
not qualification.

# Release note: Engines

- **Nemotron 3.5 Super VL is not parked on vLLM.** vLLM 0.31.0 answers wrongly after it parks and
  wakes this model, so CapyCTL refuses `deep` and `host_backed` for it; declare
  `residency: restart_only`.

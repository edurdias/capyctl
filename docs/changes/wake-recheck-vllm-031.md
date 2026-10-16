# Status: gpt-oss wake rechecked on vLLM 0.31.0; 0.1.4 status entry — 2026-10-16 (branch `fix/deep-wake-families-vllm-031`)

gpt-oss-20b was served by vLLM 0.31.0 alone on host A with sleep mode, put to sleep at level 2, woken and its weights reloaded (the deep park and wake CapyCTL performs). The reload logged `OAIAttention: Failed to load weights` for each of the 24 attention layers, as on 0.30.0, and the engine failed its first request and shut down. The gpt-oss family rule in `DEEP_WAKE_BROKEN_FAMILIES` now records both vLLM versions; its behavior is unchanged. The same check of Gemma 4 26B-A4B NVFP4 still answered garbled text after the wake, while the BF16 Gemma 4 E2B answered identically before and after, so no family rule was added for Gemma: the fault is with that checkpoint and the wake canary catches it. The status runbook gains the 0.1.4 progress entry.

# Release note: none

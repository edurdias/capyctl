# Status: vLLM share precision — 2026-11-09 (branch `fix/vllm-share-precision`)

ADR 0019 §3: vLLM's discrete-device utilization now rounds upward to ten decimal places instead of a whole percent, retaining the derived-request floor and the 0.99 ceiling. The integer calculation avoids overflow and bounds rounding overhead below the 1.25 GiB first-start allowance even for an i64-sized card. No frozen configuration schema changes: recorded revisions decode and re-render with the corrected precision. The T26 regression for a 180 GiB card and 135.125 GiB declared request failed before the fix (136.8 GiB rendered versus 136.375 GiB charged) and passes afterward. CPU and Fake-engine tests are not qualification; a live vLLM 0.32 start with the declared request and 136.5 GiB free still needs checking.

# Release note: Fixes

- **vLLM starts on large cards:** declared memory requests no longer round up to a whole percent of the card, which could exceed the admitted first-start footprint and make vLLM refuse to start.

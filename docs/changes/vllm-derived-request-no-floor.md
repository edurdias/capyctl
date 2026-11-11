# Status: vLLM derived device requests without the 75% floor — 2026-11-09 (branch `integration/0.2.0-on-0.1.11`)

Owner decision 2026-11-09, issue #225, ADR 0019 §§3–4: remove the historical
75% card floor from vLLM's derived discrete-GPU request and its rendered
`--gpu-memory-utilization`. Both a derived request and a declared request now
use their own card share, rounded up to ten-billionths between 0.01 and 0.99.
Standalone's generated request follows the same weights × 1.10 plus KV rule
as a request resolved after checkpoint measurement. Explicit KV bytes, the
CUDA-context charge, managed limits and unified-memory sizing are unchanged.
Existing frozen revisions retain their accepted memory request; deploy a new
revision to derive the smaller request.
The frozen marker survives checkpoint refresh and the remote launch plan.
A new no-floor plan requires the host's `vllm_device_request_no_floor`
capability; an older host is refused before any command is sent. An absent
plan marker keeps the legacy derivation on an upgraded host. Declared
requests and unified-memory hosts need no new capability.

Tests were updated before implementation (T14, T26): the small-model template,
the minimal discrete deployment, the measured-checkpoint derivation, and the
frozen launch's exact request, Ready charge and rendered share. CPU and
Fake-engine tests are not qualification.

**Pending live confirmation on vLLM 0.32.0 and 0.33.0.** The historical vLLM
0.29 start failure on a 16 GB card has not been explained. The separate live
work must confirm launch, explicit KV sizing and co-residence below 75%;
functional checks on unified-memory hardware do not qualify a discrete card.
No timing or memory measurements are recorded beside the owner's training.

# Release note: Memory

- **A small vLLM model no longer reserves at least 75% of a discrete GPU.**
  When CapyCTL derives its request, it uses the weights plus the KV cache and
  engine allowance, and tells vLLM that request's share of the card. Declared
  requests work the same way. Pending live confirmation on vLLM 0.32.0 and 0.33.0.

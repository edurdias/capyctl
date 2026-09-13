# mllm handoff package — revision 0.2

**Repository status (2026-09-12):** the original design package below now sits
alongside the F0/F1 implementation. For current status, read
[`design/milestones/f1-open-items.md`](design/milestones/f1-open-items.md) and
[`runbooks/spark-model-size-qualification.md`](runbooks/spark-model-size-qualification.md).
F2 SGLang is the next milestone; the design-package revision is not a software
release or a hardware qualification claim.

Start with **`AGENT_HANDOFF.md`**, then read **`SPEC.md`** before implementation planning.

## Contents

| File | Purpose |
|---|---|
| `SPEC.md` | Consolidated architecture, requirements, interfaces, resource rules, configuration, milestones, and 40 acceptance scenarios. |
| `AGENT_HANDOFF.md` | Coding-agent brief and implementation/verification boundaries. |
| `examples/server.yaml` | Explicitly networked server configuration sketch. |
| `examples/host.yaml` | Host aggregate boundaries and approved runtime profiles. |
| `examples/deployment-single.yaml` | Single-host resource and lifecycle contract. |
| `examples/deployment-multinode.yaml` | Two-host resource contract with a private host-cache tier. |
| `examples/standalone.yaml` | Embedded local server/host shape with safe unresolved defaults. |
| `DOCUMENT_CHECKS.txt` | Results of document and example consistency checks, not software or hardware tests. |

Revision 0.2 supersedes `mllm-initial-design.md` revision 0.1 and incorporates the subsequent design decisions. The old file is not repeated in the bundle to avoid conflicting instructions.

## Verification boundary

The original package's checks covered design documents and illustrative
configuration only. The repository now also contains implementation and tests;
their evidence is reported separately in the runbooks. YAML syntax checks do
not establish live-engine correctness, and installed engines, secrets and model
weights are not part of this documentation package.

Names, addresses, paths, byte budgets, and durations are examples. Source-backed upstream behavior is referenced inside the specification and still requires verification against the exact build selected for implementation.

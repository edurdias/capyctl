# mllm handoff package — revision 0.2

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

The package contains design documents and illustrative configuration only. No mllm source implementation, schemas, installed engines, secrets, model weights, or production-qualified recipes are included. YAML examples were parsed for syntax and checked for consistency with their copies in the specification; that is not validation against an implemented mllm schema or a live engine.

Names, addresses, paths, byte budgets, and durations are examples. Source-backed upstream behavior is referenced inside the specification and still requires verification against the exact build selected for implementation.

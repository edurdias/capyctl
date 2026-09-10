# ADR 0006 — License: Apache-2.0

**Status:** Accepted (2026-09-10, owner approval)
**Context:** SPEC §19 and AGENT_HANDOFF require obtaining the project owner's license
choice before publication; no assumed license grant is permitted.

## Decision

mllm is released under the Apache License 2.0.

## Rationale

- Owner's explicit choice, satisfying the spec's gate.
- Explicit patent grant and permissive terms suit an infrastructure controller intended to
  interoperate with independent engine ecosystems (vLLM, SGLang, cache backends).

## Consequences

- The `LICENSE` file is added before any publication; until then the repository remains
  private/all-rights-reserved by default behavior of git hosting.
- Third-party notices (Rust crates, vendored material) are tracked per normal Apache-2.0
  attribution practice when packaging begins (F5).
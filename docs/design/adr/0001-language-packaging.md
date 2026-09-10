# ADR 0001 — Language and packaging

**Status:** Accepted (2026-09-10, owner approval)
**Context:** SPEC §19 lists "Language/package: Rust; one executable per platform" as a
baseline proposal requiring a recorded decision before scaffolding.

## Decision

mllm's long-lived components (server, host agent, CLI) are implemented in Rust. One
executable per supported OS/architecture supplies all roles (`server`, `host`,
`standalone`, CLI actions). Linux ARM64 and x86-64 are the first validation targets.

## Rationale

- Distribution: a static-ish single binary simplifies enrollment of heterogeneous hosts
  (the agent must run on every directly managed host with minimal setup).
- Separation from engine environments: engines remain external processes with their own
  Python environments; mllm never imports engine runtime libraries to control them.
- Long-lived controller workloads (streams, supervision, reconciliation) suit Rust's
  asynchronous I/O model.
- No unmeasured speedup over Python is asserted (SPEC §3.3).

## Consequences

- Python is limited to external launch helpers and integration tests; it is not a required
  server runtime.
- A single binary is not one universal binary; system libraries may be used. GPU telemetry
  support must be optional on server-only machines.
- macOS roles and Metal-backed engines require their own later validation.
- Avoiding an unapproved Python/Rust split; any future split goes through this register.
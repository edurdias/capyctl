# ADR 0003 — Remote control transport: versioned gRPC with mutual TLS

**Status:** Accepted (2026-09-10, owner approval)
**Context:** SPEC §19 lists "Remote control: versioned gRPC + mutual TLS; agent-initiated
session" as a baseline proposal. gRPC's bidirectional streaming and TLS client
authentication are the cited building blocks (SPEC §4.2, [S8], [S9]).

## Decision

Host-agent management uses versioned gRPC over mutual TLS. The agent initiates its
management connection; the server sends commands over that session. Bootstrap enrollment
uses a narrowly scoped, server-authenticated bootstrap endpoint (server TLS plus
invitation authorization), distinct from enrolled-agent mutual TLS (SPEC §4.1).

## Rationale

- Bidirectional streaming matches the contract: an outbound agent connection carrying
  server-initiated commands, inventory reports, and operation results.
- TLS client authentication maps directly to revocable, renewable host certificates
  (SPEC §4.1).
- Strong typing and versioning support the schema-compatibility rejections the agent must
  make (SPEC §13.1).

## Consequences

- F0 freezes proto field numbers and bootstrap authorization semantics (SPEC §14 forbids
  inferring them from sketches).
- gRPC is for the management plane only. The inference surface remains HTTP
  (`/v1/models`, `/v1/chat/completions`) with its own authenticated identity/roles
  (SPEC §13.3). Management RPCs never tunnel inference payloads.
- Reconnection semantics: retry with backoff, reconnect after reboot without creating a new
  host record, at-least-once delivery with replay of known results (SPEC §13.1).
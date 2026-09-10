# ADR 0002 — Persistence: embedded transactional SQLite

**Status:** Accepted (2026-09-10, owner approval)
**Context:** SPEC §19 lists "Persistence: embedded transactional server store, local agent
journal; SQLite candidate" as a baseline proposal.

## Decision

The server uses an embedded, transactional SQLite database as its durable store
(deployments, operations, generations, reservations, qualification evidence). Each host
agent maintains a local journal for accepted operation state and ownership evidence.
Durable acceptance (SPEC §6.4: persist the deployment record and accepted operation
transactionally before returning its ID) is implemented as a SQLite transaction.

## Rationale

- Operational simplicity: no external broker or database server to enroll or operate; the
  standalone role runs with zero infrastructure.
- SQLite provides real ACID transactions, which the durability requirements (idempotent
  retries, no duplicate deployments on lost responses, T08/T09) require.
- One file per state dir supports owner-protected paths and atomic creation (SPEC §15.2).

## Consequences

- The F0 design must specify: atomic acceptance transactions, recovery on restart,
  schema migrations, and backups (per SPEC §19 required action).
- Concurrent multi-controller operation is out of scope (SPEC §13.2: single active
  controller); SQLite's single-writer model matches this.
- Journals have bounded retention and never hold inference bodies (SPEC §13.1).
- If a later milestone needs a different store, the migration goes through this register.
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

## Amendment (owner decision 2026-10-18): the standalone request ledger

Every write is committed with SQLite `synchronous=FULL`, stated when the store
opens, with one exception. A standalone role commits its request-lease grants
and closes (SPEC §10) with `synchronous=NORMAL`, and its connection returns to
`FULL` before any other write. In WAL mode a process crash loses no committed
transaction; an OS crash or power loss can lose the last ones. On a standalone
role that loss also ends every engine those leases account for, since they run
on the same machine, so no request runs unaccounted, and the next session
settles whatever leases remain through engine quiescence. Deployments,
operations, epochs, reservations and every other durable record stay `FULL`
on standalone too: they carry promises made off the machine (SPEC §6.4: a
deployment or operation ID returned to a client must survive a power loss, so
that a retried request finds it rather than creating a second one, T08 T09),
and they are written by management actions, not on every request, so `FULL`
costs the request path nothing. The control-plane server keeps `FULL` for every write: its engines
run on other hosts, which a power loss on the server does not stop, and a lost
grant would leave one of them running a request no lease accounts for.
Measured on host A's NVMe: one commit is 4.6 ms p50 with `FULL` and 0.01 ms
with `NORMAL`.

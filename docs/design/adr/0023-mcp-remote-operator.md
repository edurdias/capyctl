# ADR 0023 — Remote operator access over MCP

**Status:** Accepted (owner decisions, 2026-10-01).
**Amends:** `SPEC.md` §13.3 (an operator credential distinct from the admin token and the
inference key may reach operator routes from the network through `capyctl mcp`), §16.5 (the
MCP listener's bind when it runs), §20 T37, and adds T41.
**Related:** ADR 0017 (version skew), ADR 0019 (network inference endpoint; its §10 kept
management loopback-only), ADR 0021 (terminal output).
**Design:** `docs/plans/2026-10-01-001-feat-mcp-server-plan.md`.

## Context

CapyCTL is operated through the CLI on the control host, or through `ssh` plus the CLI from
elsewhere (ADR 0019 §10). AI agents that speak the Model Context Protocol (MCP) usually run
on the owner's laptop, not on the control host, and have no supported way to reach CapyCTL.

The owner's first questions for such an agent are operational: what is running, how loaded
a host is, and what could be freed. The answers exist only as pieces: live domain capacity in
the hosts view, reserved bytes in the ledger snapshot, and request activity inside the
coordinator and router. Neither the CLI nor an agent can answer the question without joining
them by hand.

SPEC §13.3 requires that remote control and ingress use distinct authenticated identities.
The inference key is the ingress identity and cannot authorize control. The admin token is
the root credential; putting it on the network would let one leaked secret enroll hosts,
revoke hosts and operate every deployment, with no way to revoke it per client.

## Decision

### 1. A third credential: the operator key

The server holds three pairwise-distinct credentials: the admin token, the inference key,
and a new operator key. The management API authenticates every request to a role, `admin`
or `operator`. The operator role may use every route except host invitation, host revoke and
operator-key rotation, which answer `operator_not_permitted` (403). The server enforces the
role, so the boundary holds even for a client that bypasses `capyctl mcp`. The inference key
authorizes no management or MCP request.

### 2. The operator key is created by the server and stored owner-only

Both roles create the operator key on start when it is missing, in its own owner-only file
under the private identity directory, with the same ownership and mode rules as the other
credentials (§13.3). Existing installs get a key on their first start after upgrade. The
key is never logged, never returned by a route or tool, and printed only by
`capyctl mcp config` at the owner's request. Start fails when the three credentials are not
pairwise distinct; there is no fallback key.

### 3. Rotation without a restart

An admin-only management route rotates the operator key: it writes a new key atomically and
replaces the accepted digest in memory, so the old key is refused on the next request.
`capyctl mcp rotate-key` calls it and restarts a background MCP process if one is running.

### 4. `capyctl mcp` is a separate process

`capyctl mcp` runs an MCP server over HTTP (the streamable HTTP transport) as its own
process on the control host, in the foreground by default and in the background with
`--background`. It reaches CapyCTL only through the loopback management API with the
operator key, never through engine listeners or the inference router. The server, its
loopback management listener and its listener defaults are unchanged. When `capyctl mcp` is
not running, no MCP or remote management surface exists.

### 5. The MCP listener

Every MCP request must carry the operator key as a bearer token, checked in constant time
before any MCP processing. The listener caps request bodies and concurrency and validates
`Origin` when present. It binds `0.0.0.0:7444` by default, because running the command is
the opt-in and the key is mandatory; `--listen` or `CAPYCTL_MCP_LISTEN` changes it, and a
loopback address keeps it local. CapyCTL does not terminate TLS for it: on a non-loopback
bind it prints once at start that the endpoint is plain HTTP, and exposure beyond the LAN
goes through a TLS reverse proxy or a tailnet, as for the inference endpoint.

### 6. Tools mirror the management API

MCP tools map one to one onto management API operations: the operator read views, the new
usage view, operation reads, deploy and update, the lifecycle actions the API accepts
(start, stop, park, preinitialize, delete) at deployment and instance level, and host drain.
There are no tools for host invitation, host revoke, key rotation, engine registration or
inference. Tool results carry the API's identifiers, states and error codes unchanged.

### 7. Actions wait with a deadline and never replay

Action tools keep the API's idempotency key, expected revision and durable acceptance. A
tool waits until the operation is terminal or the caller's wait limit passes, then returns
the outcome or "in progress" with the operation identifier; a wait tool resumes. Waiting
polls operation state and never resubmits. A repeated call with the same idempotency key
returns the original operation.

### 8. The usage view lives in the management API

A new usage view reports, per host memory domain, capacity, observed available, reserved and
unreserved bytes, and per deployment and instance the bytes held in each domain, the
lifecycle state, requests in flight and time since the last request. Unknown or stale
figures are reported as unknown with a reason, never as zero. `capyctl usage` presents it in
the terminal and the MCP usage tool returns it, so both show the same numbers.

## Consequences

Agents on the owner's network can operate CapyCTL without `ssh`, and the CLI gains a usage
command that answers the same question. ADR 0019 §10 still holds for the management
listener itself, which stays on loopback; remote operator access exists only while
`capyctl mcp` runs.

The operator key is a network credential with full deployment-operator power. Anyone who
holds it can stop, park, delete and deploy, so docs treat it like the admin token and
recommend a tailnet or TLS proxy for anything beyond a trusted LAN. Host trust (invitation
and revoke) and key rotation stay admin-only.

A new dependency, the official Rust MCP SDK, enters the workspace and the packaging check.
The MCP process ships in the same binary as the CLI, so it shares the server's release on
the control host; it still checks the management API version it receives (ADR 0017).

A future web app is expected to reuse the operator role and this access model.

## Verification

T37 gains: the operator key is refused on admin routes; the MCP listener refuses requests
without the operator key, including ones carrying the inference key or the admin token; the
key never appears in responses, tool results or logs. T41 covers remote operation: usage
figures from the CLI and MCP match, action tools wait with a deadline and resume, and a
repeated idempotency key does not replay an action.

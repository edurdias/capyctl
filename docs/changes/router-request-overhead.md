# Status: Router per-request overhead trimmed; ledger unchanged — 2026-10-18 (branch `perf/router-overhead`)

Measured live on host A on 2026-10-18 (llama.cpp v0.7.0, decision requests
one at a time): CapyCTL's router added about 12–26 ms per `POST /v1/systemone`
over the same engine addressed directly. Reproduced on a laptop
(i9-14900HX, ext4 on NVMe, release build) with a standalone role on the Fake
installation and an HTTP stub engine at the launch's recorded endpoint that
answers at once, through the production listener, admission gate, router,
request ledger and engine forwarder; the router added 5.8–6.0 ms (p50).
Per-phase timings on that reproduction (p50, before):

| phase | ms |
|---|---|
| request lease grant: one group commit (fsync ≈ 1.1 ms) plus the writer thread round trip | 1.66 |
| request lease close, before the answer is returned: one commit | 1.42 |
| store reads: route, `serving_instances` read twice, forwarder endpoint (engine key, routes, ingress, engine) | ≈ 2.1 |
| forward to the engine over the pooled keep-alive connection | ≈ 0 over direct |
| body read and parse, strict answer parse, spawn and join, timing, selection log line | < 0.1 |

Connection reuse to the engine works (the forwarder is cached per
incarnation, reqwest pools the connection; llama.cpp's HTTP server sets
`TCP_NODELAY`, and a keep-alive connection to a local llama-server was
measured faster than a fresh one). The router answers in one `writev`, so
Nagle's algorithm costs nothing on loopback today.

What changed, with no change to what the ledger records, when, or with what
durability (SPEC §10, T17, T38):

- **Store** (`capyctl-store`): the statements the router reads on every
  request and the request-ledger statements are prepared once per connection
  (`prepare_cached`, cache capacity 64). A cached statement is reset after
  use and re-prepared by SQLite after a schema change.
- **Router** (`capyctl-router`): a request admitted without waiting is
  ranked on the instance view admission already read (`InstanceView`), not
  on a second read of the same rows. The lease grant still re-checks the
  gate in its own transaction (T18), and any request that waited reads the
  instances afresh. `router_selection` now times the read only when it
  happens.
- **Listeners** (`capyctl-cli` `shutdown::serve`): every accepted inference,
  management and host-ingress connection has `TCP_NODELAY` set; axum 0.8 no
  longer sets it. Test: `served_connections_have_nagle_off`.

After, on the same reproduction: three alternating rounds of 400 requests
per case on a quiet machine, the router's added p50 went from 3.20–4.08 ms
(mean 3.66) to 3.00–3.50 ms (mean 3.28); in earlier runs on a busier
machine, from 5.6–6.4 ms to 4.25–4.85 ms. Planning a request now costs
0.05 ms instead of 0.5 ms, and the store reads on the way 0.9 ms instead of
2.1 ms. The remaining overhead is the two synchronous ledger commits (about
1.1 ms each here, one `fsync` of the write-ahead log) and their writer round
trips. The owner took all three options on 2026-10-18 (see "Request ledger
commits off the answer's path"): `synchronous=NORMAL` for the standalone
request ledger, closing the lease after the answer is sent, and indexes on
`lifecycle_steps(binding_id)` and `journal_entries(operation_id)` (the
instance read scanned both tables, about 1.2 ms per 1000 initialize
operations of history).

CPU and Fake-engine tests are not qualification. Live check still needed:
repeat the host A decision-request comparison (Julia-1 and Clef-Flash, one
and three questions, about 200 and 2k tokens of state) with this build.

# Release note: Requests

- **Lower router overhead per request.** The router no longer reads a
  deployment's instances twice per request, reuses its prepared store
  statements, and turns Nagle's algorithm off on its listeners. On a local
  reproduction this removed about a quarter of what the router added to a
  decision request; the request ledger's guarantees are unchanged.

# Status: Request ledger commits off the answer's path — 2026-10-18 (branch `perf/ledger-commits`)

Owner decision 2026-10-18 (SPEC §10 note of that date), following the
router-overhead measurements: two synchronous SQLite commits per request
(lease grant, lease close) were most of what CapyCTL added to a decision
request. One commit on host A's NVMe took 4.6 ms p50 and 6.8 ms p95 with
`synchronous=FULL`, and 0.01 ms with `NORMAL`. The owner chose three
changes:

- **Answer before the lease close** (`capyctl-router`). A completed
  non-streamed answer (chat or `POST /v1/systemone`) is handed to the client
  before its lease close is committed; the dispatch task then closes the lease
  on the same completion evidence and only after that releases the request's
  in-flight slot. A completed stream sends its timing and metrics comments and
  `[DONE]` while the close commits beside them, so a stalled client does not
  hold the slot. Refusals, engine errors, uncertain ends and failover are
  unchanged: their leases are settled before the answer, as before. Cost: a
  client's next request can briefly find the previous one still counted
  against the deployment's bound, and a crash between answer and commit
  leaves the lease charged for the quiescence path.
- **`synchronous=NORMAL` for the standalone request ledger**
  (`capyctl-store`, `capyctl-controller`, `capyctl-cli`). The store states
  `synchronous=FULL` when it opens. A standalone role opens its owned state
  `with_local_engines_only()`, and its request-lease batches alone commit
  with `NORMAL`; the connection is back to `FULL` before any other write uses
  it, whether the batch commits or is refused. In WAL mode the commit order
  holds, so the next `FULL` commit makes earlier batches durable too. A
  process crash loses no lease write; an OS crash or power loss that loses the
  last ones also ends the engines on that machine whose work they account
  for. The control-plane server, whose engines run on other hosts, keeps
  `FULL` for every write. Test: `a_local_lease_batch_leaves_other_writes_fully_synchronous`.
- **History indexes** (store schema v50): `lifecycle_steps(binding_id)` and
  `journal_entries(operation_id)`. The router's serving-instance read now
  searches both instead of scanning them (checked with `EXPLAIN QUERY PLAN`);
  results are unchanged. Without them the read cost about 1.2 ms per 1000
  past activations. v49 (ADR 0030) already makes this release's store
  unreadable by 0.1.4, so v50 adds no new downgrade limit.

Measured on the laptop reproduction (i9-14900HX, ext4 on NVMe, release
build, standalone role on the Fake installation with an HTTP stub engine,
decision requests one at a time), three alternating rounds of 400 requests
per case: the router's added p50 went from 4.2–4.8 ms (PR #150 alone) to
1.8–2.0 ms. Over 100 requests per case the process made 78 `fsync` calls
instead of 1117.

The ledger durability is recorded as an amendment to ADR 0002 and in the
SPEC's amendment list and T17 row. Tests (CPU, tagged T17 or T33):

- `the_standalone_ledger_commits_for_local_engines_only` and
  `the_server_ledger_commits_fully_synchronously` (`capyctl-cli`): each
  role opens its store through one function (`standalone_owned_state`,
  `server_owned_state`), and only the standalone one relaxes the ledger.
- `a_completed_answer_reaches_the_client_before_its_lease_close`
  (`capyctl-router`): with the close held back, a collected answer and a
  stream through `[DONE]` both reach the client while the lease is still
  open and the request still holds its slot; the close then ends the lease
  completed and frees the slot.
- `a_client_that_leaves_after_its_answer_still_has_its_lease_closed`: over
  a real connection, a client that hangs up right after its whole answer,
  collected or streamed, does not stop the close.
- `a_crash_between_answer_and_close_leaves_the_lease_charged` (router: the
  runtime dies with the close pending, the lease is never closed) and
  `a_local_grant_survives_a_crash_before_its_close` (store: a grant
  committed with `NORMAL` on a connection abandoned without closing is read
  back by the next session as an uncertain, charged lease).
- `v50_indexes_the_history_the_instance_read_searches`: a v49 store gains
  both indexes, keeps its rows, and the lookups by binding and operation
  use them.

`scripts/ci-local.sh --deep` passes. CPU and Fake-engine tests are not
qualification. Live check still needed: repeat the host A decision-request
comparison (Julia-1 and Clef-Flash, one and three questions, about 200 and
2k tokens of state) with this build.

# Release note: Requests

- **Less time added per request.** A completed answer now reaches the client
  before CapyCTL records the request as finished, and on a standalone machine
  the request ledger no longer waits for the disk on every request. On a local
  reproduction this cut what CapyCTL adds to a decision request from about
  4.5 ms to about 2 ms. A power loss on a standalone machine can lose the last
  request records, but it stops the engines they describe as well. A client
  that sends its next request at once may briefly find the previous one still
  counted against the deployment's limit.

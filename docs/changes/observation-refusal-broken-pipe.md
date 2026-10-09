# Status: Observation server refusal tests tolerate an early close — 2026-10-09 (branch `test/observation-bridge-broken-pipe`)

`test_sglang_observation_server` `test_foreign_peer_never_reaches_bridge` failed
intermittently in the fast `scripts/ci-local.sh` runtime step with
`BrokenPipeError` at the test's `request()` (`sendall`). It passed when run
alone. `test_aliases_and_changed_parent_permissions_deny_connections` failed
once the same way. This was a test fault; the server is unchanged. The server
refuses both connections before it reads any request: the transport
authenticates the peer (`SO_PEERCRED`) before its first read, and the server
checks socket custody before handing the connection over. It then closes the
connection. The test connected and then sent its request. When the server's
thread ran first and closed the connection, the send failed with `EPIPE`,
before the test could see what it asserts: no response. The transport tests
already accept the same early close
(`test_broken_cancel_consumption_poison_disallows_new_requests`). `request()`
now accepts `BrokenPipeError` only for these two connections, which it is told
the server refuses. Every other connection must still send its whole request.

Tests. Every loop ran on 4 cores (`taskset`) beside 8 busy loops pinned to the
same cores.

- `test_foreign_peer_never_reaches_bridge`: before, 192/200, all 8 failures
  `BrokenPipeError`. After, 200/200.
- `test_aliases_and_changed_parent_permissions_deny_connections`: before,
  193/200, all 7 failures `BrokenPipeError`. After, 200/200.
- The whole module: before, 92/100 (9 `BrokenPipeError`s across the two tests,
  once both in one run). After, 100/100.
- A 200 ms pause between the client's connect and its send, which lets the
  server's thread run first, reproduces the failure deterministically. The
  module passed 0/5 before (both refusal tests `BrokenPipeError` every time)
  and 5/5 after.

CPU tests only; they are not qualification. No live check is needed.

# Release note: none

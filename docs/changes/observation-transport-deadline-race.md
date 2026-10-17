# Status: SGLang observation transport test accepts both outcomes of its deadline race — 2026-10-17 (branch `test/observation-transport-deadline-race`)

`test_each_connection_reports_where_it_stopped_and_what_the_scheduler_did` failed once in a local
`ci-local` run (2026-10-16) and passed on reruns. In its first case the bridge expires the
request's slot at the same deadline the transport sends by. Usually the transport's deadline
passes first and no frame goes out (stage `send`); when the bridge's expiry is polled just
before it, the transport sends the correlated `uncertain` frame instead, which the transport's
contract allows ("valid observer failures may receive a correlated uncertain frame"). The test
asserted only the first. It now accepts either, and in both requires the outcome `uncertain`,
no observation, and a frame (if any) correlated to the request. Evidence: 120 runs on one core
under six busy-loop processes gave 103 runs without a frame and 17 with the uncertain frame
(each of those would have failed before), no failures. No product code changed.

# Release note: none

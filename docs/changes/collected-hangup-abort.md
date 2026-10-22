# Status: A client hang-up stops the engine's work for non-streamed requests too — 2026-10-21 (branch `fix/collected-hangup-abort`)

Owner decision of 2026-10-21 (0.1.7); SPEC §10 amended 2026-10-21, §20 T17. A
client that hung up on a streamed request already closed the engine connection
(SPEC §10, amended 2026-10-01 and 2026-10-02), but a non-streamed request, a
collected chat completion or any decision at `/v1/systemone`, ran on to its end:
its forward runs on a task of its own so that a dropped handler never leaves the
request counted, and nothing told that task the client had gone. Now the task
races the forward against the client's departure (the answer channel's receiver
dropped with the handler) and drops the forward when the client goes, which closes
the engine connection. The request is `cancelling`, settled by the amendment of
2026-10-21 and [ADR 0032](../design/adr/0032-cancelled-request-settlement.md)
decision 6: a collected chat request the engine had answered (an output event
read) settles on the engine's idle reading; one cut before its answer, and every
decision, is held until the instance's next verified stop or restart. The task
still settles every lease, and an answer it observed whole before it saw the
client go still closes the lease completed (a ready completion wins the race). A
client gone before a request was sent to an instance closes its lease as not
accepted, and no other instance is offered it. A host's ingress already ran its
engine request inside the forwarded request and its relayed body, so when the
router's connection goes away the ingress drops its engine connection, before the
engine's response head as after; this is now documented in the code and pinned by
a test. SGLang's per-request abort (ADR 0032) is a separate later change and is
not implemented here.

Tests (T17 T38 T43, CPU and scripted engines; a new testkit engine double,
`HeldEngine`, reads each request on a raw socket, never finishes answering, and
counts the connections its client closes): router
`a_hang_up_mid_collected_request_closes_the_engine_connection` (before the
engine's head and after a head and a chunk that only opens the reply; the engine
sees its connection close within 5 s; the lease ends `cancelling`, unanswered;
the in-flight slot is released),
`a_hang_up_mid_decision_closes_the_engine_connection` (the same for a decision),
`a_collected_request_cut_at_its_deadline_closes_the_engine_connection`, and unit
tests `a_completion_ready_with_the_hang_up_wins` and
`a_hang_up_cuts_and_drops_a_pending_forward`; agent
`the_engine_connection_closes_when_the_upstream_goes_away` (a chat request before
the engine's head and after its opening chunk, and a decision, through the
ingress; the forwarding slot is freed). The two router hang-up tests fail on the
code before the change (the engine connection stayed open past the 5 s bound);
the deadline and ingress tests pass on it too, pinning behavior that already held.
CPU and Fake-engine tests are not qualification.

Live check still needed (host A, SGLang 0.5.22, in an owner-scheduled live
session): start 4 non-streamed long generations against a chat deployment (for
example `max_tokens` 4096 each, `stream: false`), wait until the engine's
`sglang:num_running_reqs` reads 4, kill the 4 clients, and read the gauge again:
it should drop to 0 within a few seconds instead of staying at 4 until the
generations end. `capyctl status deployment` should show the 4 requests
cancelling (settling on the idle signal if each had produced output, otherwise
at the next stop or restart). Repeat with decision requests on a decisions
deployment, and once through a host's ingress (host A as a host of a server on
the control host).

# Release note: Requests

- **A client that hangs up on a non-streamed request now stops the engine's work on it.** Until now only a streamed answer was stopped when its client went away; a non-streamed chat request or a decision request kept the engine busy until its whole answer was ready. CapyCTL now closes the engine connection as it does for streams, also for a model served on a host, and counts the request as it counts any cut request. See [When a request is cut short](../guide/requests.md#when-a-request-is-cut-short).

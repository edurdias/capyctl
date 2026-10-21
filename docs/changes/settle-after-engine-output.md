# Status: A cancelled request settles on engine idle only after the engine answered it — 2026-10-21 (branch `fix/settle-after-engine-output`)

Owner decision 6 of 2026-10-21 in
[ADR 0032](../design/adr/0032-cancelled-request-settlement.md); SPEC §10
amended 2026-10-21, §20 T17. A `cancelling` request (client hang-up, request
deadline or stream idle cut; streamed, collected or a decision) used to settle
on the first engine-wide idle reading taken after its cancellation. Reading the
pinned engines' source showed that such a reading does not prove a request the
engine never answered cannot still reach its scheduler: vLLM 0.31.0 and 0.32.0
send no abort for a request cancelled while it is being added to the engine
core; SGLang 0.5.22 dispatches with no disconnect check after tokenizing, and
its grammar queue is not in the gauge CapyCTL reads; llama.cpp b11669 answers
`/metrics` through a task posted ahead of queued inference tasks and counts
neither its primary queue nor work before posting; TensorFold 0.6.6 (CUDA)
checks for a disconnect before it counts the request in `/health`. Source
reading only, not reproduced live.

Now the idle reading settles only a cancellation the engine answered first:
for a stream or a collected chat request, an event read from the engine that
is backend progress (content, reasoning, tool-call or finish; not headers and
not a chunk that only opens the reply, which TensorFold sends before it counts
the request); and a complete engine error answer (5xx other than 503 with a
JSON object body), which is the engine's whole answer. Any other cancellation,
including every decision or collected request cut before its answer, settles
only on its instance's verified exit at the next stop or restart, the rule ADR
0031 §6 gave engines without an idle signal: it is never asked about, stop,
idle stop and switch drains do not wait for it, an idle park or a switch
releases the instance by a stop (journaled `ready_idle_unanswered_cancellation`
or the switch's stop reason), and the verified exit journals
`request_cancellation_acknowledged` with the receipt "cancelled before the
engine answered; stop operation ...". An operator's park still drains every
recorded request first, so it does not proceed while one remains. The same
holds for remote instances (the host's load report path).

Schema v51 adds `request_lease_cancellations.engine_answered` (0 or 1). A row
written by 0.1.6 reads 0, not answered: the conservative reading. In practice
an upgrade restarts the role, which already turns every lease of the previous
session uncertain, so those leases keep settling as retired leases do.
Status: each instance's `requests` gains `settle_when_idle` and
`settle_at_stop` (decimal strings); the text line reads, for example,
`cancelling: 3 (2 settle on the engine's idle signal; 1 at the next stop or
restart: cancelled before the engine answered)`.

Tests (T17 T38 T43 T33, CPU and scripted engines): store
`an_unanswered_cancellation_is_never_settled_by_an_idle_reading`,
`an_unanswered_cancellation_stays_charged_until_the_verified_stop`,
`an_unverified_stop_keeps_an_unanswered_cancellation`,
`an_unanswered_cancellation_turns_a_park_into_a_stop`,
`v51_reads_an_existing_cancellation_as_unanswered`; controller
`a_cut_before_any_engine_output_settles_only_at_the_verified_stop`,
`a_switch_releases_an_unanswered_cancellation_by_a_stop`,
`a_remote_cancelling_lease_settles_only_on_an_idle_sample_after_the_hang_up`
(an unanswered one beside it stays charged), `closing_as_cancelling_keeps_the_lease_charged`;
router `a_stream_cut_before_any_engine_output_is_unanswered`,
`a_hang_up_after_an_engine_chunk_is_answered`,
`a_collected_request_cut_before_its_answer_is_unanswered`,
`a_decision_is_bounded_by_the_request_deadline` (unanswered),
the engine error answer tests (answered); CLI
`status_shows_each_instances_cancelling_requests_and_what_settles_them`.
Before the change the old code settled every cancellation on the first idle
reading after it, so the tests that keep an unanswered one charged through
idle readings could not pass (they do not compile against the old types
either). CPU and Fake-engine tests are not qualification. Live check still
needed: on each engine, a request cut at its deadline before its first token
stays `settle_at_stop` through idle readings and is acknowledged by the next
stop; a hang-up after the first token still settles on the idle reading.

# Release note: Fixes

- **A request cut before the engine answered it no longer settles on the engine's idle reading.** A request whose client hung up, or that missed its deadline, before the engine sent any of its answer may not have reached the engine's queue yet, so the engine reading idle proved nothing about it. Such a request now stays counted until the engine's next stop or restart; stopping and switching do not wait for it, and a model that would park stops instead. A request the engine had started answering, or answered with an error, settles on the idle reading as before. `capyctl status deployment` shows how many settle each way (`requests.settle_when_idle`, `requests.settle_at_stop`). See [When a request is cut short](../guide/requests.md#when-a-request-is-cut-short).

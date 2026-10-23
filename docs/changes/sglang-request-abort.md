# Status: SGLang: a cancelled request is aborted by its ID and settles on proof (ADR 0032) — 2026-10-22 (branch `feat/sglang-request-abort`)

Implements [ADR 0032](../design/adr/0032-cancelled-request-settlement.md) (owner decisions
of 2026-10-21) and its SPEC text: §10 (two amendments and the note of 2026-10-19), §13.1
and §20 T17.

## The request ID as `rid`

A launch is capable when its engine is SGLang at exactly 0.5.22 (the registered
version, from the profile's build fingerprint, never the adapter's descriptor contract)
with one tokenizer worker (`capyctl_adapters::sglang::request_abort_supported`), and, for
a remote instance, when the host's session declares the new `request_abort` capability.
On a capable lane every chat request carries CapyCTL's request ID (the
`X-CapyCTL-Request-Id` value, the first lease's ID, kept across failover offers; never the
caller's `X-Request-Id`):

- on the router's machine the forwarder adds `"rid"` to the JSON it sends the engine,
  after the caller's body passed validation, so a caller's `rid` is still refused;
- to a host the router sends it as the `X-CapyCTL-Request-Id` header. The ingress refuses
  a value that is not one canonical ULID (400, before the engine), never forwards the
  header, and sets `rid` only on chat to a capable launch.

Decisions never carry it. SGLang returns the ID as the answer's `id`, which is kept.

## Abort and keep reading

On a capable lane a cut (client hang-up, a failed, slow or oversized delivery, the request
deadline before the first output, the stream idle bound after it), streamed or collected,
settles the lease `cancelling` as before, with the same answered flag, and the cut client
gets its answer as before. The router then keeps polling the same engine response with a
sink that delivers nothing, asks the engine once to abort the `rid`, and bounds the rest
of the read by `queue.stream_idle_timeout` from the abort
(`crates/capyctl-router/src/stream.rs`, `AfterCut`). If the response ends with its final
output read whole (a finish chunk then `[DONE]`; on a capable lane the parser accepts
SGLang's graceful `finish_reason: "abort"`), the lease is acknowledged by its own ID:
`LeaseWrite::AcknowledgeCancelled` closes that one cancelling lease of the current session
and journals `request_cancellation_acknowledged` (count 1, receipt "the engine finished
this request after it was asked to abort it; request lease <ID>") in the same
transaction. Anything else (the abort not sent, the bound passed, an error event, a
truncated or unreadable end) drops the response, which closes the engine connection, and
leaves the lease to the rules before it. The role log records `router_request_abort`
(the cut and whether the abort was sent) and `router_abort_settled` (acknowledged or the
fallback's reason, with the time since the abort). Lanes without the capability behave
exactly as before.

The abort is `POST /abort_request {"rid": …}` on the engine's loopback port with the
launch's admin key (CapyCTL starts SGLang with both keys, and SGLang 0.5.22 then accepts
only the admin key there), bounded at 3 s; its 200 is never evidence (`cancel_work`
reports `Uncertain`). On the router's machine the coordinator calls the SGLang adapter's
`cancel_work`; on a host it sends the new `AbortRequest` member action (field 19), which
the agent runs only against the launch it retains: the same member, deployment, profile
and exact generation, claim held, launched, ready, not parked, the same live engine
group, and a capable launch. The agent does not journal it (one row per cancelled request
would grow without bound, and a repeated abort is harmless); a refusal or a failed post
never ends the session. At most 2 aborts are in flight per host; a third is not sent and
its request falls back.

## Restart when full

When a durable grant is refused as full, the router asks the authority whether the
deployment's requests are all held until the next stop. In one transaction the store
checks that the open lease rows have reached the limit of 32 and that every one is an
in-flight cancellation of the current session held until the next stop (cancelled before
the engine answered, or on a custom engine with no idle signal), with no live or
uncertain row. If so, for each instance holding them that is not already stopping, it
accepts an ordinary verified stop (principal `system:held_requests`, idempotency key
`held-restart:<index>:<generation>`), marks the instance's start pending (the same
reconciliation a revision restart uses), and journals `instance_restarted_for_held_requests`
(deployment, stop operation, instance index, generation, count). The stop's existing
verified-exit cleanup settles the held requests with its receipt; a stop that cannot
verify the exit keeps them. The router waits for the instance through the existing
activation wait, within the request's deadline, and offers the request again; a full
bound that is not all held answers `queue_full` as before. The trigger is the count,
never a timer.

## Tests (T17)

CPU and Fake-engine tests are not qualification.

- Adapters: `the_request_id_reaches_the_engine_only_as_its_lane_says`,
  `an_abort_finish_is_a_final_output_only_where_the_id_was_sent`,
  `cancel_work_posts_abort_request_with_the_admin_key_and_stays_uncertain`.
- Router (`router_core.rs`): `capyctl_request_id_reaches_only_a_capable_sglang_lane`; a
  stream hang-up, a collected hang-up, a deadline cut and an idle cut on a capable lane
  each abort once with the request's ID and settle on their own final output; an error or
  truncated end, an abort that never ends (closed at the bound) and an abort not sent
  (engine connection closed at once) are not acknowledged; a lane without the capability
  is not aborted; a full bound of held requests restarts and the request waits (with and
  without an instance view); a full bound not held answers `queue_full`.
- Store: an acknowledgement closes exactly that cancelling lease and journals it, also a
  lease held until the next stop, and leaves uncancelled, settled, uncertain and
  other-session leases alone; 31 held requests restart nothing, 32 restart the instance
  once, an uncertain or a live lease keeps the restart from triggering, an instance
  already stopping is not stopped again, and the restart's verified exit settles the held
  requests and starts the instance again (an unverified exit keeps them).
- Controller: only SGLang 0.5.22 with one tokenizer worker aborts (0.5.21 and two workers
  do not); `abort_request` reaches the lane's driver with the request ID (a vLLM lane
  sends nothing); the authority acknowledges one cancelled request by its lease; held
  requests filling the limit restart the instance, which serves again, and below the
  limit or beside live work nothing restarts; a request abort is sent only to a host that
  declared it (version skew), an `attempted` answer is not redelivered, and the per-host
  cap holds.
- Protocol: `AbortRequest` round-trips and names one canonical ULID; its results name
  their launch only; it needs its capability.
- Agent: the head posts an abort for its retained capable launch only, with the admin key
  on loopback, and refuses an unknown handle, a forged deployment, the next generation,
  another member, a passed deadline and a terminated launch; chat through the ingress
  gets `rid` and no ID header reaches the engine; SGLang 0.5.21 is refused and gets no
  `rid`; the ingress sets `rid` from a valid header only on a capable chat launch.

## Live (host A, SGLang 0.5.22, 2026-10-22)

The binaries were built from `8df7a399`: host A built its own from the synced tree, and
the control host's server binary came from the snapshot before the rebase, whose
`crates/` and `Cargo.*` are the same. The standalone rows used a fresh state directory
with `managed_limit` 108 GiB and `free_reserve` 8 GiB. The chat model is Qwen3-4B-Instruct.
Gauges are `sglang:num_running_reqs` and `sglang:num_queue_reqs`, read from the engine's
loopback `/metrics` every 50 ms. Times are the router's `router_abort_settled`
`elapsed_ms`, which runs from the router asking for the abort to the settlement.

- **L1, passes (criterion clarified 2026-10-22: within one decode step of the abort
  reaching SGLang's scheduler; from CapyCTL's request, at most two steps plus the request
  to the engine; see ADR 0032).** Standalone,
  context 8192. 4 background streams kept the engine busy, and a fifth (the victim) hung
  up mid-stream, 8 times in all.
  - Every victim was acknowledged by its own ID 66–78 ms after the abort (68, 78, 75, 74,
    66, 76, 77, 76 ms).
  - The engine took 38.8 ms per step with 5 requests running (5 / 128.8 tokens/s, the mean
    of 23 decode-batch lines), so settling took 1.7–2.0 steps after the router asked.
  - Between each hang-up and the end of the background traffic, running + waiting never
    read below 4.
  - In the 7 runs with the longer victim (120–200 chunks), the running count went from 5
    to 4. In the first run the victim hung up after 8 chunks, before any batch report
    counted it.
  - 40 `request_cancellation_acknowledged` events, one per lease (8 victims and the 32
    background streams cut at the end), and no lease left open.
  - Where those 66–78 ms go was measured afterwards; see "L1: when the abort reaches the
    scheduler" below.
- **L2, passes.** Standalone, `queue.request_deadline` 240 s. A 90 s deadline cannot be
  used: `timeouts.initialize` may not exceed it, and the SGLang start did not fit in 90 s
  (`launch_failed`, the engine was cleaned up). The deployment ran 1 request at a time,
  context 32768. A blocker streamed up to 24000 tokens, and the victim, sent 0.10 s
  after it:
  - waited in SGLang's queue (gauges: 1 running, 1 waiting from 1.2 s to 241.8 s);
  - was cut at its deadline with no output (stream headers, 0 chunks, then the end);
  - was aborted and acknowledged by its ID 42 ms after the abort, against a stream idle
    bound of 120 s. The waiting count went to 0 while the blocker still ran.

  The collected variant was not run.
- **L3, passes.** A server on the control host, with host A as its host: the harness's
  `normal` host policy, the ingress on port 9443 and the same model. The victim and 4
  background streams went through the server's endpoint, 3 times; gauges were read over an
  SSH forward to the engine's loopback port.
  - Each answer's `id` was the response's `X-CapyCTL-Request-Id`: the ingress set `rid`.
  - Each abort was sent (`sent: true`: the agent's `AbortRequest` answered within its
    bound), and the lease was acknowledged on the final output relayed through the
    ingress, 79, 92 and 76 ms after the abort.
  - Running + waiting never read below 4, and the running count went from 5 to 4.
  - 15 acknowledgements (3 victims, 12 background streams) and no lease left open.
  - The host ran without engine logs, so the engine's own record of `/abort_request` was
    not captured.
- **L4, passes.** Standalone, a decisions deployment of Clef Flash (context 8192). One
  decision with this state took 1.74 s (8185 input tokens). 32 decisions were sent and all
  hung up 1.0 s later, before any answer. The 33rd was sent 3 s after the hang-ups.
  - Within 35 ms the store accepted a stop and journaled
    `instance_restarted_for_held_requests` (count 32, generation 1).
  - On the verified exit, `request_cancellation_acknowledged` settled all 32 with the
    receipt "cancelled before the engine answered; stop operation … verified the exit of
    the instance's process group".
  - The instance started again 6.3 s after the stop.
  - The 33rd request was answered 200 by the restarted instance (a new binding) 140.2 s
    after it was sent; its own time to response was 1.71 s. No lease was left open.

Host A was cleaned afterwards: no role, engine or listener left, and the run
directories, the synced tree, and this session's `/tmp` and `/dev/shm` entries (65, the 6
per-launch `sglang_loads_*.shm` included) were removed.

## L1: when the abort reaches the scheduler (host A, 2026-10-22)

L1 was run again from `5bc0668a` with the same setup (standalone, fresh state, 108 GiB /
8 GiB, Qwen3-4B-Instruct, context 8192, 4 background streams, a victim that hangs up after
120 chunks), 8 runs after one trial run, with timestamps on both sides of the abort. No
CapyCTL code, protection or engine environment was changed:

- **SGLang's side**, from its own logging. The profile set `SGLANG_LOG_MS=1` (millisecond
  log times) and `SGLANG_LOG_FORWARD_ITERS=1` (step numbers) with `capyctl engine add
  --env`. The deployment's `extra_args` set `--decode-log-interval 1` (one line per decode
  step, written when the scheduler has the step's result) and
  `--enable-request-time-stats-logging` (a line when the scheduler sends a request's final
  output). The role ran with `--debug-engine-logs`, whose debug level makes the scheduler
  log `Abort running request` when it applies the abort. A `sitecustomize` hook is not
  possible: profile and deployment environments refuse every `PYTHON*` name.
- **CapyCTL's side**, from the role's event lines, each stamped with the wall clock as it
  arrived. The ask is the `router_abort_settled` stamp less its `elapsed_ms`.
  `router_request_abort` is written when the engine has answered the abort's POST, and
  SGLang's tokenizer manager answers only after handing the abort to the scheduler, so it
  bounds the tokenizer manager's receipt. The tokenizer manager logs nothing of its own,
  and the HTTP access log has whole seconds, so the receipt is bounded, not stamped.

Times are good to about 1 ms. SGLang ran its default overlap scheduler, which starts step
N+1 before it processes step N's result. Steps below are the median decode step of the
second before the abort, 37.0–38.0 ms with 5 requests running. Per-step logging did not
slow the engine (38.8 ms before).

| Run | Ask → scheduler | Scheduler → final output | Step that carried it | Within one step | Ask → settled |
|---|---|---|---|---|---|
| 1 | 31.8 ms (0.86 steps) | 38.0 ms (1.03 steps) | 38 ms | yes | 75 ms |
| 2 | 34.4 ms (0.93) | 38.0 ms (1.03) | 38 ms | yes | 76 ms |
| 3 | 35.1 ms (0.94) | 38.0 ms (1.01) | 38 ms | yes | 85 ms |
| 4 | 31.3 ms (0.85) | 38.0 ms (1.03) | 39 ms | yes | 74 ms |
| 5 | 31.1 ms (0.83) | 37.0 ms (0.99) | 38 ms | yes | 74 ms |
| 6 | 33.7 ms (0.90) | 37.0 ms (0.99) | 38 ms | yes | 89 ms |
| 7 | 28.3 ms (0.75) | 37.0 ms (0.99) | 38 ms | yes | 69 ms |
| 8 | 34.6 ms (0.91) | 37.0 ms (0.97) | 39 ms | yes | 76 ms |

- The engine answered the abort's POST 1.5–4.7 ms after the ask.
- The abort then waited for the step in progress. In every run the scheduler applied it
  0–1 ms after that step's result, with exactly one step ending in between.
- The final output left with the result of the next step: the one the overlap scheduler
  had already started. Measured from when the scheduler applied the abort, that took 37–38 ms,
  never longer than that step (38–39 ms). Each run settled within one decode step of the
  abort reaching the scheduler (the trial run too: 28.8 and 39.0 ms).
- From the ask to the final output leaving the scheduler took 65.3–73.1 ms. The router
  settled 3.6–18.3 ms after that, 69–89 ms after the ask. The two slowest runs (85 and
  89 ms) spent that extra time after the engine's scheduler, not in it.
- The asks came 2.9–9.7 ms after a step ended because the victim hangs up just after a
  chunk, and chunks leave at step ends. That is why the wait for the scheduler was close to
  a whole step each time. [INFERENCE] A cut at any other moment waits 0–1 step, so from
  CapyCTL's ask the final output takes 1–2 steps plus the HTTP hop.
- 45 `request_cancellation_acknowledged` (9 victims, 36 background streams) and no lease
  left open. Host A was cleaned the same way as before: role, engine, listeners, the run
  directory, the synced tree and the 11 `/tmp` and `/dev/shm` entries the launch left
  were removed.

# Release note: Requests

- **On SGLang, a cancelled request stops counting as soon as the engine ends it.** On
  SGLang 0.5.22 with one tokenizer worker, CapyCTL sends each chat request its own
  request ID (the `X-CapyCTL-Request-Id` value, also as the answer's `id`). When a client
  hangs up or a request is cut by its deadline or idle bound, CapyCTL asks SGLang to abort
  that request and settles it when SGLang ends it, instead of waiting for the engine to
  read idle, on the router's machine and on hosts alike. When cancelled requests that wait
  for their instance's next stop fill a model's 32 places, CapyCTL restarts the instance
  and the restart settles them, rather than refusing new requests with `queue_full`.

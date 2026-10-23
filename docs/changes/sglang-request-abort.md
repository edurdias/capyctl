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

## Live (host A, SGLang 0.5.22)

Not run yet. Host A was held by the LMCache live rows when this branch was finished. All
four rows remain: L1 (an abort settles within one decode step under steady traffic), L2
(a deadline cut before output settles promptly), L3 (the host path through host A's
ingress and agent) and L4 (32 cut decisions fill the limit and the instance restarts).
Until they pass, nothing here is qualified on a real engine.

# Release note: Requests

- **On SGLang, a cancelled request stops counting as soon as the engine ends it.** On
  SGLang 0.5.22 with one tokenizer worker, CapyCTL sends each chat request its own
  request ID (the `X-CapyCTL-Request-Id` value, also as the answer's `id`). When a client
  hangs up or a request is cut by its deadline or idle bound, CapyCTL asks SGLang to abort
  that request and settles it when SGLang ends it, instead of waiting for the engine to
  read idle, on the router's machine and on hosts alike. When cancelled requests that wait
  for their instance's next stop fill a model's 32 places, CapyCTL restarts the instance
  and the restart settles them, rather than refusing new requests with `queue_full`.

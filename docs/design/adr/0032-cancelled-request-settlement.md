# ADR 0032 — Settling a cancelled request on evidence about that request

**Status:** Proposed (2026-10-21). Nothing is implemented. The owner's answers to the open
questions at the end decide whether this lands, and in what scope.
**Would amend:** `SPEC.md` §10 (amended 2026-10-01: when a `cancelling` request settles;
note of 2026-10-19: what crosses a host's ingress, only if the remote half is accepted),
§13.1 (the "cancel supported work" operation family becomes a real agent operation for
one engine), §20 (T17).
**Related:** ADR 0013 (a request lease is charged to one instance), ADR 0017 (capability
gating by engine version), ADR 0028 §9 (the agent's loopback probes with the launch's
own key), ADR 0029 (llama.cpp), ADR 0031 §6 (engines without an idle signal).

## Context

### The rule today

SPEC §10 (amended 2026-10-01, 2026-10-02 and 2026-10-17): when a client hangs up, when
the router cuts a request for missing its deadline or its stream idle bound, or when the
engine answers with a complete 5xx error, the router closes the engine connection and
the request becomes `cancelling`. It stays charged until the instance's adapter reports
engine-wide quiescence, read after the hang-up: no running and no waiting requests.
"That report is the cancellation acknowledgement." Park, idle park and switch drains
wait for `cancelling` requests; an engine that never reaches quiescence keeps the
charge. ADR 0031 §6 adds that an engine with no idle signal settles them only at its
verified exit.

In code, a cancelled lease keeps its `request_leases` row (`inflight`) and gains a
`request_lease_cancellations` row stamped with the hang-up time
(`crates/capyctl-store/src/request_lease_cancellations.rs`). At most every 250 ms the
coordinator lists the bindings with cancelling leases, asks each engine
`engine_quiescent` (bounded at 3 s, backing off to 30 s after unanswered questions),
and closes the leases cancelled at or before the newest hang-up it asked about, with a
`request_cancellation_acknowledged` event and a receipt
(`crates/capyctl-controller/src/coordinator/worker.rs`, `cancelling_drivers`,
`ask_quiescence`, `settle_quiescent`). The readings are vLLM's
`vllm:num_requests_running` and `vllm:num_requests_waiting` summed over their labels,
SGLang's `sglang:num_running_reqs` and `sglang:num_queue_reqs`, llama.cpp's
`requests_processing` and `requests_deferred`, and TensorFold's `/health`. For a remote
instance the evidence is the host's load report: fresh (at most 3 s old), sampled after
the hang-up, 0 running, 0 waiting and 0 requests in flight at the ingress
(`crates/capyctl-controller/src/load_table.rs`, `proves_quiescence`).

### The problem

The ledger refuses a new lease once the deployment has 32 open lease rows
(`MAX_REQUESTS_PER_DEPLOYMENT`; `outstanding` in `crates/capyctl-store/src/dispatch.rs`
counts every row of the deployment, cancelling and uncertain ones included). Under
steady traffic an engine seldom reads 0 running and 0 waiting, so a cancelled request
can stay counted for as long as the traffic lasts, and every hang-up takes one of the 32
places until a lull. [INFERENCE, not measured: with enough hang-ups the cancelled
leases and the live ones fill the bound, new requests wait or are refused `queue_full`,
the live ones finish, the engine reads idle, and every cancelled lease settles at once.
Throughput would follow a sawtooth rather than stop.]

### The constraint

AGENTS.md (code conventions): "Uncertainty must retain accounting. Never release a
reservation, advance an epoch, or replay a dispatch without verified evidence." SPEC §10:
"Client disconnect is not proof the engine stopped working", and "Retain conservative
accounting until cancellation acknowledgement, completion observation, or controlled
cleanup." SPEC §8.3 asks an adapter to "provide explicit acknowledgement semantics where
supported; otherwise report uncertainty". Every adapter's `cancel_work` answers
`Uncertain` today.

So settling a specific cancelled request `c` sooner needs evidence of two facts:

1. the engine is not running or holding `c` now, and
2. `c` cannot reach the engine's scheduler later: it is not still in transit inside the
   engine (its HTTP layer, its renderer or tokenizer, or an input queue in front of the
   scheduler).

Any evidence that shows the first fact without the second can settle a request the
engine then runs.

### What was read

Each engine's public source at the pinned version, read on GitHub (no engine was run
and no lab host was used):

- SGLang [v0.5.22](https://github.com/sgl-project/sglang/tree/v0.5.22), commit
  `baa9bef`. Paths below are under `python/sglang/srt/`.
- vLLM [v0.31.0](https://github.com/vllm-project/vllm/tree/v0.31.0), commit `db9527a`,
  and [v0.32.0](https://github.com/vllm-project/vllm/tree/v0.32.0), commit `5df63a7`.
  Paths are under `vllm/`, with v0.31.0 line numbers unless marked; v0.32.0 is the same
  in every point below unless stated.
- llama.cpp [b11669](https://github.com/ggml-org/llama.cpp/tree/b11669), commit
  `620d222`, which is the commit of the approved v0.7.0 build
  (`scripts/live/llamacpp-build.sh`). Paths are under `tools/server/`.

TensorFold and custom engines were not examined; this ADR leaves them as they are.

## Evidence evaluated

### 1. Per-request engine evidence under an ID CapyCTL assigns

**Getting an ID to the engine.** CapyCTL refuses every chat field outside its allow-list,
SGLang's `rid` and vLLM's `request_id` included (`CHAT_REQUEST_FIELDS`,
`crates/capyctl-adapters/src/forward.rs`), and never forwards the caller's
`X-Request-Id` (SPEC §10, note of 2026-10-19). An ID would therefore be CapyCTL's
alone: the request lease's own ID, a ULID.

- SGLang takes the body field `rid` as the engine's request ID unchanged
  (`entrypoints/openai/protocol.py:986-987`, `entrypoints/openai/serving_chat.py:1320`)
  and returns it as the answer's `id` (`serving_chat.py:2109-2111`). A `rid` still in
  flight is refused as a duplicate (`managers/tokenizer_manager.py:3828-3832`). The
  scheduler matches an abort by prefix (`managers/scheduler.py:5471`, `:5587`); a
  ULID has a fixed length of 26 characters, so no lease ID is a proper prefix of
  another.
- vLLM takes `X-Request-Id`, else the body's `request_id`
  (`entrypoints/serve/engine/serving.py:120-129`), makes it `chatcmpl-<id>`
  (`entrypoints/openai/chat_completion/serving.py:291-293`), and adds 8 random
  characters for its internal ID (`v1/engine/input_processor.py:319-338`; v0.32.0
  `:354-371`). Duplicates are tolerated (`v1/engine/output_processor.py:517-548`).
- llama.cpp keeps no caller-set request ID. The completion ID is generated by the
  server (`server-context.cpp:4893`, `server-common.cpp:126`) and task IDs are internal
  integers. `X-Conversation-Id` names a resumable stream session instead, and with it
  generation deliberately continues after a disconnect (`server-stream.cpp:622-650`).

**What an engine says about that ID afterwards.**

- SGLang: the request's own terminal output, on its own response. The tokenizer marks a
  request finished only on the scheduler's output for it
  (`managers/tokenizer_manager.py:2617`) or on the scheduler's abort echo
  (`tokenizer_manager.py:3574-3640`). The scheduler echoes a waiting request after it
  has removed it from the waiting queue and released it (`managers/scheduler.py:5474-5486`),
  and finishes a running one after one more forward pass
  (`scheduler.py:5581-5597`). A graceful abort ends a chat stream through the normal
  chunk path, not as an error (`entrypoints/openai/serving_chat.py:2083-2100`). This
  proves both facts above for that request, as strongly as the completion evidence
  SPEC §10 already accepts: it is the same output path. It can be read only if CapyCTL
  keeps the engine response open after the client leaves.
- vLLM: nothing per request leaves EngineCore. An abort's terminal output is made by
  the API process itself (`v1/engine/output_processor.py:550-570`) before the abort is
  sent to EngineCore (`v1/engine/async_llm.py:876-887`), and EngineCore's abort handler
  sends nothing back (`v1/engine/core.py:532-537`). The scheduler does remove the
  request (`v1/core/sched/scheduler.py:2585` onward) and records it in its
  `finished_requests` sets used for data-parallel accounting, but none of that is
  exposed over HTTP.
- llama.cpp: nothing. Cancelling a task releases its slot and answers nothing
  (`server-context.cpp:2901-2910`).

**Failure modes.** *In transit:* SGLang registers a request before tokenizing it and
dispatches it without checking for an abort (`tokenizer_manager.py:927-951`,
`:1769-1810`); an abort that arrives first is sent to a scheduler that does not have the
request yet, and marks the state so no second abort is sent
(`tokenizer_manager.py:2240-2255`). That request runs to its natural end. The evidence
stays sound, since its terminal output still comes only from the scheduler, but the
settlement is not sooner. *Engine restart:* the response breaks and no terminal output
arrives; nothing settles from this path. *Metrics staleness:* not involved. *Multiple
instances:* a lease is charged to one instance (ADR 0013 §5) and its ID is sent only to
that instance's engine. *Several tokenizer workers:* with `tokenizer_worker_num` above 1
an abort is forwarded without a local state (`tokenizer_manager.py:2251`); how the echo
then reaches the worker holding the request was not read. *Parallel sampling:* SGLang
gives each sample a fresh ID (`tokenizer_manager.py:2130-2182`), but CapyCTL refuses
`n` other than 1.

### 2. Engine endpoints listing running request IDs

None of the three has one.

- SGLang: `/server_info` reports configuration and per-rank internal state without
  request IDs (`entrypoints/http_server.py:838-877`); `/v1/loads` reports counts only
  (`entrypoints/v1_loads.py:96-150`); request dumps are files, not a route
  (`tokenizer_manager.py:3297-3327`).
- vLLM: no route lists requests; `/server_info` is configuration and `/load` an
  aggregate (`entrypoints/launchers/api_server/routers.py:12-74`).
- llama.cpp: `/slots` shows each slot's internal `id_task`, never the completion ID the
  client sees, and an idle slot still shows its previous task
  (`server-context.cpp:733-767`). Queued tasks are not listed.

Even where one existed, a request's absence from the list proves the first fact only. It
proves the second only for a request known to have entered the scheduler, and only in a
listing ordered after the abort took effect.

### 3. Abort acknowledgements

- SGLang `POST /abort_request` (with only an API key configured, it requires that key:
  `utils/auth.py:130-140`) answers an empty 200 whether or not the request exists, and
  does not wait for the scheduler (`entrypoints/http_server.py:1686-1696`,
  `tokenizer_manager.py:2240-2260`).
- vLLM's `POST /abort_requests` exists only in development mode
  (`VLLM_SERVER_DEV_MODE`) and answers `{"status": "aborted", "aborted": N}`, counting
  the IDs it was given (`entrypoints/serve/dev/rlhf/api_router.py:95-129`; v0.32.0
  `entrypoints/rl/online/api_router.py:94-132`). v0.32.0 adds
  `/inference/v1/abort_requests` under `--enable-scale-out`, which starts the abort in
  the background and answers 200 at once
  (`entrypoints/scale_out/token_in_token_out/api_router.py:79-104`).
- llama.cpp has no abort by ID. `DELETE /v1/stream` ends a conversation session's
  output, answers 204 even when the session is gone, and leaves the generation running to
  its own stop (`server-stream.cpp:291-317`, `:561-575`).

At best an acknowledgement proves that the abort was dispatched. vLLM keeps an ADD and a
later ABORT for the same request in order (`v1/engine/core.py:1496-1525`), so a
dispatched abort will take effect, but not before the step in progress ends: the engine
may still be running the request when the answer arrives.

### 4. Counting arguments

Compare the engine's count with CapyCTL's: at the instant of a reading the engine holds
a set `E` of requests, and reports `R + W = |E|`. CapyCTL holds live (not cancelling)
leases `A` and cancelling leases `C` on that instance. Then
`|E ∩ C| = |E| − |E ∩ A| − |E ∩ X|`, where `X` is work CapyCTL did not dispatch. To
conclude that no cancelled request is running, CapyCTL needs `|E ∩ A| ≥ R + W`: a lower
bound on how many of its live requests are inside the engine at that instant. It has
none:

- a live request may not have reached the scheduler yet (rendering, tokenization,
  transport, input queues), so CapyCTL counts it and the engine does not;
- a live request may have finished inside the engine while its last bytes are still on
  the way to the router, so CapyCTL counts it and the engine does not.

Each such request leaves room in `R + W` for a cancelled one. Example: 4 live streams
running, 1 live request dispatched but not yet admitted, 1 cancelled request still
running. The engine reads 5, CapyCTL has 5 live leases, and a rule "settle when
`R + W ≤ |A|`" settles the cancelled request while it runs. Under steady traffic the set
in transit is largest, because an engine reads its input between steps
(`v1/engine/core.py:1470-1479`), which is exactly when such a rule would be needed.

The counts are also not exact or complete. SGLang's exported gauges change with batch
reports, and when idle they are republished within 30 s unless the running count is
already wrong (`managers/scheduler_components/metrics_reporter.py:1430-1480`); its queue
gauge leaves out requests waiting for grammar compilation (`metrics_reporter.py:838-841`),
and a health check may run a real one-token generation
(`entrypoints/http_server.py:707-758`). llama.cpp counts busy slots and its deferred queue,
not tasks waiting for its main loop (`server-context.cpp:2948-2966`,
`server-queue.cpp:40-70`). vLLM's gauges carry one series per engine, which CapyCTL sums.
Only `R + W = 0` bounds `|E ∩ C|` at all, and that is today's rule.

### 5. Connection-level evidence

**The engine closed its end of the connection** (seen, for instance, by the host agent
in the engine's sockets). This proves that the engine's HTTP layer noticed the
disconnect, not that its scheduler stopped. After noticing it, vLLM sends an abort to
EngineCore asynchronously, and if the cancellation lands while the request is being
added, it sends none (`v1/engine/async_llm.py:546-553`, `:713-745`: the abort runs only
`if q is not None`; v0.32.0 the same, `:780`). SGLang aborts without waiting, and for a
stream also from a background task two seconds after the response ends
(`tokenizer_manager.py:2425-2436`). llama.cpp posts cancel tasks without waiting
(`server-queue.cpp:601-617`), and checks the connection only when a one-second wait for
results times out (`server-queue.cpp:550-567`).

**The connection stays open and CapyCTL reads to the engine's own terminal event.**
On every engine, a request left to run ends with the completion evidence SPEC §10
already accepts. Without an abort by ID (vLLM, llama.cpp) this means letting the request
generate to its end for nobody, which reverses the 2026-10-01 decision to abort on a
hang-up. With SGLang's abort by `rid` it ends within about one forward pass: that is
the design below.

### Summary

| Evidence | SGLang 0.5.22 | vLLM 0.31.0 / 0.32.0 | llama.cpp v0.7.0 (b11669) | Proves "not running and never will" |
|---|---|---|---|---|
| ID CapyCTL assigns | body `rid`, used as is | `X-Request-Id` or `request_id`, prefixed and suffixed | none | — |
| Terminal output for that ID after an abort | yes, from the scheduler | synthesized by the API process | no abort by ID | SGLang only |
| Listing of running IDs | no | no | `/slots`, internal task IDs only | no engine |
| Abort acknowledgement | empty 200, fire and forget | dev mode or scale-out only, before EngineCore | none | no engine |
| Counts against CapyCTL's count | gauges | gauges | gauges | only at 0 |
| Engine closed its socket | yes | yes | yes | no |
| Read to the natural end | yes | yes | yes | yes, at full cost |

### A finding about today's rule

The second fact above also limits today's rule. A request cancelled before it reached the
scheduler is counted by no engine, so a zero reading after the hang-up does not show it:

- vLLM renders a request before adding it, and a cancellation during the add leaves it
  running with no abort (`v1/engine/async_llm.py:546-553`, `:713-745`);
- SGLang tokenizes and prepares a request before dispatching it and notices a disconnect
  only afterwards (`tokenizer_manager.py:927-951`, `:1936-1975`), and CapyCTL's queue
  reading leaves out the grammar queue;
- llama.cpp tokenizes before posting its tasks, with no connection check in between
  (`server-context.cpp:4901-4964`), and answers `/metrics` through a high-priority task
  that can run before tasks already queued (`server-context.cpp:5325`).

[INFERENCE] A request that produced output before it was cancelled had entered the
scheduler, so a zero reading after the hang-up does show it is gone. The exposure is a
request cancelled before its first output, while the engine is otherwise idle, and lasts
for its transit time. When it is hit, the request runs uncounted after it settled, and on
vLLM it may run to its end. This is not what this ADR decides; it is open question 6.

## Decision (proposed)

Settle a cancelled request on SGLang by its own terminal output after aborting it by an
ID CapyCTL assigns. Keep today's rule for every other engine, and for SGLang whenever
that evidence does not arrive.

1. **The ID.** On an SGLang launch whose engine has the capability (ADR 0017: SGLang
   0.5.22, one tokenizer worker, the chat endpoint), the body CapyCTL sends carries
   `rid` set to the request lease's own ID. A caller's `rid` is still refused. The
   engine returns it as the answer's `id`.
2. **Abort and keep reading.** On a hang-up or a deadline or idle cut, the lease becomes
   `cancelling` as today and stays charged. Instead of dropping the engine response, the
   router keeps reading it and discards what it reads, and asks the engine to abort that
   `rid` on its own loopback port with the engine key: locally through the adapter's
   `cancel_work`, and for a host through an agent operation (SPEC §13.1, "cancel
   supported work") that the agent runs as it runs the wake canary (ADR 0028 §9), never
   through the ingress or the router.
3. **Settle on that request's terminal output.** If, within a bound, the response ends
   with its terminal output read whole (a stream's final chunk and `[DONE]`, or a whole
   collected answer), the lease closes with `request_cancellation_acknowledged` and a
   receipt naming the evidence: "the engine finished this request after it was asked to
   abort it". Anything else (the bound passes, the connection breaks, an error event, a
   truncated or unreadable end, an abort that could not be sent) closes the response and
   leaves the lease to today's rule. Engine-wide quiescence still settles the lease if it
   comes first.
4. **Engine error answers** keep today's rule: the answer is complete, so there is no
   response left to read.
5. **vLLM, llama.cpp, TensorFold and custom engines** keep today's rule, ADR 0031 §6
   included. A later engine version can gain the capability only with source and a live
   check showing an acknowledgement that its scheduler emits after it removed the
   request, or a terminal output its API process does not make up.

Why this never settles a request the engine may still run: the terminal output reaches
the router only after the tokenizer received the scheduler's own finished output or
abort echo for that `rid`, and the scheduler sends either only after the request has left
its waiting queue or finished its last forward pass. A request still in transit inside
the engine when the abort arrives is not settled sooner: it runs to its own end, which
then settles it, or the bound passes and today's rule applies.

The SPEC §10 text this would add, to land with the implementation: "On an engine with
the capability (ADR 0032), a request cancelled by a hang-up or a cut is aborted by
CapyCTL's own ID while the router keeps reading its response, and settles when that
response ends with its terminal output read whole within the bound; otherwise it
settles as above."

## Consequences

- On SGLang, a cancelled request would settle about one forward pass after the abort
  reaches the scheduler, whatever else the engine runs, so hang-ups stop filling the
  bound there. [INFERENCE until measured live.]
- The router keeps an engine response open for up to the bound after each hang-up, and
  a host's ingress keeps relaying it. The ingress in-flight count then stays above 0, so
  the remote quiescence reading cannot settle anything meanwhile.
- On SGLang the answer's `id` becomes the lease ID instead of a random hex string. It is
  CapyCTL's own ID, already sent as `X-CapyCTL-Request-Id` (for a request offered to a
  second instance, the header keeps the first lease's ID and the body carries the
  second's).
- For a host, the lease ID has to reach the ingress so the ingress can set `rid`, which
  the note of 2026-10-19 ("neither request ID crosses it") does not allow today; the
  agent gains an abort operation.
- vLLM and llama.cpp deployments see no change: the problem stays on those engines.

## Alternatives considered

- **Keep today's rule everywhere.** No new path or failure mode; the problem stays on
  every engine.
- **Let a cancelled request run to its end** and settle on its terminal output, on every
  engine. Sound, but the engine spends a slot generating for nobody until `max_tokens`
  or the deadline, and it reverses the owner decision of 2026-10-01.
- **Settle on counts** (`R + W` at most the live leases, or an equivalent margin).
  Unsound: section 4.
- **Settle when the engine has closed its socket.** It proves dispatch of an abort, not
  its effect: section 5.
- **vLLM's development-mode abort followed by reading to the end.** The terminal output
  is made by the API process before EngineCore sees the abort: section 1.
- **llama.cpp slot pinning** (`id_slot`, an engine-internal field CapyCTL refuses) **and
  `/slots`.** The slot shows an internal task ID CapyCTL never learns, and queued tasks
  are not shown.
- **Not counting cancelling leases against the per-deployment bound.** It settles
  nothing; it changes what the bound means, and is left to the owner (question 7).

## Verification

When implemented: unit tests against stub engines for each branch of decision 3, tagged
T17. CPU and Fake-engine tests are not qualification. Live rows, on SGLang 0.5.22 in an
approved environment and in a session the owner schedules:

1. Under steady traffic that keeps the engine from reading idle, a client hangs up
   mid-stream; the lease settles with the per-request receipt while the gauges never read
   0, and the request is gone from the engine's running count.
2. A hang-up while the request is queued, and one during a long, chunked prefill.
3. A hang-up before the first output, and one before the engine dispatched the request.
4. The engine is killed while CapyCTL reads after the abort; the lease stays charged
   until the instance's verified exit.
5. The same on a host, through its ingress and the agent's abort.

## Open questions for the owner

1. Adopt the SGLang-only design, or keep today's rule on every engine?
2. For hosts: may the lease ID cross the ingress so that the ingress sets `rid` (amending
   the note of 2026-10-19), or should the first version cover only engines the router
   reaches on loopback?
3. The bound on reading after the abort. A running request ends after one forward pass,
   but a long chunked prefill defers the abort until its chunk ends. Proposed: the
   stream idle bound.
4. Should deadline and idle cuts take this path as hang-ups do? The cut client gets its
   answer first; only the settlement changes.
5. Is the lease ID as the answer's `id` on SGLang acceptable, or should the router
   restore a random one?
6. Today's rule can settle a request cancelled before it reached the scheduler (section
   "A finding about today's rule"). Accept the window, or narrow it: settle on a zero
   reading only the requests that produced output, read SGLang's grammar queue gauge as
   well, or wait a transit allowance after the hang-up?
7. The ledger counts cancelling and uncertain leases against the 32 per deployment. On a
   custom engine without an idle signal, 32 cancelled requests would leave the deployment
   refusing new requests until its next stop or restart, while ADR 0031 §6 point 3 says
   admission is not starved. Is that intended, or a separate fix?

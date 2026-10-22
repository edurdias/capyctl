# Status: Prompt hang-up record before the first token; SGLang startup failure line last — 2026-10-22 (branch `fix/live-017-findings`)

Two findings of the live-017 run on host A (SGLang 0.5.22, release build of
`466b6fa8`), both fixed here.

## A stream hung up before its first token

Found live: a streamed request queued in SGLang behind a streaming request was
hung up before its first token, and CapyCTL counted it as cancelling only about
10 s later. The 10 s was the run's own pause before it hung up the request
ahead: SGLang started the queued request only then, and its first chunk was
when the router noticed. The router learned of a stream's hang-up only when it
next delivered a chunk to the client and the delivery failed (SPEC §10). After
the first token chunks come continuously, so that was prompt; before it there
was nothing to deliver, so the engine connection stayed open, the engine kept
the request queued, and its cancellation was recorded only when the engine
produced a first chunk, however long that took. The client's own connection
was closed at once: hyper drops the response body when the client goes, and
that was not watched.

`crates/capyctl-router/src/stream.rs` now races each attempt's forward against
the response body's channel closing: when the client goes, the forward is
dropped at once, which closes the engine connection, and the attempt ends as
`Cancelled`. A forward whose end is ready at the same moment still ends with
that end (the race is biased toward it), as for collected requests (#180).
Nothing about settlement changed: a request hung up before the engine's first
token is cancelling as unanswered and settles only on its instance's verified
stop; one hung up after it settles on the engine's idle reading. A request
through a host's ingress already closed its engine connection when the router's
went away (the ingress relays the engine's body directly).

## SGLang's failure line was not the log's last

Found live: with the scheduler SIGKILLed during the weight load, the launch
failed promptly (#181), but the engine log tail ended with five lines after
`sglang_startup_failed: engine_launch_failed`: `Exception ignored in:
ResourceTracker.__del__ … ChildProcessError: [Errno 10] No child processes`.
The entry's cleanup reaps every child it ended, multiprocessing's resource
tracker included (a child of the entry, started when SGLang spawns its first
process). At interpreter exit the tracker object's finalizer then waited for
its process again; Python 3.12 (3.12.13 here, 3.12.14 on host A) raises
`ChildProcessError` there and prints it, while 3.14 ignores it silently.

`runtime/sglang_entry.py` now has the tracker stop itself before the entry
reaps the rest: once every descendant is killed, `_reap` calls the tracker's
own `_stop`, which closes its pipe and reaps its process, so its finalizer at
exit has nothing to do. The verified-exit cleanup is unchanged: the tree is
frozen, killed and read again until `/proc` shows none of it running, and only
then does the entry write its failure line and exit.

## Tests

- `runtime/tests/test_sglang_startup_death.py` (T12) now asserts that the
  failure line is the last line of the entry's stderr, and that the resource
  tracker's pipe is closed after `main` returns (so the finalizer has nothing
  to wait for on any Python). Before the fix it failed under Python 3.12.13
  (the five `ChildProcessError` lines followed the failure line) and under
  3.14.7 (the tracker's pipe was still open); after it, it passes under both.
- `crates/capyctl-router/tests/router_core.rs`,
  `a_stream_hung_up_before_the_first_token_closes_the_engine_connection` (T17
  T38): a held engine, with no response head and with a head and an opening
  chunk; the streamed client hangs up before any token, the engine sees its
  connection close within 2 s (well inside the 15 s between SSE keep-alive
  comments), the lease ends cancelling as unanswered and the in-flight slot is
  released. Before the fix the engine connection stayed open (failed at the
  2 s bound); after it, it closes at once.
- `crates/capyctl-router/tests/stream_safety.rs`: the transitional test
  `a_backend_that_completes_after_a_hang_up_releases_on_completion` assumed the
  router waits for a forward after its client has gone, which it no longer
  does; it is replaced by `a_hang_up_before_the_first_token_drops_the_forward`,
  which checks that a hang-up drops a forward that has produced nothing, and
  that without a ledger the slot stays charged.

CPU and Fake-engine tests are not qualification.

## Live recheck

On host A on 2026-10-22, with a release build of this branch at `90071793`
(before its rebase onto #182, which changed only docs), the approved SGLang
0.5.22 environment, Qwen3-4B-Instruct-2507 BF16, and a standalone role set up
as in the live-017 run (managed limit 108 GiB, free reserve 8 GiB, engine logs
not in debug mode).

- **Scheduler SIGKILL during the weight load:** sent to `sglang::scheduler`
  after `Load weight begin` and before `Load weight end`, its pid taken from
  the recorded entry's process group after checking the entry's pid, start
  ticks and boot id. The resource tracker was a member of that group. The
  launch failed 2.31 s later as `launch_failed`; the process group was empty
  at 2.23 s, the binding was released and no reservation was left. The engine
  log's last line is `sglang_startup_failed: engine_launch_failed (…)`; no
  `ResourceTracker` or `ChildProcessError` line and no `FileNotFoundError`
  appear in it.
- **Streamed hang-up before the first token** (two runs): a deployment with
  `max_concurrent_requests: 1`, a streaming blocker, and a streamed victim
  waiting in SGLang's queue (running 1, waiting 1) with no body byte received.
  The victim's curl was SIGKILLed; the first status reading after it, taken
  within 0.1 s, already counted it as cancelling (`settle_at_stop` 1). SGLang
  dropped it from its queue 2.65 s and 2.44 s after the hang-up while the
  blocker kept running; that delay is SGLang's own disconnect check. The
  blocker, hung up after its first token, settled on the next idle reading
  (0.39 s and 0.37 s after its hang-up), and the victim stayed held until the
  stop settled it on its verified exit. Before the fix the same case was
  counted about 10 s late, at the moment SGLang started the request.

Evidence is on the laptop under `target/live/live-017-findings/`. The role was
stopped and host A left with no engine, CapyCTL or GPU process, its ports free,
and the 30 `/tmp` and `/dev/shm` entries the engines created removed. The
killed launch's `/dev/shm/sglang_loads_*.shm` was among them: the tracker that
would unlink it is ended with the rest of the tree.

# Release note: Fixes

- **A streamed request whose client hangs up before its first token is
  cancelled at once.** It used to stay open on the engine, and uncounted as
  cancelling, until the engine produced its first token for it, which for a
  queued request could be long after.
- **A failed SGLang launch's log ends with CapyCTL's failure line.** On Python
  3.12, five lines about `ResourceTracker` and `ChildProcessError` used to
  follow it.

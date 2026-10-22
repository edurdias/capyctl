# Status: Live checks of #173, #180 and #181, and how quickly each engine stops — 2026-10-22 (branch `docs/abort-measurements`)

Run on host A (one NVIDIA GB10) on 2026-10-22 with a release build of `main` at
`466b6fa8`, one standalone role with its own state directory (managed limit
108 GiB, free reserve 8 GiB), and the approved environments: SGLang 0.5.22,
vLLM 0.32.0, llama.cpp v0.7.0 and TensorFold 0.6.6. Signals went only to
processes named by ownership evidence (the recorded launch identity) or to the
test's own clients. No code changes; the guide gains the measurement
([When a request is cut short](../guide/requests.md#how-quickly-each-engine-stops)).

## #181: an SGLang scheduler killed while the weights load

Qwen3-4B-Instruct-2507 on SGLang 0.5.22. As soon as the engine log showed
`Load weight begin` (no `Load weight end` yet), the `sglang::scheduler` in the
process group of the recorded entry (its pid, start ticks and boot id matched
the binding) got SIGKILL. The start failed 2.35 s later as a launch failure
(`initialize failed (launch_failed)`, "the engine exited before readiness"),
not ownership uncertainty; the process group was empty at the 2.25 s reading,
the binding was released and no reservation was left. The engine log had no
prometheus `FileNotFoundError`. Two runs gave the same result, one with
`--debug-engine-logs` (2.58 s) and one without (2.35 s).

One expectation did not hold: the log tail does not end with
`sglang_startup_failed: engine_launch_failed`. That line is followed by five
lines from Python's interpreter exit, `Exception ignored in: <function
ResourceTracker.__del__ ...>` ending `ChildProcessError: [Errno 10] No child
processes`. The entry reaps its own children, multiprocessing's resource tracker
among them, before it exits, and the tracker's finalizer then finds no child to
wait for (read from the traceback, not traced further). The failure is reported
correctly; the tail only has this noise after the line. With
`--debug-engine-logs` the launch-failure journal entry omits the tail, as
designed for raw logs.

A scheduler killed after Ready (an earlier run, by mistake) was reported as
`engine_exited`, and ordinary cleanup verified the group gone in 7.2 s.

## #180: a client hang-up on non-streamed requests, SGLang 0.5.22

- Chat, Qwen3-4B: four collected requests (`max_tokens` 4096); the clients were
  killed when `sglang:num_running_reqs` read 4, and the next reading, 0.1 s
  later, was 0 running and 0 queued. Status showed the four `cancelling`, all
  settling on the idle signal, and the next reading had settled them
  (`request_cancellation_acknowledged`, count 4, on the engine's counters). They
  were not held until the stop: CapyCTL collects a non-streamed answer from a
  stream, so each had produced output before the hang-up, and ADR 0032
  decision 6 settles an answered request on idle.
- Decisions, Clef-Flash: SGLang's gauge never read 4. It answered these long
  decisions (about 15,600 input tokens) one at a time and read 0 while
  answering the first. The clients were killed when that first answer came
  (gauge then 0 running, 2 queued): SGLang ran the other three to their ends,
  about 3.4 s each, and its gauge read 0 only 10.3 s after the hang-up. In an
  earlier run (clients killed at 1.5 s, gauge at 0), two of the four showed
  in SGLang's queue 2 s later and ran within 7 s. Status held all seven as
  `settle_at_stop` until the stop (3 min 15 s for the first four), through
  every idle reading, and the stop settled them when it verified the process
  group's exit (count 7, "cancelled before the engine answered; stop
  operation ... verified the exit of the instance's process group"). The
  expectation that the gauge drops within seconds therefore holds for chat
  and not for decisions: SGLang does not stop abandoned decisions.
- Not run: a decision or chat request through a host's ingress.

## #173: settling a cut request, SGLang 0.5.22 and vLLM 0.32.0

Qwen3-4B with `max_concurrent_requests: 1` and the standalone's
`queue.request_deadline` at 300 s: a streamed request (the victim) waited in the
engine's queue behind a streaming one (the blocker).

| | SGLang 0.5.22 | vLLM 0.32.0 |
|---|---|---|
| Victim cut at the deadline, before any token | 300.0 s; `settle_at_stop` 1 | 300.0 s; `settle_at_stop` 1 |
| Engine drops it from its queue | within 1 s | within 1 s |
| Blocker hung up after its first token, settled on idle | 0.25 s after the hang-up | 0.14 s after the hang-up |
| Victim still counted through idle readings | 59 readings, 61 s | 59 readings, 61 s |
| Settled at the stop | when the stop verified the exit | when the stop verified the exit |

A further SGLang run cut the queued victim by a client hang-up instead: it was
held until the stop in the same way. CapyCTL counted that cancellation only
about 10 s after the hang-up, in the same second as the blocker's hang-up let
SGLang start the queued request; how it noticed it then was not traced.

A first attempt set `request_deadline: 180s` on the deployment, under the
standalone's default `queue.request_deadline` of 1800 s. The queued stream was
not cut in 20 minutes: the bound on a reply's first output is the host's
`queue.request_deadline`, as the guide says, and the deployment's
`request_deadline` did not shorten it. In the code it bounds the deployment's
lifecycle windows (Initialize, wake, stop).

## How quickly each engine stops (0.1.7 item)

Four long generations at once, the clients killed once the engine counted four
running (for a stream, after each had read generated text), the engine's own
running and waiting counts read every 100 ms; three repeats, streamed and
non-streamed, through CapyCTL. Seconds from the hang-up to the first reading of
0 (median / max): SGLang 0.5.22 0.22 / 0.22 streamed and 0.10 / 0.10
non-streamed; vLLM 0.32.0, llama.cpp v0.7.0 (Qwen3-4B Q4_K_M) and TensorFold
0.6.6 (FrogNano-4B-2609 NVFP4) 0.10 / 0.10 both ways, the smallest figure the
100 ms reading shows. Directly against the engine (llama.cpp and TensorFold;
SGLang and vLLM answer only CapyCTL), streams stopped as fast; llama.cpp did
not stop non-streamed requests (0 after 64.6 to 65.9 s), which CapyCTL avoids by
streaming from the engine. vLLM's `/metrics` needs the engine's key, read from
the engine's environment by the measuring script and never printed.

## Other observations

- Ordinary stops leave the engines' scratch files in `/tmp` (the engine's
  environment carries no `TMPDIR`): each SGLang launch left a prometheus
  directory with `counter_<pid>.db` files and ZMQ sockets, and each vLLM launch
  a `vllm_dist_*` file and sockets. The #181 failure runs left no prometheus
  directory. 108 such entries from this session were removed by hand.

Evidence (laptop, not committed): `target/live/live-017/` (scripts, per-run
samples, status and management-event extracts). CPU and Fake-engine tests are
not qualification; this file records live runs only.

# Release note: Requests

- How quickly each engine stops a request whose client hung up, measured on SGLang,
  vLLM, llama.cpp and TensorFold, is in the requests guide. Known limit: on SGLang
  0.5.22 a decision request whose client hung up still runs to its end in the engine
  (reported upstream as sgl-project/sglang#45842); CapyCTL keeps it counted until the
  next stop.

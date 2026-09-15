# F2 mixed-engine qualification status

F2C remains incomplete. No native single-engine or mixed-engine qualification
results are claimed here. Only host-a and the selected existing Qwen3-4B
recipes are in scope; the native startup boundary remains closed.

## Local pressure monitor

The harness now owns a local pressure sampler with a 250 ms period and at most
one blocking `/proc/meminfo` read. Its independent watchdog aborts after the
two-second sample allowance, even if the read stalls. A late successful read
cannot reopen the latched abort. Dropping the monitor closes retained handles
immediately; explicit shutdown joins an outstanding read rather than pretending
that cancellation stopped it. No unbounded read queue or sample history is kept.

Every handle status check also validates freshness and clocks. `Ready` means only
that the fixed pressure guard currently passes, after its thirty-second stable
swap baseline. It is neither a lifecycle grant nor per-owner residency evidence.
The monitor reuses the existing protected-headroom, managed-ceiling and swap
growth rules without changing them. It never starts, stops or signals an engine.

The production collector is fixed to the bounded local proc reader. No caller
path or remote host label chooses its source. The future runner must separately
establish trusted host-a identity, persist its manifest, consult fresh pressure
status before submissions, and close run admission through the management API
on breach. Those composition steps are not implemented by this monitor alone.

Local verification: 18 harness tests and all-target Clippy pass. Five new tests
cover unsafe pressure, an independently observed stalled-read watchdog, late
safe results, one-reader shutdown, dropped-owner closure, normal baseline
collection and collector panic. These are CPU tests, not live qualification.

## Latency summaries

The harness provides a bounded recorder for each case, engine, mode and measured
interval. It reports total attempts, completed samples, non-timeout failures and
timeouts separately. Successful durations supply minimum, median, nearest-rank
p95 and maximum. Empty or entirely failed cases have absent latency values, not
zero-duration successes. The median averages the middle pair at nanosecond
resolution; no floating-point arithmetic is used.

Each recorder accepts at most 4,096 outcomes, including failures and timeouts.
Overflow is an explicit error and leaves the prior summary unchanged. This bounds
sample retention and sorting memory; it does not implement the separate 100 MiB
run-artifact limit. The runner must supply actual monotonic measurements and keep
cold initialization, warm restoration, direct/routed inference and overlapping
queue/activation intervals separate. Summary statistics do not establish those
identities or qualify a run.

The latency-summary slice passed 22 harness tests and all-target Clippy, including
four tests for exact statistics, empty/failure cases, duration overflow boundaries
and bounded storage. No native measurements were collected.

## Correctness and timing validation

`f3a2684` adds a bounded public marker corpus and exact response validation. The
validator accepts only the expected marker after outer ASCII whitespace trimming,
a natural stop finish reason, and a valid terminal sequence. Content is limited
to 128 UTF-8 bytes across 256 chunks, including empty chunks. Failures latch; later
input cannot repair a malformed or truncated response. This is not an HTTP/SSE
parser, and textual equality does not establish route or runtime identity.

`e4e2e95` adds validation of completed request timestamps from one monotonic clock
origin. Required timestamps and optional queue, activation and first-token
observations must be ordered within their appropriate windows. Queue and activation
may overlap and remain separate durations, never an additive latency decomposition.
Missing observations remain absent. The runner must supply actual timestamps and
keep failed and timed-out requests separate from completed latency samples.

Current full harness verification passes 33 tests and all-target Clippy: ten
library, eight pressure, four metrics, six correctness and five timing tests.
No native measurements or engine baselines were collected.

## Remaining gates

The API-driven runner, protected output manifest and metadata bounds, trusted
host preflight, run abort/admission composition, correctness-corpus integration, scenario
execution and Q1–Q11 reporting remain open. Native startup integration, trusted
allocation/identity evidence and the owned lifecycle/API/CLI cutover remain
prerequisites. Review stays consolidated at the requested F2 endpoint; no new
owner decision is required by these local helper slices.

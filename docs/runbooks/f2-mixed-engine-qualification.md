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

Collected JSON and streaming data validators now check the served model, one
choice, ordered marker content and natural stop. Streams additionally require
exactly one terminal event and reject events after completion. Alternative output,
malformed envelopes and duplicate fields fail without echoing response text.
The streaming subset rejects usage-only events.

The bounded SSE framing layer accepts LF/CRLF lines, comments and multiline data
across arbitrary network splits, including split UTF-8. Unsupported fields, lone
CR and truncated frames fail. The runner still verifies HTTP status/content type,
clean transport completion and exact binding provenance.

## Protected artifacts and numerical bounds

Request journals contain only closed outcomes, corpus ordinals and validated
monotonic durations. No free-text field accepts prompts, responses or credentials.
Partial writes and byte-limit failures stop the writer without replay or repair.

Descriptor-relative storage creates exclusive mode-0700 run directories and
mode-0600 fixed artifacts beneath a trusted parent. Writers share an at-most
100-MiB payload budget. Each file and directory requires explicit sync; dropping
a writer does not claim durability or delete evidence. These primitives do not
validate the manifest or establish host identity.

Phase-margin arithmetic computes `peak + max(2 GiB, ceil(peak/4))` with checked
integer addition. A separate helper finds the smallest whole-GiB ceiling that
covers all supplied intermediate charged demands, remains within the safe ceiling
and is strictly below direct-wake demand. No valid interval means this supplied
case cannot demonstrate Q6, not permission to shrink reservations or headroom.
Both calculations require trusted attribution and complete ledger/planner inputs
from their caller; neither grants qualification or changes resource policy.

Latest full harness run: 74 tests and all-target Clippy pass, including both
phase-margin tests and all three pressure-ceiling tests. Integrated verification
covers 508 Store/controller/management tests and four-crate all-target Clippy.
All management targets were rerun after the final candidate Abort SSE repair;
unchanged Store/controller/harness results come from the preceding full run.
No native measurements, engine baselines or qualification results were collected.

## Remaining gates

The API-driven runner, validated protected output manifest, artifact integration, trusted
host preflight, run abort/admission composition, correctness-corpus integration, scenario
execution and Q1–Q11 reporting remain open. Native startup integration, trusted
allocation/identity evidence and the owned lifecycle/API/CLI cutover remain
prerequisites. Review stays consolidated at the requested F2 endpoint; no new
owner decision is required by these local helper slices.

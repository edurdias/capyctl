# Data retention

This page lists what CapyCTL keeps about the requests it serves, where it keeps
it and for how long. CapyCTL never writes prompt or response bodies to its own
database, journal or role logs. The one place request content can reach disk is
the engine's own output, and only if the engine prints it; see
[Engine logs](#engine-logs). The engines CapyCTL launches report nothing about
their use off the host unless the host allows it; see
[Nothing reported off the host](#nothing-reported-off-the-host).

## Summary

| What | Contents | Where | How long | Turn off or reduce |
|---|---|---|---|---|
| Request records | IDs only: request, deployment, revision, generation, session, in flight or uncertain | Server or standalone database | Deleted when the request completes, fails or its launch is cleaned up | Cannot be turned off; it is how a drain knows a request is still running |
| Cancellation records | The request ID, the time it was cancelled, and whether the engine had answered it by then | Server or standalone database | Deleted together with the request's record | Cannot be turned off |
| Management events | Lifecycle events (operations, state changes, errors); no request bodies | Server or standalone database | 7 days, at most 100,000 rows and 64 MiB; older rows are pruned every time an event is written | Cannot be turned off |
| Journal | Lifecycle commands and their evidence; no request bodies, engine log tails or probe text | Server, standalone and host databases | Never pruned | Cannot be turned off |
| Latency and load figures | Per-request timings and engine load samples | Memory only | Until the role restarts | Nothing to turn off; never written to disk |
| Role logs | What each role does; no request bodies | The role's log (journald under systemd) | Your log system's retention | Set by your log system |
| Engine logs | The engine's own output, with credentials removed | A file per launch, or memory only | `files`: never deleted by CapyCTL. `memory`: 60 seconds after the engine exits | `engine_logs: memory` |
| Engine caches | Prefix and KV cache of recent requests | GPU memory of the running engine | While the engine runs; discarded when it parks or stops | Partition with `cache_salt` |
| Wake canary references | A fixed probe prompt and the engine's answer to it | Server or standalone database | Replaced by the next launch of the same instance | Cannot be turned off |

## Request records

While a request runs, CapyCTL keeps a record of it against the engine
instance serving it. The record holds only identifiers: the request ID (the
`X-CapyCTL-Request-Id` an answer carries), the deployment, its revision and
generation, the router session and whether the request is in flight or
uncertain (after a restart). The record is deleted when the request completes
or fails, or when its launch is cleaned up. A drain or park waits until no
records of in-flight requests remain.

A cancelled request also gets a cancellation record: its request ID, the
cancellation time, and whether the engine had answered it before the
cancellation (no content: only yes or no). It is deleted with the request's
record: when the engine reports itself idle afterwards if it had answered,
otherwise when the engine next stops or restarts
([When a request is cut short](../guide/requests.md#when-a-request-is-cut-short)).
A record written by 0.1.6 or earlier reads as not answered.

## Management events

The event log behind `capyctl events` and the management API keeps the last
7 days, and within that at most 100,000 rows and 64 MiB of payload. Each write
prunes everything outside those bounds, so the log never grows past them.
Events describe operations and state changes, not requests.

## Journal

The journal records lifecycle commands (start, park, wake, stop and their
steps) and the evidence each produced, so a restarted role can finish or
undo work it had started. It holds no inference bodies. The host journal does
not record engine log tails or probe text. Completed command payloads are
cleared, but the rows themselves are never pruned.

## Latency and load figures

The router's per-request timings and the engine load samples it scrapes are
kept in memory only, and are lost when the role restarts.

## Role logs

The server, host and standalone roles log what they do: operations, state
changes, errors. They do not log request or response bodies. Where those logs
end up, and for how long, is up to your log system (journald under the
packaged systemd units).

## Engine logs

Each launch's output (standard output and standard error) passes through a
redacting writer before CapyCTL stores it. The redactor removes credentials:
the launch's own keys, matched by value, and text shaped like tokens or keys.
It does not remove prompts or completions. Whatever the engine prints, minus
credentials, is kept.

The host setting `engine_logs` decides where the output goes
(see [settings](configuration.md#engine-logs)):

- `files` (default): one file per launch in the role's state directory, rotated
  at 16 MiB with two rotated files kept, so at most 48 MiB per launch. CapyCTL
  never deletes these files; remove them yourself or with your own rotation.
- `memory`: the writer keeps the last 4 MiB of each launch's output in memory
  and writes no file. The management engine-log read and CapyCTL's
  startup-failure reasons read from that buffer. The buffer is dropped 60
  seconds after the engine exits, and is gone if the machine restarts.
  `--debug-engine-logs` is refused with `engine_logs: memory`.

Set it in the host document (`engine_logs: memory`), with
`--set engine_logs=memory` or `CAPYCTL_SET__ENGINE_LOGS=memory` on a host, and
with `host.engine_logs: memory` or `--set host.engine_logs=memory` in a
standalone document.

What each engine prints by default:

- **vLLM**: CapyCTL turns request and output logging off for every supported
  version, so prompts and completions are not printed.
- **SGLang**: CapyCTL runs SGLang at its default `info` level with request
  logging and crash dumps off. SGLang prints prompts and completions only with
  request logging on. `--debug-engine-logs` raises the level to `debug`.
- **llama.cpp**: CapyCTL starts `llama-server` at its default verbosity, which
  does not print prompt or completion text.
- **TensorFold**: CapyCTL passes no logging options; the output is whatever
  TensorFold prints by default.
- **A custom engine**: CapyCTL runs the command its manifest gives and checks
  none of its options, so the output is whatever that command prints; keep
  options that log or save requests out of the manifest
  ([custom engines](../guide/engines.md#what-capyctl-cannot-promise)).

If you need no request content on disk at all, use `engine_logs: memory`.

## Engine caches

Engines keep a prefix cache and a KV cache of recent requests in GPU memory
while they run, so a repeated prompt prefix is not recomputed. CapyCTL does not
copy these caches anywhere. Parking discards the KV cache (vLLM sleep, SGLang
memory release) and stopping discards everything; a parked vLLM engine keeps
only its weights in host memory. CapyCTL also starts llama.cpp with its host
RAM prompt cache off.

To stop requests from different users sharing cached prefixes, send a
`cache_salt` field with each request. CapyCTL forwards it unchanged to an
engine that honours it (up to 1024 bytes) and refuses the request on an engine
that would ignore it, rather than serve it from a shared cache. CapyCTL does
not set a salt on its own.

## Wake canary references

When a launch first becomes ready, CapyCTL sends it a fixed probe (the prompt
"Say ready." at temperature 0, or a fixed multiple-choice probe) and stores the
prompt and the engine's answer as a reference. Each wake repeats the probe and
compares the answer, to catch an engine that came back wrong. The reference
contains no user data. Only the latest launch's reference is kept per instance;
it is replaced when the instance launches again.

## Nothing reported off the host

The engines CapyCTL launches report nothing about their use off the host
unless the host allows it. By default (`engine_telemetry: off`) every engine
starts with `DO_NOT_TRACK=1`, and vLLM also with `VLLM_DO_NOT_TRACK=1` and
`VLLM_NO_USAGE_STATS=1`, so vLLM does not post its usage report (the platform,
the model architecture and the engine settings, sent to `stats.vllm.ai` at
start, and a short report every ten minutes after). SGLang, llama.cpp and TensorFold have no usage
reporting of their own; `DO_NOT_TRACK=1` also turns off the Hugging Face Hub
library's telemetry, and TensorFold's release check is off on every launch.
A custom engine is a program CapyCTL does not know: it gets `DO_NOT_TRACK=1`,
and whether it honours it is up to the program.

A deployment or an engine profile cannot change this. Only the host can, with
`engine_telemetry: allow` ([settings](configuration.md#engine-usage-reporting)),
which leaves each engine's own default.

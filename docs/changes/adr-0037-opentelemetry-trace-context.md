# Status: ADR 0037 implemented: OpenTelemetry trace context — 2026-10-28 (branch `feat/otel-trace-context`)

ADR 0037 (accepted 2026-10-27, owner decisions of 2026-10-27 for 0.2.0) is
implemented on branch `feat/otel-trace-context`. Tracing is off until a role
names `observability.tracing.endpoint`; off, no trace header is read, no
exporter or task exists and no connection is opened.

What ships:

- **Settings** (ADR 0037 §12): `observability.tracing` in server and host
  documents (`endpoint`, `headers_file`, `sample_ratio`, `caller_context`;
  on a host `endpoint`, `headers_file`, `engines`), standalone with
  `server.observability.tracing` for the whole process and
  `host.observability.tracing.engines` only, settable by path, with the start
  refusals of §12. `headers_file` is one `Name: value` per line, owner-only,
  read at start; its values never appear in errors, logs, `config show` or
  status. An `http://` endpoint off loopback warns at start. `OTEL_*` in the
  role's environment is ignored.
- **The `capyctl-trace` crate** (ADR 0037 §§2, 5, 8, 9): W3C Trace Context
  Level 1 parsing (malformed or repeated `traceparent` ignored, `tracestate`
  joined and dropped over 512 bytes or 32 members), a closed attribute type
  for §5's allow-list, parent-based sampling, and OTLP/HTTP protobuf export
  built on `opentelemetry` 0.33.1, `opentelemetry_sdk` 0.33.1 and
  `opentelemetry-proto` 0.33.1 with the workspace's `reqwest`: a 2048-span
  queue drained every 5 seconds in batches of 512, drops counted, up to three
  retries, a role log notice at most once a minute, a 2-second flush at stop.
- **Router** (§§2, 3, 6, 10): headers read after authentication on chat and
  decisions; `capyctl.request`, `admission`, `activation_wait`, `queue_wait`,
  `dispatch` and `forward` per attempt (with `first_progress`), and
  `capyctl.settle` from the context kept beside the open lease; `traceparent`
  sent only to a host declaring `trace_context` and to an engine only on a
  launch with engine tracing.
- **Controller and protocol** (§§4, 6): activations as their own traces, with
  drain, release, reserve, launch or wake, readiness and canary steps, linked
  from each waiting request; the host capability `trace_context`; the command
  envelope's `trace_parent`, outside the canonical digest and never
  journaled.
- **Host** (§§6, 13): ingress span and propagation, host lifecycle spans as
  children of the server's step, the host's tracing status on its periodic
  report, shown by `capyctl list hosts`.
- **Engine tracing** (§§7, 11): with a host's `engines: on` and a loopback
  endpoint, vLLM 0.31.0 or newer and SGLang 0.5.22 or newer render their
  tracing options and variables; every other launch is off with
  `host_off`, `engine_unsupported`, `group`, `retention_zero` or
  `not_installed` (a launch-time check by shape that executes nothing). The
  decision is frozen per launch in the new `launch_engine_tracing` table (store
  migration `SCHEMA_V61`, at position 59 on this branch; the position is
  renumbered at rebase) and shown per instance by status. The engines'
  tracing options are owned and `OTEL_*`, `SGLANG_TRACE_*`, `SGLANG_OTLP_*`,
  `TRACEPARENT` and `TRACESTATE` are reserved, amending ADR 0014 §8.
- **Docs**: SPEC §§8.2, 10, 13.1, 13.3, 15.1, 17 and §20 (T48, live rows
  OT1–OT11); the Tracing section of the settings reference, Traces in data
  retention, Trace context in requests, Engine tracing in the engines guide
  and the engine option tables, trace headers in network access, and the
  tracing reasons and refusals in errors. A draft upstream report on SGLang
  exporting `/etc/machine-id` as `host_id` was written and not filed.

Tests: T48 tests in `capyctl-trace`, `capyctl-config`, `capyctl-adapters`,
`capyctl-agent`, `capyctl-controller`, `capyctl-protocol`,
`capyctl-management`, `capyctl-router`, `capyctl-store` and `capyctl-cli`
(also tagged T03, T17, T19, T21, T37, T40, T43 and T45 where they apply),
written to fail before the change for the behaviour they name.

CPU and Fake-engine tests are not qualification. No engine was run: the live
rows OT1–OT11 (an OpenTelemetry Collector release binary on loopback on host A
and host B) have not run, so engine propagation is unproven until OT5 and OT6
pass, and SGLang's trace level stays 1 until OT8 measures it.

# Release note: Requests

- **Tracing with OpenTelemetry.** CapyCTL can join your distributed traces:
  name an OTLP/HTTP collector in `observability.tracing.endpoint` and it reads
  a caller's `traceparent`, records spans for waiting, waking, dispatch and the
  forward to the engine, and exports them, never with request or response
  content. It is off by default. A host's `observability.tracing.engines: on`
  adds vLLM's and SGLang's own spans, exported to a collector on the same
  machine ([Tracing](../operations/configuration.md#tracing),
  [Trace context](../guide/requests.md#trace-context),
  [Engine tracing](../guide/engines.md#engine-tracing)).
- **Engine tracing options are now CapyCTL's.** vLLM `--otlp-traces-endpoint`
  and `--collect-detailed-traces`, and SGLang `--enable-trace`,
  `--otlp-traces-endpoint`, `--otlp-service-name` and `--trace-modules`, are
  refused in `extra_args` whatever the host approves, and the engine variables
  `OTEL_*`, `SGLANG_TRACE_*`, `SGLANG_OTLP_*`, `traceparent`, `tracestate`,
  `TRACEPARENT` and `TRACESTATE` are reserved (`engine_env_reserved:<name>`).
  A deployment that passed an approved `--otlp-traces-endpoint` stops working
  after the upgrade: remove the option and set
  `observability.tracing.engines: on` on the host instead
  ([Engine options](../guide/engine-flags.md)).

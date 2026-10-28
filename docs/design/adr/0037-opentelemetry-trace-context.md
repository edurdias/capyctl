# ADR 0037 — OpenTelemetry trace context: W3C propagation, CapyCTL spans, opt-in OTLP export

**Status:** Accepted (2026-10-27). Proposed 2026-10-24, for 0.2.0 (retargeted from 0.1.9
on 2026-10-27, after 0.1.9 shipped with ADR 0035 only). The owner answered every open
question on 2026-10-27, and the answers are recorded below ("Owner decisions of
2026-10-27"). One answer differs from the recommendation: the live rows use the
OpenTelemetry Collector release binary on host A and host B instead of a receiver script
(question 11). Implemented on branch `feat/otel-trace-context` (2026-10-28); the live rows
OT1–OT11 have not run. Implementation note: the export encodes with `opentelemetry-proto`
0.33.1 and posts with the workspace's `reqwest` instead of `opentelemetry-otlp`, whose HTTP
exporter reads `OTEL_EXPORTER_OTLP_*` from the environment whatever is configured (against
§8) and would add a second `reqwest`.
**Amends (with the implementation):** `SPEC.md` §10 (the request-ID note:
`traceparent` and `tracestate` are read after authentication, never refused, and replaced
rather than relayed), §13.1 (a host capability `trace_context`; trace context on ingress
requests and in the lifecycle command envelope, outside the journaled payload), §13.3 (the
two headers join the internal headers CapyCTL strips and replaces), §15.1 (server and host
settings under `observability.tracing`), §17 (traces as an opt-in export of measurements
§17 already asks for), §20 (T48 and live rows OT1–OT11); ADR 0014 §8 (the engines' tracing
options move from host approval to rendered and owned by CapyCTL).
**Related:** ADR 0010 (park tiers), ADR 0012 (deep parking and its protections), ADR 0013
(instances), ADR 0017 (capability gating), ADR 0019 (the network inference endpoint),
ADR 0027 (engine processes and helpers), ADR 0028 (groups), ADR 0030 (decisions), ADR
0031 (custom engines), ADR 0032 (CapyCTL's request ID as SGLang's `rid`), ADR 0033
(LMCache, zero retention), ADR 0036 (`/v1/embeddings`, proposed alongside this one).

## Context

A request through CapyCTL can spend most of its time outside the engine: waiting for a
slot, waiting for a parked model to wake, or waiting for another model to drain and park
so that this one can start. Callers that already trace their own applications see a gap
where CapyCTL sits: one long client span with nothing inside it. Operators ask the same
question from the other side: which of this request's seconds were queue, wake, prefill
or decode. This ADR proposes how CapyCTL takes part in a distributed trace without
weakening its privacy promises.

### What CapyCTL has today

- **Request IDs** (SPEC §10, note of 2026-10-19, PR #159). A caller may send one
  `X-Request-Id` (1 to 128 bytes of printable ASCII); a malformed or repeated one is
  refused with HTTP 400 `invalid_request` after authentication and before the body is
  read (`crates/capyctl-router/src/request_id.rs`, `crates/capyctl-router/src/lib.rs`
  `receive()`). It is echoed, written to CapyCTL's role logs for the request, and never
  sent to an ingress or an engine, never written to the ledger or an event. Every answer
  carries `X-CapyCTL-Request-Id`, the ULID of the request's first granted lease
  (`crates/capyctl-store/src/dispatch.rs`), or a fresh ULID for a request refused before
  any lease. ADR 0032 sends that ID (never the caller's) to SGLang as `rid` on capable
  launches; the ingress consumes the header and inserts `rid`
  (`crates/capyctl-agent/src/ingress.rs`).
- **Timings.** The router measures ten phases per completed request
  (`crates/capyctl-router/src/timing.rs`), served on request as the `x-capyctl-timing`
  header (`observability.timing_header`, off by default) and as JSON at
  `/management/v1/metrics/latency`; engine figures come back in `capyctl.metrics`
  (`crates/capyctl-router/src/engine_metrics.rs`). All of it is memory only
  (data-retention page, "Latency and load figures"). The activation wait and the queue
  wait are measured together today: `chat.rs` records `timing.queued()` around the
  activation join, and the later in-flight slot wait (`admit_timed()`) adds no separate
  mark.
- **Logging.** Roles write their own structured lines (`crates/capyctl-domain/src/role_log.rs`,
  text on a terminal, JSON otherwise). The workspace uses no `tracing` subscriber and has
  no OpenTelemetry dependency; `tracing 0.1.44` is in `Cargo.lock` only as a transitive
  dependency.
- **Headers.** The router does not proxy client headers: the outbound request is rebuilt
  (`crates/capyctl-adapters/src/forward.rs`: JSON body, CapyCTL's bearer key, and
  `X-CapyCTL-Request-Id` for a capable ingress lane), and the ingress "rebuild[s] every
  upstream header" (`crates/capyctl-agent/src/ingress.rs`). A caller's `traceparent`
  therefore reaches neither a host nor an engine today. SPEC §13.3 asks for exactly this:
  "strip/replace internal routing headers".
- **Engine tracing.** vLLM's and SGLang's `--otlp-traces-endpoint` are host-approval
  options in the "listeners and egress" class (ADR 0014 §8, "tracing endpoints";
  `crates/capyctl-config/src/engine_policy.rs`, `Sensitivity::ListenerOrEgress`). An
  operator can approve one today, but no trace context reaches the engine, so its spans
  start fresh traces that nothing joins.
- **Privacy defaults.** CapyCTL never writes prompt or response bodies to its database,
  journal or role logs (data-retention page). Engines report nothing off the host unless
  the host allows it (`engine_telemetry: off`, PR #188, owner decision 2026-10-22).
  `retention: zero` keeps engine logs in memory, keeps LMCache off and turns core dumps
  off (ADR 0033 §10); it does not cover role logs, which carry request IDs.

### The standards

- **W3C Trace Context** ([Level 1, Recommendation of 2021-11-23](https://www.w3.org/TR/trace-context/)):
  `traceparent` is `version-traceid-parentid-flags` in lowercase hex; an all-zero trace ID
  or parent ID is invalid and "vendors MUST ignore the traceparent"; version `ff` is
  invalid; a higher version is parsed by its first four fields when the header is at
  least 55 characters long. If `traceparent` cannot be parsed, the vendor "MUST NOT
  attempt to parse tracestate", and a `tracestate` "received without an accompanying
  traceparent header ... MUST be discarded". `tracestate` holds at most 32 list members;
  vendors "SHOULD propagate at least 512 characters" and may discard an unparseable
  header whole. A participant "receiving a traceparent request header MUST send it to
  outgoing requests" and may set its own span as the parent. Privacy: vendors "MUST NOT
  use traceparent and tracestate fields for any personally identifiable or otherwise
  sensitive information". [Level 2](https://www.w3.org/TR/trace-context-2/) is a Candidate
  Recommendation Draft (2024-03-28) adding the random-trace-id flag. Neither level defines
  a response header for trace context.
- **OpenTelemetry SDK defaults**
  ([environment variables](https://opentelemetry.io/docs/specs/otel/configuration/sdk-environment-variables/)):
  the default sampler is `parentbased_always_on`, `ParentBased(root=AlwaysOnSampler)`; the
  batch span processor exports every 5000 ms, queues at most 2048 spans and sends at most
  512 per batch, dropping spans when the queue is full. Span links relate a span to
  spans of other traces ([traces concepts](https://opentelemetry.io/docs/concepts/signals/traces/#span-links)).
- **GenAI semantic conventions**
  ([spans](https://github.com/open-telemetry/semantic-conventions-genai/blob/main/docs/gen-ai/gen-ai-spans.md)):
  input and output messages are `Opt-In` attributes that "SHOULD NOT" be captured by
  default; token counts, model names and finish reasons are ordinary attributes.

### What the engines do (read 2026-10-24 at the pinned tags; no engine was run)

- **vLLM 0.32.0** (also 0.31.0, whose `vllm/config/observability.py` has the same
  options). `--otlp-traces-endpoint` turns tracing on; `--collect-detailed-traces` adds
  per-module timings that "might have a performance impact"
  ([observability.py](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/config/observability.py)).
  The OpenAI server reads `traceparent` and `tracestate` from the request
  ([serving.py](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/entrypoints/serve/engine/serving.py),
  [tracing/utils.py](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/tracing/utils.py)),
  and the output processor emits one `llm_request` span per request with latency
  figures, token counts, sampling parameters and vLLM's request ID; no prompt or output
  text ([output_processor.py](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/v1/engine/output_processor.py),
  `do_tracing`). The exporter is gRPC unless `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` is
  `http/protobuf`, and the gRPC exporter is built with `insecure=True` (no TLS); the
  endpoint is written into the environment so worker processes inherit it
  ([tracing/otel.py](https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/tracing/otel.py)).
  The OpenTelemetry packages are vLLM dependencies
  ([requirements/common.txt](https://github.com/vllm-project/vllm/blob/v0.32.0/requirements/common.txt));
  configuring an endpoint without them fails the engine's start.
- **SGLang 0.5.22.** `--enable-trace` with `--otlp-traces-endpoint` (default
  `localhost:4317`), `--otlp-service-name` and `--trace-modules`
  ([observability fields](https://github.com/sgl-project/sglang/blob/v0.5.22/python/sglang/srt/arg_groups/fields/observability.py)).
  The tokenizer manager reads `traceparent` and `tracestate` from the HTTP request only
  when tracing is on (`tokenizer_manager.py`). Request spans carry token counts
  (cached included), sampling parameters, the served model name, finish reasons and the
  request ID, which is CapyCTL's request ID on an ADR 0032 launch; no prompt or output
  text. Thread spans also carry `host_id`, which is the machine's `/etc/machine-id` read
  verbatim, and the process ID
  ([trace.py](https://github.com/sgl-project/sglang/blob/v0.5.22/python/sglang/srt/observability/trace.py),
  `_get_host_id`). The machine-id manual says that ID "should be considered
  'confidential', and must not be exposed in untrusted environments, in particular on
  the network" ([machine-id(5)](https://www.freedesktop.org/software/systemd/man/latest/machine-id.html)).
  The exporter is gRPC with `insecure=True` unless the
  protocol variable says `http/protobuf`. SGLang's documentation warns that synchronous
  span creation "can degrade inference throughput" at large batch sizes and offers a
  trace level (`SGLANG_TRACE_LEVEL`, 0–3, default 3) and an asynchronous mode that starts
  an exporter process per worker
  ([production request tracing](https://github.com/sgl-project/sglang/blob/v0.5.22/docs/docs/references/production_request_trace.mdx)).
  The OpenTelemetry packages are the optional `tracing` extra
  ([pyproject.toml](https://github.com/sgl-project/sglang/blob/v0.5.22/python/pyproject.toml));
  `--enable-trace` without them fails the start. SGLang also serves
  `/set_trace_level` on its HTTP port.
- **llama.cpp.** No trace context support (a code search of the repository for
  `traceparent` and `opentelemetry` finds nothing).
- **TensorFold.** No trace context support is known to CapyCTL's integration
  [INFERENCE: TensorFold's source was not read for this draft].
- **TensorRT-LLM** (proposed as a native engine for 0.2.0). `trtllm-serve` has
  `--otlp_traces_endpoint` and reads the trace headers of the OpenAI request
  ([tracing.py](https://github.com/NVIDIA/TensorRT-LLM/blob/main/tensorrt_llm/llmapi/tracing.py),
  `serve/openai_server.py`, default branch). Its ADR decides whether this ADR's engine
  rules apply to it.
- **Custom engines** (ADR 0031) run a command CapyCTL does not know.

### Libraries

The Rust OpenTelemetry crates are at 0.33.1 (`opentelemetry`, `opentelemetry_sdk`,
`opentelemetry-otlp`, `opentelemetry-http`, published 2026-10-09;
[releases](https://github.com/open-telemetry/opentelemetry-rust/releases)); the
`tracing-opentelemetry` bridge is at 0.34.0. The workspace already uses `reqwest` 0.12
with `rustls-tls`, which `opentelemetry-otlp`'s HTTP exporter can use, so OTLP over
HTTP/protobuf adds no TLS stack and no gRPC stack.

## Decision

### 1. Off by default; one opt-in endpoint per role

Tracing is **off** unless a role's document names an OTLP endpoint
(`observability.tracing.endpoint`, §12). Off means: CapyCTL does not read `traceparent` or
`tracestate`, builds no exporter, starts no export task, opens no connection, creates no
spans and sends no trace header to a host or an engine. Behaviour and cost are exactly
today's. The headers are not refused either way.

Traces only: no OpenTelemetry metrics or logs (alternatives).

### 2. Accepting a caller's trace context

With tracing on, the router reads the two headers on every inference endpoint CapyCTL
serves (`POST /v1/chat/completions`, `POST /v1/systemone`, and `POST /v1/embeddings` if
ADR 0036 is accepted), **after authentication** and before the body is read, beside the
`X-Request-Id` check. A request that fails authentication gets no span and its headers
are not parsed, so an unauthenticated client cannot make CapyCTL export anything.

Parsing follows W3C Trace Context Level 1:

- one `traceparent`, version `00` exactly 55 characters, or a higher version other than
  `ff` parsed by its first four fields; lowercase hex; non-zero trace ID and parent ID;
- more than one `traceparent` header, or any invalid one, is **ignored**: the request
  starts a new trace and `tracestate` is not read;
- `tracestate` is read only with a valid `traceparent`; repeated headers are joined with
  commas; a combined value over 512 bytes, over 32 list members or not parseable is
  dropped whole;
- the request is **never refused** for its trace headers (owner decision 2026-10-27, question 2), unlike
  `X-Request-Id`: trace context is best effort by design, and an intermediary the caller
  does not control may add it.

What the router does with a valid caller context is `observability.tracing.caller_context`
(owner decision 2026-10-27, question 3): `parent` (the default) makes it the parent of the request span;
`link` starts a new trace whose request span links to it; `ignore` reads neither header.

### 3. Spans

One trace per request. Span names are fixed; `capyctl.` names are CapyCTL's own.

| Span | Kind | Parent | From → to |
|---|---|---|---|
| `capyctl.request` | server | caller's context, or none | headers read → last byte of the answer to the client (`[DONE]` for a stream), or the refusal |
| `capyctl.admission` | internal | request | ID and trace checks → body read, validated and resolved to a deployment, or refused |
| `capyctl.activation_wait` | internal | request | the deployment is not servable → the joined activation settles or the wait times out (§4) |
| `capyctl.queue_wait` | internal | request | pending ticket taken → in-flight slot granted, or refused (`queue_full`, deadline) |
| `capyctl.dispatch` | internal | request | instance selection → durable lease granted or refused; one per attempt |
| `capyctl.forward` | client | request | engine or ingress request sent → engine response ended, cut or failed; one per attempt |
| `capyctl.settle` | internal | request | lease close or the cancellation's settlement (ADR 0032), possibly after `capyctl.request` ended |
| `capyctl.ingress` | server | the router's `capyctl.forward` | ingress receives the request → its upstream response ends (host role, §6) |

`capyctl.forward` gets one event, `first_progress`, at the first backend-progress chunk
(SPEC §10's definition), with no attributes, so a trace shows time to first token. A
failover after a refusal before acceptance adds a `capyctl.dispatch` and a
`capyctl.forward` per attempt under the same request span and request ID.

A cancellation that settles later (on SGLang by the request's final output, otherwise on
the engine's idle reading or the instance's verified exit, SPEC §10 and ADR 0032) records
`capyctl.settle` when it settles, with the request span as parent. The router keeps the
request's span context (trace ID, span ID, flags: 25 bytes) in memory beside the open
lease until then; a role restart loses it, and the settlement is then not traced. A
child span ending after its parent is valid OpenTelemetry.

The `queue_wait` and `activation_wait` spans are measured separately; the existing
`router_queue_wait` timing is not changed by this ADR.

### 4. Park, wake, cold start and switch

An activation (SPEC §10 steps 2–7: drain and park or stop the victim, reserve, launch or
wake, readiness, wake canary) serves every request that joined it, so it is **its own
trace**, rooted at `capyctl.activation`, with child spans `capyctl.drain`,
`capyctl.release` (park or stop of the victim, with verified release), `capyctl.reserve`,
`capyctl.launch` or `capyctl.wake`, `capyctl.readiness` and `capyctl.canary`. Each
request's `capyctl.activation_wait` carries a **span link** to `capyctl.activation`, and
says whether it started the activation or joined one. An activation started by an
operator (`capyctl wake`, a switch) is traced the same way, without waiting requests.

Activation traces are recorded whenever tracing is on, whatever the sampling ratio (§9):
there are few of them, and they explain the longest waits. A request that waits for no
activation has no `activation_wait` span.

On a remote host the server sends the parent context with each lifecycle command (§6), and
the host records its steps (`capyctl.host.launch`, `capyctl.host.wake`,
`capyctl.host.park`, `capyctl.host.stop`, `capyctl.host.verify_release`) as children of the
server's step, when the host has an endpoint.

### 5. What a span may carry: an allow-list

CapyCTL spans carry only these attributes; a test fails on any other key (T48):

| Attribute | Value |
|---|---|
| `capyctl.request_id` | `X-CapyCTL-Request-Id` (a ULID) |
| `capyctl.endpoint` | `chat`, `decisions`, `embeddings` |
| `capyctl.deployment`, `capyctl.revision`, `capyctl.instance`, `capyctl.host` | CapyCTL's names, only after the request resolved to a deployment |
| `capyctl.generation`, `capyctl.attempt` | integers |
| `capyctl.engine`, `capyctl.engine_version` | for example `sglang`, `0.5.22` |
| `capyctl.stream` | boolean |
| `capyctl.outcome` | `completed`, `refused`, `failed`, `cancelling`, `uncertain` |
| `capyctl.code` | a CapyCTL error or refusal code from its closed set (`queue_full`, `invalid_request`, ...) |
| `capyctl.cancel_cause` | `client_hangup`, `deadline`, `stream_idle` |
| `capyctl.activation.kind`, `capyctl.activation.started` | `cold_start`, `wake`, `switch`, `restart`; whether this request started it |
| `capyctl.park_tier`, `capyctl.victim` | on activation spans: the tier, the victim deployment |
| `http.request.method`, `http.route`, `http.response.status_code` | the fixed route, never a query |
| `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens` | from the engine's `usage`, when it reported them |
| `gen_ai.response.finish_reasons` | from the engine's closed set (`stop`, `length`, `tool_calls`, ...) |

Never, in an attribute, event, status description or resource: any part of a request or
response body (messages, system prompts, tool definitions and arguments, reasoning,
completions, embedding inputs or vectors); the `model` string as the caller sent it (a
request for an unknown model records only its refusal code); `cache_salt` or its digest
(ADR 0033 §5); the caller's `X-Request-Id`; any key, token or `Authorization` value; the
client's address or user agent; an engine's error body (an engine's validation error can
quote the request) or log line; exception text. An error span's status description is its
CapyCTL code.

The resource carries `service.name` (`capyctl-server`, `capyctl-host` or
`capyctl-standalone`), `service.version` and `capyctl.role`; no machine ID, host
identifier or address. Deployment, instance and host names are the operator's own names;
the guide says they appear in spans.

### 6. Propagation

**To a host's ingress.** The router sends `traceparent` (its `capyctl.forward` span as the
parent, the sampled flag as decided) and the caller's `tracestate` with the forward,
only to a host that declares the capability **`trace_context`** (ADR 0017: the server
sends no field a host did not declare; an older host gets neither header, and the trace
ends at the router's forward span). The ingress reads them only on a request it has
authenticated as the router's, ignores them if malformed, records `capyctl.ingress` when
the host has an endpoint, and otherwise passes the router's context on unchanged (W3C
forwarding).

**In lifecycle commands.** The server puts the activation step's context in the command
**envelope**, outside the journaled command payload, so journaled digests and replay are
unchanged (SPEC §13.1); a command replayed after a restart carries no parent. Host spans
use the host's clock; skew between machines shows as is.

**To an engine.** Only when that launch runs with engine tracing (§7): the router (local
engine) or the ingress (remote engine) sends `traceparent`, with CapyCTL's forward or
ingress span as the parent, and `tracestate`. No other engine receives either header:
llama.cpp, TensorFold and custom engines never do, and neither does a vLLM or SGLang
launch without engine tracing, where the header would only be read and dropped.

### 7. Engine tracing

A host setting, `observability.tracing.engines: off | on` (default `off`), turns engine
spans on for the launches on that host that support them. With `on`:

- **vLLM 0.31.0 or newer:** `--otlp-traces-endpoint <host endpoint>` and, in the engine's
  environment, `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL=http/protobuf`,
  `OTEL_SERVICE_NAME=vllm` and `OTEL_RESOURCE_ATTRIBUTES` naming the deployment and
  instance. `--collect-detailed-traces` is never rendered.
- **SGLang 0.5.22 or newer:** `--enable-trace --otlp-traces-endpoint <host endpoint>
  --otlp-service-name sglang`, `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL=http/protobuf` and
  `SGLANG_TRACE_LEVEL=1` (important slices only) [the level is to be confirmed by OT8];
  `SGLANG_TRACE_ASYNC` stays off, so no exporter processes are added to the launch's
  group (ADR 0027).
- **Every other engine:** off, with the reason.

Rules:

- **Loopback only.** `engines: on` requires the host's endpoint to be on loopback (a local
  OpenTelemetry Collector, which may forward with TLS and its own credentials). Both
  engines' gRPC exporters run without TLS, and an engine is third-party code reaching
  off its process; keeping its export on the machine keeps SPEC §13.3's egress rule and
  `engine_telemetry`'s promise that engines report nothing off the host by CapyCTL's
  doing. Another endpoint refuses the role's start.
- **Launch-time check.** Before rendering, the adapter checks by shape that the engine's
  environment can import the OpenTelemetry SDK and the OTLP HTTP exporter, executing
  nothing (as ADR 0033 §1 does for LMCache). Without them the engine runs without
  tracing, with `engine_tracing_off:not_installed`, rather than failing its start.
- **Owned options and names.** With the implementation the engines' tracing options
  become **owned** in every spelling, whatever the host approves: vLLM
  `--otlp-traces-endpoint`, `--collect-detailed-traces`; SGLang `--enable-trace`,
  `--otlp-traces-endpoint`, `--otlp-service-name`, `--trace-modules`. Environment names
  `OTEL_*`, `SGLANG_TRACE_*`, `SGLANG_OTLP_*`, `traceparent`, `tracestate`, `TRACEPARENT`
  and `TRACESTATE` become reserved (`engine_env_reserved:<name>`). vLLM reads a
  `traceparent` from its own environment as a fallback parent (`tracing/otel.py`), and
  the OpenTelemetry Python SDK reads `OTEL_*` for endpoint, headers, sampler and
  resource. An existing approval of `--otlp-traces-endpoint` stops working (owner
  decision 2026-10-27, question 7).
- **Groups.** Multi-node groups (ADR 0028) run without engine tracing (`group`) until a
  live row covers them; on-host multi-GPU launches are single launches and follow the
  rules above.
- **SGLang's machine ID.** SGLang's thread spans export `/etc/machine-id` verbatim
  (Context). With the endpoint on loopback, that ID reaches the operator's own collector;
  the guide says so and shows a collector rule that deletes `host_id`, and an upstream
  report is drafted (owner decision 2026-10-27, question 9).

`engine_tracing_off` reasons: `host_off`, `engine_unsupported`, `not_installed`,
`group`, `retention_zero` (§11).

### 8. Export

- **Protocol.** OTLP over HTTP with protobuf encoding, through `opentelemetry-otlp` on the
  workspace's `reqwest` and `rustls`. No gRPC.
- **Never on the request path.** Spans go to a bounded queue (2048 spans, the SDK
  default) drained by one background task every 5 seconds in batches of up to 512.
  When the queue is full, spans are dropped and counted; an export failure is retried by
  the SDK's rules and then dropped. Neither delays, fails or changes a request, a
  readiness check or a lifecycle step. A role log notice reports dropped spans and export
  failures at most once a minute.
- **Credentials.** A collector that needs headers (an API key) gets them from
  `observability.tracing.headers_file`, a file under the same ownership and permission
  rules as CapyCTL's private state (SPEC §13.3); values are never logged or shown by
  `config show`.
- **Plain HTTP.** An `http://` endpoint that is not on loopback prints a warning at start,
  like `authentication: none` on a network bind (ADR 0019).
- **No ambient configuration.** CapyCTL ignores `OTEL_*` environment variables: the
  exporter, sampler and resource are built from its own settings only (an empty
  resource, no environment detector), so tracing never turns on, or reaches another
  endpoint, because a service manager's environment carries such a variable. CapyCTL's
  own precedence (`--set` > `CAPYCTL_SET__` > YAML > default) is the only way in.
- **Shutdown.** A role stopping flushes the queue for at most 2 seconds.

### 9. Sampling

`ParentBased(root = TraceIdRatioBased(observability.tracing.sample_ratio))`, the ratio
`1.0` by default, which is the OpenTelemetry default (`parentbased_always_on`). With a
caller context, the caller's sampled flag decides. An unsampled request records nothing
but still propagates its context with the sampled flag clear, so an engine with tracing
does not record it either. Activation traces ignore the ratio (§4). An authenticated
caller can force sampling of its own requests; the bounded queue limits what that costs.

### 10. Request IDs

- `X-CapyCTL-Request-Id` is `capyctl.request_id` on `capyctl.request` (the fresh ULID for
  a request refused before any lease). It is the join key between a trace, the role logs
  (`capyctl_request_id`), status and the answer header. On an ADR 0032 SGLang launch,
  SGLang's span carries the same value as `gen_ai.request.id`.
- The trace ID is not derived from the request ID or the other way round: a request keeps
  one request ID across failover attempts and has one trace; a caller's trace may span
  many requests.
- The caller's `X-Request-Id` stays what SPEC §10 says: echoed, in role logs, nowhere
  else; it is not a span attribute (owner decision 2026-10-27, question 5).
- CapyCTL returns no trace header on the answer (W3C defines none for responses): a
  caller that sent `traceparent` knows its trace ID, and any
  request's spans can be found by `capyctl.request_id`.

### 11. Zero retention and engine usage reporting

CapyCTL's spans are operational metadata, like its role logs: identifiers, timings,
codes and token counts, never content (§5). Like role logs, they are not covered by
`retention: zero`. Engine spans are produced by code CapyCTL does not control, so a
deployment with `retention: zero` (its own or its host's) gets **no engine tracing**
(`engine_tracing_off:retention_zero`) and no trace header reaches its engine; CapyCTL's
own spans for its requests are kept (owner decision 2026-10-27, question 6).

`engine_telemetry` is unchanged: it concerns the engines' own usage reports to their
vendors. Engine tracing is a separate host choice, and its export never leaves the
machine by CapyCTL's doing (§7).

### 12. Configuration

Server document (standalone: `server.observability.tracing`, which covers the whole
process, router and its embedded host):

```yaml
observability:
  timing_header: false
  tracing:
    endpoint: https://collector.example.internal:4318/v1/traces  # absent: off (default)
    headers_file: /etc/capyctl/otlp-headers   # optional
    sample_ratio: 1.0                         # 0.0 to 1.0
    caller_context: parent                    # parent | link | ignore
```

Host document (standalone: `host.observability.tracing` accepts only `engines`, and the
engines export to the server block's endpoint, which must then be on loopback):

```yaml
observability:
  tracing:
    endpoint: http://127.0.0.1:4318/v1/traces  # absent: the host records no spans
    headers_file: /etc/capyctl/otlp-headers
    engines: off                               # off | on; on needs a loopback endpoint
```

Every field is settable by path (`--set observability.tracing.endpoint=...`,
`CAPYCTL_SET__OBSERVABILITY__TRACING__ENDPOINT=...`; standalone with the `server.` or
`host.` prefix). Anything else, a ratio outside `[0, 1]`, an endpoint that is not an
`http` or `https` URL, `headers_file` without an endpoint, or `engines: on` without a
loopback endpoint refuses the start (T03). The settings are role-local, like
`engine_logs`: they never reach the document deployments are resolved against, and no
deployment or profile field changes them.

### 13. Status and documentation

- `capyctl status` shows, per role, `tracing: off` or `tracing: on → <endpoint>`, with
  spans dropped and the last export error since the start; per instance,
  `engine tracing: on` or `off (<reason>)`. JSON output carries the same fields.
  `config show` lists the settings, `headers_file` by path only.
- `docs/operations/configuration.md`: a "Tracing" section.
- `docs/operations/data-retention.md`: a summary row and a "Traces" section (what spans
  hold, that they go only to the configured collector, memory-only queue in CapyCTL,
  the collector's retention, SGLang's machine ID, zero retention's scope).
- `docs/guide/requests.md`: "Trace context" beside the request IDs: what a caller sends,
  what it gets, how to find a request's trace.
- `docs/guide/engines.md` and `docs/guide/engine-flags.md`: engine tracing, the owned
  options and reserved names.
- `docs/operations/network-access.md`: a network bind accepts the headers only from
  authenticated callers.

### 14. Cost

- **Off:** none; no header is parsed and nothing runs.
- **On, per traced request:** about seven spans (one more `dispatch` and `forward` per
  extra attempt), each a few hundred bytes of attributes [INFERENCE: not measured];
  span creation is microseconds against requests that take hundreds of milliseconds or
  more [INFERENCE]. The queue bounds memory at 2048 spans [INFERENCE: on the order of
  2 MiB]. One export task and one HTTP connection per role.
- **On, unsampled:** the header parse and non-recording spans.
- **Engines:** vLLM records one span per request at output processing; SGLang's slices
  cost more at large batches by its own documentation, hence level 1. OT8 measures router
  overhead and engine throughput with tracing on and off; no figure is promised
  (SPEC §17).

## Consequences

- New dependencies: `opentelemetry`, `opentelemetry_sdk`, `opentelemetry-otlp` 0.33.x
  (HTTP, `reqwest`, `rustls`), audited and pinned in `Cargo.lock`.
- New instrumentation at stage boundaries in `capyctl-router` (admission, queue,
  activation join, dispatch, forward, settlement), `capyctl-controller` (activation steps),
  `capyctl-agent` (ingress, host lifecycle steps) and `capyctl-adapters` (engine
  rendering, launch-time check); the trace context travels beside the request ID.
- A host capability, `trace_context`, and an additive command-envelope field.
- The engines' tracing options move from approval to owned: a deployment that passes an
  approved `--otlp-traces-endpoint` today is refused after the upgrade and must use the
  host setting (release note; owner decision 2026-10-27, question 7).
- The data retention page gains a category that leaves the role: spans exported to an
  endpoint the operator named. The default stays: nothing.
- Operator names (deployments, hosts, instances) reach the collector.
- Callers that already send `traceparent` see no change until an operator turns tracing
  on; then their traces gain CapyCTL's spans.
- CPU and Fake-engine tests are not qualification; engine propagation is unproven until
  OT5 and OT6 pass.

## Alternatives considered

- **The `tracing` crate with `tracing-opentelemetry`.** Rejected: CapyCTL has its own
  role log and no subscriber; adopting `tracing` would add a second logging convention
  and make every `#[instrument]` a place where arguments (bodies) could leak into spans.
  Explicit spans at a few boundaries keep the allow-list checkable.
- **Pass-through only** (forward the caller's headers to engines, no CapyCTL spans).
  Rejected: the time CapyCTL adds (queue, wake, switch) is what callers cannot see, and
  SPEC §13.3 asks for internal headers to be replaced, not relayed.
- **Refuse malformed trace headers with 400**, as for `X-Request-Id`. Rejected: W3C says to
  restart the trace, and proxies add these headers (owner decision 2026-10-27,
  question 2).
- **Activation spans as children of the first waiting request.** Rejected: the activation
  serves every joined request, and an operator's switch has no request; links express
  many-to-one.
- **Derive the trace ID from the request ID.** Rejected: one request ID, one trace, but a
  caller trace spans many requests, and W3C Level 2's random-trace-id flag expects
  random trace IDs, which a ULID's timestamp half is not.
- **OTLP over gRPC as well.** Rejected for this ADR: it adds a gRPC stack for no case
  HTTP does not cover; a collector accepts both.
- **Honour `OTEL_*` environment variables.** Rejected: tracing could turn on silently from
  a unit's environment; CapyCTL's settings already have a precedence chain.
- **Engine tracing through host approval of extras (today).** Rejected: CapyCTL would not know which launches trace, so it could not decide
  where to send `traceparent`, and an approved endpoint can be off the host (owner
  decision 2026-10-27, question 7).
- **Traces written to a local file.** Rejected: unbounded disk growth of request
  metadata, against the data retention page's shape; a local collector does this if
  wanted.
- **OpenTelemetry metrics and logs.** Out of scope: the latency and load figures already
  have a JSON endpoint; a later ADR can export them.
- **B3 or other propagation formats.** Out of scope: W3C Trace Context is the standard both
  engines read.

## Verification

A new conformance row, **T48 — Trace context**: with no endpoint, no trace header is
parsed, no exporter or task exists, no connection is opened, and nothing is sent to an
ingress or engine (requests with and without `traceparent` produce identical outbound
requests); the parser accepts and rejects the W3C vectors (valid `00`; uppercase hex,
all-zero trace or parent ID, version `ff`, short higher version, two `traceparent`
headers: ignored, a new trace, `tracestate` dropped; a higher version with extra fields
accepted; `tracestate` joined across headers, dropped over 512 bytes or 32 members or
without a valid `traceparent`) and never refuses a request for them; a request failing
authentication creates no span; spans, names, kinds and parentage match §3 for a
streamed and a collected chat request, a decision, a queue wait, a failover, a refusal at
each stage, a cut (hang-up, deadline, idle) with its later `capyctl.settle`, and an
activation (own trace, linked from each joined request's `activation_wait`, recorded at
ratio 0); every exported attribute key is in §5's allow-list; requests whose prompt,
messages, tool arguments, `cache_salt`, `model` string, `X-Request-Id`, API key and
engine error body each carry a sentinel export no byte of any sentinel (serialized spans
searched); `caller_context` `parent`, `link`, `ignore`; sampling by ratio and by the
caller's flag, with unsampled context still propagated with the flag clear; the
`traceparent` sent to the ingress has the forward span as parent and reaches only a host
declaring `trace_context`; the ingress ignores the headers on an unauthenticated request,
rebuilds them for the engine only on a launch with engine tracing, and passes the context
on when the host has no endpoint; the command envelope carries the context and the
journaled payload and digest are unchanged; engine rendering is exactly §7's for vLLM and
SGLang and absent for every other engine, a group, `engines: off`, `retention: zero` and
a missing package (with each reason); the owned options are refused in every spelling
and the reserved names with `engine_env_reserved:<name>`; a non-loopback endpoint with
`engines: on`, a bad ratio and the other invalid settings refuse the start; `OTEL_*` in
the role's environment changes nothing; a full queue drops and counts spans, and an
unreachable collector delays no request (bounded test clock); `headers_file` values never
appear in logs, `config show` or status. Tests also tag T03, T17, T19, T21, T37, T40,
T43 and T45 where they apply.

## Live qualification

Run in a live session the owner schedules (one live session on the hosts at a time), on
host A (and host B for the remote rows), with the approved environments
(`~/vllm-0.32.0-venv`, the approved SGLang 0.5.22 environment, llama.cpp v0.7.0) and an
OpenTelemetry Collector on loopback writing what it receives to a file. The collector is
the OpenTelemetry Collector release binary, placed in the home directory on host A and on
host B (owner decision 2026-10-27, question 11: the owner approved this new program on the
lab hosts; it is not an engine environment). Each runs with an OTLP HTTP receiver bound to
loopback and a file exporter writing into the session's evidence directory; the binary's
release version and checksum are recorded in the evidence. It is stopped at the end of the
session. No receiver script is added to `scripts/live/`. Qwen3-4B for chat on each engine, Clef-Flash for
decisions on SGLang.

| Row | What it shows |
|---|---|
| OT1 | Off by default: no endpoint; requests with a valid `traceparent` are answered as before; `ss` shows no connection to the collector ports from any CapyCTL or engine process; the engine processes' command lines and environments carry no tracing option or `OTEL_*` name. |
| OT2 | CapyCTL spans, standalone: a streamed and a collected chat request with a caller `traceparent` (sampled) produce §3's spans in the caller's trace, `capyctl.request_id` equal to the answer's `X-CapyCTL-Request-Id`; a decision likewise; without `traceparent`, a new trace; with a malformed one, a new trace and a normal answer. |
| OT3 | Nothing of content leaves: requests carrying sentinels in messages, a system prompt, tool arguments, `cache_salt` and `X-Request-Id`, plus one refused for an unknown sentinel model and one hitting an engine validation error that quotes its input; the collector's file holds none of the sentinels, CapyCTL's spans and the engines' spans alike (with OT5's engines on). |
| OT4 | Park and wake: a request to a deep-parked deployment shows `activation_wait` linked to an activation trace with `wake`, `readiness` and `canary`; a request that forces a switch shows the victim's `drain` and `release`; a cold start shows `launch`. Times agree with `x-capyctl-timing` within a millisecond per phase where both exist. |
| OT5 | Engine spans: with `engines: on`, vLLM 0.32.0's `llm_request` span and SGLang 0.5.22's request span are children of CapyCTL's `capyctl.forward`; on SGLang (one tokenizer worker), `gen_ai.request.id` equals `capyctl.request_id`; llama.cpp gets no trace header and its deployment shows `engine tracing: off (engine_unsupported)`. |
| OT6 | Remote host: server on the control host, host A remote, both with endpoints; `capyctl.ingress` is a child of the router's `capyctl.forward`; the engine's span is a child of `capyctl.ingress`; a wake on host A shows `capyctl.host.wake` under the server's `capyctl.wake`; host A without an endpoint passes the context on and the engine's span parents to the router's forward span. |
| OT7 | Collector down: the collector is stopped while requests run; every request is answered, router overhead within OT8's noise, the role log notice appears at most once a minute, status shows dropped spans; restarting the collector resumes export. |
| OT8 | Cost: 200 identical chat requests per arm on each engine, tracing off, on sampled, on unsampled, and with engine tracing; router phases from `x-capyctl-timing` and engine throughput (output tokens per second at a fixed concurrency); SGLang at trace levels 1 and 3. The record sets SGLang's level and is the cost the guide quotes. |
| OT9 | Zero retention: a deployment with `retention: zero` on a host with `engines: on` runs with `engine tracing: off (retention_zero)`, its engine gets no trace header (its spans are absent from the collector), CapyCTL's spans for its requests are present. |
| OT10 | Missing package: an engine environment without the OpenTelemetry SDK (determined in the row; [INFERENCE] the approved SGLang 0.5.22 environment may lack the `tracing` extra) runs with `engine tracing: off (not_installed)` and serves. |
| OT11 | Refusals: `engines: on` with a non-loopback endpoint refuses the host's start; an extra `--otlp-traces-endpoint` or an `OTEL_EXPORTER_OTLP_ENDPOINT` in a profile is refused. |

## Implementation plan (target 0.2.0)

1. **Settings.** `observability.tracing` in the server, host and standalone schemas and
   `remote_roles.rs`, with validation, `config show` and the start refusals (§12).
2. **Context and exporter.** A small `capyctl-trace` module (in `capyctl-domain` or its own
   crate): W3C parsing and formatting, the sampler, an exporter built from settings only,
   the bounded queue, the allow-listed attribute type (so a free-form attribute cannot be
   written), and a test exporter.
3. **Router.** Header reading after authentication; spans of §3; `capyctl.settle` with the
   stored context; the `traceparent` sent with the forward.
4. **Controller.** The activation trace, its steps and links; the context in the command
   envelope.
5. **Host.** The `trace_context` capability; ingress reading, span and propagation; host
   lifecycle spans.
6. **Adapters.** Engine tracing rendering for vLLM and SGLang, the launch-time check, the
   owned options and reserved names, reasons, status.
7. **Docs.** §13's pages, SPEC amendments, the engine flags tables, the release note,
   and a draft upstream report on SGLang's `host_id` attribute (question 9).
8. **Live rows** OT1–OT11, with the collector of the live qualification section (the
   live harness starts and stops it on host A and host B and collects its file), then the
   change file's status entry.

Everything here ships in 0.2.0 (owner decision 2026-10-27, question 12). Steps 1–4 do
not depend on any engine. Only if OT5 or OT8 fails does engine tracing (step 5's engine
part and step 6) move to a later release, with CapyCTL's spans and the ingress shipping in
0.2.0 without it.

## Owner decisions of 2026-10-27

The proposal of 2026-10-24 asked twelve questions; the owner answered all of them on
2026-10-27. The decision sections above already read as decided.

1. **Default:** off until an endpoint is named, as proposed (§1).
2. **Malformed `traceparent`:** ignored, and a new trace starts; the request is never
   refused for it (§2).
3. **Trusting the caller's context:** `caller_context: parent` by default, with `link`
   and `ignore` available (§2).
4. **Sampling default:** ratio 1.0 (§9).
5. **The caller's `X-Request-Id`:** never in spans; SPEC §10 unchanged (§10).
6. **Zero retention:** CapyCTL's spans kept, engine tracing off (§11).
7. **Existing approvals of engine tracing options:** owned by CapyCTL, refused in
   extras, with a release note for anyone who approved one (§7).
8. **Engine export endpoint:** loopback only (§7).
9. **SGLang's machine ID:** SGLang engine tracing is supported; the guide documents the
   `host_id` attribute and the collector rule that removes it, and an upstream report is
   drafted (§7, implementation plan step 7).
10. **Activation traces:** their own trace, linked from waiting requests (§4).
11. **Collector for the live rows (changed from the recommendation):** the OpenTelemetry
    Collector release binary in the home directory on host A and host B. The owner
    approved this new program on the lab hosts. The recommended receiver script in
    `scripts/live/` is not written (live qualification, implementation plan step 8).
12. **Scope for 0.2.0:** everything here ships in 0.2.0; engine tracing moves to a later
    release only if OT5 or OT8 fails (implementation plan).

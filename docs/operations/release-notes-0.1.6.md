# CapyCTL 0.1.6

CapyCTL 0.1.6 runs any engine you choose. `capyctl engine add --custom` registers
a program CapyCTL has no integration for, from a manifest that says how to start it and
which API it serves; CapyCTL starts it on a private loopback port, refuses it if it
listens anywhere else, and asks you to confirm the installation, which it then pins.
Every answer now carries a request ID, yours too if you send one, and decision answers
carry timing and token metrics. CapyCTL can keep no request data on disk: engine logs can
stay in memory, vLLM starts with request logging off, and a new data retention page lists
what is kept and how to turn it off. Three accounting bugs found by a formal review are
fixed. SGLang's options that dump to disk are reserved, and `capyctl engine flags` shows
which options CapyCTL owns. A role can run in a container, qualified on a GB10, and a new
guide shows how to drive CapyCTL from a controller program.

## Engines

- **SGLang options that dump to disk are refused.** `--watermark-config`,
  the `--debug-tensor-dump-*` options, `--msprobe-dump-config`,
  `--export-metrics-to-file`, `--export-metrics-to-file-dir` and
  `--spec-trace-dir` are reserved, whatever the host approves, and so are the
  `DUMPER_*`, `SGLANG_DUMP*`, `SGLANG_KERNEL_API_*` and
  `SGLANG_CUDA_COREDUMP*` variables and a few other SGLang dump variables.
  `capyctl engine flags <family>` prints which options and variables CapyCTL
  owns, reserves, gates behind an approval or passes unchanged, and why
  (`--format json` for scripts).
- **Custom engines.** `capyctl engine add --custom <program> --name <name>
  --manifest <file>` runs a program CapyCTL has no integration for, such as a
  model's own server, from a manifest that states its command, the API it
  serves (chat or decisions) and optionally a health path and an idle signal.
  CapyCTL owns the process, gives it a private loopback port, routes and admits
  requests, counts the memory you state in `resources` and keeps its log; it
  checks none of the program's options, does not park it, and stops a start
  whose engine listens off loopback. The engine is saved in
  `custom-engines.yaml` beside `engines.yaml`; an older CapyCTL ignores that
  file. See [Custom engines](../guide/engines.md#custom-engines).
- **Cancelled requests on an engine with no idle signal.** A request a client
  abandons on a custom engine without `idle` keeps its memory held until the
  engine's next stop or restart, since nothing shows that the engine stopped
  working on it. Stopping or switching models does not wait for such requests,
  and `capyctl status deployment` shows how many there are and when they
  clear.

## Requests

- **Request IDs.** Every answer from `/v1/chat/completions` and `/v1/systemone`
  carries `X-CapyCTL-Request-Id`, CapyCTL's ID for the request, also written in
  its logs. Send your own `X-Request-Id` (1 to 128 printable ASCII characters,
  no spaces) and it comes back on the answer and appears beside CapyCTL's ID in
  the router's logs; it is never sent to the engine or stored. Decision answers
  now carry `capyctl.metrics` too: total time, time to the engine's answer,
  input tokens, and the engine's own figures when it reports them. See
  [Request IDs and metrics](../guide/requests.md#request-ids-and-metrics).

## Decision models

- **The readiness question is published.** [Deploy a model](../guide/deploy.md#the-readiness-question)
  shows the exact request CapyCTL sends to prove a decision model ready, and
  what a ready answer must contain.

## Operations

- **Engine logs can stay in memory.** Set `engine_logs: memory` on a host (or
  `host.engine_logs: memory` in a standalone document) and CapyCTL writes no
  engine log file. It keeps the last 4 MiB of each launch's redacted output in
  memory for the management engine-log read and for the reason a failed launch
  gives, until a minute after the engine exits. `--debug-engine-logs` is refused
  with it. vLLM now starts with request and output logging explicitly off on
  every supported version, and neither can be turned back on. A new
  [data retention](../operations/data-retention.md) page lists what CapyCTL
  keeps about requests, for how long, and how to turn each part off.
- **Driving CapyCTL from a controller.** A new guide page shows how a program
  uses the management API to create, start, watch, replace and delete
  deployments: the admin token, the snapshot, idempotency keys and retries,
  revisions, the event stream, request IDs, which errors to retry, and how to
  leave every start to the controller. See
  [Driving CapyCTL from a controller](../guide/controller.md).
- **Running in a container.** A host or standalone role can run as the main
  process of a container. On a GB10 with Docker and CDI, a standalone role
  running as an unprivileged user started, parked, woke and stopped a
  llama.cpp decision model, SGLang 0.5.22 and vLLM 0.31.0, and measured each
  engine's GPU memory as it does on a host. CapyCTL publishes no image; the
  guide shows what to put in one. See
  [Running in a container](install.md#running-in-a-container).

## Fixes

- **A park that is cancelled or refused no longer reopens dispatch that a
  controller restart closed.** The instance serves again only after its
  readiness is proven in the new session.
- **A `resources:` block whose `ready` phase claims a device its `cold`,
  `parking` or `wake` phase does not is refused** when the deployment is
  written, instead of putting a second exclusive owner on the device when the
  instance becomes ready. A deployment stored before the upgrade with such a
  block keeps running and can be stopped; replace it with a block whose
  larger phases claim the device too.
- **A request whose client disconnects while it is being admitted no
  longer stays counted against its instance** until the controller restarts,
  where it refused parks of the instance and held switch drains to their timeout.

## Known issues

- **Custom engines run restart only, on one machine.** A custom engine cannot
  park and cannot span machines.
- **A custom engine without an idle signal settles cancelled requests only at
  its next stop.** Without `idle` in the manifest, a request a client abandons
  keeps its memory held until the engine's next stop or restart.
- **Containers are qualified on a GB10 only.** A role in a container has run
  live on a GB10 (unified memory) with Docker. A discrete GPU, Podman, and a
  host joined to a server have not yet been run live in a container.
- **basal answers every failure with HTTP 422.** In the guide's worked
  example, basal-serve 2.2.0 answers a malformed request and an engine failure
  (out of memory, an internal error) alike with 422, so CapyCTL passes either on
  as a rejected request. Read the end of the engine log to tell them apart.
  The issue is reported upstream.
- **basal's fp8 mode needs a package the tested environment lacks.**
  `--mode fp8` needs torchao; the example uses `--mode fast`, which takes many
  minutes to start cold, so deployments on it set `timeouts.initialize`.
- **llama.cpp can starve a decision request when several run at once.** With 4 or 8
  concurrent decision requests on llama.cpp v0.7.0, a few waited until every other request
  had finished; llama-server alone does the same, without CapyCTL. Set
  `engine_config: {max_concurrent_requests: 1}` there, or use SGLang when several
  clients share one decision model.
- **vLLM has no decision support yet.** A decisions deployment on vLLM is refused
  `capability_missing:decisions`.
- **A standalone request record trades durability for latency.** On a standalone
  machine a power loss can lose the last request records, which describe engines
  the power loss also stopped.

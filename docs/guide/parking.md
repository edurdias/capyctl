# Parking and switching

A GPU holds one or two models at a time. CapyCTL keeps the others parked: the
engine stays up but gives back its GPU memory. A request for a parked model
wakes it. The time depends on the engine, model and parking tier.

A deployment parks unless it says `residency: restart_only`, as long as its
engine supports parking: `capyctl engine list` shows `DEEP PARK enabled`.
[How it works](how-it-works.md#ready-parked-stopped) shows where the memory
goes in each state.

## How a model parks

CapyCTL picks the way from your hardware:

- On a discrete card, a parked model's weights are copied to host RAM, and a
  wake copies them back in seconds. When the copy does not fit in host RAM,
  the model parks deep instead.
- On unified memory, a model parks deep: its weights are dropped, and a wake
  reloads them from disk.

To choose yourself, set `residency: host_backed` (host RAM, discrete cards
only), `residency: deep` or `residency: restart_only` in the deployment.

## Park and wake

```bash
capyctl park deployment my-model
capyctl list deployments
```

Parking runs in the background. The first status query may show `parking`;
run `capyctl list deployments` again until it shows `parked`:

```text
NAME       STATE    READY   REVISION   HOSTS
my-model   parked   0/1     1          gpu-box
```

Send a request for `my-model` ([Make a request](requests.md)). CapyCTL wakes the
model and answers; the first answer takes longer. Afterwards:

```text
NAME       STATE   READY   REVISION   HOSTS
my-model   ready   1/1     1          gpu-box
```

## Switch between models

When a model needs memory that another one holds, CapyCTL parks the idle one.
Starting a second model with `--evict` allows that and names what it parked:

```bash
capyctl deploy model --file other-model.yaml
capyctl start deployment other-model --evict --wait
```

`--wait` waits for the checkpoint measurement, then for the model to be ready.
Without it, a newly deployed checkpoint can still be awaiting measurement and
the start is refused. When the start releases another model, its receipt includes
`victims` (the deployment and instance IDs) and `switch_id`.

After the start succeeds, check the states. When there is room to keep the first
model parked, the result looks like this:

```bash
capyctl list deployments
```

```text
NAME          STATE    READY   REVISION   HOSTS
my-model      parked   0/1     1          gpu-box
other-model   ready    1/1     1          gpu-box
```

Without `--evict`, a start never parks anything; it waits for memory instead.

A request switches the same way. Ask for `my-model` now and CapyCTL parks
`other-model`, wakes `my-model` and answers:

```text
NAME          STATE    READY   REVISION   HOSTS
my-model      ready    1/1     1          gpu-box
other-model   parked   0/1     1          gpu-box
```

CapyCTL waits for requests in progress to finish before it parks a model; it
never cuts an answer off. When a client hangs up in the middle of an answer,
CapyCTL stops the engine's work on it instead of letting it run to the end,
and waits until the engine reports no running or waiting requests before it
parks or switches. Two more cases end a streamed answer the same way: a client
that reads nothing for 10 seconds, and a single piece of the answer larger
than 64 KiB. When there is no room to keep a parked copy, CapyCTL
stops the idle model instead and says so:
`released: stopped (no room to park)`.

## How requests wait and models switch

Requests are routed by the `model` name only. CapyCTL does not pin sessions
or guess which requests belong together. When the GPU holds one model and
requests arrive for another, this is what happens:

1. A request for a model that is not ready waits. Every request waiting for
   that model joins the same start or wake; there is one switch per wave of
   waiting requests, not one per request. A client that hangs up while it
   waits gives up its own place only; the switch goes on.
2. The loaded model keeps taking new requests for the admission window
   (2 s by default), counted from the moment the switch began. Its own
   traffic does not extend the window. The window closes early once the
   loaded model has gone one window length with no request starting or
   finishing and has none running, so an idle model yields at once. A model
   that still has another ready instance elsewhere has no window.
3. Then the loaded model takes no new requests, and the ones it is running
   finish (the drain, up to 30 s by default). Nothing is cut off. If the
   drain does not finish in time, the switch fails, the loaded model keeps
   serving, and the waiting requests get a retryable error.
4. The loaded model parks, or stops when it does not park
   (`residency: restart_only`, TensorFold, or no room for a parked copy).
   The waiting model wakes or starts.
5. All its waiting requests are released together and run at the same time,
   up to 32 per model, not one by one in arrival order. Requests beyond 32
   wait for a free slot, first come first served.

When several models wait, the one whose requests have waited longest goes
next; a model with waiting requests is never the one parked to make room.
There is no minimum time a model stays loaded: if two apps keep asking for
two models in turn, the GPU switches on every turn, each costing the window,
the drain and the wake or start.

### Example: two apps, one GPU

An illustration with the default settings, not captured output. The GPU fits
`chat-a` or `chat-b`, not both. App 1 is streaming answers from `chat-a`.

| Time | What happens |
|---|---|
| 0 s | App 2 sends three requests for `chat-b`. They wait; one switch begins. |
| 0 to 2 s | `chat-a` still takes App 1's new requests (the admission window). |
| 2 s | `chat-a` takes no new requests and finishes the ones it is running. |
| after the drain | `chat-a` parks; `chat-b` wakes, or starts if it was stopped. |
| `chat-b` ready | App 2's three requests, and any more that arrived for `chat-b`, run together. |
| next | App 1's next request for `chat-a` waits and begins the switch back, under the same rules. |

App 1's requests that arrive after 2 s, while `chat-a` finishes its work, are
answered `503` (`dispatch to deployment <deployment id> is closed; retry
shortly`). Once `chat-a` has begun parking, they wait instead and become the
next switch.

### Settings

| What | Standalone | Server and hosts |
|---|---|---|
| Admission window | `host.resource_policy.queue.admission_window`, default 2 s | host: `resource_policy.queue.admission_window`, default 2 s, 1 ms to 30 s, not above the request deadline |
| Drain bound | `server.switching.drain_timeout`, default 30 s, 1 s to 600 s | server: `switching.drain_timeout`, default 30 s, 1 s to 600 s |
| How long a request may wait for its first output, the switch and the prompt's prefill included | `host.resource_policy.queue.request_deadline`, default 1800 s | host: `resource_policy.queue.request_deadline`, default 600 s, up to 3600 s |
| Longest silence in a reply after its first output | `host.resource_policy.queue.stream_idle_timeout`, default 120 s | host: `resource_policy.queue.stream_idle_timeout`, default 120 s, 1 s to 3600 s |
| Waiting requests per model | `host.resource_policy.queue.max_pending_per_deployment`, default 64 | host: `resource_policy.queue.max_pending_per_deployment`, default 64 |
| Waiting requests in total | `host.resource_policy.queue.max_pending_total`, default 256 | host: `resource_policy.queue.max_pending_total`, default 256 |
| Bodies of waiting requests, in total | `host.resource_policy.queue.max_buffered_bytes_total`, default 64 MiB | host: `resource_policy.queue.max_buffered_bytes_total`, default 64 MiB |
| Requests running at once per model | 32, fixed | 32, fixed |

The standalone bounds have the host ranges. A reply's opening chunk, which
only names the assistant role, is not output: TensorFold sends it before it
reads the prompt, so a long prompt is bounded by the request deadline, not by
the silence bound.

The admission window that applies is the one of the host where the switch
happens. With several hosts, the waiting limits and the request deadline are
the tightest any host sets. Each setting that is not fixed can also be given
with `--set` or a `CAPYCTL_SET__…` variable
([Settings](../operations/configuration.md)):

```yaml
# standalone.yaml: allow up to an hour before a reply's first output.
host:
  resource_policy:
    queue:
      request_deadline: "3600s"
```

### Errors while waiting

Each is an HTTP error with a JSON body holding `code`, `message` and
`"retryable": true`. `<deployment id>` is the deployment's ID.

| Status | Code | Message | Why |
|---|---|---|---|
| 503 | `unavailable` | `deployment <deployment id> did not become servable within the queue deadline; retry shortly` | The request waited the whole request deadline. |
| 503 | `unavailable` | `switch to <deployment id> failed: drain timeout after 30000 ms with <n> request(s) still charged; the victims serve again; retry shortly` | The loaded model did not finish its running requests within the drain bound. |
| 503 | `unavailable` | `dispatch to deployment <deployment id> is closed; retry shortly` | The model is finishing its work before it parks. |
| 429 | `queue_full` | `too many requests are waiting for deployment <deployment id>` | A waiting-request limit is reached. |
| 429 | `queue_full` | `waiting requests exceed the buffered-bytes bound` | The waiting bodies would pass the byte limit. |
| 429 | `queue_full` | `deployment <deployment id> stayed at its in-flight bound for the queue deadline` | 32 requests kept running for the whole request deadline. |

### Tuning

- A longer admission window lets the loaded model take more of its own
  requests before it yields, and lets more requests pile up for the other
  model, so each switch serves more and switches happen less often. The
  other model's requests wait longer for it.
- A longer drain bound helps when answers are long: fewer switches fail.
  It costs nothing when answers finish sooner; a switch waits only as long
  as the drain takes.
- When both apps are busy at the same time, a GPU that holds one model spends
  much of its time switching. Point both apps at one model, use models small
  enough to fit together, or give each model its own GPU.
- Give clients a timeout that covers a switch: the window, the drain, the
  wake or start, then the answer. A shorter one gives up while its request
  still waits.

## Idle models

CapyCTL stops or parks an idle model only when
`lifecycle_defaults.ready_idle_timeout` (or `parked_idle_timeout` for parked
ones) is set; both are off by default. In a server document:

```yaml
lifecycle_defaults:
  ready_idle_timeout: "5m"
  parked_idle_timeout: "30m"
```

In a standalone document the same block sits under `server`:

```yaml
server:
  lifecycle_defaults:
    ready_idle_timeout: "5m"
    parked_idle_timeout: "30m"
```

## Park or stop

| | Parked in host RAM | Parked, deep | Stopped |
|---|---|---|---|
| GPU memory | freed | freed | freed |
| Host RAM | engine and a copy of the weights | engine | none |
| A request | wakes it | wakes it | starts it after a pressure stop; refused after an operator stop |
| Back to ready | seconds | a reload from disk | a full start |

`capyctl stop deployment other-model` stops it; `capyctl start deployment other-model --wait`
starts it again.

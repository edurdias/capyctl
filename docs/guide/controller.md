# Driving CapyCTL from a controller

A program that decides which models run, such as a scheduler, a gateway or
your own automation, drives CapyCTL through the same management API the
`capyctl` command uses. There is no separate mode for it: the program is one
more user of that API. Requests to the inference endpoint still start and
wake models as they always do, unless you set
[`lifecycle.activation: explicit`](#leaving-every-start-to-the-controller).

This page calls that program the controller. It reads what CapyCTL has, decides
what it wants, sends the changes, and reads the results.

## The management API

The API listens on the server machine, or on the machine that runs CapyCTL on
its own, at `http://127.0.0.1:7443/management/v1`. It is plain HTTP with JSON
bodies and stays on loopback (any port, never another address), so the
controller runs on that machine. A role started with `--management-listen`
or `CAPYCTL_MANAGEMENT_ADDR` serves on that address instead, and records it in
`<state directory>/run/management-address`
([Settings](../operations/configuration.md#listeners)).

Every request carries the admin token, once, as a bearer token:

```bash
# CapyCTL on one machine:
TOKEN=$(sed -n 's/^admin_token: //p' ~/.local/state/capyctl/identity/credentials)
# Several machines, on the server machine:
TOKEN=$(sed -n 's/.*"admin_token": *"\([^"]*\)".*/\1/p' ~/.local/state/capyctl/identity/server-credentials.json)

curl -s http://127.0.0.1:7443/management/v1/snapshot -H "Authorization: Bearer $TOKEN"
```

The admin token is not the API key. The API key opens the inference endpoint
only, and the management API refuses it. A missing or wrong token is answered
`401`:

```text
{"api_version":"1","error":{"code":"unauthenticated","details":{},"message":"Management authentication required","operation_id":null,"retryable":false}}
```

Every error has this shape: a `code`, a `message`, and `retryable`, which
says whether the same request can succeed later
([Errors](#errors-retry-fix-or-stop)). Answers carry
`Cache-Control: no-store`.

| Request | What it does |
|---|---|
| `GET /management/v1/snapshot` | Every deployment, its instances and recent operations. |
| `GET /management/v1/events` | A stream of what changed, from a point in the snapshot. |
| `POST /management/v1/deployments` | Create a deployment. |
| `PUT /management/v1/deployments/{id}` | Replace a deployment's configuration. |
| `POST /management/v1/deployments/{id}/actions` | `start`, `stop`, `park`, `preinitialize` or `delete` a deployment. |
| `POST /management/v1/deployments/{id}/instances/{index}/actions` | `start` or `stop` one instance. |
| `GET /management/v1/deployments/{id}/effective-config` | The configuration CapyCTL resolved, with secrets redacted. |
| `GET /management/v1/metrics/load` | Live load of every model ([Reading load](parking.md#reading-load)). |
| `GET /management/v1/deployments/{id}/engine-log` | The end of an instance's engine log ([engine logs](../operations/install.md#engine-logs-and-troubleshooting)). |

In a change, `{id}` is the deployment ID, a 26-character ULID such as
`01M5E0MPXN61STKHGZ6BTEFVM3`; its name is refused `400 invalid_request`.

## The loop

1. Read the snapshot: what is deployed and what is running.
2. Compare it with what you want.
3. Send each change, with an `Idempotency-Key`.
4. Read the snapshot again, or follow the events, until each change has
   finished.

### Read the snapshot

`GET /management/v1/snapshot` takes no query and returns everything at once.
Trimmed to the parts a controller uses, for one model that is running:

```json
{
  "api_version": "1",
  "cursor": "01M5E0M1C4VQT1DXDZNMJ75NRN:12",
  "deployments": [{
    "id": "01M5E0MPXN61STKHGZ6BTEFVM3",
    "name": "my-model",
    "revision": "1",
    "desired_state": "ready",
    "observed_state": "ready",
    "desired_instances": 1,
    "ready_instances": 1,
    "conditions": [],
    "instances": [{
      "index": 0, "host_id": "gpu-box", "observed_state": "ready",
      "revision": "1", "lifecycle": "active", "operator_stopped": false
    }],
    "timeouts": {"request_deadline_ms": 1800000, "initialize_ms": 610000, "wake_ms": 65000, "stop_ms": 900000},
    "checkpoint_digest": {"state": "recorded", "host_id": "gpu-box", "provisional": false},
    "latest_operation": {"id": "01M5E0PW0GTNAZM76H039K3YFY", "kind": "initialize", "state": "succeeded"}
  }],
  "routes": [{"deployment_id": "01M5E0MPXN61STKHGZ6BTEFVM3", "route": "my-model"}],
  "operations": [{
    "id": "01M5E0PW0GTNAZM76H039K3YFY", "deployment_id": "01M5E0MPXN61STKHGZ6BTEFVM3",
    "action": "initialize", "state": "succeeded", "has_error": false,
    "accepted_at": "2026-10-21T01:20:15.888Z"
  }]
}
```

Top level:

- `cursor`: the point the [events](#follow-the-events) continue from.
- `deployments`: one entry per deployment.
- `routes`: each name clients send as `model`, and the deployment it belongs
  to.
- `operations`: each change CapyCTL accepted, with its `state` (`pending`,
  `running`, `succeeded`, `failed` or `cancelled`) and, on a failure, its
  `error_code`.

The snapshot holds other collections too. They are CapyCTL's own records, and
a controller does not need them.

For each deployment:

- `id`, `name`, and `revision`: the revision the next change must name.
  Revisions and generations are strings in the snapshot.
- `desired_state` and `observed_state`: what was last asked for and what the
  deployment as a whole is doing now.
- `desired_instances` and `ready_instances`: how many instances the
  configuration asks for, and how many serve now. `conditions` holds
  `degraded` while some, but not all, of them are ready.
- `instances`: each instance's `index`, the machine it runs on (`host_id`)
  and its `observed_state`. `last_error`, when present, says why it is not
  running, and `operator_stopped` is true after a `stop` of that one
  instance.
- `timeouts`: in milliseconds, the windows the deadlines of
  [actions](#start-stop-park-and-delete) are taken from.
- `checkpoint_digest.state`: `pending` until CapyCTL has measured the model's
  files once; a start waits for it.
- `activation`: `explicit` when only an operator starts the model; absent
  otherwise.
- `latest_operation`: the most recent change, with its `kind` and `state`, and
  on trouble a `reason`, a `hint` and `given_up: true` once a start stopped
  trying.

What is running is in each instance's `observed_state`:

| `observed_state` | Meaning |
|---|---|
| `ready` | Running and serving requests. |
| `parked` | Engine kept, GPU memory freed; the next request or `start` wakes it. |
| `queued` | Asked to start, waiting for room on a machine; `latest_operation.reason` says what it needs. |
| `starting`, `waking` | On its way to `ready`. |
| `parking`, `stopping` | On its way to `parked` or `stopped`. |
| `stopped` | Not running. |
| `failed` | A start gave up; `last_error` and `latest_operation.reason` say why. |
| `uncertain`, `reconciling` | CapyCTL cannot tell yet what the engine is doing, so it does not act on it. Wait. |

The snapshot does not record who stopped a stopped deployment. A request for a
deployment you stopped is refused `409 deployment_stopped` until you start it
again; one that was never started, or that CapyCTL stopped itself, starts on
the next request.

Read the snapshot about once a second at most. Two reads run at a time; a
third at the same moment is answered `429 queue_full`, not queued.

### Create a deployment

`POST /management/v1/deployments` takes the
[deployment file](deploy.md#the-deployment-file) a user would write, as JSON
under `config`, and `activate: false`:

```bash
curl -s -X POST http://127.0.0.1:7443/management/v1/deployments \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: create-my-model-1' \
  -d '{"config": {"name": "my-model", "engine": "vllm", "model": "Qwen3-4B"}, "activate": false}'
```

```text
{"api_version":"1","checkpoint_digest":"pending","deployment_id":"01M5E0MPXN61STKHGZ6BTEFVM3","joined":false,"operation_id":"01M5E0MPXNHG2XSYTE57ZQX64A","revision":"1"}
```

The answer is `202`: the deployment is saved, at revision 1, and nothing
runs yet. Start it with a [start action](#start-stop-park-and-delete).
`"activate": true` is refused with `503 unsupported_capability`; create, then
start.

Two things the `capyctl` command does for you are left to the controller:

- A `~/` path is refused. Send an absolute path, or a directory under the
  models directory, as `Qwen3-4B` above.
- A Hugging Face reference must name its commit,
  `{"hf": "Qwen/Qwen3-4B-Instruct-2507@<40-character commit>"}`; CapyCTL does
  not pin one for you here.

A document CapyCTL refuses is answered `400 invalid_config`, with the field in
`details.path` and the reason in `message`:

```text
{"api_version":"1","error":{"code":"invalid_config","details":{"path":"model.hf"},"message":"Invalid deployment configuration: unsupported combination at `model.hf`: `Qwen/Qwen3-4B` is not pinned to a commit: ...","operation_id":null,"retryable":false}}
```

A name another deployment already has is `409 route_conflict`.

### Replace a deployment

`PUT /management/v1/deployments/{id}` takes the whole new document and the
revision you read:

```bash
curl -s -X PUT http://127.0.0.1:7443/management/v1/deployments/$ID \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: replace-my-model-2' \
  -d '{"config": {"name": "my-model", "engine": "vllm", "model": "Qwen3-4B", "residency": "restart_only"}, "expected_revision": 1}'
```

```text
{"api_version":"1","checkpoint_digest":"pending","deployment_id":"01M5E0MPXN61STKHGZ6BTEFVM3","joined":false,"operation_id":"01M5E0QJVDN7X25DBQ0RP3EHTT","revision":"2"}
```

A running model is restarted on the new revision; a change to the instance
count alone leaves running instances as they are. Whether it was running or
not stays the same.

### Start, stop, park and delete

`POST /management/v1/deployments/{id}/actions` takes the action, the
revision you read, and a deadline:

```json
{"action": "start", "expected_revision": 1, "deadline_ms": 1792546226000}
```

`deadline_ms` is a time, in milliseconds since 1970 (Unix time), by which the
action must finish. Take it as now plus the deployment's window from
`timeouts`, as `capyctl` does: `initialize_ms` for `start`, `stop_ms` for
`stop`, `park` and `delete`, `request_deadline_ms` for `preinitialize`. A
deadline already past, or more than `request_deadline_ms` ahead, is refused
`400 invalid_request`. So is a revision sent as a string: send it as a
number.

The answer is `202` with the operation that carries the action out:

```text
{"api_version":"1","deployment_id":"01M5E0MPXN61STKHGZ6BTEFVM3","joined":false,"operation_id":"01M5E0PW0GTNAZM76H039K3YFY","revision":"1"}
```

`202` means accepted, not done; [watch](#watch-the-result) for the result.
`joined: true` means the action joined one already under way, for example a
start a request set off; `operation_id` is then that one.

| Action | What it does | Refused with |
|---|---|---|
| `start` | Starts stopped instances and wakes parked ones. Lifts an earlier `stop`. | `503 checkpoint_digest_pending` while the model's files are measured after a create; `503 still_stopping` right after a stop; `409 runtime_retained` when every instance is already running. |
| `stop` | Stops every instance. Requests are then refused (`409 deployment_stopped`) until a `start`. | |
| `park` | Parks every ready instance ([Parking](parking.md#park-and-wake)). | `503 unsupported_capability` for `residency: restart_only`. |
| `preinitialize` | Starts, checks and parks each instance in turn, so the first request only wakes it. | `503 unsupported_capability` for `residency: restart_only`. |
| `delete` | Removes the deployment and its name. The model files stay. | `409 delete_requires_cleanup` until every instance is stopped. |

A start may add `"evict": true` to park or stop other models to make room, as
`capyctl start deployment --evict` does; no other action takes it.

`capyctl delete deployment --stop` is two actions: `stop`, then, once
`observed_state` is `stopped`, `delete`. The deployment leaves the snapshot as
soon as the delete is accepted.

`POST /management/v1/deployments/{id}/instances/{index}/actions` takes the
same body for one instance, and only `start` and `stop`.

### Watch the result

Read the snapshot until the operation has finished:

- For a `start`, wait until `ready_instances` equals `desired_instances`. The
  start has failed when `latest_operation.state` is `failed`, or when it shows
  `given_up: true`: the operation then stays `pending` until its deadline, but
  nothing more is tried. An instance can also stay `queued` while it waits for
  memory; `latest_operation.reason` says how much it needs, for example
  `insufficient_device_memory: needs 13.2 GiB of gpu0 memory, 1.6 GiB available`.
- For a `stop`, wait until `observed_state` is `stopped`.
- For the others, find the `operation_id` in `operations`, and wait for
  `succeeded`, `failed` or `cancelled`.

`capyctl start deployment --wait` reads the snapshot every half second to do
the same.

### Follow the events

`GET /management/v1/events` streams each change as it is recorded, as
server-sent events. Ask for the events after the snapshot's `cursor`, so you
miss none and see none twice:

```bash
curl -sN "http://127.0.0.1:7443/management/v1/events?after=$CURSOR" \
  -H "Authorization: Bearer $TOKEN"
```

```text
id: 01M5E0M1C4VQT1DXDZNMJ75NRN:12
event: ready_committed
data: {"api_version":"1","deployment_id":"01M5E0MPXN61STKHGZ6BTEFVM3","operation_id":"01M5E0PW0GTNAZM76H039K3YFY","payload":{...},"recorded_at_ms":"1792545616717"}
```

The `operation_id` is the one the action answered with. Kinds a controller
watches for include `managed_configuration_accepted` (a create or replace),
`ready_committed`, `initialize_failed_released` (a launch failed),
`parked_committed`, `restored_committed` (a wake), `ordinary_cleanup_completed`
(a stop finished) and `deployment_deleted`.

The stream closes after five minutes; reconnect with the last `id` you
received, as `?after=<id>` or in a `Last-Event-ID` header. A comment line
arrives every 15 seconds while nothing happens. If the point you ask for is
too old, the answer is `410 cursor_expired` (or, on an open stream, a
`management_stream_error` event with `"resnapshot_required": true`): read the
snapshot again and continue from its `cursor`. Sixteen streams can be open at
once; one more is answered `429 queue_full`.

Use an event as the signal to read the snapshot again, and the snapshot as
the state.

## Idempotency keys

Every change (`POST` and `PUT`) needs an `Idempotency-Key` header, one per
change: 1 to 256 characters, not all spaces, with no control characters. A change
without one is refused `400 invalid_request`. CapyCTL keeps the answer to each
change it accepts under its key:

- Sent again with the same key and the same body, a change is not made twice.
  It gets the first answer again, the same `deployment_id`, `operation_id` and
  `revision`. This is how to retry when the answer was lost: a timeout, a
  dropped connection, `504 deadline_exceeded`.
- The same key with a different body is refused `409 idempotency_conflict`.
  The body includes `deadline_ms`, so compute the deadline once and keep it
  with the key; a retry with a new deadline is a different change. Field
  order and spacing do not matter.
- A change CapyCTL refused was not recorded, so it can be sent again under its
  key once the reason has passed, for example after `checkpoint_digest_pending`.

Keep the key and the body until the change is answered. A key built from what
the change is for, such as `start-my-model-<your run id>`, makes a retry after
the controller itself restarts send the same key.

## Revisions

Every replace and action names the revision it was decided on, in
`expected_revision`. When the deployment has moved on (someone replaced it
since you read the snapshot), the change is refused and nothing happens:

```text
{"api_version":"1","error":{"code":"revision_conflict","details":{},"message":"Expected revision does not match","operation_id":null,"retryable":false}}
```

with status `409`. Read the snapshot again, decide again against the new
revision, and send the change under a new key. Actions do not change the
revision; only a replace does.

## Request IDs

Requests to the inference endpoint carry IDs a controller or gateway can match
across systems ([Request IDs and metrics](requests.md#request-ids-and-metrics)):

- Send your own `X-Request-Id` (1 to 128 printable ASCII characters, no
  spaces) and it comes back on the answer and in CapyCTL's logs beside its own
  ID. It is never sent to the engine or stored.
- Every answer carries `X-CapyCTL-Request-Id`, CapyCTL's ID for the request,
  also in its logs.

The management API neither reads nor returns these headers. Match a
management change by its `Idempotency-Key` and the `operation_id` it was
answered with, which the snapshot's `operations` and `latest_operation` and
every event about it carry.

## Errors: retry, fix or stop

`retryable` decides it. When it is `true`, send the same request again, with
the same key and body, after a pause (a second or two, longer each time).
When it is `false`, sending it again gets the same answer: fix the request or
stop.

| Status | Code | What to do |
|---|---|---|
| 429 | `queue_full` | Retry: too many reads, changes or streams at once. |
| 504 | `deadline_exceeded` | Retry with the same key: the answer took too long, and the change may have been made. |
| 503 | `checkpoint_digest_pending`, `model_source_pending` | Retry: the model's files are being measured or downloaded. |
| 503 | `still_stopping` | Retry: a stop is finishing. |
| 503 | `capacity_blocked`, `switch_failed` | Retry once there is room, or make room: stop or park another model, or start with `"evict": true`. |
| 503 | `host_ineligible`, `reconciliation_required` | Retry: no machine can take it now (offline, drained, reconnecting). The message names each machine and why. |
| 400 | `invalid_request` | Fix the request: a missing header, a bad body, a deadline out of range. |
| 400 | `invalid_config` | Fix the field `details.path` names. |
| 409 | `revision_conflict` | Read the snapshot and decide again ([Revisions](#revisions)). |
| 409 | `idempotency_conflict` | A key was reused for a different change; use a new key. |
| 409 | `route_conflict` | The name is taken. |
| 409 | `runtime_retained`, `lifecycle_conflict` | The deployment is not in a state that allows the action; read the snapshot. |
| 409 | `delete_requires_cleanup` | Stop it, wait for `stopped`, then delete. |
| 409 | `profile_not_published`, `checkpoint_mismatch`, `checkpoint_unusable`, `model_source_failed`, `startup_requires_empty_host` | Fix the deployment or the machine as the message says; [Exit codes and errors](errors.md) has the same causes for the `capyctl` command. |
| 404 | `not_found` | The deployment does not exist, or was deleted. |
| 401 | `unauthenticated` | Stop: the token is wrong. |
| 403 | `host_policy_denied` | Stop: a machine's policy forbids the change. |
| 503 | `unsupported_capability` | Stop: this release cannot do it (for example `"activate": true`, or parking a `restart_only` model). |
| 500 | `internal` | Stop and read CapyCTL's log. |

When no answer arrives at all, treat it as `deadline_exceeded`: send the same
request again under the same key.

The inference endpoint answers with the codes in
[Errors while waiting](parking.md#errors-while-waiting), where `retryable` and
`Retry-After` say the same.

## Leaving every start to the controller

By default a request starts a stopped model and wakes a parked one, and
CapyCTL parks or stops models to make room and when they sit idle. A
controller that wants to make every one of those decisions itself sets
[`lifecycle.activation: explicit`](parking.md#only-on-your-command), in each
deployment it creates:

```json
{"config": {"name": "my-model", "engine": "vllm", "model": "Qwen3-4B", "lifecycle": {"activation": "explicit"}}, "activate": false}
```

or for every model of a machine, in its host document. Such a model moves only
on an action: a request while it is not running is refused at once with
`409 deployment_inactive`, and CapyCTL never parks or stops it to make room
or because it is idle. Nor does a request restart it when its engine exits;
the controller starts it again. The snapshot shows `"activation": "explicit"`
for it.

To have no request wait in CapyCTL either, set the waiting limit per model to
0 ([No waiting](parking.md#no-waiting)):

```yaml
# host.yaml (standalone: under host:)
resource_policy:
  queue:
    max_pending_per_deployment: 0
```

A request beyond a model's running requests is then refused at once with
`429 queue_full` and `Retry-After`, for the controller or its clients to
retry or send elsewhere.

When a gateway in front of CapyCTL checks its clients' credentials itself,
CapyCTL's API key can be turned off with `--no-inference-auth`. Do that only
where nothing but the gateway can reach the inference endpoint, for example
with the endpoint on loopback ([Turning the key off](../operations/network-access.md#turning-the-key-off)).

## A worked example

This script creates `my-model`, starts it, waits until it serves, sends one
request, then stops and deletes it. It runs on the machine that runs CapyCTL
on its own, and needs `curl` and `jq`.

```bash title="controller.sh"
#!/bin/sh
# Create, start, check, stop and delete one deployment through the
# management API, as a controller would. Needs curl and jq.
set -eu
M=http://127.0.0.1:7443/management/v1
CREDENTIALS=~/.local/state/capyctl/identity/credentials
TOKEN=$(sed -n 's/^admin_token: //p' "$CREDENTIALS")
KEY=$(sed -n 's/^api_key: //p' "$CREDENTIALS")
RUN=$(date +%s)   # makes this run's idempotency keys its own

now_ms() { echo $(( $(date +%s) * 1000 )); }

# send METHOD PATH IDEMPOTENCY-KEY BODY: one change. While CapyCTL answers
# that a retry can succeed, or does not answer at all, send the same request
# again: same key, same body.
send() {
  for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
    if out=$(curl -s -w '\n%{http_code}' -X "$1" "$M$2" \
        -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
        -H "Idempotency-Key: $3" -d "$4"); then
      status=$(printf '%s' "$out" | tail -n 1)
      body=$(printf '%s' "$out" | sed '$d')
      case $status in 2??) printf '%s\n' "$body"; return 0 ;; esac
      if [ "$(printf '%s' "$body" | jq -r '.error.retryable')" != true ]; then
        echo "refused ($status): $body" >&2
        return 1
      fi
      echo "retrying: $(printf '%s' "$body" | jq -r '.error.code')" >&2
    fi
    sleep 2
  done
  echo "gave up after 15 attempts" >&2
  return 1
}

# deployment: this deployment's entry in the snapshot.
deployment() {
  curl -sf "$M/snapshot" -H "Authorization: Bearer $TOKEN" |
    jq -c --arg id "$ID" '.deployments[] | select(.id == $id)'
}

# 1. Create. `config` is the deployment file, as JSON.
receipt=$(send POST /deployments "create-my-model-$RUN" \
  '{"config": {"name": "my-model", "engine": "vllm", "model": "Qwen3-4B"}, "activate": false}')
ID=$(printf '%s' "$receipt" | jq -r .deployment_id)
REV=$(printf '%s' "$receipt" | jq -r .revision)
echo "created $ID, revision $REV"

# 2. Start, by the deployment's initialize timeout. The deadline is part of
# the body, so it is computed once and every retry sends the same one.
deadline=$(( $(now_ms) + $(deployment | jq .timeouts.initialize_ms) ))
start=$(send POST "/deployments/$ID/actions" "start-my-model-$RUN" \
  "{\"action\": \"start\", \"expected_revision\": $REV, \"deadline_ms\": $deadline}")
echo "start accepted: operation $(printf '%s' "$start" | jq -r .operation_id)"

# 3. Wait until every instance is ready, or the start fails.
while :; do
  d=$(deployment)
  if [ "$(printf '%s' "$d" | jq '.ready_instances == .desired_instances')" = true ]; then
    break
  fi
  if [ "$(printf '%s' "$d" | jq '.latest_operation.state == "failed" or .latest_operation.given_up == true')" = true ]; then
    echo "start failed: $(printf '%s' "$d" | jq -r '.latest_operation.reason')" >&2
    exit 1
  fi
  if [ "$(now_ms)" -gt "$deadline" ]; then
    echo "not ready by the deadline: $(printf '%s' "$d" | jq -c '[.instances[] | {index, observed_state, last_error}]')" >&2
    exit 1
  fi
  sleep 1
done
printf '%s' "$d" | jq -c '{name, observed_state, ready: "\(.ready_instances)/\(.desired_instances)", instances: [.instances[] | {index, host_id, observed_state}]}'

# 4. Check it answers. CapyCTL echoes X-Request-Id and adds its own ID.
curl -s -o /dev/null -D - http://127.0.0.1:8443/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -H "X-Request-Id: check-my-model-$RUN" \
  -d '{"model": "my-model", "messages": [{"role": "user", "content": "Hello"}]}' |
  grep -i 'request-id'

# 5. Stop, wait until it is stopped, then delete.
deadline=$(( $(now_ms) + $(deployment | jq .timeouts.stop_ms) ))
send POST "/deployments/$ID/actions" "stop-my-model-$RUN" \
  "{\"action\": \"stop\", \"expected_revision\": $REV, \"deadline_ms\": $deadline}" >/dev/null
until [ "$(deployment | jq -r .observed_state)" = stopped ]; do sleep 1; done
echo "stopped"
deadline=$(( $(now_ms) + $(deployment | jq .timeouts.stop_ms) ))
send POST "/deployments/$ID/actions" "delete-my-model-$RUN" \
  "{\"action\": \"delete\", \"expected_revision\": $REV, \"deadline_ms\": $deadline}" >/dev/null
echo "deleted"
```

```bash
sh controller.sh
```

```text
created 01M5E15VFG077EJ6W8Y4C601GC, revision 1
retrying: checkpoint_digest_pending
start accepted: operation 01M5E15XMAVWZ1XPA9XNJ5SHGJ
{"name":"my-model","observed_state":"ready","ready":"1/1","instances":[{"index":0,"host_id":"gpu-box","observed_state":"ready"}]}
x-capyctl-request-id: 01M5E15YTF50G5YZ7Y5RJ9VJ97
x-request-id: check-my-model-1792546106
stopped
deleted
```

The `retrying` line is the start arriving while CapyCTL was still measuring the
model's files; the script sent the same start again until it was accepted. A
real model takes a few minutes to become ready, and the script waits for it.

With several machines, run it on the server machine with the token from
`server-credentials.json` ([The management API](#the-management-api)).

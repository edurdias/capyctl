# Parking and switching

A GPU holds one or two models at a time. mllm keeps the others parked: the
engine stays up but gives back its GPU memory. A request for a parked model
wakes it. The time depends on the engine, model and parking tier.

A deployment parks unless it says `residency: restart_only`, as long as its
engine supports parking: `mllm engine list` shows `DEEP PARK enabled`.
[How it works](how-it-works.md#ready-parked-stopped) shows where the memory
goes in each state.

## How a model parks

mllm picks the way from your hardware:

- On a discrete card, a parked model's weights are copied to host RAM, and a
  wake copies them back in seconds. When the copy does not fit in host RAM,
  the model parks deep instead.
- On unified memory, a model parks deep: its weights are dropped, and a wake
  reloads them from disk.

To choose yourself, set `residency: host_backed` (host RAM, discrete cards
only), `residency: deep` or `residency: restart_only` in the deployment.

## Park and wake

```bash
mllm park deployment my-model
mllm list deployments
```

Parking runs in the background. The first status query may show `parking`;
run `mllm list deployments` again until it shows `parked`:

```text
NAME       STATE    READY   REVISION   HOSTS
my-model   parked   0/1     1          gpu-box
```

Send a request for `my-model` ([Make a request](requests.md)). mllm wakes the
model and answers; the first answer takes longer. Afterwards:

```text
NAME       STATE   READY   REVISION   HOSTS
my-model   ready   1/1     1          gpu-box
```

## Switch between models

When a model needs memory that another one holds, mllm parks the idle one.
Starting a second model with `--evict` allows that and names what it parked:

```bash
mllm deploy model --file other-model.yaml
mllm start deployment other-model --evict --wait
```

`--wait` waits for the checkpoint measurement, then for the model to be ready.
Without it, a newly deployed checkpoint can still be awaiting measurement and
the start is refused. When the start releases another model, its receipt includes
`victims` (the deployment and instance IDs) and `switch_id`.

After the start succeeds, check the states. When there is room to keep the first
model parked, the result looks like this:

```bash
mllm list deployments
```

```text
NAME          STATE    READY   REVISION   HOSTS
my-model      parked   0/1     1          gpu-box
other-model   ready    1/1     1          gpu-box
```

Without `--evict`, a start never parks anything; it waits for memory instead.

A request switches the same way. Ask for `my-model` now and mllm parks
`other-model`, wakes `my-model` and answers:

```text
NAME          STATE    READY   REVISION   HOSTS
my-model      ready    1/1     1          gpu-box
other-model   parked   0/1     1          gpu-box
```

mllm waits for requests in progress to finish before it parks a model; it
never cuts an answer off. When there is no room to keep a parked copy, mllm
stops the idle model instead and says so:
`released: stopped (no room to park)`.

## Park or stop

| | Parked in host RAM | Parked, deep | Stopped |
|---|---|---|---|
| GPU memory | freed | freed | freed |
| Host RAM | engine and a copy of the weights | engine | none |
| A request | wakes it | wakes it | starts it after a pressure stop; refused after an operator stop |
| Back to ready | seconds | a reload from disk | a full start |

`mllm stop deployment other-model` stops it; `mllm start deployment other-model --wait`
starts it again.

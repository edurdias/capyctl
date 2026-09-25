# Parking and switching

A GPU holds one or two models at a time. mllm keeps the others parked: the
engine stays up but gives back its GPU memory. A request for a parked model
wakes it, which is much faster than starting it from scratch.

Parking needs `residency: deep` in the deployment and an engine that
supports it; `mllm engine list` shows `DEEP PARK enabled`.

## Park and wake

```bash
mllm park deployment my-model
mllm list deployments
```

```text
NAME       KIND    DESIRED   STATE    READY   REVISION   HOSTS
my-model   model   ready     parked   0/1     1          gpu-box
```

Send a request for `my-model` ([Make a request](requests.md)). mllm wakes the
model and answers; the first answer takes longer. Afterwards:

```text
NAME       KIND    DESIRED   STATE   READY   REVISION   HOSTS
my-model   model   ready     ready   1/1     1          gpu-box
```

## Switch between models

When a model needs memory that another one holds, mllm parks the idle one.
Starting a second model with `--evict` allows that and names what it parked:

```bash
mllm deploy model --file other-model.yaml
mllm start deployment other-model --evict
```

```text
Request identity: 01M3CQAQ4VA6T7Q7KQQV82KFZ4 (reuse --request-id 01M3CQAQ4VA6T7Q7KQQV82KFZ4 to recover this command)
{"api_version":"1","deployment_id":"01M3CQAK6XXW1E1DG0TZRRQY2S","joined":false,"operation_id":"01M3CQAQ953JSBEDC8XXS1Z8J6","revision":"1","switch_id":"01M3CQAQ6G0MZ4WMN79RN5YNSD","victims":["01M3CQAA0FE9VY13FNY5N94EKD/0"]}
```

`victims` names what it parked: instance 0 of `my-model`.

```bash
mllm list deployments
```

```text
NAME          KIND    DESIRED   STATE    READY   REVISION   HOSTS
my-model      model   ready     parked   0/1     1          gpu-box
other-model   model   ready     ready    1/1     1          gpu-box
```

Without `--evict`, a start never parks anything; it waits for memory instead.

A request switches the same way. Ask for `my-model` now and mllm parks
`other-model`, wakes `my-model` and answers:

```text
NAME          KIND    DESIRED   STATE    READY   REVISION   HOSTS
my-model      model   ready     ready    1/1     1          gpu-box
other-model   model   ready     parked   0/1     1          gpu-box
```

mllm waits for requests in progress to finish before it parks a model; it
never cuts an answer off.

## Park or stop

| | Parked | Stopped |
|---|---|---|
| GPU memory | freed | freed |
| A request | wakes it | is refused (`deployment_stopped`) |
| Back to ready | fast | a full start |

`mllm stop deployment other-model` stops it; `mllm start deployment other-model --wait`
starts it again.

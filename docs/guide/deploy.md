# Deploy a model

A deployment tells mllm which checkpoint to run, with which engine, and the
model name clients ask for. You write it once as a YAML file.

## The deployment file

```yaml title="my-model.yaml"
schema_version: 1
kind: deployment
name: my-model
routes: ["my-model"]
runtime_profile: vllm
runtime_profile_revision: 1
recipe: standard
residency: deep
recovery: reconcile
model:
  path: Qwen3-4B
  content_fingerprint: "qwen3-4b-1"
  revision: "1"
devices:
  - id: gpu0
    sharing: shared
engine_config:
  memory:
    request: "16GiB"
```

Change these for your model:

- `name` and `routes`: the name clients send as `model` in their requests.
- `runtime_profile`: the engine, as `mllm engine list` shows it
  ([Add an engine](engines.md)).
- `model.path`: the checkpoint's directory, relative to your models directory
  (`MLLM_MODELS_ROOT` on one machine, `model_store` on a host).
- `content_fingerprint` and `revision`: your own labels for this copy of the
  weights. Change them when the files change.
- `engine_config.memory.request`: the GPU memory the model may use while it
  runs, weights plus KV cache.
- `residency: deep` lets mllm park the model and free its memory
  ([Parking and switching](parking.md)). Use `restart_only` to stop it instead.

On several machines, add `host: gpu-box` to choose the machine. Every other
field is in [Configuration files](configuration.md).

## Deploy and start

```bash
mllm deploy model --file my-model.yaml
```

```text
Request identity: 01M3CQAA018A85DPFR3K71479Y (reuse --request-id 01M3CQAA018A85DPFR3K71479Y to recover this command)
{"api_version":"1","checkpoint_digest":"pending","deployment_id":"01M3CQAA0FE9VY13FNY5N94EKD","joined":false,"operation_id":"01M3CQAA0F7DDDNQG9Y5T7NB02","revision":"1"}
```

The deployment is saved. The first time mllm sees a checkpoint it reads the
files once to fingerprint them (`"checkpoint_digest":"pending"`). Then start
it:

```bash
mllm start deployment my-model --wait
```

`--wait` returns when the model answers. If it says `checkpoint_digest_pending`,
the fingerprint is not finished yet: run it again in a moment.
`mllm deploy model --file my-model.yaml --activate --wait` does both steps in
one command once the checkpoint is known.

## Check on it

```bash
mllm list deployments
```

```text
NAME       KIND    DESIRED   STATE   READY   REVISION   HOSTS
my-model   model   ready     ready   1/1     1          gpu-box
```

```bash
mllm status deployment my-model
```

```text
NAME       KIND    DESIRED   STATE   READY   REVISION   STARTUP    INITIALIZE   LAST OPERATION
my-model   model   ready     ready   1/1     1          16.0 GiB   130s         initialize succeeded

INSTANCE   HOST      STATE   LIFECYCLE   DEVICES   LAST ERROR
0          gpu-box   ready   active      gpu0      -
```

While parking is on, these commands also print a warning that the engine
runs with its sleep controls enabled. Those controls listen on loopback only.
Add `--format json` to any list or status command for the full record.

## Stop, start, delete

```bash
mllm stop deployment my-model               # stops the engine, keeps the deployment
mllm start deployment my-model --wait       # starts it again
mllm delete deployment my-model --stop      # stops it and removes it
```

A request for a stopped deployment is refused; it does not start it:

```text
{"code":"deployment_stopped","message":"deployment 01M3CQAK6XXW1E1DG0TZRRQY2S was stopped by an operator; inference does not start it; start it with `mllm start deployment 01M3CQAK6XXW1E1DG0TZRRQY2S`"}
```

Deleting never touches the model files.

To change a deployment, edit the file and deploy it again with its current
revision, the `REVISION` column of `mllm list deployments`:

```bash
mllm deploy model --file my-model.yaml --revision 1
```

```text
Request identity: 01M3CQB0BFGD9MTVFRSE53YRQQ (reuse --request-id 01M3CQB0BFGD9MTVFRSE53YRQQ to recover this command)
{"api_version":"1","checkpoint_digest":"pending","deployment_id":"01M3CQAA0FE9VY13FNY5N94EKD","joined":false,"operation_id":"01M3CQB0CDNAJQ7AF2T31XEAAF","revision":"2"}
```

A change restarts the model's engine. Afterwards:

```text
NAME       KIND    DESIRED   STATE   READY   REVISION   HOSTS
my-model   model   ready     ready   1/1     2          gpu-box
```

Next: [Make a request](requests.md).

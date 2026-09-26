# Deploy a model

A deployment tells mllm which model to run, with which engine, and the name
clients ask for. You write it once as a YAML file.

## The deployment file

Three fields are enough:

```yaml title="my-model.yaml"
name: my-model
engine: vllm
model: Qwen3-4B
```

- `name`: the name clients send as `model` in their requests.
- `engine`: the engine profile, as `mllm engine list` shows it
  ([Add an engine](engines.md)).
- `model`: where the weights are. A directory under your models directory
  (`~/models` unless you set another), an absolute path, or a Hugging Face
  repository:

```yaml title="qwen3-4b.yaml"
name: qwen3-4b
engine: vllm
model: {hf: Qwen/Qwen3-4B-Instruct-2507}
```

mllm pins a Hugging Face repository to the commit it points at when you
deploy (write `Qwen/Qwen3-4B-Instruct-2507@<commit>` to pick one yourself),
then downloads it into `~/models/sources` on the machine that runs it. It
checks free disk space first, and all downloads together are capped at
500 GiB. See [settings](../operations/configuration.md#models-and-downloads)
to change the cap or turn downloads off.

mllm fills in the rest: the GPU, the memory the engine may use (sized from
the checkpoint and the GPU), and how the model parks. To see what it fills
in:

```bash
mllm validate config --file my-model.yaml
```

`validate` works offline, so it refuses an `hf:` reference without a commit;
`deploy` pins it for you.

Add a field to choose something yourself, for example:

- `residency: restart_only` to stop the model instead of parking it, or
  `deep` to drop its weights when parked
  ([Parking and switching](parking.md)).
- `devices: [{id: gpu1}]` to pin a GPU on a machine with several.
- `host: gpu-box` to choose the machine, with
  [several machines](several-machines.md).
- `engine_config: {memory: {request: 16GiB}}` to set the GPU memory it may
  use: weights plus KV cache.

Every other field is in [Configuration files](configuration.md).

## Deploy and start

```bash
mllm deploy model --file my-model.yaml --activate --wait
```

This saves the deployment, starts it and returns when the model answers. The
first time mllm sees a checkpoint it reads the files once to fingerprint
them, so the first start takes longer.

Without `--activate`, `deploy` only saves it. For a second model,
`other-model.yaml`:

```bash
mllm deploy model --file other-model.yaml
```

```text
Request identity: 01M3FQEMBJ7WQBDMQ15ZWBD7W6 (reuse --request-id 01M3FQEMBJ7WQBDMQ15ZWBD7W6 to recover this command)
{"api_version":"1","checkpoint_digest":"pending","deployment_id":"01M3FQEMC25J8DBR0N2DVZYXEV","joined":false,"notice":"the checkpoint digest of other-model is being measured; `mllm start deployment other-model --wait` waits for it and starts the deployment","operation_id":"01M3FQEMC219NVYGCC71TVJ6Y7","revision":"1"}
the checkpoint digest of other-model is being measured; `mllm start deployment other-model --wait` waits for it and starts the deployment
```

Then start it when you want it:

```bash
mllm start deployment other-model --wait
```

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
my-model   model   ready     ready   1/1     1          17.2 GiB   130s         initialize succeeded

INSTANCE   HOST      STATE   LIFECYCLE   DEVICES   LAST ERROR
0          gpu-box   ready   active      gpu0      -
```

`STARTUP` is the memory mllm set aside to start the model. While parking is
on, these commands also print a warning that the engine runs with its sleep
controls enabled. Those controls listen on loopback only. Add
`--format json` to any list or status command for the full record.

## Stop, start, delete

```bash
mllm stop deployment my-model               # stops the engine, keeps the deployment
mllm start deployment my-model --wait       # starts it again
mllm delete deployment my-model --stop      # stops it and removes it
```

`stop` returns once the stop is accepted. Wait until `mllm list deployments`
shows `stopped` before you start it again; a start while the engine is still
going away is refused (`runtime_retained`).

A request for a stopped deployment is refused; it does not start it:

```text
{"code":"deployment_stopped","message":"deployment 01M3FQEMC25J8DBR0N2DVZYXEV was stopped by an operator; inference does not start it; start it with `mllm start deployment 01M3FQEMC25J8DBR0N2DVZYXEV`"}
```

Deleting never touches the model files.

To change a deployment, edit the file and deploy it again with its current
revision, the `REVISION` column of `mllm list deployments`:

```bash
mllm deploy model --file my-model.yaml --revision 1
```

```text
Request identity: 01M3FQF59H8SDGT4DCDWTTR77B (reuse --request-id 01M3FQF59H8SDGT4DCDWTTR77B to recover this command)
{"api_version":"1","checkpoint_digest":"pending","deployment_id":"01M3FQEE238THV698Y20FHNEXF","joined":false,"operation_id":"01M3FQF5AHV57TXGMQK5JM7QGV","revision":"2"}
```

A change restarts the model's engine; changing only the instance count does
not. Afterwards:

```text
NAME       KIND    DESIRED   STATE   READY   REVISION   HOSTS
my-model   model   ready     ready   1/1     2          gpu-box
```

Next: [Make a request](requests.md).

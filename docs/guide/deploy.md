# Deploy a model

A deployment tells CapyCTL which model to run, with which engine, and the name
clients ask for. You write it once as a YAML file.

## The deployment file

Three fields are enough:

```yaml title="my-model.yaml"
name: my-model
engine: vllm
model: Qwen3-4B
```

- `name`: the name clients send as `model` in their requests.
- `engine`: the engine profile, as `capyctl engine list` shows it
  ([Add an engine](engines.md)).
- `model`: where the weights are. A directory under your models directory
  (`~/models` unless you set another), an absolute path (a Hugging Face cache
  snapshot works; its directory must not be writable by other users), or a
  Hugging Face repository:

```yaml title="qwen3-4b.yaml"
name: qwen3-4b
engine: vllm
model: {hf: Qwen/Qwen3-4B-Instruct-2507}
```

CapyCTL pins a Hugging Face repository to the commit it points at when you
deploy (write `Qwen/Qwen3-4B-Instruct-2507@<commit>` to pick one yourself),
then downloads it into `~/models/sources` on the machine that runs it. It
checks free disk space first, and all downloads together are capped at
500 GiB. See [settings](../operations/configuration.md#models-and-downloads)
to change the cap or turn downloads off.

CapyCTL fills in the rest: the GPU, the memory the engine may use (sized from
the checkpoint and the GPU), and how the model parks. To see what it fills
in:

```bash
capyctl validate config --file my-model.yaml
```

`validate` works offline, so it refuses an `hf:` reference without a commit;
`deploy` pins it for you. Without `--host` it checks the document, its
timeouts, a declared `memory.startup` and a declared `resources` block, and
lists what needs a host:

```text
my-model.yaml is a valid deployment document

Not checked
  resolution against a host: pass --host <host.yaml> to check the runtime profile, placement, the host's devices and capacity, and timeouts, and to see the host's defaults; a TensorFold profile under another name than tensorfold is recognised only there
  whether each allowed host is enrolled, online and has published (host_unpublished)
  which runtime profiles the host's role accepted and published after measuring each installation (profile_not_published)
  the host's current resource policy as the server stores it (resource_policy_unavailable)
  route and deployment-name conflicts with existing deployments (route_conflict)
  a new checkpoint's digest, measured on the host after acceptance (checkpoint_digest_pending)
```

Add a field to choose something yourself, for example:

- `residency: restart_only` to stop the model instead of parking it, or
  `deep` to drop its weights when parked
  ([Parking and switching](parking.md)).
- `devices: [{id: gpu1}]` to pin a GPU on a machine with several.
- `host: gpu-box` to choose the machine, with
  [several machines](several-machines.md).
- `engine_config: {memory: {request: 16GiB}}` to set the GPU memory it may
  use: weights plus KV cache.
- `engine_config: {cuda_graphs: true}` on SGLang for faster decoding. CapyCTL
  turns SGLang's CUDA graphs off while the model can park; on a 16 GB card
  FrogNano-4B decoded 61 tokens/s with them and 35 without, and kept about
  0.6 GiB more of the card while parked.

Every other field is in [Configuration files](configuration.md).

## Deploy and start

```bash
capyctl deploy model --file my-model.yaml --activate --wait
```

```text
Request identity: 01M3R78B47ANFBFVFY40HATPYJ (reuse --request-id 01M3R78B47ANFBFVFY40HATPYJ to recover this command)
Waiting for the checkpoint digest of my-model to be measured (at most 900s)
Deployed my-model: ready

  Revision    1
  Hosts       gpu-box
  Ready       1/1
  Startup     28.5 GiB
  Context     26752 tokens
  Operation   initialize succeeded
```

This saves the deployment, starts it and returns when the model answers. The
first time CapyCTL sees a checkpoint it reads the files once to fingerprint
them, so the first start takes longer.

Without `--activate`, `deploy` only saves it. For a second model,
`other-model.yaml`:

```bash
capyctl deploy model --file other-model.yaml
```

```text
Request identity: 01M3R7A6Y7JQV9A5X404JZC93X (reuse --request-id 01M3R7A6Y7JQV9A5X404JZC93X to recover this command)
Deployment other-model created (revision 1)

  Deployment ID       01M3R7A6YJW402N962HH17A66W
  Operation           01M3R7A6YJFV3HDFWQNZTWQTN0
  Checkpoint digest   being measured
the checkpoint digest of other-model is being measured; `capyctl start deployment other-model --wait` waits for it and starts the deployment
```

Then start it when you want it:

```bash
capyctl start deployment other-model --wait
```

## Check on it

```bash
capyctl list deployments
```

```text
NAME       STATE   READY   REVISION   HOSTS
my-model   ready   1/1     1          gpu-box
```

```bash
capyctl status deployment my-model
```

```text
NAME       STATE   READY   REVISION   STARTUP    INITIALIZE   LAST OPERATION
my-model   ready   1/1     1          28.5 GiB   210s         initialize succeeded

INSTANCE   HOST      STATE   LIFECYCLE   DEVICES   LAST ERROR
0          gpu-box   ready   active      gpu0      -
```

To take a model off the GPU without stopping it, park it. The deployment shows
`parking` and then `parked` while its memory is freed:

```bash
capyctl park deployment my-model
```

```text
Request identity: 01M3R7ADN3J81JRNH2RDCVD12W (reuse --request-id 01M3R7ADN3J81JRNH2RDCVD12W to recover this command)
Park requested for my-model

  Operation   01M3R7ADNMD6JTTDDFJ9DKFXVQ
```

```bash
capyctl status deployment my-model
```

```text
NAME       STATE    READY   REVISION   STARTUP    INITIALIZE   LAST OPERATION
my-model   parked   0/1     1          28.5 GiB   210s         park succeeded

INSTANCE   HOST      STATE    LIFECYCLE   DEVICES   LAST ERROR
0          gpu-box   parked   active      gpu0      -
```

`STARTUP` is the memory CapyCTL set aside to start the model. On a discrete GPU
it is the card's memory, with the host RAM the engine process takes beside it.
When a start fails, `LAST OPERATION` says so and the instance's `LAST ERROR`
gives the reason:

```text
NAME           STATE    READY   REVISION   STARTUP                   INITIALIZE   LAST OPERATION
broken-model   failed   0/1     1          13.2 GiB (+4.0 GiB RAM)   130s         initialize failed (launch_failed)

INSTANCE   HOST      STATE    LIFECYCLE   DEVICES   LAST ERROR
0          gpu-box   failed   active      gpu0      launch_failed: launch failed: engine launch failed: the engine exited before readiness
```

[Exit codes and errors](errors.md#troubleshooting) says where to look next. While parking is
on, these commands also print a warning that the engine runs with its sleep
controls enabled. Those controls listen on loopback only. Add
`--json` to any list or status command for the full record.

## Stop, start, delete

```bash
capyctl stop deployment my-model               # stops the engine, keeps the deployment
capyctl start deployment my-model --wait       # starts it again
capyctl delete deployment my-model --stop      # stops it and removes it
```

```text
Request identity: 01M3R7BZ886JYFSRCB9K915YSJ (reuse --request-id 01M3R7BZ886JYFSRCB9K915YSJ to recover this command)
Stop requested for my-model

  Operation   01M3R7BZ8SSE6F0WGNSV09WNHV
```

`stop` returns once the stop is accepted, while the engine is still going
away. A `start` right after it is refused, exit status 25, and nothing is
started:

```text
$ capyctl start deployment my-model
Request identity: 01M3R7BZ98AKHFBBTZXBCCX1BC (reuse --request-id 01M3R7BZ98AKHFBBTZXBCCX1BC to recover this command)
error [still_stopping]: my-model is still stopping; nothing was started. Retry in a moment, or run `capyctl start deployment my-model --wait`, which waits for the stop to finish and then starts
```

With `--wait`, `start` waits for the stop to finish, starts the model and
returns when it answers:

```text
$ capyctl start deployment my-model --wait
Request identity: 01M3R7BZA67CC0V5C8F8BFF7BR (reuse --request-id 01M3R7BZA67CC0V5C8F8BFF7BR to recover this command)
Waiting for the stop of my-model to finish (at most 160s)
Started my-model: ready

  Ready       1/1
  Hosts       gpu-box
  Operation   initialize succeeded
```

A request for a stopped deployment is refused; it does not start it:

```text
{"code":"deployment_stopped","message":"deployment 01M3R7A6YJW402N962HH17A66W was stopped by an operator; inference does not start it; start it with `capyctl start deployment 01M3R7A6YJW402N962HH17A66W`"}
```

A plain `delete deployment` right after `stop` is refused the same way
(`delete_requires_cleanup`) until the stop finishes; `delete deployment --stop`
waits for it. Deleting never touches the model files.

To change a deployment, edit the file and deploy it again with its current
revision, the `REVISION` column of `capyctl list deployments`:

```bash
capyctl deploy model --file my-model.yaml --revision 1
```

```text
Request identity: 01M3R7DDQH4E287PVNR4CBR38Y (reuse --request-id 01M3R7DDQH4E287PVNR4CBR38Y to recover this command)
Deployment my-model updated (revision 2)

  Deployment ID       01M3R7A6YJW402N962HH17A66W
  Operation           01M3R7DDR0S1GTBW8DGQ55SPMG
  Checkpoint digest   being measured
```

A change restarts the model's engine; changing only the instance count does
not. Afterwards:

```text
NAME       STATE   READY   REVISION   HOSTS
my-model   ready   1/1     2          gpu-box
```

Next: [Make a request](requests.md).

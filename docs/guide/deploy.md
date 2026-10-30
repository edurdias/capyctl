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
- `topology: {tensor_parallel: 2}` to spread one model over two GPUs of the
  machine, or `topology: auto` to let CapyCTL choose
  ([One model on several GPUs](#one-model-on-several-gpus)).
- `host: gpu-box` to choose the machine, with
  [several machines](several-machines.md).
- `engine_config: {memory: {request: 16GiB}}` to set the GPU memory it may
  use: weights, KV cache and a margin for the engine's own memory. On a
  unified-memory machine such as a GB10 the margin is 8 GiB, or the weights
  x 0.15 plus 4 GiB for checkpoints above 26.7 GiB; on a discrete GPU it is
  the weights x 0.10.
- `engine_config: {cuda_graphs: false}` on SGLang to turn its CUDA graphs off.
  They are on by default; on a 16 GB card FrogNano-4B decoded 61 tokens/s with
  them and 35 without, and kept about 0.7 GiB more of the card while parked.
  After the first park CapyCTL charges a parked model what it measured it
  holding (`parked` in `capyctl status deployment <name> --json`).
- `lmcache: off` to run without [LMCache](engines.md#lmcache), the private
  KV cache in host RAM that every vLLM 0.32.0+ and SGLang 0.5.22+ deployment
  gets by default (decision deployments on SGLang excepted, for now), or
  `lmcache: required` to refuse to start without it.
  `lmcache_pool: 8GiB` sets its RAM pool (a whole number of GiB, 4 GiB by
  default); it is refused beside `lmcache: off`.
- `retention: zero` to keep this deployment's engine output in memory, start
  it with no core dumps and never give it LMCache
  ([zero data retention](../operations/data-retention.md#zero-data-retention)).

Every other field is in [Configuration files](configuration.md).

## One model on several GPUs

A model too large for one card can run on several identical cards of one
machine. Say how many with `topology`:

```yaml title="qwen3-30b.yaml"
name: qwen3-30b
engine: vllm
model: {hf: Qwen/Qwen3-30B-A3B}
topology: {tensor_parallel: 2}
```

CapyCTL picks two GPUs of the same product and memory, gives each one rank of
the model, and starts the engine on both: vLLM with `--tensor-parallel-size 2`,
SGLang with `tp_size` 2, llama.cpp with `--split-mode layer` (each card holds
part of the layers). Those options are CapyCTL's; a deployment cannot set them
itself. It is still one model and one route, on one machine: no second machine,
no group.

- **Which GPUs.** CapyCTL picks the cards with the most room. To choose them,
  name exactly as many as `tensor_parallel` says:
  `devices: [{id: gpu0}, {id: gpu1}]`. Two `devices` without a `topology` are
  refused, with a hint to add one.
- **Let CapyCTL choose the number.** `topology: auto` takes the smallest number
  of GPUs whose share of the model fits each card's memory limit:

  ```yaml
  name: qwen3-30b
  engine: sglang
  model: {hf: Qwen/Qwen3-30B-A3B}
  topology: auto
  ```

  On a machine with two 48 GB cards, Qwen3-4B gets one GPU and Qwen3-30B-A3B
  (about 61 GB of weights) gets two. The choice is made from the cards' limits,
  not from what is free at the moment, so the same model on the same machine
  always gets the same number; a busy card is made free by parking or stopping
  what is on it, as for one GPU. The number is chosen when the model starts and
  kept until it stops, parks and wakes included. `capyctl status deployment`
  shows `TP auto (2)`. `auto` cannot be combined with `devices`, `resources`,
  `memory.request` or `memory.startup`, which are per-GPU figures; a
  `memory.kv_cache` is per GPU and holds for every size.
- **What it costs.** Each card is charged its share of the weights, its KV
  cache, a margin, 1.25 GiB for the CUDA context and 1 GiB for the traffic
  between the cards; host RAM is charged 4 GiB plus 3 GiB for each card after
  the first. `capyctl validate config --file qwen3-30b.yaml --host host.yaml`
  prints the charge per GPU, and for `auto` the charge at every size and the
  size the machine would take now. Those two allowances are estimates until
  measured on a two-GPU machine.
- **Which numbers work.** For vLLM and SGLang the number must divide the model's
  attention heads (`num_attention_heads` in its `config.json`; 32 for the Qwen3
  models above, so 1, 2, 4 or 8 work). llama.cpp takes any number.
- **Refused.** Cards of different products or memory, a machine with one
  unified-memory GPU (such as a GB10), TensorFold, custom engines, decision
  models and `pipeline_parallel` on one machine. More GPUs than the machine has
  is `insufficient_gpus`; a model that does not fit is
  `insufficient_device_memory`, and both name the `tensor_parallel` that would
  fit ([errors](errors.md)).
- **Parking.** vLLM and SGLang support `deep` and `host_backed` on several
  GPUs, qualified at TP 2 on two RTX A6000 cards. Every chosen GPU must
  confirm a park or wake. `topology: auto` can choose several GPUs with either
  tier. llama.cpp stops instead of parking ([Parking and switching](parking.md)).
- **Security.** The engine's ranks talk to each other over listeners on
  `127.0.0.1` only, without authentication, and status shows
  `collectives: loopback`. Another user logged in to the same machine could
  reach them, as with any engine's own API on loopback. CapyCTL checks after
  every start that none of the engine's processes listens on any other address,
  and stops the model if one does (`engine_listener_exposed`).

If one rank exits, the whole model stops and reads `failed`; if one hangs, a
request that gets no first output within the stall timeout (120 s,
`groups.stall_timeout`) makes CapyCTL probe the model once and stop it when
the probe fails too (`tp_stalled`).

## Decision models

A decision model answers typed questions about an input with a probability
for every option, instead of generating text. It is served at
`POST /v1/systemone` rather than at chat completions. Say so with `api`:

```yaml title="decider.yaml"
name: decider
engine: llamacpp
model: Clef-Flash-GGUF
api: decisions
resources: {gpu: 16GiB, ram: 2GiB}
engine_config:
  context_length: 16384
  llamacpp: {gguf_file: Clef-Flash-Q8_0.gguf}
  accept_extra_args: true
  extra_args: [--batch-size, "16384", --ubatch-size, "16384"]
```

llama.cpp reads a decision request's whole input in one batch, so set both
batch sizes to the context. With llama.cpp's defaults (2,048 and 512 tokens) a
longer input is refused with `502 engine_error` and llama.cpp's message,
"increase the physical batch size". The batch costs memory: state it in
`resources`, as above, from the peak CapyCTL measures
(`capyctl status deployment decider --json`, `startup.measured`).
[CapyCTL's recipes](https://github.com/edurdias/capyctl-recipes) have measured
files for Clef-Flash, Clef, Kev-4B and Julia-1 on llama.cpp and Clef-Flash and
Clef on SGLang, and a comparison of six decision models on one GB10.

`api` is `chat` when you leave it out. It is part of the deployment's
revision: the same weights served as `chat` and as `decisions` are two
revisions. In this release a `decisions` deployment:

- needs llama.cpp 0.7.0 or newer, or SGLang 0.5.22 or newer, registered with
  `capyctl engine add` (which records the engine's version), or a
  [custom engine](engines.md#custom-engines) whose manifest says
  `api: decisions`. vLLM and TensorFold are refused with
  `capability_missing:decisions` ([Exit codes and errors](errors.md)). vLLM
  0.32.0 serves `POST /v1/systemone`, but it answers Clef and Clef-Flash from
  their text output instead of their decision head: questions with more than
  26 options are refused and accuracy and calibration fall well below SGLang's
  and llama.cpp's. CapyCTL admits vLLM once a release uses these models'
  decision heads;
- runs on one machine: a `topology` is refused;
- restarts instead of parking: `residency` is `restart_only`, and `deep` or
  `host_backed` is refused;
- has no tool-call or reasoning parser: `tool_call_parser`,
  `reasoning_parser` and those options in `extra_args` are refused;
- runs only on machines whose CapyCTL serves decision models; an older one is
  never chosen, and naming it is refused with
  `host_capability_missing:decisions`.

`capyctl status deployment <name>` shows an `API` line for it (`"api":
"decisions"` with `--json`); a chat deployment has neither.

A decision model generates no text, so CapyCTL proves it ready with a decision
instead: it asks one fixed question (which team handles a ticket about a
double charge, with three options) and waits for a complete answer, a
probability for every option that sums to 1. It does not have to pick the
obvious team; it has to answer. A model that answers malformed, or not at all,
never becomes ready, and CapyCTL never sends it a chat request in place of the
decision. The same check runs when a machine reconnects or CapyCTL restarts
with the model still running. The first answer is recorded for the wake check
([Parking](parking.md#park-and-wake)).

### The readiness question

This is the exact request CapyCTL sends, with `model` set to the name the
engine serves the model under:

```json
{
  "model": "<the engine's served model name>",
  "state": "The ticket says: my card was charged twice.",
  "questions": {
    "route": {
      "type": "choice",
      "instructions": "Which team should handle this ticket?",
      "criteria": {
        "billing": "payments, charges and refunds",
        "shipping": "deliveries, parcels and tracking",
        "technical": "errors, crashes and sign-in problems"
      }
    }
  }
}
```

The model is ready when it answers `route` as a `choice` with a probability
for each of the three options and no other, each between 0 and 1, summing to
1 within 0.001, and picks one of the three. Any answer that meets this counts,
whichever team it picks. Any change to the request is a new version of the
check, and a wake reference recorded with an older version is never compared
with the new answers.

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
A first start measures the peak (on a discrete GPU the card and host RAM
apart) and later starts set that aside instead of the estimate; `--json` says
where each figure came from (`startup.provenance`, and on a discrete GPU
`startup.device_provenance` and `startup.host_provenance`: `measured`, or
`default` for the estimate). Before a first start has measured it, a model
whose estimated startup peak is above a unified-memory machine's managed limit
shows that limit: its first start runs alone on the machine and sets aside all
of it (`--json` names this `whole_host`). On a discrete GPU the estimate is the
card's Ready figure (the memory request and 1.25 GiB for the engine's CUDA
context), or `memory.startup` and the same 1.25 GiB when you state it. A card
has no first start that runs alone, so a model whose figure is above the
card's managed limit can never start: status says so under the table, and
`capyctl validate config --host` says so before you deploy:

```text
Startup     a start needs 15.2 GiB of gpu0, above the 14.7 GiB it manages, so it can never start; on a card the start is charged engine_config.memory.startup (the request when unset) plus the engine's CUDA context
```

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

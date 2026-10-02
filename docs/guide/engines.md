# Add an engine

CapyCTL runs the vLLM, SGLang or TensorFold you already have. It does not install engines.
To install one first, see [Install an engine](install-engines.md).
You register each installation once per machine; CapyCTL calls it an engine
profile, and a deployment names the profile in `engine`.

## Find your installations

```bash
capyctl engine detect
```

```text
ENGINE   VERSION   CUSTOM   ENVIRONMENT                SOURCE
sglang   0.5.20    no       /home/me/venvs/sglang      venv
vllm     0.29.0    no       /home/me/venvs/vllm        venv
```

`detect` only reads package metadata; it runs nothing. It looks on your
`PATH`, in conda, uv and pipx environments, `~/venvs/*`, `~/.venv`,
`~/.virtualenvs`, `/opt/*` and any virtual environment directly in your home
directory. Add `--path <dir>` to search somewhere else.

## Add one

Name the environment directory, its `bin/vllm` or its `bin/python3`:

```bash
capyctl engine add ~/venvs/vllm
```

```text
Registered vllm (vllm 0.29.0)

  Executable     /home/me/venvs/vllm/bin/vllm
  Deep park      enabled
  CUDA           /usr/local/cuda
  Engines file   /home/me/.config/capyctl/engines.yaml (revision 1)
  Published      when capyctl starts
saved to /home/me/.config/capyctl/engines.yaml (revision 1); start capyctl (`capyctl start standalone`) to use it
no capyctl answered on /home/me/.local/state/capyctl/control.sock; if it is already running with another --state-dir or --config, restart it to publish this profile, and pass the same option to `capyctl engine`
```

The profile is named after the engine: `vllm` or `sglang`. CapyCTL runs the
engine once to check its version and whether it supports parking, and records
the CUDA toolkit it finds for the engine's kernel builds.

That output is from a first run, before CapyCTL was started: the engine is saved
and used from the first start. If CapyCTL is running, it uses the engine at once
and prints `Published      yes`, as below.

On a GPU machine that runs a host, the same command adds the engine to the
host: the host records the file it was started with, and the engine commands
use it.

The engines file lives beside the role's config file, not under `--state-dir`
(which only picks the role's state and control socket). For a separate test
setup, set both `--config` (or `XDG_CONFIG_HOME`) and `--state-dir`, and give
the CLI and the running role the same ones.

## Custom builds

`CUSTOM yes` means a version other than vLLM 0.29.0 and 0.30.0, SGLang 0.5.20 or
TensorFold 0.6.0 and 0.6.1, the versions this release of CapyCTL knows. CapyCTL still runs it. Give it its own
name so it does not replace your main one:

```bash
capyctl engine add ~/venvs/vllm-nightly --name vllm-nightly
```

```text
Registered vllm-nightly (vllm 0.30.0rc1)

  Executable     /home/me/venvs/vllm-nightly/bin/vllm
  Deep park      enabled
  CUDA           /usr/local/cuda
  Engines file   /home/me/.config/capyctl/engines.yaml (revision 2)
  Published      yes
```

A deployment then uses it with `engine: vllm-nightly`. Other options:
`--arg` adds an engine argument to every launch (repeatable),
`--deep-park disabled` turns parking off for this engine, and
`--drift refuse` refuses a launch if the installation's files changed since
you added it.

Some engine options read a path or run code, so a deployment may pass them in
`extra_args` only if the engine allows them. `--approve-option` allows one by
name and `--approve-path` names a directory its path may be inside (both
repeatable; the directory must be absolute). For a vLLM build that serves a
local draft model:

```bash
capyctl engine add ~/venvs/vllm-nightly --name vllm-nightly \
  --approve-option=--speculative-config --approve-path /srv/drafters
```

SGLang's draft model is `--approve-option=--speculative-draft-model-path`.
They are written to `engines.yaml` as `security.approved_options` and
`security.approved_paths`, which you can also edit there.

CapyCTL counts a draft model when it sizes a deployment: its weights in the
memory request, its CUDA graphs in the memory reserved for the first start,
and its KV cache layers in the context it fits to the KV cache. For a hybrid
model (one with linear-attention or Mamba layers) on vLLM, set
`memory.kv_cache` yourself: the 4 GiB default does not hold vLLM's
per-sequence state for such a model.

## TensorFold

CapyCTL runs TensorFold 0.6.0 and 0.6.1 from a plain venv. TensorFold builds
CUDA kernels the first time it starts, so the machine needs `nvcc`, `ninja` and
a C++ compiler where the engine can find them: the venv's `bin`, the CUDA
toolkit's `bin`, or `/usr/local/bin`, `/usr/bin`, `/bin`. Either install a system
CUDA toolkit, or (0.6.1) put the compiler in the venv with pip:

```bash
pip install ninja "cuda-toolkit[nvcc,cccl]==13.0.*"
```

CapyCTL finds that `nvcc` under the venv's `site-packages/nvidia`. A C++ compiler
still comes from the system. `engine add` checks all of this and names what is
missing; it never uses your shell's `PATH`.

```bash
capyctl engine add ~/tensorfold-0.6.0-venv
```

```text
Registered tensorfold (tensorfold 0.6.0)

  Executable     /home/me/tensorfold-0.6.0-venv/bin/tensorfold
  Deep park      disabled
  CUDA           /usr/local/cuda
  Engines file   /home/me/.config/capyctl/engines.yaml (revision 1)
  Published      when capyctl starts
saved to /home/me/.config/capyctl/engines.yaml (revision 1); start capyctl (`capyctl start standalone`) to use it
no capyctl answered on /home/me/.local/state/capyctl/control.sock; if it is already running with another --state-dir or --config, restart it to publish this profile, and pass the same option to `capyctl engine`
```

TensorFold has no way to free its memory while it runs, so a TensorFold model
does not park: when CapyCTL needs the memory, or the model sits idle past
`ready_idle_timeout` (off unless set), CapyCTL waits for its requests to finish, stops it, and starts it again on the next
request. `capyctl park deployment` refuses a TensorFold model; use `stop` or let
CapyCTL switch it. A TensorFold deployment states its memory with `resources`
and its `context_length`:

```yaml
schema_version: 1
kind: deployment
name: nemotron
engine: tensorfold
model: nemotron-3.5-lightning-30b-a3b-4bit
residency: restart_only
devices: [{id: gpu0}]
resources:
  cold:
    allocations: [{domain: unified, bytes: 32GiB, host_kv_bytes: 0B}]
    devices: [{id: gpu0}]
  ready:
    allocations: [{domain: unified, bytes: 30GiB, host_kv_bytes: 0B}]
    devices: [{id: gpu0}]
  parking:
    allocations: [{domain: unified, bytes: 30GiB, host_kv_bytes: 0B}]
    devices: [{id: gpu0}]
  parked:
    allocations: [{domain: unified, bytes: 0B, host_kv_bytes: 0B}]
    devices: []
  wake:
    allocations: [{domain: unified, bytes: 32GiB, host_kv_bytes: 0B}]
    devices: [{id: gpu0}]
engine_config:
  context_length: 32768
```

`cold` covers the start, `ready` the running engine; a TensorFold model never
parks, so `parked` holds nothing and `parking` and `wake` repeat `ready` and
`cold`. The values above fit Nemotron 3.5 Lightning 30B-A3B 4-bit with a
32768-token context on a GB10: TensorFold estimated 27.7 GiB at startup and
CapyCTL measured a 20.2 GiB peak.

A drafter works as a draft model does for vLLM and SGLang: allow it when you
add the engine, with the directory that holds your drafters,

```bash
capyctl engine add ~/venvs/tensorfold --name tensorfold-drafter \
  --approve-option=--drafter --approve-path /srv/drafters
```

then pass it in the deployment with
`accept_extra_args: true` and `extra_args: [--drafter, /srv/drafters/my-drafter]`.
CapyCTL does not download drafters; without one it starts TensorFold with
`--drafter none`, so TensorFold never picks one from a cache. That turns off an
external draft model only: a checkpoint with built-in MTP heads, such as
Nemotron, still drafts (the response's `tensorfold` record shows
`"drafts":true`).

Some models need an explicit choice. TensorFold 0.6.1 refuses to start Qwen3.8
dense (Qwen3.8-27B) on an NVIDIA GPU without a drafter, and the start fails
with `TensorFold needs a drafter for this model: name one with --drafter, or
add --no-drafts to turn drafts off`. To run it without drafts, turn them off;
`--no-drafts` needs no approval when you add the engine:

```yaml
engine_config:
  context_length: 32768
  accept_extra_args: true
  extra_args: [--no-drafts]
```

CapyCTL then passes `--no-drafts` in place of `--drafter none`. `--no-drafts`
also turns off built-in MTP drafts; `--mtp-drafts 0` does the same and is
ordinary too. A deployment that passes both `--no-drafts` and `--drafter` is
refused. The first start builds kernels and can take several
minutes; CapyCTL allows it up to 30 minutes, and later starts reuse the build.

TensorFold is checked on NVIDIA GB10 (unified memory) in this release. On a
discrete GPU it runs, but no live check has passed there yet.

## First model on each engine

One example per engine, from adding it to a first answer, on one GB10 machine
running `capyctl start standalone` ([Run on one machine](one-machine.md)).
The deployment files are in [`docs/examples/`](../examples/). Each run below
started after the previous engine's deployment was deleted and its profile
removed, which is why the engines file revision grows. All three use the
same request; the API key comes from the credentials file the start banner
names:

```bash
KEY=$(sed -n 's/^api_key: //p' ~/.local/state/capyctl/identity/credentials)
```

### vLLM 0.29: Qwen3-4B

<!-- include: ../examples/deployment-vllm.yaml -->

```bash
capyctl engine add ~/venvs/vllm
```

```text
Registered vllm (vllm 0.29.0)

  Executable     /home/me/venvs/vllm/bin/vllm
  Deep park      enabled
  CUDA           /usr/local/cuda
  Engines file   /home/me/.config/capyctl/engines.yaml (revision 1)
  Published      yes
```

```bash
capyctl deploy model --file docs/examples/deployment-vllm.yaml
```

```text
Request identity: 01M3X7VR7MVTJ5DG7AMY6YA1P8 (reuse --request-id 01M3X7VR7MVTJ5DG7AMY6YA1P8 to recover this command)
Deployment qwen3-4b-vllm created (revision 1)

  Deployment ID       01M3X7VR8K2W783VV2JBJ195XW
  Operation           01M3X7VR8KYB08K0H8FXHBCM6G
  Checkpoint digest   being measured
the checkpoint digest of qwen3-4b-vllm is being measured; `capyctl start deployment qwen3-4b-vllm --wait` waits for it and starts the deployment
```

```bash
capyctl start deployment qwen3-4b-vllm --wait
```

```text
Request identity: 01M3X7VWFWGMFA1STYQDZ70SCQ (reuse --request-id 01M3X7VWFWGMFA1STYQDZ70SCQ to recover this command)
Waiting for the checkpoint digest of qwen3-4b-vllm to be measured (at most 900s)
Started qwen3-4b-vllm: ready

  Ready       1/1
  Hosts       host-b
  Operation   initialize succeeded
```

It was ready 55 seconds later, the checkpoint already downloaded. `/no_think`
turns off Qwen3's thinking for one message:

```bash
curl -s http://127.0.0.1:8443/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"model": "qwen3-4b-vllm", "messages": [{"role": "user", "content": "Name the largest planet in one sentence. /no_think"}]}' \
  | jq -r '.choices[0].message.content'
```

```text
<think>

</think>

The largest planet in our solar system is Jupiter.
```

### SGLang 0.5.20: Qwen3-4B

<!-- include: ../examples/deployment-sglang.yaml -->

```bash
capyctl engine add ~/venvs/sglang
```

```text
Registered sglang (sglang 0.5.20)

  Executable     /home/me/venvs/sglang/bin/python3
  Deep park      enabled
  CUDA           /usr/local/cuda
  Engines file   /home/me/.config/capyctl/engines.yaml (revision 3)
  Published      yes
```

```bash
capyctl deploy model --file docs/examples/deployment-sglang.yaml
```

```text
Request identity: 01M3X7YHH59FJKZP0WG1D9C4QG (reuse --request-id 01M3X7YHH59FJKZP0WG1D9C4QG to recover this command)
Deployment qwen3-4b-sglang created (revision 1)

  Deployment ID       01M3X7YHJ3Y9V6NDPSEAF48SXF
  Operation           01M3X7YHJ36M809539GJNNS8V8
  Checkpoint digest   being measured
the checkpoint digest of qwen3-4b-sglang is being measured; `capyctl start deployment qwen3-4b-sglang --wait` waits for it and starts the deployment
```

```bash
capyctl start deployment qwen3-4b-sglang --wait
```

```text
Request identity: 01M3X7YHJDDR6WARHDZDJEFP5N (reuse --request-id 01M3X7YHJDDR6WARHDZDJEFP5N to recover this command)
Waiting for the checkpoint digest of qwen3-4b-sglang to be measured (at most 900s)
Started qwen3-4b-sglang: ready

  Ready       1/1
  Hosts       host-b
  Operation   initialize succeeded
```

Ready after 61 seconds.

```bash
curl -s http://127.0.0.1:8443/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"model": "qwen3-4b-sglang", "messages": [{"role": "user", "content": "Name the largest planet in one sentence. /no_think"}]}' \
  | jq -r '.choices[0].message.content'
```

```text
<think>

</think>

The largest planet in our solar system is Jupiter.
```

### TensorFold 0.6.1: Nemotron 3.5 Lightning 30B-A3B 4-bit

<!-- include: ../examples/deployment-tensorfold.yaml -->

```bash
capyctl engine add ~/venvs/tensorfold
```

```text
Registered tensorfold (tensorfold 0.6.1)

  Executable     /home/me/venvs/tensorfold/bin/tensorfold
  Deep park      disabled
  CUDA           /usr/local/cuda
  Engines file   /home/me/.config/capyctl/engines.yaml (revision 5)
  Published      yes
```

```bash
capyctl deploy model --file docs/examples/deployment-tensorfold.yaml
```

```text
Request identity: 01M3X817ZABXRW67CQ2EY9KW4W (reuse --request-id 01M3X817ZABXRW67CQ2EY9KW4W to recover this command)
Deployment nemotron-30b created (revision 1)

  Deployment ID       01M3X817ZXRQP97RZQ3JDJQJ6F
  Operation           01M3X817ZXY1R64CTYQ51NPSHP
  Checkpoint digest   being measured
the checkpoint digest of nemotron-30b is being measured; `capyctl start deployment nemotron-30b --wait` waits for it and starts the deployment
```

```bash
capyctl start deployment nemotron-30b --wait
```

```text
Request identity: 01M3X818098DHVZCS7JH936A5W (reuse --request-id 01M3X818098DHVZCS7JH936A5W to recover this command)
Waiting for the model source of nemotron-30b to be downloaded and verified (at most 1800s)
Started nemotron-30b: ready

  Ready       1/1
  Hosts       host-b
  Operation   initialize succeeded
```

This first start downloaded the 18.5 GB checkpoint and was ready after
17.5 minutes; a later `start` after `stop` was ready in 10 seconds. Nemotron
thinks before it answers and returns the thinking in `reasoning_content`:

```bash
curl -s http://127.0.0.1:8443/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"model": "nemotron-30b", "messages": [{"role": "user", "content": "Name the largest planet in one sentence."}]}' \
  | jq -r '.choices[0].message.content'
```

```text
Jupiter is the largest planet in our solar system.
```

To clean up after each one, `capyctl delete deployment <name> --stop`, then
`capyctl engine remove <profile>` once the stop has finished. Neither touches
the downloaded weights.

## List and remove

```bash
capyctl engine list
```

```text
PROFILE        SOURCE         ENGINE   VERSION     CUSTOM   DEEP PARK   PUBLISHED   DEPLOYMENTS
vllm           engines.yaml   vllm     0.29.0      no       enabled     published   -
vllm-nightly   engines.yaml   vllm     0.30.0rc1   yes      enabled     published   -
```

`PUBLISHED` shows whether the running CapyCTL uses it (`unknown` while CapyCTL is
not running). `capyctl list engines` lists the engines of every host on a server, and of this machine on a standalone.

```bash
capyctl engine remove vllm-nightly
```

```text
Removed vllm-nightly

  Engines file   /home/me/.config/capyctl/engines.yaml (revision 3)
  Published      yes
```

Scripts add `--json` to any of these commands to get the JSON result, for
example `"published":"published"` in the record of `engine add` and
`engine remove`.

A profile a deployment still uses is not removed; the command names the
deployment. `--drain` stops those deployments first. Removing needs CapyCTL
running.

The last engine can be removed too. CapyCTL keeps running with none, keeps its
deployments and places none until you add one. Its start banner and
`capyctl status deployment` then say to run `capyctl engine add <path>`. A new deploy that names a
profile nobody publishes is refused at once.

`capyctl status deployment` shows the engine a deployment actually runs: its
profile, version and executable, as the deployment was set up with them. If you
register the profile again at a new version, an existing deployment keeps the
old engine, and the status adds a note to redeploy to use the new one.

A model can be named by its Hugging Face cache directory
(`snapshots/<rev>`) as it is.

## System services

When CapyCTL runs as a system service, run the engine commands with `sudo` and
the service's configuration file. This is the one place to name it: `sudo`
runs the command as root, which does not see the service's own records, and
`--config` makes it change the file the service reads:

```bash
sudo capyctl engine add /opt/vllm --config /etc/capyctl/host.yaml
```

Next: [Deploy a model](deploy.md).

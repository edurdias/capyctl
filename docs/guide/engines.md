# Add an engine

CapyCTL runs the vLLM, SGLang or TensorFold you already have. It does not install engines.
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

## Custom builds

`CUSTOM yes` means a version other than vLLM 0.29.0 or SGLang 0.5.20, the
versions this release of CapyCTL knows. CapyCTL still runs it. Give it its own
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

## TensorFold

CapyCTL runs TensorFold 0.6.0 from a plain venv. TensorFold builds CUDA kernels
the first time it starts, so the machine needs `nvcc`, `ninja` and a C++
compiler where the engine can find them: the venv's `bin`, the CUDA toolkit's
`bin`, or `/usr/local/bin`, `/usr/bin`, `/bin`. `engine add` checks this and
names what is missing; it never uses your shell's `PATH`.

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

A drafter works as a draft model does for vLLM and SGLang: allow it on the
engine (`security.approved_options: [--drafter]` and the drafter's directory in
`security.approved_paths`), then pass it with
`accept_extra_args: true` and `extra_args: [--drafter, /path/to/drafter]`.
CapyCTL does not download drafters; without one it starts TensorFold with
`--drafter none`. The first start builds kernels and can take several
minutes; CapyCTL allows it up to 30 minutes, and later starts reuse the build.

TensorFold is checked on NVIDIA GB10 (unified memory) in this release. On a
discrete GPU it runs, but no live check has passed there yet.

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

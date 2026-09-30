# Add an engine

mllm runs the vLLM or SGLang you already have. It does not install engines.
You register each installation once per machine; mllm calls it an engine
profile, and a deployment names the profile in `engine`.

## Find your installations

```bash
mllm engine detect
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
mllm engine add ~/venvs/vllm
```

```text
Registered vllm (vllm 0.29.0)

  Executable     /home/me/venvs/vllm/bin/vllm
  Deep park      enabled
  CUDA           /usr/local/cuda
  Engines file   /home/me/.config/mllm/engines.yaml (revision 1)
  Published      when mllm starts
saved to /home/me/.config/mllm/engines.yaml (revision 1); start mllm (`mllm start standalone`) to use it
```

The profile is named after the engine: `vllm` or `sglang`. mllm runs the
engine once to check its version and whether it supports parking, and records
the CUDA toolkit it finds for the engine's kernel builds.

That output is from a first run, before mllm was started: the engine is saved
and used from the first start. If mllm is running, it uses the engine at once
and prints `Published      yes`, as below.

On a GPU machine that runs a host, the same command adds the engine to the
host: the host records the file it was started with, and the engine commands
use it.

## Custom builds

`CUSTOM yes` means a version other than vLLM 0.29.0 or SGLang 0.5.20, the
versions this release of mllm knows. mllm still runs it. Give it its own
name so it does not replace your main one:

```bash
mllm engine add ~/venvs/vllm-nightly --name vllm-nightly
```

```text
Registered vllm-nightly (vllm 0.30.0rc1)

  Executable     /home/me/venvs/vllm-nightly/bin/vllm
  Deep park      enabled
  CUDA           /usr/local/cuda
  Engines file   /home/me/.config/mllm/engines.yaml (revision 2)
  Published      yes
```

A deployment then uses it with `engine: vllm-nightly`. Other options:
`--arg` adds an engine argument to every launch (repeatable),
`--deep-park disabled` turns parking off for this engine, and
`--drift refuse` refuses a launch if the installation's files changed since
you added it.

## List and remove

```bash
mllm engine list
```

```text
PROFILE        SOURCE         ENGINE   VERSION     CUSTOM   DEEP PARK   PUBLISHED   DEPLOYMENTS
vllm           engines.yaml   vllm     0.29.0      no       enabled     published   -
vllm-nightly   engines.yaml   vllm     0.30.0rc1   yes      enabled     published   -
```

`PUBLISHED` shows whether the running mllm uses it (`unknown` while mllm is
not running). `mllm list engines` lists the engines of every host on a server, and of this machine on a standalone.

```bash
mllm engine remove vllm-nightly
```

```text
Removed vllm-nightly

  Engines file   /home/me/.config/mllm/engines.yaml (revision 3)
  Published      yes
```

Scripts add `--json` to any of these commands to get the JSON result, for
example `"published":"published"` in the record of `engine add` and
`engine remove`.

A profile a deployment still uses is not removed; the command names the
deployment. `--drain` stops those deployments first. Removing needs mllm
running.

## System services

When mllm runs as a system service, run the engine commands with `sudo` and
the service's configuration file. This is the one place to name it: `sudo`
runs the command as root, which does not see the service's own records, and
`--config` makes it change the file the service reads:

```bash
sudo mllm engine add /opt/vllm --config /etc/mllm/host.yaml
```

Next: [Deploy a model](deploy.md).

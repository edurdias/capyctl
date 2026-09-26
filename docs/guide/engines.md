# Add an engine

mllm runs the vLLM or SGLang you already have. It does not install engines.
You register each installation once per machine; mllm calls it an engine
profile, and a deployment names the profile in `engine`.

## Find your installations

```bash
mllm engine detect
```

```text
ENGINE   VERSION     CUSTOM   ENVIRONMENT                   SOURCE
vllm     0.29.0      no       /home/me/venvs/vllm           venv
vllm     0.30.0rc1   yes      /home/me/venvs/vllm-nightly   venv
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
saved to /home/me/.config/mllm/engines.yaml (revision 1); start mllm (`mllm start standalone`) to use it
```

The profile is named after the engine: `vllm` or `sglang`. mllm runs the
engine once to check its version and whether it supports parking, and records
the CUDA toolkit it finds for the engine's kernel builds.

That output is from a first run, before mllm was started: the engine is saved
and used from the first start. If mllm is running, it uses the engine at once
and prints `"published":"published"`, as below.

On a GPU machine that runs a host, add `--config` with the host's file, as
the host was started with: `mllm engine add ~/venvs/vllm --config ~/host.yaml`.

## Custom builds

`CUSTOM yes` means a version other than vLLM 0.29.0 or SGLang 0.5.20, the
versions this release of mllm knows. mllm still runs it. Give it its own
name so it does not replace your main one:

```bash
mllm engine add ~/venvs/vllm-nightly --name vllm-nightly
```

```text
{"cuda_home":"/usr/local/cuda","custom":true,"deep_park":"enabled","deep_park_probe":"available","engine":"vllm","engines_file":"/home/me/.config/mllm/engines.yaml","executable":"/home/me/venvs/vllm-nightly/bin/vllm","fingerprint":{"digest":"sha256:75e6dea2b0a0bb2a620d8ac4c492c5cb89d9fb0debaa09c3504bfcd6c7adae57","version":"0.30.0rc1"},"profile":"vllm-nightly","published":"published","revision":2,"version":"0.30.0rc1"}
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
not running). On a server, `mllm list engines --config ~/server.yaml` lists
every host's engines.

```bash
mllm engine remove vllm-nightly
```

```text
{"engines_file":"/home/me/.config/mllm/engines.yaml","published":"published","removed":"vllm-nightly","revision":3}
```

A profile a deployment still uses is not removed; the command names the
deployment. `--drain` stops those deployments first. Removing needs mllm
running.

## System services

When mllm runs as a system service, run the engine commands with `sudo` and
the service's configuration file, so they change the file the service reads:

```bash
sudo mllm engine add /opt/vllm --config /etc/mllm/host.yaml
```

Next: [Deploy a model](deploy.md).

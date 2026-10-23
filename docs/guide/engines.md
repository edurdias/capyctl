# Add an engine

CapyCTL runs the vLLM, SGLang, TensorFold or llama.cpp you already have. It does not install engines.
To install one first, see [Install an engine](install-engines.md). A program
that is none of these can run as a [custom engine](#custom-engines).
You register each installation once per machine; CapyCTL calls it an engine
profile, and a deployment names the profile in `engine`.

## Find your installations

```bash
capyctl engine detect
```

```text
ENGINE   VERSION   CUSTOM   ENVIRONMENT                SOURCE
sglang   0.5.21    no       /home/me/venvs/sglang      venv
vllm     0.31.0    no       /home/me/venvs/vllm        venv
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
Registered vllm (vllm 0.31.0)

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

`CUSTOM yes` means a version other than vLLM 0.29.0 to 0.32.0, SGLang 0.5.20 and 0.5.21 or
TensorFold 0.6.0 to 0.6.3, 0.6.5 and 0.6.6, the versions this release of CapyCTL knows; every
llama.cpp build shows `yes` for now ([llama.cpp](#llamacpp)). CapyCTL still runs it. A custom build is
not a [custom engine](#custom-engines): CapyCTL still knows its options. Give it its own
name so it does not replace your main one:

```bash
capyctl engine add ~/venvs/vllm-nightly --name vllm-nightly
```

```text
Registered vllm-nightly (vllm 0.33.0rc1)

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

Some checkpoints ship their own Python (a model class or a configuration the
engine does not have yet), and the engine runs it only with
`trust_remote_code`. A deployment asks for it with
`engine_config.trust_remote_code: true`, and CapyCTL refuses that unless the
engine allows it. `--allow-remote-code` allows it:

```bash
capyctl engine add ~/venvs/vllm --name vllm-remote-code --allow-remote-code
```

It is written to `engines.yaml` as `security.trust_remote_code: true`, and
`capyctl engine add` shows `Remote code   allowed`. The code runs as the
engine's user with everything that user can reach, so allow it only for
checkpoints you trust, and keep it on its own engine name so your other
deployments do not get it. vLLM and SGLang take it; TensorFold and llama.cpp run
no code from a checkpoint, so `engine add` refuses it for them. For the role's
own engine (`--vllm-bin` and the others), the setting is `--trust-remote-code
true` on `capyctl start` ([Configuration](../operations/configuration.md)).

CapyCTL counts a draft model when it sizes a deployment: its weights in the
memory request, its CUDA graphs in the memory reserved for the first start,
and its KV cache layers in the context it fits to the KV cache.

On SGLang, `memory.kv_cache` is the size of the KV cache, as on vLLM. A hybrid
model (one with linear-attention layers) also keeps a state for each running
request, beside the KV cache, so its `memory.request` has to hold the weights,
the KV cache and that state. With a `memory.request` you state, CapyCTL runs
your `max_concurrent_requests` (or as many as fit, up to 8) and refuses to start
when they do not fit, naming the memory request they need: Qwen3.8-27B with
DFlash2, a 16 GiB KV cache and 8 requests needs about 65 GiB. Without one,
CapyCTL adds that state to the memory request it derives, for your
`max_concurrent_requests` (or 8), as far as the GPU or the memory pool holds
it, and `capyctl status deployment` says "Running limited to N requests by the
state cache" when it holds fewer than CapyCTL accepts (32 per deployment, or
your `max_concurrent_requests`). Requests past the running limit wait in
SGLang's queue. The default is 8, as on TensorFold, because the state grows with
each running request: Qwen3.8-27B with DFlash2 holds about 16 GiB of state for 8
requests and about 61 GiB for 32. Set `max_concurrent_requests` to run more.

## TensorFold

CapyCTL runs TensorFold 0.6.0 to 0.6.3, 0.6.5 and 0.6.6 from a plain venv. TensorFold builds
CUDA kernels the first time it starts, so the machine needs `nvcc`, `ninja` and
a C++ compiler where the engine can find them: the venv's `bin`, the CUDA
toolkit's `bin`, or `/usr/local/bin`, `/usr/bin`, `/bin`. Either install a system
CUDA toolkit, or (0.6.1 and later) put the compiler in the venv with pip:

```bash
pip install ninja "cuda-toolkit[nvcc,cccl]==13.0.*"
```

CapyCTL finds that `nvcc` under the venv's `site-packages/nvidia`. A C++ compiler
still comes from the system. `engine add` checks all of this and names what is
missing; it never uses your shell's `PATH`.

```bash
capyctl engine add ~/venvs/tensorfold
```

```text
Registered tensorfold (tensorfold 0.6.6)

  Executable     /home/me/venvs/tensorfold/bin/tensorfold
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
resources: {gpu: 30GiB, ram: 2GiB}
engine_config:
  context_length: 32768
```

`gpu` is what TensorFold holds on the GPU and `ram` what its process holds in
host RAM. CapyCTL reserves both while the model starts, runs and stops, and
nothing once it is stopped: a TensorFold model never parks. On a discrete card
`gpu` is reserved on the card and `ram` in host RAM; on a GB10 both come from
the one memory pool, so CapyCTL reserves their sum, 32 GiB. The values above fit
Nemotron 3.5 Lightning 30B-A3B 4-bit with a 32768-token context on a GB10:
TensorFold estimated 27.7 GiB at startup and CapyCTL measured a 20.2 GiB peak.

`capyctl validate config` lists what the two figures stand for; with
`--host <host.yaml>` it names the memory domains they are reserved in:

```bash
capyctl validate config --file docs/examples/deployment-tensorfold.yaml
```

```text
docs/examples/deployment-tensorfold.yaml is a valid deployment document

Resources
  cold     gpu 30GiB, ram 2GiB
  ready    gpu 30GiB, ram 2GiB
  parking  gpu 30GiB, ram 2GiB
  parked   gpu 0B, ram 0B
  wake     gpu 30GiB, ram 2GiB

Not checked
  resolution against a host: pass --host <host.yaml> to check the runtime profile, placement, the host's devices and capacity, and timeouts, and to see the host's defaults; a TensorFold profile under another name than tensorfold is recognised only there
  whether each allowed host is enrolled, online and has published (host_unpublished)
  which runtime profiles the host's role accepted and published after measuring each installation (profile_not_published)
  the host's current resource policy as the server stores it (resource_policy_unavailable)
  route and deployment-name conflicts with existing deployments (route_conflict)
  a new checkpoint's digest, measured on the host after acceptance (checkpoint_digest_pending)
```

The long form spells those five phases out, each with its `allocations` (a
memory domain of the host, its `bytes` and `host_kv_bytes`) and `devices`. It
is still accepted, and needed when a phase differs, for example a start that
needs more than the running model. The short form is for a model that does not
park: it means `residency: restart_only`, and any engine takes it.

The GPU figure is also TensorFold's memory cap: CapyCTL starts it with
`TENSORFOLD_CUDA_MEMORY_LIMIT_GB` set to it (on a GB10, to the sum; in the long
form, to the `ready` bytes). Without a cap TensorFold sizes
itself from the machine's free memory: with `--parallel` its caches grow as
requests get longer, and it keeps long prompts' states for later turns, past what
CapyCTL reserved. TensorFold 0.6.3 and later honour the cap;
earlier versions ignore it. The cap covers what TensorFold allocates on the GPU,
not the rest of the process, so leave a few GiB above what the model needs. A
window that does not fit beside the streams is refused by TensorFold at start,
or its requests wait, instead of taking memory CapyCTL did not reserve.

TensorFold decodes several requests together only when it is told how many.
CapyCTL starts it with `--parallel` set to the deployment's
`max_concurrent_requests`, or 8 when the deployment does not set it; requests
past that wait in TensorFold's queue. Lower than vLLM's 32 because TensorFold
reserves its drafter's buffers for every stream at start, inside the GPU
figure: Qwen3.8-27B with DFlash2 and a 32768-token context needs about 28 GiB
for one stream and 0.7 GiB more for each other one. A context that no longer
fits is refused at start, and `capyctl status deployment` says to lower
`max_concurrent_requests` or `context_length`, or raise `gpu`. The streams'
KV caches grow as their requests get longer: a new request waits while it would
not fit beside the others. To serve one request at a time, set
`max_concurrent_requests: 1`.
`--parallel` in `extra_args` also works and wins over the default; beside
`max_concurrent_requests` it is refused. Status shows the count:

```text
Streams up to 8 requests decoded together (CapyCTL default)
```

TensorFold 0.6.3 to 0.6.5 run Nemotron-H models one request at a time whatever
`--parallel` says, and status shows one stream for them.

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

Some models need an explicit choice. TensorFold 0.6.1 and later refuse to start Qwen3.8
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

## llama.cpp

CapyCTL runs llama.cpp's `llama-server` from v0.6.0, one GGUF model per
deployment. llama.cpp is a single binary, not a Python environment: build it
([Install an engine](install-engines.md#llamacpp-v060)) and name the binary, or
the directory that holds it:

```bash
capyctl engine add ~/llama.cpp/build/bin/llama-server
```

```text
Registered llamacpp (llamacpp 0.6.0)

  Executable     /home/me/llama.cpp/build/bin/llama-server
  Deep park      disabled
  Engines file   /home/me/.config/capyctl/engines.yaml (revision 1)
  Published      when capyctl starts
```

`engine add` runs `llama-server --version` once and reads the version and the
commit it prints (`version: 0.6.0 (build 1, commit d81235049)`). The profile's
version reads `0.6.0+d81235049`: the build number depends on the clone, so the
commit names the build. CapyCTL fingerprints the binary together with the
`lib*.so` files beside it (or in `<prefix>/lib`), since `llama-server` loads
its code from them; a rebuild in place is a change of installation. A symbolic
link is refused, naming its target: register the target. `engine detect`
finds `llama-server` on your `PATH`, in `~/llama.cpp/build*/bin`,
`/opt/*/bin` and `/usr/local/bin` without running it.

`engine add` refuses a machine that has `/etc/llama.cpp/config.ini`
(`engine_unsupported`), and a launch is refused while that file exists
(`engine_config_file`): llama-server reads it as options CapyCTL cannot see.
Your own `~/.config/llama.cpp/config.ini` is not read, because CapyCTL points
the engine's configuration directory at an empty one of its own.

Every llama.cpp build lists as `CUSTOM yes` for now. A build of v0.6.0 runs;
`custom` only says this release has not finished checking llama.cpp.

### A llama.cpp deployment

<!-- include: ../examples/deployment-llamacpp.yaml -->

`model` is a directory under your models directory holding one GGUF model: one
`.gguf` file, or the shards of one split model. With several, name the one to
serve in `engine_config.llamacpp.gguf_file`, a path inside the directory; a
vision model's projector goes in `engine_config.llamacpp.mmproj_file`.

The settings llama.cpp takes from the deployment:

| Setting | What it does | Default |
|---|---|---|
| `engine_config.context_length` | Each request's window, in tokens. Required. At most the model's training context. | none |
| `engine_config.max_concurrent_requests` | How many requests llama.cpp decodes together (its slots). | 4 |
| `engine_config.kv_cache_dtype` | The KV cache type: `f32`, `f16`, `bf16`, `q8_0`, `q4_0`, `q4_1`, `iq4_nl`, `q5_0` or `q5_1`. | `f16` |
| `engine_config.llamacpp.n_gpu_layers` | How many layers run on the GPU, or `all`. | `all` |
| `engine_config.llamacpp.gguf_file` | The GGUF file to serve, when the directory holds several. | the only one |
| `engine_config.llamacpp.mmproj_file` | A vision model's projector. | none |

Every slot gets the full window: CapyCTL starts llama-server with
`--ctx-size` set to the window, rounded up to a multiple of 256, times the
slots, and `--no-kv-unified`, so 8192 tokens and 4 slots are a 32768-token
cache split four ways. Status shows the slots:

```text
Streams up to 4 requests decoded together (CapyCTL default)
```

Without `resources` or `memory.request`, CapyCTL reads the GGUF header and
derives the memory: the GGUF files, the KV cache of every slot and a margin.
For Qwen3-4B Q4_K_M with 8192 tokens and 4 slots on a 16 GiB laptop GPU it
derived 8.3 GiB of GPU memory; llama-server used 7.2 GiB at its peak and
while it ran (2.3 GiB of weights, a 4.5 GiB KV cache, which is the figure
CapyCTL computed, and its compute buffers). The derivation is refused, naming
`engine_config.memory.kv_cache` or `resources` to state instead, for models
whose cache CapyCTL cannot size from the header (sliding-window, recurrent,
hybrid and MLA attention, embedding and encoder models), for `n_gpu_layers`
other than `all`, for options that move weights or the cache off the GPU
(`--override-tensor`, `--cpu-moe`, `--n-cpu-moe`, `--no-kv-offload`) and for a
draft model's own context (`--spec-type draft-*`, `--model-draft`). Declared
`resources` work as for any engine, but where CapyCTL can size the cache from
the header, a declared `memory.kv_cache`, `memory.request` or `resources` that
leaves less room for it than llama-server allocates at start is refused, naming
the field. A draft model counts with all of its shards; one CapyCTL cannot
count, outside the approved directories or with a shard missing, is refused.

llama.cpp has no way to free its memory while it runs, so a llama.cpp model
does not park, as TensorFold does not: `capyctl park deployment` refuses it,
and a deployment asking for `residency: deep` or `host_backed` is refused
`capability_missing`. When CapyCTL needs the memory, or the model sits idle
past `ready_idle_timeout`, it waits for the model's requests to finish,
stops llama-server and starts it again on the next request. Stopping took half
a second and released all of the GPU memory; starting again took about two
seconds for a 2.3 GiB model.

Refused for llama.cpp, with the reason:

- `engine_config.dtype`, `quantization`, `cuda_graphs`, `trust_remote_code: true`
  and `language_model_only: true`: a GGUF model carries its own types and
  llama.cpp runs no model code.
- `model.draft`: pass a draft model with `--model-draft` in `extra_args`,
  approved when you add the engine (below).
- A request with `cache_salt` (`cache_salt_unsupported`): llama.cpp's prompt
  cache cannot be split by it.
- Options CapyCTL renders or keeps off, in any spelling ([Engine
  options](engine-flags.md#llamacpp-v060)), and `--name=value`: llama-server
  takes `--name value` only.
- Engine variables llama-server reads as options, as its port and routes, as
  its devices or as locations: `LLAMA_ARG_*`, `LLAMA_SERVER_*`, `AIP_*`,
  `LLAMA_API_KEY`, `MTMD_BACKEND_DEVICE`, `HF_TOKEN`, the ggml device variables
  (`GGML_CUDA_DEVICES`, `GGML_CUDA_ENABLE_UNIFIED_MEMORY`,
  `GGML_VK_VISIBLE_DEVICES`, `GGML_BACKEND_PATH` and the like), `LLAMA_CACHE`,
  `XDG_CONFIG_HOME` and `HOME` (`engine_env_reserved:<name>`), even when
  `approved_env` lists them.

Options that read files, such as `--model-draft`, `--lora` or
`--chat-template-file`, need approval when you add the engine, with the
directories they may name:

```bash
capyctl engine add ~/llama.cpp/build/bin/llama-server --name llamacpp-draft \
  --approve-option=--model-draft --approve-path /srv/drafts
```

llama-server listens on loopback with no key, reached only through
CapyCTL's endpoint, which checks your API key. Its `/metrics` and `/slots`
pages tell CapyCTL when it is idle and how loaded it is; status lists those
pages as the deployment's unauthenticated loopback surface.

Each answer carries llama.cpp's `timings` object unchanged, and CapyCTL's
figures (`prefill_ms`, `decode_tokens_per_second`, `cached_tokens`,
`ttft_ms`) come from it ([Requests](requests.md)). Tool calls and
`reasoning_content` come from the model's chat template, with no parser to
choose.

llama.cpp has run on a discrete NVIDIA GPU so far; a GB10 has not run it yet.

## Custom engines

A custom engine is a program CapyCTL has no integration for, such as a model's
own server. You write down how to start it, in a manifest, and CapyCTL runs it,
sends it requests, counts its memory and keeps its log, as for any engine.

It is not a [custom build](#custom-builds). A custom build is another version
of vLLM, SGLang, TensorFold or llama.cpp, and CapyCTL still builds and checks
its command line as that engine's. A custom engine's options are unknown to
CapyCTL, so it checks none of them: the command in the manifest runs as
written, and a deployment cannot add to it. Use one when no supported engine
serves the model.

### The manifest

A manifest is a YAML file of at most 64 KiB. An unknown field is refused.

```yaml title="basal-serve.yaml"
kind: custom_engine
schema_version: 1
api: decisions
version: "2.2.0"
args: [--mode, eager, --host, "{host}", --port, "{port}", --model, "{model_dir}"]
health: /health
```

| Field | What it holds | Required |
|---|---|---|
| `kind`, `schema_version` | `custom_engine` and `1`. | yes |
| `api` | What every deployment on the engine serves: `chat` (`POST /v1/chat/completions`) or `decisions` (`POST /v1/systemone`, [Decision models](deploy.md#decision-models)). | yes |
| `args` | The whole argument list after the executable, with placeholders (below). | yes |
| `version` | A version string, shown as the engine's version. Without it the engine shows `unversioned`. CapyCTL never runs the program to ask. | no |
| `environment` | An absolute directory to fingerprint in place of the default ([Fingerprint and drift](#fingerprint-and-drift)). | no |
| `env` | Variables for every launch, `{NAME: value}`; values may use placeholders. | no |
| `health` | A path on the engine's own port, such as `/health`, that must answer 200 before CapyCTL checks the model. | no |
| `idle` | A path on the engine's own port that says when the engine has no running and no waiting requests: `url`, with `gauges` (Prometheus gauge names) or `json` (field paths in a JSON object), not both ([Cancelled requests](#cancelled-requests)). | no |

An `idle` entry reads like this:

```yaml
idle:
  url: /metrics
  gauges: [requests_running, requests_waiting]
```

`args` runs directly, never through a shell: there is no `$VARIABLE`, `~` or
quoting inside it. A placeholder is the only substitution, and its value stays
one argument, or part of one, even when it holds spaces.

| Placeholder | Value | Required |
|---|---|---|
| `{host}` | `127.0.0.1`, always | yes |
| `{port}` | the engine's own loopback port, from `resource_policy.endpoint_port_range` | yes |
| `{model_dir}` | the absolute path of the deployment's model directory (one under your models directory, or one CapyCTL downloaded) | yes |
| `{served_name}` | the model name CapyCTL sends in each request's `model` | no |
| `{context_length}` | the deployment's `engine_config.context_length` | no |
| `{api_key}` | a key CapyCTL makes for each launch ([The engine key](#the-engine-key)) | no |

`{{` and `}}` are literal braces: `'{{"port": {port}}}'` becomes
`{"port": 8101}`. Quote every value that starts with a placeholder in YAML:
`[--port, {port}]` is read as a YAML mapping, not as text, so write
`[--port, "{port}"]`.

`engine add` refuses the manifest with `custom_manifest_invalid` for an unknown
placeholder, a missing `{host}`, `{port}` or `{model_dir}`, a placeholder in
`health` or `idle`, `{api_key}` anywhere but `args` and `env` values, a `health`
or `idle.url` that is not a path, or an `environment` that is not an absolute
directory. A deployment on an engine that uses `{context_length}` must state
`engine_config.context_length`, or it is refused
`custom_placeholder_unset:context_length`. A deployment cannot add options to
the command through a value either: a first route (the served name) or a model
path that starts with `-` or holds a control character is refused
`custom_value_invalid:served_name` or `custom_value_invalid:model_dir`.

The manifest's `env` needs no approval, but a name CapyCTL sets itself is
refused (`engine_env_reserved:<name>`, [the list](../operations/configuration.md#engine-environment)).
The engine starts with a closed environment: `PATH` is the executable's
directory and then the system directories, `HOME` and `CUDA_VISIBLE_DEVICES`
come from CapyCTL, and `HF_HUB_OFFLINE=1` and `TRANSFORMERS_OFFLINE=1` are set,
since CapyCTL downloads models and the engine does not. `DO_NOT_TRACK=1` is
set too, unless the host allows engines to report their usage
([`engine_telemetry`](../operations/configuration.md#engine-usage-reporting)).
Then come the
manifest's variables, the engine's (`--env`) and the deployment's
(`engine_config.env`, approved with `--approve-env`), each winning over the one
before.

### Add a custom engine

```bash
capyctl engine add --custom ~/basal-2.2.0-venv/bin/basal-serve --name basal-serve \
  --manifest basal-serve.yaml
```

`--name` is required. The executable must be an executable file
(`engine_not_found` otherwise). `engine add` runs nothing: before it writes
anything, it prints the executable, the argument template with its
placeholders, the API, the version, the variable names with where each comes
from, the directory it fingerprints with the Python interpreter and the digest,
the idle signal and the engine key, then asks
`register this custom engine? [y/N]`. Only `y` or `yes` registers it; any
other answer writes nothing (`declined`). Without a terminal, pass `--yes`, or
the command is refused `not_interactive`. No other `engine add` asks.

`--env`, `--approve-env` and `--drift warn|refuse` work as for every engine
([Engine variables](#engine-variables)). The options that shape a known
engine's command (`--arg`, `--approve-option`, `--approve-path`,
`--allow-remote-code`) are refused with `--custom`, as is an installation
path beside it, and `--deep-park enabled` is refused `capability_missing`.
`capyctl engine detect` never lists a custom engine.

Only `capyctl engine add` on the machine itself adds a custom engine, run by
the role's user or root; no server command or management request can, so only
the machine's administrator decides what runs there. A server shows what runs:
`capyctl list engines --json` includes each custom engine's manifest (the
executable, the argument template, the variable names with where each comes
from, the idle signal and the API), and `capyctl inspect deployment <name>
--effective-config` shows the command with the engine key hidden.

The engine is saved in `custom-engines.yaml`, beside `engines.yaml`, with the
same revision line; `engines.yaml` takes no custom engine. A name is unique
across both files and the role's own document (`profile_exists`). The manifest
is copied in with its digest, so editing your manifest file later changes
nothing: to change the command, run `capyctl engine remove` and add the engine
again. A CapyCTL older than 0.1.6 does not read `custom-engines.yaml`: after a
downgrade the custom engines are gone and every other engine keeps working.
A custom engine can also be written into the role's own document
([Settings](../operations/configuration.md#custom-engines)).

### Fingerprint and drift

CapyCTL fingerprints the executable and what it runs with. For an executable in
a virtual environment's `bin` (the directory above it holds `pyvenv.cfg`), that
is every file of the environment, its `site-packages` included, and the Python
interpreter it runs, wherever that lives. Otherwise it is the executable
alone. A Python console script such as `basal-serve` is a few hundred bytes
that import the environment, so its own digest would pin nothing. The
manifest's `environment` names another directory instead, narrower (one
package) or elsewhere (a prefix the program loads from); the executable and the
interpreter still count.

The first fingerprint reads the whole environment, several GB with PyTorch; later
ones read only the files that changed. Past 100,000 files, 16 GiB or 32
directory levels the engine is `unmeasured`, never refused. The digest
`engine add` shows before you confirm is saved with the engine
(`custom.installation_digest`) and is the one every later measurement is
compared with: if the files changed between the confirmation and the role's
start, the engine starts out drifted rather than taking the new files as its
baseline. Every launch measures again. A different digest is drift: the
machine's status and its event log show it, and the launch is refused
(`installation_drift`) only if you added the engine with `--drift refuse`
(`security.installation_drift: refuse`); by default it warns. A custom engine
and a supported engine on the same executable are fingerprinted separately.
The version shown is the manifest's `version`, or `unversioned`, and every
custom engine lists as `CUSTOM yes`.

### What CapyCTL keeps and what it refuses

CapyCTL keeps, as for every engine:

- **The process.** It starts the program in its own process group as the
  role's user, stops it with SIGTERM to the group, and frees its memory only
  once every process has exited. An engine that exits is a failed instance. A
  program that starts a background daemon and exits leaves nothing CapyCTL can
  own, and fails.
- **The port.** `{host}` and `{port}` give the engine a private loopback
  port; CapyCTL's endpoint is the only way to it from the network.
- **Requests.** Lookup by model name, your API key, waiting, starting on
  demand, the 32 running requests per deployment and fairness between models.
  A request has the same shape as for any engine: chat completions, or
  CapyCTL's decision request ([Decision requests](requests.md#decision-requests)).
- **Readiness** (below).
- **Memory.** You state `resources`; CapyCTL derives nothing. It watches the
  engine's memory and shows the startup peak (`startup.measured` in
  `capyctl status deployment <name> --json`), but it caps nothing inside the
  engine: an engine that grows past what you stated is seen, not stopped.
- **Logs.** The engine's output goes to the launch's private, size-bounded log
  with keys hidden, and the management API returns its end
  ([engine logs](../operations/install.md#engine-logs-and-troubleshooting)).

CapyCTL refuses:

- **Parking.** A custom engine is `restart_only`, the default. `deep` or
  `host_backed` is refused `capability_missing:deep_park`, and
  `capyctl park deployment` refuses it. When CapyCTL needs the memory, or the
  model sits idle past `ready_idle_timeout`, it stops the engine and starts it
  again on the next request, as for TensorFold.
- **Groups.** A `topology` is refused `group_shape_unsupported:custom`. Several
  `instances` are fine, each its own process on its own port.
- **Sizing.** `resources` is required, as for TensorFold; `engine_config.memory`
  is refused.
- **Engine options from the deployment.** `accept_extra_args`, `extra_args`,
  a draft model and every typed engine field (`dtype`, `quantization`,
  `kv_cache_dtype`, `cuda_graphs`, `trust_remote_code`,
  `max_concurrent_requests`, ...) are refused
  `custom_field_unsupported:<field>`. `engine_config` takes `context_length`
  (used only through `{context_length}`) and `env`.
- **Parsers.** `tool_call_parser` and `reasoning_parser` are refused
  (`custom_field_unsupported:<field>`). A chat engine that parses tool calls
  does it with its own options, in the manifest.
- **`cache_salt`.** A chat request with `cache_salt` is refused
  `cache_salt_unsupported`: CapyCTL cannot know the engine honours it.
- **Engine figures.** There are no per-request engine figures (`prefill_ms`,
  decode speed, cached tokens) and no load reports. `ttft_ms` is CapyCTL's own,
  the engine's `usage` passes through, and CapyCTL picks an instance by its own
  count of running requests.

A deployment's `api` must be the manifest's: `api: decisions` on a chat engine
is refused `capability_missing:decisions`, and a chat deployment on a decisions
engine `capability_missing:chat`. For a decision model, a `topology` or a
parking `residency` is refused by the decision-model rules first, with their
message ("runs on one machine", "restarts instead of parking").

### Readiness

A custom engine is ready when, in order, its port accepts a connection, its
`health` path (when the manifest has one) answers 200, and the model answers
the same check as on any engine: a short chat request for `chat`, the fixed
decision for `decisions` ([Decision models](deploy.md#decision-models)), sent
with the served name and the engine key when there is one. The engine's own
`/v1/models` is not read; its shape differs from program to program. Without
`timeouts.initialize`, the start gets the bound CapyCTL derives for every
engine; a program that loads slowly may need a longer one.

### The listener check

Since CapyCTL cannot check the program's options, it checks the one that
matters most after the fact. Once the model passes its check, and before any
request reaches it, CapyCTL lists the TCP ports the engine's processes listen
on. A listener on any address but `127.0.0.1` or `::1` stops the engine, and
the start fails `custom_listener_exposed`, naming the address and port. The
check runs again whenever the machine proves the engine still runs, after a
reconnect or a CapyCTL restart. A port opened and closed again before the check,
or opened later, is not seen: the check narrows the exposure, it does not
prove there is none. A program that listens on every interface by default
needs `--host` and `"{host}"` (or its own option for the address) in `args`.

### The engine key

Without `{api_key}` the engine has no key and listens on loopback
unauthenticated: any local process on the machine can reach its port, as with
llama.cpp and TensorFold. `capyctl engine list` shows `ENGINE KEY none`.

With `{api_key}`, CapyCTL makes a new key for every launch, puts it where the
placeholder is, and sends it as `Authorization: Bearer <key>` with every
request, check and idle reading; the log hides it. Put it in `env` when the
program reads a key from a variable:

```yaml
env: {MY_ENGINE_KEY: "{api_key}"}
```

An argument is visible to every local user in the process list
(`/proc/<pid>/cmdline`); a variable is not. `engine add` warns when the
manifest puts the key in `args`.

### Cancelled requests

When a client hangs up, a request runs past its deadline, or the engine
answers with an error, the engine may still be working on the request.
CapyCTL keeps the request's memory counted as in use until the engine shows it
stopped. Known engines show it through their counts of running and waiting
requests; a custom engine shows it only through `idle`. With `idle`, CapyCTL
reads the path after the hang-up and counts the request finished once every
named gauge or field reads 0 (or `false`). A missing, malformed or failing
reading never counts as idle. A reading counts only for a request the engine
had answered before it was cut (part of its answer, or its error); one cut
before that may not have reached the engine's queue yet, and settles at the
next stop or restart as below, whatever the engine
([When a request is cut short](requests.md#when-a-request-is-cut-short)).

Without `idle`, nothing shows that the engine stopped working on the request,
and some engines do not: basal finishes a decision whose client has gone. So
the request stays counted until the engine's next stop or restart, whether you
stop it, it sits idle past its timeout, or CapyCTL switches to another model,
and until every one of its processes has exited. Until then its memory, and the
engine's, stays held. The request no longer takes one of the deployment's
running requests, so new requests are not held back, and stopping or switching
does not wait for it: the stop itself settles it. `capyctl status deployment
<name>` shows them in one line per instance, under the instance table, when
there are any:

```text
Instance 0  cancelling: 2 (settles at the next stop or restart: the engine reports no idle signal)
```

or, with `idle`, `Instance 0  cancelling: 1 (settles on the engine's idle signal)`,
or, when some were cut before the engine answered them,
`Instance 0  cancelling: 3 (2 settle on the engine's idle signal; 1 at the next stop or restart: cancelled before the engine answered)`.
With `--json`, each instance has `requests.cancelling`,
`requests.cancelling_settles` (`idle_signal` or `stop`), and how many of them
settle each way, `requests.settle_when_idle` and `requests.settle_at_stop`.
`capyctl engine list` shows each custom engine's `IDLE SIGNAL`, or `none`.

### What CapyCTL cannot promise

For the known engines, CapyCTL keeps request logging off and keeps the engine
from downloading models, because it sets their options. For a custom engine
those hold only if the manifest keeps them. basal's `--log-decisions <file>`
would write every request's answer to a file, and its `--mcp on` adds a route
with no authentication on the engine's port. `HF_HUB_OFFLINE` and
`TRANSFORMERS_OFFLINE` stop the Hugging Face libraries from downloading, but a
program can fetch in other ways. The engine log is still hidden from other
users and has keys removed.

### Example: basal decision models

basal's `basal-serve` serves the basal-2.0 and basal-omni-1.0 decision models
on `POST /v1/systemone`. The basal-omni models run only in basal's own
`--mode eager`, which no supported engine serves, so `basal-serve` 2.2.0 runs
as a custom engine, installed in a virtual environment, `~/basal-2.2.0-venv`.

The manifest is `basal-serve.yaml` from [The manifest](#the-manifest), added
with the command in [Add a custom engine](#add-a-custom-engine). CapyCTL
fingerprints `~/basal-2.2.0-venv` (it holds `pyvenv.cfg`) and its interpreter.
basal listens on every interface unless told otherwise, so `--host "{host}"`
matters. It has no key option, so there is no `{api_key}`: it listens on
loopback unauthenticated. It reports no running or queued count (its `/health`
always answers `ok`), so there is no `idle`, and a cancelled decision stays
counted until the next stop or restart. `--media on` is left out: basal reads
images and audio from a `media` field, which CapyCTL's decision request does
not carry. basal's other request extensions (`lang`, `detail`, `facts`,
`think`, the `multi`, `act` and `tool` question types) are refused
`invalid_request` before they reach it.

A deployment, with the model's repository (its `basal.json` and calibration
files included) in `basal-omni-1.0-max` under your models directory:

```yaml title="basal-omni-max.yaml"
name: basal-omni-max
engine: basal-serve
model: basal-omni-1.0-max
api: decisions
resources: {gpu: 32GiB, ram: 8GiB}
```

basal states about 24 GB of GPU memory for basal-omni-1.0-max in bf16 and
about 16 GB for basal-omni-1.0-mini. The `resources` above leave room until
the first start: then set them from the measured peak
(`capyctl status deployment basal-omni-max --json`, `startup.measured`).
CapyCTL starts, with the closed environment:

```text
/home/me/basal-2.2.0-venv/bin/basal-serve --mode eager --host 127.0.0.1 --port <port> \
    --model /home/me/models/basal-omni-1.0-max
```

basal opens its port once the model is loaded, `/health` answers 200, the
fixed decision comes back with probabilities that sum to 1, the listener check
finds one listener on `127.0.0.1`, and requests are let in.

<!-- live: CE1-CE4 -->

**basal answers every failure with HTTP 422.** A malformed request and an
engine failure (out of memory, an internal error) both come back as 422 with a
JSON body, so CapyCTL passes either on as `engine_rejected`, a rejected
request, and counts the request as answered. When a request you believe valid
is rejected, read the end of the instance's engine log
([engine logs](../operations/install.md#engine-logs-and-troubleshooting)) to
tell the two apart. A basal release that reports its running and queued
requests can be given an `idle` entry in a new manifest, with no CapyCTL
change.

## LMCache

[LMCache](https://github.com/LMCache/LMCache) keeps the KV cache of recent
prompt prefixes in host RAM, outside the engine. CapyCTL runs it as a private
server for each model instance, started and stopped with it, so a model that
wakes from a park, or restarts, can answer a prompt that begins like an
earlier one from that cache instead of recomputing it (ADR 0033).

**What runs.** Each instance of a deployment that uses LMCache gets its own
`lmcache server`, from the engine's own environment, as a CapyCTL-owned
process:

- listening only on loopback, on two ports CapyCTL leases from the host's
  `endpoint_port_range`; a server found listening anywhere else is stopped
  and the model runs without it;
- with a pool of host RAM (4 GiB by default, `lmcache_pool` to change it)
  allocated when it starts, evicting least recently used entries, and no disk,
  remote or peer tier;
- with an environment of `PATH`, `HOME` and the instance's GPU only, and its
  script API off;
- writing its output to its own log beside the engine's, through the same
  redacting writer.

The engine connects to it: vLLM through LMCache's `LMCacheMPConnector`
(CapyCTL renders `--kv-transfer-config` with the connector's module path and
`autostart: false`), SGLang through `--enable-lmcache --lmcache-config-file`
(a private file holding only the server's address). The in-process
`LMCacheConnectorV1` is never used: it ignores `cache_salt`.

**Which engines.**

| Engine | LMCache |
|---|---|
| vLLM 0.32.0 or newer | yes |
| vLLM 0.31 and older | off: "LMCache needs vLLM 0.32.0 or newer" |
| SGLang 0.5.22 or newer | yes, for chat deployments |
| SGLang, a decision deployment or `--disable-radix-cache` | off: `engine_unsupported` (below) |
| SGLang before 0.5.22 | off: "LMCache needs SGLang 0.5.22 or newer" |
| llama.cpp, TensorFold | off: the engine has no LMCache integration |
| custom engine | off: CapyCTL does not render options for it |

vLLM 0.32.0 and SGLang 0.5.22 with LMCache 0.5.6 are the qualified versions
(measured on a GB10); a newer engine or a later LMCache 0.5.x is used when
the launch-time check below passes, and is not qualified until a live run
covers it. A deployment with a `topology` runs without LMCache.

**Decision deployments on SGLang run without LMCache**, and so does any SGLang
deployment whose arguments state `--disable-radix-cache`: SGLang 0.5.22 fails
with LMCache when its radix cache is disabled, which it always is for the
embedding models that serve decisions
([sgl-project/sglang#45929](https://github.com/sgl-project/sglang/issues/45929)).
Status shows "LMCache off" with reason `engine_unsupported` and the issue,
and `lmcache: required` on such a deployment is refused,
`lmcache_unavailable:engine_unsupported`. This lifts once a SGLang release
with the fix is qualified. A model SGLang disables its radix cache for on its
own fails its first start with LMCache and starts again without it (below).

**Install it in the engine's environment**, then register the engine again
(CapyCTL never installs it, and it uses only an LMCache that `engine add` saw):

```bash
# vLLM 0.32.0: the published wheel
~/vllm-0.32.0-venv/bin/pip install lmcache==0.5.6
# SGLang 0.5.22 pins torch 2.14.1; build LMCache against it
~/sglang-0.5.22-venv/bin/pip install --no-build-isolation lmcache==0.5.6
capyctl engine add ~/vllm-0.32.0-venv --name vllm032
```

LMCache 0.5.6 is the oldest release CapyCTL accepts (it fixes
CVE-2026-105192); a later 0.5.x is accepted. `engine add` records LMCache's
version and a digest of its files on the profile, and every launch measures it
again: a changed digest is drift, flagged and handled like the engine's
(`security.installation_drift`). An LMCache installed after `engine add` is
not used ("LMCache was installed after the engine was registered; register
the engine again").

**The launch-time check** reads files of the registered environment and runs
nothing. It passes when LMCache is 0.5.6 or a later 0.5.x; its package holds
the connector the engine needs, the `lmcache server` entry point and the
server options CapyCTL renders; vLLM's KV transfer configuration accepts a
connector module path; and SGLang declares its LMCache settings and selects
its unified LMCache radix cache. On SGLang the check also verifies that the
request's salt reaches LMCache: SGLang passes `cache_salt` to LMCache's
unified connector, which puts it in the cache key, and SGLang does not use
LMCache's older SGLang adapter (which dropped the salt, LMCache issue 4825).
A release without that path fails the check, and its deployments run without
LMCache until a release has it. A failed check names what was missing.

**`capyctl engine list`** shows an `LMCACHE` column: the registered version,
or `off (<reason>)` for a profile that cannot use it.

**Status.** `capyctl status deployment <name>` shows a line per instance:

```text
LMCache   on: pool 4 GiB, used 1.3 GiB, overhead 0.8 GiB (measured), 212 hits / 260 lookups
LMCache   off: LMCache needs vLLM 0.32.0 or newer
```

and `--json` an `lmcache` object (`state`, `reason`, `pool_bytes`,
`used_bytes`, `overhead_bytes`, `measured`, `lookups`, `hits`; a figure
CapyCTL could not read is `null`, never zero). The event `lmcache_off` records
an instance and reason once per launch.

**Memory.** An instance's reservation holds the pool plus the server's own
memory in host RAM (on a GB10, the memory the GPU uses too), and the pool
counts against `host_kv_limit`. Until a deployment's first measurement the
server's own memory is charged at 1280 MiB plus 512 MiB of GPU memory, the
largest figures measured on a GB10 plus a quarter; after it, at what was
measured. The charge stays while the model is parked. `memory.request` stays
the engine's own GPU memory.

**Choosing.** A deployment's `lmcache` is `auto` (the default), `off` or
`required`. With `auto`, a deployment that cannot use LMCache runs without
it and says why. With `required` it refuses to start instead:
`lmcache_unavailable:<reason>` (`retention_zero:lmcache`,
`host_capability_missing:lmcache`), and a failure found at launch leaves
nothing running. A host's `lmcache: off` or `retention: zero` keeps it off
on that machine ([settings](../operations/configuration.md#lmcache-and-zero-retention)).

**When the engine does not start with it.** An engine that exits or does not
become ready with the connector is stopped with its server and started once
more without LMCache, with a warning and an `lmcache_off` event
(`engine_start_failed`); later starts of that revision with that engine go
straight to the start without LMCache. Under `required` the start fails
instead. A server that does not answer within 30 seconds of starting is
stopped and the engine starts without it (`server_start_failed`). A server
that exits while its engine serves fails the instance, which is started again
with a new, empty server (`lmcache_server_exited`).

**Local reach.** Any process on the machine can reach the server's loopback
ports, as it can reach a llama.cpp engine: read its configuration, list or
clear its cache, and store or look up KV under any salt. On a machine with
untrusted local users, set the host's `lmcache: off` or `retention: zero`.

**Options.** CapyCTL renders the LMCache options itself: SGLang's
`--enable-lmcache` and `--lmcache-*` are refused in `extra_args`, so is a
vLLM `--kv-transfer-config` naming an LMCache connector, and `LMCACHE_*`
variables are reserved. A deployment that passes another KV connector runs
without LMCache (`kv_transfer_config_set`)
([engine options](engine-flags.md)).

## Tool calls and reasoning

vLLM and SGLang return structured `tool_calls` and a separate reasoning trace
only when they run with a tool-call parser and a reasoning parser. CapyCTL
picks both from the model's `config.json` and chat template for the families it
knows, with nothing in the deployment file:

| Model family | vLLM | SGLang |
|---|---|---|
| Qwen3.5, Qwen3.6, Qwen3.8 (`qwen3_5`, `qwen3_5_moe`) | `--tool-call-parser qwen3_coder --reasoning-parser qwen3 --enable-auto-tool-choice` | `--tool-call-parser qwen3_coder --reasoning-parser qwen3` |
| Qwen3 (`qwen3`, `qwen3_moe`) | `--tool-call-parser hermes --reasoning-parser qwen3 --enable-auto-tool-choice` | `--tool-call-parser qwen25 --reasoning-parser qwen3` |

A Qwen3 checkpoint whose template uses the XML tool-call format (Qwen3-Coder)
gets `qwen3_coder`, a template without tool calls gets no tool parser, and one
without `<think>` (an instruct-only model) gets no reasoning parser. Any other
model gets no parser, as before. `capyctl status deployment` prints the choice
under the engine, here for FrogNano 4B (a Qwen3.5 checkpoint) on vLLM 0.30:

```text
Engine  vllm 0.30.0 (/home/me/venvs/vllm/bin/vllm)
Parsers tool calls: qwen3_coder, reasoning: qwen3 (model family qwen3_5)
```

To choose yourself, set either parser in the engine's block: `auto` (the
default), `none`, or the engine's parser name.

```yaml
engine_config:
  vllm:
    tool_call_parser: hermes
    reasoning_parser: none
```

The same option in `extra_args` still works and wins over `auto`, so recipes
that pass `--tool-call-parser` and `--reasoning-parser` by hand run unchanged;
those arguments are no longer needed for the families above. Naming a parser
in the block and passing the same option in `extra_args` is refused.
TensorFold handles tool calls itself and has no parser setting. llama.cpp
parses tool calls and reasoning from the model's chat template, with no setting.

[Engine options](engine-flags.md) lists, for each engine, which options are
typed fields, which pass in `extra_args`, which need host approval and which
CapyCTL reserves.

## vLLM weight loading while parking

A vLLM deployment that parks `deep` reloads its weights on every wake. Unless
you set a loader, CapyCTL uses `eager` on every machine.

`eager` reads each weight file into memory before loading. On a discrete GPU
(vLLM 0.29, Qwen3-4B) this cut a wake from 57 s to 7.5 s. On a GB10 (vLLM
0.30, Qwen3.8-27B NVFP4) each `eager` wake took 23-28 s and answered
correctly.

`lazy` maps the weight files instead of reading them up front. On a machine
with unified memory (GPU and system memory share one pool, like a GB10) it
holds far less memory: with Qwen3.6-35B NVFP4 on vLLM 0.30, around 34 GiB
while loading against `eager`'s 54 GiB, about 27 GiB against 44 GiB once
running, and around 8 GB still held after parking and waking three times in
a row against `eager`'s 28 GB. The cost is a slower start and wake (about
81 s against 51 s there). On vLLM 0.30 with Qwen3.8-27B NVFP4, a `lazy` wake
took over two minutes to reload the weights and did not complete, so check
that your model wakes within its wake window (`timeouts.wake`) before relying
on it. Choose it in the deployment:

```yaml
engine_config:
  vllm:
    safetensors_load_strategy: lazy   # eager (the default while parking) or lazy
```

A deployment that does not park renders the loader only when you set it.
Passing `--safetensors-load-strategy` in `extra_args` is refused; use the
field.

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

### vLLM 0.31: Qwen3-4B

<!-- include: ../examples/deployment-vllm.yaml -->

```bash
capyctl engine add ~/venvs/vllm
```

```text
Registered vllm (vllm 0.31.0)

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
Request identity: 01M511394XJDYWP78SNPBMDWPF (reuse --request-id 01M511394XJDYWP78SNPBMDWPF to recover this command)
Deployment qwen3-4b-vllm created (revision 1)

  Deployment ID       01M511395WX25N9ZWT1BQAGMX0
  Operation           01M511395WXENKZ1KDWHMYT5Q9
  Checkpoint digest   being measured
the checkpoint digest of qwen3-4b-vllm is being measured; `capyctl start deployment qwen3-4b-vllm --wait` waits for it and starts the deployment
```

```bash
capyctl start deployment qwen3-4b-vllm --wait
```

```text
Request identity: 01M5113969KYJNVCZ4JQQEQB32 (reuse --request-id 01M5113969KYJNVCZ4JQQEQB32 to recover this command)
Waiting for the model source of qwen3-4b-vllm to be downloaded and verified (at most 900s)
Started qwen3-4b-vllm: ready

  Ready       1/1
  Hosts       host-b
  Operation   initialize succeeded
```

It was ready 53 seconds later, the checkpoint already downloaded. `/no_think`
turns off Qwen3's thinking for one message:

```bash
curl -s http://127.0.0.1:8443/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"model": "qwen3-4b-vllm", "messages": [{"role": "user", "content": "Name the largest planet in one sentence. /no_think"}]}' \
  | jq -r '.choices[0].message.content'
```

```text


The largest planet in our solar system is Jupiter.
```

CapyCTL picks Qwen3's reasoning parser ([Tool calls and
reasoning](#tool-calls-and-reasoning)), so the empty thinking block goes to
the message's `reasoning` field (`reasoning_content` on SGLang) and only the
two line breaks after it stay in `content`.

### SGLang 0.5.21: Qwen3-4B

<!-- include: ../examples/deployment-sglang.yaml -->

```bash
capyctl engine add ~/venvs/sglang
```

```text
Registered sglang (sglang 0.5.21)

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
Request identity: 01M51156B1F2ZDE4GD8ZHYZXTV (reuse --request-id 01M51156B1F2ZDE4GD8ZHYZXTV to recover this command)
Deployment qwen3-4b-sglang created (revision 1)

  Deployment ID       01M51156BX5HSE082HWSF304MG
  Operation           01M51156BX8TZ8KK2ZCYAQ181Y
  Checkpoint digest   being measured
the checkpoint digest of qwen3-4b-sglang is being measured; `capyctl start deployment qwen3-4b-sglang --wait` waits for it and starts the deployment
```

```bash
capyctl start deployment qwen3-4b-sglang --wait
```

```text
Request identity: 01M51156C7C3FPGR5E0QEP063P (reuse --request-id 01M51156C7C3FPGR5E0QEP063P to recover this command)
Waiting for the checkpoint digest of qwen3-4b-sglang to be measured (at most 900s)
Started qwen3-4b-sglang: ready

  Ready       1/1
  Hosts       host-b
  Operation   initialize succeeded
```

Ready after 91 seconds.

```bash
curl -s http://127.0.0.1:8443/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"model": "qwen3-4b-sglang", "messages": [{"role": "user", "content": "Name the largest planet in one sentence. /no_think"}]}' \
  | jq -r '.choices[0].message.content'
```

```text


The largest planet in our solar system is Jupiter.
```

### TensorFold 0.6.6: Nemotron 3.5 Lightning 30B-A3B 4-bit

<!-- include: ../examples/deployment-tensorfold.yaml -->

```bash
capyctl engine add ~/venvs/tensorfold
```

```text
Registered tensorfold (tensorfold 0.6.6)

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
Request identity: 01M5118700HSCYNZM3D0P5FF1B (reuse --request-id 01M5118700HSCYNZM3D0P5FF1B to recover this command)
Deployment nemotron-30b created (revision 1)

  Deployment ID       01M511870J843YX6WWPYTRG6BM
  Operation           01M511870JHJ94JDGQQVH4QMDP
  Checkpoint digest   being measured
the checkpoint digest of nemotron-30b is being measured; `capyctl start deployment nemotron-30b --wait` waits for it and starts the deployment
```

```bash
capyctl start deployment nemotron-30b --wait
```

```text
Request identity: 01M511870W3RP14F3CVZXSSHXM (reuse --request-id 01M511870W3RP14F3CVZXSSHXM to recover this command)
Waiting for the model source of nemotron-30b to be downloaded and verified (at most 1800s)
Started nemotron-30b: ready

  Ready       1/1
  Hosts       host-b
  Operation   initialize succeeded
```

The 18.5 GB checkpoint was already downloaded; this first start was ready
after 109 seconds, and a later `start` after `stop` in 12 seconds. A first
download adds its own time (17.5 minutes on an earlier run). Nemotron thinks
before it answers and returns the thinking in `reasoning_content`:

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

## Engine variables

`--env` gives the engine a variable on every launch with the profile.
`--approve-env` names the variables a deployment may set in
`engine_config.env` (a name, or a name ending in `*`):

```bash
capyctl engine add ~/venvs/sglang --env SGLANG_ENABLE_JIT_DEEPGEMM=0 --approve-env 'MBX_*'
```

Both are repeatable. CapyCTL refuses names it sets itself
(`engine_env_reserved:<name>`), and a deployment name the profile does not
approve (`engine_env_not_approved:<name>`). For a model across machines,
register the same variables and approvals on every machine. Variables, the
names CapyCTL owns and how status shows them:
[Engine environment](../operations/configuration.md#engine-environment).

## Groups across machines

A model can run as a group, one rank per machine
([One model across machines](several-machines.md#one-model-across-machines)).
What each engine runs:

| | vLLM 0.30.0 | SGLang 0.5.21 | TensorFold 0.6.5 |
|---|---|---|---|
| Tensor parallel across machines | yes | yes | `tensor_parallel: 2` only |
| Pipeline parallel across machines | yes | yes | no |
| Machines | any number that divides tensor × pipeline parallel | any number that divides tensor × pipeline parallel | exactly 2 |
| Parking | deep, every rank | deep, every rank | restart only |

Any shape in the table is accepted, with no warning. Live qualification covers
two machines only, with the versions in the table header; other versions this
release knows are expected to behave the same but were not run as groups.

SGLang groups park and wake again. In 0.1.3 they restarted instead: in a live
run (SGLang 0.5.21, tensor parallel 2 across two GB10 machines) a group's
first wake reloaded the head's weights, then failed `restore_uncertain` and
stopped the group. CapyCTL had read each machine's memory report before
checking the woken model, and by the time those checks finished the reports
were too old to record. A wake now records reports read after its checks.
With no `residency`, an SGLang group gets `deep`.

## List and remove

```bash
capyctl engine list
```

```text
PROFILE        SOURCE                ENGINE   VERSION     CUSTOM   DEEP PARK   IDLE SIGNAL   ENGINE KEY   PUBLISHED   DEPLOYMENTS
basal-serve    custom-engines.yaml   custom   2.2.0       yes      disabled    none          none         published   -
vllm           engines.yaml          vllm     0.31.0      no       enabled     -             -            published   -
vllm-nightly   engines.yaml          vllm     0.32.0rc1   yes      enabled     -             -            published   -
```

`IDLE SIGNAL` and `ENGINE KEY` apply to custom engines only (`-` for the
others): the idle signal the manifest states, or `none`, and whether CapyCTL
gives the engine a key per launch (`per-launch`) or it listens without one
(`none`).

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

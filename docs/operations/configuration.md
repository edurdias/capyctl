# Settings reference

Every setting CapyCTL reads is listed here, with each of the ways to state it.
Every setting of a role document can be stated three ways: in the YAML role
document, on the command line that starts the role, and in the environment.
The settings people change most have their own flag and variable (the tables
below); any setting, including those, can also be named by its YAML path with
`--set path=value` or `CAPYCTL_SET__PATH=value` (see
[Any setting by its path](#any-setting-by-its-path)). One rule decides which wins, for
every setting and every role:

**`--set` > `CAPYCTL_SET__…` > named flag > named variable > YAML document > default**

`capyctl config show` prints the value each setting will have and where it came
from (see [Seeing the effective configuration](#seeing-the-effective-configuration)).

Standalone is a server and one host in one process, so a host setting has the
same flag, variable and default in both roles. Its YAML path is the same too:
in a host document it sits at the top level, in a standalone document under
`host:`. A server setting in a standalone document sits under `server:`.

A value that cannot be read (a malformed size, an unknown switch value, a
relative path where an absolute one is needed) is refused with the name of the
flag, variable or field that stated it. It is never replaced by the default.
An empty variable counts as unset, except for the switches `CAPYCTL_DEEP_PARK`,
`CAPYCTL_INSTALLATION_DRIFT` and `CAPYCTL_ENGINE_PORTS`, where an empty value is
refused: an opt-out that was mistyped must not be read as "on".

`capyctl validate config --file <file>` checks any document offline;
`--set path=value` checks it with a setting changed, as a start would.

## Files and state

| Setting | YAML | Flag | Variable | Default | Roles |
|---|---|---|---|---|---|
| Role document | (none) | `--config <file>` | `CAPYCTL_CONFIG` | the running role's ([Which role a command uses](#which-role-a-command-uses)), else `<state root>/config/<role>.yaml` | server, host, standalone, `capyctl engine` |
| State root | `state_dir` at the top of a standalone document named by `--config` or `CAPYCTL_CONFIG` | `--state-dir <dir>` | `CAPYCTL_STATE_DIR` | `$XDG_STATE_HOME/capyctl`, else `~/.local/state/capyctl` | every command |
| Role state directory | `state_dir` (server, host); `server.state_dir`, `host.state_dir` (standalone) | `--state-dir <dir>` | `CAPYCTL_STATE_DIR` | `<state root>` (server, host); `<state root>/server`, `<state root>/host` (standalone) | server, host, standalone |
| Registered engines file | (none) | `--config` (the file beside it) | `CAPYCTL_CONFIG` (the file beside it), `XDG_CONFIG_HOME` | `~/.config/capyctl/engines.yaml` | host, standalone, `capyctl engine` |

The role document cannot name itself, so it has no YAML form. The state root
is where CapyCTL looks for the implicit role document, so its YAML form is read
only from a standalone document named with `--config` or `CAPYCTL_CONFIG` (a
relative path resolves against the document's directory). A server or host
document's `state_dir` is where that role keeps its state; `init server` and
`init host` write it from the state root. As for standalone, `--state-dir`
wins over `CAPYCTL_STATE_DIR`, which wins over the document's `state_dir`; when
one of them overrides the document, the role uses it (its identity under
`<state dir>/identity`) and prints one notice naming what it overrode.
`join host` follows the same rule and takes `--set` like `start host`. A standalone document may state
`server.state_dir` and `host.state_dir` only as `<state root>/server` and
`<state root>/host`. Commands find the role running on this machine under the
state root (see [Which role a command uses](#which-role-a-command-uses)), so
they need the same `--state-dir` or `CAPYCTL_STATE_DIR` as the role.

## Which role a command uses

A command run on a machine uses the role running there; nothing needs to be
set each time. The role document is, first match wins:

| Setting | YAML | Flag | Variable | Default |
|---|---|---|---|---|
| Role a command uses | (none) | `--config <role document>` | `CAPYCTL_CONFIG` | the role running on this machine |

- **On the server machine**, client commands (`list`, `status`, `deploy`,
  `start`, `stop`, `park`, `delete`, `drain`, `revoke`, `invite`, ...) use the
  server's management API.
- **On a standalone machine**, they use the standalone role's.
- **On a host machine**, `capyctl engine` and `capyctl config show` use the host's
  document. A host has no management API, so a command that needs the server is
  refused with "This machine is a capyctl host; run this command on the server".

The role is found under the state root from what it records there: a server's
or standalone role's credentials, the management address it serves on, and,
for a server or host started with `--config` or `CAPYCTL_CONFIG` (as the packaged
units start them), the document it was named with (`<state root>/run/`). A
command therefore needs the same `--state-dir` or `CAPYCTL_STATE_DIR` as the role,
and runs as the same user.

When more than one role keeps its state under the same root:

- a server and a host, or a standalone role and a host: client commands use
  the server or standalone role, since a host has no management API;
- a server and a standalone role: the one that answers is used; when both or
  neither answer, the command is refused, naming both and the `--config` that
  chooses;
- `config show` with more than one role is refused, naming them; choose with
  `--role` or `--config`.

The management API is served on loopback only, on every role, so another
machine is managed by running the command on it (for example over `ssh`).

## Listeners

| Setting | YAML | Flag | Variable | Default | Roles |
|---|---|---|---|---|---|
| Inference address | `listeners.inference.bind` (server); `server.listeners.inference.bind` (standalone) | `--listen <addr:port>` | `CAPYCTL_INFERENCE_ADDR` (`CAPYCTL_STANDALONE_INFERENCE_ADDR` still read, with a warning) | `0.0.0.0:8443` | server, standalone |
| Inference API key required | `listeners.inference.authentication: api_key\|none` (server; `server.` prefix in standalone) | `--no-inference-auth` | `CAPYCTL_INFERENCE_AUTH=none` | `api_key` | server, standalone |
| Management address | `listeners.management.bind` (server); `server.listeners.management.bind` (standalone) | `--management-listen <addr:port>` | `CAPYCTL_MANAGEMENT_ADDR` (`CAPYCTL_STANDALONE_MANAGEMENT_ADDR` still read, with a warning) | `127.0.0.1:7443` | server, standalone, and their client commands |
| Server bootstrap and control listeners | `listeners.bootstrap`, `listeners.control` | (none) | (none) | `127.0.0.1:7444`, `:7445` | server |

The inference listener serves every interface by default and requires the
API key; [network access](network-access.md) covers narrowing it, the key,
turning the key off (a warning is printed on a non-loopback address) and a TLS
reverse proxy. A server or standalone document that still states the old
default `127.0.0.1:8443` is updated once on the first start of 0.1.0
([install](install.md#upgrading-to-010)).

The management address stays on loopback in every form (any port). A host
has no management listener. Each start records the address it serves on in
`<state directory>/run/management-address` (owner-only), and client commands
(`capyctl status`, `capyctl deploy`, ...) find it from `CAPYCTL_MANAGEMENT_ADDR`, else
that record, else the role document, else the default, so a role started
with `--management-listen` needs nothing more for its clients. The server's
bootstrap and control listeners carry TLS identities and enrollment
addresses that must agree with each other, so they are stated together in
the document (or with `--set`).

## Models and downloads

| Setting | YAML | Flag | Variable | Default | Roles |
|---|---|---|---|---|---|
| Models directory (relative model paths resolve here) | `model_store.path` | `--models-root <dir>` | `CAPYCTL_MODELS_ROOT` | `~/models` | host, standalone |
| Hugging Face and HTTP downloads | `model_sources.huggingface`, `model_sources.http` (`allowed` or `disabled`) | `--model-sources allowed\|disabled` (both kinds) | `CAPYCTL_MODEL_SOURCES` (both kinds) | `allowed` | host, standalone |
| Cap on all downloaded models | `model_sources.max_bytes` | `--model-sources-max <size>` | `CAPYCTL_MODEL_SOURCES_MAX` | `500GiB` | host, standalone |
| Where downloads are kept | `model_sources.path` | `--model-sources-path <dir>` | `CAPYCTL_MODEL_SOURCES_PATH` | the models directory (`~/models/sources`) | host, standalone |
| Hugging Face endpoint for downloads | `model_sources.huggingface_endpoint` (`https://`) | `--hf-endpoint <url>` | `CAPYCTL_HF_ENDPOINT`, else `HF_ENDPOINT` | `https://huggingface.co` | host, standalone |
| Plain `http://` URLs for HTTP downloads | `model_sources.plain_http` (`allowed` or `disabled`) | `--model-sources-plain-http allowed\|disabled` | `CAPYCTL_MODEL_SOURCES_PLAIN_HTTP` | `denied` (HTTPS only) | host, standalone |
| Hugging Face endpoint for pinning `hf:` references | the role document's `model_sources.huggingface_endpoint` | `--hf-endpoint <url>` on `deploy model` | `CAPYCTL_HF_ENDPOINT`, else `HF_ENDPOINT` | `https://huggingface.co` | `capyctl deploy model` |
| Trust a local checkpoint's declared digest without a full read | `checkpoints.trust_declared_digest` | `--trust-declared-digest true\|false` | `CAPYCTL_TRUST_DECLARED_DIGEST` | `false` | host, standalone |
| Hosts downloads may come from | `model_sources.allowed_hosts` | (none) | (none) | any | host, standalone |

`deploy model` pins an `hf: owner/repo` reference without a commit to the
commit it names now. The endpoint it asks follows the same rule; its YAML layer
is the host or standalone document named by `--config` or `CAPYCTL_CONFIG`, else
the standalone document under the state root. A loopback `http://` mirror is
accepted there; a host downloads over `https://` only. `allowed_hosts` is a
list, which has no flag or variable form: state it in the document.

A deployment may declare its speculative drafter's weights as a source of
their own, `model.draft`, in the deployment document only (deployment fields
are YAML-only, as every other deployment field). It takes every spelling
`model.source` takes (`{type: huggingface, ...}`, `{http: {url, sha256}}`,
`{local: {path}}`) and the model's shorthands (`draft: drafts/d` for a local
path relative to the models directory, `draft: {hf: owner/repo@<commit>}`,
which `deploy model` pins like the model's). It is validated like
`model.source` (pinned commit or SHA-256, HTTPS, `secret://` token
references) and needs the host's `model_sources` policy like the weights. A
remote drafter is downloaded and verified into the sources store beside them
and the deployment activates once both copies are verified; its weights are
counted with the checkpoint's. CapyCTL passes the drafter's directory itself,
so no `approved_paths` entry is needed for it:

| Engine | What CapyCTL passes | What the deployment's engine arguments still say |
|---|---|---|
| SGLang | `--speculative-draft-model-path <dir>` | `--speculative-algorithm` (required); no draft path of their own |
| vLLM | `model: <dir>` merged into the deployment's `--speculative-config` | a `--speculative-config` with `num_speculative_tokens` (and the method), approved as before; no `model` of its own |
| TensorFold | `--drafter <dir>` instead of `--drafter none` | neither `--drafter` nor `--no-drafts` |

```yaml
model:
  source: {huggingface: {repo: org/model, revision: <40-character commit>}}
  draft: {http: {url: https://example.com/draft.tar, sha256: <64 hex>, archive: tar}}
```

## Engine installation

These describe the role's own engine installation. One executable is published
as the runtime profile `local`; several as `local-vllm`, `local-sglang` and
`local-tensorfold`. On a
host the profile is added to the document the host publishes, beside the
profiles in `runtime_profiles` and the ones registered with `capyctl engine add`;
a name stated twice is refused. The switches below apply to the `local`
profiles only: a declared or registered profile states its own `security`
block.

| Setting | YAML | Flag | Variable | Default | Roles |
|---|---|---|---|---|---|
| vLLM executable | `local_engine.vllm` | `--vllm-bin <path>` | `CAPYCTL_VLLM_BIN` | none | host, standalone |
| SGLang interpreter | `local_engine.sglang` | `--sglang-bin <path>` | `CAPYCTL_SGLANG_BIN` | none | host, standalone |
| TensorFold executable | `local_engine.tensorfold` | `--tensorfold-bin <path>` | `CAPYCTL_TENSORFOLD_BIN` | none | host, standalone |
| Build fingerprint | `local_engine.build_fingerprint` | `--engine-fingerprint <text>` | `CAPYCTL_ENGINE_FINGERPRINT` | what `<engine> --version` prints | host, standalone |
| Host-fixed vLLM arguments | `local_engine.args` (a list) | `--engine-args "<args>"` | `CAPYCTL_ENGINE_ARGS` (space-separated) | none | host, standalone |
| KV cache of generated deployments | `local_engine.kv_cache` | `--kv-cache <size>` | `CAPYCTL_KV_CACHE_BYTES` | `16GiB` (unified), sized from the GPU (discrete) | standalone |
| Deep parking | `local_engine.deep_park: on\|off` | `--deep-park on\|off` | `CAPYCTL_DEEP_PARK` | `on` | host, standalone |
| Run checkpoint-supplied Python (`trust_remote_code`) | `local_engine.trust_remote_code` | `--trust-remote-code true\|false` | `CAPYCTL_TRUST_REMOTE_CODE` (`1` or `true`) | `false` | host, standalone |
| When the installation's files change | `local_engine.installation_drift: warn\|refuse` | `--installation-drift warn\|refuse` | `CAPYCTL_INSTALLATION_DRIFT` | `warn` | host, standalone |
| CapyCTL's runtime directory | `runtime_dir` | `--runtime-dir <dir>` | `CAPYCTL_RUNTIME_DIR` | the managed copy in `<state dir>/runtime` | host, standalone |
| Engine port range (loopback) | `resource_policy.endpoint_port_range` (`start`, `end`) | `--engine-ports <start-end>` | `CAPYCTL_ENGINE_PORTS` (`CAPYCTL_STANDALONE_ENGINE_PORTS` still read, with a warning) | `8100-8199` | host, standalone |
| CUDA toolkit for engine kernel builds (vLLM's FlashInfer, TensorFold's first start) | `local_engine.cuda_home` | `--cuda-home <dir>` | `CAPYCTL_CUDA_HOME` | none (the engine PATH stays minimal) | host, standalone |

A host generates no deployment of its own, so `--kv-cache` exists only on
`start standalone`; on a host, `local_engine.kv_cache` is refused and each
deployment states `engine_config.memory.kv_cache`. SGLang takes no host-fixed
arguments, so `args` applies to the vLLM and TensorFold profiles only. Name `runtime_dir` only
to run from a directory you maintain yourself; CapyCTL never writes to it.

Engine tuning such as vLLM's loader under deep parking
(`engine_config.vllm.safetensors_load_strategy`: `eager` or `lazy`) is a deployment
setting, written in the deployment file like every other `engine_config` field; see
[Add an engine](../guide/engines.md#vllm-weight-loading-while-parking).

A running host or standalone takes its engine and model settings at start. A
live `capyctl engine add` or `remove` changes runtime profiles only; any other
change needs a restart.

## Multi-node groups

| Setting | YAML | Flag | Variable | Default | Roles |
|---|---|---|---|---|---|
| Request-stall timeout: a request to a multi-node group with no first token within it makes the server probe the group's head once, and stop the group (`group_stalled`) if the probe fails too | `groups.stall_timeout` (server); `server.groups.stall_timeout` (standalone) | `--group-stall-timeout <duration>` (`start server`) | `CAPYCTL_GROUP_STALL_TIMEOUT` | `120s` (`1s` to `3600s`) | server, standalone |
| Peer address: this machine's address on the direct link that group members use | `resource_policy.groups.peer_address` | `--peer-address <ip>` | `CAPYCTL_PEER_ADDRESS` | none (the machine cannot join a group) | host, standalone |
| Rendezvous ports a group head picks from | `resource_policy.groups.rendezvous_port_range` (`start`, `end`) | `--rendezvous-ports <start-end>` | `CAPYCTL_RENDEZVOUS_PORTS` | `25000-25099` | host, standalone |
| Refuse a group when `memlock` or `infiniband` is missing | `resource_policy.groups.require_rdma` | `--require-rdma true\|false` | `CAPYCTL_REQUIRE_RDMA` | `false` | host, standalone |

An idle group is never probed: only a request in flight can start the probe,
and requests to single-host deployments are not watched. Standalone never runs
a group, so its value has no effect there.

Each setting follows one order: flag, then variable, then YAML, then the
default. The host checks are read, never changed. The `compaction` check never
refuses: it warns with either value of `require_rdma`. `memlock` and
`infiniband` warn by default and refuse only with `require_rdma: true`.

For a group launch CapyCTL sets `GLOO_SOCKET_IFNAME` to the interface that
holds the member's peer address. It sets no `NCCL_*` variable and passes none
through, so there is no NCCL tuning setting.

## Engine environment

Engine variables come from two places: the engine profile (every launch with
it) and the deployment (only that deployment, and only names the profile
approves). A deployment value wins over a profile value of the same name.

| Setting | YAML | Flag | Variable |
|---|---|---|---|
| Profile variables | `env` in the profile | `--env K=V` (repeatable, `capyctl engine add`) | `CAPYCTL_ENGINE_ADD_ENV` (`K=V;K=V`) |
| Names a deployment may set | `security.approved_env` in the profile (a name, or a name ending in `*`) | `--approve-env <glob>` (repeatable, `capyctl engine add`) | `CAPYCTL_APPROVE_ENV` (`GLOB;GLOB`) |
| Deployment variables | `engine_config.env` in the deployment | `--engine-env K=V` (repeatable, `capyctl deploy model`) | `CAPYCTL_ENGINE_ENV` (`K=V;K=V`) |

Between flag and variable, a flag wins for the same name. Any `--approve-env`
replaces `CAPYCTL_APPROVE_ENV` as a whole; the variable is used only when no
flag is given. A name both in the deployment file and on the command line is
refused (`engine_env_conflict:<name>`).

Status and the effective configuration show each name and where it came from,
never its value. Values are stored as written: this is not secret storage.

CapyCTL owns these names; neither a profile nor a deployment can set them,
whatever an approval says (`engine_env_reserved:<name>`):

- prefixes `NCCL_`, `GLOO_`, `MASTER_`, `CAPYCTL_`, `LD_`, `PYTHON` (except
  `PYTHONUNBUFFERED`);
- `VLLM_HOST_IP`, `SGLANG_HOST_IP`, `HOST_IP`, `SGLANG_LOCAL_IP_NIC`,
  `SGLANG_DISTRIBUTED_INIT_METHOD_OVERRIDE`, `TF_COMM_BACKEND`, `TF_NCCL_LIB`,
  `PATH`, `PYTHONPATH`, `CUDA_HOME`, `CUDA_VISIBLE_DEVICES`, `HOME`,
  `CUDA_DEVICE_ORDER`, `HF_HUB_OFFLINE`, `TRANSFORMERS_OFFLINE`,
  `VLLM_PLUGINS`, `VLLM_SERVER_DEV_MODE`, `VLLM_API_KEY`,
  `TORCH_EXTENSIONS_DIR`, `TENSORFOLD_CUDA_MEMORY_LIMIT_GB`,
  `TENSORFOLD_NO_UPDATE_CHECK`, `VLLM_PORT`,
  `VLLM_ALLOW_INSECURE_SERIALIZATION`.

Names are compared upper-cased. `RUST_LOG`, `TOKENIZERS_PARALLELISM`,
`PYTHONUNBUFFERED`, `MAX_JOBS` and `FLASHINFER_NVCC_THREADS` need no approval.

## Secrets

A secret is never a command-line flag: flags are visible to every user in the
process list and are kept in shell history. Secrets are protected files that
CapyCTL writes or reads, or environment variables.

| Secret | File | Variable | Notes |
|---|---|---|---|
| Inference API key and management admin token | standalone: `<state root>/identity/credentials` (owner-only, `api_key:` and `admin_token:` lines), generated on first start; server: `<identity_dir>/server-credentials.json` (owner-only JSON), created by `init server` | (none) | Generated, never typed or printed; read the file to use them ([network access](network-access.md#default)). |
| Server enrollment and host identity | `identity_dir` (`<state dir>/identity`) | (none) | Created by `init server` and `join host`. |
| Per-launch engine keys | written by CapyCTL for each launch | (none) | Never an operator setting. |
| Hugging Face token for a source that names none | `model_sources.huggingface_token_file` (absolute path to an owner-only file) | `CAPYCTL_HF_TOKEN`, else `HF_TOKEN` | The variable wins over the file. The host that downloads reads it; `deploy model` also uses the variable to pin a private repository. |
| Hugging Face token for one source | `model.source.token_ref: secret://<name>` in the deployment (`type: huggingface`), naming `<state dir>/secrets/<name>` on the host (owner-only) | (none) | Wins over the host's default token. |
| A download URL for one source | `model.source.url_ref: secret://<name>` in the deployment (`type: http`, instead of `url`), naming `<state dir>/secrets/<name>` on the host (owner-only) | (none) | For a presigned or otherwise sensitive URL. The deployment, its revision, status and logs hold the reference only; the host reads the URL when it fetches, checks it against its own `model_sources` policy, and verifies the payload against `sha256`. Rotating the file's contents needs no new revision. |

A token file readable by other users is refused, not used.

## Settings without a named flag or variable

These are structured policies (named domains, devices, profiles, labels,
queues) or server policies that must agree with each other, so they have no
flag or variable of their own. State them in the document, or change one for
a run with `--set` or `CAPYCTL_SET__…` (next section), and restart the role.

| Settings | Where |
|---|---|
| Role name, fingerprints | `name`, `hardware_fingerprint`, `environment_fingerprint` |
| Enrollment addresses | server: `enrollment.bootstrap_address`, `enrollment.control_address` |
| Idle, heartbeat, switching and shutdown bounds (unset, an idle timer is off: no idle model is stopped or parked) | server: `lifecycle_defaults`, `control`, `switching.drain_timeout` (standalone: `server.lifecycle_defaults`, `server.switching`); every role: `shutdown.drain_timeout` |
| Response timing header | server: `observability.timing_header` (standalone: `server.observability`) |
| Private ingress | host: `ingress` |
| Memory domains, devices, limits, queues, labels, parked growth | host: `resource_policy` (standalone derives its own: its document accepts `auto` values there, the memory limits `memory.system.managed_limit`, `free_reserve` and `parked_limit`, `parked_growth_limit`, `endpoint_port_range`, and the `queue` bounds of a host) |
| Runtime profiles | host: `runtime_profiles`; or `capyctl engine add` (its own flags: `--name`, `--deep-park`, `--drift`, `--arg`, `--approve-option`, `--approve-path`; they write `engines.yaml`, which a host and standalone read alike) |
| Load report period | host: `load_report_interval` |
| Activation policy: `on_demand` (default), or `explicit`, under which every deployment that may run on the host starts, stops, parks and wakes only on an operator's action | host: `lifecycle.activation` (standalone: `host.lifecycle.activation`) |

`lifecycle.activation` follows the server's lifecycle settings: the document,
`--set lifecycle.activation=explicit` on `capyctl start host` (or
`--set host.lifecycle.activation=explicit` on `capyctl start standalone`), or
`CAPYCTL_SET__LIFECYCLE__ACTIVATION=explicit`
(`CAPYCTL_SET__HOST__LIFECYCLE__ACTIVATION` for standalone); any other value
refuses the start. A host publishes it with its document, so a change takes a
restart. A deployment states the same policy for itself as
`lifecycle.activation` in its own document; deployment fields are document-only,
like `lifecycle.warm`. A deployment is explicit when it or any host it may run
on says so. What that changes, and how it combines with `lifecycle.warm` and an
operator's stop: [Only on your command](../guide/parking.md#only-on-your-command).

## Any setting by its path

Every setting in a server, host or standalone document can be changed without
editing the file:

- on the command line, with `--set <path>=<value>` (repeatable) on
  `capyctl start server`, `capyctl start host`, `capyctl start standalone`,
  `capyctl validate config` and `capyctl config show`;
- in the environment, with `CAPYCTL_SET__<PATH>=<value>`, where a double
  underscore separates the keys of the path. Keys match the document's field
  names in any case, so `CAPYCTL_SET__SHUTDOWN__DRAIN_TIMEOUT=45s` sets
  `shutdown.drain_timeout`. A name inside a map (a listener, a runtime
  profile, a label) is read in lower case from a variable.

The path is the field's place in the document, in the document's own shape:
a standalone document's host settings are under `host.` and its server
settings under `server.`.

```bash
# A longer shutdown drain for this run of a server.
capyctl start server --set shutdown.drain_timeout=90s

# A host that reports its engine load every 2 seconds, from its unit file.
CAPYCTL_SET__LOAD_REPORT_INTERVAL=2s

# A host where no request waits: one beyond a model's 32 running requests, or
# for a model not loaded, is refused 429 at once (see the parking guide).
capyctl start host --set resource_policy.queue.max_pending_per_deployment=0

# Standalone: the switch drain bound and the response timing header.
capyctl start standalone --set server.switching.drain_timeout=45s \
  --set server.observability.timing_header=true

# Standalone: a longer silence bound between a reply's outputs, from the environment.
CAPYCTL_SET__HOST__RESOURCE_POLICY__QUEUE__STREAM_IDLE_TIMEOUT=600s
```

The same standalone setting in its document:

```yaml
host:
  resource_policy:
    queue:
      stream_idle_timeout: "600s"
```

### Standalone memory limits

Standalone admits deployments against limits it derives from the memory it
observes at start: a managed limit of 50 % and a free reserve of 20 % (on a
discrete-GPU machine, of host RAM; each card's limits are derived from the card
and are not settable). To admit a larger model, raise the managed limit with a
size or a whole percentage:

```bash
capyctl start standalone --set host.resource_policy.memory.system.managed_limit=90GiB
CAPYCTL_SET__HOST__RESOURCE_POLICY__MEMORY__SYSTEM__MANAGED_LIMIT=75%
```

```yaml
host:
  resource_policy:
    memory:
      system:
        managed_limit: "90GiB"   # or "75%"; auto is 50 %
        free_reserve: auto       # auto is 20 %
```

The managed limit and the free reserve must fit the memory together; a start
whose limits do not is refused with both numbers. Every start applies the limits
in force to the stored policy, and `auto` returns to the default. A limit lowered
below what running models already hold is refused until they stop. A higher limit
leaves less memory for everything else on the machine.

How a value is read and checked:

- A value is read exactly as the same text in the YAML document would be:
  `true` and `false` are booleans, `30` is a number, `30s` and `16GiB` are
  text; a list is written in YAML's bracket form, `[--enforce-eager, --max-num-seqs, 4]`.
  The document is then checked exactly as if the file said it, so a duration
  written as `30` or a timeout outside its range is refused, naming the
  override that stated it.
- A path that does not exist is refused, with the valid paths nearest to it.
  A path that names a block (such as `shutdown`) is refused with the settings
  inside it.
- `--set` wins over `CAPYCTL_SET__…` for the same setting; both win over the
  document.
- A setting that also has a named flag or variable (the tables above) can be
  stated both ways only if the two agree: `--deep-park on` together with
  `--set local_engine.deep_park=off` (or `CAPYCTL_DEEP_PARK=on` with
  `CAPYCTL_SET__LOCAL_ENGINE__DEEP_PARK=off`) refuses the start and names both.
- Secrets are never command-line values. `--set` is refused for a setting
  that names a key, a token or a credential (for example
  `model_sources.huggingface_token_file`, a profile's `credential_ref`, or a
  profile's engine `env`); state it in the document or with `CAPYCTL_SET__…`.
- A host applies its overrides again when `capyctl engine add` or `remove`
  reloads its document, so a live reload compares the file plus the same
  overrides with what the host runs.

### Standalone parked limit

Parked models keep part of their memory: their parked footprint. Standalone
lets parked models hold at most a quarter of the memory it observes (on a
discrete-GPU machine, of host RAM). A park whose footprint would exceed it is
refused (`parked_capacity`) and the model stays loaded; a parked
Qwen3.8-27B with a DFlash2 drafter measured 33.4 GiB, above the quarter of a
128 GB machine. Raise the parked limit with a size or a whole percentage:

```bash
capyctl start standalone --set host.resource_policy.memory.system.parked_limit=40GiB
CAPYCTL_SET__HOST__RESOURCE_POLICY__MEMORY__SYSTEM__PARKED_LIMIT=35%
```

```yaml
host:
  resource_policy:
    memory:
      system:
        parked_limit: "40GiB"    # or "35%"; auto is 25 %
```

`--set` wins over the variable, and both over the document. The parked limit
is part of the managed memory, so it may not exceed the managed limit: a start
whose parked limit does is refused with both numbers (raise the managed limit
too). `0B` parks nothing. A document that leaves it out or says `auto` keeps the
derived quarter, and its stored policy is unchanged. A card's own parked limit
(the CUDA contexts parked engines leave on it) stays derived. A host role states
its limits literally, in `resource_policy.domains.<domain>.parked_limit`, and
takes `--set` and `CAPYCTL_SET__…` the same way.

### Parked growth limit

Some engines leave memory behind on every park and wake: vLLM 0.30 on a GB10
held a parked charge of 4.7 GiB after its first park, 9.6 GiB after the
second and 13.4 GiB after the third, while its GPU memory returned to the same
22.1 GiB on every wake. CapyCTL measures every park, so it charges what the
engine holds, but the engine holds more each cycle. A fresh start gives that
memory back.

So once a launch's parked charge has grown past a bound since its first
measured park, its next park is a stop instead: an idle park, a switch that
releases it, and `capyctl park` alike. The stop is an ordinary one, so the
next request starts the model fresh; `capyctl status` prints a `Parked` line
(`its next park stops it (parked_growth)`, then `stopped instead of parked`),
and the JSON status lists the numbers under `parked.growth`.

The bound is host policy, `resource_policy.parked_growth_limit`:

- `auto` (the default): the first parked charge again, so a launch may double
  it;
- a whole percentage of the first parked charge, `0%` to `10000%`;
- a size of growth, such as `8GiB`;
- `off`: never stop for growth.

```bash
# A host role.
capyctl start host --set resource_policy.parked_growth_limit=50%
CAPYCTL_SET__RESOURCE_POLICY__PARKED_GROWTH_LIMIT=8GiB

# Standalone.
capyctl start standalone --set host.resource_policy.parked_growth_limit=off
CAPYCTL_SET__HOST__RESOURCE_POLICY__PARKED_GROWTH_LIMIT=200%
```

```yaml
host:
  resource_policy:
    parked_growth_limit: auto   # or "50%", "8GiB", off
```

A host document states it as `resource_policy.parked_growth_limit`. `--set`
wins over the variable, and both over the document. A document that leaves it
out or says `auto` keeps the default, and its published and stored policy is
unchanged.

## Seeing the effective configuration

`capyctl config show` prints the value each setting of a role will have, and
where it came from: `default`, `yaml`, `env` (a named variable or
`CAPYCTL_SET__…`), `flag` or `set`. It reads the document named by `--config` (or
`CAPYCTL_CONFIG`), whose `kind` is the role; without one it reads the document
of the role on this machine (`--role server|host|standalone` chooses; with no
role there yet, standalone's), which may not exist yet. It applies `--set` and the environment
as the start would, checks the result the same way, and writes nothing.

```text
$ capyctl config show --set server.switching.drain_timeout=45s
standalone (document /home/me/.local/state/capyctl/config/standalone.yaml)
SETTING                                            VALUE                               SOURCE
host.connection                                    embedded                            yaml
host.local_engine.deep_park                        on                                  default
host.local_engine.installation_drift               warn                                default
host.local_engine.trust_remote_code                false                               default
host.model_sources.http                            allowed                             default
host.model_sources.huggingface                     allowed                             default
host.model_sources.huggingface_endpoint            https://huggingface.co              default
host.model_sources.max_bytes                       500GiB                              default
host.model_sources.plain_http                      denied                              default
host.model_store.path                              /home/me/models                     default
host.name                                          local                               yaml
host.resource_policy.allowed_devices               auto                                yaml
host.resource_policy.endpoint_port_range.end       8199                                default
host.resource_policy.endpoint_port_range.start     8100                                default
host.resource_policy.memory.accounting             auto                                yaml
host.resource_policy.memory.system.free_reserve    auto                                yaml
host.resource_policy.memory.system.managed_limit   auto                                yaml
host.resource_policy.memory.system.parked_limit    auto                                yaml
host.resource_policy.parked_growth_limit           auto                                yaml
host.state_dir                                     /home/me/.local/state/capyctl/host     yaml
name                                               local                               yaml
server.groups.stall_timeout                        120s                                default
server.listeners.inference.authentication          api_key                             yaml
server.listeners.inference.bind                    0.0.0.0:8443                        yaml
server.listeners.management.authentication         admin_token                         yaml
server.listeners.management.bind                   127.0.0.1:7443                      yaml
server.name                                        local                               yaml
server.observability.timing_header                 false                               default
server.state_dir                                   /home/me/.local/state/capyctl/server   yaml
server.switching.drain_timeout                     45s                                 set
shutdown.drain_timeout                             30s                                 default
state_dir                                          /home/me/.local/state/capyctl          default
```

`--format json` (or `--json`) prints the JSON result instead:
`{"role", "document", "settings": [{"path", "value", "source"}]}`. The flags a
role start takes (`--deep-park`, `--listen`, ...) are not options of `config
show`; their variables are read from the environment, and `--set` stands in
for them.

`config show` does not ask a running role. It shows what a start with the
same document, the same environment and the same `--set` options would use,
so a role started with `--set`, a named flag or a unit's `.env` file shows
those values only when `config show` is given the same `--set` options or
variables.

## Command options that are not settings

`--format text|json` (`--json`), `--output`, `--request-id`, `--wait`,
`--activate`, `--evict`, `--initialize-timeout`, `validate config --host` and
the other per-command options change one command's behaviour, not the role's
configuration, so they have no variable or YAML form. `--debug-engine-logs` on
`start host` and `start standalone` is a flag only on purpose: full engine logs
are written without redaction and may contain secrets, so a variable left in a
shell or unit file must not turn them on. The engine log's redaction, rotation
(16 MiB, two older files) and the management tail bound (256 KiB) are fixed,
not settings ([engine logs](install.md#engine-logs-and-troubleshooting)).

Commands print text (tables, or a summary with details) unless `--format json`
is given, even when their output is piped. `start server`, `start host` and
`start standalone` print text when their output is a terminal and one JSON
object per line otherwise, so the journal and log files are JSON; `--format`
overrides that either way.

## Variables CapyCTL sets for engines

CapyCTL starts each engine with a closed environment. `CAPYCTL_ENGINE_LOG`,
`CAPYCTL_EXTRA_APPROVALS`, `CAPYCTL_RENDEZVOUS_DIR`, `CAPYCTL_OBSERVATION_DIR`,
`CAPYCTL_VLLM_ADMIN_KEY`, `CAPYCTL_ENGINE_API_KEY` and `CAPYCTL_DEBUG_ENGINE_LOGS` are
written by CapyCTL for the engine process; setting them yourself has no effect.
For TensorFold it also writes `TENSORFOLD_NO_UPDATE_CHECK`, `HF_HUB_OFFLINE`,
`TRANSFORMERS_OFFLINE` and `TORCH_EXTENSIONS_DIR`
(`<state dir>/engines/tensorfold/<version>/torch_extensions`, private to the
service user).
Other variables in CapyCTL's own environment do not reach an engine. The CUDA
toolkit is the exception by design: a profile's `cuda_home` (stated as above
for the role's own installation, detected by `capyctl engine add`, or written in
`runtime_profiles`) puts `<cuda_home>/bin` on the engine's PATH and sets its
`CUDA_HOME`.

The installer has its own options; see [Install](install.md#installing-with-installsh).

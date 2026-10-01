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
| Hugging Face endpoint for pinning `hf:` references | the role document's `model_sources.huggingface_endpoint` | `--hf-endpoint <url>` on `deploy model` | `CAPYCTL_HF_ENDPOINT`, else `HF_ENDPOINT` | `https://huggingface.co` | `capyctl deploy model` |
| Hosts downloads may come from | `model_sources.allowed_hosts` | (none) | (none) | any | host, standalone |

`deploy model` pins an `hf: owner/repo` reference without a commit to the
commit it names now. The endpoint it asks follows the same rule; its YAML layer
is the host or standalone document named by `--config` or `CAPYCTL_CONFIG`, else
the standalone document under the state root. A loopback `http://` mirror is
accepted there; a host downloads over `https://` only. `allowed_hosts` is a
list, which has no flag or variable form: state it in the document.

## Engine installation

These describe the role's own engine installation. One executable is published
as the runtime profile `local`; both as `local-vllm` and `local-sglang`. On a
host the profile is added to the document the host publishes, beside the
profiles in `runtime_profiles` and the ones registered with `capyctl engine add`;
a name stated twice is refused. The switches below apply to the `local`
profiles only: a declared or registered profile states its own `security`
block.

| Setting | YAML | Flag | Variable | Default | Roles |
|---|---|---|---|---|---|
| vLLM executable | `local_engine.vllm` | `--vllm-bin <path>` | `CAPYCTL_VLLM_BIN` | none | host, standalone |
| SGLang interpreter | `local_engine.sglang` | `--sglang-bin <path>` | `CAPYCTL_SGLANG_BIN` | none | host, standalone |
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
arguments, so `args` applies to the vLLM profile only. Name `runtime_dir` only
to run from a directory you maintain yourself; CapyCTL never writes to it.

A running host or standalone takes its engine and model settings at start. A
live `capyctl engine add` or `remove` changes runtime profiles only; any other
change needs a restart.

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
| Idle, heartbeat, switching and shutdown bounds | server: `lifecycle_defaults`, `control`, `switching.drain_timeout`; every role: `shutdown.drain_timeout` |
| Response timing header | server: `observability.timing_header` (standalone: `server.observability`) |
| Private ingress | host: `ingress` |
| Memory domains, devices, limits, queues, labels | host: `resource_policy` (standalone derives its own: its document accepts only `auto` values there, and `endpoint_port_range`) |
| Runtime profiles | host: `runtime_profiles`; or `capyctl engine add` (its own flags: `--name`, `--deep-park`, `--drift`, `--arg`) |
| Load report period | host: `load_report_interval` |

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

# Standalone: the switch drain bound and the response timing header.
capyctl start standalone --set server.switching.drain_timeout=45s \
  --set server.observability.timing_header=true
```

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
host.model_store.path                              /home/me/models                     default
host.name                                          local                               yaml
host.resource_policy.allowed_devices               auto                                yaml
host.resource_policy.endpoint_port_range.end       8199                                default
host.resource_policy.endpoint_port_range.start     8100                                default
host.resource_policy.memory.accounting             auto                                yaml
host.resource_policy.memory.system.free_reserve    auto                                yaml
host.resource_policy.memory.system.managed_limit   auto                                yaml
host.state_dir                                     /home/me/.local/state/capyctl/host     yaml
name                                               local                               yaml
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
`--activate`, `--evict`, `--initialize-timeout` and the other per-command
options change one command's behaviour, not the role's configuration, so they
have no variable or YAML form. `--debug-engine-logs` on `start host` and
`start standalone` is a flag only on purpose: full engine logs may contain
secrets, so a variable left in a shell or unit file must not turn them on.

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

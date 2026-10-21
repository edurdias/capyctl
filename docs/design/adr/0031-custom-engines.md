# ADR 0031 — Custom engines: one-off engines from a host-approved manifest

**Status:** Accepted (owner decision 2026-10-20). Proposed 2026-10-18; the owner answered
every open question on 2026-10-20, and the answers are recorded below (summary at the
end). Implemented in 0.1.6, with amendment A1 (owner decision 2026-10-21): custom engine
profiles live in `custom-engines.yaml`, beside `engines.yaml`.
**Amends:** `SPEC.md` §8.2 (a custom engine is a custom/fixed recipe in §8.1's sense:
CapyCTL does not validate its flags), §9.4 (later backends), §10 (when a cancelling lease
settles on an engine with no idle signal), §15.1 (amendment A1: the second engines file),
§20 (T44); ADR 0018 §1 (a third installation path beside the dist-info environment and
the bare binary).
**Related:** ADR 0008 (installations, fingerprints, drift; its runtime type `command` is a
different thing, see §1), ADR 0012 (deep parking and its protections), ADR 0014 (§5 memory
request, A2 measured startup peak), ADR 0017 (capability gating), ADR 0018 (registration,
`engines.yaml`, live reload), ADR 0019 (network endpoint, loopback engine listeners), ADR
0023 (TensorFold: restart-only, `resources` required), ADR 0024 (parsers), ADR 0028 §2.1
(engine environment rules), ADR 0029 (llama.cpp: bare binary, no engine key), ADR 0030
(decision models).

## Context

CapyCTL knows four engine families. Each one took an ADR, a design, a plan and a set of
adapter, rendering and readiness code, because CapyCTL owns that engine's flags: it
renders the listener, the device, the memory settings and the parsers, refuses reserved
options in every spelling, and reads the engine's own gauges.

Some engines will never justify that. The first case is basal (Apache-2.0,
github.com/rkinas/basal; v2.1.0 read on 2026-10-18, v2.2.0 of 2026-10-19 checked on
2026-10-20 and unchanged in every point below): `basal-serve` serves the basal-2.0 and
basal-omni-1.0 decision models on `POST /v1/systemone`, the API ADR 0030 added. The
basal-omni models (Gemma 4 based) run only in basal's own `--mode eager` (or its `vllm`
mode for one FP8 checkpoint); neither llama.cpp nor SGLang serves them. What its server
(`basal/server.py`) and documentation (`docs/SERVING.md`, `docs/API.md`, `docs/OMNI.md`)
say:

- Routes: `POST /v1/systemone`, `POST /v1/basal` (the same handler), `GET /v1/models`
  (`{"models": [{"name", ...}]}`, not the OpenAI shape) and `GET /health`, which always
  answers `{"status": "ok"}`. There is no `/metrics` and no running or queued count on
  any route; the request queue is internal.
- The model loads before the HTTP server binds, so the port opens only once the model is
  loaded.
- `--host` defaults to `0.0.0.0`; `--port` to 8000. There is no API key; the
  documentation says to bind `--host 127.0.0.1` for local use.
- `--model` is a local directory or a Hugging Face repository id; a path that exists is
  used as is. The directory must hold the repository's `basal.json` and calibration
  files; a missing calibration file is refused at start.
- Every exception in the request handler, a malformed body and an engine failure alike,
  answers HTTP 422 `{"error": "..."}`. A request whose client disconnects is not
  cancelled; it is answered into a closed connection.
- `--log-decisions <file>` appends every request's probabilities to a file; it is off by
  default. `--mcp on` adds an unauthenticated `/mcp` route on the same port.
- `BASAL_DEVICE` selects the device (else CUDA, else Apple Silicon, else CPU).
- There is no `--version`; the version is the package's.
- basal-omni-1.0-max needs about 24 GB of GPU memory in bf16, basal-omni-1.0-mini about
  16 GB (`docs/OMNI.md`).

The 422 for every failure and the missing in-flight counts are filed upstream as
rkinas/basal#22 (2026-10-20); this ADR does not depend on its outcome.

A dedicated `basal` engine family would cost what a full engine costs and would track a
fast-moving project's flags. The owner asked instead for a generic engine kind: the host
administrator states how to start the program, and CapyCTL keeps what it owns for every
engine.

## Decision

### 1. Engine kind and registration

`custom` joins `vllm`, `sglang`, `tensorfold` and `llamacpp` in every closed engine set.
A profile of this kind is "a custom engine". It is not a custom build: a custom build
(the guide's "Custom builds") is another version of a known engine, which CapyCTL still
renders and validates as that engine, while a custom engine is a program CapyCTL has no
integration for. Nor is it ADR 0008's runtime type `command`, which says how any
installation is started; a custom engine is started as a command too.

A custom engine is registered only with

```
capyctl engine add --custom <executable> --name <profile> --manifest <file>
    [--env K=V]... [--approve-env GLOB]... [--drift warn|refuse] [--yes]
```

- `--name` is required; there is no default profile name. `profile_exists` applies as
  for every profile.
- `--custom` and the positional installation path are exclusive. `--arg`,
  `--approve-option`, `--approve-path` and `--allow-remote-code` are refused with
  `--custom` (they configure flags CapyCTL renders for known engines);
  `--deep-park enabled` is refused `capability_missing` (§4).
- `<executable>` is resolved lexically, as ADR 0018 §1 resolves an installation, and
  must be an executable regular file (`engine_not_found` otherwise).
- `engine add` executes nothing. Custom engines have no common version flag, and running
  an unknown program to ask is not safe, so the version is the manifest's `version`
  string, self-declared, or `unversioned`. No custom engine is ever in the verified set:
  `engine list` shows each one with `CUSTOM yes` (ADR 0017).
- `engine detect` never lists custom engines; there is nothing to detect.
- The manifest is read once, validated (§2), and copied into the profile in
  `custom-engines.yaml` (amendment A1), with its digest. Editing the file later changes
  nothing; changing the command means `engine remove` and `engine add` again, which ADR
  0018 §4 already makes safe for running deployments.
- A profile of kind `custom` may also be written by hand under `runtime_profiles` in the
  role document, in the same shape, and is validated the same way at role start. There
  is no `local_engine.custom` and no start flag or variable for one: the three-ways rule
  (ADR 0018 A1) covers the role's own installation, which stays one of the known engines.
- A release before 0.1.6 does not read `custom-engines.yaml`: a downgrade loses the
  custom engines and keeps every other engine (amendment A1).

`--env` and `--approve-env` are the ADR 0028 §2.1 settings, with their environment
variables (`CAPYCTL_ENGINE_ADD_ENV`, `CAPYCTL_APPROVE_ENV`). `--drift` writes
`security.installation_drift` (§3).

### 2. The manifest

A YAML document, at most 64 KiB, closed (an unknown field is refused):

```yaml
kind: custom_engine
schema_version: 1
api: decisions               # chat | decisions; required
version: "2.2.0"             # optional; shown as the profile's build
environment: /home/u/basal-2.2.0-venv   # optional; a narrower or other root to pin (§3)
args: [--mode, eager, --host, "{host}", --port, "{port}", --model, "{model_dir}"]
env: {}                      # optional; values may use placeholders
health: /health              # optional; polled before the readiness probe (§5)
idle:                        # optional; the idle signal (§6)
  url: /metrics
  gauges: [requests_running, requests_waiting]
```

**Arguments.** `args` is the whole argument vector after the executable. It is executed
directly, never through a shell (SPEC §8.1): a placeholder is the only substitution, and
its value is one argument or a part of one, never split. Placeholders, in `args` and in
`env` values:

| Placeholder | Value | Required |
|---|---|---|
| `{host}` | `127.0.0.1`, always | yes |
| `{port}` | the instance's leased loopback port (`resource_policy.endpoint_port_range`) | yes |
| `{model_dir}` | the absolute path of the deployment's checkpoint directory (a local model, or a materialized `hf:` source, ADR 0008) | yes |
| `{served_name}` | the name CapyCTL forwards as `model` | no |
| `{context_length}` | the deployment's `engine_config.context_length` | no |
| `{api_key}` | the launch's per-launch engine key (§7) | no |

`{{` and `}}` are literal braces. An unknown placeholder, a required one missing, a
placeholder in `health` or `idle`, or `{api_key}` outside `args` and `env` values is
refused `custom_manifest_invalid`. A deployment whose engine uses `{context_length}` and
states no `context_length` is refused at resolution
`custom_placeholder_unset:context_length`.

**Environment.** The manifest's `env` is host-authored like a profile's `env` and needs
no approval, but ADR 0028 §2.1 rule 1 applies: a CapyCTL-owned name (`PATH`, `LD_*`,
`CUDA_VISIBLE_DEVICES`, `CUDA_HOME`, `CAPYCTL_*`, `HF_HUB_OFFLINE`,
`TRANSFORMERS_OFFLINE`, every name an adapter pins, and the rendezvous names) is refused
`engine_env_reserved:<name>`, at `engine add` and again at launch. A deployment's
`engine_config.env` needs a profile `approved_env` entry, as for every engine (rules 2–8
unchanged). The launch environment is closed (SPEC §13.3): `PATH` is the executable's
directory then the fixed system directories, `HOME` and `CUDA_VISIBLE_DEVICES` come
from CapyCTL, `HF_HUB_OFFLINE=1` and `TRANSFORMERS_OFFLINE=1` are set (CapyCTL's model
store fetches models; the engine does not), then the manifest's, the profile's and the
deployment's names, in that order of precedence from lowest to highest.

**API.** `api` is what every deployment on the profile serves. A deployment's `api` must
equal it: `api: decisions` on a chat engine is refused `capability_missing:decisions`,
and `chat` (or no `api`) on a decisions engine `capability_missing:chat`. The host
advertises `decisions` (ADR 0030 §1) as before; a decisions custom engine needs both
capabilities.

### 3. Identity, fingerprint and drift

The installation is the executable plus its environment. By default the environment is
the whole one the executable runs in: for an executable in a virtual environment's
`bin/` (the directory above it holds `pyvenv.cfg`), every regular file of that
environment, its `site-packages` included, and the interpreter its `bin/python3` runs,
followed to its file (a virtual environment usually links it from outside the tree);
otherwise the executable alone. A Python console script such as `basal-serve` is a few
hundred bytes that import the environment, so its digest alone would pin nothing. The
manifest's `environment` names another root instead, narrower (a package directory) or
outside the default (a prefix the executable loads from); it must be an absolute
directory, and it replaces the default root, not the executable or the interpreter.

The digest is ADR 0008's canonical manifest (relative path, size, SHA-256, sorted;
bytecode caches left out), with the installation bounds (100,000 files, 16 GiB, depth
32); beyond them the installation is `unmeasured`, never refused. The first measurement
reads the whole environment (several GB with torch); later ones re-read only changed
files. The manifest's own digest is part of the profile and of the recipe fingerprint.
Every launch measures again: a different digest is drift, flagged in the host's status
and journaled, and it refuses the launch (`installation_drift`) only when the profile
says `security.installation_drift: refuse` (`--drift refuse`); the default is `warn`.
`build_fingerprint` is the manifest's `version`, else `unversioned`.

### 4. What CapyCTL keeps and what it refuses

CapyCTL keeps, unchanged, for a custom engine:

- **Process ownership.** Start in its own process group as the role's service user, the
  launch journal, stop by SIGTERM to the group, verified exit, then release; an engine
  that exits is a failed instance. A command that daemonizes and exits leaves no owned
  handle and fails as SPEC §8.1 and T13 require.
- **Listener and port.** The loopback port lease and `{host}` (§7).
- **Routing, authentication and admission.** Lookup by `model`, the API key at the
  router and the host ingress, waiting, activation, the in-flight bound, request leases
  and fairness (SPEC §10). The request contract is the endpoint's: chat completions, or
  ADR 0030's closed `/v1/systemone` contract (`noul`, `choice` and `score` questions
  about a text, object or message state), unchanged for custom engines.
- **Readiness** by the existing probes (§5).
- **Memory accounting.** The deployment states `resources` (the short form or the
  phases); there is no derivation. The host observes the process group's memory, and
  status shows the measured startup peak (`startup.measured`), as for every engine
  (ADR 0014 A2). CapyCTL caps nothing inside the engine: as with TensorFold before its
  cap, an engine that outgrows its reservation is seen, not prevented.
- **Logs.** Standard output and error go to the launch's private, size-bounded log
  through the redacting writer (SPEC §13.3), with the management log tail.

CapyCTL refuses for a custom engine:

- **Parking.** Residency resolves `restart_only`; `deep` or `host_backed` is refused
  `capability_missing:deep_park`, and `capyctl park deployment` is refused as
  unsupported. The profile is written `security.deep_park: disabled`.
- **Groups.** A `topology` is refused `group_shape_unsupported:custom`. Several
  instances, each its own process on its own port, are allowed.
- **Resource sizing.** `resources` is required, refused as a missing required field as
  for TensorFold (ADR 0023); `engine_config.memory` is refused.
- **Engine-flag validation.** CapyCTL does not know the program's options, so it cannot
  refuse a conflicting or reserved one, read a hidden configuration file, or verify
  rendered values after the engine's parser (SPEC §8.2). A custom engine is therefore a
  custom/fixed recipe (SPEC §8.1): the manifest is the whole command, and a deployment
  cannot add to it. `accept_extra_args`, `extra_args` and every typed engine field
  (`dtype`, `quantization`, `kv_cache_dtype`, `cuda_graphs`, `trust_remote_code`,
  `max_concurrent_requests` as an engine setting, ...) are refused
  `custom_field_unsupported:<field>`. `engine_config` accepts `context_length` (used
  only through its placeholder) and `env`.
- **Parsers.** `tool_call_parser` and `reasoning_parser` are refused
  (`custom_field_unsupported:<field>`); ADR 0024's family detection does not apply. A
  chat custom engine that parses tool calls does it with its own options in the
  manifest.
- **`cache_salt`.** Refused `cache_salt_unsupported` (SPEC §10): CapyCTL cannot know the
  engine honours it.
- **Engine figures.** No per-request engine figures (`prefill_ms`, decode speed, cached
  tokens) and no load reports; `ttft_ms` is the router's, the engine's `usage` passes
  through, and instance selection uses the router's own in-flight counts.

### 5. Readiness

Ready is, in order: the port accepts a connection; the manifest's `health` path, when
stated, answers 200; then the existing probe of the profile's API, unchanged: the bounded
chat probe for `chat`, the fixed decision of ADR 0030 §3 for `decisions`, with the
served name as `model` and the engine key when the manifest uses `{api_key}`. The wake
check after a restart is the API's (ADR 0030 §4 for decisions). `/v1/models` is not read:
its shape varies (basal's is not OpenAI's), and SPEC §6.1 makes the probe, not a listing,
the readiness evidence. Undeclared `timeouts.initialize` follows ADR 0014 A1.

After the probe passes and before admission opens, the host checks the listener (§7).

### 6. Request leases with and without an idle signal

SPEC §10 keeps a request `cancelling` (charged) after a hang-up, a deadline cut or an
engine error answer until the engine reports quiescence: no running and no waiting
requests, read after the hang-up. That report needs an engine signal.

**With an idle signal.** `idle.url` is a path on the engine's own loopback port, read
with the engine key when there is one. With `gauges`, the body is Prometheus text and
each named gauge must be present and 0 (summed over its labels); with `json` (a list of
field paths instead of `gauges`), the body is a JSON object and each named field must be
present and 0 or `false`. Anything missing, malformed, unreachable or non-200 is unknown,
never idle. The signal is the cancellation acknowledgement of SPEC §10, and the read
before the stop signal (ADR 0029 §10, ADR 0023 §6): idle, exited or not listening is
signalled at once; unanswered is signalled when the bound ends; busy at the bound is not
signalled and the cleanup stays uncertain.

**Without one** (owner decision 2026-10-20, as proposed). Nothing can show that the
engine stopped working on a cancelled request, and with basal it has not: a decision
whose client left is still computed. So:

1. A cancelling lease on such an instance, whether from a hang-up, a deadline cut or an
   engine error answer, is never settled while the engine runs. It closes only on
   controlled cleanup (SPEC §10): the verified exit of the instance's process group at
   the next stop, idle stop, switch release or restart, with
   `request_cancellation_acknowledged` journaled with a receipt naming that stop.
2. Draining does not wait for it. A stop, idle stop or switch drain waits for leases
   still being answered, as today, but not for cancelling ones: waiting could only end
   at the drain timeout, and with the existing rules every switch away from the instance
   would fail and every stop would wait its full bound after one hang-up. The release
   that follows is a stop (the engine cannot park); its verified exit settles the
   cancelling leases in the same transaction that records it. Memory is never freed
   before the verified exit: a stop that cannot prove the exit keeps the leases charged
   and the reservation held, as any uncertain cleanup.
3. The router releases the request's in-flight slot as it does today for a cancelling
   request, so admission is not starved; only the ledger keeps the charge.

The SPEC §10 amendment this makes, to land with the implementation: "On an engine that
reports no idle signal (a custom engine without `idle`, ADR 0031 §6), a `cancelling`
request settles only on the verified exit of its instance at the next stop or restart,
and park, idle park and switch drains do not wait for it; its charge and the instance's
reservation are kept until that exit is verified." A cancelled request has no client
left to answer, so the rule against killing live requests is not touched.

**Status.** `capyctl status deployment <name>` shows each instance's cancelling count
and when it settles: `cancelling: 2 (settles at the next stop or restart: the engine
reports no idle signal)`, or `(settles on the engine's idle signal)`; the JSON view
gains `requests.cancelling` and `requests.cancelling_settles` (`idle_signal` or `stop`).
`capyctl engine list` shows each custom engine's idle signal or `none`. Uncertain leases
are unchanged: they settle only on cleanup evidence for every engine.

### 7. Listener, engine key and the loopback check

`{host}` is always `127.0.0.1` and the port is a private loopback lease; the routed path
is the only network path to the engine, as for TensorFold and llama.cpp (ADR 0019, ADR
0029 §4). A custom engine cannot park, so ADR 0012's protections are not required.

**No `{api_key}`.** The engine listens unauthenticated on loopback: any local process can
reach its port. This is the known limitation of llama.cpp and TensorFold, shown in
`engine list` (`engine key: none`) and in the guide.

**With `{api_key}`.** CapyCTL generates a key per launch, renders it, and sends
`Authorization: Bearer <key>` on every forwarded request, probe and idle read; the log
writer redacts it by value. A key in `args` is visible to every local user in
`/proc/<pid>/cmdline`; `engine add` warns when the manifest puts it there and the guide
recommends an `env` value (`env: {SOME_KEY: "{api_key}"}`) where the engine reads one.

**Loopback check** (kept, owner decision 2026-10-20). Because CapyCTL cannot validate the
engine's options, it verifies the one that matters most after the fact: once the probe
passes, the host lists the TCP listening sockets held by the launch's owned processes
(their socket inodes in `/proc/<pid>/fd` against `/proc/net/tcp` and `tcp6`). Any
listener on an address other than `127.0.0.1` or `::1` stops the launch, with gone
evidence required, and fails it `custom_listener_exposed`, naming the address and port.
The check runs again whenever a host re-proves the launch. A listener opened before
readiness and closed again, or opened later, is not seen; this narrows the exposure, it
does not prove it absent.

### 8. Security of arbitrary commands

A custom engine runs whatever program the host administrator names, with arguments they
wrote. That is SPEC §8.1's "administrator-authored scripts are trusted execution, not a
sandbox", made explicit:

- **Approval is `engine add`.** Only the local CLI on the host writes the engines files
  (`custom-engines.yaml` for custom engines, amendment A1), as the role's service user
  or root (ADR 0018 §2); no management or remote API creates or
  edits a custom engine, so a server operator or deployment owner cannot introduce a
  program (SPEC §13.3). `engine add --custom` prints the executable, the argument
  template, the API, the environment names, the root pinned and its digest, and asks
  for confirmation on a terminal (kept, owner decision 2026-10-20); without a terminal
  it needs `--yes`, else `not_interactive`. No other `engine add` asks.
- **Deployment operators only select it** and fill the placeholders from their
  deployment: the model (a CapyCTL-materialized or approved local path), the served name
  and the context length. They cannot add arguments (§4), and their environment
  variables need the profile's `approved_env` (ADR 0028 §2.1).
- **The server sees what runs.** The published profile carries the manifest, so `capyctl
  list engines` on the server and `inspect deployment --effective-config` show the
  executable, the rendered arguments with the key redacted, and the environment names
  with their sources.
- **What CapyCTL cannot promise.** Request logging off (SPEC §13.3) and no model fetches
  are promised for the known engines because CapyCTL controls their options. For a
  custom engine they hold only if the manifest keeps them: basal's `--log-decisions`
  would write every answer to a file. The guide says so; the engine log is still
  redacted.

### 9. Host capability and gating

A host that implements this ADR declares `custom_engines` (ADR 0017). The server places a
deployment whose profile is of kind `custom` only on a host that declared it; any other
is refused `host_capability_missing:custom_engines` before anything is sent. A host on an
older release never publishes a `custom` profile (it does not read `custom-engines.yaml`,
amendment A1), and a server is never older than its hosts (ADR 0017), so publication
needs no gate.

### 10. Refusal codes

| Where | Code | When |
|---|---|---|
| `engine add` | `engine_not_found` (exit 16) | the executable is missing or not an executable regular file |
| `engine add` | `custom_manifest_invalid` (new, exit 27) | the manifest is unreadable, too large, not closed, or has an unknown or missing required placeholder, a placeholder where none is allowed, an `idle` or `health` value that is not a loopback path, or an `environment` that is not an absolute directory |
| `engine add` | `engine_env_reserved:<name>` | the manifest's or profile's `env` names a CapyCTL-owned variable |
| `engine add` | `capability_missing` | `--deep-park enabled` |
| `engine add` | `profile_exists` (19), `not_interactive` (23) | as ADR 0018; `not_interactive` also without a terminal and without `--yes` |
| resolution | `capability_missing:deep_park` | `residency: deep` or `host_backed` |
| resolution | `capability_missing:decisions`, `capability_missing:chat` | the deployment's `api` differs from the manifest's |
| resolution | missing required field `resources` (as TensorFold) | no `resources` |
| resolution | `group_shape_unsupported:custom` | a `topology` |
| resolution | `custom_field_unsupported:<field>` | `extra_args`, `accept_extra_args`, a parser, `memory` or a typed engine field |
| resolution | `custom_placeholder_unset:context_length` | the template uses `{context_length}` and the deployment states none |
| placement | `host_capability_missing:custom_engines` | the host did not declare the capability |
| request | `cache_salt_unsupported` | a chat request with `cache_salt` |
| launch | `custom_listener_exposed` | an owned process listens off loopback (§7) |
| launch | `installation_drift`, `engine_env_reserved:<name>` | as ADR 0008 and ADR 0028 |

### 11. Worked example: basal-omni on `basal-serve` 2.2.0

The manifest, `basal-serve.yaml`:

```yaml
kind: custom_engine
schema_version: 1
api: decisions
version: "2.2.0"
args: [--mode, eager, --host, "{host}", --port, "{port}", --model, "{model_dir}"]
health: /health
```

Registration, on the machine that runs it:

```
capyctl engine add --custom ~/basal-2.2.0-venv/bin/basal-serve --name basal-serve \
    --manifest basal-serve.yaml
```

The root pinned is `~/basal-2.2.0-venv` (it holds `pyvenv.cfg`), with its interpreter.
There is no `{api_key}`: basal has no key option, so the engine listens unauthenticated
on loopback (§7). There is no `idle`: basal exposes no running or queued count
(`/health` is a constant), so a cancelled decision settles only at the next stop or
restart (§6), and status says so. `--media on` is left out: basal reads images and audio
from a `media` field, which the closed `/v1/systemone` contract refuses (§12).

A deployment:

```yaml
name: basal-omni-max
engine: basal-serve
model: {hf: Remek/basal-omni-1.0-max@<commit>}
api: decisions
resources: {gpu: 32GiB, ram: 8GiB}
```

`resources` is a placeholder above basal's stated 24 GB until the first start's
measured peak replaces it. The launch runs, with the closed environment,

```
~/basal-2.2.0-venv/bin/basal-serve --mode eager --host 127.0.0.1 --port <leased> \
    --model ~/models/sources/huggingface/Remek--basal-omni-1.0-max@<commit>
```

The port opens once the model is loaded; `/health` answers 200; the fixed decision of
ADR 0030 §3 is answered with calibrated probabilities summing to 1; the loopback check
finds one listener on `127.0.0.1`; admission opens.

**basal's 422** (accepted as is, owner decision 2026-10-20). basal answers every failure
with 422 and a JSON body, so CapyCTL relays it as `engine_rejected` and settles the
lease as answered. That is right for accounting, since the engine did answer, but a
basal 422 may mean an engine failure (out of memory, an internal error) and not a bad
request. The guide says so and points to the instance's engine log (the management
engine-log tail, SPEC §13.3) to tell them apart. rkinas/basal#22 asks
upstream for 5xx on internal failures and for in-flight counts; a release that adds
counts can be given an `idle` entry in its manifest without a CapyCTL change.

### 12. Out of scope

- basal's request extensions: `media` (images and audio), `lang`, `detail`, `facts`,
  `think`, the `multi`, `act` and `tool` question types, `max_error` and the task types.
  `/v1/systemone` stays ADR 0030's closed contract (`noul`, `choice`, `score`, text
  states) for every engine (owner decision 2026-10-20); the extensions need their own
  ADR.
- Parking, groups, derived sizing and any validation of the program's own options (§4).

## Consequences

- A fifth engine kind in every closed set, a third installation path in `engine add`, a
  manifest parser and placeholder renderer in `capyctl-config`, a generic adapter (port,
  optional health, the API probes, the optional idle reader, the listener check) and the
  `custom_engines` capability.
- A host administrator can run an engine CapyCTL does not know without waiting for an
  integration, and gets ownership, routing, admission, accounting and logs. In exchange
  CapyCTL validates nothing inside the command; the manifest's author owns its
  correctness (ADR 0011).
- Without an idle signal, every hang-up, deadline cut or engine error answer leaves a
  cancelling lease that settles only at the next verified stop or restart, and status
  says so. Drains on such instances no longer wait for cancelling leases (the SPEC §10
  amendment of §6).
- New guide sections: adding a custom engine and how it differs from a custom build, the
  manifest, the basal example with its 422 caveat, and the limits of §4 and §8;
  `errors.md` gains the codes of §10.

## Alternatives considered

- **A `basal` engine family.** Rejected for now: a full integration (flag ownership,
  reserved options, readiness, gauges) for a project whose flags change every release,
  and the next one-off engine would need the same again. A family can still follow if
  basal becomes a supported engine.
- **Run basal through llama.cpp or SGLang.** Not possible for basal-omni: basal's GGUF
  and SGLang paths cover basal-2.0 only, and its own `llamacpp` and `sglang` modes put a
  basal server in front of the engine, which is the same unknown command.
- **The name `command`.** Rejected by the owner (2026-10-20): it collides with ADR
  0008's runtime type `command`, which every installation already has.
- **Validate flags by asking the program (`--help`).** Rejected: not every program has
  one, its output is not a contract, and `engine add` would execute an unknown program.
- **Treat a cancelling lease without an idle signal as settled at once.** Rejected by
  SPEC §10: a closed connection is not proof the engine stopped, and basal keeps
  computing.
- **Keep draining for cancelling leases.** Rejected (owner decision 2026-10-20): one
  hang-up would fail every switch away from the instance until an operator stops it.
- **A warning instead of the loopback check, or no confirmation at `engine add`.**
  Rejected (owner decision 2026-10-20): both safeguards stay.
- **Pin only the executable.** Rejected: a console script pins nothing; the whole
  environment is the default and the manifest may name a narrower root.
- **Deployment-level arguments with an approval list.** Rejected: without knowing the
  program's options CapyCTL cannot tell an ordinary argument from one that moves the
  listener or loads code, so the only safe author is the host administrator.

## Verification

A new conformance row, **T44 — custom engines**: `engine add --custom` executes nothing,
requires `--name`, refuses an unknown or missing required placeholder, a reserved
environment name and `--deep-park enabled`, asks for confirmation (or `--yes`, else
`not_interactive`) and writes the manifest into `custom-engines.yaml` with its digest
(amendment A1); the
digest covers the executable, the whole environment with its interpreter, or the
manifest's `environment` root, and drift is flagged and refused only under
`installation_drift: refuse`; rendering substitutes each placeholder into one argument,
never through a shell, with `{host}` 127.0.0.1 and the leased port; a deployment is
refused with a parking residency, a `topology`, no `resources`, `extra_args`, a parser,
a typed engine field, a mismatched `api`, or an unset `{context_length}`; placement
refuses a host without `custom_engines`; readiness passes only on the API's probe (chat
and decisions); an owned listener off loopback stops the launch with
`custom_listener_exposed`; with an idle signal a cancelling lease settles on idle
readings after the hang-up and never on missing or malformed ones; without one, a lease
cancelled by a hang-up, a deadline cut or an engine error stays charged while the engine
runs, drains do not wait for it, and it settles on the stop's verified exit and not
before, with the reservation held until then; status shows the cancelling count and how
it settles; `cache_salt` is refused; `engines.yaml` refuses `engine: custom`, a profile
name is unique across both engines files and the role document, and a release before
0.1.6 ignores `custom-engines.yaml` (amendment A1). Tests
also tag T13, T14, T17, T37 and T43 where they apply.

CPU and Fake-engine tests (the fake engine gains a custom mode with and without an idle
signal) are not qualification. Live rows CE1–CE4 qualify it, on host A with basal 2.2.0
in `~/basal-2.2.0-venv` (approved 2026-10-20, listed in AGENTS.md) and basal-omni-1.0:
CE1 registration, confirmation and drift; CE2 decisions readiness and the loopback
check; CE3 a hang-up whose lease stays charged while the engine runs and settles at the
verified stop; CE4 restart and the wake check.

## Owner decisions of 2026-10-20

The open questions of the proposal, answered:

1. **Name:** the kind is `custom` (`engine add --custom`, "a custom engine"), not
   `command` (§1).
2. **Drain without an idle signal:** as proposed (§6), with the SPEC §10 amendment.
3. **Safeguards:** both kept: the loopback check (§7) and confirmation at `engine add`
   (§8).
4. **`/v1/systemone`:** stays closed; basal's extensions need their own ADR (§12).
5. **basal's 422 and missing counts:** accepted as is and documented (§11); upstream
   issue rkinas/basal#22. basal v2.2.0 (2026-10-19) checked: unchanged.
6. **Qualification:** CE1–CE4 on host A with `~/basal-2.2.0-venv` (approved).
7. **Release and digest:** release 0.1.6; the digest covers the whole environment by
   default, the manifest may name a narrower root, and drift warns unless
   `installation_drift: refuse` (§3).

## Amendment A1 (owner decision 2026-10-21)

**Custom engine profiles live in `custom-engines.yaml`.** Found while implementing: 0.1.5
reads `engines.yaml` strictly against a closed profile shape, so a custom profile there,
with its copied manifest, would make 0.1.5 refuse the whole file after a downgrade, and
every registered engine with it, instead of skipping one profile as §1 first said. So:

- `capyctl engine add --custom` writes the profile to `custom-engines.yaml`, beside
  `engines.yaml` (the same directory, found the same way from `--config`,
  `CAPYCTL_CONFIG` or `XDG_CONFIG_HOME`), with the same first-line revision header
  (`# capyctl-document-revision: N`), lock and atomic write; its document kind is
  `custom_engines`. `engine remove` removes a custom engine from it.
- `engines.yaml` keeps 0.1.5's profile shape and refuses `engine: custom`;
  `custom-engines.yaml` refuses any other engine. A profile written by hand under
  `runtime_profiles` in the role document is unchanged (§1).
- A profile name is unique across both files and the role document: a name taken in any
  of them is `profile_exists`. Both files count together toward the size bound of the
  profiles a host publishes.
- A downgrade to a release before 0.1.6 ignores `custom-engines.yaml` and loses the
  custom engines (a deployment naming one finds no host that publishes it), while every
  other engine keeps working. This replaces §1's sentence that a release predating this
  ADR skips a `custom` profile in `engines.yaml` with a warning; §9 holds unchanged, since
  an older release never publishes a custom profile.
- For a deployment with `api: decisions`, ADR 0030's document check refuses a `topology`
  or a parking residency (`deep`, `host_backed`) before the profile is known, so on a
  decisions custom engine those two refusals carry ADR 0030's wording (an invalid
  `topology` or `residency`, "a decisions deployment runs on one machine" and "restarts
  instead of parking"). A chat deployment on a custom engine gets this ADR's codes,
  `group_shape_unsupported:custom` and `capability_missing:deep_park` (§4, §10). Either
  way nothing is stored or started.

This amends SPEC §15.1 (the engines file) and T44, and replaces the `engines.yaml`
references of §1, §8 and the Verification above.

# ADR 0031 — Command engines: one-off engines from a host-approved command manifest

**Status:** Proposed (2026-10-18). The owner approved writing it on 2026-10-18; the
decisions below are recommendations, and the open questions at the end need answers
before it is accepted. Nothing is implemented.
**Amends (once accepted):** `SPEC.md` §8.2 (a command engine is a custom/fixed recipe in
§8.1's sense: CapyCTL does not validate its flags), §9.4 (later backends), §10 (when a
cancelling lease settles on an engine with no idle signal), §20 (T44); ADR 0018 §1 (a
third installation path beside the dist-info environment and the bare binary).
**Related:** ADR 0008 (installations, fingerprints, drift), ADR 0012 (deep parking and its
protections), ADR 0014 (§5 memory request, A2 measured startup peak), ADR 0017 (capability
gating), ADR 0018 (registration, `engines.yaml`, live reload), ADR 0019 (network
endpoint, loopback engine listeners), ADR 0023 (TensorFold: restart-only, `resources`
required), ADR 0024 (parsers), ADR 0028 §2.1 (engine environment rules), ADR 0029
(llama.cpp: bare binary, no engine key), ADR 0030 (decision models).

## Context

CapyCTL knows four engine families. Each one took an ADR, a design, a plan and a set of
adapter, rendering and readiness code, because CapyCTL owns that engine's flags: it
renders the listener, the device, the memory settings and the parsers, refuses reserved
options in every spelling, and reads the engine's own gauges.

Some engines will never justify that. The first case is basal (Apache-2.0,
github.com/rkinas/basal, v2.1.0 read on 2026-10-18): `basal-serve` serves the basal-2.0
and basal-omni-1.0 decision models on `POST /v1/systemone`, the API ADR 0030 added. The
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
- Every exception in the request handler, a malformed body included, answers HTTP 422
  `{"error": "..."}`. A request whose client disconnects is not cancelled; it is answered
  into a closed connection.
- `--log-decisions <file>` appends every request's probabilities to a file; it is off by
  default. `--mcp on` adds an unauthenticated `/mcp` route on the same port.
- `BASAL_DEVICE` selects the device (else CUDA, else Apple Silicon, else CPU).
- There is no `--version`; the version is the package's.
- basal-omni-1.0-max needs about 24 GB of GPU memory in bf16, basal-omni-1.0-mini about
  16 GB (`docs/OMNI.md`).

A dedicated `basal` engine family would cost what a full engine costs and would track a
fast-moving project's flags. The owner asked instead for a generic engine kind: the host
administrator states how to start the program, and CapyCTL keeps what it owns for every
engine.

## Decision

### 1. Engine kind and registration

`command` joins `vllm`, `sglang`, `tensorfold` and `llamacpp` in every closed engine set.
An installation of kind `command` is registered only with

```
capyctl engine add --command <executable> --name <profile> --manifest <file>
    [--env K=V]... [--approve-env GLOB]... [--drift warn|refuse] [--yes]
```

- `--name` is required; there is no default profile name. `profile_exists` applies as
  for every profile.
- `--command` and the positional installation path are exclusive. `--arg`,
  `--approve-option`, `--approve-path` and `--allow-remote-code` are refused with
  `--command` (they configure flags CapyCTL renders for known engines);
  `--deep-park enabled` is refused `capability_missing` (§4).
- `<executable>` is resolved lexically, as ADR 0018 §1 resolves an installation, and
  must be an executable regular file (`engine_not_found` otherwise).
- `engine add` executes nothing. Command engines have no common version flag, and
  running an unknown program to ask is not safe, so the version is the manifest's
  `version` string, self-declared, or `unversioned`. No command engine is ever in the
  verified set: `engine list` shows every one as `custom` (ADR 0017).
- `engine detect` never lists command engines; there is nothing to detect.
- The manifest is read once, validated (§2), and copied into the profile in
  `engines.yaml`, with its digest. Editing the file later changes nothing; changing the
  command means `engine remove` and `engine add` again, which ADR 0018 §4 already makes
  safe for running deployments.
- A profile of kind `command` may also be written by hand under `runtime_profiles` in the
  role document, in the same shape, and is validated the same way at role start. There
  is no `local_engine.command` and no start flag or variable for one: the three-ways rule
  (ADR 0018 A1) covers the role's own installation, which stays one of the known engines.
- A release that predates this ADR skips a `command` profile with a warning at start
  (ADR 0018 A3) and keeps the rest.

`--env` and `--approve-env` are the ADR 0028 §2.1 settings, with their environment
variables (`CAPYCTL_ENGINE_ADD_ENV`, `CAPYCTL_APPROVE_ENV`).

### 2. The manifest

A YAML document, at most 64 KiB, closed (an unknown field is refused):

```yaml
kind: command_engine
schema_version: 1
api: decisions               # chat | decisions; required
version: "2.1.0"             # optional; shown as the profile's build
environment: /home/u/basal-env   # optional; the tree pinned with the executable (§3)
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
refused `command_manifest_invalid`. A deployment whose engine uses `{context_length}`
and states no `context_length` is refused at resolution
`command_placeholder_unset:context_length`.

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
advertises `decisions` (ADR 0030 §1) as before; a decisions command engine needs both
capabilities.

### 3. Identity, fingerprint and drift

The installation is the executable plus its environment tree. The tree is the manifest's
`environment` when stated, else the virtual environment around the executable (the
directory above `bin/` when it holds `pyvenv.cfg`), else nothing: the executable alone.
A Python console script such as `basal-serve` is a few hundred bytes that import the
environment, so its digest alone would pin nothing.

The digest is ADR 0008's canonical manifest (relative path, size, SHA-256, sorted;
bytecode caches left out) over the executable and every regular file of the tree, with
the installation bounds (100,000 files, 16 GiB, depth 32); beyond them the installation
is `unmeasured`, never refused. The manifest's own digest is part of the profile and of
the recipe fingerprint. Every launch measures again (the measurer reuses digests of
unchanged files): a different digest is drift, flagged and journaled, and refuses the
launch only under `--drift refuse` (`installation_drift`). `build_fingerprint` is the
manifest's `version`, else `unversioned`.

### 4. What CapyCTL keeps and what it refuses

CapyCTL keeps, unchanged, for a command engine:

- **Process ownership.** Start in its own process group as the role's service user, the
  launch journal, stop by SIGTERM to the group, verified exit, then release; an engine
  that exits is a failed instance. A command that daemonizes and exits leaves no owned
  handle and fails as SPEC §8.1 and T13 require.
- **Listener and port.** The loopback port lease and `{host}` (§7).
- **Routing, authentication and admission.** Lookup by `model`, the API key at the
  router and the host ingress, waiting, activation, the in-flight bound, request leases
  and fairness (SPEC §10). The request contract is the endpoint's: chat completions, or
  ADR 0030's closed `/v1/systemone` contract.
- **Readiness** by the existing probes (§5).
- **Memory accounting.** The deployment states `resources` (the short form or the
  phases); there is no derivation. The host observes the process group's memory, and
  status shows the measured startup peak (`startup.measured`), as for every engine
  (ADR 0014 A2). CapyCTL caps nothing inside the engine: as with TensorFold before its
  cap, an engine that outgrows its reservation is seen, not prevented.
- **Logs.** Standard output and error go to the launch's private, size-bounded log
  through the redacting writer (SPEC §13.3), with the management log tail.

CapyCTL refuses for a command engine:

- **Parking.** Residency resolves `restart_only`; `deep` or `host_backed` is refused
  `capability_missing:deep_park`, and `capyctl park deployment` is refused as
  unsupported. The profile is written `security.deep_park: disabled`.
- **Groups.** A `topology` is refused `group_shape_unsupported:command`. Several
  instances, each its own process on its own port, are allowed.
- **Resource sizing.** `resources` is required, refused as a missing required field as
  for TensorFold (ADR 0023); `engine_config.memory` is refused.
- **Engine-flag validation.** CapyCTL does not know the program's options, so it cannot
  refuse a conflicting or reserved one, read a hidden configuration file, or verify
  rendered values after the engine's parser (SPEC §8.2). A command engine is therefore a
  custom/fixed recipe (SPEC §8.1): the manifest is the whole command, and a deployment
  cannot add to it. `accept_extra_args`, `extra_args` and every typed engine field
  (`dtype`, `quantization`, `kv_cache_dtype`, `cuda_graphs`, `trust_remote_code`,
  `max_concurrent_requests` as an engine setting, ...) are refused
  `command_field_unsupported:<field>`. `engine_config` accepts `context_length` (used
  only through its placeholder) and `env`.
- **Parsers.** `tool_call_parser` and `reasoning_parser` are refused
  (`command_field_unsupported:<field>`); ADR 0024's family detection does not apply. A
  chat command engine that parses tool calls does it with its own options in the
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
engine 5xx until the engine reports quiescence: no running and no waiting requests, read
after the hang-up. That report needs an engine signal.

**With an idle signal.** `idle.url` is a path on the engine's own loopback port, read
with the engine key when there is one. With `gauges`, the body is Prometheus text and
each named gauge must be present and 0 (summed over its labels); with `json` (a list of
field paths instead of `gauges`), the body is a JSON object and each named field must be
present and 0 or `false`. Anything missing, malformed, unreachable or non-200 is unknown,
never idle. The signal is the cancellation acknowledgement of SPEC §10, and the read
before the stop signal (ADR 0029 §10, ADR 0023 §6): idle, exited or not listening is
signalled at once; unanswered is signalled when the bound ends; busy at the bound is not
signalled and the cleanup stays uncertain.

**Without one.** Nothing can show that the engine stopped working on a cancelled
request, and with basal it has not: a decision whose client left is still computed. So:

1. A cancelling lease on such an instance is never settled while the engine runs. It
   closes only on controlled cleanup (SPEC §10): the verified exit of the instance's
   process group at a stop, an idle stop, a switch's release or a restart, with
   `request_cancellation_acknowledged` journaled with a receipt naming that stop.
2. Draining does not wait for it. A stop, idle stop or switch drain waits for leases
   still being answered, as today, but not for cancelling ones: waiting could only end
   at the drain timeout, and with the existing rules that would make every switch away
   from the instance fail and every stop wait its full bound after one hang-up. The
   release that follows is a stop (the engine cannot park), its verified exit settles the
   cancelling leases in the same transaction that records it, and a stop that cannot
   prove the exit keeps them charged, with the reservation, as any uncertain cleanup.
   This amends SPEC §10's "switch drains wait for `cancelling` requests" for engines
   without an idle signal; a cancelled request has no client left to answer, so the
   rule against killing live requests is not touched.
3. The router releases the request's in-flight slot as it does today for a cancelling
   request, so admission is not starved; only the ledger keeps the charge.

**Status.** `capyctl status deployment <name>` shows each instance's cancelling count
and when it settles: `cancelling: 2 (settles at the next stop or restart: the engine
reports no idle signal)`, or `(settles on the engine's idle signal)`; the JSON view
gains `requests.cancelling` and `requests.cancelling_settles` (`idle_signal` or `stop`).
`capyctl engine list` shows each command engine's idle signal or `none`. Uncertain leases
are unchanged: they settle only on cleanup evidence for every engine.

### 7. Listener, engine key and the loopback check

`{host}` is always `127.0.0.1` and the port is a private loopback lease; the routed path
is the only network path to the engine, as for TensorFold and llama.cpp (ADR 0019, ADR
0029 §4). A command engine cannot park, so ADR 0012's protections are not required.

**No `{api_key}`.** The engine listens unauthenticated on loopback: any local process can
reach its port. This is the known limitation of llama.cpp and TensorFold, shown in
`engine list` (`engine key: none`) and in the guide.

**With `{api_key}`.** CapyCTL generates a key per launch, renders it, and sends
`Authorization: Bearer <key>` on every forwarded request, probe and idle read; the log
writer redacts it by value. A key in `args` is visible to every local user in
`/proc/<pid>/cmdline`; `engine add` warns when the manifest puts it there and the guide
recommends an `env` value (`env: {SOME_KEY: "{api_key}"}`) where the engine reads one.

**Loopback check.** Because CapyCTL cannot validate the engine's options, it verifies the
one that matters most after the fact: once the probe passes, the host lists the TCP
listening sockets held by the launch's owned processes (their socket inodes in
`/proc/<pid>/fd` against `/proc/net/tcp` and `tcp6`). Any listener on an address other
than `127.0.0.1` or `::1` stops the launch, with gone evidence required, and fails it
`command_listener_exposed`, naming the address and port. The check runs again whenever
a host re-proves the launch. A listener opened before readiness and closed again, or
opened later, is not seen; this narrows the exposure, it does not prove it absent.

### 8. Security of arbitrary commands

A command engine runs whatever program the host administrator names, with arguments
they wrote. That is SPEC §8.1's "administrator-authored scripts are trusted execution,
not a sandbox", made explicit:

- **Approval is `engine add`.** Only the local CLI on the host writes `engines.yaml`, as
  the role's service user or root (ADR 0018 §2); no management or remote API creates or
  edits a command engine, so a server operator or deployment owner cannot introduce a
  program (SPEC §13.3). `engine add --command` prints the executable, the argument
  template, the API, the environment names, the tree pinned and its digest, and asks
  for confirmation on a terminal; without a terminal it needs `--yes`, else
  `not_interactive`.
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
  command engine they hold only if the manifest keeps them: basal's `--log-decisions`
  would write every answer to a file. The guide says so; the engine log is still
  redacted.

### 9. Host capability and gating

A host that implements this ADR declares `command_engines` (ADR 0017). The server places
a deployment whose profile is of kind `command` only on a host that declared it; any
other is refused `host_capability_missing:command_engines` before anything is sent. A
host never publishes a `command` profile from an older release (it skips it, §1), and a
server is never older than its hosts (ADR 0017), so publication needs no gate.

### 10. Refusal codes

| Where | Code | When |
|---|---|---|
| `engine add` | `engine_not_found` (exit 16) | the executable is missing or not an executable regular file |
| `engine add` | `command_manifest_invalid` (new, exit 27) | the manifest is unreadable, too large, not closed, or has an unknown or missing required placeholder, a placeholder where none is allowed, or an `idle` or `health` value that is not a loopback path |
| `engine add` | `engine_env_reserved:<name>` | the manifest's or profile's `env` names a CapyCTL-owned variable |
| `engine add` | `capability_missing` | `--deep-park enabled` |
| `engine add` | `profile_exists` (19), `not_interactive` (23) | as ADR 0018 |
| resolution | `capability_missing:deep_park` | `residency: deep` or `host_backed` |
| resolution | `capability_missing:decisions`, `capability_missing:chat` | the deployment's `api` differs from the manifest's |
| resolution | missing required field `resources` (as TensorFold) | no `resources` |
| resolution | `group_shape_unsupported:command` | a `topology` |
| resolution | `command_field_unsupported:<field>` | `extra_args`, `accept_extra_args`, a parser, `memory` or a typed engine field |
| resolution | `command_placeholder_unset:context_length` | the template uses `{context_length}` and the deployment states none |
| placement | `host_capability_missing:command_engines` | the host did not declare the capability |
| request | `cache_salt_unsupported` | a chat request with `cache_salt` |
| launch | `command_listener_exposed` | an owned process listens off loopback (§7) |
| launch | `installation_drift`, `engine_env_reserved:<name>` | as ADR 0008 and ADR 0028 |

### 11. Worked example: basal-omni on `basal-serve`

The manifest, `basal-serve.yaml`:

```yaml
kind: command_engine
schema_version: 1
api: decisions
version: "2.1.0"
args: [--mode, eager, --host, "{host}", --port, "{port}", --model, "{model_dir}"]
health: /health
```

Registration, on the machine that runs it:

```
capyctl engine add --command ~/basal-env/bin/basal-serve --name basal-serve \
    --manifest basal-serve.yaml
```

The tree pinned is `~/basal-env` (it holds `pyvenv.cfg`). There is no `{api_key}`: basal
has no key option, so the engine listens unauthenticated on loopback (§7). There is no
`idle`: basal exposes no running or queued count (`/health` is a constant), so a
cancelled decision settles only at the next stop or restart (§6), and status says so.
`--media on` is left out: basal reads images and audio from a `media` field, which the
closed `/v1/systemone` contract refuses (open question 5).

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
~/basal-env/bin/basal-serve --mode eager --host 127.0.0.1 --port <leased> \
    --model ~/models/sources/huggingface/Remek--basal-omni-1.0-max@<commit>
```

The port opens once the model is loaded; `/health` answers 200; the fixed decision of
ADR 0030 §3 is answered with calibrated probabilities summing to 1; the loopback check
finds one listener on `127.0.0.1`; admission opens. A malformed or unanswerable request
comes back as basal's 422 and is relayed as `engine_rejected`; so would an internal
failure such as running out of memory, since basal answers every handler exception with
422 (open question 6).

## Consequences

- A fifth engine kind in every closed set, a third installation path in `engine add`, a
  manifest parser and placeholder renderer in `capyctl-config`, a generic adapter (port,
  optional health, the API probes, the optional idle reader, the listener check) and the
  `command_engines` capability.
- A host administrator can run an engine CapyCTL does not know without waiting for an
  integration, and gets ownership, routing, admission, accounting and logs. In exchange
  CapyCTL validates nothing inside the command; the manifest's author owns its
  correctness (ADR 0011).
- Without an idle signal, every hang-up or deadline cut leaves a cancelling lease that
  settles only at the next stop or restart, and status says so. Drains on such instances
  no longer wait for cancelling leases (a SPEC §10 amendment).
- New guide sections: adding a command engine, the manifest, the basal example and the
  limits of §4 and §8; `errors.md` gains the codes of §10.

## Alternatives considered

- **A `basal` engine family.** Rejected for now: a full integration (flag ownership,
  reserved options, readiness, gauges) for a project whose flags change every release,
  and the next one-off engine would need the same again. A family can still follow if
  basal becomes a supported engine.
- **Run basal through llama.cpp or SGLang.** Not possible for basal-omni: basal's GGUF
  and SGLang paths cover basal-2.0 only, and its own `llamacpp` and `sglang` modes put a
  basal server in front of the engine, which is the same unknown command.
- **Validate flags by asking the program (`--help`).** Rejected: not every program has
  one, its output is not a contract, and `engine add` would execute an unknown program.
- **Treat a cancelling lease without an idle signal as settled at once.** Rejected by
  SPEC §10: a closed connection is not proof the engine stopped, and basal keeps
  computing.
- **Keep draining for cancelling leases.** Possible (open question 2), but one hang-up
  would then fail every switch away from the instance until an operator stops it.
- **Deployment-level arguments with an approval list.** Rejected: without knowing the
  program's options CapyCTL cannot tell an ordinary argument from one that moves the
  listener or loads code, so the only safe author is the host administrator.

## Verification

A new conformance row, **T44 — command engines**: `engine add --command` executes
nothing, requires `--name`, refuses an unknown or missing required placeholder, a
reserved environment name and `--deep-park enabled`, asks for confirmation (or `--yes`)
and writes the manifest into `engines.yaml` with its digest; the digest covers the
executable and its tree, and drift is flagged and refused only under `--drift refuse`;
rendering substitutes each placeholder into one argument, never through a shell, with
`{host}` 127.0.0.1 and the leased port; a deployment is refused with a parking residency,
a `topology`, no `resources`, `extra_args`, a parser, a typed engine field, a mismatched
`api`, or an unset `{context_length}`; placement refuses a host without
`command_engines`; readiness passes only on the API's probe (chat and decisions); an
owned listener off loopback stops the launch with `command_listener_exposed`; with an
idle signal a cancelling lease settles on idle readings after the hang-up and never on
missing or malformed ones; without one it stays charged while the engine runs, drains
do not wait for it, and it settles on the stop's verified exit and not before; status
shows the cancelling count and how it settles; `cache_salt` is refused; an older release
skips a `command` profile. Tests also tag T13, T14, T17, T37 and T43 where they apply.

CPU and Fake-engine tests (the fake engine gains a command mode with and without an idle
signal) are not qualification. Live rows CE1–CE4 with `basal-serve` and basal-omni-1.0
(registration and drift, decisions readiness, a hang-up settled at stop, the loopback
check) qualify it, on a host and in an environment the owner approves (open question 8).

## Open questions for the owner

1. **The kind's name.** ADR 0008 already names the runtime type `command` (how an
   installation is started: a command, later a container), and every installation today
   is of that runtime type. `engine: command` beside `runtime.type: command` will
   confuse. Keep `command` as approved, or call the kind `custom` (or `generic`)?
2. **Draining without an idle signal.** This ADR lets stops, idle stops and switches
   proceed past cancelling leases and settles them on the verified exit (§6). The
   conservative alternative keeps SPEC §10 as written: drains wait, a switch fails at
   its drain timeout, and an operator's stop is needed after any hang-up. Which?
3. **The loopback check.** Keep it as a launch failure, make it a warning, or leave it
   out (as for llama.cpp and TensorFold, whose `--host` CapyCTL renders and verifies)?
4. **Confirmation at `engine add`.** No other `engine add` asks. Keep the prompt and
   `--yes` for command engines?
5. **basal's request extensions.** The closed `/v1/systemone` contract refuses basal's
   `media` (its images and audio), `facts`, `lang`, `detail`, `think` and its `multi`,
   `act`, `tool` and task question types. Through CapyCTL, basal serves the System One
   subset only. Keep the contract closed, or add a per-profile allowance for extension
   fields (forwarded unchecked)?
6. **basal's 422 for every failure.** An internal error, out-of-memory included, comes
   back as 422 and is relayed as `engine_rejected` with the request settled, which is
   right for accounting (the engine answered) but reads as a client error. Accept, or
   ask upstream for 5xx on internal failures?
7. **An idle signal from basal.** basal has no running or queued count. Should we ask
   upstream for one (for example `requests_running` and `requests_queued` on `/health`)
   so the manifest can name `idle: {url: /health, json: [...]}`?
8. **Qualification.** A basal environment (torch and `basal` 2.1.0; DGX Spark needs the
   cu130 torch build) is a new engine environment. On a maintainer's machine in the home
   directory, which AGENTS.md allows (basal-omni-1.0-mini needs about 16 GB of GPU), or on
   host A or B, which needs your approval and an AGENTS.md entry?
9. **The pinned tree.** The default pins the whole virtual environment, torch included
   (several GB; the first measurement reads it all, later ones only changed files).
   Acceptable, or pin only the executable unless the manifest names `environment`?
10. **Release.** Which release carries command engines?

# Status: Custom engines (ADR 0031) — 2026-10-21 (branch `feat/custom-engines`)

Implements [ADR 0031](../design/adr/0031-custom-engines.md) (owner decisions of
2026-10-20) with its amendment A1 (owner decision 2026-10-21), for 0.1.6. A
fifth engine kind, `custom`, runs a program CapyCTL has no integration for from a
host-approved manifest: `capyctl engine add --custom <executable> --name <profile>
--manifest <file>` executes nothing, validates the manifest (closed, at most 64
KiB; `api` `chat` or `decisions`; `args` with the placeholders `{host}`, `{port}`,
`{model_dir}` and optionally `{served_name}`, `{context_length}`, `{api_key}`;
optional `version`, `environment`, `env`, `health`, `idle`), prints what will run
and asks for confirmation on a terminal, or needs `--yes` (`not_interactive`
otherwise; a refusal at the prompt is `declined`, nothing written). The manifest
is copied into the profile with its digest, which is part of the recipe
fingerprint. The installation digest covers the executable and, for one in a
virtual environment, the whole environment with its interpreter, or the
manifest's `environment` root; drift warns unless `installation_drift: refuse`.
Resolution makes a custom engine a custom/fixed recipe: `resources` required,
`restart_only` (`capability_missing:deep_park`), no `topology`
(`group_shape_unsupported:custom`), `api` equal to the manifest's
(`capability_missing:decisions` / `capability_missing:chat`), every
`engine_config` field but `context_length` and `env` refused
`custom_field_unsupported:<field>`, and `{context_length}` without a declared one
refused `custom_placeholder_unset:context_length`. The launch renders each
placeholder into one argument without a shell, in a closed environment where the
manifest's `env` is the lowest layer; readiness is the port, the optional
`health` path, then the API's existing probe; after it the host lists the
launch's TCP listeners and stops a launch listening off loopback
(`custom_listener_exposed`). Without an idle signal, a request left cancelling by
a hang-up, a deadline cut or an engine error settles only on the instance's
verified exit at the next stop or restart, drains do not wait for it, and its
charge and the instance's reservation are kept until then; status shows each
instance's cancelling count and how it settles (`requests.cancelling`,
`requests.cancelling_settles`), and `engine list` gains `IDLE SIGNAL` and
`ENGINE KEY`. Placement needs the new host capability `custom_engines`
(`host_capability_missing:custom_engines`). New CLI exit 27,
`custom_manifest_invalid`.

Amendment A1: 0.1.5 reads `engines.yaml` strictly against a closed profile shape
and would refuse the whole file over a copied manifest, so custom engine profiles
live in `custom-engines.yaml` beside `engines.yaml` (same revision header, lock
and atomic write; document kind `custom_engines`). `engines.yaml` keeps 0.1.5's
shape and refuses `engine: custom`; a profile name is unique across both files
and the role document (`profile_exists`). A downgrade below 0.1.6 ignores
`custom-engines.yaml` and loses the custom engines; every other engine keeps
working. For a decisions deployment, ADR 0030's document check refuses a
`topology` or a parking residency before the profile is known, so those two
refusals carry ADR 0030's wording there; a chat deployment gets ADR 0031's codes.

SPEC amendments landed with the implementation: §8.2 (a custom engine is a
custom/fixed recipe whose flags CapyCTL does not validate), §9.4 (custom engines
as a later backend), §10 (the settlement of `cancelling` requests on an engine
with no idle signal, and the `idle` signal), §15.1 (the second engines file), §20
(T44, live rows CE1–CE4) and §21. Guide: [Custom engines](../guide/engines.md#custom-engines),
with the basal-serve 2.2.0 example and its 422 caveat; settings:
[Custom engines](../operations/configuration.md#custom-engines); the new codes in
[Exit codes and errors](../guide/errors.md).

Tests: tagged T44 (and T13, T14, T17, T37, T43 where they apply), in
`crates/capyctl-config/src/custom.rs` and `crates/capyctl-config/tests/custom_engines.rs`
(manifest validation, placeholders, digests, profile files, resolution
refusals), `crates/capyctl-adapters/src/custom/` (rendering), the CLI
(`crates/capyctl-cli/tests/errors.rs`: codes and exits), the controller's
installation gate and version-skew capability tests, and the store's cancelling
settlement (`crates/capyctl-store/src/ordinary_lifecycle/park_cancelling_tests.rs`);
the integrator completes this list with the final test names at commit. CPU and
Fake-engine tests are not qualification.

Live qualification, host A (GB10), basal 2.2.0 in `~/basal-2.2.0-venv` (torch
2.11.0 cu130, `basal[media]`) and basal-2.0-4.5B at revision e5ba914, through
`capyctl start standalone` (ports 38443/37443, isolated state and config):
CE1 passed on 2026-10-21: `engine add --custom` without a terminal and without
`--yes` was refused `not_interactive` (exit 23) with nothing written; a "n" at
the terminal prompt was `declined` (exit 2) with nothing written; `--yes` and a
"y" wrote `custom-engines.yaml` (revision 1, manifest and digest) and left
`engines.yaml` absent; the pinned root was the whole virtual environment with
its external interpreter (23,021 files, 4.87 GB); `engine list` showed `CUSTOM
yes`, `IDLE SIGNAL none`, `ENGINE KEY none`. The run found one listing bug (a
second custom profile on the same root showed the first one's version), fixed
here.

Drift launch passed: after a file under a scratch `environment` root changed,
the profile registered with `--drift refuse` was refused `installation_drift`
before any effect, and the `warn` one launched (basal `--mode eager`, ready in
about 60 s) with `installation_drift_flagged` journaled and the host's profile
shown `drifted`. CE2 passed: `basal-20` (`--mode fast --template-prefix off`,
`resources: {gpu: 24GiB, ram: 8GiB}`, `timeouts.initialize: 30m`) became ready
on the fixed decision in 703 s with a warm compile cache; the engine's only
listener was 127.0.0.1:38500; a decision through the router answered in 67 ms;
a manifest binding `--host 0.0.0.0` failed `custom_listener_exposed: an owned
process listens on 0.0.0.0:38500, not on loopback; the engine was stopped`,
with no listener and no process left. CE4 passed: after an operator stop
(verified exit, `ordinary_cleanup_completed`) `start deployment basal-20 --wait`
was ready again in 691 s through the decision probe, and its first decision gave
the same probabilities as before the restart (0.9638675 for `technical`).

CE3 is not demonstrated on basal. A client that hangs up on `POST /v1/systemone`
does not make the request `cancelling`: a collected answer is still completed
when the engine answers (the hung-up request's lease closed completed, status
showed `cancelling: 0` before and after; basal kept computing it, so the next
request waited 9.6 s). A deadline cut cannot be produced on basal either: a
deployment's `request_deadline` must be at least its `timeouts.initialize`, and
basal's start takes 11 to 14 minutes.

CE3 passed on 2026-10-21 on a chat custom engine: llama-server from the
approved llama.cpp v0.7.0 build, added with a manifest of `api: chat` and no
`idle` (`-m "{model_dir}/Qwen3-4B-Q4_K_M.gguf" --host "{host}" --port "{port}"
-c 4096 --alias "{served_name}"`), deployment `resources: {gpu: 8GiB, ram:
4GiB}` on the Qwen3-4B GGUF directory, ready in under 20 s. A streamed chat
request (`stream: true`, `max_tokens: 3000`) cut by the client after 3 s
(53 KB received) left `instances[].requests` at `{cancelling: "1",
cancelling_settles: "stop"}` (0 before), and the text status read `cancelling:
1 (settles at the next stop or restart: the engine reports no idle signal)`.
`capyctl stop deployment` returned in 0.05 s without waiting on it; 3 s later
the deployment was `stopped` with `cancelling: 0`, no llama-server left, and the
journal held `request_cancellation_acknowledged` (count 1, receipt: no idle
signal, the stop operation's cleanup step verified the exit of the process
group) beside that operation's `ordinary_cleanup_completed`.

Deep gate (`scripts/ci-local.sh --deep --only workspace`, host A, branch head
3a99062a): 3436 passed, 3 failed, 1 ignored over 244 test binaries. The three
failures are `a_role_warns_when_its_limits_exceed_the_memory_available`
(`admission_figures`), `standalone_memory_limits_follow_the_document_and_its_overrides`
and `standalone_parked_limit_follows_the_document_and_its_overrides`
(`switching_cli`); they fail the same way on `main` (169081b9) on the same
host, so they are host-specific, not regressions: their assertions depend on
the host's free memory, and host A had 10.0 GiB of its 32 GiB unified pool
available, less than its 16 GiB limit plus 6.4 GiB reserve
(`admission_figures.rs:165`, `switching_cli.rs:336` and `:422`).

basal's `--mode fp8` needs torchao, which the approved environment does not
have; `--mode fast` (bf16, compiled) served a decision directly in 69 ms after
an 843 s cold start (683 s with a warm compile cache), so deployments on it
state `timeouts.initialize`.

# Release note: Engines

- **Custom engines.** `capyctl engine add --custom <program> --name <name>
  --manifest <file>` runs a program CapyCTL has no integration for, such as a
  model's own server, from a manifest that states its command, the API it
  serves (chat or decisions) and optionally a health path and an idle signal.
  CapyCTL owns the process, gives it a private loopback port, routes and admits
  requests, counts the memory you state in `resources` and keeps its log; it
  checks none of the program's options, does not park it, and stops a start
  whose engine listens off loopback. The engine is saved in
  `custom-engines.yaml` beside `engines.yaml`; an older CapyCTL ignores that
  file. See [Custom engines](../guide/engines.md#custom-engines).
- **Cancelled requests on an engine with no idle signal.** A request a client
  abandons on a custom engine without `idle` keeps its memory held until the
  engine's next stop or restart, since nothing shows that the engine stopped
  working on it. Stopping or switching models does not wait for such requests,
  and `capyctl status deployment` shows how many there are and when they
  clear.

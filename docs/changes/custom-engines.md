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
here. The drift launch, CE2 (decisions readiness and the loopback check), CE3
(a hang-up held until the verified stop) and CE4 (restart and the wake check)
are still to run. basal's `--mode fp8` needs torchao, which the approved
environment does not have; `--mode fast` (bf16, compiled) served a decision
directly in 69 ms after an 843 s cold start (683 s with a warm compile cache),
so deployments on it state `timeouts.initialize`.

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

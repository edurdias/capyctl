# TensorFold Engine Implementation Plan

**Goal:** CapyCTL deploys, routes, drains, stops-to-park and relaunches-to-wake TensorFold 0.6.0 installations, found in a venv and registered with `capyctl engine add`, on hosts and in standalone alike, and TensorFold passes live rows TF1–TF5 on host B.

**Architecture:** `tensorfold` becomes the third variant of every closed engine-kind set (`Engine`, `LaunchSettings`, `AdapterSpec`, `PreparedLaunch`, the latency engine list). Resolution makes every TensorFold deployment `restart_only`, requires an explicit `resources` block and `context_length`, and maps a small typed block (`engine_config.tensorfold`) to TensorFold flags. A new adapter module, `capyctl-adapters::tensorfold`, renders `<env>/bin/tensorfold serve <model dir> ...` with a closed environment, treats `GET /health` (`ok`, `busy`, `requests_running`) plus `/v1/models` as readiness, and reads the same `/health` before any stop signal. The host agent and the embedded (standalone) coordinator build the launch through that one builder, so the two paths cannot drift. A drafter is passed the way vLLM and SGLang pass a draft model today: as an approved extra argument whose directory must lie inside the profile's approved paths. A TensorFold installation can also be the role's own `local_engine.tensorfold`, exactly like `local_engine.vllm` and `local_engine.sglang`.

**Tech Stack:** Rust 2021 workspace (tokio, axum for test stubs, reqwest, serde_json, rusqlite), Python 3 runtime probes under `runtime/`, Astro site under `site/`, bash live harness under `scripts/live/matrix/`.

**Spec:** `docs/specs/2026-10-01-tensorfold-engine-design.md` (owner-approved 2026-10-01). Read it with this plan. Governing documents: `docs/SPEC.md` §1, §6.2–§6.5, §8.1–§8.2, §9, §10, §13.3, §20; ADR 0008, ADR 0012, ADR 0014 §3 and §7, ADR 0018 §1; `AGENTS.md`.

## Rulings (made while planning; 2, 7, 8 and 9 decided by the owner)

The spec leaves the points below open or in tension with the code. Each ruling is what the tasks implement. Rulings 2, 7, 8 and 9 were decided by the owner on 2026-10-01 (2 and 7 replaced the first proposals; 8 and 9 accepted).

1. **SPEC section numbers.** SPEC §9.3 is "Later backends" today. The new TensorFold section becomes §9.3 and "Later backends" moves to §9.4. Nothing in the tree cites §9.3 (checked with `grep -rn "§9\.3"`).
2. **`local_engine.tensorfold` (owner decision 2026-10-01).** TensorFold matches today's engines: YAML `local_engine.tensorfold`, flag `--tensorfold-bin` on `start host` and `start standalone`, variable `CAPYCTL_TENSORFOLD_BIN`, one precedence (flag > environment > YAML), implemented once in `crates/capyctl-config/src/engine_settings.rs` for both roles. One `local_engine` executable is the profile `local`; several are `local-vllm`, `local-sglang` and `local-tensorfold`. A TensorFold environment installation gets the same toolchain check and the same forced `deep_park: disabled` as `engine add` (shared code). Task 12.
3. **Typed fields live in `engine_config.tensorfold`.** `max_tokens` and `thinking` are TensorFold-only, so they sit in a family block like `engine_config.vllm` and `engine_config.sglang`. `context_length` and `kv_cache_dtype` stay common fields. The common fields TensorFold has no flag for (`dtype`, `quantization`, `max_concurrent_requests`, `cuda_graphs`, `language_model_only`, `trust_remote_code`) are refused on a TensorFold profile, with the path named.
4. **What CapyCTL renders.** Besides the spec's command line, CapyCTL always renders `--backend cuda` (MLX hosts are out of scope), `--snapshot-dir none` (TensorFold's default writes prefix snapshots under the service user's home; `none` keeps them in memory) and `--drafter none` when the deployment's extra arguments name no `--drafter` (TensorFold's default `auto` would pick up a drafter from the Hugging Face cache). The engine environment also sets `HF_HUB_OFFLINE=1` and `TRANSFORMERS_OFFLINE=1`, so nothing is fetched by the engine.
5. **`--vision` stays ordinary.** It is a prefix of the sensitive `--vision-urls`, so the abbreviation rule would flag it. It joins `ORDINARY_EXACT` (exact name only).
6. **No protected entry.** TensorFold launches its own `bin/tensorfold` script; there is no `runtime/tensorfold_entry.py`. Reserved names are refused in Rust at deploy time and again by `validate_rendered_args` when the command is rendered, abbreviations included (argparse's `allow_abbrev` is on in TensorFold 0.6.0). SPEC §8.2's "verified again after the engine's own parser resolves them" is therefore met by the prefix rule, not by a parser re-check. ADR 0023 says so.
7. **Drafter, as other engines' draft models (owner decision 2026-10-01).** Finding: vLLM passes a draft model only inside `--speculative-config`, an extra argument that needs `accept_extra_args`, named approval in `security.approved_options`, and whose `model` key must be an absolute path inside `security.approved_paths` (ADR 0014 Amendment A3), checked lexically at deploy time (`engine_policy.rs`, `Sensitivity::SpeculativeConfig`) and through symlinks at launch (`runtime/extra_args_policy.py`). SGLang's `--speculative-draft-model-path` is a `Sensitivity::Path` option with the same two gates. Neither accepts a Hugging Face id, neither fetches the draft model, and neither records or checks its digest; neither is a reserved name. TensorFold matches: `--drafter` is not reserved and not a typed field; it is a `Sensitivity::Path` extra argument (approved by name, value inside `approved_paths`), checked lexically at deploy time by the shared policy and, since TensorFold has no protected entry, through symlinks by the launch builder before spawn. A repository id is refused because it is not an approved path. No drafter digest, no fetch.
8. **Startup bound.** The coordinator's Initialize deadline is fixed by the server before the host knows whether a TensorFold build exists. So: an undeclared `timeouts.initialize` on a TensorFold deployment resolves to 1800 s, and an undeclared `request_deadline` resolves to `max(host default, 1800 s)` so the first-build bound is not lowered (SPEC §6: no operation deadline beyond the request deadline). The host's TensorFold Initialize gives up at the ordinary derived bound when `TORCH_EXTENSIONS_DIR` already holds a build, and at the full deadline when it does not. "The profile can change both" is the deployment's existing `timeouts.initialize` and `request_deadline`, which replace both values; no new profile field. Accepted by the owner 2026-10-01.
9. **Drain gate.** TensorFold has no park, so its "park" is the existing restart-only release: a switch victim or an idle-policy stop (SPEC §6.5), both of which leave the deployment eligible for on-demand activation. `capyctl park deployment` on a TensorFold deployment fails clearly, as SPEC §6.3 requires for a tier without parking (existing `Unsupported`). Before any stop signal, the process owner (the host agent, or the embedded coordinator in standalone) reads TensorFold's `/health` for up to 30 s or the command's remaining time, whichever is less. Idle (`ok`, `requests_running: 0`, `busy: false`) lets the signal go. Busy or inconsistent counters at the bound fail the cleanup as uncertain: no signal, accounting retained, as an unprovable terminate does today. Accepted by the owner 2026-10-01.
10. **`toolchain_missing` exit code 26** (next free after `still_stopping` 25). A `--deep-park enabled` request on a TensorFold installation is refused `capability_missing` (exit 5, the unsupported class) and nothing is written.
11. **Readiness probe answers.** A thinking model may spend the eight probe tokens on its reasoning trace. The Initialize probe and the host's fresh probe accept a non-empty `content` or a non-empty `reasoning_content`, for every engine.
12. **Non-streaming responses keep the `tensorfold` object.** CapyCTL forwards chat as a stream and assembles a non-streaming answer from the chunks; the assembler now copies a top-level `tensorfold` object from the final chunk, so the spec's "responses pass through unchanged" holds for both shapes.
13. **Single process.** TensorFold 0.6.0 serves from one process (no worker children; checked in the installed package). Its Initialize evidence requires the API process only, where vLLM requires a worker too.
14. **Live session shape.** TF1–TF5 run in standalone on host B with the models root set to the spike's Hugging Face cache, so nothing is copied or moved. TF4's vLLM side uses host B's authorized vLLM 0.29 environment and a Qwen3-4B checkpoint fetched with the `hf:` shorthand into that root's `sources` directory (downloads are owner-approved for experiments; check free disk first).

## Global Constraints

- Engine kind serde name `tensorfold`; default profile name `tensorfold`; entry point `<env>/bin/tensorfold`; `build_fingerprint` is the checked version (spec §1, §2).
- Verified set gains `(tensorfold, "0.6.0")`; any other version lists as `custom` (spec §2).
- Detection reads `tensorfold-*.dist-info` under `site-packages`, executes nothing, same bounds and locations as ADR 0018 §1 (spec §2).
- Version check: `<env>/bin/tensorfold --version` under the existing bounded check (60 s, 4 KiB, cleared environment, own process group); TensorFold prints `tensorfold 0.6.0`.
- Toolchain check at `engine add`: `ninja`, `nvcc`, and `c++` or `g++`, looked up only on the closed launch PATH (installation `bin`, then `<cuda_home>/bin`, then `/usr/local/bin:/usr/bin:/bin`), never the caller's PATH, running nothing; refusal `toolchain_missing` names the missing tools and where it looked (spec §2, SPEC §13.3).
- Launch: `<env>/bin/tensorfold serve <model dir> --name <served name> --host 127.0.0.1 --port <service port> --no-update-check [engine flags]` (spec §3), plus rulings 4.
- Engine environment adds `TENSORFOLD_NO_UPDATE_CHECK=1`, `CUDA_VISIBLE_DEVICES` from placement, `TORCH_EXTENSIONS_DIR=<state>/engines/tensorfold/<build_fingerprint>/torch_extensions` (service user, mode 0700) (spec §3).
- Reserved flags: `--host`, `--port`, `--name`, `--alias`, `--backend`, `--context`, `--tp`, `--rank`, `--master`, `--master-port`, `--snapshot-dir`, `--no-update-check` (spec §3, ruling 7: `--drafter` is not reserved).
- Typed mapping: `context_length` → `--context` (required), `kv_cache_dtype` → `--kv-dtype`, `tensorfold.max_tokens` → `--max-tokens`, `tensorfold.thinking` → `--thinking` / `--no-thinking` (spec §3, ruling 3).
- Sensitive TensorFold 0.6.0 options needing host approval by name: `--vision-urls` (listener or egress), `--lane-kernels` (code), `--drafter` (path, value inside `security.approved_paths`, as SGLang's `--speculative-draft-model-path`); `--snapshot-dir` is reserved (spec §3, ruling 7).
- `local_engine.tensorfold` / `--tensorfold-bin` / `CAPYCTL_TENSORFOLD_BIN`, flag > environment > YAML, host and standalone (ruling 2).
- A TensorFold deployment needs an explicit `resources` block (spec §3).
- Readiness: `GET /health` 200 with `"ok": true` and `GET /v1/models` listing the served name; a listening port is not readiness (SPEC §6.1). First-build bound 1800 s (spec §4, ruling 8).
- Residency: `restart_only` only; `deep` or `host_backed` on a TensorFold profile fails resolution with `capability_missing` (spec §6).
- Loopback-only engine listener; no engine control path; no engine key (spec §3, SPEC §9.1 protections stay as they are for vLLM and SGLang).
- No new environments on the hosts except the owner's TensorFold exception: one venv, `~/tensorfold-0.6.0-venv`, on host B (owner exception 2026-10-01). Host names are never written in the tree; use "host A" and "host B".
- Cite the governing requirement inline (`// SPEC §6.1: ...`, `// ADR 0023 §3: ...`); tag every new test with its acceptance ID: `// T41` for TensorFold conformance, plus the existing IDs a test also covers (T01, T03, T07, T14, T21, T37).
- CPU and Fake-engine tests are not qualification. Only TF1–TF5 qualify the engine. Say so in every status claim.
- Verification before every commit: `cargo fmt --all --check`; `cargo clippy --workspace --all-targets --locked -- -D warnings`; the core suite `cargo test -p capyctl-adapters -p capyctl-store -p capyctl-controller -p capyctl-management -p harness --all-targets --no-fail-fast --locked -- --test-threads=4`; `cargo test --workspace --all-targets --locked`. Tasks touching `runtime/` also run `python3 -m unittest discover -s runtime/tests -p 'test_*.py'`. Tasks touching `site/` or the guides run `cd site && npm run check`.
- Prose in documents and commit messages is normal English; no AI attribution, machine names, IPs or home paths of the maintainer in anything committed.

## Review Focus

The inputs below are implied by the spec, untested by any task's main path, and most likely to bite a user first. Each has a pinning test in the task named.

1. **A thinking model whose first eight probe tokens are all reasoning** (Nemotron with `--thinking` on, the default): readiness must pass on a non-empty `reasoning_content`, not fail with "empty content". Test: Task 9, `a_reasoning_only_probe_answer_is_an_answer`.
2. **A deployment that names a Hugging Face repository id as the drafter** (`extra_args: ["--drafter", "z-lab/Qwen3.8-27B-DFlash2"]`, what TensorFold's own docs show), on a host that approved `--drafter`: refused at deploy time as a path outside the approved paths, never passed to TensorFold. Test: Task 2, `a_drafter_repository_id_is_refused`.
3. **`extra_args: ["--vision"]`** (an ordinary option that is a prefix of the sensitive `--vision-urls`): accepted without approval, while `--vision-urls` still needs it. Test: Task 2, `vision_is_ordinary_and_vision_urls_needs_approval`.
4. **A venv holding both `vllm` and `tensorfold` packages, named by its directory:** refused as ambiguous with the three entry points named, never silently registered as one of them. Test: Task 3, `an_environment_with_two_engines_names_each_entry_point`.
5. **A second TensorFold launch after the first one built its kernels:** the warm launch uses the ordinary bound, not 30 minutes, so a hung warm start fails in minutes. Test: Task 7, `a_warm_launch_gives_up_at_the_ordinary_bound`.

---

## File Structure

New files:

| File | Responsibility |
|---|---|
| `docs/design/adr/0023-tensorfold-engine.md` | The decision record; amends SPEC §1, §9 (new §9.3), §20 (T41) and ADR 0018 §1. |
| `crates/capyctl-config/tests/tensorfold.rs` | Engine kind, option policy and resolution tests (T41). |
| `crates/capyctl-agent/src/engines/toolchain.rs` | Toolchain lookup on the closed launch PATH; executes nothing. |
| `crates/capyctl-agent/src/engine_cache.rs` | `EngineCacheRoot`: the private per-version `TORCH_EXTENSIONS_DIR`. |
| `crates/capyctl-adapters/src/tensorfold/mod.rs` | Module root and re-exports. |
| `crates/capyctl-adapters/src/tensorfold/args.rs` | `PlanInputTensorfold`, `render_command`, the closed engine environment. |
| `crates/capyctl-adapters/src/tensorfold/frozen.rs` | `plan_from_effective`: the one builder both execution paths use. |
| `crates/capyctl-adapters/src/tensorfold/http.rs` | `/health` and `/v1/models` reads, `HealthReport`. |
| `crates/capyctl-adapters/src/tensorfold/adapter.rs` | `TensorfoldAdapter`: `EngineAdapter` and `ChatForward`. |
| `crates/capyctl-adapters/src/tensorfold/initialize.rs` | The Initialize step (spawn, readiness, probe, evidence). |
| `crates/capyctl-adapters/tests/tensorfold_args.rs` | Rendering tests. |
| `crates/capyctl-adapters/tests/tensorfold_adapter.rs` | Readiness, work, Initialize tests against an axum stub. |
| `crates/capyctl-agent/src/native_execution/tensorfold.rs` | Host-side plan, adapter, and the idle gate before Terminate. |

Modified files (main ones; each task lists exact symbols):

| File | Change |
|---|---|
| `docs/SPEC.md` | §1 R01, new §9.3, §9.4 renumber, §20 T41, §21 later amendments. |
| `docs/design/adr/0018-engine-registration.md` | Amendment note pointing at ADR 0023. |
| `crates/capyctl-config/src/engine_policy.rs` | `Engine::Tensorfold`, `name`/`from_name`, TensorFold reserved, typed and sensitive tables. |
| `crates/capyctl-config/src/registration.rs` | `VERIFIED` gains 0.6.0; `profile_document` uses `Engine::name`. |
| `crates/capyctl-config/src/schema.rs` | `engine_config.tensorfold` block. |
| `crates/capyctl-config/src/effective.rs`, `effective/engine_config.rs`, `effective/core.rs`, `effective/timeouts.rs`, `effective/snapshot.rs`, `effective/legacy.rs`, `context_fit.rs`, `engine_settings.rs` | Resolution, timeouts, snapshot, `local_engine.tensorfold`. |
| `crates/capyctl-domain/src/launch.rs` | `TensorfoldLaunchSettings`, `LaunchSettings::Tensorfold`, `provenance_mut`. |
| `crates/capyctl-store/src/development_controls.rs` | TensorFold's unauthenticated loopback surfaces. |
| `crates/capyctl-agent/src/engines.rs`, `engines/resolve.rs`, `installation.rs`, `load.rs`, `native_execution.rs`, `lib.rs` | Detection, entry point, probe names, load gauges, launch. |
| `runtime/engine_capabilities.py`, `runtime/tests/test_engine_capabilities.py` | TensorFold probe. |
| `crates/capyctl-adapters/src/lib.rs`, `resolve.rs`, `traits.rs`, `engine_env.rs`, `forward.rs`, `vllm/initialize.rs` | New module, `AdapterSpec::Tensorfold`, `idle_before_signal`, shared `SYSTEM_PATH`, assembler. |
| `crates/capyctl-protocol/src/reports.rs` | `LATENCY_ENGINES` gains `tensorfold`. |
| `crates/capyctl-controller/src/engine_bindings.rs`, `engine_provider.rs`, `installation_gate.rs`, `checkpoint_digests.rs` (forwarding only), `coordinator/local_adoption.rs`, `coordinator/worker.rs` | Embedded path. |
| `crates/capyctl-management/src/hosts.rs` | `custom` via `Engine::from_name`. |
| `crates/capyctl-cli/src/engine.rs`, `grammar.rs`, `output.rs`, `roles.rs`, `remote_roles.rs`, `standalone_engines.rs`, `standalone_config.rs` | Engine names, toolchain refusal, exit 26, cache root wiring, `--tensorfold-bin`. |
| `crates/capyctl-cli/tests/engine_cli.rs`, `tests/errors.rs` | CLI tests. |
| `docs/guide/engines.md`, `docs/guide/errors.md`, `docs/operations/configuration.md`, `docs/operations/release-notes-0.1.1.md` (new), `site/src/components/landing/Platforms.astro`, `site/src/pages/index.astro` | Docs and site. |
| `docs/runbooks/f2-current-status.md` | Status entry. |

Task order and dependencies: 1 (ADR) → 2 (config and domain) → 3 (detection and probe) → 4 (`engine add`) → 5 (engine cache) → 6 (rendering) → 7 (adapter) → 8 (forwarding) → 9 (host execution) → 10 (load reports) → 11 (standalone) → 12 (`local_engine.tensorfold`) → 13 (docs and site) → 14 (live qualification and status). Tasks 8 and 10 depend only on Task 2 and may run any time after it.

---

### Task 1: ADR 0023 and the SPEC amendments

**Files:**
- Create: `docs/design/adr/0023-tensorfold-engine.md`
- Modify: `docs/SPEC.md` §1.1 (row R01, line 39), §9 (after §9.2, before "### 9.3 Later backends", line 397), §20 (after row T40), §21 "Later amendments" (after the ADR 0019 bullet)
- Modify: `docs/design/adr/0018-engine-registration.md` (status block at the top)

**Interfaces:**
- Consumes: the spec and the rulings above.
- Produces: the names later tasks cite: `ADR 0023 §1`–`§8`, acceptance ID `T41`, closed codes `toolchain_missing` (exit 26) and `capability_missing`, `TENSORFOLD_FIRST_BUILD_MS = 1_800_000`.

- [ ] **Step 1: Check the owner decisions.** Rulings 2, 7, 8 and 9 were decided by the owner on 2026-10-01; the ADR text below records them as decided. No confirmation is needed.

- [ ] **Step 2: Write ADR 0023.** Create `docs/design/adr/0023-tensorfold-engine.md` with exactly:

```markdown
# ADR 0023 — TensorFold as the third engine

**Status:** Accepted (owner decision 2026-10-01).
**Amends:** `SPEC.md` §1 (supported engines), §9 (a new §9.3; "Later backends" becomes
§9.4), §20 (T41), and ADR 0018 §1 (detection and the verified set).
**Related:** ADR 0008 (installations and capability probes), ADR 0012 (deep parking),
ADR 0014 §3 and §7 (reserved settings, checkpoint identity), ADR 0018 (engine
registration). Design: `docs/specs/2026-10-01-tensorfold-engine-design.md`.

## Context

TensorFold (Apache-2.0) is an inference server with an OpenAI-compatible API and
drafter-based parallel decoding on NVIDIA GPUs. Home users run it by hand, one model
per process. It has no sleep, release or unload API, no API key, and sizes itself from
free memory unless `--context` fixes its KV allocation. Its first start builds CUDA
extensions with `torch.utils.cpp_extension`.

## Decision

### 1. Engine kind

`tensorfold` joins `vllm` and `sglang` in every closed engine set. The default
profile name is `tensorfold`; the entry point is `<env>/bin/tensorfold`; the
version check is `<env>/bin/tensorfold --version`. Detection reads
`tensorfold-*.dist-info` with ADR 0018 §1's bounds and locations and executes
nothing. The verified set gains TensorFold 0.6.0.

### 2. Registration

`capyctl engine add` refuses a TensorFold installation with `toolchain_missing`
(exit 26) unless `ninja`, `nvcc` and `c++` or `g++` are on the engine's closed
launch PATH: the installation's `bin`, the profile's `<cuda_home>/bin`, then
`/usr/local/bin:/usr/bin:/bin`. The caller's PATH is never used and nothing is run.
The profile is written `security.deep_park: disabled`; `--deep-park enabled` is
refused `capability_missing`. Like vLLM and SGLang, a role's own TensorFold is
`local_engine.tensorfold` (`--tensorfold-bin`, `CAPYCTL_TENSORFOLD_BIN`), with the
same toolchain check and profile names (`local`, or `local-tensorfold` beside
another engine).

### 3. Launch

`<env>/bin/tensorfold serve <model dir> --name <served> --host 127.0.0.1
--port <port> --no-update-check --backend cuda --snapshot-dir none --context <n>
[--drafter none] [typed flags] [host args] [extra args]`, with `--drafter none`
only when the extra arguments name no `--drafter`. The environment is
closed (SPEC §13.3) and adds `TENSORFOLD_NO_UPDATE_CHECK=1`, `HF_HUB_OFFLINE=1`,
`TRANSFORMERS_OFFLINE=1`, the placement's `CUDA_VISIBLE_DEVICES` and
`TORCH_EXTENSIONS_DIR=<state>/engines/tensorfold/<build_fingerprint>/torch_extensions`
(service user, 0700). Reserved: `--host`, `--port`, `--name`, `--alias`,
`--backend`, `--context`, `--tp`, `--rank`, `--master`, `--master-port`, `--snapshot-dir`, `--no-update-check`, checked with the
abbreviation rule at deploy time and again when the command is rendered. There is
no protected entry: TensorFold's parser accepts abbreviations, and the prefix rule
is what re-verifies them. `--vision-urls`, `--lane-kernels` and `--drafter` need
host approval by name; `--vision` is ordinary.

### 4. Deployment configuration

`context_length` is required and renders `--context`; `kv_cache_dtype` renders
`--kv-dtype`; `engine_config.tensorfold` takes `max_tokens` and `thinking`. The common fields TensorFold has no flag for are refused. A
TensorFold deployment states `resources`. Its residency is `restart_only`; `deep`
or `host_backed` fails resolution with `capability_missing`. An undeclared
`timeouts.initialize` is 1800 s and an undeclared `request_deadline` is at least
1800 s; the host gives up at the ordinary derived bound once
`TORCH_EXTENSIONS_DIR` holds a build.

### 5. Drafter

A drafter is handled as vLLM's and SGLang's draft models are (ADR 0014 §8 and
Amendment A3): `--drafter <dir>` is an extra argument the host approves by name,
whose directory must lie inside `security.approved_paths`, checked lexically at
deploy time and through symlinks before launch. A repository id is refused.
CapyCTL neither fetches a drafter nor records its digest, as for the other
engines' draft models.

### 6. Readiness, drain, park and wake

Ready is `GET /health` with `"ok": true` and the served name in `/v1/models`, then
the chat probe; a non-empty `content` or `reasoning_content` is an answer.
Parking a TensorFold deployment is the restart-only release: drain, read `/health`
until `requests_running` is 0 and `busy` is false, SIGTERM the owned group, verify
exit, then release memory. If the counters are not idle when the bound (30 s or
the command's remaining time) ends, nothing is signalled and the cleanup is
uncertain. Waking is a fresh launch of the same pinned effective contract.
`capyctl park deployment` on a TensorFold deployment is refused as unsupported.

### 7. Requests

Routing selects the deployment; the forwarded request carries the served name.
Responses pass through unchanged, including `reasoning_content` and the
`tensorfold` object, in both streaming and collected form.

### 8. Load reports

The host scrapes `tensorfold:requests_running`, `tensorfold:requests_waiting` and
`tensorfold:kv_cache_usage_ratio`, and forwards `tensorfold:time_to_first_token_seconds`
and `tensorfold:request_latency_seconds` as engine latency series.

## Consequences

Qualification covers host B (GB10, unified memory) only; discrete GPUs run
TensorFold unqualified until a live row passes there. CPU and Fake-engine tests
are not qualification. Out of scope: deep and host-backed residency, `--tp 2`,
Apple Silicon and MLX, Docker, the recipes repository, `deploy --recipe`, and
fetching or digesting a drafter.
```

- [ ] **Step 3: Amend SPEC §1.** In `docs/SPEC.md`, replace row R01 with:

```markdown
| R01 | Multi-engine architecture from the start: vLLM first, SGLang the next engine deliverable, TensorFold third (ADR 0023). |
```

- [ ] **Step 4: Add SPEC §9.3 and renumber.** Rename `### 9.3 Later backends` to `### 9.4 Later backends`, and insert before it:

```markdown
### 9.3 TensorFold

> **Added by [ADR 0023](design/adr/0023-tensorfold-engine.md)** (owner decision 2026-10-01).

TensorFold has no sleep, release or unload API and no API key. Its deployments are
`restart_only`: parking drains, waits for TensorFold's own `/health` to report no
running request, stops the owned process and verifies its exit; waking launches
the same pinned contract again. `deep` and `host_backed` fail resolution with
`capability_missing`. A TensorFold deployment states its `resources` and its
`context_length`, which fixes the engine's KV allocation; TensorFold has no flag
that caps its memory. The engine listens on loopback only, and CapyCTL's routed
path is the only way to it. Its first start builds CUDA extensions into a private
per-version directory, so registration checks the build toolchain on the engine's
closed PATH and the first start's bound is 1800 s.
```

- [ ] **Step 5: Add T41 to §20.** After the T40 row:

```markdown
| T41 | TensorFold conformance | Detection reads metadata only; `engine add` refuses a missing toolchain on the closed PATH; launch arguments and reserved flags; readiness from `/health` and the model list; drain waits for `requests_running: 0` and `busy: false`; `restart_only` park and wake; `deep` refused; a drafter repository id refused; `local_engine.tensorfold` three ways (ADR 0023). |
```

- [ ] **Step 6: Add the §21 amendment line.** After the ADR 0019 bullet under "Later amendments":

```markdown
- 2026-10-01, [ADR 0023](design/adr/0023-tensorfold-engine.md): TensorFold as the third engine, `restart_only`, registered with `engine add` (§1, §9.3, §9.4, T41).
```

- [ ] **Step 7: Point ADR 0018 at the amendment.** In `docs/design/adr/0018-engine-registration.md`, after the `**Related:**` paragraph, add:

```markdown
**Amended by:** ADR 0023 (2026-10-01): detection also reads `tensorfold-*.dist-info`,
the entry point `<env>/bin/tensorfold`, and the verified set gains TensorFold 0.6.0.
```

- [ ] **Step 8: Check links and commit.**

Run: `grep -n "9.3 TensorFold\|9.4 Later backends\|T41" docs/SPEC.md`
Expected: the three new anchors print.

```bash
git add docs/design/adr/0023-tensorfold-engine.md docs/SPEC.md docs/design/adr/0018-engine-registration.md
git commit -m "docs: record ADR 0023, TensorFold as the third engine"
```

---

### Task 2: The `tensorfold` engine kind, its option policy and its resolution

This task makes `tensorfold` a configuration-level engine kind end to end: policy tables, the typed block, resolution rules, timeouts, snapshots and the development-controls mark. Downstream crates get the arms they need to compile; the launch arms refuse until Tasks 9 and 11 replace them.

**Files:**
- Modify: `crates/capyctl-config/src/engine_policy.rs` (`Engine` line 16; `VLLM_RESERVED_FLAGS` line 24; `ORDINARY_EXACT` line 496; `reserved_options` 594; `reserved_families` 614; `typed_options` 621; `sensitivity` 719; `path_within`)
- Modify: `crates/capyctl-config/src/registration.rs` (`VERIFIED` line 321; `profile_document` line 400)
- Modify: `crates/capyctl-config/src/schema.rs` (`ENGINE_CONFIG`, after the `sglang` block, line 322)
- Modify: `crates/capyctl-domain/src/launch.rs` (`LaunchSettings` line 15 and its methods; new `TensorfoldLaunchSettings` after `SglangLaunchSettings`)
- Modify: `crates/capyctl-config/src/effective/engine_config.rs` (`overhead_margin` 86, `RawEngineConfig` 113, `device_request_from_weights` 232, `normalize_engine_config` 673, `declared_engine_config` 975)
- Modify: `crates/capyctl-config/src/effective.rs` (profile lookup near line 1060; `provenance` matches at 1148 and 1188; request deadline at 1206; `resolve_timeouts` call at 1215)
- Modify: `crates/capyctl-config/src/effective/core.rs` (`normalize_profile`, after the deep-park check at line 102)
- Modify: `crates/capyctl-config/src/effective/timeouts.rs` (`resolve_timeouts` line 161)
- Modify: `crates/capyctl-config/src/effective/snapshot.rs` (family list line 232)
- Modify: `crates/capyctl-config/src/effective/legacy.rs` (lines 86 and 106)
- Modify: `crates/capyctl-config/src/context_fit.rs` (lines 334 and 387)
- Modify: `crates/capyctl-config/src/engine_settings.rs` (line 498)
- Modify: `crates/capyctl-store/src/development_controls.rs` (`classify` line 142, `for_effective` line 169)
- Modify (compile arms): `crates/capyctl-agent/src/installation.rs` (lines 62, 70), `crates/capyctl-agent/src/engines/resolve.rs` (`entry`, `check_version`), `crates/capyctl-agent/src/native_execution.rs` (`prepare` 804, `probe_adapter` 882, `authorize_residency` 1485), `crates/capyctl-controller/src/engine_bindings.rs` (`spec` 172), `crates/capyctl-controller/src/engine_provider.rs` (`from_profile` 99), `crates/capyctl-cli/src/engine.rs` (`engine_name` 52, line 447), `crates/capyctl-cli/src/roles.rs` (lines 910 and 989), `crates/capyctl-cli/src/standalone_config.rs` (`engine_name` 57), `crates/capyctl-cli/src/standalone_engines.rs` (line 421), `crates/capyctl-management/src/hosts.rs` (line 246), `crates/capyctl-testkit/src/lib.rs` (new `tensorfold_launch_settings`)
- Test: `crates/capyctl-config/tests/tensorfold.rs` (new)

**Interfaces:**
- Produces:
  - `capyctl_config::engine_policy::Engine::{Vllm, Sglang, Tensorfold}`; `Engine::ALL: [Engine; 3]`; `Engine::name(self) -> &'static str`; `Engine::from_name(&str) -> Option<Engine>`.
  - `capyctl_config::engine_policy::TENSORFOLD_RESERVED_FLAGS: &[&str]`.
  - `capyctl_domain::launch::TensorfoldLaunchSettings { common, memory, max_tokens: Option<u32>, thinking: Option<bool>, extra_args: Vec<String>, provenance }` and `LaunchSettings::Tensorfold(TensorfoldLaunchSettings)`; `LaunchSettings::provenance_mut(&mut self)`.
  - `capyctl_config::effective::TENSORFOLD_FIRST_BUILD_MS: i64 = 1_800_000`; `resolve_timeouts(raw, request_deadline_ms, facts, engine: Engine)`.
  - `capyctl_testkit::tensorfold_launch_settings() -> LaunchSettings`.

- [ ] **Step 1: Write the failing tests.** Create `crates/capyctl-config/tests/tensorfold.rs`:

```rust
//! ADR 0023: the TensorFold engine kind, its option policy and its resolution.
//! CPU tests only; none of this qualifies TensorFold.

use capyctl_config::effective::{resolve_effective, TENSORFOLD_FIRST_BUILD_MS};
use capyctl_config::engine_policy::{
    sensitivity, validate_extra_args, Engine, ExtraArgsContext, Sensitivity,
};
use capyctl_config::registration::is_verified;
use capyctl_config::ConfigErrorCode;
use capyctl_domain::launch::LaunchSettings;
use serde_json::{json, Value};
use std::collections::BTreeSet;

fn fixture() -> (Value, Value) {
    let all: Value =
        serde_json::from_str(include_str!("fixtures/f2-deployment.json")).expect("fixture JSON");
    let (mut deployment, mut host) = (all["deployment"].clone(), all["host"].clone());
    let profile = &mut host["runtime_profiles"]["local"];
    profile["engine"] = "tensorfold".into();
    profile["executable"] = "/opt/tf/bin/tensorfold".into();
    profile["build_fingerprint"] = "0.6.0".into();
    profile["args"] = json!([]);
    profile["security"]["deep_park"] = "disabled".into();
    profile["security"]["approved_paths"] = json!(["/srv/drafters"]);
    deployment["residency"] = "restart_only".into();
    deployment["engine_config"] = json!({"context_length": 8192});
    (deployment, host)
}

fn context<'a>(approved: &'a BTreeSet<String>, fixed: &'a BTreeSet<String>) -> ExtraArgsContext<'a> {
    ExtraArgsContext {
        engine: Engine::Tensorfold,
        sleep_mode: false,
        approved_options: approved,
        approved_paths: &[],
        checkpoint_root: None,
        host_fixed: fixed,
    }
}

// T41 T01: the serde name, the closed name list and the verified set.
#[test]
fn tensorfold_is_a_named_engine_with_a_verified_version() {
    assert_eq!(Engine::from_name("tensorfold"), Some(Engine::Tensorfold));
    assert_eq!(Engine::Tensorfold.name(), "tensorfold");
    assert_eq!(serde_json::to_value(Engine::Tensorfold).unwrap(), "tensorfold");
    assert_eq!(Engine::from_name("TensorFold"), None);
    assert!(is_verified(Engine::Tensorfold, "0.6.0"));
    assert!(!is_verified(Engine::Tensorfold, "0.6.1"));
}

// T41 T14: every reserved flag is refused, abbreviated and as a value form.
#[test]
fn reserved_tensorfold_flags_are_refused_in_every_spelling() {
    let none = BTreeSet::new();
    for args in [
        json!(["--host", "0.0.0.0"]),
        json!(["--port=9000"]),
        json!(["--name", "x"]),
        json!(["--alias", "y"]),
        json!(["--backend", "mlx"]),
        json!(["--context", "4096"]),
        json!(["--tp", "2"]),
        json!(["--rank", "1"]),
        json!(["--master", "10.0.0.1"]),
        json!(["--master-port", "29551"]),
        json!(["--snapshot-dir", "/tmp/s"]),
        json!(["--no-update-check"]),
        json!(["--snapshot", "/tmp/s"]),
        json!(["--conte", "4096"]),
    ] {
        let args: Vec<String> = serde_json::from_value(args.clone()).unwrap();
        let error = validate_extra_args(&args, &context(&none, &none))
            .expect_err(&format!("{args:?} must be refused"));
        assert!(error.to_string().contains("reserved"), "{args:?}: {error}");
    }
}

// T41 T14: a typed field's native spelling in extra_args is a duplicate.
#[test]
fn typed_tensorfold_spellings_are_refused_in_extra_args() {
    let none = BTreeSet::new();
    for (args, field) in [
        (vec!["--kv-dtype", "int8"], "kv_cache_dtype"),
        (vec!["--max-tokens", "512"], "tensorfold.max_tokens"),
        (vec!["--no-thinking"], "tensorfold.thinking"),
    ] {
        let args: Vec<String> = args.into_iter().map(str::to_owned).collect();
        let error = validate_extra_args(&args, &context(&none, &none)).unwrap_err();
        assert!(error.to_string().contains(field), "{args:?}: {error}");
    }
}

// T41 T37 (Review Focus 3): `--vision` is ordinary although it is a prefix of
// `--vision-urls`, which needs named approval; `--lane-kernels` loads code.
#[test]
fn vision_is_ordinary_and_vision_urls_needs_approval() {
    let none = BTreeSet::new();
    assert_eq!(sensitivity(Engine::Tensorfold, "--vision"), None);
    assert_eq!(
        sensitivity(Engine::Tensorfold, "--vision-urls"),
        Some(Sensitivity::ListenerOrEgress)
    );
    assert_eq!(
        sensitivity(Engine::Tensorfold, "--lane-kernels"),
        Some(Sensitivity::Code)
    );
    let ok: Vec<String> = vec!["--vision".into(), "--parallel".into(), "2".into()];
    validate_extra_args(&ok, &context(&none, &none)).unwrap();
    let egress: Vec<String> = vec!["--vision-urls".into()];
    assert!(validate_extra_args(&egress, &context(&none, &none)).is_err());
    let approved: BTreeSet<String> = ["--vision-urls".to_owned()].into();
    validate_extra_args(&egress, &context(&approved, &none)).unwrap();
}

// T41 T14: the typed block maps to settings; restart_only is the residency.
#[test]
fn a_tensorfold_deployment_resolves_restart_only_settings() {
    let (mut deployment, host) = fixture();
    deployment["engine_config"] = json!({
        "context_length": 8192, "kv_cache_dtype": "int8",
        "tensorfold": {"max_tokens": 1024, "thinking": false}
    });
    let effective = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(effective.profile.engine, Engine::Tensorfold);
    assert_eq!(
        effective.residency,
        capyctl_config::effective::Residency::RestartOnly
    );
    let LaunchSettings::Tensorfold(settings) = &effective.engine_config else {
        panic!("TensorFold settings");
    };
    assert_eq!(settings.common.context_length, Some(8192));
    assert_eq!(settings.common.kv_cache_dtype.as_deref(), Some("int8"));
    assert_eq!(settings.max_tokens, Some(1024));
    assert_eq!(settings.thinking, Some(false));
}

// T41 T03: context_length and resources are required; deep is refused with
// capability_missing; foreign typed fields are refused with their path.
#[test]
fn tensorfold_requirements_are_refused_with_their_path() {
    let (mut deployment, host) = fixture();
    deployment["engine_config"] = json!({});
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(error.path, "engine_config.context_length", "{error}");

    let (mut deployment, host) = fixture();
    deployment.as_object_mut().unwrap().remove("resources");
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert_eq!(error.code, ConfigErrorCode::MissingRequired, "{error}");
    assert_eq!(error.path, "resources", "{error}");

    for residency in ["deep", "host_backed"] {
        let (mut deployment, host) = fixture();
        deployment["residency"] = residency.into();
        let error = resolve_effective(&deployment, &host).unwrap_err();
        assert!(error.detail.starts_with("capability_missing"), "{error}");
    }
    // An operator who wrote deep_park: enabled on the profile still gets it.
    let (mut deployment, mut host) = fixture();
    host["runtime_profiles"]["local"]["security"]["deep_park"] = "enabled".into();
    deployment["residency"] = "deep".into();
    let error = resolve_effective(&deployment, &host).unwrap_err();
    assert!(error.detail.starts_with("capability_missing"), "{error}");

    for field in [
        "dtype",
        "quantization",
        "max_concurrent_requests",
        "cuda_graphs",
        "language_model_only",
        "trust_remote_code",
    ] {
        let (mut deployment, host) = fixture();
        let value = match field {
            "max_concurrent_requests" => json!(4),
            "cuda_graphs" | "language_model_only" | "trust_remote_code" => json!(true),
            "dtype" => json!("bfloat16"),
            _ => json!("fp8"),
        };
        deployment["engine_config"][field] = value;
        let error = resolve_effective(&deployment, &host).unwrap_err();
        assert_eq!(error.path, format!("engine_config.{field}"), "{error}");
    }
    let (mut deployment, host) = fixture();
    deployment["engine_config"]["vllm"] = json!({"block_size_tokens": 16});
    assert_eq!(
        resolve_effective(&deployment, &host).unwrap_err().path,
        "engine_config.vllm"
    );
}

// T41 T37 (Review Focus 2, ruling 7): `--drafter` is a path option, as
// SGLang's `--speculative-draft-model-path`: it needs named approval, and its
// value must lie inside the approved paths; a repository id is refused. The
// typed block has no drafter field.
#[test]
fn a_drafter_repository_id_is_refused() {
    let none = BTreeSet::new();
    let approved: BTreeSet<String> = ["--drafter".to_owned()].into();
    let paths = [std::path::PathBuf::from("/srv/drafters")];
    let with_paths = |approved| ExtraArgsContext { approved_paths: &paths, ..context(approved, &none) };
    assert_eq!(
        sensitivity(Engine::Tensorfold, "--drafter"),
        Some(Sensitivity::Path { checkpoint_exempt: false })
    );
    let args = |value: &str| vec!["--drafter".to_owned(), value.to_owned()];
    assert!(validate_extra_args(&args("/srv/drafters/d"), &with_paths(&none)).is_err(), "needs approval");
    validate_extra_args(&args("/srv/drafters/d"), &with_paths(&approved)).unwrap();
    for refused in ["z-lab/Qwen3.8-27B-DFlash2", "drafters/d", "/elsewhere/d", "/srv/drafters/../etc"] {
        assert!(validate_extra_args(&args(refused), &with_paths(&approved)).is_err(), "{refused}");
    }
    let (mut deployment, host) = fixture();
    deployment["engine_config"]["tensorfold"] = json!({"drafter": "/srv/drafters/d"});
    assert!(resolve_effective(&deployment, &host).is_err(), "no typed drafter field");
}

// T41 T14 (ruling 8): the first-build bound and the request deadline floor.
#[test]
fn an_undeclared_initialize_timeout_covers_the_first_build() {
    let (mut deployment, host) = fixture();
    deployment.as_object_mut().unwrap().remove("request_deadline");
    let effective = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(effective.timeouts.initialize_ms, TENSORFOLD_FIRST_BUILD_MS);
    assert!(effective.request_deadline_ms >= TENSORFOLD_FIRST_BUILD_MS);
    let (mut deployment, host) = fixture();
    deployment["request_deadline"] = "600s".into();
    deployment["timeouts"] = json!({"initialize": "300s"});
    let effective = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(effective.timeouts.initialize_ms, 300_000);
    assert_eq!(effective.request_deadline_ms, 600_000);
}

// T41 T08: a TensorFold revision snapshots and re-resolves exactly.
#[test]
fn a_tensorfold_snapshot_decodes_exactly() {
    let (mut deployment, host) = fixture();
    deployment["engine_config"]["tensorfold"] =
        json!({"max_tokens": 256, "thinking": true});
    let effective = resolve_effective(&deployment, &host).unwrap();
    let raw = capyctl_config::effective::encode_effective_snapshot(&effective).unwrap();
    assert_eq!(
        capyctl_config::effective::decode_effective_snapshot(&raw).unwrap(),
        effective
    );
}
```

`context()` sets `approved_paths: &[]`; the drafter test builds its own context with `/srv/drafters` approved. Use the snapshot encoder the store uses (`grep -n "pub fn encode_effective_snapshot\|pub fn decode_effective_snapshot" crates/capyctl-config/src/effective/snapshot.rs`); if the encoder has another name, call that name.

- [ ] **Step 2: Run the tests to see them fail.**

Run: `cargo test -p capyctl-config --test tensorfold --locked`
Expected: FAIL to compile: no variant `Tensorfold`, no `TENSORFOLD_FIRST_BUILD_MS`.

- [ ] **Step 3: Add the engine kind and names.** In `crates/capyctl-config/src/engine_policy.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    Vllm,
    Sglang,
    /// ADR 0023: TensorFold, restart-only, registered with `engine add`.
    Tensorfold,
}

impl Engine {
    /// Every engine kind, in the order lists show them.
    pub const ALL: [Engine; 3] = [Engine::Vllm, Engine::Sglang, Engine::Tensorfold];

    /// The serde, CLI and profile name of the kind.
    pub fn name(self) -> &'static str {
        match self {
            Engine::Vllm => "vllm",
            Engine::Sglang => "sglang",
            Engine::Tensorfold => "tensorfold",
        }
    }

    /// The kind a name stands for; exact, lowercase.
    pub fn from_name(name: &str) -> Option<Engine> {
        Self::ALL.into_iter().find(|engine| engine.name() == name)
    }
}
```

- [ ] **Step 4: Add the TensorFold option tables.** In the same file, after `SGLANG_RESERVED_ALIASES`:

```rust
/// ADR 0023 §3: TensorFold 0.6.0 `serve` options capyctl renders or forbids
/// (`tensorfold/cli_args.py`). `--drafter` is not here: like other engines'
/// draft model options it is an approved path option. `--tp`, `--rank`, `--master` and
/// `--master-port` belong to multi-rank, which is out of scope.
pub const TENSORFOLD_RESERVED_FLAGS: &[&str] = &[
    "--host",
    "--port",
    "--name",
    "--alias",
    "--backend",
    "--context",
    "--tp",
    "--rank",
    "--master",
    "--master-port",
    "--snapshot-dir",
    "--no-update-check",
];
const TENSORFOLD_RESERVED_FAMILIES: &[&str] = &["--capyctl-"];
/// ADR 0023 §4: the native spelling of each typed TensorFold field.
const TENSORFOLD_TYPED_OPTIONS: &[(&str, &str)] = &[
    ("--kv-dtype", "kv_cache_dtype"),
    ("--max-tokens", "tensorfold.max_tokens"),
    ("--thinking", "tensorfold.thinking"),
];
/// ADR 0023 §3: sensitive TensorFold 0.6.0 options. `--snapshot-dir` is
/// also reserved. `--drafter` is a path option exactly as SGLang's
/// `--speculative-draft-model-path` (ADR 0014 §8, ruling 7): named approval,
/// and a value inside `security.approved_paths`.
const TENSORFOLD_SENSITIVE: &[(&str, Sensitivity)] = &[
    ("--vision-urls", Sensitivity::ListenerOrEgress),
    ("--lane-kernels", Sensitivity::Code),
    (
        "--snapshot-dir",
        Sensitivity::Path {
            checkpoint_exempt: false,
        },
    ),
    (
        "--drafter",
        Sensitivity::Path {
            checkpoint_exempt: false,
        },
    ),
];
const TENSORFOLD_SHAPED: &[&str] = &[];
```

Change `ORDINARY_EXACT` to `&["--reasoning-parser", "--vision"]` with the comment `// ADR 0023 §3: TensorFold's --vision is a prefix of --vision-urls.` Add the `Engine::Tensorfold` arm to each match:

```rust
// reserved_options
Engine::Tensorfold => TENSORFOLD_RESERVED_FLAGS
    .iter()
    .map(|name| (*name).to_owned())
    .collect(),
// reserved_families
Engine::Tensorfold => TENSORFOLD_RESERVED_FAMILIES,
// typed_options
Engine::Tensorfold => TENSORFOLD_TYPED_OPTIONS,
// sensitivity
Engine::Tensorfold => (TENSORFOLD_SENSITIVE, TENSORFOLD_SHAPED),
```

`validate_profile_args` needs no change: only SGLang refuses profile arguments. Make `path_within` `pub(crate)` so resolution reuses it.

- [ ] **Step 5: Add the verified version.** In `crates/capyctl-config/src/registration.rs`:

```rust
pub const VERIFIED: &[(Engine, &str)] = &[
    (Engine::Vllm, "0.29.0"),
    (Engine::Sglang, "0.5.20"),
    // ADR 0023 §1.
    (Engine::Tensorfold, "0.6.0"),
];
```

and in `profile_document` replace the engine match with `"engine": spec.engine.name(),`.

- [ ] **Step 6: Add the domain settings.** In `crates/capyctl-domain/src/launch.rs`, add after `SglangLaunchSettings`:

```rust
/// ADR 0023 §4: a TensorFold deployment's resolved settings. TensorFold has
/// no park strategy, so nothing here is derived from residency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TensorfoldLaunchSettings {
    pub common: CommonEngineSettings,
    pub memory: MemoryRequest,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
    pub extra_args: Vec<String>,
    pub provenance: BTreeMap<String, SettingSource>,
}
```

Add `Tensorfold(TensorfoldLaunchSettings)` to `LaunchSettings`, the `Self::Tensorfold(settings) => &settings.<field>` arm to `common`, `memory`, `memory_mut`, `extra_args` and `provenance`, and:

```rust
    pub fn provenance_mut(&mut self) -> &mut BTreeMap<String, SettingSource> {
        match self {
            Self::Vllm(settings) => &mut settings.provenance,
            Self::Sglang(settings) => &mut settings.provenance,
            Self::Tensorfold(settings) => &mut settings.provenance,
        }
    }
```

Replace the two hand-written `let provenance = match &mut engine_config { ... }` blocks in `crates/capyctl-config/src/effective.rs` (lines 1148 and 1188) with `let provenance = engine_config.provenance_mut();`.

- [ ] **Step 7: Add the schema block.** In `crates/capyctl-config/src/schema.rs` `ENGINE_CONFIG`, after the `sglang` entry:

```rust
        // ADR 0023 §4: TensorFold's own typed fields.
        (
            "tensorfold",
            FieldSpec::Struct(&[("max_tokens", SCALAR), ("thinking", SCALAR)]),
        ),
```

- [ ] **Step 8: Resolve the typed block.** In `crates/capyctl-config/src/effective/engine_config.rs`:

Add `pub const TENSORFOLD_OVERHEAD_MARGIN_BYTES: i64 = 0;` beside the other margins with the comment `// ADR 0023 §4: a TensorFold deployment declares its resources, so nothing is derived from a margin.`, the `Engine::Tensorfold => TENSORFOLD_OVERHEAD_MARGIN_BYTES` arm in `overhead_margin`, and `Engine::Sglang | Engine::Tensorfold => 0` in `device_request_from_weights`.

Add to `RawEngineConfig`:

```rust
    #[serde(default)]
    tensorfold: Option<RawTensorfoldFields>,
```

and:

```rust
#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTensorfoldFields {
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    thinking: Option<bool>,
}
```

In `normalize_engine_config`, replace the family-mismatch match with:

```rust
    let foreign: &[(&str, bool)] = &[
        ("vllm", raw.vllm.is_some()),
        ("sglang", raw.sglang.is_some()),
        ("tensorfold", raw.tensorfold.is_some()),
    ];
    for (block, present) in foreign {
        if *present && *block != engine.name() {
            return Err(family_mismatch(block));
        }
    }
    // ADR 0023 §4: TensorFold has no flag for these common fields.
    if engine == Engine::Tensorfold {
        for (field, set) in [
            ("dtype", raw.dtype.is_some()),
            ("quantization", raw.quantization.is_some()),
            ("max_concurrent_requests", raw.max_concurrent_requests.is_some()),
            ("cuda_graphs", raw.cuda_graphs.is_some()),
            ("language_model_only", raw.language_model_only.is_some()),
            ("trust_remote_code", raw.trust_remote_code.is_some()),
        ] {
            if set {
                return Err(invalid(
                    format!("engine_config.{field}"),
                    "TensorFold has no option for this field; remove it",
                ));
            }
        }
        if raw.context_length.is_none() {
            return Err(ConfigError::new(
                ConfigErrorCode::MissingRequired,
                "engine_config.context_length",
                "a TensorFold deployment states context_length: it fixes the engine's KV \
                 allocation, and TensorFold has no flag that caps its memory",
            ));
        }
    }
```

Extend the `declared` array to 16 entries with `("tensorfold.max_tokens", ...)` and `("tensorfold.thinking", ...)` read from `raw.tensorfold.clone().unwrap_or_default()`. Add the settings arm:

```rust
        Engine::Tensorfold => {
            positive("engine_config.tensorfold.max_tokens", tensorfold.max_tokens)?;
            LaunchSettings::Tensorfold(TensorfoldLaunchSettings {
                common,
                memory,
                max_tokens: tensorfold.max_tokens,
                thinking: tensorfold.thinking,
                extra_args,
                provenance,
            })
        }
```

A drafter reaches TensorFold only as `extra_args: ["--drafter", "<dir>"]` under the shared extra-argument policy (ruling 7); resolution adds nothing for it. Add the `"tensorfold"` block to `declared_engine_config`:

```rust
        "tensorfold": raw.tensorfold.as_ref().map(|t| serde_json::json!({
            "max_tokens": t.max_tokens, "thinking": t.thinking,
        })),
```

Only add the key when `raw.tensorfold` is `Some`, so the command fingerprint of every vLLM and SGLang deployment is unchanged: build the object, then `if raw.tensorfold.is_none() { object.remove("tensorfold"); }`.

- [ ] **Step 9: Profile and deployment rules.** In `crates/capyctl-config/src/effective/core.rs` `normalize_profile`, before the existing deep-park check:

```rust
    // ADR 0023 §6, SPEC §9.3: TensorFold has no park path whatever its
    // profile says.
    if raw_profile.engine == Engine::Tensorfold && residency.parks() {
        return Err(ConfigError::new(
            ConfigErrorCode::UnsupportedCombination,
            "residency",
            "capability_missing: TensorFold supports restart_only only",
        ));
    }
```

and change the credential check so a TensorFold profile needs no `credential_ref` (it has no key): `matches!(raw_profile.engine, Engine::Vllm | Engine::Sglang)` stays as written, which already excludes TensorFold.

In `crates/capyctl-config/src/effective.rs`, right after the profile lookup (line 1062):

```rust
    // ADR 0023 §4: TensorFold has no memory cap, so its reservation is the
    // operator's explicit statement.
    if raw_profile.engine == Engine::Tensorfold && d.resources.is_none() {
        return Err(ConfigError::new(
            ConfigErrorCode::MissingRequired,
            "resources",
            "a TensorFold deployment states resources: TensorFold sizes itself from free \
             memory and has no flag that caps it",
        ));
    }
```

Replace the request-deadline default at line 1206:

```rust
            .unwrap_or_else(|| {
                // ADR 0023 §4 (ruling 8): the first build's bound must not be
                // lowered by the request deadline.
                if raw_profile.engine == Engine::Tensorfold {
                    host.queue.request_deadline_ms.max(timeouts::TENSORFOLD_FIRST_BUILD_MS)
                } else {
                    host.queue.request_deadline_ms
                }
            }),
```

and pass `raw_profile.engine` to `resolve_timeouts` at line 1215 (if `raw_profile` was moved by then, copy `let engine = raw_profile.engine;` near the lookup).

- [ ] **Step 10: The first-build timeout.** In `crates/capyctl-config/src/effective/timeouts.rs`:

```rust
/// ADR 0023 §4: the first TensorFold start builds its CUDA extensions; the
/// host gives up earlier once a build exists (`capyctl-adapters::tensorfold`).
pub const TENSORFOLD_FIRST_BUILD_MS: i64 = 1_800_000;
```

Add `engine: Engine` as the last parameter of `resolve_timeouts`, and change the Initialize pick to:

```rust
    let derived_initialize = match engine {
        Engine::Tensorfold => TENSORFOLD_FIRST_BUILD_MS,
        Engine::Vllm | Engine::Sglang => derived_initialize_ms(weights),
    };
    let initialize_ms = pick("initialize", initialize, derived_initialize);
```

Update the two other callers (`validate_declared_timeouts` passes `Engine::Vllm`, which keeps today's floors, and the unit tests in the same file pass `Engine::Vllm`). Export the constant from `effective.rs`'s `pub use timeouts::{...}` list.

- [ ] **Step 11: Snapshot, legacy, context fit, home expansion, settings.**
  - `effective/snapshot.rs` line 232: add `Some("tensorfold") => &["max_tokens", "thinking"],`.
  - `effective/legacy.rs`: replace the `expected` match with `let expected = engine.name();`, and in the `match engine` at line 106 add `Engine::Tensorfold => return Err(refuse("TensorFold has no legacy launch settings")),`.
  - `context_fit.rs`: add `LaunchSettings::Tensorfold(s) => (&s.common, &s.memory, None, 0),` at line 341 and `LaunchSettings::Tensorfold(s) => s.common.context_length,` at line 389.
  - `engine_settings.rs` line 498: `Engine::Vllm | Engine::Tensorfold => settings.args.clone(),`.

- [ ] **Step 12: Development controls.** In `crates/capyctl-store/src/development_controls.rs`:

```rust
/// ADR 0023 §3: TensorFold has no engine key; its whole HTTP surface is
/// unauthenticated on loopback, reached only through capyctl's routed path.
pub const TENSORFOLD_UNAUTHENTICATED_LOCAL_SURFACES: UnauthenticatedSurfaces =
    UnauthenticatedSurfaces {
        surface: &["/v1", "/health", "/metrics"],
        listener: "loopback",
        access: "inference",
    };
```

In `classify`, after the SGLang branch: `if engine == Engine::Tensorfold { controls.unauthenticated_local_surfaces = Some(TENSORFOLD_UNAUTHENTICATED_LOCAL_SURFACES); }`; in `for_effective` add `LaunchSettings::Tensorfold(_) => None,`.

- [ ] **Step 13: Close the downstream matches.** Run `cargo check --workspace --all-targets --locked` and add these arms; every other report is a match this list missed, handled the same way (fail closed for anything that would launch, `Engine::name`/`from_name` for anything that names):
  - `capyctl-agent/src/installation.rs`: `capability_names` `Engine::Tensorfold => &["core", "deep_park", "metrics"],`; `package_name` `Engine::Tensorfold => "tensorfold",`.
  - `capyctl-agent/src/engines/resolve.rs`: `entry` `Engine::Tensorfold => env.join("bin/tensorfold"),`; `check_version` `Engine::Vllm | Engine::Tensorfold => command.arg("--version"),`.
  - `capyctl-agent/src/native_execution.rs`: in `prepare` and `probe_adapter`, `Engine::Tensorfold => Err(SessionError),` with the comment `// ADR 0023: refused until the host's TensorFold launch lands (Task 9).`; in `authorize_residency`, `Engine::Tensorfold => Err(JournalError::Unauthorized),` with `// ADR 0023 §6: TensorFold never parks.` (this one is final).
  - `capyctl-controller/src/engine_bindings.rs` `spec`: `Engine::Tensorfold => Err(CoordinatorError::Service("TensorFold launches are not wired on this path yet".into())),` (replaced in Task 11).
  - `capyctl-controller/src/engine_provider.rs` `from_profile`: `let engine = profile["engine"].as_str().and_then(Engine::from_name).unwrap_or(Engine::Vllm);`.
  - `capyctl-cli/src/engine.rs`: `engine_name` becomes `engine.name()` at each call site and the function is deleted; line 447 becomes `let engine = profile["engine"].as_str().and_then(Engine::from_name).unwrap_or(Engine::Vllm);`.
  - `capyctl-cli/src/roles.rs`: line 911 `Engine::Vllm | Engine::Tensorfold => settings.args,`; line 989 `let engine = profile["engine"].as_str().and_then(Engine::from_name).unwrap_or(Engine::Vllm);`.
  - `capyctl-cli/src/standalone_config.rs`: delete `engine_name` and use `engine.name()`.
  - `capyctl-cli/src/standalone_engines.rs` line 421: `"engine": n.installation.engine.name(),`.
  - `capyctl-management/src/hosts.rs` line 246: `let custom = Engine::from_name(&engine).is_none_or(|e| !is_verified(e, &version));`.
  - `capyctl-testkit/src/lib.rs`: add

```rust
/// ADR 0023: TensorFold settings with a fixed context.
pub fn tensorfold_launch_settings() -> LaunchSettings {
    LaunchSettings::Tensorfold(capyctl_domain::launch::TensorfoldLaunchSettings {
        common: CommonEngineSettings {
            context_length: Some(8192),
            ..CommonEngineSettings::default()
        },
        memory: MemoryRequest {
            request_bytes: 30 << 30,
            kv_cache_bytes: 4 << 30,
            margin_bytes: 0,
            weights_bytes: None,
            startup_bytes: None,
            device_total_bytes: None,
            overhead_bytes: None,
        },
        max_tokens: None,
        thinking: None,
        extra_args: Vec::new(),
        provenance: Default::default(),
    })
}
```

- [ ] **Step 14: Run the tests to see them pass.**

Run: `cargo test -p capyctl-config --test tensorfold --locked`
Expected: PASS, 9 tests.

Run: `cargo test -p capyctl-config -p capyctl-domain -p capyctl-store --all-targets --locked`
Expected: PASS; no existing vLLM or SGLang fingerprint changed (the `deployment_command_fingerprint` tests in `crates/capyctl-config/tests/effective.rs` still pass).

- [ ] **Step 15: Full verification and commit.** Run the Global Constraints verification commands.

```bash
git add crates/capyctl-config crates/capyctl-domain crates/capyctl-store/src/development_controls.rs \
  crates/capyctl-agent/src/installation.rs crates/capyctl-agent/src/engines/resolve.rs \
  crates/capyctl-agent/src/native_execution.rs crates/capyctl-controller/src/engine_bindings.rs \
  crates/capyctl-controller/src/engine_provider.rs crates/capyctl-cli/src crates/capyctl-management/src/hosts.rs \
  crates/capyctl-testkit/src/lib.rs
git commit -m "feat: tensorfold engine kind, option policy and resolution (ADR 0023)"
```

---

### Task 3: Detection, resolution and the capability probe

**Files:**
- Modify: `crates/capyctl-agent/src/engines.rs` (`engine_of` line 17)
- Modify: `crates/capyctl-agent/src/engines/resolve.rs` (`resolve`: the `wanted` match and the ambiguity message)
- Modify: `crates/capyctl-agent/src/engines/detect.rs` (only if a vLLM- or SGLang-specific filter appears; `grep -n "vllm\|sglang" crates/capyctl-agent/src/engines/detect.rs` must print nothing afterwards)
- Modify: `runtime/engine_capabilities.py` (`ENGINES`, new `probe_tensorfold`, `_run`)
- Test: `crates/capyctl-agent/tests/engines.rs`, `runtime/tests/test_engine_capabilities.py`, `crates/capyctl-agent/src/installation.rs` (unit test module)

**Interfaces:**
- Consumes: `Engine::Tensorfold`, `Engine::name`, `package_name(Engine::Tensorfold) == "tensorfold"` (Task 2).
- Produces: `capyctl_agent::engines::resolve(path)` returns `Resolved { engine: Tensorfold, executable: <env>/bin/tensorfold, .. }`; `CapabilityReport::parse(Engine::Tensorfold, ..)` accepts the probe's report.

- [ ] **Step 1: Write the failing Rust tests.** Append to `crates/capyctl-agent/tests/engines.rs`:

```rust
// T41 T07: a TensorFold venv, its directory or its bin/tensorfold, resolves
// to <env>/bin/tensorfold from dist-info; 0.6.0 is verified.
#[test]
fn a_tensorfold_environment_resolves_to_its_entry_point() {
    let dir = tempfile::tempdir().unwrap();
    let env = fake_env(&dir.path().join("tf"), &[("tensorfold", "0.6.0")]);
    script(&env.join("bin/tensorfold"), "echo tensorfold 0.6.0");
    for named in [env.clone(), env.join("bin/tensorfold")] {
        let resolved = resolve(&named).unwrap();
        assert_eq!(resolved.engine, Engine::Tensorfold);
        assert_eq!(resolved.version, "0.6.0");
        assert_eq!(resolved.executable, env.join("bin/tensorfold"));
        assert!(!resolved.custom());
    }
    let resolved = resolve(&env).unwrap();
    assert_eq!(
        check_version(&resolved, Duration::from_secs(10)).unwrap(),
        "0.6.0"
    );
}

// T41 T07 (Review Focus 4): two engines in one venv named by its directory
// are refused, naming each entry point.
#[test]
fn an_environment_with_two_engines_names_each_entry_point() {
    let dir = tempfile::tempdir().unwrap();
    let env = fake_env(&dir.path().join("both"), &[("vllm", "0.29.0"), ("tensorfold", "0.6.0")]);
    script(&env.join("bin/vllm"), "echo 0.29.0");
    script(&env.join("bin/tensorfold"), "echo tensorfold 0.6.0");
    let error = resolve(&env).unwrap_err();
    assert_eq!(error.code(), "engine_unsupported");
    let text = error.to_string();
    for entry in ["bin/vllm", "bin/tensorfold"] {
        assert!(text.contains(entry), "{text}");
    }
    assert_eq!(resolve(&env.join("bin/tensorfold")).unwrap().engine, Engine::Tensorfold);
}

// T41 T07 T37: detection lists a TensorFold venv from metadata alone.
#[test]
fn detection_finds_a_tensorfold_environment_and_runs_nothing() {
    let home = tempfile::tempdir().unwrap();
    let env = fake_env(&home.path().join("tensorfold-0.6.0-venv"), &[("tensorfold", "0.6.0")]);
    let marker = home.path().join("ran");
    script(&env.join("bin/tensorfold"), &format!("touch {}", marker.display()));
    let found = detect(&empty_roots(home.path()), &ScanBounds::default());
    assert!(found.iter().any(|c| c.engine == Engine::Tensorfold && c.version == "0.6.0"), "{found:?}");
    assert!(!marker.exists(), "detection executes nothing");
}
```

Field names on `Candidate` may differ (`grep -n "pub struct Candidate" -A12 crates/capyctl-agent/src/engines/detect.rs`); use the actual ones.

Add to the unit tests in `crates/capyctl-agent/src/installation.rs`:

```rust
    // T41: the TensorFold probe report parses with its three capabilities.
    #[test]
    fn a_tensorfold_probe_report_parses() {
        let good = br#"{"schema":"capyctl/engine-capabilities/v1","engine":"tensorfold","capabilities":{"core":[],"deep_park":["unsupported"],"metrics":[]}}"#;
        let report = CapabilityReport::parse(Engine::Tensorfold, good).unwrap();
        assert_eq!(report.available("core"), Some(true));
        assert_eq!(report.available("deep_park"), Some(false));
    }
```

- [ ] **Step 2: Write the failing Python test.** Append to `runtime/tests/test_engine_capabilities.py`, inside the existing `TestCase` class that holds `test_malformed_invocation_is_refused_without_output` (use `grep -n "class " runtime/tests/test_engine_capabilities.py` to find it):

```python
    def test_tensorfold_probe_names_its_destinations_and_never_parks(self):
        # ADR 0023 §1, T41: core checks the serve destinations capyctl renders
        # or reserves; deep_park is always missing.
        import argparse
        parser = argparse.ArgumentParser()
        sub = parser.add_subparsers()
        serve = sub.add_parser("serve")
        for dest in capabilities.TENSORFOLD_DESTINATIONS:
            serve.add_argument("--" + dest.replace("_", "-"))
        report = capabilities.probe_tensorfold(parser, importer=lambda name: None)
        self.assertEqual(report.missing_labels("core"), ())
        self.assertEqual(report.missing_labels("deep_park"), ("unsupported",))
        bare = capabilities.probe_tensorfold(argparse.ArgumentParser(), importer=lambda name: None)
        self.assertIn("destination:context", bare.missing_labels("core"))
```

- [ ] **Step 3: Run them to see them fail.**

Run: `cargo test -p capyctl-agent --test engines --locked && cargo test -p capyctl-agent --lib installation --locked`
Expected: FAIL: `holds no vllm or sglang package` (detection does not know `tensorfold`).

Run: `python3 -m unittest discover -s runtime/tests -p 'test_engine_capabilities.py'`
Expected: FAIL: `module has no attribute TENSORFOLD_DESTINATIONS`.

- [ ] **Step 4: Detection and resolution.** In `crates/capyctl-agent/src/engines.rs`:

```rust
fn engine_of(package: &str) -> Option<Engine> {
    // ADR 0018 §1, ADR 0023 §1: the package names of the engine kinds.
    Engine::ALL.into_iter().find(|engine| crate::installation::package_name(*engine) == package)
}
```

Update the doc comment on `packages` to say "the vLLM, SGLang and TensorFold packages". In `engines/resolve.rs` `resolve`, extend the file-name match:

```rust
        let wanted = if name == "vllm" {
            Engine::Vllm
        } else if name == "tensorfold" {
            Engine::Tensorfold
        } else if name == "python" || name == "python3" || name.starts_with("python3.") {
            Engine::Sglang
        } else {
            return Err(ResolveError::Unsupported(format!(
                "{} is not bin/vllm, bin/tensorfold or bin/python3",
                path.display()
            )));
        };
```

replace the "holds no vllm or sglang package" message with "holds no vllm, sglang or tensorfold package", and the two-engine refusal with:

```rust
        None => {
            let entries: Vec<String> = found
                .iter()
                .map(|(engine, _)| format!("{} for {}", entry(&env, *engine).display(), engine.name()))
                .collect();
            return Err(ResolveError::Unsupported(format!(
                "{} holds several engines; name {}",
                env.display(),
                entries.join(", or ")
            )));
        }
```

The message names the entry point of each engine found, so an environment holding vLLM and TensorFold names `bin/vllm` and `bin/tensorfold`, as the test asserts.

- [ ] **Step 5: The probe.** In `runtime/engine_capabilities.py`, document the capability set in the module docstring (`deep_park` for TensorFold is always missing: it has no release API, ADR 0023 §6) and add:

```python
ENGINES = {
    "sglang": ("core", "deep_park", "metrics", "observation"),
    "vllm": ("core", "deep_park", "metrics"),
    "tensorfold": ("core", "deep_park", "metrics"),
}
# ADR 0023 §3, §4: the TensorFold 0.6.0 `serve` destinations capyctl renders,
# reserves or types (tensorfold/cli_args.py).
TENSORFOLD_DESTINATIONS = ("host", "port", "name", "alias", "backend", "context",
                           "drafter", "tp", "rank", "master", "master_port",
                           "snapshot_dir", "no_update_check", "kv_dtype",
                           "max_tokens", "thinking")
# Load gauges the host agent scrapes (crates/capyctl-agent/src/load.rs).
TENSORFOLD_GAUGES = ("requests_running", "requests_waiting", "kv_cache_usage_ratio")


def tensorfold_metrics(importer):
    module = _import(importer, "tensorfold.server.metrics")
    if module is None:
        return ("metrics_module",)
    return tuple("gauge:" + name for name in _names_in_source(module, TENSORFOLD_GAUGES))


def probe_tensorfold(parser, importer=importlib.import_module):
    destinations = parser_destinations(parser) if parser is not None else frozenset()
    return Report("tensorfold", (
        ("core", tuple("destination:" + name for name in TENSORFOLD_DESTINATIONS
                       if name not in destinations) if parser is not None else ("parser",)),
        ("deep_park", ("unsupported",)),
        ("metrics", tensorfold_metrics(importer)),
    ))
```

and in `_run`, before the vLLM branch:

```python
    if engine == "tensorfold":
        try:
            from tensorfold import cli_args
            parser = cli_args.build_parser({name: None for name in
                                            ("serve", "pull", "models", "update", "info")})
        except Exception:
            parser = None
        return probe_tensorfold(parser)
```

(Measured on the installed 0.6.0 package: `cli_args` imports in 25 ms under `-I -S` with the site directory appended, and `build_parser` takes the handler dictionary shown.)

- [ ] **Step 6: Run the tests to see them pass.**

Run: `cargo test -p capyctl-agent --test engines --locked && cargo test -p capyctl-agent --lib installation --locked`
Expected: PASS.

Run: `python3 -m unittest discover -s runtime/tests -p 'test_*.py'`
Expected: OK.

- [ ] **Step 7: Verify and commit.** Run the Global Constraints verification commands.

```bash
git add crates/capyctl-agent/src/engines.rs crates/capyctl-agent/src/engines crates/capyctl-agent/src/installation.rs \
  crates/capyctl-agent/tests/engines.rs runtime/engine_capabilities.py runtime/tests/test_engine_capabilities.py
git commit -m "feat: detect and probe TensorFold installations (ADR 0023 §1)"
```

---

### Task 4: `engine add` for TensorFold: toolchain check and the deep-park rule

**Files:**
- Create: `crates/capyctl-agent/src/engines/toolchain.rs`
- Modify: `crates/capyctl-agent/src/engines.rs` (`pub mod toolchain;`)
- Modify: `crates/capyctl-adapters/src/engine_env.rs` (new `pub const SYSTEM_PATH`), `crates/capyctl-adapters/src/vllm/initialize.rs` (line 51: use it)
- Modify: `crates/capyctl-cli/src/engine.rs` (`add`, after `resolve`, before `register`)
- Modify: `crates/capyctl-cli/src/output.rs` (`ExitCode::TOOLCHAIN_MISSING = 26`, `exit_code`)
- Modify: `crates/capyctl-cli/src/grammar.rs` (`EngineAdd` path help text, line 460)
- Modify: `docs/guide/errors.md` (rows 16, 17 and new 26)
- Test: `crates/capyctl-agent/tests/engines.rs`, `crates/capyctl-cli/tests/engine_cli.rs`, `crates/capyctl-cli/tests/errors.rs`

**Interfaces:**
- Consumes: `Resolved` (Task 3), `detect_cuda_home` (`capyctl_config::registration`).
- Produces:
  - `capyctl_adapters::engine_env::SYSTEM_PATH: &str = "/usr/local/bin:/usr/bin:/bin"`.
  - `capyctl_agent::engines::toolchain::check(engine_bin: &Path, cuda_home: Option<&Path>, system: &str) -> Result<(), ToolchainMissing>`; `ToolchainMissing { pub missing: Vec<&'static str>, pub searched: Vec<PathBuf> }` with `Display`.
  - CLI closed code `toolchain_missing`, exit 26.

- [ ] **Step 1: Write the failing tests.** Append to `crates/capyctl-agent/tests/engines.rs`:

```rust
// T41 T37: the toolchain is looked up on the closed launch PATH only, in
// order, and nothing is executed.
#[test]
fn the_toolchain_is_found_on_the_closed_path_only() {
    use capyctl_agent::engines::toolchain::check;
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("env/bin");
    let cuda = dir.path().join("cuda");
    let system = dir.path().join("system");
    for d in [&bin, &cuda.join("bin"), &system] {
        std::fs::create_dir_all(d).unwrap();
    }
    let marker = dir.path().join("ran");
    let touch = format!("touch {}", marker.display());
    let system_path = system.to_string_lossy().into_owned();
    let missing = check(&bin, Some(&cuda), &system_path).unwrap_err();
    assert_eq!(missing.missing, vec!["ninja", "nvcc", "c++ or g++"]);
    assert_eq!(missing.searched, vec![bin.clone(), cuda.join("bin"), system.clone()]);
    let text = missing.to_string();
    assert!(text.contains("ninja") && text.contains(&bin.display().to_string()), "{text}");
    script(&bin.join("ninja"), &touch);
    script(&cuda.join("bin/nvcc"), &touch);
    script(&system.join("g++"), &touch);
    check(&bin, Some(&cuda), &system_path).unwrap();
    // Without cuda_home its bin is not searched.
    assert_eq!(check(&bin, None, &system_path).unwrap_err().missing, vec!["nvcc"]);
    // A directory or a non-executable file is not a tool.
    std::fs::remove_file(bin.join("ninja")).unwrap();
    std::fs::create_dir(bin.join("ninja")).unwrap();
    std::fs::write(system.join("ninja"), "").unwrap();
    assert_eq!(check(&bin, Some(&cuda), &system_path).unwrap_err().missing, vec!["ninja"]);
    assert!(!marker.exists(), "the check executes nothing");
    // The caller's PATH is never read.
    std::env::set_var("PATH", bin.to_string_lossy().as_ref());
    assert!(check(&dir.path().join("empty"), None, "/nonexistent").is_err());
}
```

Append to `crates/capyctl-cli/tests/engine_cli.rs`:

```rust
/// A TensorFold venv: `tensorfold --version` prints `tensorfold <reported>`,
/// the interpreter answers the probe, and `bin` holds the build tools.
fn tensorfold_env(root: &Path, version: &str, tools: &[&str]) -> PathBuf {
    let site = root.join("lib/python3.12/site-packages");
    std::fs::create_dir_all(site.join("tensorfold")).unwrap();
    std::fs::create_dir_all(site.join(format!("tensorfold-{version}.dist-info"))).unwrap();
    std::fs::write(
        site.join(format!("tensorfold-{version}.dist-info/METADATA")),
        format!("Name: tensorfold\nVersion: {version}\n"),
    )
    .unwrap();
    std::fs::write(root.join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
    std::fs::create_dir_all(root.join("bin")).unwrap();
    script(&root.join("bin/tensorfold"), &format!("echo tensorfold {version}"));
    let report = json!({"schema": "capyctl/engine-capabilities/v1", "engine": "tensorfold",
        "capabilities": {"core": [], "deep_park": ["unsupported"], "metrics": []}});
    script(&root.join("bin/python3"), &format!("echo '{report}'"));
    for tool in tools {
        script(&root.join("bin").join(tool), "exit 0");
    }
    root.to_path_buf()
}

// T41 T07 T21: TensorFold registers as `tensorfold`, deep park disabled.
#[tokio::test]
async fn add_registers_tensorfold_with_deep_park_disabled() {
    let dir = private_dir();
    let env = tensorfold_env(&dir.path().join("tf"), "0.6.0", &["ninja", "nvcc", "c++"]);
    let document = host_doc(dir.path());
    let (_role, _stop) = role(&document, json!({"ok": true, "published": "published"})).await;
    let out = execute(&add(&env), Some(&document), dir.path()).await.unwrap();
    assert_eq!(out["profile"], "tensorfold");
    assert_eq!(out["engine"], "tensorfold");
    assert_eq!(out["custom"], false);
    let profile = &engines_of(&document).profiles["tensorfold"];
    assert_eq!(profile["executable"], env.join("bin/tensorfold").to_string_lossy().as_ref());
    assert_eq!(profile["build_fingerprint"], "0.6.0");
    assert_eq!(profile["security"]["deep_park"], "disabled");
    let asked = Command::EngineAdd {
        path: Some(env.clone()),
        name: Some("tf-deep".into()),
        deep_park: Some(DeepParkChoice::Enabled),
        drift: DriftChoice::Warn,
        args: vec![],
    };
    let error = execute(&asked, Some(&document), dir.path()).await.unwrap_err();
    assert_eq!(error.code, "capability_missing");
    assert!(!engines_of(&document).profiles.contains_key("tf-deep"));
}

// T41 T03: a missing toolchain is refused before anything runs or is written.
#[tokio::test]
async fn add_refuses_tensorfold_without_its_toolchain() {
    let dir = private_dir();
    let env = tensorfold_env(&dir.path().join("tf"), "0.6.0", &["nvcc", "c++"]);
    let document = host_doc(dir.path());
    let error = execute(&add(&env), Some(&document), dir.path()).await.unwrap_err();
    if error.code == "toolchain_missing" {
        assert!(error.message.contains("ninja"), "{}", error.message);
        assert!(error.message.contains(&env.join("bin").display().to_string()));
        assert!(!engines_beside(&document).exists());
    } else {
        // This machine has ninja in a system directory: the closed PATH
        // found it there, which is the documented order.
        assert!(which_in_system("ninja"), "{error:?}");
    }
}

fn which_in_system(tool: &str) -> bool {
    capyctl_adapters::engine_env::SYSTEM_PATH
        .split(':')
        .any(|d| Path::new(d).join(tool).is_file())
}
```

Append to `crates/capyctl-cli/tests/errors.rs`:

```rust
// T01 T41 (ADR 0023 §2): a missing TensorFold build toolchain exits 26.
#[test]
fn toolchain_missing_exits_26() {
    let error = StructuredError {
        code: "toolchain_missing",
        message: String::new(),
    };
    assert_eq!(error.exit_code(), ExitCode(26));
    assert_eq!(ExitCode::TOOLCHAIN_MISSING, ExitCode(26));
}
```

Add `ExitCode::TOOLCHAIN_MISSING` to the list in the test above it that asserts every code differs from `HOST_INELIGIBLE`. Add `capyctl-adapters` to `crates/capyctl-cli/Cargo.toml` `[dev-dependencies]` if it is not already a dependency (`grep -n capyctl-adapters crates/capyctl-cli/Cargo.toml`).

- [ ] **Step 2: Run them to see them fail.**

Run: `cargo test -p capyctl-agent --test engines the_toolchain --locked; cargo test -p capyctl-cli --test engine_cli tensorfold --locked; cargo test -p capyctl-cli --test errors --locked`
Expected: FAIL to compile (`toolchain` module, `TOOLCHAIN_MISSING`).

- [ ] **Step 3: Share the system PATH.** In `crates/capyctl-adapters/src/engine_env.rs`:

```rust
/// SPEC §13.3 (amended 2026-09-25): the fixed system tool directories after
/// the engine's own bin and the profile's `<cuda_home>/bin`.
pub const SYSTEM_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
```

and in `crates/capyctl-adapters/src/vllm/initialize.rs` replace the local `const SYSTEM_PATH` with `use crate::engine_env::SYSTEM_PATH;`.

- [ ] **Step 4: The toolchain lookup.** Create `crates/capyctl-agent/src/engines/toolchain.rs`:

```rust
//! ADR 0023 §2: the tools TensorFold's first start needs to build its CUDA
//! extensions, looked up on the engine's closed launch PATH (SPEC §13.3): the
//! installation's `bin`, the profile's `<cuda_home>/bin`, then the fixed
//! system directories. The caller's PATH is never read and nothing is run.
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// One requirement: its name in messages and the file names that satisfy it.
const TOOLS: &[(&str, &[&str])] = &[
    ("ninja", &["ninja"]),
    ("nvcc", &["nvcc"]),
    ("c++ or g++", &["c++", "g++"]),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolchainMissing {
    pub missing: Vec<&'static str>,
    pub searched: Vec<PathBuf>,
}

impl std::fmt::Display for ToolchainMissing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let searched: Vec<String> = self.searched.iter().map(|d| d.display().to_string()).collect();
        write!(
            f,
            "TensorFold builds CUDA extensions on its first start and needs {}, which \
             {} not on the engine's PATH ({}); install {} there, or name a CUDA toolkit \
             with CUDA_HOME, then add the engine again",
            self.missing.join(", "),
            if self.missing.len() == 1 { "is" } else { "are" },
            searched.join(":"),
            if self.missing.len() == 1 { "it" } else { "them" },
        )
    }
}

fn executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `Ok` when every tool is an executable regular file in one of the
/// directories, searched in launch order.
pub fn check(engine_bin: &Path, cuda_home: Option<&Path>, system: &str) -> Result<(), ToolchainMissing> {
    let searched: Vec<PathBuf> = std::iter::once(engine_bin.to_path_buf())
        .chain(cuda_home.map(|home| home.join("bin")))
        .chain(system.split(':').filter(|d| !d.is_empty()).map(PathBuf::from))
        .collect();
    let missing: Vec<&'static str> = TOOLS
        .iter()
        .filter(|(_, names)| {
            !searched
                .iter()
                .any(|dir| names.iter().any(|name| executable(&dir.join(name))))
        })
        .map(|(label, _)| *label)
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(ToolchainMissing { missing, searched })
    }
}
```

and `pub mod toolchain;` in `crates/capyctl-agent/src/engines.rs`. (`std::fs::metadata` follows a symlink, which is what the engine's own PATH lookup does; the file is never opened or run.)

- [ ] **Step 5: Wire it into `engine add`.** In `crates/capyctl-cli/src/engine.rs` `add`, compute the CUDA toolkit once before the deep-park decision and refuse early for TensorFold:

```rust
    // SPEC §13.3 amendment (owner decision 2026-09-25).
    let cuda_home = capyctl_config::registration::detect_cuda_home(
        std::env::var("CUDA_HOME").ok().as_deref(),
        |nvcc| nvcc.is_file(),
    );
    if resolved.engine == Engine::Tensorfold {
        // ADR 0023 §2: TensorFold has no park path; refuse a request for one
        // before anything runs or is written.
        if deep_park == Some(DeepParkChoice::Enabled) {
            return Err(error(
                "capability_missing",
                "TensorFold has no sleep or release API; it runs restart_only, so \
                 --deep-park enabled is refused and nothing was written",
            ));
        }
        let bin = resolved.executable.parent().unwrap_or(&resolved.env);
        capyctl_agent::engines::toolchain::check(
            bin,
            cuda_home.as_deref(),
            capyctl_adapters::engine_env::SYSTEM_PATH,
        )
        .map_err(|missing| error("toolchain_missing", format!("{missing}; nothing was written")))?;
    }
```

Place it after the profile-name checks and before `register`. Use `cuda_home` for `ProfileSpec.cuda_home` instead of the second `detect_cuda_home` call. The deep-park decision stays as written: TensorFold's probe reports `deep_park` missing, so `None` resolves to disabled; if the probe cannot run (`None` report), force it for TensorFold: `None => resolved.engine != Engine::Tensorfold && registration.deep_park_missing != Some(true),`. Change the profile-name default to `resolved.engine.name()`.

- [ ] **Step 6: The exit code.** In `crates/capyctl-cli/src/output.rs`:

```rust
    /// ADR 0023 §2: TensorFold's build toolchain is not on the engine's
    /// closed PATH; nothing was written.
    pub const TOOLCHAIN_MISSING: Self = Self(26);
```

and in `exit_code`: `"toolchain_missing" => ExitCode::TOOLCHAIN_MISSING,` and `"capability_missing" => ExitCode::UNSUPPORTED,` (explicit, although the fallback is the same). Update the `EngineAdd` path help in `grammar.rs` to `/// A venv directory, its bin/vllm, bin/tensorfold or bin/python3. Omit to pick interactively.`

- [ ] **Step 7: The errors page.** In `docs/guide/errors.md`, change row 16's meaning to "The path holds no `vllm`, `sglang` or `tensorfold` package." and its fix to "Name the environment, its `bin/vllm`, `bin/tensorfold` or `bin/python3`, ...", row 17's fix to "Register a vLLM, SGLang or TensorFold installation.", and add after row 25:

```markdown
| 26 | `toolchain_missing` | `capyctl engine add` found a TensorFold installation, but `ninja`, `nvcc` or a C++ compiler is not on the engine's PATH (its `bin`, the CUDA toolkit's `bin`, then `/usr/local/bin`, `/usr/bin`, `/bin`). TensorFold builds CUDA kernels on its first start. Nothing was written. | Install what the message names into one of those directories, or set `CUDA_HOME` to a toolkit that has `nvcc`, then add the engine again. |
```

and, in the row for exit 5, add `capability_missing` with "`--deep-park enabled` on an engine that cannot park (TensorFold), or a deployment asking such an engine to park." / "Leave `--deep-park` out; use `residency: restart_only`."

- [ ] **Step 8: Run the tests to see them pass.**

Run: `cargo test -p capyctl-agent --test engines --locked && cargo test -p capyctl-cli --test engine_cli --test errors --test site_errors_page --locked`
Expected: PASS.

- [ ] **Step 9: Verify and commit.** Run the Global Constraints verification commands, then `cd site && npm run check` (the errors page is synced into the site).

```bash
git add crates/capyctl-agent/src/engines.rs crates/capyctl-agent/src/engines/toolchain.rs crates/capyctl-agent/tests/engines.rs \
  crates/capyctl-adapters/src/engine_env.rs crates/capyctl-adapters/src/vllm/initialize.rs \
  crates/capyctl-cli/src/engine.rs crates/capyctl-cli/src/output.rs crates/capyctl-cli/src/grammar.rs \
  crates/capyctl-cli/tests/engine_cli.rs crates/capyctl-cli/tests/errors.rs crates/capyctl-cli/Cargo.toml docs/guide/errors.md
git commit -m "feat: engine add registers TensorFold after a closed-PATH toolchain check"
```

---

### Task 5: The private TensorFold build cache

**Files:**
- Create: `crates/capyctl-agent/src/engine_cache.rs`
- Modify: `crates/capyctl-agent/src/lib.rs` (`pub mod engine_cache;`)
- Modify: `crates/capyctl-agent/src/native_execution.rs` (new field `engine_cache: Option<EngineCacheRoot>` and builder `with_engine_cache_root`, beside `with_rendezvous_root` at line 416)
- Modify: `crates/capyctl-controller/src/engine_bindings.rs` (`ProfileBindings`: field and `with_engine_cache_root`, beside `with_rendezvous_root` at line 85)
- Modify: `crates/capyctl-cli/src/remote_roles.rs` (line 1051: `.with_engine_cache_root(config.state_dir.join("engines"))`), `crates/capyctl-cli/src/roles.rs` (line 1041: `.with_engine_cache_root(log_dir.with_file_name("engines"))`; create `<state>/engines` 0700 at role start where `<state>/rendezvous` is created — `grep -n RENDEZVOUS_DIR crates/capyctl-cli/src/*.rs`)
- Test: unit tests in `crates/capyctl-agent/src/engine_cache.rs`

**Interfaces:**
- Produces:
  - `capyctl_agent::engine_cache::EngineCacheRoot::new(dir: PathBuf) -> Self`.
  - `EngineCacheRoot::torch_extensions(&self, fingerprint: &str) -> Result<PathBuf, EngineCacheError>`: creates and checks `<root>/tensorfold/<fingerprint>/torch_extensions`.
  - `capyctl_agent::engine_cache::has_build(dir: &Path) -> bool`.
  - `NativeHostExecution::with_engine_cache_root(self: Arc<Self>, dir: PathBuf) -> Arc<Self>`; `ProfileBindings::with_engine_cache_root(self, dir: PathBuf) -> Self`.

- [ ] **Step 1: Write the failing tests.** Create `crates/capyctl-agent/src/engine_cache.rs` with the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn root() -> (tempfile::TempDir, EngineCacheRoot) {
        let dir = tempfile::tempdir().unwrap();
        let engines = dir.path().join("engines");
        std::fs::create_dir(&engines).unwrap();
        std::fs::set_permissions(&engines, std::fs::Permissions::from_mode(0o700)).unwrap();
        (dir, EngineCacheRoot::new(engines))
    }

    // T41 T37 (ADR 0023 §3): the extensions directory is created private on
    // every level and reused.
    #[test]
    fn the_extensions_directory_is_private_and_per_version() {
        let (_dir, root) = root();
        let path = root.torch_extensions("0.6.0").unwrap();
        assert!(path.ends_with("tensorfold/0.6.0/torch_extensions"));
        for level in [path.parent().unwrap().parent().unwrap(), path.parent().unwrap(), &path] {
            let meta = std::fs::symlink_metadata(level).unwrap();
            assert_eq!(meta.mode() & 0o7777, 0o700, "{}", level.display());
            assert_eq!(meta.uid(), unsafe { libc::geteuid() });
        }
        assert_eq!(root.torch_extensions("0.6.0").unwrap(), path);
        assert!(!has_build(&path));
        std::fs::create_dir(path.join("tensorfold_qmm_v3")).unwrap();
        assert!(has_build(&path));
    }

    // T37: a fingerprint is a path component, never a path; a widened mode or
    // a symlink anywhere on the way is refused, not repaired.
    #[test]
    fn unsafe_fingerprints_and_directories_are_refused() {
        let (dir, root) = root();
        for bad in ["", "..", "a/b", "0.6.0/../x", &"x".repeat(200)] {
            assert!(root.torch_extensions(bad).is_err(), "{bad:?}");
        }
        let path = root.torch_extensions("0.6.0").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(root.torch_extensions("0.6.0").is_err());
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.path().join("engines/tensorfold/0.7.0")).unwrap();
        assert!(root.torch_extensions("0.7.0").is_err());
    }
}
```

- [ ] **Step 2: Run them to see them fail.**

Run: `cargo test -p capyctl-agent --lib engine_cache --locked`
Expected: FAIL to compile (`EngineCacheRoot` undefined).

- [ ] **Step 3: Implement.** Above the tests in `engine_cache.rs`:

```rust
//! ADR 0023 §3, SPEC §13.3: TensorFold builds CUDA extensions into
//! `TORCH_EXTENSIONS_DIR` and loads them on every later start, so whoever can
//! write that directory can run code in the engine. Each version gets its own
//! directory under the role's private state, owned by the service user, mode
//! 0700 at every level capyctl creates; anything else found there is refused.
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineCacheError {
    #[error("the build fingerprint is not a plain path component")]
    Fingerprint,
    #[error("{0} is not a private directory of the service user")]
    NotPrivate(PathBuf),
    #[error("{0} could not be created")]
    Create(PathBuf),
}

/// The role's private engine cache root (`<state>/engines`, 0700).
#[derive(Clone, Debug)]
pub struct EngineCacheRoot {
    dir: PathBuf,
}

fn component(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value != "."
        && value != ".."
        && value.bytes().all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
}

fn private(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| {
        meta.is_dir()
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o7777 == 0o700
    })
}

fn ensure(path: &Path) -> Result<(), EngineCacheError> {
    if std::fs::symlink_metadata(path).is_err() {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|_| EngineCacheError::Create(path.to_path_buf()))?;
    }
    private(path)
        .then_some(())
        .ok_or_else(|| EngineCacheError::NotPrivate(path.to_path_buf()))
}

impl EngineCacheRoot {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// `<root>/tensorfold/<fingerprint>/torch_extensions`, created private.
    pub fn torch_extensions(&self, fingerprint: &str) -> Result<PathBuf, EngineCacheError> {
        if !component(fingerprint) {
            return Err(EngineCacheError::Fingerprint);
        }
        ensure(&self.dir)?;
        let mut path = self.dir.clone();
        for part in ["tensorfold", fingerprint, "torch_extensions"] {
            path.push(part);
            ensure(&path)?;
        }
        Ok(path)
    }
}

/// ADR 0023 §4: whether an earlier start left a build here (any entry).
pub fn has_build(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some())
}
```

`EngineCacheRoot::new` takes the root the role created 0700; `ensure(&self.dir)` re-checks it. Add the builders:

```rust
    /// ADR 0023 §3: where TensorFold launches keep their extension builds.
    pub fn with_engine_cache_root(mut self: Arc<Self>, dir: PathBuf) -> Arc<Self> {
        Arc::make_mut(&mut self).engine_cache = Some(crate::engine_cache::EngineCacheRoot::new(dir));
        self
    }
```

on `NativeHostExecution` (match the pattern `with_rendezvous_root` uses; it already handles `Arc<Self>`), and the plain-`self` equivalent on `ProfileBindings`. Wire both roles as listed under **Files**.

- [ ] **Step 4: Run the tests to see them pass.**

Run: `cargo test -p capyctl-agent --lib engine_cache --locked`
Expected: PASS, 2 tests.

- [ ] **Step 5: Verify and commit.** Run the Global Constraints verification commands.

```bash
git add crates/capyctl-agent/src/engine_cache.rs crates/capyctl-agent/src/lib.rs crates/capyctl-agent/src/native_execution.rs \
  crates/capyctl-controller/src/engine_bindings.rs crates/capyctl-cli/src/remote_roles.rs crates/capyctl-cli/src/roles.rs
git commit -m "feat: private per-version build cache for TensorFold launches"
```

---

### Task 6: The TensorFold launch plan and command

**Files:**
- Create: `crates/capyctl-adapters/src/tensorfold/mod.rs`, `tensorfold/args.rs`, `tensorfold/frozen.rs`
- Modify: `crates/capyctl-adapters/src/lib.rs` (`pub mod tensorfold;`)
- Test: `crates/capyctl-adapters/tests/tensorfold_args.rs`

**Interfaces:**
- Consumes: `TensorfoldLaunchSettings` (Task 2), `engine_env::{tool_path, build_overrides, SYSTEM_PATH}` (Task 4), `EffectiveDeployment::cuda_namespace`, `capyctl_config::effective::{derived_initialize_ms, TimeoutSource}`.
- Produces:
  - `capyctl_adapters::tensorfold::PlanInputTensorfold` (fields below, `Default`, redacting `Debug` not needed: it holds no key).
  - `render_command(&PlanInputTensorfold) -> Result<RenderedCommand, TensorfoldArgsError>`.
  - `engine_environment(rendered: &BTreeMap<String,String>, plan: &PlanInputTensorfold, inherited: &dyn Fn(&str) -> Option<String>, toolchain: &BTreeMap<String,String>) -> BTreeMap<String,String>`; `ENGINE_ENV_ALLOWLIST`.
  - `plan_from_effective(effective: &EffectiveDeployment, port: u16, engine_log: String, extensions_dir: Option<String>) -> Result<PlanInputTensorfold, TensorfoldPlanError>`.

- [ ] **Step 1: Write the failing tests.** Create `crates/capyctl-adapters/tests/tensorfold_args.rs`:

```rust
//! ADR 0023 §3: the TensorFold command and its closed environment. CPU tests
//! only; they prove the rendering, not that TensorFold starts.
use capyctl_adapters::tensorfold::{engine_environment, render_command, PlanInputTensorfold};
use std::collections::BTreeMap;

fn plan() -> PlanInputTensorfold {
    PlanInputTensorfold {
        engine_bin: "/opt/tf/bin/tensorfold".into(),
        engine_path_extra: Some("/opt/tf/bin".into()),
        model_path: "/srv/models/nemotron".into(),
        served_model_name: "nemotron".into(),
        port: 8101,
        context_length: 32768,
        extensions_dir: Some("/var/lib/capyctl/engines/tensorfold/0.6.0/torch_extensions".into()),
        engine_log: Some("/var/lib/capyctl/logs/i.log".into()),
        ..PlanInputTensorfold::default()
    }
}

// T41 T14: the spec's command line, rulings 4's fixed flags, in order.
#[test]
fn the_command_is_serve_with_the_reserved_settings_rendered() {
    let cmd = render_command(&plan()).unwrap();
    assert_eq!(
        cmd.argv,
        [
            "/opt/tf/bin/tensorfold", "serve", "/srv/models/nemotron",
            "--name", "nemotron", "--host", "127.0.0.1", "--port", "8101",
            "--no-update-check", "--backend", "cuda", "--snapshot-dir", "none",
            "--context", "32768", "--drafter", "none",
        ]
    );
    assert_eq!(cmd.env["TENSORFOLD_NO_UPDATE_CHECK"], "1");
    assert_eq!(
        cmd.env["TORCH_EXTENSIONS_DIR"],
        "/var/lib/capyctl/engines/tensorfold/0.6.0/torch_extensions"
    );
    assert_eq!(cmd.env["HF_HUB_OFFLINE"], "1");
    assert!(!cmd.argv.iter().any(|a| a.contains("key")));
}

// T41 T14: typed fields render in TensorFold's spelling; then host-fixed
// arguments, then extra arguments.
#[test]
fn typed_fields_host_args_and_extras_render_in_order() {
    let mut input = plan();
    input.kv_dtype = Some("int8".into());
    input.max_tokens = Some(1024);
    input.thinking = Some(false);
    input.engine_args = vec!["--parallel".into(), "2".into()];
    // Ruling 7: an approved `--drafter` extra replaces capyctl's `--drafter none`.
    input.extra_args = vec!["--vision".into(), "--drafter".into(), "/srv/drafters/d".into()];
    let argv = render_command(&input).unwrap().argv;
    // argv[0..14] is the fixed head (binary through `--snapshot-dir none`).
    let tail: Vec<&str> = argv[14..].iter().map(String::as_str).collect();
    assert_eq!(
        tail,
        [
            "--context", "32768", "--kv-dtype", "int8", "--max-tokens", "1024",
            "--no-thinking", "--parallel", "2", "--vision", "--drafter", "/srv/drafters/d",
        ]
    );
    input.thinking = Some(true);
    assert!(render_command(&input).unwrap().argv.contains(&"--thinking".to_string()));
}

// T41 T14: a reserved or typed name in the pass-through vector is refused
// again at render time, abbreviations included.
#[test]
fn reserved_names_are_refused_again_when_rendering() {
    for extra in [vec!["--host", "0.0.0.0"], vec!["--snap", "/x"], vec!["--kv-dtype", "int4"], vec!["--capyctl-x"]] {
        let mut input = plan();
        input.extra_args = extra.iter().map(|s| s.to_string()).collect();
        assert!(render_command(&input).is_err(), "{extra:?}");
    }
    let mut input = plan();
    input.context_length = 0;
    assert!(render_command(&input).is_err(), "a context is required");
}

// T41 T37: the environment is a closed allowlist with the closed PATH.
#[test]
fn the_engine_environment_is_closed() {
    let mut input = plan();
    input.cuda_home = Some("/usr/local/cuda".into());
    let rendered = render_command(&input).unwrap().env;
    let inherited = |name: &str| match name {
        "HOME" => Some("/home/svc".to_string()),
        "CUDA_VISIBLE_DEVICES" => Some("0".to_string()),
        "LD_PRELOAD" | "PYTHONPATH" | "HF_TOKEN" => Some("bad".to_string()),
        _ => None,
    };
    let toolchain = BTreeMap::from([("CUDA_HOME".to_string(), "/usr/local/cuda".to_string()),
        ("MAX_JOBS".to_string(), "4".to_string())]);
    let env = engine_environment(&rendered, &input, &inherited, &toolchain);
    assert_eq!(env["PATH"], "/opt/tf/bin:/usr/local/cuda/bin:/usr/local/bin:/usr/bin:/bin");
    assert_eq!(env["HOME"], "/home/svc");
    assert_eq!(env["CUDA_VISIBLE_DEVICES"], "0");
    assert_eq!(env["MAX_JOBS"], "4");
    for absent in ["LD_PRELOAD", "PYTHONPATH", "HF_TOKEN", "VLLM_API_KEY"] {
        assert!(!env.contains_key(absent), "{absent}");
    }
    assert_eq!(env["CAPYCTL_ENGINE_LOG"], "/var/lib/capyctl/logs/i.log");
}
```

Add a `plan_from_effective` test in the same file that resolves the Task 2 fixture through `capyctl_config::effective::resolve_effective` (copy `fixture()` from `crates/capyctl-config/tests/tensorfold.rs`, reading the JSON with `include_str!("../../capyctl-config/tests/fixtures/f2-deployment.json")`) and asserts `engine_bin == "/opt/tf/bin/tensorfold"`, `served_model_name == "toy"`, `context_length == 8192`, `warm_startup_ms == capyctl_config::effective::derived_initialize_ms(None)`; tag it `// T41`. Add a second `// T41 T37` test: with `security.approved_options: ["--drafter"]`, `security.approved_paths: [<tmp>/drafters]`, `accept_extra_args: true` and `extra_args: ["--drafter", "<tmp>/drafters/d"]`, `plan_from_effective` succeeds when `<tmp>/drafters/d` is a real directory and fails with `PathNotApproved` when it is a symlink to a directory outside `<tmp>/drafters` (the launch-time check through symlinks that `runtime/extra_args_policy.py` does for vLLM and SGLang).

- [ ] **Step 2: Run them to see them fail.**

Run: `cargo test -p capyctl-adapters --test tensorfold_args --locked`
Expected: FAIL to compile (`capyctl_adapters::tensorfold` missing).

- [ ] **Step 3: The module root.** Create `crates/capyctl-adapters/src/tensorfold/mod.rs`:

```rust
//! TensorFold adapter module (ADR 0023). Engine-specific endpoints and launch
//! parameters for TensorFold live here and only here (SPEC §9).
pub mod adapter;
pub mod args;
mod frozen;
pub mod http;
mod initialize;

pub use adapter::TensorfoldAdapter;
pub use args::{engine_environment, render_command, PlanInputTensorfold, TensorfoldArgsError, ENGINE_ENV_ALLOWLIST};
pub use frozen::{plan_from_effective, TensorfoldPlanError};
pub use http::HealthReport;

/// ADR 0023 §6: the longest the process owner waits for TensorFold's own
/// counters to read idle before a stop signal.
pub const ENGINE_IDLE_BOUND: std::time::Duration = std::time::Duration::from_secs(30);
```

Until Task 7, keep `adapter`, `http` and `initialize` as empty files with a module doc line so the crate compiles; do not export their symbols until Task 7 (comment out those `pub use` lines and restore them there).

- [ ] **Step 4: Rendering.** Create `crates/capyctl-adapters/src/tensorfold/args.rs`:

```rust
//! ADR 0023 §3: one TensorFold `serve` command from a resolved plan. No shell,
//! no key: TensorFold has none, and capyctl's routed path is the only way in.
use std::collections::BTreeMap;

use capyctl_config::engine_policy::{normalize_option_name, validate_rendered_args, Engine, ProfileArgError};

use crate::traits::RenderedCommand;

/// Every variable a TensorFold engine may start with (SPEC §13.3 / T21).
pub const ENGINE_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "CUDA_VISIBLE_DEVICES",
    "CUDA_DEVICE_ORDER",
    "HF_HUB_OFFLINE",
    "TRANSFORMERS_OFFLINE",
    "TENSORFOLD_NO_UPDATE_CHECK",
    "TORCH_EXTENSIONS_DIR",
    "CAPYCTL_ENGINE_LOG",
    "CUDA_HOME",
    "MAX_JOBS",
    "FLASHINFER_NVCC_THREADS",
];
/// Variables taken from the agent's own environment.
const PASS_THROUGH: &[&str] = &["HOME", "CUDA_VISIBLE_DEVICES"];

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlanInputTensorfold {
    pub engine_bin: String,
    pub engine_path_extra: Option<String>,
    pub cuda_home: Option<String>,
    pub build_env: BTreeMap<String, String>,
    pub model_path: String,
    pub served_model_name: String,
    pub port: u16,
    /// ADR 0023 §4: required; fixes TensorFold's KV allocation.
    pub context_length: u32,
    pub kv_dtype: Option<String>,
    pub max_tokens: Option<u32>,
    pub thinking: Option<bool>,
    pub engine_args: Vec<String>,
    pub extra_args: Vec<String>,
    pub extensions_dir: Option<String>,
    pub engine_log: Option<String>,
    pub cuda_namespace: Option<capyctl_config::effective::CudaNamespace>,
    /// ADR 0023 §4 (ruling 8): the bound a launch with an existing build
    /// gives up at, in milliseconds.
    pub warm_startup_ms: i64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TensorfoldArgsError {
    #[error("reserved option {0} conflicts with a capyctl-owned setting")]
    Reserved(String),
    #[error("duplicate option {0}")]
    Duplicate(String),
    #[error("malformed engine arguments: {0}")]
    Malformed(String),
    #[error("a TensorFold launch needs a context length")]
    NoContext,
}

/// The typed flags, in TensorFold's own spelling (`tensorfold/cli_args.py`).
fn typed_args(input: &PlanInputTensorfold) -> Vec<String> {
    let mut argv = vec![
        "--context".to_owned(),
        input.context_length.to_string(),
    ];
    if let Some(dtype) = &input.kv_dtype {
        argv.extend(["--kv-dtype".to_owned(), dtype.clone()]);
    }
    if let Some(tokens) = input.max_tokens {
        argv.extend(["--max-tokens".to_owned(), tokens.to_string()]);
    }
    match input.thinking {
        Some(true) => argv.push("--thinking".to_owned()),
        Some(false) => argv.push("--no-thinking".to_owned()),
        None => {}
    }
    argv
}

pub fn render_command(input: &PlanInputTensorfold) -> Result<RenderedCommand, TensorfoldArgsError> {
    if input.context_length == 0 {
        return Err(TensorfoldArgsError::NoContext);
    }
    // ADR 0023 §3 (ruling 6): no protected entry, so the shared policy's
    // prefix rule is the second check of the complete pass-through vector.
    let pass_through: Vec<String> = input.engine_args.iter().chain(&input.extra_args).cloned().collect();
    validate_rendered_args(Engine::Tensorfold, &pass_through, false).map_err(|error| match error {
        ProfileArgError::Reserved(name) | ProfileArgError::ConfigFile(name) => TensorfoldArgsError::Reserved(name),
        ProfileArgError::Duplicate(name) => TensorfoldArgsError::Duplicate(name),
        other => TensorfoldArgsError::Malformed(other.to_string()),
    })?;
    let typed = typed_args(input);
    let typed_names: Vec<String> = typed
        .iter()
        .filter(|t| t.starts_with("--"))
        .map(|t| normalize_option_name(t).trim_start_matches("--no-").to_owned())
        .collect();
    for name in pass_through.iter().filter(|a| a.starts_with("--")).map(|a| normalize_option_name(a)) {
        let base = name.strip_prefix("--no-").map_or(name.clone(), |rest| format!("--{rest}"));
        if typed_names.iter().any(|t| t == &base || base.starts_with(t.as_str()) && t.len() > 2) {
            return Err(TensorfoldArgsError::Duplicate(name));
        }
    }
    let mut argv = vec![
        input.engine_bin.clone(),
        "serve".into(),
        input.model_path.clone(),
        // ADR 0023 §3: capyctl owns the served name and the loopback listener.
        "--name".into(),
        input.served_model_name.clone(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        input.port.to_string(),
        "--no-update-check".into(),
        // Ruling 4: NVIDIA only; prefix snapshots stay in memory.
        "--backend".into(),
        "cuda".into(),
        "--snapshot-dir".into(),
        "none".into(),
    ];
    argv.extend(typed);
    // Ruling 4, 7: TensorFold's default `--drafter auto` would read the
    // Hugging Face cache; without an approved `--drafter` extra it is `none`.
    let names_drafter = pass_through
        .iter()
        .any(|a| normalize_option_name(a) == "--drafter");
    if !names_drafter {
        argv.extend(["--drafter".to_owned(), "none".to_owned()]);
    }
    argv.extend(pass_through);
    let mut env = BTreeMap::new();
    env.insert("TENSORFOLD_NO_UPDATE_CHECK".into(), "1".into());
    // Ruling 4: the engine fetches nothing; capyctl's model store does.
    env.insert("HF_HUB_OFFLINE".into(), "1".into());
    env.insert("TRANSFORMERS_OFFLINE".into(), "1".into());
    if let Some(dir) = &input.extensions_dir {
        env.insert("TORCH_EXTENSIONS_DIR".into(), dir.clone());
    }
    if let Some(namespace) = &input.cuda_namespace {
        for (name, value) in namespace.environment() {
            env.insert(name.into(), value);
        }
    }
    Ok(RenderedCommand { argv, env })
}

/// The closed environment one launch starts with (SPEC §13.3): the rendered
/// variables, the closed PATH, the toolchain limits, a few pass-throughs.
pub fn engine_environment(
    rendered: &BTreeMap<String, String>,
    plan: &PlanInputTensorfold,
    inherited: &dyn Fn(&str) -> Option<String>,
    toolchain: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for name in PASS_THROUGH {
        if let Some(value) = inherited(name) {
            env.insert((*name).to_owned(), value);
        }
    }
    env.extend(rendered.iter().map(|(k, v)| (k.clone(), v.clone())));
    env.insert(
        "PATH".into(),
        crate::engine_env::tool_path(
            plan.engine_path_extra.as_deref(),
            plan.cuda_home.as_deref(),
            crate::engine_env::SYSTEM_PATH,
        ),
    );
    env.extend(toolchain.iter().map(|(k, v)| (k.clone(), v.clone())));
    if let Some(log) = &plan.engine_log {
        env.insert("CAPYCTL_ENGINE_LOG".into(), log.clone());
    }
    env.retain(|name, _| ENGINE_ENV_ALLOWLIST.contains(&name.as_str()));
    env
}
```

The duplicate loop above is the reviewer's to simplify; its contract is the test: a pass-through option naming a typed flag, or its `--no-` form, or an abbreviation of it, is a duplicate. Prefer `validate_extra_args`-style reuse if `engine_policy` exposes the typed match (`typed_option` is private; making a `pub fn typed_option_of(engine, name) -> Option<(&str, &str)>` wrapper is acceptable and shorter).

- [ ] **Step 5: The builder.** Create `crates/capyctl-adapters/src/tensorfold/frozen.rs`:

```rust
//! ADR 0023 §3: the one TensorFold launch builder the host agent and the
//! embedded coordinator both use (Spec §3: the paths cannot drift).
use std::path::Path;

use capyctl_config::effective::{derived_initialize_ms, EffectiveDeployment, TimeoutSource};
use capyctl_domain::launch::LaunchSettings;

use super::args::PlanInputTensorfold;

#[derive(Debug, thiserror::Error)]
pub enum TensorfoldPlanError {
    #[error("the frozen profile declares TensorFold but carries another family's launch settings")]
    OtherFamily,
    #[error("the deployment serves no route")]
    NoRoute,
    #[error("{0}")]
    Unresolved(String),
    #[error("a TensorFold deployment states context_length")]
    NoContext,
    #[error("the selected GPU cannot be pinned")]
    UnpinnableDevice,
    /// ADR 0014 §8, ruling 7: a path option's value resolves outside the
    /// approved paths through a symlink (the launch-time half of the check).
    #[error("{0} names a path outside the approved paths")]
    PathNotApproved(String),
}

pub fn plan_from_effective(
    effective: &EffectiveDeployment,
    port: u16,
    engine_log: String,
    extensions_dir: Option<String>,
) -> Result<PlanInputTensorfold, TensorfoldPlanError> {
    let profile = &effective.profile;
    let LaunchSettings::Tensorfold(settings) = &effective.engine_config else {
        return Err(TensorfoldPlanError::OtherFamily);
    };
    let served_model_name = effective.routes.first().cloned().ok_or(TensorfoldPlanError::NoRoute)?;
    let model_path = effective
        .model
        .require_resolved_path()
        .map_err(|error| TensorfoldPlanError::Unresolved(error.to_string()))?
        .to_owned();
    // Ruling 8: a declared Initialize timeout bounds every launch; a derived
    // one is the ordinary bound once a build exists.
    let warm_startup_ms = match effective.timeouts.provenance.get("initialize") {
        Some(TimeoutSource::Declared) => effective.timeouts.initialize_ms,
        _ => derived_initialize_ms(settings.memory.weights_bytes).min(effective.request_deadline_ms),
    };
    Ok(PlanInputTensorfold {
        engine_bin: profile.executable.clone(),
        engine_path_extra: Path::new(&profile.executable)
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(|parent| parent.to_string_lossy().into_owned()),
        cuda_home: profile.cuda_home.clone(),
        build_env: crate::engine_env::build_overrides(&profile.env),
        model_path,
        served_model_name,
        port,
        context_length: settings.common.context_length.ok_or(TensorfoldPlanError::NoContext)?,
        kv_dtype: settings.common.kv_cache_dtype.clone(),
        max_tokens: settings.max_tokens,
        thinking: settings.thinking,
        engine_args: profile.args.clone(),
        extra_args: settings.extra_args.clone(),
        extensions_dir,
        engine_log: Some(engine_log),
        cuda_namespace: effective.cuda_namespace().map_err(|_| TensorfoldPlanError::UnpinnableDevice)?,
        warm_startup_ms,
    })
}
```

Before the `Ok(PlanInputTensorfold { .. })`, add the launch-time path check (TensorFold has no protected entry, so this is where `runtime/extra_args_policy.py`'s realpath check happens for it):

```rust
    // ADR 0014 §8, ruling 7: every path option among the extras (the drafter
    // included) must still lie inside an approved path once symlinks resolve.
    let approved: Vec<std::path::PathBuf> = profile
        .security
        .approved_paths
        .iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .collect();
    let options = capyctl_config::engine_policy::parse_options(&settings.extra_args)
        .map_err(|error| TensorfoldPlanError::Unresolved(error.to_string()))?;
    for option in options {
        if let Some(capyctl_config::engine_policy::Sensitivity::Path { .. }) =
            capyctl_config::engine_policy::sensitivity(capyctl_config::engine_policy::Engine::Tensorfold, &option.name)
        {
            let value = option.value.unwrap_or_default();
            let inside = std::fs::canonicalize(&value)
                .is_ok_and(|real| approved.iter().any(|root| real.starts_with(root)));
            if !inside {
                return Err(TensorfoldPlanError::PathNotApproved(option.name));
            }
        }
    }
```

Export `derived_initialize_ms` and `TimeoutSource` from `capyctl_config::effective` if they are not already (`grep -n "pub use timeouts" crates/capyctl-config/src/effective.rs`).

- [ ] **Step 6: Run the tests to see them pass.**

Run: `cargo test -p capyctl-adapters --test tensorfold_args --locked`
Expected: PASS, 5 tests.

- [ ] **Step 7: Verify and commit.** Run the Global Constraints verification commands.

```bash
git add crates/capyctl-adapters/src/lib.rs crates/capyctl-adapters/src/tensorfold crates/capyctl-adapters/tests/tensorfold_args.rs
git commit -m "feat: render the TensorFold serve command from the resolved plan"
```

---

### Task 7: The TensorFold adapter: readiness, work and Initialize

**Files:**
- Modify: `crates/capyctl-adapters/src/tensorfold/http.rs`, `adapter.rs`, `initialize.rs` (fill), `mod.rs` (restore the `pub use` lines)
- Modify: `crates/capyctl-adapters/src/traits.rs` (`EngineAdapter::idle_before_signal`, default `None`)
- Modify: `crates/capyctl-adapters/src/resolve.rs` (`AdapterSpec::Tensorfold`, `engine`, `resolve`)
- Modify: `crates/capyctl-controller/src/installation_gate.rs` (`impl EngineAdapter for InstallationGate`, line 231) and `crates/capyctl-controller/src/checkpoint_digests.rs` (`impl EngineAdapter for CheckpointGate`, line 706): forward `idle_before_signal` to `self.inner`
- Test: `crates/capyctl-adapters/tests/tensorfold_adapter.rs`, `crates/capyctl-adapters/src/resolve/tests.rs`

**Interfaces:**
- Consumes: `PlanInputTensorfold`, `render_command`, `engine_environment` (Task 6), `crate::forward::engine_forwarder`, `crate::launch_failure::summary`.
- Produces:
  - `TensorfoldAdapter::new(endpoint: reqwest::Url, fingerprint: String, model_id: String) -> Self`; `.with_launch(PlanInputTensorfold)`, `.with_tools(Arc<dyn OwnedProcessLaunch>)`, `.with_extensions_built(bool)`; `async fn health(&self) -> Result<HealthReport, AdapterError>`.
  - `HealthReport { pub ok: bool, pub busy: bool, pub requests_running: u64 }`, `HealthReport::idle(&self) -> Option<bool>` (`None` when the counters disagree).
  - `EngineAdapter::idle_before_signal(&self, member: &MemberRef) -> Option<bool>` (default `None`).
  - `AdapterSpec::Tensorfold { endpoint: reqwest::Url, fingerprint: String, model_id: String, launch: Option<PlanInputTensorfold>, extensions_built: bool }`.

- [ ] **Step 1: Write the failing tests.** Create `crates/capyctl-adapters/tests/tensorfold_adapter.rs`. Copy, unchanged, these items from `crates/capyctl-adapters/tests/vllm_initialize.rs`: the imports, `ScriptedTool` with its `impl OwnedProcessLaunch`, `api_identity`, `url`, `now_ms`, `free_port`; then:

```rust
#[derive(Clone)]
struct Stub {
    model: String,
    ready_after: usize,
    polls: Arc<AtomicUsize>,
    running: Arc<AtomicUsize>,
    /// `busy` reported independently of `requests_running`, to make them disagree.
    busy_override: Arc<Mutex<Option<bool>>>,
    reasoning_only: bool,
}

async fn health(State(stub): State<Stub>) -> axum::response::Response {
    let seen = stub.polls.fetch_add(1, Ordering::SeqCst);
    if seen < stub.ready_after {
        // TensorFold answers /health only after the model is loaded; before
        // that the port is not listening at all, which a 503 stands in for.
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let running = stub.running.load(Ordering::SeqCst);
    let busy = stub.busy_override.lock().unwrap().unwrap_or(running > 0);
    Json(json!({"ok": true, "backend": "tensorfold", "busy": busy, "requests_running": running})).into_response()
}

async fn models(State(stub): State<Stub>) -> Json<Value> {
    Json(json!({"object": "list", "data": [{"id": stub.model, "object": "model", "owned_by": "tensorfold"}]}))
}

async fn chat(State(stub): State<Stub>, Json(body): Json<Value>) -> axum::response::Response {
    assert_eq!(body["model"], stub.model, "the forwarded request keeps the served name");
    let delta = if stub.reasoning_only {
        json!({"reasoning_content": "The user wants"})
    } else {
        json!({"content": "Ready."})
    };
    let chunk = |delta: Value, finish: Value| json!({"id": "chatcmpl-1", "object": "chat.completion.chunk",
        "created": 1, "model": stub.model, "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]});
    let mut end = chunk(json!({}), json!("length"));
    end["tensorfold"] = json!({"drafted": 3, "accepted": 2});
    end["usage"] = json!({"prompt_tokens": 3, "completion_tokens": 8, "total_tokens": 11});
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(json!({"role": "assistant"}), Value::Null), chunk(delta, Value::Null), end
    );
    ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

async fn stub_engine(model: &str, ready_after: usize, reasoning_only: bool) -> (Stub, u16) {
    let stub = Stub {
        model: model.into(),
        ready_after,
        polls: Arc::default(),
        running: Arc::default(),
        busy_override: Arc::default(),
        reasoning_only,
    };
    let app = axum::Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .with_state(stub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (stub, port)
}

fn plan(port: u16, warm_startup_ms: i64) -> PlanInputTensorfold {
    PlanInputTensorfold {
        engine_bin: "/opt/tf/bin/tensorfold".into(),
        engine_path_extra: Some("/opt/tf/bin".into()),
        model_path: "/srv/models/nemotron".into(),
        served_model_name: "nemotron".into(),
        port,
        context_length: 8192,
        warm_startup_ms,
        ..PlanInputTensorfold::default()
    }
}

fn initialize_command(deadline_in_ms: i64) -> RuntimeCommand {
    // As in vllm_initialize.rs, with TensorFold settings.
    RuntimeCommand {
        action: RuntimeAction::Initialize,
        context: StepExecutionContext {
            token: TransitionToken {
                deployment_id: "d-1".into(),
                revision: 1,
                generation: 1,
                operation_id: "o-1".into(),
                step_id: "s-1".into(),
            },
            binding_id: "b-1".into(),
            incarnation: "i-1".into(),
            issued_at_ms: now_ms(),
            deadline_ms: now_ms() + deadline_in_ms,
            identities: ExecutionIdentities::OwnedLaunch,
            completion_target: None,
            grant_id: Some("g-1".into()),
            launch_settings: Some(capyctl_testkit::tensorfold_launch_settings()),
        },
    }
}

fn member() -> MemberRef {
    MemberRef { deployment_id: "d-1".into(), member_id: "b-1".into() }
}

// T41 (SPEC §6.1): spawn, wait for /health and the model list, probe, and
// report the single API process; no key reaches the engine.
#[tokio::test]
async fn initialize_waits_for_health_and_reports_one_process() {
    let (stub, port) = stub_engine("nemotron", 3, false).await;
    let tool = Arc::new(ScriptedTool::alive(api_identity(), vec![]));
    let adapter = TensorfoldAdapter::new(url(port), "0.6.0".into(), "nemotron".into())
        .with_launch(plan(port, 60_000))
        .with_tools(tool.clone());
    let observation = adapter.execute_persisted(&initialize_command(30_000)).await.unwrap();
    assert!(stub.polls.load(Ordering::SeqCst) >= 4, "a listening port is not readiness");
    assert_eq!(observation.identities, vec![api_identity()]);
    assert!(observation.facts.contains(&Milestone::ModelUsable));
    let spawned = tool.spawned.lock().unwrap();
    assert_eq!(spawned[0].argv[1], "serve");
    assert!(!spawned[0].env.keys().any(|k| k.contains("KEY")));
}

// T41 (Review Focus 1): reasoning tokens alone are an answer.
#[tokio::test]
async fn a_reasoning_only_probe_answer_is_an_answer() {
    let (_stub, port) = stub_engine("nemotron", 0, true).await;
    let adapter = TensorfoldAdapter::new(url(port), "0.6.0".into(), "nemotron".into())
        .with_launch(plan(port, 60_000))
        .with_tools(Arc::new(ScriptedTool::alive(api_identity(), vec![])));
    adapter.execute_persisted(&initialize_command(30_000)).await.unwrap();
}

// T41 (Review Focus 5): with a build present the launch gives up at the
// ordinary bound; without one it waits for the full deadline.
#[tokio::test]
async fn a_warm_launch_gives_up_at_the_ordinary_bound() {
    let (_stub, port) = stub_engine("nemotron", usize::MAX, false).await;
    let warm = TensorfoldAdapter::new(url(port), "0.6.0".into(), "nemotron".into())
        .with_launch(plan(port, 1_500))
        .with_extensions_built(true)
        .with_tools(Arc::new(ScriptedTool::alive(api_identity(), vec![])));
    let started = Instant::now();
    let error = warm.execute_persisted(&initialize_command(20_000)).await.unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(6), "{:?}", started.elapsed());
    assert!(error.to_string().contains("ordinary startup bound"), "{error}");
    let cold = TensorfoldAdapter::new(url(port), "0.6.0".into(), "nemotron".into())
        .with_launch(plan(port, 1_500))
        .with_extensions_built(false)
        .with_tools(Arc::new(ScriptedTool::alive(api_identity(), vec![])));
    let started = Instant::now();
    let error = cold.execute_persisted(&initialize_command(5_000)).await.unwrap_err();
    assert!(started.elapsed() >= Duration::from_secs(2), "the first build waits for the deadline");
    assert!(error.to_string().contains("deadline"), "{error}");
}

// T41 (spec §5): idle needs requests_running 0 and busy false; disagreement
// is not idle; an unreachable engine is not idle.
#[tokio::test]
async fn idle_before_signal_reads_the_engines_own_counters() {
    let (stub, port) = stub_engine("nemotron", 0, false).await;
    let adapter = TensorfoldAdapter::new(url(port), "0.6.0".into(), "nemotron".into());
    assert_eq!(adapter.idle_before_signal(&member()).await, Some(true));
    stub.running.store(1, Ordering::SeqCst);
    assert_eq!(adapter.idle_before_signal(&member()).await, Some(false));
    stub.running.store(0, Ordering::SeqCst);
    *stub.busy_override.lock().unwrap() = Some(true);
    assert_eq!(adapter.idle_before_signal(&member()).await, Some(false));
    let gone = TensorfoldAdapter::new(url(free_port().await), "0.6.0".into(), "nemotron".into());
    assert_eq!(gone.idle_before_signal(&member()).await, Some(false));
    assert!(!adapter.prepare_park(&member()).await.unwrap().quiescent);
}

// T41 T21: no park, restore or reload path exists.
#[tokio::test]
async fn there_is_no_park_path() {
    let adapter = TensorfoldAdapter::new(url(1), "0.6.0".into(), "nemotron".into());
    assert!(matches!(adapter.park(&member(), ParkLevel::Two).await, Err(AdapterError::UnsupportedCapability)));
    assert!(matches!(adapter.restore(&member()).await, Err(AdapterError::UnsupportedCapability)));
    for action in [RuntimeAction::Park, RuntimeAction::Restore, RuntimeAction::ReloadWeights] {
        let mut command = initialize_command(1_000);
        command.action = action;
        assert_eq!(adapter.execute_persisted(&command).await.unwrap_err(), RuntimeError::Unsupported);
    }
}
```

(`ScriptedTool::alive` with no workers builds a group of the API process only, which is what TensorFold 0.6.0 runs.) In `crates/capyctl-adapters/src/resolve/tests.rs`, add a `// T41` test that `resolve(Engine::Tensorfold, AdapterSpec::Tensorfold { .. }, None)` succeeds and `resolve(Engine::Vllm, AdapterSpec::Tensorfold { .. }, None)` is `Err(RuntimeError::Unsupported)`.

- [ ] **Step 2: Run them to see them fail.**

Run: `cargo test -p capyctl-adapters --test tensorfold_adapter --locked`
Expected: FAIL to compile (`TensorfoldAdapter` missing).

- [ ] **Step 3: The trait hook.** In `crates/capyctl-adapters/src/traits.rs`, add to `EngineAdapter`:

```rust
    /// ADR 0023 §6: the engine's own account of in-flight work, read by the
    /// process owner just before a stop signal. `None`: this engine has no
    /// such account and the router's lease ledger alone decides. `Some(true)`
    /// is idle; `Some(false)` is busy, inconsistent or unreadable.
    async fn idle_before_signal(&self, _member: &MemberRef) -> Option<bool> {
        None
    }
```

Forward it in `InstallationGate` and `CheckpointGate`: `async fn idle_before_signal(&self, member: &MemberRef) -> Option<bool> { self.inner.idle_before_signal(member).await }`.

- [ ] **Step 4: The HTTP reads.** `crates/capyctl-adapters/src/tensorfold/http.rs`:

```rust
//! ADR 0023 §4, §6: TensorFold's `/health` and `/v1/models`, bounded reads.
use std::time::Duration;

use crate::traits::AdapterError;

/// A read of TensorFold's own surfaces must finish within this.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest body read (a model list or a health object is small).
const MAX_BODY: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthReport {
    pub ok: bool,
    pub busy: bool,
    pub requests_running: u64,
}

impl HealthReport {
    /// `Some(true)` idle, `Some(false)` busy, `None` when the two counters
    /// disagree (spec §5: that is not idle either).
    pub fn idle(&self) -> Option<bool> {
        match (self.busy, self.requests_running) {
            (false, 0) => Some(true),
            (true, n) if n > 0 => Some(false),
            _ => None,
        }
    }
}

pub(crate) struct Http {
    base: reqwest::Url,
    client: reqwest::Client,
}

pub(crate) enum Read<T> {
    /// Not listening yet, or answering before the model is loaded.
    NotYet,
    Answer(T),
}

impl Http {
    pub(crate) fn new(base: reqwest::Url) -> Self {
        Self {
            base,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .timeout(READ_TIMEOUT)
                .build()
                .expect("static client configuration"),
        }
    }

    async fn json(&self, path: &str) -> Result<Read<serde_json::Value>, AdapterError> {
        let url = self.base.join(path).map_err(|_| AdapterError::Uncertain("bad engine URL".into()))?;
        let response = match self.client.get(url).send().await {
            Ok(response) => response,
            Err(error) if error.is_connect() => return Ok(Read::NotYet),
            Err(_) => return Err(AdapterError::Uncertain(format!("{path} did not answer"))),
        };
        if response.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE {
            return Ok(Read::NotYet);
        }
        if !response.status().is_success() {
            return Err(AdapterError::Uncertain(format!("{path} answered {}", response.status())));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|_| AdapterError::Uncertain(format!("{path} body unreadable")))?;
        if bytes.len() > MAX_BODY {
            return Err(AdapterError::Uncertain(format!("{path} body too large")));
        }
        serde_json::from_slice(&bytes)
            .map(Read::Answer)
            .map_err(|_| AdapterError::Uncertain(format!("{path} is not JSON")))
    }

    pub(crate) async fn health(&self) -> Result<Read<HealthReport>, AdapterError> {
        Ok(match self.json("/health").await? {
            Read::NotYet => Read::NotYet,
            Read::Answer(body) => Read::Answer(HealthReport {
                ok: body["ok"] == true,
                busy: body["busy"].as_bool().ok_or_else(|| AdapterError::Uncertain("health has no busy".into()))?,
                requests_running: body["requests_running"]
                    .as_u64()
                    .ok_or_else(|| AdapterError::Uncertain("health has no requests_running".into()))?,
            }),
        })
    }

    pub(crate) async fn models(&self) -> Result<Read<Vec<String>>, AdapterError> {
        Ok(match self.json("/v1/models").await? {
            Read::NotYet => Read::NotYet,
            Read::Answer(body) => Read::Answer(
                body["data"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|m| m["id"].as_str().map(str::to_owned))
                    .collect(),
            ),
        })
    }
}
```

- [ ] **Step 5: The adapter.** `crates/capyctl-adapters/src/tensorfold/adapter.rs`:

```rust
//! ADR 0023: the TensorFold adapter. Ready is `/health` `ok` plus the served
//! name listed (SPEC §6.1); there is no park, restore or reload path.
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::http::{Http, Read};
use super::{args::PlanInputTensorfold, HealthReport};
use crate::traits::*;

pub struct TensorfoldAdapter {
    endpoint: reqwest::Url,
    fingerprint: String,
    model_id: String,
    http: Http,
    forward: Arc<dyn ChatForward>,
    launch: Option<PlanInputTensorfold>,
    tools: Option<Arc<dyn OwnedProcessLaunch>>,
    extensions_built: bool,
    claimed: Mutex<Option<(String, String)>>,
}

impl TensorfoldAdapter {
    pub fn new(endpoint: reqwest::Url, fingerprint: String, model_id: String) -> Self {
        Self {
            http: Http::new(endpoint.clone()),
            // ADR 0023 §3: TensorFold has no key; nothing is presented.
            forward: crate::forward::engine_forwarder(endpoint.clone(), model_id.clone(), None),
            endpoint,
            fingerprint,
            model_id,
            launch: None,
            tools: None,
            extensions_built: false,
            claimed: Mutex::new(None),
        }
    }
    pub fn with_launch(mut self, launch: PlanInputTensorfold) -> Self {
        self.launch = Some(launch);
        self
    }
    pub fn with_tools(mut self, tools: Arc<dyn OwnedProcessLaunch>) -> Self {
        self.tools = Some(tools);
        self
    }
    /// ADR 0023 §4: whether `TORCH_EXTENSIONS_DIR` held a build when the
    /// launch was prepared; decides the startup bound.
    pub fn with_extensions_built(mut self, built: bool) -> Self {
        self.extensions_built = built;
        self
    }
    pub async fn health(&self) -> Result<HealthReport, AdapterError> {
        match self.http.health().await? {
            Read::Answer(report) => Ok(report),
            Read::NotYet => Err(AdapterError::Uncertain("TensorFold is not answering /health".into())),
        }
    }
    pub(super) fn launch_parts(&self) -> Result<(PlanInputTensorfold, Arc<dyn OwnedProcessLaunch>), RuntimeError> {
        match (&self.launch, &self.tools) {
            (Some(launch), Some(tools)) => Ok((launch.clone(), tools.clone())),
            _ => Err(RuntimeError::Unsupported),
        }
    }
    pub(super) fn claim_incarnation(&self, binding: &str, incarnation: &str) -> Result<(), RuntimeError> {
        let mut claimed = self.claimed.lock().map_err(|_| RuntimeError::Uncertain("claim poisoned".into()))?;
        if claimed.is_some() {
            return Err(RuntimeError::Unsupported);
        }
        *claimed = Some((binding.to_owned(), incarnation.to_owned()));
        Ok(())
    }
    pub(super) fn extensions_built(&self) -> bool {
        self.extensions_built
    }
    pub(super) fn endpoint(&self) -> &str {
        self.endpoint.as_str()
    }
    pub(super) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

#[async_trait]
impl EngineAdapter for TensorfoldAdapter {
    async fn execute_persisted(
        &self,
        command: &RuntimeCommand,
    ) -> Result<capyctl_domain::completion::EffectObservation, RuntimeError> {
        match command.action {
            RuntimeAction::Initialize => super::initialize::initialize(self, command).await,
            // ADR 0023 §6: no sleep, release or unload API.
            _ => Err(RuntimeError::Unsupported),
        }
    }
    async fn inspect(&self, member: &MemberRef) -> Result<EngineState, AdapterError> {
        let phase = match self.check_readiness(member).await? {
            Readiness::Ready => Phase::Ready,
            Readiness::Initializing => Phase::Startup,
        };
        Ok(EngineState { phase, retained_bytes: 0, build_fingerprint: Some(self.fingerprint.clone()) })
    }
    async fn render_plan(&self, _plan: &PlanInput) -> Result<RenderedCommand, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn check_readiness(&self, _member: &MemberRef) -> Result<Readiness, AdapterError> {
        // SPEC §6.1: liveness of an HTTP server is not model readiness.
        let healthy = matches!(self.http.health().await?, Read::Answer(HealthReport { ok: true, .. }));
        if !healthy {
            return Ok(Readiness::Initializing);
        }
        Ok(match self.http.models().await? {
            Read::Answer(ids) if ids.iter().any(|id| *id == self.model_id) => Readiness::Ready,
            _ => Readiness::Initializing,
        })
    }
    async fn prepare_park(&self, member: &MemberRef) -> Result<Quiescence, AdapterError> {
        Ok(Quiescence { quiescent: self.idle_before_signal(member).await == Some(true) })
    }
    async fn park(&self, _member: &MemberRef, _level: ParkLevel) -> Result<ParkOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn restore(&self, _member: &MemberRef) -> Result<RestoreOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn reload_weights(&self, _member: &MemberRef) -> Result<ReloadOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn observe_work(&self, member: &MemberRef) -> Result<WorkObservation, AdapterError> {
        Ok(match self.idle_before_signal(member).await {
            Some(true) => WorkObservation::Idle,
            _ => WorkObservation::Unknown,
        })
    }
    async fn cancel_work(&self, _member: &MemberRef, _req: &RequestRef, _ack: bool) -> Result<CancellationOutcome, AdapterError> {
        // TensorFold cancels a request when its socket closes; it sends no ack.
        Ok(CancellationOutcome::Uncertain)
    }
    async fn idle_before_signal(&self, _member: &MemberRef) -> Option<bool> {
        // Spec §5: unreadable or inconsistent counters are not idle.
        Some(matches!(self.http.health().await, Ok(Read::Answer(report)) if report.idle() == Some(true)))
    }
}

#[async_trait]
impl ChatForward for TensorfoldAdapter {
    async fn forward_chat_stream_async(&self, body: &serde_json::Value, sink: &mut dyn ChatSink) -> Result<StreamEnded, AdapterError> {
        self.forward.forward_chat_stream_async(body, sink).await
    }
    async fn forward_chat(&self, body: &serde_json::Value) -> Result<serde_json::Value, AdapterError> {
        self.forward.forward_chat(body).await
    }
    async fn forward_chat_observed(&self, body: &serde_json::Value, observer: &mut dyn ChatSink) -> Result<serde_json::Value, AdapterError> {
        self.forward.forward_chat_observed(body, observer).await
    }
    async fn forward_chat_stream(&self, body: &serde_json::Value, on_chunk: &mut (dyn FnMut(String) + Send)) -> Result<StreamEnded, AdapterError> {
        self.forward.forward_chat_stream(body, on_chunk).await
    }
}
```

- [ ] **Step 6: Initialize.** `crates/capyctl-adapters/src/tensorfold/initialize.rs` follows `crates/capyctl-adapters/src/vllm/initialize.rs` step for step; the differences are the whole content of this file:

```rust
//! ADR 0023 §4: the TensorFold Initialize step: render, spawn through the
//! director's tool, wait for `/health` and the model list while watching the
//! process, probe once, and report the API process.
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;

use capyctl_domain::completion::{EffectObservation, ExecutionIdentities, Milestone, Presence};

use super::{adapter::TensorfoldAdapter, args::{engine_environment, render_command}};
use crate::traits::{ChatForward, EngineAdapter, MemberRef, Readiness, RuntimeCommand, RuntimeError};
use crate::vllm::args::redact_text;

const READINESS_POLL: Duration = Duration::from_millis(500);
const BUILDER_MARGIN_MS: i64 = 2_000;

fn now_ms() -> Result<i64, RuntimeError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| RuntimeError::Uncertain("system clock is before the epoch".into()))
        .map(|d| d.as_millis() as i64)
}

/// SPEC §6.1, ruling 11: a probe answer is non-empty content or reasoning.
pub(crate) fn answered(answer: &serde_json::Value) -> bool {
    let message = &answer["choices"][0]["message"];
    ["content", "reasoning_content"]
        .iter()
        .any(|field| message[*field].as_str().is_some_and(|text| !text.is_empty()))
}

pub(super) async fn initialize(adapter: &TensorfoldAdapter, command: &RuntimeCommand) -> Result<EffectObservation, RuntimeError> {
    let context = &command.context;
    let (plan, tools) = adapter.launch_parts()?;
    if !matches!(context.launch_settings, Some(capyctl_domain::launch::LaunchSettings::Tensorfold(_)))
        || !matches!(context.identities, ExecutionIdentities::OwnedLaunch)
    {
        return Err(RuntimeError::Unsupported);
    }
    adapter.claim_incarnation(&context.binding_id, &context.incarnation)?;
    let mut cmd = render_command(&plan).map_err(|e| RuntimeError::Uncertain(format!("render: {e}")))?;
    let (toolchain, limits) = crate::engine_env::toolchain_environment(
        plan.cuda_home.as_deref(),
        &plan.build_env,
        crate::engine_env::mem_available_bytes(),
        crate::engine_env::cpu_count(),
    );
    capyctl_domain::role_log::notice(
        capyctl_domain::role_log::Level::Notice,
        &format!("{limits} (binding {})", context.binding_id),
    );
    cmd.env = engine_environment(&cmd.env, &plan, &|name| std::env::var(name).ok(), &toolchain);
    let incarnation = context.incarnation.clone();
    let spawn_tools = tools.clone();
    let api = tokio::task::spawn_blocking(move || spawn_tools.spawn_durable(&incarnation, &cmd))
        .await
        .map_err(|_| RuntimeError::Uncertain("spawn task failed".into()))??;
    // ADR 0023 §4 (ruling 8): the ordinary bound once a build exists, the
    // whole (first-build) deadline otherwise.
    let deadline = context.deadline_ms.saturating_sub(BUILDER_MARGIN_MS);
    let warm = adapter.extensions_built();
    let stop_at = if warm { deadline.min(now_ms()?.saturating_add(plan.warm_startup_ms)) } else { deadline };
    let member = MemberRef { deployment_id: context.token.deployment_id.clone(), member_id: context.binding_id.clone() };
    loop {
        match adapter.check_readiness(&member).await {
            Ok(Readiness::Ready) => break,
            Ok(Readiness::Initializing) => {}
            Err(e) => return Err(RuntimeError::Uncertain(redact_text(&format!("readiness: {e}")))),
        }
        let presence_tools = tools.clone();
        let watched = api.clone();
        match tokio::task::spawn_blocking(move || presence_tools.present(&watched))
            .await
            .map_err(|_| RuntimeError::Uncertain("presence task failed".into()))?
        {
            Presence::Alive => {}
            Presence::Gone => {
                let tail = crate::launch_failure::log_tail(plan.engine_log.as_deref());
                return Err(RuntimeError::LaunchFailed(format!(
                    "{}; log tail:\n{tail}",
                    crate::launch_failure::summary(&tail, None)
                )));
            }
            Presence::Unknown => {
                return Err(RuntimeError::Uncertain("engine presence could not be established during readiness".into()))
            }
        }
        if now_ms()? >= stop_at {
            return Err(RuntimeError::Uncertain(if warm && stop_at < deadline {
                "the ordinary startup bound passed with the engine alive (its kernels were already built)".into()
            } else {
                "readiness deadline reached with the engine alive (the first start builds CUDA kernels)".into()
            }));
        }
        tokio::time::sleep(READINESS_POLL).await;
    }
    let body = json!({
        "model": plan.served_model_name,
        "messages": [{"role": "user", "content": "Say ready."}],
        "max_tokens": 8,
        "temperature": 0,
    });
    let budget = Duration::from_millis(u64::try_from(deadline - now_ms()?).unwrap_or(0));
    let answer = tokio::time::timeout(budget, adapter.forward_chat(&body))
        .await
        .map_err(|_| RuntimeError::Uncertain("probe deadline reached with the engine alive".into()))?
        .map_err(|e| RuntimeError::Uncertain(redact_text(&format!("engine listed the model but did not answer: {e}"))))?;
    if !answered(&answer) {
        return Err(RuntimeError::Uncertain("engine answered with empty content".into()));
    }
    let group_tools = tools.clone();
    let led_by = api.clone();
    let identities = tokio::task::spawn_blocking(move || group_tools.observe_group(&led_by))
        .await
        .map_err(|_| RuntimeError::Uncertain("group task failed".into()))??;
    // Ruling 13: TensorFold 0.6.0 serves from one process.
    if identities.first().map(|i| i.role.as_str()) != Some("api") {
        return Err(RuntimeError::Uncertain("engine group has no API process".into()));
    }
    Ok(EffectObservation {
        token: context.token.clone(),
        binding_id: context.binding_id.clone(),
        incarnation: context.incarnation.clone(),
        identities,
        observed_at_ms: now_ms()?,
        receipt: format!("tensorfold {} ready on {}; probe answered", adapter.fingerprint(), adapter.endpoint()),
        facts: vec![Milestone::AllocationsRestored, Milestone::WeightsUsable, Milestone::CacheValid, Milestone::ModelUsable],
    })
}
```

The vLLM file keeps its log-tail helpers private. Move `log_tail` and `read_tail_bytes` from `vllm/initialize.rs` into `crates/capyctl-adapters/src/launch_failure.rs` as `pub fn log_tail(path: Option<&str>) -> String` (20 lines, 64 KiB, redacted, unchanged behaviour) and call it from both files; the vLLM tests must still pass unchanged.

- [ ] **Step 7: The adapter spec.** In `crates/capyctl-adapters/src/resolve.rs`, add the variant with its doc comment ("ADR 0023: TensorFold talks plain HTTP on loopback and has no key; an owned launch carries its plan.") and fields from **Interfaces**; `Self::Tensorfold { .. } => Engine::Tensorfold` in `engine`; and in `resolve`:

```rust
        AdapterSpec::Tensorfold { endpoint, fingerprint, model_id, launch, extensions_built } => {
            let mut adapter = crate::tensorfold::TensorfoldAdapter::new(endpoint, fingerprint, model_id)
                .with_extensions_built(extensions_built);
            if let Some(launch) = launch {
                adapter = adapter.with_launch(launch);
            }
            if let Some(tools) = tools {
                adapter = adapter.with_tools(tools);
            }
            Box::new(adapter)
        }
```

Add `AdapterSpec::Tensorfold { .. } => {}` to the key-sealing match in `crates/capyctl-controller/src/coordinator/worker.rs` only if its `_ => {}` arm is missing (it is present today) and the `match &mut spec` in `crates/capyctl-controller/src/coordinator/local_adoption.rs` (line 73): `AdapterSpec::Tensorfold { .. } => {}` with `// ADR 0023 §3: TensorFold holds no key to recover.`

- [ ] **Step 8: Run the tests to see them pass.**

Run: `cargo test -p capyctl-adapters --all-targets --locked`
Expected: PASS, including `vllm_initialize` (log tail moved).

- [ ] **Step 9: Verify and commit.** Run the Global Constraints verification commands.

```bash
git add crates/capyctl-adapters crates/capyctl-controller/src/installation_gate.rs crates/capyctl-controller/src/checkpoint_digests.rs \
  crates/capyctl-controller/src/coordinator/local_adoption.rs
git commit -m "feat: TensorFold adapter with health readiness and an idle check before stop"
```

---

### Task 8: Forwarding keeps TensorFold's extras; reasoning-only probe answers

**Files:**
- Modify: `crates/capyctl-adapters/src/forward.rs` (`assemble`, line 291; tests module)
- Modify: `crates/capyctl-agent/src/native_execution.rs` (the fresh probe's content check, line 1379)
- Modify: `crates/capyctl-adapters/src/vllm/initialize.rs` and `crates/capyctl-adapters/src/sglang/initialize.rs` (their probe content checks)
- Test: `crates/capyctl-adapters/src/forward.rs` (unit tests), `crates/capyctl-adapters/tests/vllm_initialize.rs`

**Interfaces:**
- Consumes: `crate::tensorfold::initialize::answered` (Task 7) — make it `pub fn probe_answered(answer: &Value) -> bool` in `crate::forward` instead, and have `tensorfold/initialize.rs` call that.
- Produces: `capyctl_adapters::forward::probe_answered(&serde_json::Value) -> bool`.

- [ ] **Step 1: Write the failing tests.** In the unit tests of `crates/capyctl-adapters/src/forward.rs`:

```rust
    // T41 (ADR 0023 §7): the final chunk's `tensorfold` object survives
    // collection, beside usage.
    #[test]
    fn a_collected_response_keeps_the_tensorfold_object() {
        let chunk = |delta: Value, finish: Value| json!({"id": "c", "object": "chat.completion.chunk",
            "created": 1, "model": "m", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}).to_string();
        let mut end: Value = serde_json::from_str(&chunk(json!({}), json!("stop"))).unwrap();
        end["tensorfold"] = json!({"accepted": 2});
        end["usage"] = json!({"total_tokens": 4});
        let response = assemble(vec![chunk(json!({"role": "assistant"}), Value::Null),
            chunk(json!({"content": "hi"}), Value::Null), end.to_string()]).unwrap();
        assert_eq!(response["tensorfold"], json!({"accepted": 2}));
        assert_eq!(response["usage"]["total_tokens"], 4);
        assert_eq!(response["choices"][0]["message"]["content"], "hi");
    }

    // T41 T22 (ruling 11): reasoning alone answers the probe; nothing does not.
    #[test]
    fn a_reasoning_only_answer_answers_the_probe() {
        assert!(probe_answered(&json!({"choices": [{"message": {"content": "", "reasoning_content": "ok"}}]})));
        assert!(probe_answered(&json!({"choices": [{"message": {"content": "Ready."}}]})));
        assert!(!probe_answered(&json!({"choices": [{"message": {"content": ""}}]})));
        assert!(!probe_answered(&json!({})));
    }
```

- [ ] **Step 2: Run them to see them fail.**

Run: `cargo test -p capyctl-adapters --lib forward --locked`
Expected: FAIL: `tensorfold` is `null`; `probe_answered` undefined.

- [ ] **Step 3: Implement.** In `assemble`, beside the `usage` copy:

```rust
        // ADR 0023 §7: TensorFold's statistics ride the final chunk; a
        // collected response carries them as the engine's own would.
        if let Some(stats) = chunk.get("tensorfold").filter(|v| v.is_object()) {
            response["tensorfold"] = stats.clone();
        }
```

and add:

```rust
/// SPEC §6.1 (ruling 11): a readiness probe's answer is non-empty content
/// or, for a model that reasons first, non-empty reasoning.
pub fn probe_answered(answer: &Value) -> bool {
    let message = &answer["choices"][0]["message"];
    ["content", "reasoning_content"]
        .iter()
        .any(|field| message[*field].as_str().is_some_and(|text| !text.is_empty()))
}
```

Replace the three `answer["choices"][0]["message"]["content"].as_str().is_none_or(str::is_empty)` checks (`vllm/initialize.rs`, `sglang/initialize.rs`, `native_execution.rs` fresh probe) with `!capyctl_adapters::forward::probe_answered(&answer)` (crate-local path inside the adapters crate), and make `tensorfold/initialize.rs` use it, deleting its own `answered`. The existing `a_probe_that_is_refused_or_answers_with_nothing_fails_the_step` test in `vllm_initialize.rs` must still pass: its empty answer has no reasoning.

- [ ] **Step 4: Run the tests to see them pass.**

Run: `cargo test -p capyctl-adapters --all-targets --locked && cargo test -p capyctl-agent --lib --locked`
Expected: PASS.

- [ ] **Step 5: Verify and commit.** Run the Global Constraints verification commands.

```bash
git add crates/capyctl-adapters/src crates/capyctl-agent/src/native_execution.rs
git commit -m "fix: keep TensorFold response extras and accept reasoning-only probe answers"
```

---

### Task 9: Remote host execution of TensorFold launches

**Files:**
- Create: `crates/capyctl-agent/src/native_execution/tensorfold.rs`
- Modify: `crates/capyctl-agent/src/native_execution.rs` (`mod tensorfold;`; `PreparedLaunch` line 156; `resolve_as` line 521 and 550; `prepare` 804; `launch_adapter` 821; `probe_adapter` 882; the `MemberAction::Terminate` branch at line 1165)
- Test: unit tests in `crates/capyctl-agent/src/native_execution.rs` (`mod tests`)

**Interfaces:**
- Consumes: `capyctl_adapters::tensorfold::{plan_from_effective, PlanInputTensorfold, TensorfoldAdapter, ENGINE_IDLE_BOUND}`, `EngineCacheRoot` and `has_build` (Task 5).
- Produces: `NativeHostExecution::tensorfold_plan(&self, &EffectiveDeployment, &SingleLaunchPlan) -> Result<(PlanInputTensorfold, bool), JournalError>` (the bool is "a build exists"); `NativeHostExecution::tensorfold_adapter(&self, &EffectiveDeployment, &SingleLaunchPlan, served: &str) -> Result<TensorfoldAdapter, SessionError>`; `NativeHostExecution::tensorfold_idle_before_terminate(&self, owned: &MemberCommand, deadline_ms: i64) -> bool`.

- [ ] **Step 1: Write the failing tests.** In `crates/capyctl-agent/src/native_execution.rs` `mod tests`, add a fixture and tests:

```rust
    /// A TensorFold profile on the lab document, restart_only, with a cache root.
    fn tensorfold_fixture(
        root: &std::path::Path,
        identity_dir: &std::path::Path,
    ) -> (Arc<NativeHostExecution>, serde_json::Value, String) {
        let (executor, mut deployment, _) = sglang_fixture_with(root, identity_dir, "restart_only", |document| {
            let profile = &mut document["runtime_profiles"]["local"];
            profile["engine"] = "tensorfold".into();
            profile["executable"] = "/opt/tf/bin/tensorfold".into();
            profile["build_fingerprint"] = "0.6.0".into();
            profile["args"] = serde_json::json!([]);
            profile["security"]["deep_park"] = "disabled".into();
            profile["security"].as_object_mut().unwrap().remove("admin_credential_ref");
        });
        deployment["engine_config"] = serde_json::json!({"context_length": 8192});
        let engines = root.join("engines");
        std::fs::create_dir(&engines).unwrap();
        std::fs::set_permissions(&engines, std::fs::Permissions::from_mode(0o700)).unwrap();
        let executor = executor.with_engine_cache_root(engines);
        let policy = capyctl_config::remote_resources::policy_fingerprint(
            &executor.profiles.accepted().config.document,
        );
        (executor, deployment, policy)
    }

    // T41 (ADR 0023 §3): the host resolves and prepares a TensorFold launch
    // from its own document; the plan renders with a private extensions
    // directory and no build yet.
    #[test]
    fn a_tensorfold_launch_prepares_from_local_policy() {
        let root = directory();
        let identity_dir = directory();
        let (executor, deployment, policy) = tensorfold_fixture(root.path(), identity_dir.path());
        let (mut launch, plan) = launch_with(&deployment, &policy, "");
        launch.identity.profile_fingerprint = "0.6.0".into();
        launch.identity.payload_digest = launch.canonical_digest();
        let effective = executor.resolve(&launch).unwrap();
        let (input, built) = executor.tensorfold_plan(&effective, &plan).unwrap();
        assert!(!built);
        assert!(input.extensions_dir.unwrap().ends_with("engines/tensorfold/0.6.0/torch_extensions"));
        assert_eq!(input.context_length, 8192);
        assert!(matches!(executor.prepare(&effective, &plan, "toy"), Ok(PreparedLaunch::Tensorfold(..))));
    }

    // T41 T21 (ADR 0023 §6): a Park of a TensorFold launch is refused by its
    // tier before anything is journaled.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tensorfold_park_is_refused_unchanged() {
        let root = directory();
        let identity_dir = directory();
        let (executor, deployment, policy) = tensorfold_fixture(root.path(), identity_dir.path());
        let (mut launch, _) = launch_with(&deployment, &policy, "");
        launch.identity.profile_fingerprint = "0.6.0".into();
        launch.identity.payload_digest = launch.canonical_digest();
        let session = executor.journal.connect().unwrap();
        executor.connected(session).unwrap();
        executor.journal.accept(session, &launch, capyctl_protocol::now_unix_ms(), &Admit).unwrap();
        let mut park = MemberCommand {
            identity: checkpoint_identity("park", "ready"),
            action: MemberAction::Park { owned_handle: "launch".into() },
        };
        park.identity.payload_digest = park.canonical_digest();
        let refused = executor.execute(session, park).await.unwrap();
        assert_eq!(refused.refused, "residency_tier");
        assert_eq!(refused.residency.as_ref().unwrap().state, "unchanged");
    }

    // T41 (spec §5): before Terminate the host waits for TensorFold's own
    // counters; busy at the bound is not idle.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminate_waits_for_tensorfold_to_be_idle() {
        // A stub /health on the launch's service port, idle after two reads.
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = reads.clone();
        let app = axum::Router::new().route("/health", axum::routing::get(move || {
            let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move { axum::Json(serde_json::json!({"ok": true, "busy": n < 2, "requests_running": if n < 2 { 1 } else { 0 }})) }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let root = directory();
        let identity_dir = directory();
        let (executor, deployment, policy) = tensorfold_fixture(root.path(), identity_dir.path());
        let (mut launch, _) = launch_with(&deployment, &policy, "");
        if let MemberAction::LaunchSingle(plan) = &mut launch.action {
            plan.service_port = port;
        }
        launch.identity.profile_fingerprint = "0.6.0".into();
        launch.identity.payload_digest = launch.canonical_digest();
        let deadline = capyctl_protocol::now_unix_ms() + 10_000;
        assert!(executor.tensorfold_idle_before_terminate(&launch, deadline).await);
        assert!(reads.load(std::sync::atomic::Ordering::SeqCst) >= 3);
        let short = capyctl_protocol::now_unix_ms() + 1_200;
        reads.store(0, std::sync::atomic::Ordering::SeqCst);
        assert!(!executor.tensorfold_idle_before_terminate(&launch, short).await);
    }
```

The fixture's `endpoint_port_range` must contain the stub's port for `resolve` to accept it; set `document["resource_policy"]["endpoint_port_range"] = json!({"start": 1024, "end": 65535})` in the fixture edit if the fixture's range is narrower. Add `axum` to `crates/capyctl-agent/Cargo.toml` `[dev-dependencies]` (workspace version) if missing. Use `executor.profiles.accepted()` or whatever accessor the fixture's policy fingerprint needs; the SGLang fixture computes it from `config.document` before construction, so compute it there instead if `profiles` is private to the test.

- [ ] **Step 2: Run them to see them fail.**

Run: `cargo test -p capyctl-agent --lib native_execution::tests::a_tensorfold --locked`
Expected: FAIL to compile (`tensorfold_plan`, `PreparedLaunch::Tensorfold`).

- [ ] **Step 3: The host module.** Create `crates/capyctl-agent/src/native_execution/tensorfold.rs`:

```rust
//! ADR 0023: remote single-rank TensorFold on the host agent. The controller
//! sends a frozen deployment document and a leased port; the agent resolves
//! the launch from its own approved document through the shared builder
//! (`capyctl_adapters::tensorfold::plan_from_effective`), and drains against
//! TensorFold's own counters before any stop signal (spec §5).
use super::NativeHostExecution;
use crate::{journal::JournalError, session::SessionError};
use capyctl_adapters::tensorfold::{plan_from_effective, PlanInputTensorfold, TensorfoldAdapter, ENGINE_IDLE_BOUND};
use capyctl_adapters::traits::{EngineAdapter, MemberRef};
use capyctl_config::effective::EffectiveDeployment;
use capyctl_protocol::execution::{MemberAction, MemberCommand, SingleLaunchPlan};
use std::time::Duration;

/// How often the idle check reads `/health`.
const IDLE_POLL: Duration = Duration::from_millis(250);

impl NativeHostExecution {
    /// The plan, and whether this version's extensions are already built.
    pub(super) fn tensorfold_plan(
        &self,
        effective: &EffectiveDeployment,
        plan: &SingleLaunchPlan,
    ) -> Result<(PlanInputTensorfold, bool), JournalError> {
        // ADR 0023 §3, SPEC §13.3: the build directory is private state; a
        // host without a private root never launches TensorFold.
        let cache = self.engine_cache.as_ref().ok_or(JournalError::Unauthorized)?;
        let dir = cache
            .torch_extensions(&effective.profile.build_fingerprint)
            .map_err(|_| JournalError::Unauthorized)?;
        let built = crate::engine_cache::has_build(&dir);
        let input = plan_from_effective(
            effective,
            plan.service_port,
            self.log_dir.join(format!("{}.log", plan.incarnation)).to_string_lossy().into_owned(),
            Some(dir.to_string_lossy().into_owned()),
        )
        .map_err(|_| JournalError::Unauthorized)?;
        Ok((input, built))
    }

    pub(super) fn tensorfold_adapter(
        &self,
        effective: &EffectiveDeployment,
        plan: &SingleLaunchPlan,
        served: &str,
    ) -> Result<TensorfoldAdapter, SessionError> {
        let endpoint: reqwest::Url = format!("http://{}", Self::endpoint(plan)).parse().map_err(|_| SessionError)?;
        Ok(TensorfoldAdapter::new(endpoint, effective.profile.build_fingerprint.clone(), served.into()))
    }

    /// Spec §5 (ruling 9): `true` once TensorFold reads idle; `false` when it
    /// does not within [`ENGINE_IDLE_BOUND`] or the command's remaining time.
    /// A launch that is not TensorFold's is idle by this check.
    pub(crate) async fn tensorfold_idle_before_terminate(&self, owned: &MemberCommand, deadline_ms: i64) -> bool {
        let MemberAction::LaunchSingle(plan) = &owned.action else {
            return true;
        };
        let Ok(effective) = self.resolve_retained(owned) else {
            return false;
        };
        if effective.profile.engine != capyctl_config::engine_policy::Engine::Tensorfold {
            return true;
        }
        let Some(served) = effective.routes.first().cloned() else {
            return false;
        };
        let Ok(adapter) = self.tensorfold_adapter(&effective, plan, &served) else {
            return false;
        };
        let member = MemberRef { deployment_id: owned.identity.deployment_id.clone(), member_id: plan.binding_id.clone() };
        let remaining = Duration::from_millis(
            u64::try_from(deadline_ms.saturating_sub(capyctl_protocol::now_unix_ms()).saturating_sub(1_000)).unwrap_or(0),
        );
        let until = tokio::time::Instant::now() + ENGINE_IDLE_BOUND.min(remaining);
        loop {
            if adapter.idle_before_signal(&member).await == Some(true) {
                return true;
            }
            if tokio::time::Instant::now() >= until {
                return false;
            }
            tokio::time::sleep(IDLE_POLL).await;
        }
    }
}
```

- [ ] **Step 4: Wire it in.** In `native_execution.rs`:
  - `PreparedLaunch` gains `Tensorfold(Box<(capyctl_adapters::tensorfold::PlanInputTensorfold, bool)>)`.
  - `resolve_as`: change the guard to `matches!(effective.profile.engine, Engine::Sglang | Engine::Vllm | Engine::Tensorfold)`, and after the vLLM admission add `if effective.profile.engine == Engine::Tensorfold { self.tensorfold_plan(&effective, plan)?; }` (authorization-time refusal before anything durable, as `admit_vllm` does).
  - `prepare`: `Engine::Tensorfold => Ok(PreparedLaunch::Tensorfold(Box::new(self.tensorfold_plan(effective, plan).map_err(|_| SessionError)?))),`.
  - `launch_adapter`: `PreparedLaunch::Tensorfold(prepared) => { let (input, built) = *prepared; Box::new(self.tensorfold_adapter(effective, plan, served)?.with_launch(input).with_extensions_built(built).with_tools(tools)) }`.
  - `probe_adapter`: `Engine::Tensorfold => Box::new(self.tensorfold_adapter(effective, plan, served)?),`.
  - `Terminate` branch, after `self.ingress.close(&scope)` and before the `spawn_blocking` of `journal.execute`:

```rust
                    // Spec §5 (ruling 9): TensorFold's own counters must read
                    // idle before the signal; at the bound nothing is sent and
                    // the terminate stays uncertain, accounting retained.
                    if let Ok(owned) = self.journal.retained_command(owned_handle) {
                        if !self
                            .tensorfold_idle_before_terminate(&owned, command.identity.deadline_ms)
                            .await
                        {
                            return Err(SessionError);
                        }
                    }
```

`Err(SessionError)` is the path an unprovable terminate already takes (`JournalError::Uncertain` maps to it at the same call); the controller keeps the reservation and the cleanup uncertain, and its redelivery checks again.

- [ ] **Step 5: Run the tests to see them pass.**

Run: `cargo test -p capyctl-agent --lib native_execution --locked`
Expected: PASS, including the existing vLLM and SGLang tests.

- [ ] **Step 6: Verify and commit.** Run the Global Constraints verification commands.

```bash
git add crates/capyctl-agent/src/native_execution.rs crates/capyctl-agent/src/native_execution/tensorfold.rs crates/capyctl-agent/Cargo.toml
git commit -m "feat: host agent launches TensorFold and drains on its own counters"
```

---

### Task 10: Load and latency reports for TensorFold

**Files:**
- Modify: `crates/capyctl-agent/src/load.rs` (`Family` constants line 44; `parse_engine_load` 151; histogram tables 160; `parse_engine_histograms` 332)
- Modify: `crates/capyctl-protocol/src/reports.rs` (`LATENCY_ENGINES` line 40)
- Test: `crates/capyctl-agent/tests/load.rs`, `crates/capyctl-protocol/src/reports.rs` tests (or `crates/capyctl-protocol/tests/` where latency validation is tested: `grep -rn "LATENCY_ENGINES\|engine: \"sglang\"" crates/capyctl-protocol`)

**Interfaces:**
- Produces: `parse_engine_load` and `parse_engine_histograms` recognize the `tensorfold:` family; latency reports may name `tensorfold`.

- [ ] **Step 1: Write the failing tests.** Append to `crates/capyctl-agent/tests/load.rs`:

```rust
// T41 (ADR 0023 §8): TensorFold 0.6.0's gauges and histograms
// (`tensorfold/server/metrics.py`), one KV series per pool.
#[test]
fn tensorfold_metrics_parse() {
    let body = "\
# TYPE tensorfold:requests_running gauge
tensorfold:requests_running 1
tensorfold:requests_waiting 2
tensorfold:kv_cache_usage_ratio{pool=\"0\"} 0.25
tensorfold:kv_cache_usage_ratio{pool=\"1\"} 0.5
tensorfold:time_to_first_token_seconds_bucket{le=\"0.5\"} 1
tensorfold:time_to_first_token_seconds_bucket{le=\"+Inf\"} 2
tensorfold:time_to_first_token_seconds_sum 1.5
tensorfold:time_to_first_token_seconds_count 2
";
    let load = capyctl_agent::load::parse_engine_load(body).unwrap();
    assert_eq!((load.running, load.waiting), (1, 2));
    assert_eq!(load.kv_usage_ppm, 500_000);
    let (engine, histograms) = capyctl_agent::load::parse_engine_histograms(body).unwrap();
    assert_eq!(engine, "tensorfold");
    assert_eq!(histograms[0].0, "engine_time_to_first_token");
}
```

and a protocol test that a `pb::SampleLatency { engine: "tensorfold".into(), .. }` converts (copy the existing "sglang" latency conversion test next to `LATENCY_ENGINES` and change the engine name; tag `// T41`).

- [ ] **Step 2: Run them to see them fail.**

Run: `cargo test -p capyctl-agent --test load tensorfold --locked; cargo test -p capyctl-protocol --locked`
Expected: FAIL (`None` from `parse_engine_load`; latency engine refused).

- [ ] **Step 3: Implement.** In `load.rs`:

```rust
/// TensorFold 0.6.0 `tensorfold/server/metrics.py`. The KV ratio has one
/// series per stream pool; the most pressured pool counts.
const TENSORFOLD: Family = Family {
    running: "tensorfold:requests_running",
    waiting: "tensorfold:requests_waiting",
    kv_usage: "tensorfold:kv_cache_usage_ratio",
};
/// TensorFold 0.6.0 has no queue, prefill, decode or inter-token histogram.
const TENSORFOLD_HISTOGRAMS: &[(&str, &str)] = &[
    ("engine_time_to_first_token", "tensorfold:time_to_first_token_seconds"),
    ("engine_e2e_request_latency", "tensorfold:request_latency_seconds"),
];

/// The one family whose three gauges parse, with its name and histograms.
fn family_of(text: &str) -> Option<(&'static str, EngineLoad, &'static [(&'static str, &'static str)])> {
    let found: Vec<_> = [("vllm", &VLLM, VLLM_HISTOGRAMS), ("sglang", &SGLANG, SGLANG_HISTOGRAMS), ("tensorfold", &TENSORFOLD, TENSORFOLD_HISTOGRAMS)]
        .into_iter()
        .filter_map(|(name, family, table)| family_load(text, family).map(|load| (name, load, table)))
        .collect();
    match found.as_slice() {
        [one] => Some(*one),
        _ => None,
    }
}

pub fn parse_engine_load(text: &str) -> Option<EngineLoad> {
    family_of(text).map(|(_, load, _)| load)
}
```

and rewrite `parse_engine_histograms` on `family_of` the same way. In `reports.rs`: `pub const LATENCY_ENGINES: &[&str] = &["vllm", "sglang", "tensorfold"];` with `// ADR 0023 §8.`

- [ ] **Step 4: Run the tests to see them pass.**

Run: `cargo test -p capyctl-agent --test load --locked && cargo test -p capyctl-protocol --locked`
Expected: PASS.

- [ ] **Step 5: Verify and commit.** Run the Global Constraints verification commands.

```bash
git add crates/capyctl-agent/src/load.rs crates/capyctl-agent/tests/load.rs crates/capyctl-protocol/src/reports.rs
git commit -m "feat: report TensorFold load gauges and latency histograms"
```

---

### Task 11: Standalone (embedded) execution of TensorFold

**Files:**
- Modify: `crates/capyctl-controller/src/engine_bindings.rs` (`spec`, the `Engine::Tensorfold` arm from Task 2; new `tensorfold_plan`)
- Modify: `crates/capyctl-controller/src/coordinator/worker.rs` (`drive_cleanup`, after `drain_before_terminate(shared, work, stop).await?;` at line 2671)
- Modify: `crates/capyctl-cli/src/roles.rs` (`role_installation_as` and `runtime_dir`: a TensorFold installation needs no `capyctl_vllm_guard.py` check bypass — see Step 4)
- Test: `crates/capyctl-controller/src/engine_bindings/tests.rs`, `crates/capyctl-controller/src/coordinator/tests_cleanup.rs`, `crates/capyctl-cli/tests/standalone_engines.rs`

**Interfaces:**
- Consumes: `capyctl_adapters::tensorfold::plan_from_effective`, `EngineCacheRoot` (Task 5), `EngineAdapter::idle_before_signal` (Task 7).
- Produces: `ProfileBindings::spec` returns `AdapterSpec::Tensorfold { .. }` for a TensorFold profile; the embedded cleanup refuses to signal a TensorFold engine that is not idle.

- [ ] **Step 1: Write the failing tests.** In `crates/capyctl-controller/src/engine_bindings/tests.rs`, following the file's existing vLLM `spec` test (`grep -n "fn .*spec" crates/capyctl-controller/src/engine_bindings/tests.rs`), add:

```rust
// T41 (ADR 0023 §3): the embedded path builds the same TensorFold plan the
// host agent builds, with the private extensions directory, and no key.
#[test]
fn a_tensorfold_profile_builds_a_tensorfold_spec() {
    let work = tensorfold_work(); // the file's vLLM InitializeWork helper, with the Task 2 TensorFold fixture
    let dir = tempfile::tempdir().unwrap();
    let engines = dir.path().join("engines");
    std::fs::create_dir(&engines).unwrap();
    std::fs::set_permissions(&engines, std::fs::Permissions::from_mode(0o700)).unwrap();
    let bindings = ProfileBindings::new(dir.path().join("logs"), dir.path().join("runtime"))
        .with_engine_cache_root(engines);
    let AdapterSpec::Tensorfold { launch: Some(launch), extensions_built, .. } = bindings.spec(&work).unwrap() else {
        panic!("a TensorFold spec");
    };
    assert!(!extensions_built);
    assert!(launch.extensions_dir.unwrap().ends_with("tensorfold/0.6.0/torch_extensions"));
    let without = ProfileBindings::new(dir.path().join("logs"), dir.path().join("runtime"));
    assert!(without.spec(&work).is_err(), "no private cache root, no launch");
}
```

Build `tensorfold_work()` from the file's existing work helper by swapping the profile and deployment exactly as the Task 2 `fixture()` does. In `crates/capyctl-controller/src/coordinator/tests_cleanup.rs`, add a `// T41` test whose `Driver.engine` is a small adapter returning `Some(false)` from `idle_before_signal` (wrap `capyctl_testkit::FakeEngine` and override that one method) and assert the cleanup sends no termination (the scripted tool's `terminations()` is empty) and the cleanup does not complete; and the same with `Some(true)` terminating normally.

In `crates/capyctl-cli/tests/standalone_engines.rs`, add a `// T41` test that registers a TensorFold profile in `engines.yaml` (the `tensorfold_env` helper from `engine_cli.rs`, copied) and starts the standalone provider's `installations`: the published profile is named `tensorfold`, engine `tensorfold`, deep park disabled.

- [ ] **Step 2: Run them to see them fail.**

Run: `cargo test -p capyctl-controller --lib engine_bindings coordinator::tests_cleanup --locked; cargo test -p capyctl-cli --test standalone_engines --locked`
Expected: FAIL (`spec` refuses TensorFold; the cleanup signals a busy engine).

- [ ] **Step 3: The embedded spec.** In `engine_bindings.rs`:

```rust
    /// ADR 0023 §3: the embedded TensorFold plan through the shared builder.
    fn tensorfold_plan(
        &self,
        work: &InitializeWork,
        effective: &capyctl_config::effective::EffectiveDeployment,
    ) -> Result<(capyctl_adapters::tensorfold::PlanInputTensorfold, bool), CoordinatorError> {
        let refuse = |what: String| CoordinatorError::Service(format!("cannot build a TensorFold launch plan: {what}"));
        let endpoint = crate::port::engine_url(work.endpoint()).ok_or_else(|| refuse("no address".into()))?;
        let port = endpoint.port().ok_or_else(|| refuse("the leased endpoint names no port".into()))?;
        let cache = self.engine_cache.as_ref().ok_or_else(|| refuse("no private engine cache".into()))?;
        let dir = cache
            .torch_extensions(&effective.profile.build_fingerprint)
            .map_err(|error| refuse(error.to_string()))?;
        let built = capyctl_agent::engine_cache::has_build(&dir);
        let plan = capyctl_adapters::tensorfold::plan_from_effective(
            effective,
            port,
            self.log_dir
                .join(&work.fence().deployment_id)
                .join(format!("{}.log", work.incarnation()))
                .to_string_lossy()
                .into_owned(),
            Some(dir.to_string_lossy().into_owned()),
        )
        .map_err(|error| refuse(error.to_string()))?;
        Ok((plan, built))
    }
```

and replace the Task 2 refusal arm in `spec`:

```rust
            Engine::Tensorfold => {
                let endpoint = crate::port::engine_url(work.endpoint()).ok_or_else(|| {
                    CoordinatorError::Service(format!("frozen binding endpoint names no address: {}", work.endpoint()))
                })?;
                let (launch, extensions_built) = self.tensorfold_plan(work, &self.sized(work)?)?;
                Ok(AdapterSpec::Tensorfold {
                    endpoint,
                    fingerprint: profile.build_fingerprint.clone(),
                    model_id: effective.routes.first().cloned().unwrap_or_else(|| effective.name.clone()),
                    launch: Some(launch),
                    extensions_built,
                })
            }
```

The worker's factory requires `work.credential_ref()` to be non-empty (`worker.rs` line 1302). Check what the store puts there for a profile without `credential_ref` (`grep -n "credential_ref" crates/capyctl-store/src/ordinary_lifecycle/startup.rs`); if it is empty for TensorFold, change that guard to `work.endpoint().is_empty() || (work.credential_ref().is_empty() && work.effective().profile.engine != Engine::Tensorfold)` with `// ADR 0023 §3: TensorFold has no key to reference.`, and make sure `endpoint_of` in `coordinator_port.rs` already copes with `engine_key: None` (it does: the key is an `Option`).

- [ ] **Step 4: The installation.** `crates/capyctl-cli/src/roles.rs` `runtime_dir` refuses a directory without `capyctl_vllm_guard.py`. That file is part of every managed runtime, so a TensorFold installation passes; keep the check. In `role_installation_as`, `deep_park` for a TensorFold profile is the profile's own `disabled`; nothing else changes. Confirm with the `standalone_engines` test.

- [ ] **Step 5: The embedded idle gate.** In `worker.rs` `drive_cleanup`, after `drain_before_terminate(shared, work, stop).await?;`:

```rust
    // Spec §5 (ruling 9, ADR 0023 §6): an engine with its own work counters
    // (TensorFold) must read idle before the signal. At the bound nothing is
    // sent; the cleanup stays uncertain and keeps its accounting.
    let member = capyctl_adapters::traits::MemberRef {
        deployment_id: work.deployment_id.clone(),
        member_id: work.binding_id.clone(),
    };
    let budget = Duration::from_millis(
        u64::try_from(work.deadline_ms.saturating_sub((shared.clock)()?).saturating_sub(
            i64::try_from(shared.options.protocol_timeout.as_millis()).unwrap_or(i64::MAX),
        ))
        .unwrap_or(0),
    );
    let until = tokio::time::Instant::now() + capyctl_adapters::tensorfold::ENGINE_IDLE_BOUND.min(budget);
    loop {
        match driver.engine.idle_before_signal(&member).await {
            None | Some(true) => break,
            Some(false) if tokio::time::Instant::now() >= until => {
                return Err(CoordinatorError::Service(
                    "the engine still reports work in flight; the stop was not sent".into(),
                ));
            }
            Some(false) => tokio::time::sleep(Duration::from_millis(250)).await,
        }
    }
```

Use the field names `OrdinaryCleanupReceipt` actually has for the deployment id (`grep -n "pub struct OrdinaryCleanupReceipt" -A20 crates/capyctl-store/src`). A remote binding's engine (`RemoteEngine`) returns `None`, so remote cleanups are unchanged here; the host agent gates them (Task 9).

- [ ] **Step 6: Run the tests to see them pass.**

Run: `cargo test -p capyctl-controller --all-targets --locked && cargo test -p capyctl-cli --test standalone_engines --locked`
Expected: PASS.

- [ ] **Step 7: Verify and commit.** Run the Global Constraints verification commands.

```bash
git add crates/capyctl-controller crates/capyctl-cli/src/roles.rs crates/capyctl-cli/tests/standalone_engines.rs
git commit -m "feat: standalone runs TensorFold through the shared launch builder"
```

---

### Task 12: `local_engine.tensorfold`, `--tensorfold-bin`, `CAPYCTL_TENSORFOLD_BIN`

Owner decision 2026-10-01 (ruling 2): a role's own TensorFold is declared exactly like its own vLLM or SGLang, three ways with one precedence, in the shared code both roles use (standalone is a server plus one host).

**Files:**
- Modify: `crates/capyctl-config/src/engine_settings.rs` (module table at the top; `TENSORFOLD_BIN_ENV` beside `SGLANG_BIN_ENV` line 41; `EngineOverrides` fields, `or`, `names_an_engine`, `from_env`, `from_document` line 232; `EngineSettings` line 366 and `resolve`; `installations` line 413; the host-document profile build at line 470–510)
- Modify: `crates/capyctl-config/src/schema.rs` (`LOCAL_ENGINE`, line 226: `("tensorfold", SCALAR)`)
- Modify: `crates/capyctl-config/src/registration.rs` (`ENVIRONMENT_PROFILES` line 329)
- Modify: `crates/capyctl-cli/src/grammar.rs` (new `tensorfold_bin` beside `sglang_bin` line 583; `engines()` line 651)
- Modify: `crates/capyctl-cli/src/roles.rs` (`EnvEngineProvider::installation` line 947 and its two "declares no engine" messages; `role_installation_as`: toolchain check and forced deep park for TensorFold; `probe_fingerprint` line 1156)
- Modify: `crates/capyctl-cli/src/engine.rs` (the reserved-name message at line 315)
- Modify: `docs/operations/configuration.md` (settings table, after the SGLang row at line 151)
- Test: `crates/capyctl-config/src/engine_settings.rs` (unit tests, the precedence table test at line 629), `crates/capyctl-cli/tests/standalone_engines.rs`, `crates/capyctl-cli/tests/setting_overrides_cli.rs`

**Interfaces:**
- Consumes: `Engine::Tensorfold`, `Engine::name` (Task 2); `capyctl_agent::engines::toolchain::check` and `capyctl_adapters::engine_env::SYSTEM_PATH` (Task 4).
- Produces: `capyctl_config::engine_settings::TENSORFOLD_BIN_ENV = "CAPYCTL_TENSORFOLD_BIN"`; `EngineOverrides::tensorfold` and `EngineSettings::tensorfold: Option<PathBuf>`; profile names `local` (one executable) or `local-vllm`, `local-sglang`, `local-tensorfold` (several); `ENVIRONMENT_PROFILES` gains `local-tensorfold`.

- [ ] **Step 1: Write the failing tests.** In the `engine_settings.rs` unit tests, add a row to the precedence table test (the one with the `"sglang"` row at line 645), mirroring it exactly:

```rust
            (
                "tensorfold",
                EngineOverrides {
                    tensorfold: Some("/flag/tensorfold".into()),
                    ..Default::default()
                },
                vec![(TENSORFOLD_BIN_ENV, "/env/tensorfold")],
                json!({"local_engine": {"tensorfold": "/yaml/tensorfold"}}),
                |s| format!("{:?}", s.tensorfold),
                [
                    "Some(\"/flag/tensorfold\")",
                    "Some(\"/env/tensorfold\")",
                    "Some(\"/yaml/tensorfold\")",
                ],
            ),
```

(match the row's real tuple shape; copy the SGLang row and change the names) and:

```rust
    // T41 T03 (ADR 0023 §2): one executable is `local`; several are named by
    // their engine.
    #[test]
    fn tensorfold_installations_are_named_like_the_others() {
        let one = EngineSettings { tensorfold: Some("/t/bin/tensorfold".into()), ..defaults() };
        assert_eq!(one.installations(), vec![("local", Engine::Tensorfold, "/t/bin/tensorfold".into())]);
        let two = EngineSettings { vllm: Some("/v/bin/vllm".into()), tensorfold: Some("/t/bin/tensorfold".into()), ..defaults() };
        assert_eq!(
            two.installations(),
            vec![
                ("local-vllm", Engine::Vllm, "/v/bin/vllm".into()),
                ("local-tensorfold", Engine::Tensorfold, "/t/bin/tensorfold".into()),
            ]
        );
    }
```

where `defaults()` is `resolve(&Default::default(), &Default::default(), &Default::default())`. In `crates/capyctl-cli/tests/standalone_engines.rs` add a `// T41` test: with `CAPYCTL_TENSORFOLD_BIN` pointing at a fake TensorFold venv's `bin/tensorfold` (the `tensorfold_env` helper from `engine_cli.rs`, copied, with `ninja`, `nvcc`, `c++` in its `bin`), the standalone provider's `installations` publish `local`, engine `tensorfold`, version `0.6.0`, deep park disabled even though `CAPYCTL_DEEP_PARK` is unset (default on); and the same venv without `ninja` makes start refuse with `toolchain_missing` naming `ninja` (skip that half when `ninja` is in `SYSTEM_PATH`, as Task 4's test does). In `setting_overrides_cli.rs`, add `--tensorfold-bin` to the flag list the file checks against `capyctl start standalone --help` and `start host --help` (follow the existing `--sglang-bin` entry).

- [ ] **Step 2: Run them to see them fail.**

Run: `cargo test -p capyctl-config --lib engine_settings --locked; cargo test -p capyctl-cli --test standalone_engines --test setting_overrides_cli --locked`
Expected: FAIL to compile (`tensorfold` field, `TENSORFOLD_BIN_ENV`).

- [ ] **Step 3: The shared settings.** In `engine_settings.rs`:
  - Module table: add `//! | TensorFold executable | `local_engine.tensorfold` | `--tensorfold-bin` | `CAPYCTL_TENSORFOLD_BIN` |` after the SGLang row, and change "one of them is the runtime profile `local`, both are `local-vllm` and `local-sglang`" to "one of them is the runtime profile `local`; several are `local-vllm`, `local-sglang` and `local-tensorfold`".
  - `pub const TENSORFOLD_BIN_ENV: &str = "CAPYCTL_TENSORFOLD_BIN";`
  - `EngineOverrides`: `pub tensorfold: Option<PathBuf>,`; `or`: `tensorfold: self.tensorfold.or(lower.tensorfold),`; `names_an_engine`: `|| self.tensorfold.is_some()`; `from_env`: `tensorfold: text(TENSORFOLD_BIN_ENV).map(PathBuf::from),`; `from_document`: `tensorfold: path_of(local.get("tensorfold"), "local_engine.tensorfold")?,`.
  - `EngineSettings`: `pub tensorfold: Option<PathBuf>,`; `resolve`: `tensorfold: merged.tensorfold,`.
  - `installations`:

```rust
    /// ADR 0018 §5, ADR 0023 §2: the role's own installations and their
    /// profile names: one executable is `local`; several are `local-<engine>`.
    pub fn installations(&self) -> Vec<(&'static str, Engine, PathBuf)> {
        let named: Vec<(Engine, PathBuf)> = [
            (Engine::Vllm, &self.vllm),
            (Engine::Sglang, &self.sglang),
            (Engine::Tensorfold, &self.tensorfold),
        ]
        .into_iter()
        .filter_map(|(engine, path)| path.clone().map(|path| (engine, path)))
        .collect();
        if let [(engine, path)] = named.as_slice() {
            return vec![("local", *engine, path.clone())];
        }
        named
            .into_iter()
            .map(|(engine, path)| {
                let name = match engine {
                    Engine::Vllm => "local-vllm",
                    Engine::Sglang => "local-sglang",
                    Engine::Tensorfold => "local-tensorfold",
                };
                (name, engine, path)
            })
            .collect()
    }
```

  - The host-document profile build: `deep_park: settings.deep_park && engine != Engine::Tensorfold,` with `// ADR 0023 §6: TensorFold never parks, whatever local_engine.deep_park says.`, and the `profile_exists` message names `--vllm-bin/--sglang-bin/--tensorfold-bin` and `CAPYCTL_VLLM_BIN/CAPYCTL_SGLANG_BIN/CAPYCTL_TENSORFOLD_BIN`.
  - `schema.rs` `LOCAL_ENGINE`: `("tensorfold", SCALAR),` after `("sglang", SCALAR)`.
  - `registration.rs`: `pub const ENVIRONMENT_PROFILES: &[&str] = &["local", "local-vllm", "local-sglang", "local-tensorfold"];`

- [ ] **Step 4: The flag and the roles.** In `grammar.rs`, beside `sglang_bin`:

```rust
    /// The TensorFold executable (`<venv>/bin/tensorfold`) this role runs as
    /// its `local` profile. Wins over CAPYCTL_TENSORFOLD_BIN and
    /// local_engine.tensorfold.
    #[arg(long, value_name = "PATH", value_parser = parse_engine_path)]
    tensorfold_bin: Option<PathBuf>,
```

and `tensorfold: self.tensorfold_bin.clone(),` in `engines()`. In `roles.rs`:
  - `installation`: match on `settings.installations().into_iter().next()` instead of the `(vllm, sglang)` pair, keeping "the vLLM one first" (the order `installations` returns), and add `--tensorfold-bin` / `CAPYCTL_TENSORFOLD_BIN` to both "this host declares no engine" messages.
  - `role_installation_as`, for `engine == Engine::Tensorfold`: run the Task 4 toolchain check on the executable's directory with `settings.cuda_home` and `SYSTEM_PATH`, refusing start with `no_installation(format!("toolchain_missing: {missing}"))`; and set `deep_park = false` (ADR 0023 §6), whatever `settings.deep_park` says.
  - `probe_fingerprint`: keep the last whitespace-separated token of the last non-empty line of `--version` output, so `tensorfold 0.6.0` publishes `0.6.0` as `engine add` records it; vLLM's bare `0.29.0` is unchanged. If the engine-settings `probe` closure used for host documents is a different function, apply the same rule there.
  - `engine.rs` line 315: the reserved-name message names `--tensorfold-bin`, `CAPYCTL_TENSORFOLD_BIN` as well.

- [ ] **Step 5: The settings reference.** In `docs/operations/configuration.md`, after the SGLang interpreter row:

```markdown
| TensorFold executable | `local_engine.tensorfold` | `--tensorfold-bin <path>` | `CAPYCTL_TENSORFOLD_BIN` | none | host, standalone |
```

and change the sentence after the table that says SGLang takes no host-fixed arguments to "SGLang takes no host-fixed arguments, so `args` applies to the vLLM and TensorFold profiles only. One `local_engine` executable is the profile `local`; several are `local-vllm`, `local-sglang` and `local-tensorfold`."

- [ ] **Step 6: Run the tests to see them pass.**

Run: `cargo test -p capyctl-config --all-targets --locked && cargo test -p capyctl-cli --all-targets --locked`
Expected: PASS; the existing two-engine `local-vllm`/`local-sglang` tests still pass.

- [ ] **Step 7: Verify and commit.** Run the Global Constraints verification commands and `cd site && npm run check` (the settings page is synced into the site).

```bash
git add crates/capyctl-config crates/capyctl-cli docs/operations/configuration.md
git commit -m "feat: local_engine.tensorfold, --tensorfold-bin and CAPYCTL_TENSORFOLD_BIN"
```

---

### Task 13: Guides, settings reference, release notes and site

Example output in guides must match what the binary prints. Produce it from the branch build against a fake TensorFold venv (Step 1), and replace it with the live output in Task 14 if the two differ.

**Files:**
- Modify: `docs/guide/engines.md` (new section "TensorFold" after "Custom builds")
- Modify: `docs/operations/configuration.md` (the CUDA toolkit row at line 160 and "Variables CapyCTL sets for engines" at line 333)
- Create: `docs/operations/release-notes-0.1.1.md`
- Modify: `site/src/components/landing/Platforms.astro` (Engines row), `site/src/pages/index.astro` (line 10 description)
- Modify: `docs/guide/deploy.md` only if it lists engine names (`grep -n "SGLang" docs/guide/*.md`; update each list of engines to include TensorFold)

**Interfaces:** none (documents only).

- [ ] **Step 1: Capture real output.** Build and run against a fake venv shaped like `tensorfold_env` in `crates/capyctl-cli/tests/engine_cli.rs` (a `bin/tensorfold` that prints `tensorfold 0.6.0`, `ninja`, `nvcc`, `c++` scripts, the dist-info):

Run: `cargo build --locked -p capyctl-cli && HOME=$(mktemp -d) ./target/debug/capyctl engine add <fake venv>`
Expected: a `Registered tensorfold (tensorfold 0.6.0)` block. Copy it verbatim, replacing the temporary paths with `/home/me/...` as the existing guide examples do.

- [ ] **Step 2: The engines guide.** Change the first line of `docs/guide/engines.md` to "CapyCTL runs the vLLM, SGLang or TensorFold you already have." and add after "Custom builds":

````markdown
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
<the block captured in Step 1>
```

TensorFold has no way to free its memory while it runs, so a TensorFold model
does not park: when CapyCTL needs the memory, or the model sits idle, CapyCTL
waits for its requests to finish, stops it, and starts it again on the next
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
resources:
  cold:
    allocations: [{domain: unified, bytes: 32GiB}]
  ready:
    allocations: [{domain: unified, bytes: 30GiB}]
engine_config:
  context_length: 32768
```

A drafter works as a draft model does for vLLM and SGLang: allow it on the
engine (`security.approved_options: [--drafter]` and the drafter's directory in
`security.approved_paths`), then pass it with
`accept_extra_args: true` and `extra_args: [--drafter, /path/to/drafter]`.
CapyCTL does not download drafters; without one it starts TensorFold with
`--drafter none`. The first start builds kernels and can take several
minutes; CapyCTL allows it up to 30 minutes, and later starts reuse the build.

TensorFold is checked on NVIDIA GB10 (unified memory) in this release. On a
discrete GPU it runs, but no live check has passed there yet.
````

Write the YAML exactly as a deployment the Task 2 resolution accepts: validate it with `./target/debug/capyctl validate config --file <it>` (`validate config` takes the file and prints the completed document; `grep -n "validate" docs/guide/deploy.md` for the exact flag) and adjust until it passes. The resources shape must match what `resolve_effective` accepts for `restart_only` (the Task 2 fixture is the reference).

- [ ] **Step 3: The settings reference.** In `docs/operations/configuration.md`, change the CUDA toolkit row's text to "CUDA toolkit for engine kernel builds (vLLM's FlashInfer, TensorFold's first start)", and in "Variables CapyCTL sets for engines" add `TENSORFOLD_NO_UPDATE_CHECK`, `TORCH_EXTENSIONS_DIR` (`<state dir>/engines/tensorfold/<version>/torch_extensions`, private to the service user), `HF_HUB_OFFLINE` and `TRANSFORMERS_OFFLINE` to the list of variables CapyCTL writes for the engine. (The `local_engine.tensorfold` row was added in Task 12.)

- [ ] **Step 4: Release notes.** Create `docs/operations/release-notes-0.1.1.md`:

```markdown
# CapyCTL 0.1.1

## TensorFold

CapyCTL now runs TensorFold 0.6.0 beside vLLM and SGLang. Register an existing
TensorFold venv with `capyctl engine add <venv>`, or name it as the role's own
engine with `--tensorfold-bin` (`CAPYCTL_TENSORFOLD_BIN`, `local_engine.tensorfold`); CapyCTL checks that the CUDA
build tools TensorFold needs on its first start are on the engine's PATH.
TensorFold models do not park: CapyCTL stops them when it needs the memory and
starts them again on the next request. A TensorFold deployment states its
`resources` and `context_length`. Checked on NVIDIA GB10; discrete GPUs run it
unchecked in this release.

## Other changes

- A readiness check now accepts a model that answers with reasoning first.
- New exit code 26, `toolchain_missing`, from `capyctl engine add`.

## Upgrade

Replace the binary and restart the roles; 0.1.0 state is reused.
```

Check the upgrade sentence against the store schema version: if Tasks 2–12 changed none (they should not), it stands; if any did, say so and how the store migrates.

- [ ] **Step 5: The site.** In `Platforms.astro`: `<tr><th scope="row">Engines</th><td>vLLM, SGLang, TensorFold</td></tr>`. In `index.astro` line 10: "... on vLLM, SGLang and TensorFold, behind one OpenAI-compatible endpoint."

- [ ] **Step 6: Check.**

Run: `cd site && npm run check`
Expected: every check passes (tests, build, links, hero, commands, size, voice).

Run: `cargo test -p capyctl-cli --test site_errors_page --test wording_gate --locked`
Expected: PASS.

- [ ] **Step 7: Commit.**

```bash
git add docs/guide docs/operations site/src/components/landing/Platforms.astro site/src/pages/index.astro
git commit -m "docs: TensorFold in the engines guide, settings, site and 0.1.1 notes"
```

---

### Task 14: Live qualification TF1–TF5 on host B, and the status entry

The steps below are run by whoever executes the plan, on host B, after Tasks 1–13 are merged into the branch and the local checks pass. Only one live session runs on the hosts at a time. Host names, addresses and home paths are never written into the repository; evidence goes into the commit message and the status runbook in prose.

**Files:**
- Modify: `docs/runbooks/f2-current-status.md` (new entry at the top)
- Modify: `docs/guide/engines.md` (replace the captured output if the live one differs)

**Interfaces:**
- Consumes: everything above; `scripts/live/matrix/sync.sh`, `scripts/live/matrix/hosts.local.env` (untracked), host B's TensorFold venv `~/tensorfold-0.6.0-venv`, the spike model in `~/tensorfold-spike/hf`.

- [ ] **Step 1: Preconditions.** On the control machine: `scripts/live/matrix/sync.sh snapshot && scripts/live/matrix/sync.sh push b && scripts/live/matrix/sync.sh build b`. On host B: no capyctl role running (`pgrep -a capyctl` prints nothing; stop the host service with its own CLI if one runs, as the runbooks describe), no GPU process (`nvidia-smi --query-compute-apps=pid --format=csv,noheader` is empty), and at least 20 GB free on the disk holding `~/tensorfold-spike/hf` (for TF4's download). Find the Nemotron snapshot directory: `find ~/tensorfold-spike/hf -path '*snapshots*' -name config.json -printf '%h\n'`. Run every command below with the built binary `~/capyctl-f2/target/release/capyctl`, a fresh state directory (`export CAPYCTL_STATE_DIR=$(mktemp -d ~/capyctl-tf-state.XXXX)`; check the variable name with `capyctl config show`), and `export CAPYCTL_MODELS_ROOT=~/tensorfold-spike/hf`.

- [ ] **Step 2: TF1 — `engine add` lists TensorFold 0.6.0.**

Run: `capyctl engine add ~/tensorfold-0.6.0-venv` then `capyctl engine list`
Expected: `Registered tensorfold (tensorfold 0.6.0)`, deep park `disabled`, CUDA toolkit found; the list shows `tensorfold engines.yaml tensorfold 0.6.0 no disabled`. Record the output for the guide.

- [ ] **Step 3: TF2 — deploy, plain and streaming chat, peak within the reservation.** Start `capyctl start standalone` in a terminal (its banner prints the API key path). Write `nemotron.yaml` as in the engines guide, with `model:` the snapshot directory relative to the models root, `context_length: 32768`, and `resources` of 32 GiB cold and 30 GiB ready (the spike's estimate was 27.11 GiB). Then:

Run: `capyctl deploy model --file nemotron.yaml && capyctl start deployment nemotron --wait`
Expected: ready. The first start builds kernels; note how long it took (the first-build bound is 30 minutes).

Run a plain and a streaming chat completion against the endpoint with the key (`curl -s -H "Authorization: Bearer $KEY" https://127.0.0.1:8443/v1/chat/completions -d '{"model":"nemotron","messages":[{"role":"user","content":"What is 17+25? Answer with only the number."}],"max_tokens":256}'`, then the same with `"stream":true`; use the TLS and key handling the requests guide shows).
Expected: both answer; the non-streaming answer carries the `tensorfold` object. While a long request runs, sample the engine process's memory (`nvidia-smi --query-compute-apps=pid,used_memory --format=csv` and the host's `capyctl status deployment nemotron --json` reservation) and confirm the peak stays within the 32 GiB cold and 30 GiB ready allocations.

- [ ] **Step 4: TF3 — park releases memory, a request wakes it.** Stop the role, add `lifecycle_defaults: {ready_idle_timeout: 60s}` to the standalone document's server section (check the exact place with `capyctl config show`), and start it again. Leave the deployment idle for 90 s.

Expected: the deployment reads `stopped` (idle stop, on-demand eligible), the TensorFold process is gone (its PID from `capyctl inspect deployment nemotron --json` no longer exists) and its GPU memory is free. Then send one chat request.
Expected: it answers after a warm start (the spike measured 7 s to `/health`); the start used the ordinary bound because the build directory exists. `capyctl park deployment nemotron` answers the unsupported refusal.

- [ ] **Step 5: TF4 — vLLM and TensorFold switch under memory pressure.** Register host B's authorized vLLM 0.29 environment (`HOST_B_VLLM_VENV_DIR` in `hosts.local.env`): `capyctl engine add ~/$HOST_B_VLLM_VENV_DIR`. Deploy Qwen3-4B with `model: {hf: Qwen/Qwen3-4B}` (the CLI pins the commit), `engine: vllm`, and `memory.kv_cache` sized so that it and the TensorFold deployment do not both fit in the host's managed limit. Start the vLLM deployment with `--evict --wait`, then send a request for `nemotron`, then one for the vLLM deployment, three times each.

Expected: each request switches: the idle side is released (vLLM parks, TensorFold stops with `released: stopped`), the other wakes and answers. Peak memory stays within each deployment's reservation (sample as in TF2). No request fails.

- [ ] **Step 6: TF5 — cancel mid-stream, then park.** Start a long streaming request to `nemotron` (`max_tokens: 2048`) and close the client after the first chunks (`curl ... | head -c 2000`). Read TensorFold's own counters through the host: `curl -s http://127.0.0.1:<engine port>/health` on host B (the port is in `capyctl inspect deployment nemotron --json`).

Expected: within the drain bound (30 s) `busy` is `false` and `requests_running` is `0`. Then trigger the release (start the vLLM deployment with `--evict --wait`): the TensorFold process receives its signal only after the counters read idle (the role log shows the stop after the idle read), and it exits within the stop bound.

- [ ] **Step 7: Clean up.** Stop the role with a drained shutdown, delete both deployments, remove the profiles with `capyctl engine remove`, and confirm no capyctl or engine process and no GPU process is left on host B. Keep the downloaded Qwen3-4B source unless the owner asks for it to be removed. Remove the temporary state directory.

- [ ] **Step 8: The status entry.** Add at the top of `docs/runbooks/f2-current-status.md`:

```markdown
## TensorFold engine — <date>

ADR 0023 is implemented: `tensorfold` is the third engine kind, registered with
`capyctl engine add` after a closed-PATH toolchain check, launched restart-only
with a private per-version build cache, ready on `/health` and the model list,
drained on its own counters before a stop, and part of the switch planner like
the other engines. A role's own TensorFold is `local_engine.tensorfold`
(`--tensorfold-bin`, `CAPYCTL_TENSORFOLD_BIN`); a drafter is an approved path
extra argument, as vLLM's and SGLang's draft models are.

Live on host B (GB10, standalone, TensorFold 0.6.0 venv, Nemotron 3.5 Lightning
30B-A3B): TF1 <result>, TF2 <result: first start N s, peak X GiB of 32>, TF3
<result: warm wake N s>, TF4 <result: three switches each way>, TF5 <result:
idle after N s>. Local checks: formatting, Clippy with warnings denied, core
suite <n> passed / 0 failed, workspace <n> / 0, Python runtime <n> OK, site
check. Discrete GPUs run TensorFold unqualified. CPU and Fake-engine tests are
not qualification.
```

Fill every `<...>` with the measured value from Steps 2–6; a row that failed is written as failed with its reason, and the task is not done until it passes or the owner accepts the failure.

- [ ] **Step 9: Commit.** Update `docs/guide/engines.md` if the live `engine add` output differs from the captured one.

```bash
git add docs/runbooks/f2-current-status.md docs/guide/engines.md
git commit -m "docs: TensorFold live rows TF1-TF5 on host B"
```

---

## Self-Review

**Spec coverage.** §1 engine kind: Task 2. §2 detection and `engine add`: Tasks 3, 4 (toolchain, verified set, deep park disabled). §3 launch: Tasks 5, 6, 9, 11 (command, environment, `TORCH_EXTENSIONS_DIR`, reserved flags, typed mapping, sensitive options, resources required, loopback, no key). §3 drafter: Tasks 2 (path option policy), 6 (launch-time check through symlinks), 13 (guide); ruling 7 records the vLLM and SGLang finding. §4 readiness and bounds: Tasks 2 (timeouts), 7 (readiness, warm bound). §5 draining: Tasks 7, 9, 11 (ruling 9). §6 park and wake: Tasks 2 (`capability_missing`), 7 (no park path), 9 (Park refused), existing restart-only release paths. §7 requests: Task 8. §8 settings: Task 12 (`local_engine.tensorfold` three ways), 13 (docs). Errors: Tasks 2, 4. Testing T41 and TF1–TF5: every task, Task 14. Documentation: Tasks 1, 4, 13, 14.

**Placeholders.** The only angle-bracket fields are live measurements in Task 14's status text, filled from the run. The Task 2 compile arms that refuse (agent `prepare`, `probe_adapter`, controller `spec`) are explicit fail-closed code replaced in Tasks 9 and 11.

**Type consistency.** `PlanInputTensorfold` fields are defined in Task 6 and used with the same names in Tasks 7, 9, 11. `AdapterSpec::Tensorfold { endpoint, fingerprint, model_id, launch, extensions_built }` is defined in Task 7 and built in Task 11. `idle_before_signal` is defined in Task 7 and called in Tasks 9 and 11. `EngineSettings::tensorfold` and `TENSORFOLD_BIN_ENV` are produced and consumed in Task 12.

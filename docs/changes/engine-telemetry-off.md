# Status: Engines no longer report usage off the host by default — 2026-10-22 (branch `feat/engine-telemetry-off`)

Owner decision 2026-10-22: the engines' own usage reporting is off for every engine by
default, and a host opts in. SPEC §13.3 (the closed engine environment) and ADR 0028
§2.1 rule 1 (names CapyCTL owns) govern where it lands.

What each engine reports, read from its own source:

- vLLM (0.31.0, `vllm/usage/usage_lib.py`, `vllm/envs.py`): rank 0 posts the platform,
  the model architecture and the engine settings to `https://stats.vllm.ai` when it
  starts, and its id and runtime figures every ten minutes after, unless
  `VLLM_NO_USAGE_STATS=1`,
  `VLLM_DO_NOT_TRACK=1` or `DO_NOT_TRACK=1` (a set `VLLM_DO_NOT_TRACK` hides
  `DO_NOT_TRACK`, so all three are set). [INFERENCE] 0.30 and 0.32 read the same
  names; only 0.31.0 was read.
- SGLang 0.5.22 (the published wheel's source): no usage reporting of its own, no usage
  library and no tracking switch; its OpenTelemetry tracing needs an exporter the
  launch asks for. The Hugging Face Hub library it imports turns its telemetry off
  under `DO_NOT_TRACK=1` (`huggingface_hub/constants.py`, read in the vLLM and
  TensorFold environments).
- llama.cpp: no usage reporting in the v0.6.0 source. [INFERENCE] v0.7.0 the same.
- TensorFold 0.6.6: no usage reporting; its GitHub release check was already off on
  every launch (`--no-update-check`, `TENSORFOLD_NO_UPDATE_CHECK=1`) and stays off
  whatever the new setting says.
- Custom engines (ADR 0031): unknown programs; `DO_NOT_TRACK=1` as the general
  convention.

What changed:

- A host setting `engine_telemetry: off | allow` (default `off`; standalone
  `host.engine_telemetry`), role-local like `engine_logs`: it never reaches the
  document deployments are resolved against, so no recipe or revision changes. Any
  other value refuses the start by name. The host and standalone roles hand it to the
  adapters when they start (`capyctl_adapters::engine_telemetry`), so it holds for
  every launch, embedded and remote.
- Under `off`, every launch environment gets `DO_NOT_TRACK=1`; vLLM also gets
  `VLLM_DO_NOT_TRACK=1` and `VLLM_NO_USAGE_STATS=1`. That covers vLLM (group workers
  too), SGLang, llama.cpp, TensorFold and custom engines. Under `allow` CapyCTL sets
  none of them and each engine keeps its own default.
- `DO_NOT_TRACK`, `VLLM_DO_NOT_TRACK` and `VLLM_NO_USAGE_STATS` are CapyCTL-owned
  names: a profile, a deployment or a custom manifest that sets one is refused with
  `engine_env_reserved:<name>`, whatever the approval says and whatever the case.
  A custom profile whose manifest already set `DO_NOT_TRACK` is refused at its next
  launch, as for any newly reserved name (ADR 0031 §2).
- Docs: the settings reference (the owned names, the setting, the variables CapyCTL
  sets), data retention (a "Nothing reported off the host" section) and the custom
  engine environment in the engines guide.

## Tests

Each failed before the change (the names were absent or settable) and passes after:

- `capyctl-adapters` `vllm::initialize` `usage_reporting_is_off_unless_the_host_allows_it`
  (T21): a single-rank and a group-worker launch get the three vLLM opt-outs under
  `off` and none under `allow`; `tests/vllm_initialize.rs`
  `the_engine_environment_is_a_closed_allowlist` (T21) sees them in the spawned
  environment.
- `tests/sglang_initialize.rs`
  `initialize_spawns_protected_waits_probes_and_reports_the_group` (T21): the spawned
  SGLang environment carries `DO_NOT_TRACK=1`.
- `tests/llamacpp_args.rs` `the_environment_is_closed`, `tests/tensorfold_args.rs`
  `the_engine_environment_is_closed`, `custom::args`
  `the_environment_is_closed_and_layered` and `tests/custom_adapter.rs` (T21):
  `DO_NOT_TRACK=1` under `off`, nothing under `allow`, TensorFold's release check off
  either way.
- `engine_telemetry` `off_writes_the_opt_outs_and_allow_leaves_the_environment` and
  `tests/engine_telemetry_host.rs` `the_role_setting_reaches_every_launch` (T21): the
  role's setting reaches every launch and every opt-out is an owned name.
- `capyctl-config` `tests/remote_roles.rs`
  `engine_telemetry_defaults_to_off_and_stays_role_local` and `standalone`
  `standalone_honours_engine_telemetry` (T21): `off` when omitted, `allow` accepted,
  role-local, any other value refused with its path; `tests/engine_env.rs` (T21): the
  three names are refused at the profile and the deployment level.

CPU and Fake-engine tests are not qualification.

## Live check still needed

On host A, with a vLLM chat deployment and an SGLang deployment: start the host with
the default and read `/proc/<engine pid>/environ` for the engine's API process (PID from
the ownership evidence, names only, never values other than these switches): vLLM shows
`DO_NOT_TRACK=1`, `VLLM_DO_NOT_TRACK=1`, `VLLM_NO_USAGE_STATS=1`; SGLang (and its
scheduler children) `DO_NOT_TRACK=1`; and no connection from the engine processes to
`stats.vllm.ai` during start and ten minutes after (`ss -tnp` for the engine's process
tree, or the resolver log). Then restart the host with `--set engine_telemetry=allow`:
the vLLM launch shows none of the three switches and `~/.config/vllm/usage_stats.json`
of the service user gains an entry; the engine still serves. Optional: a llama.cpp, a
TensorFold and a custom launch each show `DO_NOT_TRACK=1` under the default.

# Release note: Privacy

- **Engines no longer report usage off the host by default.** Every engine CapyCTL
  starts gets `DO_NOT_TRACK=1`, and vLLM also `VLLM_DO_NOT_TRACK=1` and
  `VLLM_NO_USAGE_STATS=1`, so vLLM no longer sends its usage report to
  `stats.vllm.ai`. A host that wants the engines' own defaults sets
  `engine_telemetry: allow`. A deployment or profile that sets one of these names is
  now refused (`engine_env_reserved:<name>`). See
  [data retention](../operations/data-retention.md#nothing-reported-off-the-host).

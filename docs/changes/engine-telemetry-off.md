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
  `DO_NOT_TRACK`, so all three are set). 0.30.0 and 0.32.0 read the same three names
  (their `usage_lib.py`, read on the lab hosts); there the report is made by the rank 0
  worker (`v1/worker/gpu_worker.py`), which for one rank runs in the `VLLM::EngineCore`
  process.
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

## Live check (host A and host B, 2026-10-23)

Release build of `58c1bf99` from a git archive on each host (the same binary sha256 on
both). Each host had a standalone role on 127.0.0.1:38443 (management 37443) with a fresh
state directory, a 108 GiB limit and an 8 GiB free reserve. `HOME` and
`XDG_CONFIG_HOME` pointed into the run directory. The engines inherit that `HOME`: their
caches and vLLM's usage file were written there, and neither host's own
`~/.config/vllm/usage_stats.json` changed (same size, modification time and sha256
before and after). Host A ran vLLM 0.32.0 and SGLang 0.5.22 on Qwen3-4B, and llama.cpp
v0.7.0 on Qwen3-4B Q4_K_M. Host B ran the rest of step 3 on vLLM 0.30.0 with
Qwen3-4B-Instruct-2507, because host A was needed for another run and vLLM 0.32.0 is
installed only on host A. Context was 8192 everywhere. Environments were read from
`/proc/<pid>/environ` for the three names only. The evidence is in
`target/t188-live/` on the laptop.

1. **The default sets the opt-outs: pass.** On host A, vLLM 0.32.0's API process and its
   `multiprocessing` resource tracker showed `DO_NOT_TRACK=1`, `VLLM_DO_NOT_TRACK=1` and
   `VLLM_NO_USAGE_STATS=1`. SGLang's entry process and its tracker showed
   `DO_NOT_TRACK=1`, and so did `llama-server`. On host B, vLLM 0.30.0 showed the same
   three.

   Processes that rename themselves (`VLLM::EngineCore`, `sglang::scheduler`,
   `sglang::detokenizer`) show none of the names, because setproctitle zeroes the
   start of the area `/proc/<pid>/environ` reads. That is the first 728 to 733 bytes,
   and the names that survive after it are the tail of the tracker's list, the three
   names sitting at the head. EngineCore makes vLLM's report. Under the default it
   wrote no usage file in its `HOME`; under `allow` the same process wrote one and
   connected (step 3). That shows the names reach it.
2. **No egress: pass.** On host A, `ss -tuanp` was sampled every 0.2 s over the role's
   whole process tree, 77 PIDs including compile workers, from before the first
   start (03:08:06 UTC) to 03:23:47. That is 11 min 52 s after vLLM was Ready
   (03:10:53), 11 min 23 s after SGLang (03:12:24) and 11 min 21 s after llama.cpp
   (03:12:26). There were 4685 samples and 22682 loopback socket observations. There
   were no connections to anything else, and no listening or unconnected socket off
   loopback. Each engine answered a request ("42").

   On host B (vLLM 0.30.0, start to 22 s after Ready) a second watcher read the tree's
   sockets every 20 ms, 12375 times. It found no resolver lookup and no connection
   off loopback.
3. **`allow`: pass.** The role was restarted with
   `--set host.engine_telemetry=allow`, keeping the same state directory.
   - **Host A (vLLM 0.32.0):** the API process and its tracker showed none of the three
     names. vLLM wrote one report record to `<run-dir HOME>/.config/vllm/usage_stats.json`,
     1473 bytes with the architecture, GPU and engine settings. It writes that record
     just before it posts, so it attempted the report. The 0.2 s sampler saw no
     connection, because the post is too short for it. The engine still served (200,
     "42").
   - **Host B (vLLM 0.30.0):** the 20 ms watcher caught the report. EngineCore looked up
     a name through 127.0.0.53 at 03:37:08.39. It then opened a TCP connection to
     `[2606:4700:3032::6815:58f5]:443`, an address `stats.vllm.ai` resolves to, at
     03:37:08.55 (SYN-SENT, then ESTAB until 03:37:08.79). The usage file gained one
     record and the engine still served (200, "42").
   - **Restored `off` (host B):** the role was restarted without the setting. vLLM again
     showed the three names, the usage file stayed at its one record, and neither
     watcher saw anything off loopback.
4. **Reserved names are refused: pass.** `capyctl engine add <vLLM venv>` with
   `--env DO_NOT_TRACK=0`, `--env vllm_do_not_track=0` or
   `--env VLLM_NO_USAGE_STATS=0`, and with `--approve-env DO_NOT_TRACK`, was refused
   with `engine_env_reserved:<name>` (exit 2), and no profile was saved. A deployment
   with `engine_config.env` setting `DO_NOT_TRACK` or `VLLM_NO_USAGE_STATS` (vLLM), or
   `do_not_track` (llama.cpp), was refused at `deploy model` with `invalid_config` at
   `engine_config.env: engine_env_reserved:<name>`.

Not run: a TensorFold launch and a custom launch under the default (optional).

# Release note: Privacy

- **Engines no longer report usage off the host by default.** Every engine CapyCTL
  starts gets `DO_NOT_TRACK=1`, and vLLM also `VLLM_DO_NOT_TRACK=1` and
  `VLLM_NO_USAGE_STATS=1`, so vLLM no longer sends its usage report to
  `stats.vllm.ai`. A host that wants the engines' own defaults sets
  `engine_telemetry: allow`. A deployment or profile that sets one of these names is
  now refused (`engine_env_reserved:<name>`). See
  [data retention](../operations/data-retention.md#nothing-reported-off-the-host).

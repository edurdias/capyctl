# Status: `engine add --allow-remote-code` — 2026-10-16 (branch `feat/engine-add-remote-code`)

A deployment with `engine_config.trust_remote_code: true` is refused unless its
installation has `security.trust_remote_code: true` (Spec §3, ADR 0014 §8), and
`capyctl engine add` always wrote `false`. Serving Nemotron 3.5 Super VL on vLLM
needed checkpoint code, so `engines.yaml` had to be edited by hand and the role
restarted.

`capyctl engine add` now takes `--allow-remote-code`, which writes
`security.trust_remote_code: true` for the new profile (`ProfileSpec` carries the
switch, so the environment profiles built from `local_engine.trust_remote_code`
use the same field instead of patching the document afterwards). The result
reports `trust_remote_code`, and the text view adds `Remote code   allowed` when
it is set. TensorFold and llama.cpp have no remote-code option and already refuse
`trust_remote_code: true` in a deployment (ADR 0023 §4, ADR 0029 §5), so
`engine add --allow-remote-code` is refused for them with `capability_missing`
before anything runs or is written. The deploy-time refusal keeps its text and
now names `capyctl engine add --allow-remote-code` (and `--trust-remote-code true`
for the role's own engine); the refusal of a profile argument
`--trust-remote-code` names the option too.

Tests: `add_allows_checkpoint_code_only_when_asked` (engine_cli) adds one vLLM
profile with the option and one without, then validates a deployment with
`trust_remote_code: true` against each through `capyctl validate config`: the
first resolves, the second is refused naming `--allow-remote-code`. The
TensorFold and llama.cpp add tests assert the refusal and that nothing is
written or run. CPU and Fake-engine tests are not qualification; the live check
still needed is registering the vLLM installation on a lab host with
`--allow-remote-code` and deploying Nemotron 3.5 Super VL on it without editing
`engines.yaml`.

# Release note: Engines

- **`capyctl engine add --allow-remote-code` lets deployments on that engine
  run the code a checkpoint ships.** It writes `security.trust_remote_code: true`
  to `engines.yaml`, so a deployment with `engine_config.trust_remote_code: true`
  no longer needs a hand edit and a restart; the refusal without it names the
  option. vLLM and SGLang only: TensorFold and llama.cpp refuse it
  ([Engines](../guide/engines.md#custom-builds)).

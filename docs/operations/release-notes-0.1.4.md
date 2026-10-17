# CapyCTL 0.1.4

CapyCTL 0.1.4 parks SGLang groups again: a park or wake now records only memory reports
that are still current, which also fixes the cause of the failed SGLang group wake in
0.1.3. `capyctl engine add --allow-remote-code` replaces hand edits of `engines.yaml` for
checkpoints that ship their own code, `status` shows the startup memory a first start
really sets aside, `capyctl version` names the commit a release was built from, and
standalone reports the engine's own latency. Nemotron 3.5 Super VL is refused parking on
vLLM, whose wake of it answers wrongly.

## Engines

- **`capyctl engine add --allow-remote-code` lets deployments on that engine
  run the code a checkpoint ships.** It writes `security.trust_remote_code: true`
  to `engines.yaml`, so a deployment with `engine_config.trust_remote_code: true`
  no longer needs a hand edit and a restart; the refusal without it names the
  option. vLLM and SGLang only: TensorFold and llama.cpp refuse it
  ([Engines](../guide/engines.md#custom-builds)).
- **Nemotron 3.5 Super VL is not parked on vLLM.** vLLM 0.31.0 answers wrongly after it parks and
  wakes this model, so CapyCTL refuses `deep` and `host_backed` for it; declare
  `residency: restart_only`.

## Multi-node groups

- **SGLang groups park and wake again.** In 0.1.3 an SGLang group across machines
  restarted instead of parking, after a live group wake failed `restore_uncertain`. The
  cause was in CapyCTL: a park or wake could record a machine's memory report that had
  become too old to accept, either because the report was read before a slow check of the
  woken model or because the other machine reported later. Every report a park or wake
  records is now recent, for groups and single machines alike, and an SGLang group with no
  `residency` gets `deep` again. Rechecked live on two GB10 hosts: five park and wake
  cycles each for an SGLang 0.5.21 and a vLLM 0.31.0 group.

## Fixes

- **`status deployment` shows the startup memory a first start really sets aside.** A model
  whose estimated startup peak is above the machine's managed limit starts alone and sets
  aside the whole limit; status now shows that limit (provenance `whole_host`) instead of the
  estimate, which could exceed the machine's memory (265.2 GiB for a 71 GiB checkpoint on a
  121 GiB machine).
- **`capyctl version` shows the commit.** `capyctl version` prints the version
  and the commit the release was built from (`--json` for a JSON object);
  include it in bug reports. `capyctl --version` still prints only `capyctl
  <version>`.
- **Engine latency in standalone.** `GET /management/v1/metrics/latency` and
  `capyctl status deployment --json` now show the engine's own latency
  distributions (`tier: engine`, `source: engine`) for standalone instances
  of vLLM, SGLang and TensorFold (llama.cpp exports none), as they do on a
  server with hosts. Before, standalone showed CapyCTL's router series only.

## Known issues

- **Some models do not wake on vLLM 0.31.0.** gpt-oss still fails to reload its weights
  after a deep park, as on 0.30.0, and CapyCTL refuses to park it. Gemma 4 26B-A4B NVFP4
  answers wrongly after a wake; CapyCTL's wake check stops it instead of serving it.
  Declare `residency: restart_only` for both.
- **vLLM 0.31.0 can crash on a repeated long prompt with Nemotron 3.5 Super VL.** With
  prefix caching on (vLLM's default), sending the same 32k or 64k-token document a second
  time crashed the engine with a CUDA illegal memory access; CapyCTL restarted it and
  served the waiting request. Turn prefix caching off for this model
  (`--no-enable-prefix-caching`), as its recipe does.
- **A vLLM wake under `lazy` is slow.** On a GB10 with vLLM 0.30.0 and
  Qwen3.8-27B NVFP4, a deep-parked deployment loading with `lazy` took about
  2 minutes (123 s) to reload its weights in place on its first wake, against
  23–28 s with `eager`. Keep the default `eager` where wake time matters.

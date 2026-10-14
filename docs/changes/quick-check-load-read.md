# Status: Engine quick check reads load through CapyCTL, not unkeyed /metrics — 2026-10-14 (branch `fix/vllm031-metrics-auth`)

The vLLM 0.31.0 quick check on host A failed its `engine /metrics` step: the
probe curled the launch's loopback `/metrics` without a key and got `401
{"error": "Unauthorized"}`. That answer is CapyCTL's own key guard, by design,
and not a CapyCTL defect:

- Under deep parking (default on, ADR 0012, SPEC §9.1 / T21) the vLLM argv
  carries `--middleware capyctl_vllm_guard.RequireEngineKey`
  (`crates/capyctl-adapters/src/vllm/args.rs`) and the per-launch key rides
  `VLLM_API_KEY` in the engine's environment, not `--api-key`. The guard keys
  every path but `/health`, `/metrics` included
  (`runtime/capyctl_vllm_guard.py`, `_INFERENCE_EXACT`), whatever the vLLM
  version, so vLLM 0.30.0 answers the same.
- vLLM's own `AuthenticationMiddleware` guards only `/v1`, `/v2`,
  `/inference` and `/cohere` (`vllm/entrypoints/serve/middleware/authenticate.py`,
  `GUARDED_PREFIX`); `/metrics` is a separate mount
  (`entrypoints/serve/instrumentator/metrics.py`). Read from the vLLM 0.30.0
  sdist and the installed 0.31.0: the auth, middleware registration and
  metrics mount are the same apart from whitespace, and every gauge and
  histogram name CapyCTL reads is in both `v1/metrics/loggers.py`.
- Every CapyCTL reader sends the launch key: the host agent and standalone's
  embedded load (`capyctl_agent::load::keyed_get`, used by
  `capyctl_controller::embedded_load`), and the vLLM adapter's quiescence read
  (`EngineHttp::work_counts`, keyed through `auth`). Their tests already serve
  a fake engine whose `/metrics` refuses an unkeyed read
  (`crates/capyctl-agent/tests/load.rs`, `crates/capyctl-adapters/tests/vllm_http.rs`).

Live on the maintainers' RTX 4090 laptop, vLLM 0.31.0, Qwen2.5-1.5B-Instruct,
an isolated standalone from `origin/main` (057c1a47): the engine argv has the
guard and no `--api-key`; unkeyed `/metrics` and `/v1/models` answer 401,
`/health` 200; during a streamed request every
`GET /management/v1/metrics/load` read carried a fresh sample with
`engine.running` 1 and `waiting` 0, and 0 and 0 once it finished. Cleaned up
afterwards (deployment, profile and role gone, GPU back to its baseline).

The probe (`scripts/live/quick-check/remote.sh` `metrics`) now checks
CapyCTL's contract: during a request, the new `load_check.py` polls the
management load read with the admin token and passes when a fresh sample for
the deployment carries the engine's running and waiting counts. The unkeyed
`/metrics` code and whether the guard is on the argv are kept as notes in
`metrics-notes.txt`. Exercised against the laptop's live role; the quick-check
driver itself has not been rerun on host A.

Seen on the same run, unrelated to the key: standalone's latency view
carries router series only. `EmbeddedLoad::tick` records each report's load
sample but not its engine latency histograms, so `source: engine` series are
absent on standalone for every engine and version.

# Release note: none

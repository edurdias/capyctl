# Status: Extraction models: `api: extractions` and `fields` on `POST /v1/systemone` (ADR 0041) — 2026-11-04 (branch `feat/extraction-models`)

ADR 0041 (owner decisions 2026-11-04) adds a fourth deployment API,
`extractions`, with declared custom-engine `field_types`. It is part of the
frozen revision; manifests normalize and digest the declared set. Every native
engine is refused. Extractions run restart-only on one machine without a
topology or parsers, with LMCache off, and require both `custom_engines` and
`extractions` on the host.

`POST /v1/systemone` chooses the closed `questions` or `fields` contract from
the deployment API. The other API's key, whatever its value (both keys
included), and undeclared field types are refused before activation. Lookup
now precedes contract validation on this path, for decisions too: an unknown
model is reported before a malformed contract. Setting shapes are checked
on every type; their meaning and agreement belong to the engine. Responses
use decisions' completion evidence and 16 MiB bound; host ingress checks
its own 8 MiB request bound. Extractions use decisions' routing, admission,
leases, settlement, request IDs, trace context and metrics; requests cut
before a whole answer settle only at verified stop. Discovery lists
`capyctl.api: "extractions"` and `capyctl.field_types`. Readiness is a fixed
extraction of each declared type, accepting both published forecast forms;
the wake reference uses ADR 0041's typed comparisons and inclusive tolerances.
SPEC amendments cover §6.1, §10, §15.1/§16 and T50; deployment, request,
engine, error and operations pages document the contract.

Remote launches resolve the declared types from the frozen custom manifest and
require both host capabilities. The router retains its System One lookup through
admission instead of repeating it; chat and embeddings keep their existing
validation order. Extraction wake comparisons apply the decision canary's
inclusive tolerance with its `1e-9` rounding allowance, and readiness receipts
distinguish the host's report from evidence observed by the server.

Registration shows field types for administrator confirmation. Before
downgrading to a release without extraction support, remove every extraction
engine with `capyctl engine remove`. Older releases refuse the whole
`custom-engines.yaml` file if any manifest has `field_types`, preventing role
startup.

Verification: `scripts/ci-local.sh --deep` passed every step at `25ab2c80`
(fmt, name, clippy, unit, runtime, shellcheck, installer, changes, core and
workspace). A local smoke on a maintainer laptop (RTX 4090 Laptop GPU, isolated
standalone, `capyctl-extract` 0.1.0 from its own repository with a random
tiny Qwen3.5-architecture checkpoint on CPU torch): `engine add --custom`
showed the field types; the deployment became ready on the fixed extraction
of all nine types, and again after a stop and start; a declared subset
(`[number, text]`, stored as `text, number`) became ready on its smaller
probe; through the router the fixed request answered 200 with nine answers
and `capyctl.metrics`; `questions`, both keys and chat were `api_mismatch`,
an unknown setting `invalid_request`, an undeclared `date`
`field_type_unsupported`, an engine-side `min > max` `engine_rejected`, and
`/v1/models` listed `capyctl.api` and `capyctl.field_types`. A random
checkpoint proves the plumbing, not extraction quality. CPU tests and this
smoke are not qualification. CapyCTL's own model, `capyctl-extract-4b`, is
untrained. The first live qualification still needs its server, `capyctl-extract`,
on host A, in the serving environment approved on 2026-11-04 once it is made.
ADR 0041 ships in the first release after the model is trained.

# Release note: Extraction models

- **Typed extraction at `POST /v1/systemone`.** A deployment with
  `api: extractions` answers `fields`: text, numbers, dates, times, datetimes,
  durations, lists, estimates and forecasts, as declared by its custom-engine
  manifest. It uses the same endpoint and API key as decisions, with the
  deployment API choosing the contract. Unsupported types and the wrong shape
  are refused before waking a model. Only custom engines are accepted, on one
  machine, restart-only. Before downgrading to a release without extraction
  support, remove every extraction engine with `capyctl engine remove`;
  older releases refuse `custom-engines.yaml` containing `field_types`.
  CapyCTL's own extraction model is untrained and has not been qualified live;
  CPU tests are not qualification
  ([Extraction models](../guide/deploy.md#extraction-models),
  [Extraction requests](../guide/requests.md#extraction-requests)).

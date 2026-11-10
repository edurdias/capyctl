# Status: Extraction models: `api: extractions` and `fields` on `POST /v1/systemone` (ADR 0041) — 2026-11-10 (branch `feat/extraction-models-0.2.0`)

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

Verification on the 0.2.0 integration: `scripts/ci-local.sh` passed fmt, name,
clippy, unit, runtime, shellcheck, installer and changes.
`scripts/ci-local.sh --deep` passed those steps plus core and workspace.
The extraction router suite passes all 10 tests, including the added
raw-byte/numeric-spelling regression. CPU and Fake-engine tests are not
qualification.

A local smoke on a maintainer laptop (RTX 4090 Laptop GPU, isolated
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
smoke are not qualification. The owner approved `capyctl-extract-4b`'s recipe
after E1–E10: frozen Qwen3.5-4B-Base, rank-64 attention LoRA, schema-first
layout, v7 span-set lists without refinement, exact computed scalars and
evidence-pointer distributions executed in the extraction server. ADR 0041's
2026-11-10 amendment records the two-seed 15M-token evidence, data v8, training
recipe and RTX PRO 6000/two-Spark hardware. These are pilot results; the
100M-token main run is launched separately and has no final results here.
The approved training environments on hosts A and B, including their installed
versions and flash-linear-attention, are recorded in AGENTS.md.

The owner selected 0.2.0: land `integration/0.2.0` first, then extraction
support with its embeddings/tracing integration retained. This branch is based
on `integration/0.2.0-on-0.1.11` at `7e0774d6` and replays only the nine extraction
commits above that base. Conflict resolution retains raw request/answer bytes
and declared concurrency policy from the newer integration line. Schema v62
follows its existing v61. The first live qualification still needs the trained
model's server, `capyctl-extract`, on host A in the approved serving environment
once it is made. CPU and Fake-engine tests are not qualification.

Table/record extraction is explicitly deferred to a follow-up release. The
proposed `records` contract, readiness implications, occurrence-conditioned
column readout, data, metrics and open questions are in
[Extraction records](../specs/extraction-records.md). This release keeps ADR 0041's
nine-type wire contract unchanged.

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
  This is part of 0.2.0; table/record extraction is deferred. The model recipe
  is selected, but the main training run and live qualification remain release
  gates; CPU and Fake-engine tests are not qualification
  ([Extraction models](../guide/deploy.md#extraction-models),
  [Extraction requests](../guide/requests.md#extraction-requests)).

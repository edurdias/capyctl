# Status: Extraction models: `api: extractions` and `fields` on `POST /v1/systemone` (ADR 0041) — 2026-11-11 (branch `feat/extraction-models-0.2.0`)

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
on every type except scalar-only `null_threshold`, rejected on lists, estimates
and forecasts with 400 `invalid_request`; other meaning and agreement belong
to the engine. Responses use decisions' completion evidence and 16 MiB bound;
host ingress checks
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
follows its existing v61.

Live host-A qualification on 2026-11-10, using release binaries from
`035ce601`: **plumbing PASS (bf16), model quality FAIL for rc1; not a T50
release-gate pass**. The model server was repaired and reinstalled from
`capyctl-extract` commit `2aae3ea` after its original CPU-device default failed
the fixed extraction while `/health` stayed healthy. The repaired installation
was re-confirmed and pinned. All nine types, computed numbers, reference-date
offsets, qualified lists, comparable estimates, history forecasts and an image
returned complete routed answers with metrics through the remote host ingress.
Readiness, discovery, stopped-model pre-activation refusals, explicit restart,
engine-rejection completion and signal-only uncertain-request retention through
verified stop were exercised. No host-wide limits or owner jobs were changed.

The candidate's answers are not consistently plausible: quantity 3 × price 20
plus 10% tax returned 40 rather than 66; comparables 90–130 returned median 12;
a delivery datetime lost its clock time; a damage list omitted a stated item;
and the image amount/datetime were wrong. These failures reproduced with the
evaluated fp32 configuration under a larger per-deployment allocation, so they
are not attributable solely to bf16. The initial smaller-allocation fp32
SIGTERM's sender/cause was not established by the available event evidence.

Two conformance concerns were investigated. A Ready launch had no typed canary
row at the sampled point: the initial successful extraction answer is discarded,
then a second probe and reference write run best-effort after Ready with errors
ignored. Preserving and persisting the initial answer needs a cross-layer fix,
not a speculative live-session patch. Eventual persistence after the second
probe's full bound was not established. Separately, immediate lease closure on
a complete JSON 400 rejection is correct SPEC §10 behavior, not a defect;
ADR 0041 §3 now clarifies rejections versus 5xx engine errors. An own host-role
restart also left its online host's deployment reconciling after 120 seconds;
the retained charge settled on explicit verified stop.

The T50 controller fix now carries the validated startup extraction answer through
the adapter observation and the host's durable launch result. The controller
records that exact typed answer before completing Initialize or opening dispatch
(SPEC §6.1, ADR 0041 §§4–5). A missing or malformed answer, or a failed reference
write, fails the launch with a reason; extractions no longer run the optional
second readiness canary. T50 regressions cover the original answer surviving a
changed later answer, a second probe that would fail, reference-storage failure,
bounded protocol evidence and its durable host replay.

The reconnect investigation reproduced a retained-policy failure: a launch still
resolves while its approved host document is the in-memory previous publication,
but that document disappears when the host restarts with a newer publication.
Reproof then fails before sending the fixed extraction. The host now journals
the exact approved profile snapshot with the launch and can recover it for
retained-launch checks after restart (ADR 0018 §3, ADR 0031 §§5, 7). Recovery
requires the active profile and every non-profile host setting to remain
unchanged; an unknown or changed ownership, installation, key or safety policy
does not reopen ingress. T50 exercises the reopened host journal and ingress
against the Fake engine's `/v1/systemone` mode, including failed health,
malformed extraction and wrong-owner refusals. The saved live evidence does
not identify its failed reproof stage, so this reproduced cause is not claimed
as a confirmed diagnosis of that specific host-A attempt.

Before the fix, the first-answer regression failed because the successful
launch had no `wake_canaries` row. The retained-policy regression failed with
zero extraction requests after reopening the host journal, where one fixed
extraction was required. An unchanged-policy host restart already passed;
the failure was lost approved context, not a missing extraction branch.
Completed-command compaction removes the full saved policy payload, and a
verified stop evicts its in-memory copy; neither happens on uncertainty.

Verification for these fixes on 2026-11-11: both failing-before T50 regressions
pass after the fixes. `scripts/ci-local.sh` passed fmt, name, clippy, unit,
runtime, shellcheck, installer and changes. `scripts/ci-local.sh --deep`
passed all eight steps plus core and workspace. The read-only correctness and
security reviews found no remaining functional or security defect; the saved
policy payloads now follow completed-command compaction, and an unreadable
startup policy snapshot emits a bounded diagnostic while staying closed.

Both startup-reference persistence and host reconnect need live T50
re-qualification. No lab host was accessed for these fixes. CPU and Fake-engine
tests are not qualification, and rc1's model-quality failures remain unchanged.

The [live qualification record](../runbooks/f2-current-status.md#extraction-model-live-qualification-outcome--2026-11-10-branch-featextraction-models-020)
contains pass/fail tables, requests/answers, source and binary/checkpoint hashes,
defect locations/proposed fixes, and cleanup evidence. The final ledger had no
live resources or leases; every started process was stopped. Only the installed
release binary and authorized bf16 registration context remain on host A,
alongside the separately repaired approved serving environment. AGENTS.md now
records that environment as created, with its fingerprinted versions. The owner
will produce a new candidate in a quality phase and rerun T50; CPU and
Fake-engine tests are not qualification.

Table/record extraction is explicitly deferred to a follow-up release. The
proposed `records` contract, readiness implications, occurrence-conditioned
column readout, data, metrics and open questions are in
[Extraction records](../specs/extraction-records.md). This release retains ADR 0041's
nine field types with the pre-release wire amendments below.

The day-one contract now specifies scalar nulling at `p_null >= null_threshold`,
half-open Unicode-code-point spans, percentile 1–99 quantile arrays (one array
per forecast step), and `state_text` echo with unchanged `return_state_text`.
`mode` and `computation` stay internal as `raw['_mode']` and
`raw['_computation']`. Measured state/image/schema token usage aggregates
across prefix passes; schema accounting includes both copies, markers,
reference/control text and candidate suffixes, while `passes` counts full-prefix
field groups rather than cached suffix evaluations. The model's 8192-token
context reports truncation; PDFs and image text extraction are unsupported.
Image-only text and text-list fields are refused before inference with
`field_unsupported_input` by the model engine, not globally by the router.
Other image field types remain supported. The guide and ADR record deliberate
route, images/assets, options, string-error and 8 MiB boundary differences;
a future `/api/alpha/extractions` adapter is outside 0.2.0. Adapter and router
regressions cover non-scalar thresholds without tightening other irrelevant
settings.

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
  is selected, but the rc1 live run failed model quality despite passing the
  routed plumbing checks. A new candidate and T50 rerun remain release gates;
  CPU and Fake-engine tests are not qualification
  ([Extraction models](../guide/deploy.md#extraction-models),
  [Extraction requests](../guide/requests.md#extraction-requests)).
- **Extraction startup and retained-host recovery.** The typed canary reference is
  now the actual startup answer, persisted before a launch succeeds; persistence
  failure leaves dispatch closed. A retained engine can recover its exact approved
  profile after an unrelated profile publication and host restart, without
  adopting changed engine credentials or safety policy. Live T50 re-qualification
  remains pending.

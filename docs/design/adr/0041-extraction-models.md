# ADR 0041 — Extraction models (`api: extractions`, `fields` on `POST /v1/systemone`)

**Status:** Accepted (generic API decisions of 2026-11-04, PDF contract amendment of
2026-11-12, and release-scope decision of 2026-11-23).
Target release: 0.2.0, in one combined integration and extraction release-candidate
pull request. Extraction is an engine capability, independent of any model project.
Current built-in native engine paths remain refused; a patched engine that serves
the contract can use the existing custom-engine manifest mechanism (ADR 0031).
**Amends (with the implementation):** `SPEC.md` §6.1 (readiness of an extraction model and
its wake check), §10 (a second closed shape on `POST /v1/systemone`, chosen by the
deployment's `api`), §15.1/§16 (a fourth `api` value), §20 (T50); ADR 0030 §§1, 2, 5
(`api` gains `extractions`; `/v1/systemone` answers `fields` for extraction deployments;
`capyctl.api` gains a value); ADR 0031 §2 (a custom engine's manifest `api` gains
`extractions`, with `field_types`).
**Related:** ADR 0012 (deep parking), ADR 0013 (instances and request leases), ADR 0017
(capability gating), ADR 0024 (parsers apply to chat only), ADR 0028 (groups), ADR 0032
(settling a cancelled request), ADR 0033 (LMCache), ADR 0035 (several GPUs on one host),
ADR 0036 (embeddings), ADR 0037 (trace context).

## Context

Typed extraction returns named values from a caller's state rather than selecting
from enumerated decision options (ADR 0030) or requiring clients to parse chat
output. The contract covers:

- **extracted** text, a span of the input with its character offsets;
- **computed** numbers, dates, times, datetimes, durations and lists, read from the input
  or worked out from it (a price plus its tax; the days between two dates);
- **estimated** values the input gives evidence for, as a distribution inside bounds the
  caller declares, with a median and an 80% interval;
- **forecast** values, one estimate per future step of a series.

The request is a `state`, optional media and a map of typed `fields`. Each scalar
answer carries `value` (null when the input does not determine it), `guess`,
`p_null` and `confidence`; an estimate carries `median`, `p10`, `p90`, `p_below`
and `p_above`. Engines own the inference implementation, including whether it
generates tokens internally. CapyCTL owns the typed boundary, routing and evidence.

### Engine support boundary

The accepted engine-support target includes vLLM, SGLang, llama.cpp, TensorRT-LLM
and TensorFold, including patched or custom versions that serve
`POST /v1/systemone` with `fields`. This is not a claim that their current native
implementations serve the contract: none of CapyCTL's built-in unpatched engine
paths does, and those paths remain refused.

A patched executable can be registered with `capyctl engine add --custom` using
a manifest declaring `api: extractions` and its supported `field_types`. For
example, `vllm-extraction` can name a profile for a patched vLLM installation.
Registration does not qualify the engine or enable its built-in native path.
Native support requires an engine release implementing the contract and live
qualification before its capability gate is opened.

### Baseline before extraction support

- Before this implementation, `api` is `chat`, `decisions` or `embeddings` (`DeploymentApi`,
  `crates/capyctl-config/src/effective.rs`). `chat` is omitted from the frozen revision,
  so a revision without `api` keeps its fingerprint.
- `POST /v1/systemone` is ADR 0030's closed decision contract: `model`, `state`,
  `questions`, `images`, `files` (`crates/capyctl-adapters/src/forward.rs`). The router
  and the host ingress pick that contract from the path.
- A custom engine's manifest (`CustomManifest`, `crates/capyctl-config/src/custom.rs`)
  states `api`; a deployment's `api` must equal it. The manifest's digest is the SHA-256
  of its serialized form and part of the profile and the recipe fingerprint.
- Readiness of a non-chat model is a fixed request of the model's API, judged
  well-formed (ADR 0030 §3, ADR 0036); the first answer of a launch is the wake canary's
  reference, stored by kind and probe version in `wake_canaries`. On the combined
  integration, embeddings extend it at schema v60 and extractions at v63.

## Decision

### 1. A deployment declares `api: extractions`

`api` gains a fourth value, `extractions`. It is part of the frozen revision and of its
identity, as ADR 0030 §1 made `decisions`; revisions without it keep their fingerprints.

Resolution currently accepts `api: extractions` only on a custom engine whose
manifest says `api: extractions` (§2). Every built-in native engine path, at every
currently supported version, is refused with `capability_missing:extractions`:
none serves the contract. The engine-support target above does not bypass this
gate; enabling a native path requires a qualified implementation and an amendment.

An extraction deployment keeps ADR 0030 §7's scope:

1. **One machine.** A `topology` is refused, with ADR 0030's wording for decisions
   ("an extractions deployment runs on one machine").
2. **Restart instead of park.** Residency resolves `restart_only`; `deep` and
   `host_backed` are refused. A custom engine cannot park anyway (ADR 0031 §4).
3. **No parser.** `tool_call_parser` and `reasoning_parser` are refused (ADR 0024 applies
   to chat only).
4. **No readout in CapyCTL.** CapyCTL forwards to the engine's own `/v1/systemone`; it
   never turns another model into an extraction model.

LMCache resolves off, as for every custom engine (ADR 0033). A host advertises the
capability `extractions` (ADR 0017); the server never places an extractions deployment on
a host without it (`host_capability_missing:extractions`), and an extractions custom
engine needs both `custom_engines` and `extractions`.

### 2. The manifest declares the field types it answers

ADR 0031's manifest gains `api: extractions` and a field `field_types`:

```yaml
kind: custom_engine
schema_version: 1
api: extractions
field_types: [text, number, date, time, datetime, duration, list, estimate, forecast]
version: "0.1.0"
args: [--host, "{host}", --port, "{port}", --model, "{model_dir}", --served-name, "{served_name}"]
health: /health
```

- `field_types` is required with `api: extractions` and refused with any other `api`
  (`custom_manifest_invalid`).
- It is a non-empty list of distinct names from the closed set `text`, `number`, `date`,
  `time`, `datetime`, `duration`, `list`, `estimate`, `forecast`. An unknown name, a
  duplicate or an empty list is `custom_manifest_invalid`.
- It is stored in that canonical order whatever order the manifest lists it in, so the
  manifest's digest names the set, not its spelling. It is absent from the serialized
  manifest of every other `api`, so existing manifests keep their digests.
- `list` covers lists of text, numbers and dates (`items`).

Changing `field_types` changes the manifest's digest, so it is a new profile, as every
manifest change is (ADR 0031 §3). `engine add --custom` shows `field_types` beside `api`
in what it asks the administrator to confirm (ADR 0031 §8), since it is part of what they
approve.

**Downgrade.** `custom-engines.yaml` is read strictly (ADR 0031 A1), so a release before
this one refuses the whole file once it holds a manifest with `field_types`, and its role
does not start. Before downgrading, the administrator removes every extraction engine with
`capyctl engine remove`; the guide and the change file say so. A separate file, as A1
chose for custom engines, was not taken: extraction engines are custom engines, and the
release that first writes `field_types` is the release that first reads it.

### 3. `POST /v1/systemone` answers `fields` for an extractions deployment

The router serves extractions on the same path, with the same API key, body bound, lookup
of the deployment by `model`, waiting, activation on request, admission, instance
selection, request lease, request IDs, trace context and metrics as decisions (ADR 0030
§2, SPEC §10). The deployment's `api` chooses the contract:

- a decisions deployment takes ADR 0030's `questions` contract, unchanged;
- an extractions deployment takes the `fields` contract below;
- any other deployment refuses the path with `api_mismatch`, as today.

**Order of checks on `/v1/systemone`.** The contract depends on the deployment, so the
router looks the deployment up before it validates the body: authenticate, check
`X-Request-Id`, read the body under the bound, read `model`, look the deployment up
(`unknown_model`), refuse a mismatched API (`api_mismatch`), validate the body against the
deployment's contract (`invalid_request`, `field_type_unsupported`), then wait, activate,
admit and lease. For decisions this moves one observable thing: an unknown model is now
reported before a malformed body. The `api` and the `field_types` are those of the
deployment's current frozen revision.

**Shape mismatch.** A body that carries the other API's key, whatever its value (`null`
included), is refused with HTTP 400 `api_mismatch` before any waiting or activation,
naming what the model answers: a `questions` member sent to an extractions deployment,
or a `fields` member sent to a decisions deployment. A body with both keys is refused the
same way, on the key the model does not take. An extractions deployment asked on
`/v1/chat/completions` or `/v1/embeddings` is `api_mismatch` naming `POST /v1/systemone`.

**The `fields` contract.** The router checks shapes, as SPEC §10 has it for decisions:
members, types, enumerations and ranges. What a setting means for a field, and whether
settings agree with each other, is the engine's to judge (its rejection comes back as
`engine_rejected`). The top level and each field spec are closed; anything outside them is
HTTP 400 `invalid_request` naming the member, before activation:

| Member | Rule |
|---|---|
| `model` | required, a non-empty string |
| `state` | optional string; absent or empty only when `images` is non-empty or a PDF asset is supplied. With a PDF, the only non-empty accepted value is its sole asset key; that key is not source text |
| `fields` | required, an object with at least one member; each name matches `^[A-Za-z_][A-Za-z0-9_.\-]{0,63}$`; each value a field spec (below) |
| `images` | optional array of at most 4 items, each a string beginning `data:` (any case) or an object with exactly `content_type` and `base64`, both strings. CapyCTL does not decode them; their bytes count toward the body bound |
| `assets` | optional named map with exactly one non-empty key and a closed object: `format: "pdf"`, non-empty canonical standard-base64 `data_base64`, and optional `type: "document"`. No other formats or members, multiple assets, non-empty text plus PDF, or non-empty `images` plus PDF |
| `reference_date` | optional string, an ISO calendar date `YYYY-MM-DD` |
| `options` | optional object with only `include_quantiles` and `return_state_text`, both booleans |

A field spec is an object with a required `type`, one of the nine names, and any of the
settings below, checked for shape on every type except the scalar-only threshold:

| Setting | Rule |
|---|---|
| `instructions` | string |
| `null_threshold` | scalar types (`text` through `duration`) only; number, `0 < x ≤ 1`; refused on `list`, `estimate` and `forecast` with HTTP 400 `invalid_request` |
| `unit` | string |
| `integer` | boolean |
| `decimals` | integer 0 to 6 |
| `items` | `text`, `number` or `date`; required on a `list` |
| `min`, `max` | each a finite number or an ISO date `YYYY-MM-DD`; both required on an `estimate` and a `forecast` |
| `step` | a positive finite number, or `month` or `year`; a number with date bounds counts days, which CapyCTL does not interpret |
| `scale` | `linear` or `log` |
| `horizon` | integer 1 to 64 |
| `interval` | `step`, `minute`, `hour`, `day`, `week`, `month`, `quarter` or `year` |

An unknown setting, or a value outside its rule, is `invalid_request` naming the field
and the setting. Apart from `null_threshold`, a setting on a type it means nothing
for (`horizon` on a `text`, say) is forwarded.

**Declared types.** A field whose `type` is not in the deployment's `field_types` is
refused with HTTP 400 `field_type_unsupported` naming the field and the type, before any
waiting or activation. The router reads the declared types from the frozen revision's
custom profile, never from the engine.

CapyCTL sets no limit on the number of fields, and none on the size of images beyond the
body bound (the count is the contract's four); an engine's own limit comes back as its
rejection (`engine_rejected`, ADR 0030 §2).

**Forwarding and evidence.** As for decisions: the body goes to the engine with `model`
replaced by the served name and comes back whole with `model` replaced by the public name.
A response is complete evidence when it is a whole JSON object whose `answers` is a
non-empty object of objects each naming a string `type`, the rule decisions use (SPEC
§10); an engine 400, 413 or 422 with a complete JSON body is a rejection; anything else
leaves the request uncertain. The response is read whole up to the decision bound (16
MiB); a longer one (99 quantiles per estimate with `include_quantiles`, a state echoed by
`return_state_text`) is uncertain, is never replayed and is held until the instance
stops, as for decisions. CapyCTL does not judge individual answers on the request path,
except PDF evidence: `state_text` is mandatory, reported text spans must match that
returned text in Unicode code points, and optional `page`/parallel `pages` must be
positive one-based page numbers (or null when no single page applies). An invalid
PDF response leaves the exchange uncertain, never completed against the asset key.
The host ingress permits `/v1/systemone` for an extraction launch with this contract and
its own body check (8 MiB; a larger body is refused with HTTP 400 on a host),
choosing the contract from the launch's `api`, never from the body.

**Settlement.** An extraction request is settled as a decision is (SPEC §10, ADR 0032):
the engine takes no request ID, so a request cut by a hang-up or a deadline before its
whole answer settles only at the instance's next verified stop, and ADR 0032 §7's restart
for held requests applies. A complete JSON rejection (HTTP 400, 413 or 422)
closes the lease as completed; a complete engine error answer (`engine_error`,
HTTP 5xx other than 503) leaves it `cancelling`, settled on the manifest's idle
signal when it has one (ADR 0031 §6), otherwise at the next verified stop.

**Metrics.** An answer carries `capyctl.metrics` with the figures a decision carries
(SPEC §10): `total_ms`, `time_to_response_ms`, and `input_tokens` from the engine's
`usage`. No generated-text figure is recorded.

**Day-one answer contract.** Scalar `value` is null when `p_null >= null_threshold`
(using the engine's default threshold when omitted); otherwise it equals `guess`.
Text `span` and text-list `spans` are half-open `[start, end)` offsets in Unicode
code points of `state`, not bytes or UTF-16 code units; for PDF input they index the
mandatory returned `state_text` instead. A single-page span may include `page`;
a list may include `pages`, parallel to `spans`, with null for a cross-page span.
`include_quantiles` adds `quantiles`: a 99-element array for percentiles 1 through
99 on an estimate, and one such array per forecast step. `return_state_text`
echoes the input under response member `state_text`, not `state`; PDF input always
returns `state_text`, even when this option is absent or false.
`mode` and `computation` are never public response members; engine diagnostics
do not add members to the public contract.

Engines report their measured input usage; CapyCTL does not infer model internals
or enforce a particular inference architecture through usage accounting.

**PDF amendment (2026-11-12).** The engine alone decodes and converts PDFs;
CapyCTL forwards the asset and state unchanged, apart from the normal model-name
replacement. Returned `state_text` is the source of text-span evidence, not the
asset key. Unsupported, malformed or over-limit input is the engine's to reject;
CapyCTL relays a complete rejection under its existing `engine_rejected`
envelope. Engine-specific decoded-size and page limits do not replace CapyCTL's
unchanged 8 MiB HTTP request bound, which includes base64 overhead.

**Compatibility boundary.** A future `/api/alpha/extractions` boundary adapter
may translate OpenRouter's live Extractions contract; it is not part of 0.2.0.
The deliberate differences are:

| Area | CapyCTL 0.2.0 | OpenRouter Extractions |
|---|---|---|
| Route | `POST /v1/systemone`, selected by deployment API | `/api/alpha/extractions` |
| Asset input | existing top-level `images`; OpenRouter-aligned named `assets` map restricted to one PDF | broader `assets` support |
| Options | top-level `options` | `provider.options` |
| Errors | string error codes | numeric error codes |

Only the named PDF asset map is aligned with OpenRouter. No broader boundary
adapter, compatibility shim or additional response-readiness requirement is
introduced; the fixed text-only readiness probe remains unchanged.

### 4. Readiness is a fixed extraction

A decisions deployment proves readiness with a fixed decision (ADR 0030 §3); an
extractions deployment sends one fixed `/v1/systemone` request with **one field of each
type its manifest declares**, to the launch's own loopback endpoint with its own key,
never through ingress or the router. It is version `1` of the probe of kind `extraction`
(`crates/capyctl-adapters/src/extraction_probe.rs`). The request, with every type
declared:

```json
{
  "model": "<the engine's served model name>",
  "state": "Order A-88412 was delivered on 2026-09-28 at 14:30 with a cracked screen and a dented corner. The customer paid 649.00 USD and wrote on 2026-10-09. Returns logged on the last five days: 12, 15, 11, 14, 13.",
  "reference_date": "2026-10-09",
  "fields": {
    "order_id": {"type": "text", "instructions": "The order number"},
    "amount_paid": {"type": "number", "decimals": 2, "unit": "USD", "instructions": "The amount the customer paid"},
    "delivered_on": {"type": "date", "instructions": "The delivery date"},
    "delivered_time": {"type": "time", "instructions": "The time of the delivery"},
    "delivered_at": {"type": "datetime", "instructions": "The date and time of the delivery"},
    "days_waited": {"type": "duration", "unit": "days", "integer": true, "instructions": "Days from the delivery to the date the customer wrote"},
    "damage": {"type": "list", "items": "text", "instructions": "Each kind of damage described"},
    "returns_today": {"type": "estimate", "min": 0, "max": 100, "step": 1, "instructions": "Returns logged on the day the customer wrote"},
    "returns_next": {"type": "forecast", "horizon": 2, "interval": "day", "min": 0, "max": 100, "step": 1, "instructions": "Returns logged on each of the next two days"}
  }
}
```

A manifest that declares fewer types gets the same request with only the fields of its
types. CapyCTL sends the request's members in a fixed order (sorted by name), so the
bytes are the same for every launch with the same declared types; the order carries no
meaning. The state is ASCII, so character offsets mean the same in every unit an engine
may count in.

The answer is ready evidence when it is a whole JSON object whose `answers` object has
exactly the requested field names, each answer's `type` equal to the field's type, and
each well-formed. Readiness proves the model answers in its types, not that it is right:
no value is compared with the state's facts, and a `null` `value` is well-formed.

| Type | Well-formed answer |
|---|---|
| every scalar type (`text` to `duration`) | `p_null` and `confidence` finite in [0, 1]; `guess` present, not null, and well-formed for the type; `value` null or equal to `guess` (the probe sends no `null_threshold`, so the engine's default applies) |
| `text` | `guess` a string; `span` two integers `[start, end]` with `0 ≤ start < end ≤` the state's length, and the state's characters `start..end` equal `guess` |
| `number` | `guess` finite, with at most 2 decimals (`|100·g − round(100·g)| ≤ 1e-6`, a test meant for the probe's small values) |
| `date` | `guess` a valid calendar date `YYYY-MM-DD` |
| `time` | `guess` `HH:MM`, `HH:MM:SS` or `HH:MM:SS.fff…`, hours 00–23, minutes and seconds 00–59, then optionally `Z` or `±HH:MM` |
| `datetime` | a date, `T`, a time as above |
| `duration` | `guess` a finite number with no fractional part (`11` and `11.0` both pass) |
| `list` | `value` null (then `spans` null or absent), or an array of strings with `spans` an array of the same length, each span valid for the state as for `text` and equal to its item; `confidence`, if present, finite in [0, 1]. An empty list is well-formed; the order of the items is the engine's |
| `estimate` | `value`, `median`, `p10`, `p90` finite with `0 ≤ p10 ≤ median ≤ p90 ≤ 100`, `value` within [0, 100], each with no fractional part (the step-1 grid); `p_below` and `p_above` finite in [0, 1] |
| `forecast` | at least one of the two accepted forms, and every form present well-formed: `value`, `median`, `p10` and `p90` arrays of 2 numbers, or `steps` an array of 2 objects each with `value`, `median`, `p10` and `p90`; each step obeys the estimate's order, bounds and grid rules; `p_below` and `p_above` are optional here and, when present (per step or per object), finite in [0, 1] |

The accepted `time` and `datetime` spellings follow the ISO 8601 forms above.

Members the table does not name (`mean`, `unit`, `quantiles`, `usage`, `truncated`, ...)
are allowed and ignored. The kind, the version and the declared types are recorded with
the wake canary's reference (§5); any change to the request is a new version, and a
reference recorded by another version or another set of types is never compared with it.
This holds at start, after a restart, and when a host re-proves a launch after
reconnecting; `/health` and `/v1/models` alone are not readiness (SPEC §6.1). Before the
probe, ADR 0031 §5's order applies: the port, then the manifest's `health` path.

### 5. The wake check

The first answer to the fixed request is recorded as the launch's reference, as for
decisions. A later answer passes when, field by field:

- **text, number, date, time, datetime, duration:** `guess` is equal (a text's `span`
  too; numbers within `1e-9` relative). Whether `value` is null is not compared: it
  follows `p_null` against a threshold, and `p_null` may move within the tolerance below;
- **list:** the same items and spans, in order, or null in both;
- **estimate and forecast:** `value`, `median`, `p10` and `p90` (each step of a forecast,
  in every form the reference has) each within one `step` of the reference, the probe's
  step being 1;
- **every probability** (`p_null`, `confidence`, `p_below`, `p_above`) within 0.05 of the
  reference, ADR 0030 §4's tolerance; one present in one answer and absent in the other
  fails.

Every tolerance is inclusive, with the decision canary's `1e-9` allowance for decimal
answers read as binary floats (ADR 0030 §4, `wake_canary.rs`), so a move from `0.85` to
`0.90` passes as it does for a decision. Discrete answers must repeat exactly
(owner decision 2026-11-04). This is the wake-evidence contract, not a claim about
the numerical behavior of an unqualified engine.

Anything else fails with `wake_mismatch`, and the instance is stopped, as today. With
`restart_only` (§1) and no LMCache, every restart records a new first answer, so in the
first version the check has nothing to compare, exactly as for a decision model without
LMCache (`docs/guide/parking.md`). It is defined now so that parking and LMCache can
follow without a new probe.

### 6. Discovery

`GET /v1/models` keeps its shape. An extractions entry has `capyctl.api: "extractions"`
and `capyctl.field_types`, the declared types in canonical order, so a client knows which
fields the model answers without sending one.

### 7. Refusal codes

| Where | Code | When |
|---|---|---|
| resolution | `capability_missing:extractions` | `api: extractions` on a native engine, or on a custom engine whose manifest says another `api` |
| resolution | `capability_missing:chat`, `capability_missing:decisions`, `capability_missing:embeddings` | that `api` on a custom engine whose manifest says `extractions` (ADR 0031 §2) |
| document check | invalid `topology` / `residency` | a `topology`, or `deep` or `host_backed` (§1) |
| resolution | `custom_field_unsupported:<parser>` | a parser (ADR 0031 §4) |
| `engine add --custom` | `custom_manifest_invalid` | `field_types` missing with `api: extractions`, present with another `api`, empty, duplicated, or naming an unknown type |
| placement | `host_capability_missing:extractions` | a host without the capability |
| router (400) | `api_mismatch` | the wrong endpoint, or a body with the other System One API's key (§3) |
| router (400) | `invalid_request` | a body outside the `fields` contract (§3) |
| router (400) | `field_type_unsupported` | a field of a type the deployment does not declare (§3) |
| readiness | none: the instance never becomes ready, and the start fails as a decision model's does | the fixed extraction is not answered well-formed (§4) |
| wake | `wake_mismatch` | §5 |

### 8. Out of scope

- Built-in native extraction paths until qualified (§1), groups, several GPUs
  (ADR 0035 refuses custom engines), parking and LMCache.
- Model architecture, training, quality evaluation and model publication.
- General validation of answers on the request path beyond the PDF evidence
  requirements in §3, and any CapyCTL limit on field count or decoded media size.

## Consequences

- A fourth `api` value reaches every closed match: config, the custom manifest, the
  protocol's probe and evidence messages, the store's API column and wake-canary table
  (schema 63), the router, the host ingress, trace attributes, the controller's readiness,
  remote execution and canary, management's placement gate, the CLI's exit codes, and the
  fake engine.
- Documentation changes with the implementation: the SPEC sections above, the guide
  (`deploy.md`, `requests.md`, `engines.md`, `errors.md`), and the operations pages that
  list `api` values or inference paths (`configuration.md`, `network-access.md`), plus
  `data-retention.md` for images carried in request bodies.
- `/v1/systemone` stops being one contract: the router and the ingress choose it from the
  deployment or launch, never from the body.
- A new conformance row, **T50 — extraction models**: `api: extractions` resolves only on a
  custom engine with a matching manifest and round-trips through the frozen revision; a
  revision without it keeps its fingerprint; `field_types` is validated, normalized and
  digested; native engines, a `topology`, a parking residency and a parser are refused; an
  old host never receives an extractions deployment; `POST /v1/systemone` validates the
  `fields` contract, refuses an undeclared type and the other API's shape before
  activation, and is routed, admitted, leased and settled like a decision; readiness passes
  only on a well-formed fixed extraction of the declared types; the canary passes within
  tolerance and fails on a changed value, a move of more than one step or a probability
  moved by more than 0.05.
- CPU and fake-engine tests are not qualification. Each engine implementation must
  be qualified live; custom registration alone is not such evidence.

## Alternatives considered

- **`POST /v1/extractions`.** Rejected by the owner (2026-11-04): the deployment's
  `api` already tells CapyCTL which contract applies on `/v1/systemone`.
- **`api: decisions` accepting both shapes.** Rejected: readiness could not tell which
  shape to probe, and a wrong-shape request would wake the model only to be refused, the
  failure ADR 0030 §1 designed out.
- **One `api: systemone` with a list of shapes.** Rejected: it renames ADR 0030's frozen
  value and changes every decisions revision's fingerprint.
- **A fixed probe of all nine types for every model.** Chosen first, then replaced by
  declared types (owner, 2026-11-04): it would leave any engine implementing fewer
  types never ready.
- **Extraction on top of chat models in the router.** Rejected: inference belongs
  to the engine, and SPEC §10 forbids the router to rewrite prompts.

## Amendment of 2026-11-23: combined 0.2.0 integration

The owner chose one release-candidate pull request containing the 0.2.0
integration and extraction support, rather than requiring the integration to land
separately. Main's schema v59 remains the browser-auth migration; embeddings,
LMCache defaults, tracing and extraction use v60–63 in that order. Earlier
integration databases with conflicting v59–62 stamps are refused rather than
migrated under the wrong schema assumptions. Existing integration behavior,
readiness evidence and lifecycle/lease gates remain intact.

Table/record extraction is deferred to a follow-up release. It is not another
spelling of `list`, is not an accepted field type, and does not change this
release's probes. The proposed contract and remaining decisions are in
[Extraction records](../../specs/extraction-records.md).

**Input-capability gap.** A manifest declares `field_types`, not accepted input
modalities. The router therefore cannot reject images or PDFs before activation
based on a text-only engine's declaration. Engines must reject unsupported inputs
themselves; the fixed text-only readiness probe proves no image or PDF support.
No text-only/media capability declaration or media qualification is included in
this release. The current PDF completion checks remain strict.

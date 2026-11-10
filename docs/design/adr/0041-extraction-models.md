# ADR 0041 — Extraction models (`api: extractions`, `fields` on `POST /v1/systemone`)

**Status:** Accepted (owner decisions 2026-11-04 and amendment of 2026-11-10).
Target release: 0.2.0, with `integration/0.2.0` landing first and extraction support
on top, alongside `capyctl-extract-4b`. The first engine is
CapyCTL's own extraction server run as a custom engine (ADR 0031); no native engine
serves extractions, and every native engine is refused until one does and is qualified.
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

On 2026-10-19 Cloudflare announced Clairic, "a new class of System One-style models:
extraction models" ([blog](https://blog.cloudflare.com/clairic-extraction-model/),
[model reference](https://developers.cloudflare.com/workers-ai/models/clairic/), read
2026-11-03). Where a decision model (ADR 0030) gives a probability to every option the
caller lists, an extraction model returns values the caller cannot list in advance, in one
forward pass and with no generated tokens:

- **extracted** text, a span of the input with its character offsets;
- **computed** numbers, dates, times, datetimes, durations and lists, read from the input
  or worked out from it (a price plus its tax; the days between two dates);
- **estimated** values the input gives evidence for, as a distribution inside bounds the
  caller declares, with a median and an 80% interval;
- **forecast** values, one estimate per future step of a series.

The request is a `state` (text, up to four images) and a map of typed `fields`. Each
scalar answer carries `value` (null when the input does not determine it), `guess`,
`p_null` and `confidence`; an estimate carries `median`, `p10`, `p90`, `p_below` and
`p_above`. Clairic's input schema
([schema-input.json](https://developers.cloudflare.com/workers-ai/models/clairic/schema-input.json))
lists `model`, `state`, `fields`, `images`, `reference_date` and `options`; the schema
itself does not close the object, but the hosted endpoint answers any other member with
HTTP 422 (observed by a gateway on 2026-10-23, [Bifrost #8958](https://github.com/maximhq/bifrost/issues/8958)).
For a forecast, the launch post shows `value`, `median`, `p10` and `p90` as arrays of one
number per step, and the output schema
([schema-output.json](https://developers.cloudflare.com/workers-ai/models/clairic/schema-output.json))
names `steps`, "one estimate per step"; both forms are published.

### What serves it

Read 2026-11-03:

| Server | Extraction support |
|---|---|
| Clairic | hosted on Workers AI only (`@cf/cloudflare/clairic`); open weights promised "soon", no date |
| llama.cpp v0.7.0, SGLang 0.5.22, vLLM 0.32.0 | `/v1/systemone` with `questions` only; no `fields` anywhere |
| kenari (a hosted gateway) | `POST /v1/systemone` with `fields` in place of `questions`, exactly one of the two ([docs](http://kenari.id/en/docs/systemone)) |
| Bifrost (a gateway) | proposes a separate `POST /v1/extractions` ([issue #8958](https://github.com/maximhq/bifrost/issues/8958)) |
| open models | `tasksource/UltraBERT-380M-clairic-extraction` and GLiNER2 return spans only (no numbers, dates or distributions) and ship no server; NuExtract3 writes JSON token by token, as a chat model |

No engine CapyCTL runs can serve an extraction model, and none exists in open weights with
the whole type set. The owner decided (2026-11-04) that CapyCTL builds its own: a model
trained from an open base, and a server for it that CapyCTL runs as a custom engine. This
ADR records the decisions about that model (§9) and decides what CapyCTL needs to run it
and any later extraction model.

### What CapyCTL does today

- `api` is `chat`, `decisions` or `embeddings` (`DeploymentApi`,
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
  reference, stored by kind and probe version in `wake_canaries` (its shape from schema
  59; the store is at schema 61).

## Decision

### 1. A deployment declares `api: extractions`

`api` gains a fourth value, `extractions`. It is part of the frozen revision and of its
identity, as ADR 0030 §1 made `decisions`; revisions without it keep their fingerprints.

Resolution accepts `api: extractions` only on a custom engine whose manifest says
`api: extractions` (§2). Every native engine, at every version, is refused with
`capability_missing:extractions`: none serves the contract. A native engine is admitted
by amendment once a release serves this contract and is qualified live; the first planned
is llama.cpp, with the model's output layers ported (§9).

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
| `state` | optional string; absent or empty only when `images` is non-empty (CapyCTL adds no default). Text answers are spans of this string |
| `fields` | required, an object with at least one member; each name matches `^[A-Za-z_][A-Za-z0-9_.\-]{0,63}$`; each value a field spec (below) |
| `images` | optional array of at most 4 items, each a string beginning `data:` (any case) or an object with exactly `content_type` and `base64`, both strings. CapyCTL does not decode them; their bytes count toward the body bound |
| `reference_date` | optional string, an ISO calendar date `YYYY-MM-DD` |
| `options` | optional object with only `include_quantiles` and `return_state_text`, both booleans |

A field spec is an object with a required `type`, one of the nine names, and any of the
settings of Clairic's schema, each checked for its shape on every type:

| Setting | Rule |
|---|---|
| `instructions` | string |
| `null_threshold` | number, `0 < x ≤ 1` |
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
and the setting. A setting on a type it means nothing for (`horizon` on a `text`, say) is
forwarded, as Clairic's schema allows it.

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
stops, as for decisions. CapyCTL does not judge individual answers on the request path.
The host ingress permits `/v1/systemone` for an extraction launch with this contract and
its own body check (8 MiB; a larger body is refused with HTTP 400 on a host, so a body at
Clairic's own 16 MiB limit is not accepted there), choosing the contract from the
launch's `api`, never from the body.

**Settlement.** An extraction request is settled as a decision is (SPEC §10, ADR 0032):
the engine takes no request ID, so a request cut by a hang-up or a deadline before its
whole answer settles only at the instance's next verified stop, and ADR 0032 §7's restart
for held requests applies; a complete engine error answer leaves it `cancelling`, settled
on the manifest's idle signal when it has one (ADR 0031 §6), otherwise at the next
verified stop.

**Metrics.** An answer carries `capyctl.metrics` with the figures a decision carries
(SPEC §10): `total_ms`, `time_to_response_ms`, and `input_tokens` from the engine's
`usage`. No generated-text figure is recorded.

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
| `forecast` | at least one of the two published forms, and every form present well-formed: `value`, `median`, `p10` and `p90` arrays of 2 numbers (the launch post's form), or `steps` an array of 2 objects each with `value`, `median`, `p10` and `p90` (the output schema's form); each step obeys the estimate's order, bounds and grid rules; `p_below` and `p_above` are optional here and, when present (per step or per object), finite in [0, 1] |

The accepted `time` and `datetime` spellings are CapyCTL's reading of ISO 8601; Clairic's
documentation shows only `2026-07-01T13:34:00` [INFERENCE: no other spelling was
observed].

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
`0.90` passes as it does for a decision. Discrete answers must repeat exactly (owner decision
2026-11-04): they are a copied span or a value decoded from a class, not a regression,
so batching noise moves their probabilities, not the value itself [INFERENCE: true of
CapyCTL's own model (§9); measured on no engine yet].

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

- Any native engine (§1), groups, several GPUs (ADR 0035 refuses custom engines),
  parking and LMCache.
- The web UI playground (ADR 0040, not yet on this branch): extraction requests follow
  when both land.
- Validating answers on the request path, and any CapyCTL limit on the number of fields
  or the size of images.

### 9. CapyCTL's extraction model

The model, `capyctl-extract-4b`, and its server live in their own repository,
`capyctl-extract` (a private GitHub repository until weights ship; names decided by the
owner 2026-11-04).
CapyCTL depends only on its server speaking §3's contract and declaring its types in a
custom-engine manifest (§2). The original decisions below are superseded where noted
by the amendment of 2026-11-10:

| Topic | Decision |
|---|---|
| Base | Qwen3.5-4B-Base (Apache-2.0; text and images fused; the base of Kev-4B, which llama.cpp already serves) |
| Types | All nine: text, number, date, time, datetime, duration, list, estimate, forecast |
| Images | In the first model (up to 4, as Clairic); text and text-list fields answer only from the `state` string, since a one-pass model cannot copy text out of an image |
| Training | Frozen backbone, rank-256 LoRA adapters and new output layers (span start and end, `p_null`, typed value heads, distributions on the declared grid), trained with cross-entropy, Brier and CRPS/pinball losses; then weight-space edits only to fix specific failures found in evaluation, each kept only if held-out scores improve |
| Data | Commercial-clean only: permissive sources (tasksource's `extraction` config filtered on `license_use`), code-generated data, and model-generated data only from models whose terms allow it; weights ship under Apache-2.0 |
| Hardware | Pilot runs on host A (one approved training environment, scheduled around live sessions); full runs on rented GPUs, set up and removed by a script as ADR 0035 §14's machine is |
| Serving | Its own server first (this ADR's custom engine), then a llama.cpp port of its output layers; both checked against the same test set |

Each environment this needs is an engine environment under AGENTS.md's rules. The
original 2026-11-04 approval covered training and serving environments on host A
and a rented machine in principle. Subsequent approvals and the installed training
versions on both hosts are recorded in AGENTS.md; see the amendment below.

## Consequences

- A fourth `api` value reaches every closed match: config, the custom manifest, the
  protocol's probe and evidence messages, the store's API column and wake-canary table
  (schema 62), the router, the host ingress, trace attributes, the controller's readiness,
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
- CPU and fake-engine tests are not qualification. The first live qualification is the
  model's own server on host A, after its environment is approved.

## Alternatives considered

- **`POST /v1/extractions`.** Rejected by the owner (2026-11-04): Clairic's request body
  works unchanged on `/v1/systemone`, and kenari serves it there; the deployment's `api`
  already tells CapyCTL which contract applies.
- **`api: decisions` accepting both shapes.** Rejected: readiness could not tell which
  shape to probe, and a wrong-shape request would wake the model only to be refused, the
  failure ADR 0030 §1 designed out.
- **One `api: systemone` with a list of shapes.** Rejected: it renames ADR 0030's frozen
  value and changes every decisions revision's fingerprint.
- **A fixed probe of all nine types for every model.** Chosen first, then replaced by the
  declared types (owner, 2026-11-04): it would leave any model with fewer types, an open
  Clairic among them, never ready.
- **Extraction on top of chat models in the router.** Rejected: it generates tokens, and
  SPEC §10 forbids the router to rewrite prompts.

## Owner decisions of 2026-11-04

1. Base model: Qwen3.5-4B-Base.
2. Field types: all nine.
3. Images: in the first model.
4. Training: adapters and output layers, then weight-space edits as a repair step.
5. Data: commercial-clean only.
6. Training hardware: pilots on host A, full runs rented.
7. Code: a new repository.
8. Serving: own server first, then llama.cpp.
9. Deployment: `api: extractions`; the endpoint `POST /v1/systemone` with `fields`.
10. Readiness: one field per declared type (manifest `field_types`); the wake canary
    allows one grid step on estimates and forecasts and 0.05 on probabilities.
11. Environments: training and serving environments on host A, and a rented machine in
    principle (details first); no copy on host B.
12. Names: the repository and server `capyctl-extract`, the model `capyctl-extract-4b`.
13. Release: the first release after `capyctl-extract-4b` is trained.
14. Branches: `feat/extraction-models` stays local until it is rebased onto `main`;
    `capyctl-extract` is a private GitHub repository.

## Amendment of 2026-11-10: initial model and release scope

The owner approved the following architecture after experiments E1–E10 in the
`capyctl-extract` repository. It replaces the provisional rank-256 recipe in §9.
**The wire contract is unchanged:** the nine field types, request and answer shapes,
manifest declarations, discovery, readiness probe and wake comparisons in §§1–7
remain as accepted. Model internals and evidence pointers are not additional API
members, and CapyCTL does not execute arithmetic in the router.

### Architecture

- Freeze Qwen3.5-4B-Base; train rank-64 LoRA on the attention projections and
  extraction output heads.
- Put the schema first, including each field's schema before the state. The earlier
  BIO list head failed qualifier selection until this layout was introduced.
- Use the v7 span-set list head. Do not include the refinement layer: it gave no
  gain at 15 million tokens.
- Compute scalar values in the extraction server's code. Heads select operand
  pointers, an operation and a unit; exact Decimal and calendar execution handles
  copy plus 13 operations. Direct value heads remain auxiliary fallback heads.
  A digit-wise year head represents years.
- For estimates, point at comparable samples and execute exact empirical quantiles
  in code. Forecast evidence can be per-step scenarios or ordered history; history
  uses a seasonal-naive forecaster in code. Learned distribution heads remain the
  fallback when evidence is unavailable. These pointers select evidence in the
  input; they do not add a new response shape.

### Recipe and hardware

Data v8 mixes the compositional v6 generator (60%), commercially filtered
tasksource data (30%) and rendered receipt images (10%). The optimizer is AdamW
at `2e-4`, one parameter group, 20 warmup steps, warmup-stable-decay with a 20%
decay phase, approximately 64K tokens per step, and bf16.

Experiments used an RTX PRO 6000 rental and the two Spark hosts. Host B's training
environment was subsequently approved on 2026-11-04 ("use it as well") and made
the same way as host A's; installed versions, including flash-linear-attention
and fla-core, are in AGENTS.md. Two-Spark DDP measured 1.73× one Spark. The rank-0
resume broadcast fix made the distributed run crash-safe. This records completed
experiments and approvals, not permission for another rental or live session.

### Measured evidence and limits

At 15 million training tokens, seeds 42 and 7 respectively, on the held-out
compositional suite:

| Metric | Seed 42 | Seed 7 |
|---|---:|---:|
| Text | 0.90 | 0.92 |
| Number | 0.78 | 0.68 |
| Duration | 0.75 | 0.75 |
| Date | 0.91 | 0.84 |
| Datetime | 0.84 | 0.74 |
| Time | 0.91 | 0.82 |
| Non-empty lists, exact | 0.44 | 0.46 |
| Tasksource text | 0.66 | 0.64 |
| Null accuracy | 0.997 | 0.976 |
| Confidence ECE, uncalibrated | 0.14 | 0.18 |

List item F1 was 0.88. Evidence-pointer estimates had 24% lower pinball loss
than bounds-only estimates, and history forecasts improved by 61–65%.
Per-step scenario forecasts were not learned: their improvements were −1% and
−8%, so they are not a demonstrated strength of this release. These are pilot
measurements, not results of the 100-million-token main run or live engine
qualification. The main run is being launched separately; its results and serving
qualification remain release gates.

### Release boundary

Extraction support ships with 0.2.0 after the integration line, preserving its
embeddings and tracing support. This supersedes the original decisions 13–14;
the extraction branch is `feat/extraction-models-0.2.0`, based on
`integration/0.2.0-on-0.1.11`.

Table/record extraction is deferred to a follow-up release. It is not another
spelling of `list`, is not an accepted field type, and does not change this
release's probes. The proposed contract, model approach and remaining decisions
are in [Extraction records](../../specs/extraction-records.md).

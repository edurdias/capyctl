# ADR 0030 — Decision models (`api: decisions`, `POST /v1/systemone`)

**Status:** Accepted (owner decisions 2026-10-17). The deployment field is `api: decisions`;
decision models are the 0.1.5 release; every engine is upgraded to a version that serves
them (SGLang 0.5.22 approved and installed; llama.cpp v0.7.0 already serves them; vLLM and
TensorFold follow as below); the release ships recipes for Clef-Flash, Clef, Kev-4B and
one of Julia-1 or Laya; and the first version keeps the three scope limits of section 7.
Nothing is implemented yet.
**Amended:** 2026-10-21 (owner decision), §6: vLLM stays refused on every version (see
"Amendment 2026-10-21" at the end).
**Amends:** `SPEC.md` §6.1 (readiness of a model that generates no text), §10 (the
inference surface), §15.1/§16 (a deployment field), §20 (T43); ADR 0028 §§9, 11, 12 only
by excluding decision deployments from groups (§7.1); ADR 0024 §1 (parsers apply to chat
deployments only).
**Related:** ADR 0008 (installations), ADR 0012 (deep parking and its protections), ADR
0014 (engine configuration), ADR 0017 (version skew), ADR 0019 (network access), ADR 0029
(llama.cpp).

## Context

A decision model answers typed questions about an input instead of generating text. The
request names a `state` (text, JSON, or chat messages; images on some models) and a map
of questions, each `noul` (yes or no), `choice` (one of named options) or `score` (an
ordered scale); the answer gives every option a probability, in one forward pass, with no
generated tokens. The API is the System One API TypeSafe introduced with Jev (2026-09-15):
`POST /v1/systemone`. Open-weight decision models followed: Cloudflare's Clef (27B, on
Qwen3.8-27B), Clef-Flash (9B, on Qwen3.5-9B) and Clef-omni (30B-A3B), Liquid's d1-3B,
Kev-4B, lev, Laya, Julia-1 and others, mostly Apache-2.0.

What the engines serve (read 2026-10-17; the first two on the lab hosts):

| Engine | Release | `/v1/systemone` |
|---|---|---|
| llama.cpp | v0.7.0 (built on both hosts) | yes: native decision models in GGUF (Clef, Kev, lev, Laya, Julia-1, OpenJev, pplx-decider, vllm-d3, lfm2-d1); `/v1/models` marks them with `architecture.output_modalities: ["decisions"]` |
| SGLang | 0.5.22 (approved 2026-10-17, installed on both hosts) | yes: label scoring for chat models (since 0.5.21), and Clef and Clef-Flash through their joint schema head (0.5.22) |
| vLLM | 0.31.0 | no; the endpoint (Clef-Flash, Clef-omni, d1-3B, Decision 3.0) landed on `main` between 2026-10-08 and 2026-10-12, after 0.31.0 |
| TensorFold | 1.0.12 | yes (d1-3B on CUDA); 1.0 is a native binary that CapyCTL's TensorFold integration (built for the 0.6 Python package) does not run yet |

CapyCTL serves only `GET /v1/models` and `POST /v1/chat/completions`, and it proves a
model usable by generating text: an 8-token chat completion at start, after a restore and
after a host reconnects; a 1-token completion for a group head; an 8-token canary compared
by token at each wake. A decision model generates nothing, so it would launch and never
become ready.

## Decision

### 1. A deployment declares its API

A deployment gains `api`, `chat` (the default) or `decisions`. It is part of the frozen
revision and of its identity: the same checkpoint served as chat and as decisions are two
revisions. `kind` is not reused; it already names the document kind.

Resolution refuses `api: decisions` on an engine installation that cannot serve it, with
`capability_missing:decisions`: llama.cpp v0.7.0 or newer and SGLang 0.5.22 or newer
serve it; vLLM and TensorFold are refused until a supported release (§6). A decisions
deployment gets no reasoning or tool-call parser (ADR 0024 applies to chat deployments).
Its memory is sized as the engine's model of the same checkpoint is today; a decision
request holds no generated tokens, which only lowers its use.

A host advertises the capability `decisions` (ADR 0017). A server never places a
decisions deployment on a host without it, so an older host never receives a deployment
whose checks it would run as chat.

### 2. The inference surface gains `POST /v1/systemone`

The router serves `POST /v1/systemone` beside chat completions, with the same API key,
body bound, lookup of the deployment by `model`, waiting, activation on request, admission,
instance selection and request lease. Only a deployment with `api: decisions` answers it;
a chat deployment refuses it, and a decisions deployment refuses chat completions, each
with HTTP 400 `api_mismatch` before any activation.

The request is validated against its own closed contract, not the chat contract: `model`,
`state`, `questions` (each `type`, `instructions`, `criteria`), and `images`/`files` where
the engine accepts them. The router does not build prompts, score labels or tokenize
(SPEC §10). The request goes to the engine as one JSON body with `model` replaced by the
served name; the response comes back whole, with `model` replaced by the public name.

A response is complete evidence when it is a whole JSON object with an `answers` object;
an engine 400, 413 or 422 with a complete JSON body is a rejection; anything else leaves
the request uncertain, exactly as for chat (no replay after the request may have been
accepted). The host ingress permits exactly this path besides chat completions, with its
own body check; its body limit (8 MiB) is the limit for images too.

Timing histograms keep their durations; time to first token and decode speed are not
recorded for decision requests. The engine's `usage` is passed through. Whether the
engines' running and waiting gauges count decision requests is qualified per engine
before parking or switching relies on them.

### 3. Readiness is a fixed decision

Where a chat deployment generates text to prove readiness, a decisions deployment sends
one fixed `/v1/systemone` request: a short text state and one `choice` question with three
options, one of which the state names. It is ready when the answer is complete and
well-formed: the question answered, a probability for every option, each finite and
between 0 and 1, summing to 1 within 1e-3. It does not have to pick the named option;
readiness proves the model answers, not that it is right. This holds at start, after a
restart, and when a host re-proves a launch after reconnecting. `/health` and `/v1/models`
alone are not readiness (SPEC §6.1).

The request is version `1` of the probe of kind `decision`
(`crates/capyctl-adapters/src/decision_probe.rs`), sent to the launch's own loopback
endpoint with its own key, never through ingress or the router:

```json
{
  "model": "<the engine's served model name>",
  "state": "The ticket says: my card was charged twice.",
  "questions": {
    "route": {
      "type": "choice",
      "instructions": "Which team should handle this ticket?",
      "criteria": {
        "billing": "payments, charges and refunds",
        "shipping": "deliveries, parcels and tracking",
        "technical": "errors, crashes and sign-in problems"
      }
    }
  }
}
```

The answer is ready evidence when the question `route` is answered as a `choice`, with a
probability for every option and no other, each finite and within [0, 1], summing to 1
within 1e-3, and the pick one of the three options. It is a well-formed answer, not the
correct choice. The kind and version are recorded with the wake canary's reference (§4),
so any change to the request, its state, question, instructions, options or descriptions,
is a new version, and a reference recorded by another version is never compared with it.

### 4. The wake check compares probabilities

The first answer to the fixed request is recorded as the reference, as the chat canary is
today. A later check passes when it picks the same option and every probability is within
0.05 of the reference. Probabilities move slightly with batching and caching (SGLang
reports up to 0.07 between cold and cached requests on hybrid models), so exact equality
is not the rule; a model that picks differently, or moves by more, fails with
`wake_mismatch` and is stopped, as today.

With §7.2 a decisions deployment does not park, so in the first version this check runs
after a restart of a deployment that has a reference, and is the basis for parking later.

### 5. Discovery

`GET /v1/models` keeps its shape. Each entry gains `capyctl.api` (`chat` or `decisions`)
so a client can tell which endpoint a model answers.

### 6. Engines

- **llama.cpp v0.7.0** (installed): the first engine for decision models, for every recipe
  in this release.
- **SGLang 0.5.22** (installed 2026-10-17): Clef and Clef-Flash in BF16, and chat models
  answering decisions through label scoring; qualified live before it is listed.
- **vLLM:** refused until a release ships `/v1/systemone`; then verified and admitted.
  Amendment 2026-10-21: 0.32.0 ships it but does not read Clef and Clef-Flash with their
  decision head, so vLLM stays refused.
- **TensorFold 1.0:** a separate change (a new engine integration for its native binary);
  decision models follow it.

### 7. Scope of the first version (owner decision 2026-10-17)

1. **One machine only.** A decisions deployment with a `topology` is refused. Group
   readiness, stall detection and the group canary would each need a decision form; the
   models in this release fit one GB10 or one discrete card.
2. **Restart instead of park.** A decisions deployment resolves `restart_only`, and
   `deep` or `host_backed` is refused. llama.cpp cannot park anyway, the models start in
   seconds, and parking can follow once §4 has been qualified on SGLang.
3. **No readout in CapyCTL.** CapyCTL forwards to the engine's own `/v1/systemone`; it
   does not turn a chat model into a decision model itself. SGLang does that for chat
   models with `api: decisions`.

## Consequences

- Recipes gain a decision form: latency at p50 and p95 by question count and state
  length, decisions per second as users increase, peak memory, start time, and accuracy on
  a pinned public set, without tokens per second. `capyctl-bench` gains a decisions mode.
- Every place that proves readiness gains a decision branch (adapters, agent, controller
  and the stored canary); the fake engine gains a `/v1/systemone` mode so CPU tests cover
  routing, accounting, readiness and the canary.
- A new conformance row: **T43 — decision models.** `api` resolves and is refused on an
  engine without the capability; `POST /v1/systemone` is validated, routed, admitted,
  leased and settled like chat; the mismatched endpoint is refused before activation; a
  decisions deployment becomes ready only on a well-formed fixed decision; the canary
  passes within tolerance and fails on a changed pick; an old host never receives a
  decisions deployment.

## Alternatives considered

- **Forward `/v1/systemone` without a field.** Rejected: readiness and the canary must
  know a deployment's API before any request, and an engine serving both endpoints (SGLang
  with a chat model) gives no signal.
- **Detect the API from the checkpoint.** llama.cpp marks native decision models, but a
  chat model can answer decisions too (SGLang), and the operator chooses; detection may
  later fill in a default.
- **Readiness by `/v1/models` only.** Rejected by SPEC §6.1: a listening server is not a
  usable model.
- **Exact canary equality.** Rejected: probabilities are not bit-stable across batches and
  caches on the engines that serve these models.

## Amendment 2026-10-21: vLLM 0.32.0 measured, still refused (§6; owner decision)

vLLM 0.32.0 ships `POST /v1/systemone` (`entrypoints/generate/structured_decisions`), and
was measured live through CapyCTL on one GB10 (isolated standalone, a build that let it
resolve `api: decisions`). Clef and Clef-Flash (BF16) became ready on the fixed decision
of §3, answered through the router, were refused `api_mismatch` on chat and
`invalid_request` on a malformed body, and ran capyctl-bench's decisions suites.

vLLM 0.32.0 reads both checkpoints (architecture `Qwen3_5ForConditionalGeneration`) from
their text output, scoring each option's label token with the language-model head (its
`NextTokenStrategy`); it uses a checkpoint's joint schema head only for Clef-Omni. The
answers are not the models' decisions as published:

| Clef-Flash, capyctl-bench | vLLM 0.32.0 | SGLang 0.5.22 | llama.cpp v0.7.0 |
|---|---|---|---|
| API-Bank | refused (all 508: "at most 26 options for this model") | 93.1% | 92.9% |
| Home appliances | 0.0% | 97.7% | 96.6% |
| GPQA Diamond | 44.4% | 51.0% | 51.5% |
| Typed Decisions (ECE) | 69.5% (0.243) | 70.2% (0.012) | 70.7% (0.013) |
| AG News / BoolQ | 90.2% / 89.8% | 89.4% / 89.6% | 89.6% / 89.6% |
| 1 question, 2k-token text, p50 | 517 ms | 545 ms | 718 ms |

Clef on vLLM 0.32.0 scored 43.2% on Home appliances (SGLang 79.5%, llama.cpp 81.8%).

Decision: resolution keeps refusing `api: decisions` on vLLM, on every version, with
`capability_missing:decisions` and this reason. vLLM is admitted once a release reads
these models with their decision heads, qualified live as §6 requires. The vLLM adapter
keeps the decision readiness probe and wake canary of §§3, 4 (covered by T43 tests), which
no resolved deployment reaches while the refusal stands. vLLM 0.32.0 is verified for
chat (ADR 0018 §1).

Parking, for the later decision of §7.2: Clef-Flash launched directly on vLLM 0.32.0 with
sleep mode slept at level 2 (about 33 GB freed), woke its weights, reloaded them, woke the
KV cache and answered the fixed decision with bit-identical probabilities.

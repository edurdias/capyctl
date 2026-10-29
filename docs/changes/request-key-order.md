# Status: The engine receives the caller's request bytes — 2026-10-29 (branch `fix/preserve-json-key-order`)

Issue #220. The router parsed every inference request into `serde_json::Value`
and forwarded it re-encoded. Without `serde_json`'s `preserve_order` a `Value`
object is a sorted map, so every JSON object reached the engine with its keys in
alphabetical order. SGLang's System One prompt numbers a decision's questions in
the order of the `questions` object, so through CapyCTL the model saw a
different prompt from the one the caller sent: measured on Clef-Flash FP8 with
SGLang 0.5.22, 94 of 104 answers changed and 6 top answers flipped against the
same engine called directly. The property order of a chat request's JSON
schema (`response_format`, `tools`) was reordered the same way.

SPEC §10: the router now keeps the bytes it received
(`capyctl_adapters::request::InferenceRequest`, which `ChatForward` takes in
place of `Value`). Validation still reads the parsed form; the forwarder sends
the caller's members in their order and bytes, nested objects untouched, and
writes only what CapyCTL sets (the served `model`, `stream`, the effective
`cache_salt`, `stream_options` on a collected request, `rid`) in place, or
after the caller's members when the caller did not send it. The host ingress
adds `rid` the same way instead of re-encoding the body. A request with a
duplicated key at any depth is now refused with `invalid_request` (400) by the
router and the ingress, because the validated request and the bytes an
engine's parser reads must be one request; it used to be accepted, last value
winning.

`preserve_order` was not turned on: it applies to the whole build, and
configuration, receipt and command digests are computed over serialized
`Value`s and compared with digests stored in ledgers and sent between versions
(`managed_configuration.rs`, `current_policy.rs`, `execution.rs`), so it would
have changed them.

Tests: `capyctl-router` `decisions::the_engine_receives_the_callers_key_order`
(T43 T19) sends a decision with questions `urgency`, `cause`, `doctor` and a
chat request whose schema lists `zeta` before `alpha` through the router and
the real forwarder to an engine double, and checks the bytes it receives. On
`main` it fails (the engine receives `cause`, `doctor`, `urgency`); it passes
here. `capyctl-agent` `ingress::the_capyctl_request_id_becomes_rid_only_on_a_capable_chat_launch`
now checks the ingress forwards the router's bytes with `rid` appended, and
refuses a duplicated key; `capyctl-adapters` `request::tests` cover splicing
and refusal. CPU and Fake-engine tests are not qualification. Live check still
needed: repeat the issue's 41-request comparison through CapyCTL and directly
against SGLang 0.5.22 and expect bit-identical answers.

# Release note: Fixes

- **Requests reach the engine as you sent them.** The router used to forward
  every request with its JSON object keys sorted, which reordered the
  questions of a `/v1/systemone` decision (SGLang numbers them in its prompt
  in the order you send them) and changed answers, and reordered the
  properties of a chat request's JSON schema. The engine now receives your
  request's bytes, with only the served model name and the fields CapyCTL
  sets changed. A request that repeats a key in the same object is refused
  with `invalid_request` (issue #220).

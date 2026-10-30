# Status: The caller receives the engine's answer bytes — 2026-10-30 (branch `fix/router-answer-bytes`)

Issue #223, the response side of #220. The router parsed every engine answer
into `serde_json::Value` and sent it re-encoded: `decide` wrote the public
`model`, `engine_metrics::attach_decision` and `attach` added
`capyctl.metrics`, each relayed stream chunk was re-encoded to write `model`,
and a collected chat answer was rebuilt from parsed chunks. Without
`serde_json`'s `float_roundtrip` feature, parsing a float is not always
correctly rounded, so a probability or a log probability could come back one
unit in the last place away from the engine's. The live check for #220 on
0.1.10 (host B, SGLang 0.5.22, `clef-flash` as `api: decisions`) found 23 of
37 answers identical through the router against 37 of 37 for the engine
called twice, with differences up to 2.2e-16. Re-encoding also wrote numbers
in serde_json's spelling (`1e-05` came back as `0.00001`, a non-shortest
decimal as its shortest form) and sorted every object's keys.

SPEC §10: the caller now receives the engine's bytes. A new
`capyctl_adapters::answer::EngineAnswer`, which `ChatForward::forward_chat`,
`forward_chat_observed` and `forward_decision` return in place of `Value`,
keeps the bytes beside the parsed form. Validation and the metrics still read
the parsed form; `EngineAnswer::set` records each top-level member CapyCTL
writes (the public `model`, `capyctl`), and `encode` writes those in place, or
after the engine's members when the engine sent none, with every other member
byte for byte, using the same splice as the request side
(`capyctl_adapters::request::splice`). The router answers with those bytes
(`chat::relay`). A relayed stream chunk is spliced the same way, with only
`model` written anew. A collected chat answer is assembled as bytes: the
first chunk's members in their order, each token's log probabilities, and the
last `usage`, `tensorfold`, `metrics` and `timings` the engine reported go as
the engine sent them; only what collecting builds (the message, the choice,
`object`) is written by CapyCTL. Validation is unchanged: an answer that is not
one JSON object with distinct keys at every depth, or a decision without an
`answers` object, is still uncertain.

`float_roundtrip` was not turned on: like `preserve_order` it applies to the
whole build and changes how every float is parsed, so configuration, receipt
and command digests computed over parsed `Value`s and compared with stored ones
would need an audit, and it would still not give the engine's bytes, since
serde_json writes its own spelling of a number. With pass-through no float is
parsed on the way to the caller, so no digest or stored value changes.

Tests: `capyctl-router` `decisions::a_decision_answer_keeps_the_engines_bytes`
(T43) has an engine double answer a decision whose floats serde_json's
default parser does not read back exactly (`0.9856906946328695`,
`0.21291890726713458`, `0.9259338926496359`, `4.332222558366402e-308`,
`2.2250738585072011e-308`, `4.9406564584124654e-324`, `1.1224000000000001`,
`1e-05`) with keys out of alphabetical order, and checks the router's answer is
the engine's bytes with `model` replaced in place and `capyctl` appended.
`decisions::a_chat_answer_keeps_the_engines_floats` does the same for a
stream (each relayed chunk is the engine's with only `model` replaced) and a
collected answer (the log probabilities and `timings` appear verbatim). On
`main` both fail: the router answers
`"tension":0.925933892649636`, `"confidence":4.3322225583664017e-308`,
`"1":0.00001`, `"sleep":5e-324`, keys sorted, and a stream chunk carries
`"logprob":-0.21291890726713456`. Both pass here. `capyctl-adapters`
`answer::tests` cover the splice and the refusals. CPU and Fake-engine tests
are not qualification. Live check still needed: repeat the #220 comparison
(host B, SGLang 0.5.22, `clef-flash` as `api: decisions`, 12 requests with 37
questions) through CapyCTL and directly, and expect 37 of 37 answers
bit-identical; a chat request with `logprobs` through CapyCTL and directly on
one engine should match the same way.

# Release note: Fixes

- **Answers come back exactly as the engine wrote them.** The router used to
  re-encode every answer, which could move a decision's probabilities or a
  chat answer's log probabilities by one unit in the last place, respell
  numbers and sort keys. A `/v1/systemone` answer and each streamed chunk are
  now the engine's bytes with only `model` set to the name you sent and
  `capyctl` added; a collected chat answer carries the engine's log
  probabilities, `usage` and statistics as the engine sent them (issue #223).

# Status: ADR 0036 accepted: embedding models — 2026-10-27 (branch `docs/adr-embeddings`)

ADR 0036 records the owner's decisions of 2026-10-27 for serving embedding models at
`POST /v1/embeddings` in 0.2.0. It was proposed on 2026-10-24, and the owner answered its
eleven open questions and named the models to qualify. A deployment declares
`api: embeddings` (a third value beside ADR 0030's `chat` and `decisions`, part of the
frozen revision); vLLM renders `--runner pooling`, SGLang `--is-embedding` and llama.cpp
`--embeddings`, with batch sizes rendered so each input fits one physical batch.
TensorFold documents no embeddings route on 0.6.6 or 1.1.4 and is refused. The request has
a closed OpenAI text contract (`model`, `input` as a string, strings, token IDs or arrays
of token IDs, at most 2048 inputs, `encoding_format` `float` or `base64`, `dimensions`,
`user`); llama.cpp, which ignores `dimensions`, is refused any other than the native
dimension, and `cache_salt` is refused. Readiness is a fixed embedding (kind `embedding`,
version 1), and the canary compares later vectors by relative distance, with a tolerance
set from live row EM3 and checked against 0.02. Custom engines may declare
`api: embeddings`, chat checkpoints may be served as embeddings, and today's sizing rules
stay. Scope: one GPU, text only, LMCache off.

Decisions that changed the design:

- vLLM 0.32.0, SGLang 0.5.22 and llama.cpp v0.7.0 are certified; older versions, other
  checkpoints and custom engines run as uncertified custom setups and are never refused
  for being uncertified (owner rule of 2026-10-27).
- vLLM and SGLang embeddings deployments park `deep` in 0.2.0, with the canary after every
  wake, qualified by the new live row EM9; `host_backed` is refused, and llama.cpp stays
  `restart_only`.
- The answer is streamed through to the client and validated as it passes against one
  fixed JSON shape, with `model` rewritten on the fly before or after `data`; there is no
  answer cap, and the proposed 64 MiB limit and its refusal before forwarding are gone. A
  malformed answer is detected after part of it reached the client, which then receives an
  incomplete body, and the request is uncertain as before.
- A cut embedding request runs to its end and settles on its answer; a cut request whose
  answer never completes still settles at the next verified stop.
- Live qualification covers ten open-weight embedding models (Qwen3-Embedding-8B and 4B,
  bge-m3, pplx-embed-v1-0.6b and 4b, multilingual-e5-large, bge-base-en-v1.5,
  all-MiniLM-L6-v2, all-mpnet-base-v2, Nemotron-3-Embed-1B) on every engine that can run
  each, with the combinations the engines' source does not support documented; EM2 to EM9
  run on Qwen3-Embedding-4B.

The ADR gives the metrics, the ingress path, the docs, conformance row T47 and live rows
EM1–EM9. Nothing is implemented; no code or tests change. CPU and Fake-engine tests are not
qualification, and the EM rows are the live check still needed.

# Release note: none

# Status: ADR 0036 proposed: embedding models — 2026-10-24 (branch `docs/adr-embeddings`)

ADR 0036 (Proposed, for the owner to decide; target 0.1.9) designs `POST /v1/embeddings`.
A deployment declares `api: embeddings` (a third value beside ADR 0030's `chat` and
`decisions`, part of the frozen revision); resolution refuses it with
`capability_missing:embeddings` except on vLLM 0.32.0 or newer (`--runner pooling`),
SGLang 0.5.22 or newer (`--is-embedding`) and llama.cpp v0.7.0 or newer (`--embeddings`,
with batch sizes rendered so each input fits one physical batch); TensorFold documents no
embeddings route on 0.6.6 or 1.1.4 and is refused. The request has a closed OpenAI
contract (`model`, `input` as a string, strings, token IDs or arrays of token IDs, at most
2048 inputs, `encoding_format` `float` or `base64`, `dimensions`, `user`); llama.cpp,
which ignores `dimensions`, is refused any other than the native dimension. Answers are
read whole up to 64 MiB, and a request whose answer could exceed that is refused before
forwarding. Request IDs are handled as for chat and decisions; CapyCTL's own ID is not sent
to the engine. A cut embedding request was never answered, so it settles at the next
verified stop, with ADR 0032's restart when such requests fill the deployment. Readiness
is a fixed embedding (kind `embedding`, version 1): one finite, non-zero vector, whose
length is recorded as the native dimension; the wake canary compares later vectors with
the reference by relative distance, with a tolerance set from live measurement. Scope of
the first version: one GPU, restart only, text only, LMCache off. The ADR gives the
metrics, the ingress path, the docs, conformance row T47, live rows EM1–EM8 and eleven open
questions with a recommendation each. Nothing is implemented; no code or tests change.
CPU and Fake-engine tests are not qualification, and the EM rows are the live check still
needed.

# Release note: none

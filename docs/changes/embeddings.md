# Status: Embedding models: `api: embeddings` and `POST /v1/embeddings` (ADR 0036) — 2026-10-28 (branch `feat/embeddings`)

ADR 0036 (accepted 2026-10-27, owner decisions of that day) is implemented in commits
`f5d6b32e..6516d7ab` with its SPEC texts (§6.1, §6.2, §9.1, §9.2, §10, §15.1, §20 T47) and
guide pages. A deployment declares `api: embeddings`, part of the frozen revision. vLLM
renders `--runner pooling`, SGLang `--is-embedding` and llama.cpp `--embeddings` with
`--ubatch-size` and `--batch-size` equal to the per-slot context unless the arguments
state them. vLLM's `--runner` and `--convert` and SGLang's `--is-embedding` are reserved on
every deployment; llama.cpp's `--pooling` is ordinary on an embeddings deployment, `none`
refused. TensorFold is refused `capability_missing:embeddings`; a `topology`,
`host_backed`, a parser and `lmcache: required` are refused, and LMCache is off with
`embeddings_unsupported`. vLLM and SGLang deployments that state no residency park `deep`
(while the profile permits deep parking); llama.cpp and custom engines are `restart_only`.
Resolution records `certified` for vLLM 0.32.0, SGLang 0.5.22 and llama.cpp 0.7.0 and
`uncertified` otherwise, shown by `capyctl status deployment`; no version is refused for
being uncertified. A custom engine's manifest may declare `api: embeddings`. Hosts
advertise the capability `embeddings` (`host_capability_missing:embeddings`).

The router serves `POST /v1/embeddings` with OpenAI's closed contract, refusing any other
field with `400 invalid_request`, and `api_mismatch` across the three endpoints. On
llama.cpp a `dimensions` other than the native dimension recorded at readiness is refused.
The answer is streamed through without a size limit, checked as it passes, with `model`
rewritten to the public name; `capyctl.metrics` (`total_ms`, `time_to_response_ms`,
`input_tokens`, `inputs`) and the closing brace are written only after the last check, so a
malformed or broken answer reaches the client as an incomplete body and the request is
uncertain. A cut request keeps its engine connection, is read to its end and settles on a
complete answer; one whose answer never completes settles at the next verified stop.
CapyCTL's request ID is never sent to the engine. Readiness is the fixed embedding (kind
`embedding`, version 1); the wake canary compares vectors by relative distance after every
wake and restart, with a provisional tolerance of 0.02 until row EM3 sets it. The host
ingress allows the path with its own validator and relays the answer as it arrives.

Tests are tagged T47 (with T17 and T43 where they apply) in the implementation commits.
CPU and Fake-engine tests are not qualification. Nothing is certified: live rows EM1–EM9
(ADR 0036, Live qualification) are the live check still needed, on host A with vLLM
0.32.0, SGLang 0.5.22 and llama.cpp v0.7.0, and EM9 decides whether vLLM and SGLang keep
`deep` parking for embeddings in 0.2.0. The recipes and the `capyctl-bench` embeddings mode
in the recipes repository are still to come.

# Release note: Embedding models

- **Embedding models at `POST /v1/embeddings`.** A deployment with `api: embeddings` serves
  OpenAI's Embeddings API on the same endpoint and key as chat: strings or token IDs, up to
  2048 inputs a request, `float` or `base64`, and `dimensions` where the model supports it.
  It runs on vLLM, SGLang, llama.cpp or a custom engine (not TensorFold), on one GPU; on vLLM
  and SGLang it parks `deep` and is checked after every wake, on llama.cpp it restarts. The
  answer streams to the client as the engine produces it, with no size limit; an answer
  that fails CapyCTL's checks ends without its closing brace, so it never parses as JSON.
  Status shows the deployment `certified` or `uncertified`; certification of each model
  comes from live rows that have not run yet, so no model is certified in this release
  ([Embedding models](../guide/deploy.md#embedding-models),
  [Embedding requests](../guide/requests.md#embedding-requests)).

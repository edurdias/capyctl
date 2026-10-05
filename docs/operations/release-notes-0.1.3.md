# CapyCTL 0.1.3

Unreleased.

## Memory

- **Short `resources`.** A model that restarts instead of parking can state
  its memory as `resources: {gpu: 11GiB, ram: 2GiB}` instead of five phases:
  CapyCTL reserves both figures while it starts, runs and stops and nothing
  once it is stopped. On a unified-memory machine the pool is charged their
  sum. The five phases are still accepted, and `capyctl validate config` shows
  what the short form stands for. See the TensorFold section of the
  [engines guide](../guide/engines.md).
- **SGLang on a discrete GPU accepts the context `status` shows.** SGLang took
  its memory fraction of the GPU memory free when it started, not of the
  card's total, so on a 16 GB laptop GPU it held about two thirds of the KV
  cache CapyCTL passed and refused inputs over 42,772 tokens where `status`
  showed 63,920. The fraction now covers that difference; SGLang still
  allocates only the KV cache and state CapyCTL sized.
- **SGLang hybrid models get their concurrency.** When you state no memory
  request, CapyCTL now adds a hybrid model's per-request state to the request it
  derives, for `max_concurrent_requests` (or 8) as far as the machine holds it,
  instead of borrowing it from the margin or the KV cache. Qwen3.8-27B at the
  default KV cache runs 8 requests where it ran 5, and asks for about 6 GiB
  more (16 GiB with DFlash2). A revision measured before this release keeps its
  sizing; a new revision of it gets the state.
- **SGLang hybrid models run 8 requests by default.** Without
  `max_concurrent_requests`, a hybrid model on SGLang runs up to 8 requests at
  once, as TensorFold does, instead of up to 32; dense models are unchanged.
  The router still accepts 32 per deployment and the rest wait in SGLang's
  queue. Set `max_concurrent_requests` to run more, and CapyCTL sizes the state
  for that count.

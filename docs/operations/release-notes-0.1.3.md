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
- **SGLang hybrid models get their concurrency.** When you state no memory
  request, CapyCTL now adds a hybrid model's per-request state to the request it
  derives, for `max_concurrent_requests` (or 32) as far as the machine holds it,
  instead of borrowing it from the margin or the KV cache. Qwen3.8-27B at the
  default KV cache runs 32 requests where it ran 5, and asks for about 23 GiB
  more; state `max_concurrent_requests` to ask for less. A revision measured
  before this release keeps its sizing; a new revision of it gets the state.

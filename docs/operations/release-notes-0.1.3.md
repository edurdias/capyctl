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
- **SGLang with speculative decoding parks again.** A deployment with
  `--speculative-algorithm` in its arguments parks `deep` by default instead of
  restarting: the park gives back the KV cache and keeps the weights, the
  draft model's included, so a wake reloads nothing. It frees less than a
  model without a draft model (the weights stay in memory), and the parked
  charge is measured on the first park. `residency: host_backed` is still
  refused for it; a deployment that states `restart_only` keeps restarting.
  On a 128 GB GB10, Qwen3.8-27B NVFP4 with DFlash2 parks in about a second,
  holds 33.4 GiB parked and answers about 2 s after a request wakes it. That
  is above a standalone's parked limit (a quarter of the memory), so once
  that is measured on its first park, later parks are refused and it stays
  loaded.

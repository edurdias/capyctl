# Status: LMCache by default (ADR 0034) — 2026-10-28 (branch `feat/lmcache-default`)

ADR 0034 (accepted 2026-10-27, owner decisions and the owner's rule on certified
combinations of the same day) is implemented for 0.2.0; its live rows have not run.

- **Engine-driven transfer for parking residencies (§1, owner decisions 1 and 5).** A launch
  whose residency resolves `deep` or `host_backed` renders the LMCache server with
  `--supported-transfer-mode engine_driven` (never `auto`), vLLM's connector with
  `"lmcache.mp.mp_transfer_mode": "engine_driven"` and SGLang's owner-only LMCache file
  with the same key under `extra_config`, and no `--shm-name`: the server maps no engine
  memory, so a sleep releases the engine's KV. Every server renders
  `--eviction-trigger-watermark 0.8 --eviction-ratio 0.2`. `restart_only` keeps
  `lmcache_driven` transfer with A13's charge and A15's bound until the released support
  data (`ENGINE_DRIVEN_ROWS`) records LC16 and LC18 as passed; neither has run. The
  launch-time check verifies the eviction options for every launch and, for a launch that
  renders engine-driven transfer, the server's `--supported-transfer-mode` with
  `engine_driven`, vLLM's adapter reading `mp_transfer_mode` and SGLang's unified
  connector reading `lmcache.mp.mp_transfer_mode` (`check_failed` naming the missing
  piece). Hosts declare the capability `lmcache_engine_driven`; a parking launch on a host
  without it is planned off (`host_capability_missing`). The pickle path is documented
  beside "Local reach" with the host's `lmcache: off` as its remedy; the upstream issue
  asking for a payload format that is not pickle is drafted in the pull request, not
  filed.
- **Uncertified runs (§3a, §6, owner rule of 2026-10-27).** Under `lmcache: required`, a
  parking combination outside `QUALIFIED_PARKING`, or a model on several GPUs of one host,
  runs LMCache with the certified rendering as an uncertified setup: `deploy --wait` warns,
  status shows "LMCache on (uncertified: ...)", JSON gains `certified: false`, and the
  event `lmcache_uncertified` records it once per launch. The refusals
  `lmcache_unavailable:park_unqualified` and `:multi_gpu_unqualified` are removed, and so
  is ADR 0035 §1a's narrowing of `topology: auto` to N = 1 for `lmcache: required`;
  `lmcache: auto` still resolves off with both reasons. The table stays empty and
  `RECORDED_FAILURES` holds no failure yet.
- **The automatic pool (§4, owner decisions 2, 3 and 4).** `lmcache_pool: auto`, the
  default, sizes a launch's pool to `max(4 GiB, ceil_GiB(C × B / 0.64))` from the context
  fit's KV bytes per token and the launch's context, capped at the placed host's
  system-domain `host_kv_limit` less the deployment's own host KV (4 GiB on a host with no
  limit, or when B or C is unknown). An enrolled host reports B and C with its checkpoint
  sizing and digest (store schema v60; v59 is ADR 0036's). A pool that keeps less than
  one context warns in `deploy --wait`, status and `inspect deployment --effective-config` and journals
  `lmcache_pool_short` once per launch; an automatic pool under `lmcache: auto` that does
  not fit the host-KV room left shrinks to it (whole GiB, at least 1 GiB) or runs without
  LMCache (`host_kv_room`); a stated pool and `lmcache: required` wait or are refused as
  before. Status gains the basis, JSON `pool_source`, `pool_tokens` and `warning`.
  Standalone's `host_kv_limit` stays a tenth of the memory observed at each start.
- **The `restart_only` advice is dropped (§5, owner decision 6)** from the parking guide;
  behaviour is unchanged.
- **The model list (§2, owner decision 9)** is compiled from OpenRouter's usage ranking of
  2026-10-28 and is in the pull request for the owner, before any live row: Qwen3-4B
  (`qwen3`, required) plus `mistral`, `llama`, `mistral3`, `granite`, `qwen3_vl` and
  `qwen2`; only Qwen3-4B fits the discrete-GPU laptop by the ADR's rule.

Tests (T45; T46 for the multi-GPU table) cover the transfer mode and the exact argument
vectors and connector settings for both modes, the launch-time check failing each missing
piece, the table of examples and the cap, the shrink and `host_kv_room`, the warning and
its event, the remote basis report and its recording, uncertified runs and their event,
and `lmcache: auto` still off with `park_unqualified` and `multi_gpu_unqualified`. CPU and
Fake-engine tests are not qualification. Still needed live: LC12–LC18 on host A and
LC13–LC16 with `host_backed` on the discrete machine, after the owner reviews the model
list; their passing rows go into `QUALIFIED_PARKING` and `ENGINE_DRIVEN_ROWS`.

# Release note: LMCache

- **LMCache sizes its pool to your model.** With `lmcache_pool: auto`, the new default, an
  instance's LMCache pool holds one full context of its model after LMCache's eviction (at
  least 4 GiB, within the machine's `host_kv_limit`): 8 GiB for Qwen3-4B at 32k tokens.
  When one context does not fit, `deploy` and status warn and the model runs; when the
  machine's host-KV room is taken, the pool shrinks to what is left, or the model runs
  without LMCache (`host_kv_room`), instead of being refused. A stated `lmcache_pool`
  works as before.
- **Parking models are ready for LMCache.** A model that parks deep (or `host_backed` on
  a discrete GPU) now uses engine-driven transfer, so a park releases all of its KV. It
  uses LMCache by default once its engine, model type and GPU are certified; this release
  certifies none yet. `lmcache: required` runs it now as an uncertified setup instead of
  refusing, and the same goes for a model on several GPUs. Declaring
  `residency: restart_only` just to get LMCache is no longer advised.

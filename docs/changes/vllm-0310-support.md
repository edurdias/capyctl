# Status: vLLM 0.31.0 launches under a version-skew-aware reservation — 2026-10-10 (branch `feat/vllm-0.31.0-support`)

Owner decision from the 2026-10-10 engine qualification (RTX 4090 laptop,
vLLM 0.31.0 wheel, Python 3.12.13, torch 2.13.0+cu130; evidence under
`~/projects/crossfunctionalai/engine-qual-2026-10-10/`, `ROWS.txt` indexes
`raw/`): CapyCTL launches vLLM 0.31.0, without changing one rendered byte of a
0.30.0 launch.

**The reservation follows the parser (ADR 0014 §3, ADR 0017 §2).** vLLM 0.31.0
moved logging into the nested `--logging-config` dotted config
(`vllm/config/logging.py`, `LoggingConfig`: `log_level`, `configure_logging`,
`pylogging_config_file`; `--log-level` and legacy `--log-config-file` override
their matching fields, the deprecated alias leaves in 0.33.0). The serve parser
defines the whole surface on one destination, `logging_config` — the
`--logging-config` JSON and every `--logging-config.<field>` resolve to it —
beside legacy flat `--log-level` and `--log-config-file` overrides, each an
argparse `SUPPRESS` destination: absent from a parsed namespace until a token
sets it (`raw/vllm-0310-apiserver-help.txt`). The entry's `RESERVED` list named
the flat 0.29.0/0.30.0 spellings, so `log_config_file` — a name no longer
present in a 0.31.0 namespace — failed every launch closed:
`vllm_startup_failed: effective_args_mismatch` (`status/start-frognano.txt`,
`status/inspect-frognano-failed.json`).

The rule now: the entry reads the destinations the installed parser's own
argparse tree defines (`engine_capabilities.parser_destinations`, the same
probe the capability report uses) and reserves what the build spells. Flat
`RESERVED` names stay unconditional — a build that defines none of them is
drift, never a silent un-reservation (ADR 0014 open issue 4: drift fails
closed); `RESERVED_IF_PRESENT` (rendezvous, scale-out) stays as it was;
`RESERVED_IF_DEFINED` adds the 0.31.0 logging surface (`logging_config`,
`log_level`), reserved wherever the build defines it, nothing reserved on a
build without it. An `SUPPRESS` destination absent from both parses is
agreement; absent from either one alone, or any value difference, is closed
drift. A 0.30.0 launch renders and compares byte-identically: every flat name
is defined with ordinary defaults, and `logging_config`/`log_level` are
reserved on 0.31.0 alone. Deploy-time policy (`engine_policy.rs`) reserves
`--logging-config` and `--log-level` beside `--log-config-file` (checked twice,
ADR 0014 §3), so a user cannot double-set the logging configuration on any
build, in any spelling: the JSON object, any dotted field, `=value`,
underscores, or an abbreviation.

**Cached tokens stay a usage figure (SPEC §17, the metrics contract of #101).**
vLLM 0.31.0's `usage.prompt_tokens_details` is `null` without
`--enable-prompt-tokens-details` (`raw/vllm-manual/chat-repeat-cached.json`,
`raw/vllm-qwen3/cached-tokens-repeat.json`: 418-token repeat prompt, 416/416
prefix-cache hits, `prompt_tokens_details` null), and the per-request `metrics`
object — which gained `speculative_decoding` and `tokens_per_second` — never
carries them (`raw/vllm-manual/chat-stream.sse`, usage chunk). The figure maps
from `usage.prompt_tokens_details.cached_tokens` on both the 0.30.0 and 0.31.0
response shapes — a response-shape-driven rule like the existing per-engine
mapping, since the router cannot know the engine version at request time — and
stays absent, never zero, when the engine reported none
(`crates/capyctl-router/src/engine_metrics.rs`).

Live recheck (laptop, RTX 4090, same `~/vllm-0.31.0-venv`, local machine only;
evidence `~/projects/crossfunctionalai/engine-qual-2026-10-10/recheck/`):
every row PASS through the release build of this branch, driven with
`--config` on the client as well (an earlier attempt without it resolved the
machine's own implicit registry — `vllm` 0.29.0 — and refused with
`profile_exists`; nothing was written there). `engine add`: vLLM 0.31.0
registered, custom, deep park enabled, published. `deploy model` +
`start --wait`: Ready 1/1 (initialize succeeded) — the exact launch the flat
reservation refused before. Chat via the relay (the deployment passes
`--enable-prompt-tokens-details` as an extra): non-streaming and streamed
answers both carry `cached_tokens` `{"source": "engine", "value": 0}` — the
engine measured zero, because FrogNano-4B-2609 is a hybrid (Mamba) model
whose prefix cache does not hit on these prompts (`vllm:prefix_cache_hits_total
0.0` in the engine's own scrape, matching the manual qualification; the dense
Qwen3-4B row in `raw/vllm-qwen3/` hit 416/418); the figure maps from
`usage.prompt_tokens_details.cached_tokens` on the 0.31.0 shape and would stay
absent had the engine reported nothing. Deep-park cycle: park released
9.5 GiB to 727 MiB, one request woke it in 7.5 s with the figures present,
memory returned. `stop`: engines stopped, GPU back at the 111 MiB baseline.
Unlike the CPU and Fake-engine tests, this recheck ran the real engine.

Tests (T14, T21, T40, T41): the entry suite gained a `VersionSkewTests` class
with a fixture parser per shape — the 0.29.0/0.30.0 flat parser unchanged, and
a 0.31.0 parser whose logging actions are `SUPPRESS`-destined dotted
destinations (`fake_parser_0310`) — checking the rendered block launches on
the 0.31.0 shape, the whole 0.31.0 logging surface stays reserved however
spelled, a rendered logging configuration is kept exactly and re-set is
refused on either shape, and a build without a flat reserved name fails closed
(`runtime/tests/test_vllm_entry.py`). Router unit
(`vllm_0310_maps_cached_tokens_from_usage_or_leaves_it_absent`) and integration
(`vllm_0310_maps_cached_tokens_from_usage`, streamed and not, relayed byte for
byte) tests read the raw 0.31.0 response and usage-chunk shapes and hold the
absent-not-zero rule; the 0.30.0 tests are untouched. The adapters' logging
test refuses every 0.31.0 spelling in host-fixed and deployment arguments
(`crates/capyctl-adapters/tests/vllm_args.rs`), and the reserved-parity test
(`crates/capyctl-config/tests/reserved_parity.rs`) folds `RESERVED_IF_DEFINED`
into the one-table check on both sides of the launch. CPU and Fake-engine
tests are not qualification.

Live recheck (laptop, same `~/vllm-0.31.0-venv`, local machine only): recorded
under `~/projects/crossfunctionalai/engine-qual-2026-10-10/recheck/` — engine
add, start `--wait` to Ready, one non-streaming chat and one streamed chat via
the relay with `cached_tokens` present on a repeat prompt, a deep-park cycle,
and stop, with the GPU back at its baseline.

# Release note: Requests

- **vLLM 0.31.0 launches.** The reserved argument check now follows the
  destinations the installed vLLM parser itself defines, so a build that moved
  an option reserves the name it spells: vLLM 0.31.0 moved logging into the
  `--logging-config` object (`--logging-config.<field>`, with `--log-level`
  and `--log-config-file` as flat overrides), and every spelling of it stays
  reserved beside `--uvicorn-log-level` — a deployment or host cannot set the
  logging configuration, on 0.30.0 or 0.31.0. vLLM 0.31.0 answers carry the
  same per-request figures as 0.30.0 (`capyctl.metrics`, the
  `: x-capyctl-metrics` stream comment); `cached_tokens` still maps from
  `usage.prompt_tokens_details.cached_tokens`, which 0.31.0 reports only with
  `--enable-prompt-tokens-details` passed as an extra argument, and the figure
  is left out when the engine reported none, never shown as zero.
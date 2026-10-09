# Status: llama.cpp adapter, readiness and restart-only lifecycle (plan slice L3) — 2026-10-09 (branch `feat/llamacpp-l3-adapter`)

[ADR 0029](../design/adr/0029-llamacpp-engine.md) §4, §6, §7, §10, §11, plan slice
L3 (`docs/plans/2026-10-09-llamacpp-engine.md`). A llama.cpp deployment now
launches, becomes ready, serves chat and stops, on a host agent and in standalone
alike. The launch refusals slices L1 and L2 left in place are gone: the host
agent's launch authorization (`resolve_as`), `prepare`, the launching and probe
adapters, and the embedded coordinator's `ProfileBindings::spec`. Shapes were read
from the v0.6.0 tag (`tools/server/server-context.cpp`, `server-http.cpp`,
`server-task.cpp`, `common/arg.cpp`, commit `d812350`).

- **Adapter** (`capyctl-adapters::llamacpp::{adapter,http,initialize}`,
  `AdapterSpec::Llamacpp`). No key: llama-server listens on loopback and is
  reached only through CapyCTL's routed path. Initialize refuses a machine-wide
  `config.ini`, renders through the shared builder, spawns through the director's
  tool, then waits (watching the process) for `/health` 200 (every route answers
  503 `Loading model` until the model is loaded) and for `/v1/models` to list the
  served name. SPEC §8.2's check after the engine's parser: `/props` must report
  `total_slots` equal to the rendered `--parallel` and `endpoint_metrics` and
  `endpoint_slots` true, and `/v1/models` `meta.n_ctx` must equal the rendered
  slot window `pad256(context_length)`, capped at `meta.n_ctx_train` as
  llama-server caps it; any difference fails the launch `effective_args_mismatch`
  before the probe, and the coordinator stops it on its recorded processes. Then
  the bounded chat probe (non-empty `content` or `reasoning_content`). The bound is
  the Initialize window (ADR 0014 A1 and A7). Reads are bounded (5 s, 4 MiB: `/props`
  carries the chat templates). The completion probe uses `POST /v1/completions`;
  llama-server answers its text, without token ids.
- **Restart-only.** Park, Restore and ReloadWeights are unsupported; the Park arms
  of the host agent and the coordinator refuse the kind as before (`residency_tier`
  on a host); a group member never resolves to llama.cpp. A wake is a fresh launch
  of the same frozen contract.
- **Idle gate and quiescence** (ADR 0029 §10, ADR 0023 §6). `idle_before_signal`,
  `engine_quiescent`, `prepare_park` and `observe_work` read `/metrics`:
  `llamacpp:requests_processing` and `llamacpp:requests_deferred` both 0 is idle;
  either one above 0 is busy; nothing listening or 503 is signalled at once; an
  answer without both gauges, an error status or no answer is signalled when the
  bound ends (30 s or the command's remaining time); busy at the bound withholds
  the signal and the cleanup stays uncertain. The wait is the shared
  `tensorfold::wait_idle`, which the embedded coordinator already ran for every
  engine. On the host agent the Terminate gate (`native_execution/idle_gate.rs`,
  moved out of `tensorfold.rs` unchanged for TensorFold) now builds a llama.cpp
  adapter for a llama.cpp launch.
- **Requests.** `cache_salt` is refused `cache_salt_unsupported` before anything is
  sent. SSE `:` comments (llama-server's `--sse-ping-interval` pings) were already
  not progress and are not relayed; a test now pins it for llama.cpp. A served name
  llama-server would split or trim (`--alias` takes a comma list) is refused at
  render, since readiness could never find it listed.
- **Hidden configuration at launch** (ADR 0029 §6). A llama.cpp launch on a host
  where `/etc/llama.cpp/config.ini` exists is refused before any effect with the
  new closed policy refusal `engine_config_file`: in the host agent's admission
  (provisioning, pre-admission and the locked recheck alike, so no key is stored,
  nothing is journaled and the reservation is released on that evidence), and in
  the adapter's Initialize before the spawn (the embedded path, where a `Refused`
  Initialize settles as the installation gate's does). The root is `/`; tests name
  their own (`with_llamacpp_system_root`, `LlamacppAdapter::with_system_root`).
  `engine_config_file` and `effective_args_mismatch` have status hints.
- **Launch-failure summary.** llama-server's `error: invalid argument: --name` is
  read like the other parsers' refusals, so an engine that exits on a bad argument
  is reported `engine_argument_rejected` naming the option, never its value.
- **Development controls.** llama.cpp launches are marked with their
  unauthenticated loopback surface (`/v1`, `/health`, `/metrics`, `/props`,
  `/slots`, `/lora-adapters`, access `inference`), as TensorFold's are.
- **Still refused or not here:** park and group paths (restart-only, multi-rank out
  of scope, ADR 0029); deriving the request from the GGUF header and the
  training-context refusal (slice L4); per-request figures and load reports
  (slice L5); the errors guide, engine guide and live rows (slice L6).

Tests (T42 with T14, T16, T17, T21, T29, T37, T41): `capyctl-adapters/tests/
llamacpp_adapter.rs` against an axum engine with llama-server 0.6.0's shapes
(readiness through 503 and an unlisted model; `effective_args_mismatch` for
`total_slots`, `n_ctx` below and above the window, and `endpoint_metrics`; the
window capped at the training context; a reasoning-only probe; an exit reporting
the rejected argument; `config.ini` refusing before any spawn; the gauges; the
wait at the bound; no park path; `cache_salt`; SSE pings; a hang-up quiescent only
once the gauges read 0), `llamacpp_args.rs` (the served-name refusal),
`launch_failure` and `llamacpp::http` unit tests, `resolve` tests, the agent's
native-execution tests (prepare from local policy, Park refused `residency_tier`,
`engine_config_file` as a terminal refusal with no key and no journal entry, the
Terminate gate on `/metrics`), the controller's `engine_bindings` test (the embedded
spec, no key, the same command on every start, no cache root no launch) and the
embedded stop test `only_an_engine_answering_busy_is_not_signalled`, now run for
llama.cpp's `/metrics` too. A CPU build of the v0.6.0 tag served a 0.5B Qwen2.5
GGUF with the rendered command: `/health`, `/v1/models` (`n_ctx` 512 for
`context_length` 500, `n_ctx_train` 32768), `/props` (`total_slots` 4, both
endpoints on), `/metrics` (processing 1 during a stream, 0 within half a second of
the client hanging up), the completion probe's text and SIGTERM (exit in 0.1 s)
matched what the adapter reads. CPU and Fake-engine tests are not qualification;
only the live rows LC1–LC6 (slice L6) qualify llama.cpp. Still needing a live
check on a GPU: start time to readiness, the SIGTERM exit and memory release, and
the hang-up behaviour mid-prefill.

# Release note: none

# Status: vLLM on a discrete GPU: a declared request is rendered as its own share of the card (#225) — 2026-10-30 (branch `fix/vllm-discrete-request`)

Issue #225: on one RTX PRO 6000 (96 GB), a vLLM 0.32.0 deployment stating a
12 GiB KV cache was reserved 71.69 GiB (Ready 72.94 GiB) and launched with
`--gpu-memory-utilization 0.75`, while its measured peak was 44.15 GiB, so a
second model that fit in the rest of the card was refused.

Why the floor exists. It came in with discrete GPU support (design of
2026-09-25, commits `cbbbee30`, `2a96fc0d`, `a514b783`; ADR 0019 §3 and §4):
"vLLM 0.29 with CUDA graphs does not start a 4B model on a 16 GB card below
`--gpu-memory-utilization 0.75`", observed on the discrete-GPU laptop host;
the cause was not recorded. It is not there to size the KV cache: every vLLM
launch gets the KV cache as explicit bytes (`--kv-cache-memory-bytes`,
`plan_from_effective` always sets it), and on unified memory vLLM 0.29 to
0.32 start with `--gpu-memory-utilization 0.10` beside those bytes. So the
floor guards an observed start failure and is kept for a request CapyCTL
derives (and in the standalone template), and it is now documented as such.

The escape. The floor was applied twice: to a derived request
(`device_request_from_weights`, unchanged) and to the utilization rendered
from any request (`device_utilization_pct` clamped to 75..=99). A deployment
that declared `engine_config.memory.request` was charged that request but vLLM
was still told 0.75 of the card, and vLLM checks at start that its share of
the card is free, so a declared request below 0.75 could not start beside a
model holding more than a quarter of the card. Now a declared request is
rendered as its own share (rounded up to a whole percent, at most 0.99), so an
operator who measured a peak declares the request from it, and the card's
charge and vLLM's start check agree. No new field: `memory.request` already
is "the GPU memory it may use".

Design decision for the owner: this amends ADR 0019 §4 ("vLLM renders
`--gpu-memory-utilization` as the device request's share of the card ...
between 0.75 and 0.99"): the 0.75 lower bound now applies only to a request
CapyCTL derived, recorded as ADR 0019's proposed amendment of 2026-10-30.
Admission and accounting do not change (a declared request
was already charged as declared). The issue's other request, sizing a derived
vLLM request from weights + KV + overheads without the floor, was not done:
the floor guards a start failure whose cause is unknown, and dropping it for
every deployment could stop small cards from starting models they start
today. A vLLM deployment already accepted with a declared request below 0.75
of its card is told a smaller fraction at its next start; if that model is
one that needs more (the 0.29 observation), it fails to start
(`launch_failed`) and its request has to be raised. Unified-memory hosts are
unchanged (the fixed low gate).

Tests (failing before, passing after):

- `capyctl-adapters` `vllm_frozen::a_declared_request_takes_the_floor_off_the_cards_utilization`,
  with the issue's card (97887 MiB) and numbers: a deployment stating only
  its 12 GiB KV cache keeps the floor (71.69 GiB request, 72.94 GiB Ready,
  0.75); the same deployment declaring a 43 GiB request (the measured 44.15 GiB
  peak less the CUDA context) is charged 44.25 GiB and told 0.45, with 12 GiB
  of KV bytes, and the issue's 22.6 GiB second model fits beside it. Before,
  `0.75` was rendered instead of `0.45`.
- `capyctl-agent` `native_vllm::a_declared_request_on_a_card_is_rendered_as_its_share`
  (a remote host launches the Fake vLLM engine through the real launch path):
  before, the engine's argv carried `--gpu-memory-utilization 0.75`; now
  `0.45` and `--kv-cache-memory-bytes 12884901888`.
- `vllm_args::a_discrete_launch_renders_kv_bytes_and_utilization` covers the
  floor parameter (`DEVICE_UTILIZATION_FLOOR_PCT`, 75 for a derived request,
  none for a declared one).

CPU and Fake-engine tests are not qualification. Live check still needed: on
a discrete card (the 96 GB card of the issue, or the 16 GB laptop with a
smaller model), start a vLLM 0.32.0 deployment with a declared request below
0.75 of the card and its KV cache, confirm the launch renders the request's
share, the model starts and serves, and a second deployment then starts
beside it; on the laptop, also check whether a 4B model with CUDA graphs
starts below 0.75 on vLLM 0.30 to 0.32, which would show whether the derived
floor is still needed.

# Release note: Fixes

- **A vLLM model on a discrete GPU can share the card when you state its
  memory request.** CapyCTL sizes a vLLM request on a discrete GPU at 75 % of
  the card or more, a guard for a start failure seen with vLLM 0.29, and that
  stays for deployments that state only their KV cache. A deployment that
  states `engine_config.memory.request` was charged that request, but vLLM was
  still told to check for 75 % of the card free at start, so it could not
  start beside another model. vLLM is now told the stated request's share of
  the card. To run two models on one large card, state the vLLM model's
  request from its measured peak (`startup.measured` in `capyctl status
  deployment <name> --json`, less 1.25 GiB for the CUDA context). On
  upgrade, a vLLM deployment already accepted with a stated request below
  75 % of its card is told that smaller share at its next start; if the
  model needs more it fails to start (`launch_failed`), and raising
  `engine_config.memory.request` fixes it.

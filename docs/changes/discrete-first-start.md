# Status: Discrete GPU: a declared request's first start is charged its Ready figure (#219) — 2026-10-30 (branch `fix/discrete-first-start`)

Issue #219: on a 16 GB laptop card (14.7 GiB managed), an SGLang deployment
with a declared 12.86 GiB request was refused at every start (`host … needs
15.4 GiB of gpu0 memory, 14.7 GiB free of its 14.7 GiB limit`, startup
provenance `default`), although its Ready charge, 14.1 GiB, fits the card. It
never started, so no measured peak ever replaced the estimate.

Cause. ADR 0019 amendment A1 states that beside a request declared for a card
an undeclared `memory.startup` is the request. The resolver applied that rule
only when the checkpoint's weights were known at resolution, tying it to the
device margin (weights x 0.10). Acceptance resolves a declared request without
the weights (only a revision whose memory is derived from them is frozen
provisional and re-resolved), so such a revision took the unified startup
placeholder instead: the request plus ADR 0014 amendment A8's first-start graph
allowance (1.25 GiB), and the card was charged that plus the CUDA context
(another 1.25 GiB). A card has no solo first start (that rule is for one pool),
so a first start above the card's limit was refused forever.

What changed.

- The A1 rule now applies whether or not the weights are known: a request
  declared for a card, without `memory.startup` or `resources:`, has a startup
  peak equal to the request (provenance `default`, an estimate the first run
  measures per ADR 0019 amendment A2). The issue's first start is charged
  14.1 GiB instead of 15.4 GiB. This is A1 as written; it narrows A8's graph
  allowance, which no longer applies to a request declared for a card. The
  derived request (weights x 1.10 + KV) already behaved this way. It covers a
  model on N identical GPUs of one host (ADR 0035 §3) too: each card's first
  start is charged its Ready figure (request, CUDA context and communication
  allowance) without the per-GPU graph allowance, and a speculative
  deployment's first start loses its doubled allowance (2.5 GiB) the same
  way. ADR 0019 records this as a proposed amendment of 2026-10-30.
- A revision frozen with the placeholder records `startup_graphs_bytes` and
  re-resolves exactly as it was (new checkpoint fact
  `legacy_device_placeholder`), so stored revisions still decode. Such a
  revision keeps its figure; deploying it with `memory.startup` equal to its
  request makes a revision that starts.
- A declared `memory.startup` already replaced the placeholder (nothing is
  added for graphs; the card is charged it plus the CUDA context, and
  validation only requires it to be positive and at least the request), so
  declaring it equal to the request was a working way around the issue. It is
  documented as the figure without the CUDA context.
- A start that cannot be admitted on the card it was resolved for is now
  visible. `capyctl validate config --host` adds `startup.device_bytes` and
  `startup.host_bytes` on a discrete host and `startup.over_limit` (host,
  card, charge, managed limit) when that card's startup charge is above its
  limit, with a `Startup` note in text; `capyctl status deployment` adds
  `startup.over_limit` (against the host's current policy) and the same note
  under the table. It names one host and card, the one the revision was
  resolved on (validate: the lowest-index GPU; status: the revision's first
  host and GPU), and says the start "cannot start there": placement resolves
  every allowed host and GPU on its own, so another one may still hold it.
  New `capyctl_config::effective::startup_over_limit`.
- Not changed: the runtime guard. There is still no abort of a start whose
  real peak exceeds its reservation (the issue's option 3). On a card the
  engine is bounded by the fraction CapyCTL renders from the request, the
  launch check requires the cold charge to be physically free, and the first
  run's measured peak replaces the estimate for later starts (ADR 0019
  amendment A2); a declared `memory.startup` is never measured or replaced.
  Sizing the graph allowance from the deployment (option 2) was not done: on a
  card the A1 rule already removes it.

Owner review: this lowers the first-start reservation on a card for a
declared request accepted before its weights were known, from request + 2.5 GiB
to request + 1.25 GiB (ADR 0019 amendment A1 applied as written; ADR 0014
amendment A8's graph allowance no longer applies there, on one GPU, on N
GPUs, and for a draft model).

Version skew (ADR 0017: every 0.1.x host is fully supported). A host
re-resolves the deployment itself, and one from 0.1.10 or earlier resolves
such a revision by the old rule (15.4 GiB on the issue's card) and would
refuse a start the server admitted at 14.1 GiB. Hosts lay the plan's
per-domain startup figures over their own estimate (`startup_bytes` for the
card, `startup_host_bytes` for host RAM, ADR 0019 amendment A2), but the
server sent them only for a measured peak. The server now sends both for an
unmeasured start on a card too, whenever the host declares the
`startup_host_bytes` capability, which every host from 0.1.7 on does
(introduced by #175); those hosts charge the server's figure, and a host
from 0.1.7 to 0.1.10 admits the start (checked against the old rule in a
test). A host older than 0.1.7 gets the plan it always got and still
resolves its own figure, so it refuses such a start where the old figure
does not fit; upgrade it. The overlay the host applies moved, unchanged, into
`capyctl_config::effective::apply_planned_startup`, which the agent and the
test share.

Tests (failing before, passing after):

- `capyctl-config` `startup_budget::a_request_declared_for_a_card_starts_at_its_ready_charge_before_the_weights_are_known`:
  before, the card's cold charge was 16493051904 bytes (15.4 GiB) against an
  expected 15150874624 (14.1 GiB).
- `capyctl-store` `device_placement::a_request_declared_for_a_card_starts_when_its_ready_charge_fits`
  (deploy, start, arm and Ready through the store with the issue's numbers):
  before, the start was refused `CapacityBlocked("host lab needs 15.4 GiB of
  gpu0 memory, 14.7 GiB free of its 14.7 GiB limit")`, the issue's message.
- `capyctl-cli` `startup_budget::validate_shows_the_first_start_on_a_card_and_one_that_can_never_start`
  (the real binary): before, `startup_bytes` was the request plus the graph
  allowance and no card figure or over-limit note was shown.
- New guards: `a_card_revision_frozen_with_the_placeholder_keeps_it`,
  `a_declared_startup_on_a_card_is_charged_with_the_cuda_context_only`,
  `status_names_a_card_whose_startup_charge_is_above_its_limit`, the table
  test `status_says_when_a_start_can_never_fit_its_card`,
  `multi_gpu::a_request_declared_per_gpu_starts_at_its_ready_charge_on_two_gpus`
  (TP 2: each card's cold equals its Ready figure), and the controller's
  `an_unmeasured_start_on_a_card_carries_the_servers_split` (the plan of an
  unmeasured first start carries the server's card and host RAM figures to a
  host that takes them; a host resolving by the old rule, 40.5 GiB of a
  40 GiB card, lays them over its own and fits at 39.25 GiB; without the
  capability the plan is unchanged).

CPU and Fake-engine tests are not qualification. Live check still needed: on
the discrete-GPU laptop (16 GB card), deploy the issue's SGLang 0.5.22 model
with a declared 12.86 GiB request, confirm `validate config --host` shows a
14.1 GiB card figure without `over_limit`, that the first start is admitted
and reaches Ready, and that the measured card peak (`startup.measured`) stays
within the card; a deployment with `memory.startup: 14GiB` should show the
`Startup` note in `validate` and `status`. With a remote host, check that a
0.1.10 host admits the same first start from a 0.1.11 server.

# Release note: Fixes

- **A model on a discrete GPU with a stated memory request starts when its
  running charge fits the card.** Its first start was charged 2.5 GiB above
  the request (graph allowance and CUDA context) when the deployment was
  accepted before its checkpoint was measured, so on a 16 GB card a model
  whose running charge fit could be refused at every start. The first start
  is now charged the request plus the CUDA context until measured. A
  deployment whose start cannot fit the card it was resolved for, such as
  one whose `memory.startup` is too large, is named by `capyctl validate
  config --host` and `capyctl status deployment`, with the host, the card,
  the charge and its limit. A deployment accepted by an earlier release keeps
  its figure; deploy it with `memory.startup` equal to its request. Hosts
  from 0.1.7 on take the server's figure for such a start; upgrade older
  hosts with the server.

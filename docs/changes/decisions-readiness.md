# Status: Decision models, slice C: readiness and the wake check — 2026-10-17 (branch `feat/decisions-readiness`)

ADR 0030 (accepted, owner decisions 2026-10-17) §3 and §4; SPEC §6.1 is
amended and the amendment log gains the entry. Depends on slice A (the `api`
field, #144); the `/v1/systemone` route is slice B.

- **The fixed decision probe** (`capyctl_adapters::decision_probe`, kind
  `decision`, version 1): one `POST /v1/systemone` with the state "The ticket
  says: my card was charged twice." and one `choice` question (billing,
  shipping, technical, each with a description, and the instructions every
  engine requires). Ready when the answer is complete and well-formed: the
  question answered as a `choice`, a probability for every option and no
  other, each finite in [0, 1], summing to 1 within 1e-3, the pick one of
  the options (not necessarily billing).
- **Where it runs instead of a chat probe** for a decisions deployment: the
  llama.cpp and SGLang Initialize (bounded by the remaining startup window,
  since SGLang's first decision took 36 s live), SGLang's post-wake Probe
  step, the host agent's re-proof after a reconnect and after a restore
  (`fresh_probe`), and a restarted standalone role's re-proof of an adopted
  launch (which for chat stays a model list, unchanged). A decisions
  deployment is never sent a chat probe; vLLM and TensorFold cannot run one
  (slice A) and are unchanged.
- **The wake check:** the first readiness records the decision answer as
  the launch's reference (deployment, instance, generation, launch
  incarnation, as the chat canary), with the probe's version and the rule:
  same pick and every probability within 0.05. Store schema v49 rebuilds
  `wake_canaries` with `kind`, `probe_version`, `decision_json` and
  `tolerance`; existing references migrate as `completion`. A reference of
  another kind or probe version is never compared: the answer finds no
  usable reference. A wake that picks differently or moves a probability by
  more than 0.05 is `wake_mismatch` with the existing stop. The
  `Unsupported => passed` escape is never taken for a decisions launch (the
  adapters and the remote binding answer uncertain instead).
- **Which transitions compare today: none.** A decisions deployment
  resolves `restart_only`, so it never parks and never wakes; every restart
  (idle stop then start, operator restart, crash) is a new generation and
  launch that records its own reference. A host reconnect and a standalone
  restart keep the launch but re-prove readiness only (well-formedness), as
  for chat. The comparison is implemented in the wake path and tested
  there; it becomes live when parking a decisions deployment is allowed.
- **Protocol** (ADR 0017): `ExecuteMember.probe_decision` (field 18) asks a
  host for the decision probe of a retained decisions launch and needs the
  `decisions` capability; `MemberExecutionResult.probe_decision` (field 20)
  carries the answer, accepted only for a completed decision probe and only
  well formed. Both encode nothing when unset, so every existing command
  digests as before. The controller judges a relayed answer well-formed
  again before it counts.
- **Fake engine:** `FakeEngine::with_decisions`/`set_decision` answer
  `/v1/systemone` as scripted (well formed, malformed, another pick, other
  probabilities) and refuse chat, counting both; its lifecycle Initialize is
  proven by the decision, and its wake canary is the decision.
  Its `forward_decision` serves the public `/v1/systemone` route (#145),
  so the Fake answers a decision request end to end.

Tests (T43, with T20, T34): `decision_probe` unit tests (request shape,
well-formed and malformed answers, the 1e-3 sum); llama.cpp
`a_decisions_launch_is_ready_only_on_a_well_formed_decision` against a stub
engine (no chat request, the canary is a decision); controller
`a_decisions_launch_is_ready_only_on_a_well_formed_decision` (Fake),
`a_decision_canary_within_tolerance_completes_the_wake`,
`a_changed_pick_or_a_moved_probability_fails_the_wake`,
`a_chat_reference_is_never_compared_with_a_decision_answer`, the
`wake_canary` unit tests, and
`a_decision_canary_counts_only_a_well_formed_relayed_decision`; agent
`a_decisions_reproof_is_the_fixed_decision_never_chat`; store round trip,
refusals and the v48 to v49 migration; protocol encoding, validation and
capability tests; router
`a_fake_decision_model_becomes_ready_and_answers_through_the_router` (ready
on the decision, answered through the router, lease closed completed, no
chat request). CPU and Fake-engine tests are not qualification. Live
check still needed: a decisions deployment on llama.cpp 0.7.0 and SGLang
0.5.22 becomes ready on the probe, and re-proves after a host reconnect.

# Release note: Decision models

- **Decision models become ready on a decision.** A deployment with
  `api: decisions` is proven ready by one fixed question it must answer with
  a complete set of probabilities, at start and whenever CapyCTL re-checks
  it; it is never sent a chat request. See
  [Decision models](../guide/deploy.md#decision-models).

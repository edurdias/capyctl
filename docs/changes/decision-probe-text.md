# Status: The exact decision readiness request — 2026-10-21 (branch `docs/decision-probe-text`)

Docs only. ADR 0030 §3 now publishes the exact v1 readiness request of a
decisions deployment (kind `decision`, version `1`), copied from
`crates/capyctl-adapters/src/decision_probe.rs`: the JSON body with `model`
as the engine's served name, the readiness rule (every option present and no
other, finite probabilities in [0, 1], sum within 1e-3, the pick one of the
options; a well-formed answer, not the correct choice), and the rule that any
change to the request is a new version. The deploy guide gains "The readiness
question" with the same JSON. No code or tests change; CPU and Fake-engine
tests are not qualification, and no live check is needed.

# Release note: Decision models

- **The readiness question is published.** [Deploy a model](../guide/deploy.md#the-readiness-question)
  shows the exact request CapyCTL sends to prove a decision model ready, and
  what a ready answer must contain.

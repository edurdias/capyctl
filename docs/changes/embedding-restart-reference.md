# Status: Compare the retained embedding reference on restart — 2026-11-10 (branch `fix/em4-restart-reference`)

ADR 0036 §§5–6 and EM4 (T47): an embeddings deployment now compares its fixed probe with the reference retained before stop, before Ready opens dispatch. A first launch records a reference without a comparison; a passing restart carries the original vector and tolerance forward. A mismatching restart records `wake_mismatch`, keeps dispatch closed and terminates the owned launch, releasing its charge only against verified gone evidence. Chat and decision references remain launch-scoped.

The durable, operation-scoped `embedding_canary_checked` event names `pass` or `fail`, the instance and launch, the reference launch, the probe kind/version, both dimensions, the relative distance and the retained tolerance. Different dimensions have no defined distance and record `null`, with a failing verdict. Different probe versions are not compared. The management events stream projects this closed payload, including numeric distances and tolerances, without vectors or credentials (SPEC §14, T37).

The Fake-engine controller regression `an_embedding_restart_compares_before_ready_and_preserves_the_first_reference` failed before the fix because the restart emitted no comparison event; it passes with an in-tolerance vector, an out-of-tolerance vector and a changed dimension, and checks first-launch recording, store reopening across stop, event ordering and verified cleanup. Store regressions cover baseline retention across generations and incarnations, first launches, probe-version changes and failing measurements. The management test `every_store_event_kind_projects_through_the_events_stream` caught the initially missing projection; it now passes, with dedicated pass/fail, null-distance and closed-payload coverage. Targeted controller residency, store wake-canary, failed-launch and management event tests and the formatting check pass.

CPU and Fake-engine tests are not qualification. EM4 must still be rechecked on the qualification VM and the original unified-memory machine, on every supported embedding engine. EM3 must still establish the measured tolerance and margin; this fix does not change the existing provisional 0.02.

# Release note: Fixes

- **Embedding restart checks.** Restarted embeddings deployments compare their probe with the retained reference before serving requests and record the measured pass/fail verdict. A mismatch stops the launch and leaves dispatch closed instead of silently replacing the reference.

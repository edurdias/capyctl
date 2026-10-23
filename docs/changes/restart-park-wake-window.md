# Status: a `restart_only` wake gets the start's window — 2026-10-23 (branch `fix/restart-park-wake-window`)

Found in the 0.1.7 LMCache benchmark on host A (standalone, SGLang 0.5.22 with LMCache
0.5.6, Qwen3-4B, `residency: restart_only`, `lmcache: required`): one wake in 16 failed
`wake_uncertain` after 103 s. The wake of a `restart_only` deployment with LMCache starts
its engine again (ADR 0033 §7), but its undeclared wake window was the deep-park formula of
ADR 0014 amendment A1 (60 s + 5 s per GB: 105 s for the 8.05 GB checkpoint), and SGLang
restarts the model in 94–99 s; the readiness poll's own bound, two seconds before the
deadline, ran out at 103 s while the engine was still capturing CUDA graphs. The same
deadline ended a server-plus-host wake at 102.5 s in another run on host A. vLLM 0.32.0
restarted the model in 64–68 s, inside the window only because it starts faster.

ADR 0014 amendment A23: an undeclared wake window of a deployment that resolves
`restart_only` is its Initialize window (declared or derived, lowered to the request
deadline), still `derived`; a declared `timeouts.wake` is kept and parking residencies keep
the formula. The store gives a wake the Initialize window whenever the revision resolved
`restart_only`, on the deployment or on a host, and its wake is derived, so revisions frozen
before this change are covered; such a snapshot still decodes with the window it was frozen
with. Status shows the window the store gives.

Tests (T14 T20): `a_restart_only_wake_gets_the_initialize_window` (config: derivation,
declared values, request-deadline bound), `a_restart_only_revision_frozen_with_the_deep_park_wake_still_decodes`,
and the store's `a_restart_only_wake_gets_the_initialize_window` (frozen, host-resolved,
declared and parking revisions). CPU and Fake-engine tests are not qualification. Live
check (host A, 2026-10-23, standalone built from this branch with `fix/wake-flakes` on
`0b6f6fb6`; SGLang 0.5.22 with LMCache 0.5.6, Qwen3-4B, the benchmark's deployment): status
shows a wake window of 1,200 s with `timeouts.initialize: 1200s` and 690 s for the same
deployment without `timeouts` (both were 105 s). 36 restart-park wakes over three launches
(14, 11, 11), each woken by a router request with a new prompt, were all served: 92–99 s,
and one of 197 s that the old window would have failed. The first launch's 15th wake was
held back for memory (`insufficient_memory`, then a 503 after 600 s): the LMCache server's
GPU memory grew by one engine KV cache (5,328 MiB) per restart-park while the charge
counted one. That is a separate defect with its own fix, not this one. The benchmark's
failed wake was the 11th consecutive park of one launch, where that growth had probably
slowed the restart [inference].

# Release note: Fixes

- **A `restart_only` model with LMCache no longer fails a slow wake.** Its wake starts
  the engine again, but it was given the shorter window of a deep-park wake (105 s for an
  8 GB model), and an SGLang model that took longer to start was stopped
  (`wake_uncertain`). Unless the deployment states `timeouts.wake`, the wake now has as
  long as a start (`timeouts.initialize`, or the window CapyCTL derives for a start),
  deployments made before this release included
  ([Parking with LMCache](../guide/parking.md#parking-with-lmcache)).

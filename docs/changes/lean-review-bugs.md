# Status: three accounting bugs from the formal review, fixed on main — 2026-10-20 (branch `fix/lean-review-bugs`)

The Lean review in closed PR #16 (tag `archive/formal-review-lean`) found three
bugs that never reached `main`. Each was re-checked against `main` at
`af661ac2` and was still live; each new test failed there and passes now. The
Lean models are not ported.

- **A cancelled or refused park reopened a gate a restart had closed**
  (SPEC §6.1, §10). A restart closes every dispatch gate without a closure row;
  an adopted launch reopens only through `reverify_local_dispatch`, on fresh
  readiness evidence with no lease left by the retired session. A park
  accepted on the adopted launch and then cancelled at its deadline, or
  refused, called `reopen()` and opened dispatch over the retired session's
  lease. The residency plan now records `reopens_dispatch`: true when the park
  found the gate open, or when a switch releasing its drained victim handed it
  the closure it recorded on this session's open gate. A park adopted from a
  retired session has it cleared. Plans stored without the field never reopen;
  readiness supervision reopens those gates on evidence. Test (T33 T19):
  `a_cancelled_park_does_not_reopen_a_gate_a_restart_closed`.
- **A Ready phase could claim a device its peak phase lacked** (SPEC §7.3).
  Start and wake completions write the Ready footprint without admission, and
  `validate_recipe` compared bytes between phases but never device claims. A
  declared `resources:` block with no device in `cold` and `gpu0` exclusive in
  `ready` resolved, and completion then put a second exclusive owner on the
  GPU. Every base-phase claim must now be held by its peak phase on the same
  device, equally or exclusively. Tests (T26 T27):
  `recipe_peaks_cover_each_adjacent_phase_device_claims` (scheduler) and
  `a_ready_phase_may_not_claim_a_device_its_cold_phase_lacks` (config). The
  `sequence` fixture that built such a recipe now claims the device in its
  peak phases too.
- **A granted request lease leaked when its caller was dropped after the reply
  was sent** (SPEC §10). The writer closed a grant as not accepted only when
  `reply.send` failed. The first lease of a request is opened in the handler
  future, before the dispatch task is spawned (#152 moved only the close), so
  a client disconnect between the send and the caller's next poll dropped the
  ticket inside the channel and left the lease `inflight` for the session. The
  ticket now travels in a guard whose `Drop` queues its `Finish`; the idle
  writer looks for such orphans every 50 ms and flushes them before it stops.
  Test (T17 T19): `a_grant_sent_but_never_taken_is_closed_as_not_accepted`.

CPU and Fake-engine tests are not qualification. No live check is needed for
the recipe rule; the park and lease paths have no live repro yet.

# Release note: Fixes

- **A park that is cancelled or refused no longer reopens dispatch that a
  controller restart closed.** The instance serves again only after its
  readiness is proven in the new session.
- **A `resources:` block whose `ready` phase claims a device its `cold`,
  `parking` or `wake` phase does not is refused** when the deployment is
  written, instead of putting a second exclusive owner on the device when the
  instance becomes ready.
- **A request whose client disconnects while its lease is being granted no
  longer leaves that lease charged** until the controller restarts, where it
  refused parks of the instance and held switch drains to their timeout.

# Status: A park the parked growth bound turns into a stop runs the stop — 2026-10-09 (branch `fix/parked-growth-stop-runs`)

Found live 2026-10-09 on host A (standalone, vLLM 0.30.0 deep, eager
loader). This fixes ADR 0014 amendment A19. A launch's parked charge grew
4.7, 9.6, then 13.1 GiB. Its fourth `capyctl park deployment` became the
growth stop, and status said so. That stop never ran. Its operation stayed
`pending` and its run `queued` for 25 minutes, and the generation-1 engine
kept its 22.6 GiB. An operator's Stop was refused with `lifecycle_conflict`,
and `drain host` asked for reconciliation. A role restart did not clear it.

Cause: `accept_park_command` answers with the stop's operation. It stored the
park's own command receipt (the deployment's `#park` scope, or a sibling
instance's) naming that operation, beside the receipt the stop already has.
The ordinary cleanup's validation (`cleanup::read`) requires exactly one
receipt per cleanup operation. So every read of that stop failed with
`CorruptStoredData`: discovery, arming, status and adoption after a restart.
The coordinator's worker halted with `corrupt stored lifecycle data`. Nothing
was released: the accounting stayed held, with the engine running. The
generation 2 in the ledger is normal: every stop fences its launch to the
next generation. The idle-policy and switch-victim growth stops store only
the stop's own receipt, so they were not affected. Their tests now drive
their stops too.

Fix: the cleanup validation also admits the park command's receipt, and only
that one. It must be under the stop's principal and key, in the deployment's
or this instance's `#park` scope, for a stop journaled `park_growth_stop`.
Any other extra receipt is still corruption. An exact retry of the park
still replays the stop, also after the stop has settled. The stop still
releases only on its gone evidence.

Tests, failing before and passing after:

- `a_park_the_growth_bound_turns_into_a_stop_runs_the_stop` (controller
  scheduler, scripted embedded engine whose host reports growing parked
  residues). Three parks and wakes are measured at 3, 5.5 and 7 GiB. The
  fourth park becomes the stop. Before the fix, the worker read
  `Failed("coordinator service failed: corrupt stored lifecycle data")` with
  the stop `pending` and the instance at generation 2, observed `ready`, as on
  host A. After the fix, the engine is terminated once, the instance is
  released and reads `stopped`, the fourth park is never sent, the worker keeps
  running, and an operator's Stop afterwards is accepted.
- `a_park_after_the_parked_charge_outgrew_its_first_park_is_a_stop` (store).
  The stop is discovered, armed and released on gone evidence, and a retry
  replays it before and after. Before the fix, discovery failed with
  `CorruptStoredData`.

New: `a_growth_stop_refuses_a_park_receipt_it_did_not_answer` (the same
receipt under another principal is still corruption). The idle-policy and
switch-victim growth tests now also drive their stops; they passed before.

CPU and Fake-engine tests only; they are not qualification. Live recheck
still needed: on host A, on a fresh ledger, repeat the four park/wake cycles
on vLLM 0.30.0 deep and confirm the fourth park's stop terminates the
engine, releases it and leaves it stopped.

# Release note: none

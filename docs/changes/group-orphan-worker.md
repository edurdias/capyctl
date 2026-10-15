# Status: A killed group worker's unrecorded processes are stopped — 2026-10-15 (branch `fix/group-orphan-worker`)

Found live in MN5 vLLM (0.1.3 matrix, run matrix-20261015T071325Z): SIGKILL of the
worker member's vLLM process (rank 1 on host B) left its `VLLM::Worker_TP1` (38 GiB of
device memory) and its resource tracker running, reparented to the host role. Rank 1
stayed `uncertain` with its 52.9 GB charge held, the deployment stayed `stopping` for
more than 1 h 45 min, `start` answered `still_stopping`, `delete` answered
`delete_requires_cleanup`, and host B logged `host control session ended: a local
effect failed; reconnecting` 15 times.

Cause. A group worker's recorded tree is the one `worker_spawned` reports once the
tree is unchanged for one second after the spawn; vLLM forks its TP worker about four
seconds later, so rank 1 recorded only its leader (`members-vg-30-ready.json`). The
leader's launcher reaped it at once, and a leaderless process group never authorized a
signal (ADR 0028 §8, §11: its id can name an unrelated group once the pid is reused).
Each `Terminate` (one per stop retry) therefore signalled only the dead leader and
ended in `recorded processes gone but the group still has members`, which the host's
Terminate effect turns into a session end (the 15 reconnects); no gone evidence was
ever answered. The stop's retries ended with its deadline (the last `cleanup_retried`
at 08:39:48), so nothing asked again after the orphans were killed by hand; a fresh
`stop` is refused `still_stopping` and host-role restarts only reconcile a journal
whose launch claim was still retained.

Fix (ADR 0028 amendment of 2026-10-15, SPEC §13.2): the durable launcher holds each
launch's leader unreaped (`waitid` with `WNOWAIT`) until no other process of its group
runs, which pins the group id; `terminate_escalating` signals the whole group through
a held leader, alive or dead, so the unrecorded worker gets the group's SIGTERM and
SIGKILL, and the Terminate answers gone evidence once every recorded identity is gone
and the group is empty. A held leader that has exited reads gone, and its exit is
reported as before with the status the wait observed. A leader this role did not spawn
(launched before a restart of the host role) is not held, and its leaderless group is
still never signalled: that case stays uncertain and charged. Recording the worker's
whole tree at launch is a separate change (MN4 park).

Tests (T31, T33). `capyctl-launchers`
`an_unrecorded_worker_of_a_dead_held_leader_is_terminated` fails on `main` (the
Terminate errors) and passes now; `an_unheld_leaderless_group_is_never_signalled`
guards the unheld case and passes on both. `capyctl-agent` `group_launch`
`a_dead_worker_leaders_unrecorded_child_is_terminated_and_the_stop_settles` replaces
`late_worker_children_are_recorded_and_unrecorded_survivors_stay_uncertain`: it fails
on `main` (the Terminate ends the session) and passes now, with gone evidence for
exactly the recorded identities and the claim released. CPU and Fake-engine tests are
not qualification. Live check still needed: rerun MN5 vLLM; after the rank-1 kill, the
plan should settle within seconds with `failed_rank 1`, no `VLLM::Worker_TP1` or
resource tracker left on host B, no `a local effect failed` in host B's log, and the
member's charge released.

# Release note: Multi-node groups

- **A killed group worker no longer leaves its engine processes behind.** When a
  worker member's engine process died while processes it had started still ran (a
  vLLM `Worker_TP1` holding device memory), the stop could not prove the member gone,
  the deployment stayed `stopping` with the member's memory charged, and `start` and
  `delete` were refused. The host now stops every process of the member's launch and
  the group settles.

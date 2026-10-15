# Status: A group member on an unreachable host reads uncertain — 2026-10-15 (branch `fix/group-unreachable-member`)

ADR 0028 §11 says a member whose host cannot be reached stays charged and
`uncertain` (`group_member_uncertain`). The 0.1.3 live row MN7 (vLLM TP2 across
two GB10 hosts) stopped host B's agent: host B showed offline at once, but
rank 1 stayed `launched` for seven minutes (`13-uncertain-check.out`,
`18-held-check.out`). Nothing moved a member on session loss. Only a stop, a
silent park or an unanswered Launch marked a member uncertain, and a
single-rank instance only gets its dispatch closed (`reconciling`).

The readiness supervisor (`remote_readiness.rs`) already runs a pass whenever
a host session changes, and every 250 ms. Each pass now also visits every
launched or uncertain member of an active group plan:

- A `launched` member whose host has no current session (lost, draining, or
  silent past the heartbeat bound) becomes `uncertain`
  (`Store::mark_member_unreachable`). Its charge, dispatch fence and recorded
  identities are kept, and nothing is released.
- An `uncertain` member becomes `launched` again only when its host's
  reconciled session replayed a journal that still claims the member's Launch
  (`AgentSessions::claims_launch`, the session's `ReconcileHistory`), through
  `Store::mark_member_reconciled`. The store refuses this when another path
  owns the uncertainty: no identities recorded (an unanswered Launch), a
  recorded group failure, a stop run in progress, or an instance status that
  names an unsettled stop. A host back with an empty journal proves nothing,
  so its member stays uncertain until a stop settles it on evidence. An
  engine that exited while its host was away is reported again once the
  member is back to `launched`, and that exit fails the group as any exit
  does.

Tests (T30, T32, `coordinator/tests_groups.rs`, which drives the production
supervisor over the group world):
`a_lost_member_host_reads_uncertain_until_its_journal_returns`,
`a_member_host_back_with_an_empty_journal_stays_uncertain` and
`a_member_that_exited_while_its_host_was_away_fails_the_group_on_return`.
All three fail without the member pass and pass with it. CPU and Fake-engine
tests are not qualification. Live check still needed: rerun MN7 (vLLM) and
check that rank 1 reads `uncertain` with its charge held within a second or so
of host B's agent stopping, then reads `launched` after the agent returns,
and that the group still stops cleanly.

# Release note: Multi-node groups

- **A member on an unreachable host reads `uncertain`.** When a member host's
  agent goes offline, its member now shows `uncertain`
  (`group_member_uncertain`) at once and keeps its memory charge. When the
  agent comes back with its journal, the member shows `launched` again.

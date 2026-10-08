# Status: Explicit activation — 2026-10-08 (branch `feat/explicit-activation`)

Owner-approved 2026-10-08. `lifecycle.activation: explicit` (deployment
document, or a host document for every deployment that may run on it;
standalone `host.lifecycle.activation`, also `--set` / `CAPYCTL_SET__…`)
makes a deployment move only on an operator's action (SPEC §6.5, §10
amendments). A request while it is stopped or parked is refused at once, 409
`deployment_inactive`, with nothing accepted; it is never a switch victim,
never idled, never reclaimed from the parked set (a park that needs its room
is refused `parked_capacity`). An operator's stop keeps `deployment_stopped`.
Schema v45 adds `deployment_revision_instances.activation` (`on_demand` for
existing rows) and `host_activation_policies`, written with each approved
host publication and at every standalone start. Unset: command identity,
recipe fingerprints and the host's resolution document are unchanged (tests).
Status shows `activation: explicit` (additive).

Tests (failing before, passing after): config `instances`, `remote_roles`,
`setting_overrides`; store `instances_placement`, `host_publication`,
`park_tests`, migration v45; controller `tests_switching`; CLI
`explicit_activation` (HTTP 409 end to end, park and wake by operator).
CPU and Fake-engine tests only; not qualification. Pending: a live check on a
lab host (explicit host, a request for a parked deployment answered 409 with
no wake, then `start` waking it; a second deployment's request not evicting
it). Under a `resource_policy.queue.max_pending_per_deployment` of 0 the
refusal is still 409 `deployment_inactive`, not 429 `queue_full`, since no
activation would start (CLI test
`with_no_waiting_an_inactive_explicit_deployment_is_refused_409`).

# Release note: Lifecycle

- **Models that move only on your command.** `lifecycle.activation: explicit`
  in a deployment, or in a host document for every model the host may run
  (`host.lifecycle.activation` in standalone, also through `--set` and
  `CAPYCTL_SET__…`), makes CapyCTL start, stop, park and wake that model only
  when an operator asks. A request while it is stopped or parked is refused
  at once with 409 `deployment_inactive` and starts nothing; it is never
  parked or stopped to make room for another model, never by the idle
  timers, and never stopped to make room among parked models. `start`,
  `stop`, `park` and the rest work as before. Omitted, nothing changes. See
  [Only on your command](../guide/parking.md#only-on-your-command). With a
  queue of zero, a model that only an operator may start (stopped by one, or
  with explicit activation) is still answered 409, not 429 `queue_full`.

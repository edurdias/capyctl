# Status: Two load-dependent failures: the subreaper fixture and the port probe — 2026-10-09 (branch `test/fix-two-flakes`)

Two tests failed once each under load and passed when run again.

`subreaper::tests::an_inherited_zombie_is_reaped_and_one_that_cannot_be_is_reported`
(capyctl-launchers, from #88) failed in CI "CPU checks" with
`left: Gone, right: Unknown` at `subreaper/tests.rs:159`. This was a test
fault. The fixture's background children exited on their own (`/bin/true`,
`sleep 0.5`) while their shell might still be a shell. A shell reaps a
background child that has already exited when it next finishes a command,
built-ins included: dash does this after `echo $!`. When the child exited
before that point, the shell reaped it. The "zombie of another live process"
was then already gone (CI's failure). Locally the more common symptom was
`timed out waiting: the child exits`. The children now run `sleep 30` and the
fixture kills them: the orphan only after its shell has exited, and the zombie
only once its parent has exec'd `sleep` (its `/proc/<pid>/comm`), which never
waits. No production code changed for this one.

`group_launch` `a_worker_answers_no_completion_probe` failed in the #97 deep run
with `service_port_in_use:23868`. That port came from the `capyctl_testkit`
pool (#92), and this was the first test of the binary to finish. The earlier
`sglang_worker_ignoring_sigterm_is_escalated` failures with the same signature
(ports 33274, 37834 and 41162) ran before #92. Every FakeHost test takes this
path. The cause was in the agent. `host_checks::port_free` and
`refusal::engine_port_free` probed with `TcpListener::bind`, which leaves the
probe socket listening until it is dropped. The agent forks engines from other
threads (`pre_exec`, so a real `fork`). A child forked while a probe was open
kept a listening copy of it until its `exec`, and the next probe of the same
port was refused as if another program held it. A group Launch probes its port
five times: three binds in `port_free`, then `engine_port_free` in pre-admission
and again under the journal lock. So a free port was refused while sibling
tests were spawning engines. In production this was a spurious, retried
refusal. The probes now go through `host_checks::probe_bind`, which binds with
`SO_REUSEADDR` (as `TcpListener::bind` does) and never listens. A copy of it
in a child refuses no later probe and no listener that sets `SO_REUSEADDR`.
It still refuses whenever anything listens on the address or on an address
overlapping it.

Tests. Every loop ran on 4 cores (`taskset`) beside 8 busy loops pinned to the
same cores.

- Subreaper test alone: before, 46/50 (three `timed out waiting: the child
  exits` and CI's `Gone`/`Unknown`). After, 50/50 and 200/200. Whole
  `capyctl-launchers` lib: before, 18/20 (both failures in this test). After,
  20/20.
- Port probe mechanism, as a standalone program: `port_free`'s three binds ran
  in a loop on a free port while 4 threads spawned `/bin/true` through `fork`.
  The listening probe refused 304,966 of 423,039 probes. The bound probe
  refused 0 of 225,553. Both probes refuse a listener on 127.0.0.1, 0.0.0.0,
  `::`, `::1`, 127.0.0.2 and a LAN address.
- In place: during the Launch of `a_worker_answers_no_completion_probe`, 16
  threads of the test process spawned `/bin/true` through `fork`, as sibling
  tests spawn engines. Before, 94/100, with six `service_port_in_use:<pool
  port>`. After, 100/100. Without that injection the failure is too rare to
  measure here: the whole suite passed 35/35 before at 4 test threads. After
  the fix, the failing test alone passed 50/50, the escalation test alone
  50/50, and the whole suite at 32 test threads 20/20.
- New unit tests `host_checks::tests::a_probe_left_open_holds_nothing` and
  `refusal::tests::a_probe_left_open_is_no_port_conflict` hold a probe's socket
  open, as a forked child would. Both fail with the listening probe and pass
  with the bound one.

Seen while stressing, not changed here: with five instances of the
`group_launch` suite at once on 4 busy cores,
`late_worker_children_are_recorded_and_unrecorded_survivors_stay_uncertain`
failed 50/50 at `group_launch.rs:1426`. Its Launch took longer than the 3 s
after which the fake starts its unrecorded child. Under normal load it
passes.

CPU and Fake-engine tests only; they are not qualification. No live check is
needed: the probe refuses the same holders as before.

# Release note: none

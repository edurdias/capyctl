# Status: Group launch tests clean up their processes and directories — 2026-10-20 (branch `fix/group-launch-test-cleanup`)

Found on the development laptop on 2026-10-20: four `capyctl-group-launch-*`
fixture directories from 2026-10-09 in the home directory, and two fake SGLang
engine processes (`runtime/sglang_entry.py`) from them, running for 11 days.
They came from `crates/capyctl-agent/tests/group_launch.rs`. The fake engine's
leader watched its parent, but the children it forked watched only the fixture
directory. A test that is interrupted or killed runs no `Drop`, so its
directory stays, and children whose leader had already died ran on. On a
panic, the cleanup terminated only the launches the host's journal still
claimed.

The fix is in that test file only (SPEC §13.2: the cleanup is of the test's
own process groups, found by the fixture path on their command lines):

- **The fixture guard.** The host's fixture directory is now owned by a guard
  whose drop runs when a test passes, fails an assertion or panics. It kills
  every process whose command line names a path inside the fixture, each with
  its whole process group (never the test's own group), waits until none is
  left, then removes the directory, retrying while the host's threads may
  still write into it. It replaces the journal-based cleanup.
- **Children die with the test.** Every process of the fake engine's tree (the
  leader, the child it forks at once and its late children) watches the test
  process that spawned it, by pid and start time, and the fixture directory,
  and kills its process group once either is gone. `PR_SET_PDEATHSIG` fires
  when the spawning thread exits, and the agent starts engines from tokio's
  blocking pool (`journal.execute` under `spawn_blocking`), whose threads exit
  when idle, so it cannot be used for the engines; the new interruption test
  uses it, as the CLI tests' `Guarded` does, for the child test process it
  starts.
- The fixture stays under the home directory, because the owner-only checks of
  the runtime directory and the host's private state (SPEC §13.3) refuse a
  shared `/tmp` ancestor. The other tests that create temporary directories
  under the home directory remove them on drop and were not changed.

Tests (T12): `an_interrupted_test_leaves_no_engine_process_behind` runs the
test binary again as a test whose SGLang worker's leader died and left its
children, kills it with SIGKILL, and requires every process of the tree to end
by itself. With the old fake engine it failed (three processes of the killed
test's tree still running after 10 s); it passes now.
`a_failed_test_leaves_no_engine_process_or_directory_behind` panics while it
owns such a host and requires no fixture process and no fixture directory
afterwards; it passed before as well and guards the new cleanup. Checked by
hand with the whole `group_launch` binary (four test threads) interrupted after
12 s with SIGINT, SIGTERM and SIGKILL: each time 8 fixture processes were
running, none 3 s after the signal. The four fixture directories of an
interrupted run remain, since no `Drop` runs on a signal death, but nothing
runs in them.

CPU and Fake-engine tests are not qualification. No live check is needed: the
change is to a test file only.

# Release note: none

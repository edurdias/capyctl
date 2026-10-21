# Status: Tests no longer depend on the machine's GPU, memory or speed — 2026-10-21 (branch `test/hardware-independent`)

Tests only; no product behavior changes. Several tests passed or failed with
the machine running them rather than with the code (AGENTS.md: tests state
their observations). Each now states what it observes through an existing
seam, and none was deleted, skipped or loosened:

- **GPU (SPEC §7.2, design §1).** Every `capyctl` process a binary test spawns
  observes no GPU through `CAPYCTL_TEST_PINNED_GPU`, read only by debug builds
  like the existing `CAPYCTL_TEST_PINNED_HOST_MEMORY`: `nvidia-smi`'s memory
  and process samples and the device inventory read nothing of the machine.
  On the laptop, whose GPU another program held (1.9 GiB of 16 free), every
  standalone launch in `engine_exit` (2 tests) and `role_shutdown` (5) failed
  `insufficient_device_memory`. Their documents are now always sized for the
  unified shape the pinned binary publishes.
- **Device inventory (SPEC §3).** A standalone boot through the test entry
  points collects no device inventory, as it already sampled no GPU. On
  host A the real collector named the host after the machine, so
  `admission_figures::a_role_warns_when_its_limits_exceed_the_memory_available`
  and `switching_cli::standalone_memory_limits_follow_the_document_and_its_overrides`
  and `standalone_parked_limit_follows_the_document_and_its_overrides` found
  their warnings and policy under `spark-0292` instead of `standalone`.
  Production boots still run the collector.
- **Model store (ADR 0008).** `prune_sources_lists_then_removes_only_unreferenced_copies`
  resolves the store with an empty environment layer; the process's own
  named host A's real store, which the listing then showed.
- **Speed.** `only_an_engine_answering_busy_is_not_signalled` no longer
  requires the idle, exited and loading engines to finish within 900 ms: they
  get a minute-long idle-check bound and must complete inside the 15 s the
  stop is awaited, so one that waited for its bound still fails.
  `group_engine_env_is_approved_everywhere_and_equal` and
  `empty_journal_reconnect_settles_on_recorded_identities_only` state the
  longest observation TTL a policy accepts (10 s); under the default 2 s a
  loaded run (703 s for the controller suite) committed on stale evidence and
  the store refused it (`evidence freshness`).

Each fixed test passed 20 runs in a row on the laptop, under the gate lock.
CPU and Fake-engine tests are not qualification; no live check is needed.

# Release note: none

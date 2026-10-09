# Status: Binary-driven CLI tests no longer read the machine's free memory — 2026-10-08 (branch `test/standalone-memory-independent`)

Owner decision 2026-10-08. The `capyctl-cli` integration tests that spawn the
real binary (standalone and host roles) read this machine's own
`/proc/meminfo`. With about 14 GiB available of 61, every deploy failed with
`insufficient resources`, on clean `main` too: standalone derives a 20% free
reserve, and the golden host states a 16 GiB one. In debug builds only,
`capyctl_agent::memory::read_host_memory` now reads the test-only
`CAPYCTL_TEST_PINNED_HOST_MEMORY` (`<capacity_bytes>:<available_bytes>`) when
it is set. Release builds compile that read out. It is not a user setting and
is not documented as one. `support::capyctl()` pins every role it spawns to the
in-process suite's stated 32 GiB, all of it free. This goes through
`support::pin_host_memory`, which the two helpers that clear the environment
call again. The in-process tests already state their memory
(`support::test_memory`). The real `/proc/meminfo` path keeps its own tests
(`crates/capyctl-agent/tests/memory.rs`). On a discrete machine, GPU memory is
still sampled from `nvidia-smi`.

Tests: the five `role_shutdown` and two `engine_exit` standalone tests named in
the decision failed before with 14 GiB available. With a held allocation keeping
13.5–14.8 GiB available, all 20 binary-driven test files passed 3 times
(163 tests each). `cargo test -p capyctl-cli --all-targets` passed (463 tests)
with 13.7 GiB available. New unit tests check the pinned reading's format.
Still open: in one earlier run,
`a_remote_engine_exit_is_reported_settled_and_relaunched_on_demand` waited out
its 900 s deploy deadline. Its reason was not captured, and 14 later runs
passed, 10 of them on 2 cores. CPU and Fake-engine tests only; they are not
qualification. No live check is needed.

# Release note: none

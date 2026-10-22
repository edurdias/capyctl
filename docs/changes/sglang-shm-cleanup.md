# Status: SGLang: a failed launch leaves no shared memory behind — 2026-10-22 (branch `fix/sglang-shm-cleanup`)

Found live in the live-017 recheck on host A (SGLang 0.5.22): with the scheduler
SIGKILLed during the weight load, the launch failed promptly and nothing it started kept
running, but `/dev/shm/sglang_loads_*.shm` was left behind. `/dev/shm` is RAM. SGLang's
scheduler creates that file (its load snapshot, `managers/load_snapshot.py`) and maps it,
and SGLang never removes it outside its own CI sweep; segments made through Python's
`multiprocessing.shared_memory` are removed only by the resource tracker, which the
entry's cleanup kills with everything else (SPEC §13.2: the entry ends every process it
started, verified gone).

`runtime/sglang_entry.py` now watches `/dev/shm` while `launch_server` runs and claims the
files that are the launch's on evidence, never on a name pattern: a file is claimed only
if it appeared after the launch began and, when first seen, the entry or one of its
descendants held it open or mapped (read from `/proc/<pid>/fd` and `/proc/<pid>/maps`).
When the launch fails, the entry claims once more while the processes still run, ends
them as before (frozen, killed, and `/proc` read again until none runs), and only then
removes each claimed file that is still the same file (device and inode), owned by this
user with no group or other access, and held by no live process other than the exiting
entry. Every process the user can inspect is checked; a non-dumpable process of the user
started after the entry keeps every file. A segment that existed before the launch, one
another process made during it, and one a process outside the launch still maps all
stay. The watch lists the directory every 0.25 s, so a file whose creator ended within
that time of making it is not claimed and stays. A launch that serves is not affected:
its stop is unchanged.

## Tests

- `runtime/tests/test_sglang_startup_death.py`,
  `test_a_failed_launch_removes_the_shared_memory_it_left` (T12): a stub SGLang scheduler
  creates and maps a `sglang_loads_*.shm` file as SGLang 0.5.22 does, creates a
  `multiprocessing.shared_memory` segment, maps a segment made before the launch, and
  makes one more that a process outside the launch (detached into its own session) maps;
  the test makes another segment of its own during the launch and then SIGKILLs the
  scheduler. The launch fails with `engine_launch_failed` as its last line, the load
  snapshot file and the multiprocessing segment are gone, and the other three remain.
  Before the fix the load snapshot file remained.

CPU and Fake-engine tests are not qualification.

# Release note: Fixes

- **A failed SGLang launch no longer leaves shared memory behind.** A launch whose
  scheduler died while loading left SGLang's `/dev/shm/sglang_loads_*.shm` file, and
  other shared-memory segments its processes made, in RAM. The protected entry now
  removes the segments the launch made, once its processes are verified gone and no
  other process holds them.

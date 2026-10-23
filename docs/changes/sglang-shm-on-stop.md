# Status: SGLang: a stopped launch leaves no shared memory behind — 2026-10-23 (branch `fix/sglang-shm-on-stop`)

Found live on host A (SGLang 0.5.22) in the 0.1.7 checks: after a normal stop
(`delete --stop`, then the role drained) `/dev/shm/sglang_loads_*.shm` (16 KiB) and a
`sem.mp-*` semaphore were still there. The failed-launch cleanup of `sglang-shm-cleanup`
(#185) ran only when `launch_server` raised. A stop sends SIGTERM to the launch's process
group, the entry included, and the engine logs show what SGLang 0.5.22 then does: once
it serves, its handler drains, kills its process tree without the entry and
`launch_server` returns (`kill_process_tree called: ... include_parent=False`, then
uvicorn's `Finished server process`); after a child's exit its SIGQUIT handler instead
kills the tree and then the entry with SIGKILL (`include_parent=True`); while it loads it
has no SIGTERM handler, so SIGTERM's default action ended the entry at once. In none of
these did anything in the entry remove the launch's files.

`runtime/sglang_entry.py` (SPEC §13.2, T12) now ends the launch the same way on every one
of those paths, once: when `launch_server` returns or raises; on a SIGTERM the entry
handles itself while `launch_server` runs (SGLang's own handler replaces it once it
serves); and on the entry's SIGKILL to itself, which its `os.kill` intercepts while
`launch_server` runs (psutil's `Process.kill`, which SGLang's `kill_process_tree` uses,
goes through `os.kill`). The watch claims once more, every process the entry started is
ended and seen gone (frozen, killed, `/proc` read again until none runs), and only then
is each claimed file removed under #185's rules: claimed on evidence (appeared after the
launch began, held by the entry or a descendant when first seen), still the same private
file of this user, and held by no other live process. The SIGTERM then ends the entry as
before, and the SIGKILL is delivered. A semaphore the entry made and removed this way has
its pending multiprocessing finalizer cancelled, so a normal exit prints no
`FileNotFoundError` traceback after the entry's last line. A child forked while the
entry's handler is in place (a worker pool) is ended by SIGTERM as before and never ends
the launch. Both hooks are handed back when `launch_server` returns. A stop the host
escalates to SIGKILL (after its 15 s grace) ends the entry with no chance to run, and
its files stay.

## Tests

- `runtime/tests/test_sglang_startup_death.py`, `SharedMemoryAfterStopTests` (T12): a stub
  SGLang tree whose scheduler makes the segments of the failed-launch test (a load
  snapshot file, a `multiprocessing.shared_memory` segment, a pre-existing segment it
  maps, one a process outside the launch maps) and whose entry keeps a
  `multiprocessing.Lock` (`sem.mp-*`); the test makes one more segment during the launch
  and stops it with SIGTERM to the process group, as the host does. Three ways: stopped
  while serving, the stub drains, kills its tree without the entry and returns (exit 0);
  stopped while serving, it kills the tree and the entry with SIGKILL through `os.kill`
  (exit by SIGKILL); stopped while loading, before it has a SIGTERM handler (exit by
  SIGTERM). Each time the process group is empty, the load snapshot file, the
  multiprocessing segment and the semaphore are gone, the other three remain, and stderr
  holds no traceback. All three failed before the change.
- `ForkedChildTests` (T12): a child forked with the handler in place, sent SIGTERM, ends
  itself with SIGTERM and touches neither the watch nor the entry.
- `test_sglang_entry.py`: the in-process launch tests now stub `_end_engine_processes`,
  since a returning `launch_server` ends the launch's processes, which in the test
  runner would be its own children.

CPU and Fake-engine tests are not qualification.

## Live check still needed (host A, SGLang 0.5.22)

Release build of this branch, standalone role on 38443/37443 with a fresh state
directory, Qwen3-4B-Instruct on SGLang 0.5.22. Record `ls -la /dev/shm` before the start,
while serving (the launch's `sglang_loads_*.shm` and any new `sem.mp-*`) and after each
stop. Stop it three ways: `capyctl stop deployment` while it serves (SGLang drains and
returns); `capyctl stop deployment` while it loads weights (the entry's own handler);
and, on `~/sglang-0.5.22-lmcache-venv` with `lmcache: required` and
`residency: restart_only`, a `capyctl park`, which stops the engine. Then one stop
through SGLang's SIGQUIT path: SIGKILL the launch's detokenizer (its pid from the
recorded process group after pid, start ticks and boot id are checked) and let SGLang end
the rest. Each time: the engine log shows no traceback after SGLang's own lines, the stop
settles with a verified exit within the 15 s grace (not escalated), the process group is
empty, and no `/dev/shm` entry the launch made remains, while entries that existed
before stay. Then one deep park and wake, to show a serving launch keeps its files until
it stops.

# Release note: Fixes

- **A stopped SGLang model no longer leaves shared memory behind.** Every SGLang start
  left its `/dev/shm/sglang_loads_*.shm` file and a semaphore in RAM after the model
  stopped. The protected entry now removes the shared memory the launch made when it
  stops, as it already did for a failed start, once its processes are verified gone
  and no other process holds it.

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

## Live on host A (2026-10-23, SGLang 0.5.22)

Release build of `064bbf1d` (this branch rebased on `05eee83b`), standalone on
38443/37443 with a fresh state directory, Qwen3-4B-Instruct (`context_length: 16384`,
`kv_cache: 8GiB`). For each case, `/dev/shm` was listed before the start, when the launch
was up (the new entries, and which launch process maps or holds each one), and after
the end. The 70 entries already in `/dev/shm` from earlier runs (old `sglang_loads_*`,
`sem.mp-*`, a `cuda.shm.*` and a `torch_*`) were still there after every case.

| Case | Launch's new entries | How it ended | Group empty | Left behind |
| --- | --- | --- | --- | --- |
| Stop while serving (`capyctl stop deployment`, after one chat) | `sem.mp-pac61y7_`, `sglang_loads_1a0c…shm` | SGLang drains, `kill_process_tree … include_parent=False`, `Finished server process`: `launch_server` returned | 2.8 s | none |
| Stop during weight load: `timeouts.initialize: 35s` ran out with 1 of 3 shards loaded | `sglang_loads_cc44…shm` | CapyCTL ended the start; the receipt says every recorded process was seen gone 0.8 s after the group started exiting; status `failed` (`launch_failed`) | under 1 s | none |
| `restart_only` park, `~/sglang-0.5.22-lmcache-venv`, `lmcache: required` (LMCache on, 4 GiB pool) | `sem.mp-ha3edrcl`, `sglang_loads_c3dd…shm`, `torch_28618_…_0`, `cuda.shm.3e8.6fca.1` | SGLang drains, `include_parent=False`, `Finished server process`; status `parked` | 2.8 s | `cuda.shm.3e8.6fca.1` (80 KiB, see below) |
| SIGKILL to the owned `sglang::detokenizer` (pid checked against the entry's recorded pid, start ticks and boot id) | `sem.mp-tuo3vhk4`, `sglang_loads_b7eb…shm` | `detokenizer … crashed with exit code -9. Triggering SIGQUIT`, 5 s wait, `kill_process_tree … include_parent=True`; status `failed` | 6.3 s | none |
| Deep park, wake by a chat, then stop | `sem.mp-o8cbjwrl`, `sglang_loads_7dae…shm` | Park: `release_memory_occupation` 200, all 4 processes and both entries kept. Wake: chat 200 in 57.9 s (sent while still parking), same entry pid, both entries kept. Stop: SIGTERM, then `detokenizer … exit code -15`, the SIGQUIT path, `include_parent=True` | 6.1 s | none |

Every end settled well inside the 15 s grace. No stop escalated to SIGKILL, and each
binding was released with `every recorded process observed gone`. Nothing was logged
after SGLang's own last line. The engine logs have no `FileNotFoundError` and no
traceback after the end began. Before the end, each request the engine served logged a
uvicorn access-log `--- Logging error ---` traceback (`TypeError: cannot unpack
non-iterable NoneType object`). The same tracebacks appear in the 2026-10-22 SGLang 0.5.22
logs taken before this change, so the change does not cause them.

- **A stop during weight loading is deferred.** A `capyctl stop deployment` sent one
  second after `Load weight begin` was recorded as `administrative_stop_deferred` and was
  delivered only when the start reached Ready, 80 s later. The SIGTERM then reached a
  serving engine, and SGLang took its SIGQUIT path (the scheduler exited -15; a SIGKILL
  to the entry, intercepted). The group was gone about 7 s after the SIGTERM (the log
  has whole seconds), and its `sglang_loads_27dc…shm` was removed. So `stop` never
  reaches the entry's own handler while weights load. The initialize timeout above does
  reach it. That launch's file is gone, which neither SIGTERM's default action nor a
  SIGKILL would do (inference: the entry's handler ran; it logs nothing).
- **LMCache leaves one CUDA IPC file per launch.** The LMCache park case fails the
  "nothing left" criterion with one file. `cuda.shm.3e8.6fca.1` is named for the
  launch's scheduler (pid 28618 = 0x6fca). When the entry ended, the LMCache server
  (pid 28230, CapyCTL-owned, kept through a `restart_only` park) also mapped it, so the
  entry correctly left it under #185's rule. It was still there after `capyctl stop
  deployment` ended that server, so nothing removes it. vLLM 0.32.0 with LMCache does the
  same on host A (a `cuda.shm.*` and a `torch_*` per EngineCore). The leftover is outside
  this change: neither the entry nor the LMCache server's stop removes it.
- **Known limit, not exercised:** a stop that the host escalates to SIGKILL after its 15 s
  grace ends the entry with no chance to clean up, and its files stay.

Evidence: `target/live/live-018/` on the maintainer laptop (`res/b1-…` to `res/b5-…`,
each with `shm-before/launch/after`, `held-by-launch.txt`, `engine.log`, `poll.txt`,
`lifecycle-evidence.txt`). This is a live check of one engine version and one model on
one host.

# Release note: Fixes

- **A stopped SGLang model no longer leaves shared memory behind.** Every SGLang start
  left its `/dev/shm/sglang_loads_*.shm` file and a semaphore in RAM after the model
  stopped. The protected entry now removes the shared memory the launch made when it
  stops, as it already did for a failed start, once its processes are verified gone
  and no other process holds it.

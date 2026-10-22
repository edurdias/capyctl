# Status: SGLang: a startup failure fails the launch promptly — 2026-10-22 (branch `fix/sglang-startup-death`)

Found live on host A on 2026-10-20, in a container: SGLang's scheduler failed at
startup (a missing CUDA library) but the protected entry stayed alive, so the host
waited out the whole readiness deadline and reported that runtime ownership was
uncertain instead of a launch failure (SPEC §§6.4, 13.2). No raw log survives. The
cause below is read from SGLang's public source (v0.5.20, v0.5.21 and v0.5.22 alike)
and reproduced with a synthetic SGLang tree; it is not confirmed against that run.

A scheduler that dies before it reaches its own exception handler (an import that
fails in the spawned child, a missing shared library) sends its parent no SIGQUIT.
SGLang's `_wait_for_scheduler_ready` (`entrypoints/engine.py`) polls the scheduler
every 5 s, sees it dead and raises out of `launch_server`. The entry reported
`sglang_startup_failed: startup_error` and returned, but interpreter exit then joins
the processes SGLang had already started (the detokenizer, any live scheduler), which
are non-daemon children that never exit. The entry stayed alive, so the host's
readiness loop saw the engine present until its deadline. A scheduler that raises
inside its handler was already prompt: it sends SIGQUIT, and SGLang's handler kills
the whole tree, the entry included.

`runtime/sglang_entry.py` now catches an exception raised out of `launch_server`
and ends everything it started before it leaves: it stops every running descendant
(reading the tree again until each one is stopped, so none forks a survivor), kills
them, reaps its own children, and exits only once `/proc` shows none running,
reporting `sglang_startup_failed: engine_launch_failed` with a fixed hint. The host
sees the entry gone at its next presence check and reports a launch failure with the
engine's log tail. Nothing about release changed: memory is still released only on
the host's verified gone evidence for the launch's process group. A process that will
not die keeps the entry alive, and the readiness deadline reports uncertainty as
before, never a release.

The repeated prometheus `FileNotFoundError` traceback about a missing `/tmp` file
was ours, and the same fix removes it. With `enable_metrics` on (CapyCTL's reserved
setting), SGLang 0.5.22's `set_prometheus_multiproc_dir` (`utils/common.py`) makes
the multiprocess directory a `tempfile.TemporaryDirectory` owned by the parent, and
its children create their metric files in it. The engine's environment is cleared
and carries no `TMPDIR`, so the directory lands in `/tmp`; that only places it. The
entry's normal exit after the failure ran that directory's finalizer before
multiprocessing's join of the still-running children (atexit runs the last-registered
handler first, and `import torch` loads `multiprocessing.util`, which registers the
join, long before SGLang makes the directory), so the
children kept failing to create files in it. With the children ended first, the
directory is removed with nothing left writing into it. SGLang's own failure path
never reaches this: its SIGQUIT handler kills the parent outright, with no finalizer.

Tests: `runtime/tests/test_sglang_startup_death.py` (T12) runs the entry to
interpreter exit as its own process group's leader, as the launcher does, against a
synthetic SGLang tree whose `launch_server` does what 0.5.22's does: the parent-owned
prometheus directory, a non-daemon spawned detokenizer with a child of its own, a
scheduler that exits 1 without signalling its parent, and the readiness poll. The
entry exits 1 within seconds (about 1 s here) with `engine_launch_failed`, no running
process is left in its group, the detokenizer and its child are gone, no
`FileNotFoundError` was printed, and the prometheus directory is removed. Before the
fix the same test timed out at 60 s with the entry alive, and the surviving child
printed 83 `FileNotFoundError` tracebacks in its first 5 s. CPU and Fake-engine tests
are not qualification.

Live check still needed, on host A with the approved SGLang 0.5.22 environment: start
a deployment and, while the weights load, send SIGKILL to its `sglang::scheduler`
process (a member of the launch's process group, whose leader is the recorded
entry). A killed scheduler runs no handler, which is the path seen live. Expect the
launch to fail within about 10 s as a launch failure, not ownership uncertainty; the
log tail to end with `sglang_startup_failed: engine_launch_failed`; the launch's
process group to be empty and its memory released on that evidence; and no prometheus
`FileNotFoundError` in the engine log.

# Release note: Fixes

- **An SGLang launch whose scheduler dies while starting now fails within seconds.**
  It used to wait out the whole readiness deadline and then report that ownership of
  the engine was uncertain. CapyCTL now ends the engine's remaining processes, reports
  the launch as failed with the engine's own log lines, and releases its memory once
  they are gone. The repeated prometheus `FileNotFoundError` messages that followed
  such a failure no longer appear.

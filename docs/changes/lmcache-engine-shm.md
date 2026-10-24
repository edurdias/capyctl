# Status: LMCache launches leave no shared memory behind — 2026-10-24 (branch `fix/lmcache-engine-shm`)

Found live on host A (SGLang 0.5.22 and vLLM 0.32.0 with LMCache 0.5.6, `restart_only`):
a launch with LMCache left the engine's CUDA driver file
`/dev/shm/cuda.shm.<uid>.<pid>.1` (80 KiB, named for the SGLang scheduler's or vLLM engine
core's pid) after it stopped, and on vLLM also a `torch_<pid>_*` file (80 KiB, named for
the engine core). The LMCache server maps the engine's CUDA driver file through CUDA IPC,
so the SGLang entry's cleanup correctly leaves a file the running server still maps (a
`restart_only` park keeps the server), and the server's verified stop removed only the
server's own file. vLLM's engine core makes the torch file only with the connector on and
never removes it; no process holds it once the engine has exited. Without LMCache, vLLM
0.32.0 left neither file (checked on host A).

Every verified stop of an owned launch runs through
`DurableProcessLaunch::terminate_escalating` (the embedded host's and a remote host's, a
full stop and a restart-park's group stops alike). For a launch with LMCache (the stop names
its server, or is a group stop, which only LMCache launches make) it now also removes the
files its engines left (ADR 0033 A5 amended, SPEC §13.2):

- Once the stop has proven every recorded identity gone, each file in `/dev/shm` named for
  an engine process of that stop (the exact `cuda.shm.<uid hex>.<pid hex>.1` for this user,
  or `torch_<pid>_<n>_<n>`) is removed if it is still the same file (device and inode), a
  regular file (never followed through a symlink) owned by this user with no group or other
  access, and no live process this user can inspect holds it (its mappings and
  descriptors).
- After a restart-park's engine stop, a file the running server still holds is recorded
  with its holders instead. After the verified stop of a server that was one of them, the
  file is removed by the same rules, also if the server let go of it earlier (as it lets go
  of a parked engine's KV once a woken engine stores).
- A file named for no process of the stop, one another live process holds, one others may
  read and one replaced at the same name all stay; a launch without LMCache is stopped as
  before. The record lives in the role's memory: a role restarted between a restart-park
  and the server's stop leaves those files, as before.

The SGLang entry's own cleanup is unchanged.

## Tests

- `crates/capyctl-launchers/tests/lmcache_launch.rs`,
  `a_launch_stop_removes_the_files_its_engines_left` (T45 T12): one stop of both groups
  removes the CUDA driver and torch files named for the engine's processes, held by the
  server or by nothing, and keeps one a live process outside the launch holds, one with
  group and other read access, and one named for a pid outside the launch.
- `a_parked_engines_files_are_removed_once_its_server_stops` (T45 T12): the engine's stop
  alone removes its file nothing holds and keeps those the running server holds; the
  server's later stop removes them, including one only a server process that exited before
  the stop held, and keeps the file that replaced one at the same name.
- `a_stop_without_lmcache_leaves_the_engine_files` (T45): an engine-only launch's stop
  removes nothing.
- `launch_shm::tests::only_the_exact_driver_and_torch_names_carry_a_creator` (T45).
- The first two failed before the fix (every file stayed). The existing test that removes
  the server's own file now makes the engine's file there group-readable, so it still
  shows that only the server's file is removed.

CPU and Fake-engine tests are not qualification.

## Live check (host A, 2026-10-24)

Release build of this branch's code (on origin/main with #194), standalone role on
127.0.0.1:38443 with a fresh state directory, Qwen3-4B-Instruct-2507, `restart_only`,
`lmcache: required`, SGLang 0.5.22 and vLLM 0.32.0 with LMCache 0.5.6. `/dev/shm` held 70
entries before the run and 70 identical entries after it. Each launch made a
`cuda.shm.3e8.<pid>.1` named for its scheduler or engine core, held by that process and
the LMCache server, a `torch_<pid>_*` file held by that process alone, and SGLang's or
multiprocessing's own files.

| Case | After the park | Left after the stop |
|---|---|---|
| SGLang: chat, park, stop | the engine's `cuda.shm` only, held by the server | none |
| vLLM: chat, park, stop | the engine's `cuda.shm` only, held by the server (the torch file was gone) | none |
| SGLang: chat, stop | - | none |
| vLLM: chat, stop | - | none |
| vLLM: chat, park, wake by a chat, stop | the first engine's `cuda.shm`; after the wake both engines' | none |
| SGLang: chat, park, wake by a chat, stop | the first engine's `cuda.shm`; after the wake both engines' | none |

Every chat answered 200; after each stop no engine or server process ran, and the GPU was
empty at the end.

A first build without #194 showed that SGLang's `sglang_loads_*`, `sem.mp-*` and torch
files are the entry's to remove (only the engine's process held them), and a build that
removed only files the server held left vLLM's torch file in both vLLM cases, which led to
the rule above. Evidence is in `target/live/lmcache-engine-shm/` on the laptop; the host
was cleaned (the run directory and the 98 `/tmp` entries the run made removed).

# Release note: Fixes

- **LMCache launches no longer leave shared memory behind.** A deployment with LMCache left
  its engine's CUDA shared-memory file in `/dev/shm` (80 KiB, and on vLLM a `torch_*` file
  too) after it stopped, also after a `restart_only` park. CapyCTL now removes those files
  once the engine's processes have stopped, or, for a file the LMCache server still holds,
  once the server has stopped too, and nothing else holds them.

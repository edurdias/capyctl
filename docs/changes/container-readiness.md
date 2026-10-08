# Status: A role behaves correctly inside a container — 2026-10-08 (branch `feat/container-readiness`)

Owner decision 2026-10-08: make a host or standalone role correct as a container's main process now; the live container run on a GB10 comes later. Four changes. (1) Memory (SPEC §7.2, T26): `memory::read_host_memory` bounds `/proc/meminfo` by cgroup v2. The process's unified cgroup comes from `/proc/self/cgroup`; for it and every ancestor under `/sys/fs/cgroup` with a numeric `memory.max`, capacity is `min(MemTotal, memory.max)` and availability `min(MemAvailable, memory.max − (memory.current − inactive_file))`, the tightest level winning. Crediting `inactive_file` is a deliberate refinement of the requested `memory.max − memory.current`: without it a model's weights in the page cache read as used. The source travels as `HostMemorySample.source` and in the additive proto field `DomainObservation.memory_source` (`meminfo`, `cgroup_v2:<cgroup>`, `meminfo:cgroup_v1_unread`, `meminfo:cgroup_v2_unreadable`). The server keeps it in `DomainView` (bounded like other names, status only), standalone's host domains report it, and the role logs it at start when it is not plain meminfo. cgroup v1 is out of scope: never read, reported as such. Every consumer of the host reading follows the bound: the unified (GB10) domain, a discrete host's `distinct` system domain, admission refusal, standalone's derived limits and doctor. Engine `MAX_JOBS` sizing (`capyctl-adapters` `engine_env`) still reads the machine's `MemAvailable`; documented. (2) Reaping (SPEC §13.2, T12 T33): new `capyctl_launchers::subreaper`. At start a role sets `PR_SET_CHILD_SUBREAPER` (PID 1 reaps without it) and sweeps once a second, reaping every exited child it did not spawn itself. Its own direct children keep their waiters: a child in CapyCTL's process group, or one spawned through `subreaper::spawn_direct` (ExecLauncher, DurableSpawn, the installation probe and the engine version check; spawn and registration happen under a shared gate the sweep takes exclusively). `process_absence` reads an exited, single-threaded zombie as `Observed::Exited`. When the role is its parent it reaps it on sight, and the next reading proves it gone. Otherwise it is `Unknown` and a recorded set reports the new `GoneProof::Unreaped`, so it is no longer read as alive forever. Termination no longer refuses over such a process (it holds only its pid), and a zombie leader's group is scanned by pgid. PID-reuse protection is unchanged: start ticks are matched before every reap, and only the parent can reap a zombie, so its pid cannot change before the wait. Known limit: an orphan left in CapyCTL's own process group (a grandchild of a plain `Command`) is never reaped. (3) `nvidia-smi` (T26): `gpu_memory::nvidia_smi` tries `/usr/bin` and `/bin`, then each absolute `PATH` directory, and only executable files. Used by the GPU sample (and so the device inventory's corroboration) and by process residency, with the 3 s bound unchanged. (4) Docs: "Running in a container" in `docs/operations/install.md`. CPU tests only (cgroup fixtures, a re-executed subreaper fixture that reaps an inherited zombie and reports an unreapable one, PATH lookup, server and proto round trips); not run in a container or live. The live container run on a GB10 is pending.

# Release note: Containers

- **A role runs correctly as a container's main process.** See
  [Running in a container](../operations/install.md#running-in-a-container). CPU tests only;
  not yet run live in a container.
  - **Memory limits are honoured.** With cgroup v2, host memory is bounded by
    the tightest `memory.max` of the role's cgroup and its ancestors: the
    capacity by the limit, the available memory by the limit minus the
    cgroup's usage (its inactive page cache counted as free). A unified
    (GB10) machine's single domain follows the same bound. The role says at
    start which cgroup bounds it, and `inspect host --json` shows it as
    `memory_source`. cgroup v1 limits are not read, and the role warns.
  - **`--init` is not required.** A host or standalone role makes itself a
    child subreaper at start and reaps the processes its engines leave
    behind; as PID 1 it reaps every orphan. Before, a zombie read as a
    running process, so with no init a stop's verified cleanup never
    completed. An exited process whose parent never waits for it is now
    reported as such instead of read as running.
  - **`nvidia-smi` is also found on `PATH`**, after `/usr/bin` and `/bin`, so
    an image with the NVIDIA tools elsewhere observes its GPUs.

# CapyCTL 0.1.8

CapyCTL 0.1.8 fixes problems found in 0.1.7. Most importantly, a vLLM model that parks
deep no longer fails its 13th wake. In 0.1.7 this happened to every such model after
twelve parks. Models that restart when they wake (`restart_only`) now get as long to
wake as they get to start. LMCache now works through repeated parking and on hosts
enrolled to a server, and stopped launches no longer leave files in shared memory. On
one machine you can now raise the limit on LMCache's host memory.

## Parking and waking

- **A vLLM model that parks deep wakes every time.** In 0.1.7, after twelve parks and
  wakes in one launch, the next wake of a vLLM model failed with `wake_uncertain` and the
  model was stopped. The request after it then started the model from scratch. This
  happened to every vLLM model with the default deep parking. A model now parks and
  wakes as often as it is asked to for as long as it runs. SGLang had a limit of the
  same kind, much higher, and it is gone too.
- **A `restart_only` model gets as long to wake as to start.** A `restart_only` model,
  such as one with LMCache, starts its engine again when it wakes. In 0.1.7 that wake had
  only the shorter time a deep-park wake gets: 105 s for an 8 GB model. An SGLang model
  that took longer to restart was stopped with `wake_uncertain`. Unless the deployment
  sets `timeouts.wake`, the wake now gets as long as a start (`timeouts.initialize`, or
  the time CapyCTL works out for a start). Deployments created before this release get
  the longer window too
  ([Parking with LMCache](../guide/parking.md#parking-with-lmcache)).

## LMCache

- **A host enrolled to a server wakes a model parked with LMCache.** In 0.1.7, waking a
  `restart_only` deployment with LMCache on an enrolled host failed every time with "the
  host did not report the launch's engine alive with the wake canary", even though the
  engine had started. These wakes now complete, on vLLM and SGLang.
- **A model parked and woken again and again no longer runs out of memory.** Each time a
  `restart_only` model with LMCache parked, its LMCache server kept the stopped engine's
  cache in GPU memory. On SGLang it never released it, so after about ten parks and
  wakes the machine ran out of memory and the next wake failed. CapyCTL now always counts
  at least the memory the LMCache server is seen to use. If the server still holds an
  earlier engine's cache when the model is parked, CapyCTL stops the model and its server
  instead of parking it, and the next request starts it from scratch, which takes as long
  as a first start. See [parking](../guide/parking.md#parking-with-lmcache).
- **LMCache launches no longer leave shared memory behind.** In 0.1.7, a deployment with
  LMCache left its engine's CUDA shared-memory file in `/dev/shm` after it stopped,
  including after a `restart_only` park (80 KiB, and on vLLM a `torch_*` file as well).
  CapyCTL now removes those files once the engine's processes have stopped. A file the
  LMCache server still uses is removed once the server has stopped too. Files that
  another process still uses, or that other users can read, are left alone. One limit
  remains: if the role restarts between a park and the server's stop, those files stay,
  as they did before.

## Memory

- **On one machine, the host-KV limit can be set.** On standalone, the memory that all
  LMCache pools may use together (the host-KV limit) was fixed at a tenth of the
  machine's memory: 12.17 GiB on a 128 GB GB10, so no `lmcache_pool` above 12 GiB fitted.
  You can now raise it with
  `--set host.resource_policy.memory.system.host_kv_limit=24GiB`, with
  `CAPYCTL_SET__HOST__RESOURCE_POLICY__MEMORY__SYSTEM__HOST_KV_LIMIT=20%`, or in the
  configuration file. It takes a size or a whole percentage, as a host's
  `host_kv_limit` does, and `auto` keeps the tenth. A long prompt needs a larger pool to
  come back whole: see
  [Standalone host-KV limit](../operations/configuration.md#standalone-host-kv-limit) and
  [LMCache](../guide/engines.md#lmcache).
- **When a start is refused for lack of host-KV room, the error names the limit to
  raise.** The refusal is still `insufficient_resources` (exit 4). It now says `host_kv`,
  with what the model needs, what other models already use, the limit and the setting to
  raise, for example
  `host_kv (needs 20.0 GiB, limit 12.2 GiB; raise host.resource_policy.memory.system.host_kv_limit or lower lmcache_pool)`.
  With a server and hosts, it names that host's
  `resource_policy.domains.<domain>.host_kv_limit`. See
  [errors](../guide/errors.md#lmcache).

## SGLang

- **A stopped SGLang model no longer leaves shared memory behind.** In 0.1.7, every
  SGLang start left a `/dev/shm/sglang_loads_*.shm` file and a semaphore behind after the
  model stopped. The launch now removes the shared memory it made when it stops, as it
  already did when a start failed, once its processes are gone and no other process uses
  that memory. One limit remains: if a stop takes longer than 15 s and CapyCTL has to
  kill the engine, the files stay.

## Release page

- **Links on the release page work.** The release notes link to the guides by paths
  inside the repository, which broke when the notes were shown on the release page. From
  this release on, the release page's links point at the guides as they were at the
  release.

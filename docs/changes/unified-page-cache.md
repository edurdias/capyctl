# Status: Unified memory: a launch drops its checkpoint's page cache before the engine starts (#226) — 2026-10-30 (branch `fix/unified-page-cache`)

Issue #226, third direction. On a GB10 an SGLang 0.5.22 engine started beside a
running one failed creating its CUDA context (`CUDA_ERROR_OUT_OF_MEMORY` from
`cuDevicePrimaryCtxRetain`, `NV_ERR_NO_MEMORY` from `kgrctxAllocMainCtxBuffer`)
while `MemAvailable` read tens of GiB: most of it was page cache, which the
driver does not wait to reclaim for a context's physically contiguous buffers.
Part of that cache, all of it on a first placement, is the checkpoint CapyCTL
itself reads to verify its digest (ADR 0014 §7). NVIDIA's DGX Spark known
issues give flushing the buffer cache as the workaround on unified memory.

What changed (SPEC §7.2; ADR 0019 for what a discrete GPU is):

- New `capyctl_agent::page_cache`: when a launch's starting footprint (its
  `cold` allocations) lands on a `memory: unified` domain, every regular file
  of its checkpoint gets `posix_fadvise(POSIX_FADV_DONTNEED)`. Unprivileged;
  nothing else on the machine is touched (no `/proc/sys/vm/drop_caches`), and
  pages another engine has mapped stay. The files come from
  `checkpoint::drop_page_cache`, the same confined walk the digest uses
  (inside the model store or the checkpoint's own root, links resolved only
  inside it and opened with `O_NOFOLLOW`, the same file, directory and depth
  bounds); a checkpoint that cannot be walked is not advised at all.
- Best effort: a failed file, or a checkpoint that cannot be walked, is logged
  once per launch as the event `checkpoint_page_cache_kept` (deployment id,
  counts, first errno or the closed reason; no path) and the launch goes on.
- Enrolled host: in `launch_native`, after admission's verification and right
  before the adapter's Initialize spawns the engine, and in the restart-park
  wake (ADR 0033 §7, a new engine on the running LMCache server) right after
  its checkpoint check. The recheck at the spawn, under the journal's lock,
  reads at most the small files again, so the drop runs outside that lock.
- Standalone: the embedded `CheckpointGate` does the same after it verifies
  the checkpoint, before Initialize, and before a Restore of a deployment that
  does not park in place (`restart_only`, whose wake starts a new engine). A
  parked engine's wake keeps its context and is left alone.
- Discrete hosts (`device` and `distinct` domains) are unchanged.

No admission or accounting rule changes: memory is still admitted on
`MemAvailable` as before; only the cache of the launch's own checkpoint is
given back.

Tests (failing before, passing after):

- `crates/capyctl-agent/tests/native_vllm.rs`,
  `a_unified_launch_drops_its_checkpoint_page_cache_before_the_engine_starts`
  (T26): a full remote launch on the fake vLLM with an injected advice seam;
  every regular file of the checkpoint (a nested file, a link inside the store
  as the file it names, tool metadata left out) is advised once, before the
  engine wrote its record. Before the fix the advice was never called
  (`left: []`).
- `a_failed_page_cache_drop_does_not_fail_the_launch` (T26): every advice
  fails with EIO and the model is still proven usable. Before the fix: no file
  advised (`left: 0, right: 3`).
- `a_discrete_launch_leaves_the_page_cache_alone` (T26): `memory: distinct`,
  nothing advised (passes before and after; guards the gate).
- `crates/capyctl-controller/src/checkpoint_digests/tests.rs`,
  `the_embedded_gate_drops_the_checkpoint_page_cache_before_an_engine_starts`
  (T26): the standalone gate advises every file before Initialize, again
  before a `restart_only` Restore, not before a `deep` Restore, and a failing
  advice still reaches the engine. Before the fix: `left: []`.
  `the_embedded_gate_leaves_the_page_cache_alone_on_distinct_memory` (T26).
- `crates/capyctl-agent/src/checkpoint/tests.rs`:
  `the_page_cache_drop_advises_every_file_of_the_checkpoint` (with the real
  `posix_fadvise`, and failures counted with their errno) and
  `the_page_cache_drop_never_leaves_the_store` (a link out of the store is
  refused whole, nothing advised).

CPU and Fake-engine tests are not qualification: nothing here shows that a
GB10 engine's CUDA context creation now succeeds.

Live check still needed, on a GB10 (unified memory): with one SGLang engine
serving and the page cache filled by reading model files (`MemFree` near the
low watermark), start a second SGLang deployment and confirm it reaches Ready
without `CUDA error: out of memory`, that the host log has no
`checkpoint_page_cache_kept` line, and that `Cached` in `/proc/meminfo` drops
by about the checkpoint's size just before the engine starts. The failure was
intermittent, so repeat the start several times.

# Release note: Fixes

- **Engines start more reliably beside others on unified memory.** On a
  unified-memory machine such as a GB10, CapyCTL now drops the page cache of a
  model's checkpoint files right before its engine starts, without
  privileges and without touching the rest of the cache. An engine started
  beside a running one could fail creating its CUDA context with
  `CUDA error: out of memory` while most of the memory was page cache, much
  of it the checkpoint CapyCTL had just read to verify it. If the drop fails,
  the host log says so once and the engine starts anyway. Discrete GPUs are
  unchanged. See [installation](../operations/install.md#discrete-nvidia-gpus).

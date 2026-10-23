# Status: An enrolled host wakes a model parked with LMCache — 2026-10-23 (branch `fix/remote-lmcache-wake`)

Found live 2026-10-23 (the LMCache benchmark): with the server on the control host and
host A as its host, parking a `restart_only` vLLM 0.32.0 deployment with LMCache and
waking it failed 3 of 3, each after about 73 s, with "the wake canary was not answered:
… the host did not report the launch's engine alive with the wake canary". Standalone
restart-park wakes passed. Reproduced on v0.1.7 (release binaries, laptop server, host A,
`~/vllm-0.32.0-lmcache-venv`, Qwen3-4B): the same message after 77.5 s.

Cause ([ADR 0033](../design/adr/0033-lmcache.md) A14): a remote host's answer to the
wake canary counts only with the engine the canary's context names alive (ADR 0028
note of 2026-10-16). The coordinator ran the canary on the restore's own context, whose
processes are what the launch recorded before the wake. A restart-park records the
LMCache server alone, so that context named no engine and the canary failed after the
host had started the new engine and proven the restore. The embedded adapters never read
those processes, which is why standalone passed. SGLang restart-parks on an enrolled
host take the same path.

Fix: the canary runs on the restore's context with the processes the restore's evidence
names (`woken_context` in the coordinator worker): the new engine beside the same server
for a restart-park, the recorded engine for any other wake. The remote check is unchanged
and now lives in `remote_execution::canary_engine_alive`.

Tests (T45 T20 T34): the scripted remote restart-park in `coordinator/tests_lmcache.rs`
now answers the wake canary as a host does (its processes alive now, judged by
`canary_engine_alive`); without the fix the wake does not reach Ready, with it the
readiness and wake canaries both count. A unit test covers `canary_engine_alive` on a
restart-park's restore: the new engine counts, the server alone, another engine, a
recorded engine process gone, or the claim not retained do not. CPU and Fake-engine tests
are not qualification.

On the same v0.1.7 setup the SGLang 0.5.22 wake also ran out of its derived wake window
(105 s for Qwen3-4B) at 102.5 s, "remote residency change is unproven", before the
canary was reached: a restart-park's wake restarts the engine. That window is changed
separately (branch `fix/restart-park-wake-window`); the SGLang live check below declares
`timeouts.wake`.

Live check (2026-10-23, host A, server on the control host at 17443/18443, fresh state
on both, `fe0b16ad` built on each machine): Qwen3-4B, `restart_only`, `lmcache:
required`, 20 GiB pool, 5 GiB KV cache, 32k context, `timeouts.initialize` 1200 s, each
deployment parked and woken by a chat request 3 times, all at generation 1 on the same
launch, every wake answering 200 and Ready with no last error:

- vLLM 0.32.0 (`~/vllm-0.32.0-lmcache-venv`, the deployment that failed): wakes of
  80.2, 77.4 and 78.4 s (restore accepted to completed 79.5, 76.7, 77.7 s).
- SGLang 0.5.22 (`~/sglang-0.5.22-lmcache-venv`), with `timeouts.wake: 1200s`: wakes of
  107.6, 106.1 and 109.0 s (105.5-108.3 s restores), past the 105 s derived window.

Both readiness canary references were recorded and every wake was compared with them.
Host A was left with no CapyCTL or engine process, listener or GPU process, and with
`/tmp` and `/dev/shm` back at their starting counts.

# Release note: Fixes

- **An enrolled host wakes a model parked with LMCache.** Waking a `restart_only`
  deployment with LMCache on a host enrolled to a server failed every time with "the
  host did not report the launch's engine alive with the wake canary", though the
  engine had started. Such wakes now complete, on vLLM and SGLang.

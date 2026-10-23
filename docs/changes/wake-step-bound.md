# Status: a vLLM model's 13th deep-park wake no longer fails — 2026-10-23 (branch `fix/wake-flakes`)

Found in the 0.1.7 LMCache benchmark on host A (standalone, vLLM 0.32.0, deep park,
LMCache off): one wake in 16 failed `wake_uncertain` ("the restore stopped after its
first effect: runtime operation is unsupported"), and CapyCTL stopped the engine right
after vLLM had answered `/wake_up` and `/reset_prefix_cache` with 200. It was not
intermittent: it was the launch's 13th park and wake. SPEC §13.2's step fence in the vLLM
adapter admitted each residency step id once, but also refused every step once the launch
had run 64 of them over its life. A deep park and its wake are 5 steps (park, weight
wake, reload, KV wake, probe), so after 12 cycles the 13th wake's probe (step 65) was
refused before any engine call (`Unsupported`), which the coordinator reads as uncertain
after the restore's first effect (T20). SGLang's fence had the same lifetime bound at
4,096 steps.

Both fences now remember the most recent 64 step ids (`capyctl-adapters::step_fence`): a
repeat of a recent step is still refused without an engine call, one step runs at a time,
and nothing runs after an uncertain step, but a launch parks and wakes as often as it is
asked to. Step ids are the store's, unique per step, and the store's step state stays the
durable guard.

Tests (T20 T16): `a_launch_parks_and_wakes_as_often_as_it_is_asked` (25 park and wake
cycles on one vLLM adapter, each probe answered by the engine, a recent step's repeat still
refused; run against the old fence it fails at the 13th cycle's probe) and the guard's unit
test. CPU and Fake-engine tests are not qualification. Live check (host A, 2026-10-23,
standalone built from this branch with `fix/restart-park-wake-window` on `0b6f6fb6`): vLLM
0.32.0, Qwen3-4B, deep park, LMCache off: 30 park and wake cycles on one launch (150
residency steps), each woken by a router request, all served (wake 7.2–8.7 s, median
8.1 s), one binding throughout, no uncertain wake.

# Release note: Fixes

- **A vLLM model that parks deep wakes every time.** After twelve parks and wakes of one
  run, the next wake of a vLLM model failed (`wake_uncertain`) and the model was stopped;
  the request after it started the model fresh. It now parks and wakes for as long as it
  runs.

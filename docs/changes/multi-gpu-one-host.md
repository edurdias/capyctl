# Status: One model across several GPUs of one host (ADR 0035), implemented; live rows pending — 2026-10-25 (branch `feat/multi-gpu-one-host`)

ADR 0035 (owner decisions 2026-10-25) is implemented for 0.1.9. A deployment with
`topology: {tensor_parallel: N}` and no `placement.hosts` runs as one instance on N
identical discrete GPUs of one host, on an enrolled host and on standalone; `devices` may
pin exactly N GPUs, and `topology: auto` lets CapyCTL choose N.

- **Configuration** (§1, §1a): a world size above 1 without two or more named hosts is one
  host, not a group; `pipeline_parallel` above 1 there, a unified host, TensorFold, custom
  engines and decision deployments are refused with their codes, and so are pinned GPUs that
  differ (`multi_gpu_mixed_devices`), a pinned count other than N
  (`topology_devices_mismatch`) and, on vLLM and SGLang, an N that does not divide the
  model's attention heads (`tensor_parallel_indivisible`). `auto` records a candidate table
  in the revision (each N's per-GPU and host-RAM charge), is refused beside named hosts,
  `devices`, `resources`, `memory.request` and `memory.startup`, and narrows to N = 1 under a
  stated parking tier or `lmcache: required`.
- **Accounting** (§3): one owner, one allocation per chosen GPU's `device` domain (ADR 0028's
  rank share with `T = N`, KV cache, margin, 1.25 GiB CUDA context, 1 GiB communication
  allowance, both `derived` until measured) and the system domain (4 GiB plus 3 GiB per rank
  after the first). Every refusal for memory or GPU count names the `tensor_parallel` that
  would fit.
- **Placement** (§2, §1a): sets of N identical GPUs, the largest smallest headroom first;
  eviction by the smallest union of victims, a multi-GPU victim whole; the chosen set and N
  recorded with the instance (new forward-only schema version) and kept through parks and
  wakes; `auto` chooses N from the cards' managed limits, never their free memory, and
  journals `topology_auto_changed` when a later start chooses another N. Measured peaks and
  parked residue are kept per N.
- **Readings and protocol** (§8, §11): `nvidia-smi --query-compute-apps=pid,gpu_uuid,used_memory`,
  one reading per (process, GPU), each GPU's domain credited with the bytes on that GPU; an
  unknown UUID refuses the sample. `ProcessResidency.per_device`, `SingleLaunchPlan.devices`
  and the host capability `local_tensor_parallel` (an older host is refused
  `host_capability_missing:local_tensor_parallel` before anything is sent).
- **Launch** (§4): vLLM `--tensor-parallel-size N --distributed-executor-backend mp`, SGLang
  `tp_size` N with `base_gpu_id` 0 and `gpu_id_step` 1, llama.cpp `--split-mode layer`,
  `--tensor-split` and `--main-gpu 0` with `NCCL_SOCKET_IFNAME=lo`, each on the GPUs' UUIDs in
  rank order; the frozen plans pin N and a drift refuses the launch (`tp_drift:<field>`); the
  single-host loopback pin applies unchanged.
- **Ownership and failure** (§5–§7): rank processes are named at readiness from per-GPU
  readings (`rank-N`; `tp_rank_unobserved` at the initialize timeout) and watched as engine
  processes; one rank's exit fails and stops the instance (`engine_rank_exited`) with nothing
  released before every process is gone; ADR 0028's stall rule covers every N > 1 instance
  (`tp_stalled`); ADR 0031's listener check runs after readiness and at every re-proof
  (`engine_listener_exposed`). Status shows `collectives: loopback` and `TP auto (N)`.
- **Parking** (§9): vLLM and SGLang deep parking at N > 1 is built on per-GPU evidence and
  ships off in the support data (owner decision 3): until MG4 and MG5 pass, a multi-GPU
  deployment resolves `restart_only` and a stated `deep` or `host_backed` is refused
  `capability_missing:deep_park`. LMCache is off for N > 1 (`multi_gpu_unqualified`).
- **Docs**: SPEC §7.2, §8.2, §9.1, §9.2, §9.4, §11, §15.1 and §20 (T46, MG1–MG14); ADR 0019,
  0027, 0028 and 0029 amendments; the deploy, parking, errors, engine flags and
  several-machines guides; the install and settings references. AGENTS.md records the rented
  machine's one session and its engine environments.
- **Live script set**: `scripts/live/multi-gpu/run.sh` runs MG1–MG14 and a download bandwidth
  test on the rented two-GPU machine in one session, installing only vLLM 0.32.0, SGLang
  0.5.22 and the llama.cpp v0.7.0 build, and removes everything it created
  ([its README](../../scripts/live/multi-gpu/README.md)).

**Not yet on this branch (draft):** per-GPU park and wake evidence (§9): the vLLM judgment
per GPU, SGLang's per-rank saver read on the host and the controller's per-device judgment,
and the parked residue credited per GPU. Until they land, `deep_park` must stay off at
N > 1 and MG4–MG6, MG10 and MG13's park must not run. The remaining placement tests are on
`feat/mg-tests`. This file is corrected to match when they merge.

Tests (T46), new in `capyctl-config` (`tests/multi_gpu.rs`), `capyctl-store`
(`tests/multi_gpu_placement.rs`, the v58 migration), `capyctl-scheduler`, `capyctl-domain`
(`local_tp`), `capyctl-agent`, `capyctl-controller`, `capyctl-router` and `capyctl-protocol`.
Fast gate (`scripts/ci-local.sh`) on this branch: every step passes, 1679 unit tests, 0
failed. The deep gate is still owed before merge. CPU and Fake-engine tests are not qualification: nothing here has
run on two GPUs. The live check still needed is the rented session (ADR 0035 §14): MG1–MG14
and the bandwidth test; MG4 and MG5 decide whether vLLM's and SGLang's `deep_park` is turned
on at N > 1, and MG1–MG3, MG6–MG9 and MG13 must pass before the release notes call the
feature qualified.

# Release note: Several GPUs

- **One model across several GPUs of one machine.** A deployment with
  `topology: {tensor_parallel: 2}` (or `topology: auto`, which picks the smallest number of
  GPUs whose share of the model fits each card) runs on that many identical GPUs of one
  machine, with vLLM, SGLang or llama.cpp: one model, one route, each card charged its share,
  and the engine's traffic between the cards kept on loopback (`collectives: loopback` in
  status). `devices` can pin the cards. Refusals for memory or GPU count name the
  `tensor_parallel` that would fit. For now such a model stops instead of parking, and it
  runs without LMCache; this is not yet qualified on two-GPU hardware
  ([One model on several GPUs](../guide/deploy.md#one-model-on-several-gpus)).

# Status: ADR 0035 accepted: one model across several GPUs on one host — 2026-10-25 (branch `docs/adr-multi-gpu`)

ADR 0035 records the owner's decisions of 2026-10-25 for running one model on several
identical discrete GPUs of one host in 0.1.9. It was proposed on 2026-10-24, and the
owner answered its ten open questions. A deployment asks with ADR 0028's
`topology: {tensor_parallel: N}` and names no hosts. `devices` may pin exactly N GPUs.
Different GPUs, pipeline parallel on one host, and an N that does not divide the model's
attention heads (vLLM, SGLang) are refused.

`topology: auto` (owner decision 10, changed from the recommendation) picks the smallest N
that fits on one host with identical GPUs. N is chosen at placement from the cards'
managed limits, never from their free memory, and frozen for the launch's life. The charge
is that N's row of the candidate table the revision records, and the engine command is the
explicit N's. Status shows `TP auto (N)`, and a later start that picks another N journals
`topology_auto_changed`. Every refusal for memory or GPU count names what would fit.

The instance stays one owner. Placement picks a set of N GPUs, and each GPU's `device`
domain is charged one rank (ADR 0028's per-rank share, KV cache, margin, CUDA and
communication allowances). Host RAM is charged an overhead per rank. Measured peaks and
parked residue are kept per GPU and per N.

vLLM renders `--tensor-parallel-size N --distributed-executor-backend mp`, SGLang
`tp_size=N`, and llama.cpp `--split-mode layer --tensor-split` (`layer` only), each on a
UUID device list. The single-rank loopback pin applies unchanged. ADR 0031's listener check
runs on every launch with N > 1 (`engine_listener_exposed`). Unauthenticated loopback
collective listeners are accepted and documented, and status shows `collectives: loopback`.

Per-process GPU readings become per (process, GPU). Rank processes are named from those
readings and watched as engine processes (`rank-N`, amending ADR 0027). One rank's exit
fails and stops the instance, and nothing is released before gone evidence. ADR 0028's
stall rule covers every instance with N > 1 (`tp_stalled`).

vLLM and SGLang deep parking at N > 1 is built now on per-GPU evidence. Per engine it
stays off, with `restart_only` the default, until its row (MG4 or MG5) passes. LMCache is
off for N > 1 (ADR 0034, `multi_gpu_unqualified`). The new host capability is
`local_tensor_parallel`; the ADR also adds codes, conformance row T46 and live rows
MG1–MG14.

Hardware (owner decision 2): when the implementation is ready, the owner rents one x86_64
machine with two RTX A6000 48 GB PCIe cards. All MG rows and a download-bandwidth test run
in one session, with fresh installs of vLLM 0.32.0, SGLang 0.5.22 and llama.cpp v0.7.0
only. The machine is authorized as a live host for that session only and shut down
straight afterwards.

Docs only: nothing is implemented, and no code or tests change. CPU and Fake-engine tests
are not qualification; the MG rows in the rental session are the live check still needed.

# Release note: none

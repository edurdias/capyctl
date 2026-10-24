# Status: ADR 0035 proposed: one model across several GPUs on one host — 2026-10-24 (branch `docs/adr-multi-gpu`)

ADR 0035 (Proposed, for the owner to decide, target 0.1.9) designs running one model on
several identical discrete GPUs of one host. A deployment asks with ADR 0028's existing
`topology: {tensor_parallel: N}` and names no hosts; `devices` may pin exactly N GPUs. It
stays one instance and one owner. Placement picks a set of N GPUs, and admission charges
each GPU's `device` domain one rank: its share of the weights by ADR 0028's per-rank
formula, plus its KV cache, margin, CUDA allowance and a communication allowance. Host RAM
is charged an overhead per rank. vLLM renders `--tensor-parallel-size N
--distributed-executor-backend mp`, SGLang `tp_size=N`, and llama.cpp
`--split-mode layer --tensor-split` across a UUID device list. The single-rank loopback pin
(gloo and NCCL on `lo`, file rendezvous) applies unchanged, and ADR 0031's listener check
runs on every such launch (`engine_listener_exposed`). The engines' source shows no
off-loopback listener in these shapes: vLLM 0.30–0.32's file store and IPC queues, and
SGLang 0.5.22's IPC channels without DP attention (which stays refused). Per-process GPU
readings become per (process, GPU) (`gpu_uuid`). Rank processes are named from the
driver's per-GPU readings and watched as engine processes (`rank-N`, amending ADR 0027).
One rank's exit fails and stops the whole instance, releasing nothing before gone
evidence. ADR 0028's stall rule covers a hung rank (`tp_stalled`). vLLM and SGLang deep
parking at N > 1 uses per-GPU evidence (SGLang: one saver map per rank) and stays off,
`restart_only` by default, until live rows MG4/MG5 pass. llama.cpp, TensorFold and custom
engines stay one GPU or `restart_only`. LMCache is off for N > 1 (ADR 0034,
`multi_gpu_unqualified`). Decision and embeddings deployments keep their one-GPU rules.
There is a new host capability, `local_tensor_parallel`, plus codes, conformance row T46
and live rows MG1–MG12. No project hardware has two GPUs (the GB10 hosts have one each;
the laptop has one card), so the ADR proposes renting one machine with two identical PCIe
GPUs (48 GB each preferred, 24 GB minimum), subject to owner authorization, cost and
engine-environment approval. Ten open questions, each with a recommendation. Docs only:
no code or tests change. CPU and Fake-engine tests are not qualification; the MG rows on a
two-GPU host are the live check still needed.

# Release note: none

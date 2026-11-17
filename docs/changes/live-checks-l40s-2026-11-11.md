# Status: Live checks on a rented L40S: #229, #232, #233, and 0.2.0's vLLM floor and SGLang graph phases — 2026-11-11 (branch `docs/live-checks-20261111`)

Owner decision 2026-11-11: with the lab hosts busy with training, rent one
small machine for one session. Shadeform, massedcompute, one NVIDIA L40S
(46068 MiB, compute capability 8.9), 12 CPUs, 70 GiB RAM, Ubuntu 22.04, driver
595.99.02 (CUDA 13.2). It ran under `scripts/live/shade_session.py` as a user
service with a provider auto-delete at $8 or 6 hours; the supervisor confirmed
deletion at 2026-11-11T20:15:15Z, provider-estimated cost $0.63. Fresh installs
in the home directory, removed with the machine: vLLM 0.32.0 and vLLM 0.33.0
(`--torch-backend=cu130`; 0.33.0 approved by the owner for this session),
SGLang 0.5.22, and NVIDIA's CUDA 13.0 toolkit wheels as the profiles'
`cuda_home` (FlashInfer's JIT needs `nvcc`; without it the first vLLM start
failed `Could not find nvcc`). Binaries: the published 0.1.10 and 0.1.12
x86_64 tarballs, and `integration/0.2.0-on-0.1.11` at `370db8dc` built there.
Models: `Qwen/Qwen3-14B`, `Qwen/Qwen3-4B-Instruct-2507`,
`Qwen/Qwen2.5-1.5B-Instruct`. Every role was a standalone with its own state;
the managed GPU limit was 41.4 GiB. Each answer checked was `42` to "What is
17+25?".

0.2.0, vLLM derived request without the 75% floor (#225, PR #248), CUDA
graphs on (captured, per the engine log):

| Binary, vLLM | Deployments (derived requests) | Rendered `--gpu-memory-utilization` | Result |
|---|---|---|---|
| 0.2.0, 0.32.0 | `qwen3-14b` alone | 0.7615 | Ready, startup peak 33.7 GiB measured |
| 0.2.0, 0.32.0 | 4B, then 1.5B beside it | 0.2721, 0.1592 | both Ready and answering, 21.7 GiB in use |
| 0.2.0, 0.33.0 | 4B, then 1.5B beside it | 0.2721, 0.1592 | both Ready and answering, 20.5 GiB in use |
| 0.1.12, 0.32.0 | 4B, then 1.5B beside it | 0.75 | 4B Ready charged 35.0 GiB; 1.5B refused `capacity_blocked` (6.4 GiB free of 41.4) |

0.2.0, SGLang graph phases (#236, PR #248), SGLang 0.5.22, 4B: with
`cuda_graphs: {prefill: false, decode: true}` the engine logged "Disable
prefill CUDA graph because cuda_graph_config resolved prefill.backend='disabled'",
captured decode graphs for batch sizes 1 to 32, and ran prefill batches with
`cuda graph: False`; Ready with a measured startup of 13.5 GiB. Without the
setting it captured prefill graphs (7.6 s, 0.80 GB) and decode graphs, ran
prefill with `cuda graph: True`, and measured 14.4 GiB.

#233 (0.1.11), vLLM 0.32.0, declared requests 14 GiB (4B) and 8 GiB (1.5B)
side by side: 0.1.12 rendered 0.3112 and 0.1778, both Ready and answering.
0.1.10 rendered 0.75 for the 4B, and the 1.5B failed to start: "Free memory
on device cuda:0 (29.87/44.39 GiB) on startup is less than desired GPU memory
utilization (0.75, 33.3 GiB)", the failure #225 described.

#229 (0.1.11), vLLM 0.32.0, `Qwen2.5-1.5B`, six greedy prompts with
`logprobs` and five `top_logprobs`, prefix caching off on both engines (with
it on, a repeated prompt changed vLLM's own digits): every log probability,
as spelled, in order, compared with vLLM serving the model directly. Through
0.1.12 the router's stream matched vLLM's stream in 6 of 6 answers, and so
did its collected (non-streamed) answer; through 0.1.10, 0 of 6 for both. A
collected answer spells numbers as vLLM's stream does (`-0.0000295634672511369`,
`-2.0265558760002023e-6`); vLLM's own non-streamed answer spells the same
values `-2.95634672511369e-05`, because vLLM serializes the two differently.
The values are equal.

#232 (0.1.11), SGLang 0.5.22, `qwen3-14b` with a declared 39.5 GiB request:
0.1.12 charged the first start 40.8 GiB (request plus the CUDA context),
started it and measured a 40.0 GiB card peak, within the 41.4 GiB limit.
#219's refusal did not reproduce here as a before case: 0.1.10 charged the same
40.8 GiB and started it, from a local checkpoint and from a Hugging Face source
not yet downloaded.

Observed: on Ubuntu 22.04 (glibc 2.35) the published 0.1.10 and 0.1.12
binaries print "weak version `GLIBC_2.39' not found" on every invocation and
then run normally.

CPU and Fake-engine tests are not qualification. Still owed: 0.2.0's
embeddings rows (EM1 to EM9), #236 with the issue's own hybrid model on a
16 GB card, #219 with the issue's own deployment, #230 with the official
image, and #228 under ingress overload.

# Release note: none

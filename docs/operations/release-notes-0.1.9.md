# CapyCTL 0.1.9

CapyCTL 0.1.9 runs one model across several identical GPUs on one machine.
Choose the GPU count or let CapyCTL select the smallest count that fits.
Each card's memory is accounted for separately; vLLM and SGLang can park
and wake the model with every card checked.

## Several GPUs

- **One model across several GPUs of one machine.** A deployment with
  `topology: {tensor_parallel: 2}` (or `topology: auto`, which picks the smallest number of
  GPUs whose share of the model fits each card) runs on that many identical GPUs of one
  machine, with vLLM, SGLang or llama.cpp: one model, one route, each card charged its share,
  and the engine's traffic between the cards kept on loopback (`collectives: loopback` in
  status). `devices` can pin the cards. Refusals for memory or GPU count name the
  `tensor_parallel` that would fit. Qualified on two RTX A6000 cards with vLLM 0.32.0,
  SGLang 0.5.22 and llama.cpp v0.7.0. vLLM and SGLang support deep and host-backed
  parking with every GPU checked; llama.cpp stops instead. LMCache remains off
  ([One model on several GPUs](../guide/deploy.md#one-model-on-several-gpus)).

## Qualification and limits

The complete MG1–MG14 matrix and download-bandwidth check passed on two
RTX A6000 48 GiB PCIe cards, with vLLM 0.32.0, SGLang 0.5.22 and
llama.cpp v0.7.0. This qualifies those TP2 combinations, not every model,
GPU count or hardware platform. CPU and Fake-engine tests are not native
engine qualification.

vLLM and SGLang each passed five deep-park/wake cycles and three
host-backed cycles. Deep-park memory per card fell from 34,238 to 740 MiB
on vLLM and from 34,320 to 2,710 MiB on SGLang; every wake passed its canary.
The [measured comparison](https://github.com/edurdias/capyctl-recipes/tree/main/comparisons/native-tp2-direct-vs-capyctl-qwen3-30b-a3b-a6000)
includes direct-engine versus CapyCTL throughput, parking timings,
hardware and measurement conditions. Its single-run throughput differences
are observations, not evidence of a general speedup.

Multi-GPU deployments still require identical discrete NVIDIA GPUs.
llama.cpp restarts instead of parking, and LMCache remains off for models
spread across several GPUs. The engines' unauthenticated inter-rank
listeners stay on loopback; that does not make engine development controls
production-hardened.

## Upgrading

Upgrade the server and hosts together before using single-host multi-GPU
deployments. A host without the `local_tensor_parallel` capability is
refused before launch. Back up existing state before upgrading; database
migrations are forward-only
([Upgrade](../operations/install.md#upgrade)).

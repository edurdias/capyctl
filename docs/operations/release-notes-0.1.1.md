# CapyCTL 0.1.1

## TensorFold

CapyCTL now runs TensorFold 0.6.0 and 0.6.1 beside vLLM and SGLang. Register an existing
TensorFold venv with `capyctl engine add <venv>`, or name it as the role's own
engine with `--tensorfold-bin` (`CAPYCTL_TENSORFOLD_BIN`, `local_engine.tensorfold`); CapyCTL checks that the CUDA
build tools TensorFold needs on its first start are on the engine's PATH. With
0.6.1 the CUDA compiler can come from pip (`pip install ninja
"cuda-toolkit[nvcc,cccl]==13.0.*"`) instead of a system toolkit.
TensorFold models do not park: CapyCTL stops them when it needs the memory and
starts them again on the next request. A TensorFold deployment states its
`resources` and `context_length`. Checked on NVIDIA GB10; discrete GPUs run it
unchecked in this release.

## Other changes

- A first start of vLLM or SGLang that builds kernels (an empty FlashInfer
  or SGLang kernel cache) no longer records the build's memory as the
  deployment's startup peak, which left every later start of that
  deployment `capacity_blocked` on a unified-memory host. A deployment that
  recorded such a peak before this release keeps it: delete it and deploy it
  again once.
- An engine whose own processes start helper processes, such as the torch
  inductor compile workers SGLang starts with CUDA graphs on, is no longer
  stopped when those helpers exit on their own a few minutes after it is
  ready. Only the engine's own processes exiting stops it; a stop still ends
  the helpers.
- A readiness check now accepts a model that answers with reasoning first.
- On SGLang, `memory.kv_cache` is now the size of the KV cache, as on vLLM. A
  hybrid model's per-request state is sized beside it, for
  `max_concurrent_requests` (or as many as fit, up to 32). A stated
  `memory.request` that cannot hold it is refused with the request it needs;
  without one, CapyCTL runs as many requests as fit and status shows the
  limit. Before, the state took part of the KV cache and could limit a hybrid
  model to 2 running requests.
- New exit code 26, `toolchain_missing`, from `capyctl engine add`.
- A client that hangs up in the middle of a streamed answer now stops the
  engine's work on it, for vLLM, SGLang and TensorFold. CapyCTL keeps the
  request counted until the engine reports nothing running or waiting, so a
  park or a switch that follows no longer waits for the whole answer. A
  client that reads nothing for 10 seconds, and a single piece of an answer
  larger than 64 KiB, now end the answer the same way; before, the engine ran
  the answer to its end.
- A model named by its Hugging Face cache directory (`snapshots/<rev>`) is
  measured even when its files link twice, as recent `huggingface_hub`
  versions store them. A checkpoint that cannot be measured now says why,
  instead of reporting a digest mismatch.
- `capyctl validate config` without `--host` checks a `resources` block and
  refuses a TensorFold deployment without one, and lists what still needs a
  host.
- A role with no engine starts and says how to add one; `capyctl engine
  remove` can remove the last engine.
- `capyctl status deployment` shows the engine a deployment actually runs, with a note
  when that engine is no longer registered.
- Idle models are stopped or parked only when `ready_idle_timeout` or
  `parked_idle_timeout` is set; both are off by default.
- SGLang keeps its CUDA graphs on while a model can park (about 75 % faster
  decoding on a 16 GB card). A parked model is charged the GPU memory CapyCTL
  measured it holding after its first park, instead of a fixed 1 GiB, shown as
  `parked` in `capyctl status deployment <name> --json`. Set
  `engine_config: {cuda_graphs: false}` to turn the graphs off; a deployment
  revision created before keeps them off.
- CI is a fast gate; the integration suites run with `scripts/ci-local.sh --deep`
  before a merge, and tests no longer depend on runner speed.

## Upgrade

Replace the binary and restart the roles; 0.1.0 state is reused.

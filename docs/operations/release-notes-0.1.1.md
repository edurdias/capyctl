# CapyCTL 0.1.1

## TensorFold

CapyCTL now runs TensorFold 0.6.0 beside vLLM and SGLang. Register an existing
TensorFold venv with `capyctl engine add <venv>`, or name it as the role's own
engine with `--tensorfold-bin` (`CAPYCTL_TENSORFOLD_BIN`, `local_engine.tensorfold`); CapyCTL checks that the CUDA
build tools TensorFold needs on its first start are on the engine's PATH.
TensorFold models do not park: CapyCTL stops them when it needs the memory and
starts them again on the next request. A TensorFold deployment states its
`resources` and `context_length`. Checked on NVIDIA GB10; discrete GPUs run it
unchecked in this release.

## Other changes

- A readiness check now accepts a model that answers with reasoning first.
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
- Idle models are stopped or parked only when `ready_idle_timeout` or
  `parked_idle_timeout` is set; both are off by default.

## Upgrade

Replace the binary and restart the roles; 0.1.0 state is reused.

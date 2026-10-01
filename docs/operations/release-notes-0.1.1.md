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

## Upgrade

Replace the binary and restart the roles; 0.1.0 state is reused.

# Install an engine

CapyCTL does not install engines. This page walks through installing one
yourself, in its own Python virtual environment, on Linux with one NVIDIA GPU,
then registering it with CapyCTL. Pick the engine you want; you can install
more than one, each in its own environment.

| Engine | Versions this release knows | Environment used below |
|---|---|---|
| vLLM | 0.30.0, 0.29.0 | `~/venvs/vllm` |
| SGLang | 0.5.21, 0.5.20 | `~/venvs/sglang` |
| TensorFold | 0.6.5, 0.6.3, 0.6.2, 0.6.1, 0.6.0 | `~/venvs/tensorfold` |
| llama.cpp | v0.6.0 (every build lists as `CUSTOM yes` for now) | `~/llama.cpp` (a build, no environment) |

Another version still runs, but `capyctl engine detect` shows it as
`CUSTOM yes` ([Custom builds](engines.md#custom-builds)). Pin the version as
shown to get one CapyCTL knows.

## Before you start

1. Check the GPU and driver:

   ```bash
   nvidia-smi --query-gpu=name,compute_cap,driver_version --format=csv
   nvidia-smi
   ```

   The first command prints the GPU's name, compute capability and driver
   version. The header of the second shows `CUDA Version` (`CUDA UMD Version` on
   newer drivers), the newest CUDA the driver supports. Every install below uses CUDA 13.0 builds of PyTorch, so it
   must read 13.0 or higher; if it does not, update the driver first
   ([CUDA release notes](https://docs.nvidia.com/cuda/cuda-toolkit-release-notes/)
   list the minimum driver for each CUDA version).

2. Compute capability: vLLM needs 7.5 or higher. TensorFold needs 8.9 or
   higher (RTX 40 and 50 series, Hopper, Blackwell, DGX Spark's GB10) and
   refuses older GPUs at startup.

3. Install `uv`, which creates the environments and their Python
   ([uv installation](https://docs.astral.sh/uv/getting-started/installation/)):

   ```bash
   curl -LsSf https://astral.sh/uv/install.sh | sh
   ```

   The commands below ask `uv` for Python 3.12 and let it download that Python,
   so the system Python does not matter.

4. Disk space: each environment takes several gigabytes (about 6 GB for vLLM,
   9 GB for SGLang and 5 GB for TensorFold on x86-64), plus `uv`'s download
   cache in `~/.cache/uv`. Models need their own space in `~/models`.

5. Architecture: the same commands work on x86-64 and on ARM64 (aarch64), such
   as a DGX Spark. vLLM, SGLang's kernels and PyTorch 2.13.0 publish wheels
   for both, and so does the PyTorch CUDA 13.0 index.

## vLLM 0.30.0 or 0.29.0

1. Create the environment:

   ```bash
   uv venv --python 3.12 --seed --managed-python ~/venvs/vllm
   ```

2. Install vLLM with PyTorch built for CUDA 13.0 (for 0.29.0, write
   `vllm==0.29.0`):

   ```bash
   uv pip install --python ~/venvs/vllm/bin/python vllm==0.30.0 --torch-backend=cu130
   ```

   Both releases use PyTorch 2.13.0. `--torch-backend=cu130` makes `uv` take
   PyTorch from its CUDA 13.0 index; `--torch-backend=auto` picks the index
   from your driver instead.

3. Check it:

   ```bash
   ~/venvs/vllm/bin/vllm --version
   ```

   It prints the version, for example `0.30.0`.

vLLM's own guide:
[GPU installation](https://docs.vllm.ai/en/v0.30.0/getting_started/installation/gpu/).

## SGLang 0.5.21 or 0.5.20

1. Create the environment:

   ```bash
   uv venv --python 3.12 --seed --managed-python ~/venvs/sglang
   ```

2. Install SGLang. This is SGLang's own `uv` command, pinned to 0.5.21
   (write `0.5.20` for that version):

   ```bash
   uv pip install --python ~/venvs/sglang/bin/python --prerelease=allow "sglang==0.5.21"
   ```

   Both versions need CUDA 13 and install PyTorch 2.13.0 and
   `torch_memory_saver`, which CapyCTL uses to park SGLang models.
   `--prerelease=allow` is needed with `uv` older than 0.12.0 and does nothing
   on newer ones.

3. Check it, and check that PyTorch sees the GPU with CUDA 13.0:

   ```bash
   ~/venvs/sglang/bin/python -c "import sglang; print(sglang.__version__)"
   ~/venvs/sglang/bin/python -c "import torch; print(torch.__version__, torch.version.cuda, torch.cuda.is_available())"
   ```

   The first prints `0.5.21`, the second `2.13.0`, `13.0` and `True` (the
   version can carry a `+cu130` suffix).

SGLang's own guide: [Installation](https://docs.sglang.io/docs/get-started/install).
For DGX Spark, SGLang links
[this guide](https://lmsys.org/blog/2025-11-03-gpt-oss-on-nvidia-dgx-spark/).

## TensorFold 0.6.5, 0.6.3, 0.6.2, 0.6.1 or 0.6.0

TensorFold builds CUDA kernels the first time it serves, so besides the Python
packages it needs `nvcc`, `ninja` and a C++ compiler. TensorFold's own
instructions for NVIDIA GPUs use NVIDIA's PyTorch container; CapyCTL runs
TensorFold from a plain environment, which is what these steps build.

1. Create the environment:

   ```bash
   uv venv --python 3.12 --seed --managed-python ~/venvs/tensorfold
   ```

2. Install PyTorch and Triton built for CUDA 13.0. TensorFold does not install
   them itself:

   ```bash
   uv pip install --python ~/venvs/tensorfold/bin/python "torch==2.13.0" triton \
     --index-url https://download.pytorch.org/whl/cu130
   ```

3. Install TensorFold from its release tag, and `ninja` (for an earlier
   version, write `@v0.6.3`, `@v0.6.2`, `@v0.6.1` or `@v0.6.0`). This needs `git`:

   ```bash
   uv pip install --python ~/venvs/tensorfold/bin/python \
     "tensorfold @ git+https://github.com/ashhart/TensorFold.git@v0.6.5" ninja
   ```

4. Provide the CUDA compiler. Either:

   - a system CUDA toolkit with `nvcc`, usually at `/usr/local/cuda`
     (`ls /usr/local/cuda/bin/nvcc` to check; otherwise
     [NVIDIA's CUDA downloads](https://developer.nvidia.com/cuda-downloads)),
     or
   - 0.6.1 and later: NVIDIA's compiler wheels in the environment, matched to
     PyTorch's CUDA 13.0:

     ```bash
     uv pip install --python ~/venvs/tensorfold/bin/python "cuda-toolkit[nvcc,cccl]==13.0.*"
     ```

   The C++ compiler always comes from the system, for example
   `sudo apt install build-essential` on Ubuntu.

5. Check it:

   ```bash
   ~/venvs/tensorfold/bin/tensorfold --version
   ```

   It prints `tensorfold 0.6.5`.

TensorFold's own guide: the
[README](https://github.com/ashhart/TensorFold/blob/v0.6.5/README.md) and
[runbook](https://github.com/ashhart/TensorFold/blob/v0.6.5/RUNBOOK.md)
at the release tag.

## llama.cpp v0.6.0

llama.cpp is a C++ program: CapyCTL runs its `llama-server` binary, built from
the release tag. The build needs `git`, `cmake`, a C++ compiler and a CUDA
toolkit with `nvcc` (`ls /usr/local/cuda/bin/nvcc` to check).

1. Get the release tag:

   ```bash
   git clone --branch v0.6.0 --depth 1 https://github.com/ggml-org/llama.cpp ~/llama.cpp
   ```

2. Build `llama-server` with CUDA. `-DLLAMA_BUILD_IS_DEV=OFF` makes the binary
   report `0.6.0` rather than `0.6.0-dev`; `CMAKE_CUDA_ARCHITECTURES` limits
   the kernels to your GPU (`89` for RTX 40 series; leave it out to build for
   every architecture, which takes much longer):

   ```bash
   cd ~/llama.cpp
   cmake -B build -DGGML_CUDA=ON -DLLAMA_BUILD_IS_DEV=OFF \
     -DCMAKE_CUDA_COMPILER=/usr/local/cuda/bin/nvcc -DCMAKE_CUDA_ARCHITECTURES=89
   cmake --build build -j --target llama-server
   ```

   The binary is `~/llama.cpp/build/bin/llama-server`, with the libraries it
   loads (`libllama.so`, `libggml-cuda.so`, ...) beside it. Keep them together:
   CapyCTL fingerprints the binary and those libraries as one installation.

3. Check it:

   ```bash
   ~/llama.cpp/build/bin/llama-server --version
   ```

   It prints `version: 0.6.0 (build 1, commit d81235049)` on standard error.

4. Get a model as a GGUF file, in a directory of its own under `~/models`,
   for example:

   ```bash
   hf download Qwen/Qwen3-4B-GGUF Qwen3-4B-Q4_K_M.gguf --local-dir ~/models/Qwen3-4B-GGUF
   ```

Do not create `/etc/llama.cpp/config.ini`: llama-server reads it as options, so
CapyCTL refuses to register or start llama.cpp on a machine that has it.

llama.cpp's own guide: the
[server README](https://github.com/ggml-org/llama.cpp/blob/v0.6.0/tools/server/README.md)
at the release tag.

## Register it with CapyCTL

1. Find it:

   ```bash
   capyctl engine detect
   ```

   Environments in `~/venvs` are found without options; the `VERSION` column
   should show the version you installed and `CUSTOM` should read `no`.

2. Add it:

   ```bash
   capyctl engine add ~/venvs/vllm
   ```

   Name `~/venvs/sglang` or `~/venvs/tensorfold` for the others, or the
   `llama-server` binary (`~/llama.cpp/build/bin/llama-server`) for llama.cpp.
   [Add an engine](engines.md) shows what it prints.

   `engine add` writes `engines.yaml` beside the role's config file, not under
   `--state-dir`. For a separate test setup, set both `--config` (or
   `XDG_CONFIG_HOME`) and `--state-dir`, and give the CLI and the running role
   the same ones.

3. Run a first model: [First model on each engine](engines.md#first-model-on-each-engine)
   has a deployment file and a request for each engine. More deployment files,
   each with the engine version, GPU and numbers it was measured with, are in
   [capyctl-recipes](https://github.com/edurdias/capyctl-recipes).

## Troubleshooting

- **PyTorch built for the wrong CUDA.** `torch.version.cuda` prints something
  other than `13.0`, or `torch.cuda.is_available()` prints `False`: the
  environment has a CPU or other-CUDA PyTorch. Create the environment again and
  install with the commands above; for vLLM keep `--torch-backend=cu130`.
- **`toolchain_missing` from `engine add` (TensorFold).** `nvcc`, `ninja` or a
  C++ compiler is missing where the engine looks: the environment's `bin`, the
  CUDA toolkit's `bin`, `/usr/local/bin`, `/usr/bin`, `/bin`. The message names
  which one. Install it (step 3 or 4 of TensorFold), or set `CUDA_HOME` to a
  toolkit that has `nvcc`, then add the engine again. CapyCTL does not use your
  shell's `PATH` for this. See [exit codes and errors](errors.md).
- **`CUSTOM yes`.** The installed version is not one this release knows,
  usually because the install was not pinned and took a newer release. Install
  the pinned version, or keep it and register it under its own name
  ([Custom builds](engines.md#custom-builds)).
- **TensorFold's first start is slow.** It compiles its kernels; CapyCTL allows
  up to 30 minutes, and later starts reuse the build.
- **SGLang permissions warning in the engine log.** The environment is group-
  or world-writable; `chmod -R go-w ~/venvs/sglang` clears it
  ([errors](errors.md)).

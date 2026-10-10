# CapyCTL documentation

CapyCTL runs vLLM, SGLang, TensorFold and llama.cpp models on GPU machines you own. It
parks the models nobody is using, which frees their GPU memory, and wakes the
one a request asks for. Clients use one OpenAI-compatible endpoint, on your
network, with an API key.

Start here:

1. [How it works](how-it-works.md): one picture of the parts, and what parks
   and wakes mean.
2. [Install](install.md) CapyCTL with one command.
3. [Run on one machine](one-machine.md): add an engine, start, deploy, send a
   request.
4. [Run on several machines](several-machines.md): one server, several GPU
   machines.

Tasks:

- [Install an engine](install-engines.md): vLLM, SGLang or TensorFold in a
  virtual environment, or a llama.cpp build, step by step.
- [Add an engine](engines.md): register the vLLM, SGLang, TensorFold or llama.cpp you have.
- [Deploy a model](deploy.md): the deployment file; start, stop, delete.
- [Make a request](requests.md): curl, streaming, the Python client, other
  machines.
- [Parking and switching](parking.md): more models than your GPU holds.
- [How requests wait and models switch](parking.md#how-requests-wait-and-models-switch):
  two apps, two models, one GPU.

Reference: [CLI](/docs/reference/cli/), [configuration files](configuration.md),
[settings](../operations/configuration.md), [engine options](engine-flags.md),
[network access](../operations/network-access.md),
[installer options](installer.md), [exit codes and errors](errors.md).

CapyCTL does not install engines or GPU drivers. Bring your own vLLM, SGLang or
TensorFold installation; [Install an engine](install-engines.md) shows how. Models come from a directory on your machine or from Hugging
Face.

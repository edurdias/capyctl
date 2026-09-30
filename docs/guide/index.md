# CapyCTL documentation

CapyCTL runs vLLM and SGLang models on GPU machines you own. It parks the models
nobody is using, which frees their GPU memory, and wakes the one a request
asks for. Clients use one OpenAI-compatible endpoint, on your network, with an
API key.

Start here:

1. [How it works](how-it-works.md): one picture of the parts, and what parks
   and wakes mean.
2. [Install](install.md) CapyCTL with one command.
3. [Run on one machine](one-machine.md): add an engine, start, deploy, send a
   request.
4. [Run on several machines](several-machines.md): one server, several GPU
   machines.

Tasks:

- [Add an engine](engines.md): register the vLLM or SGLang you have.
- [Deploy a model](deploy.md): the deployment file; start, stop, delete.
- [Make a request](requests.md): curl, streaming, the Python client, other
  machines.
- [Parking and switching](parking.md): more models than your GPU holds.

Reference: [CLI](/docs/reference/cli/), [configuration files](configuration.md),
[settings](../operations/configuration.md),
[network access](../operations/network-access.md),
[installer options](installer.md), [exit codes and errors](errors.md).

CapyCTL does not install engines or GPU drivers. Bring your own vLLM or SGLang
installation. Models come from a directory on your machine or from Hugging
Face.

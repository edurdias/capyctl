# mllm documentation

mllm runs vLLM and SGLang models on GPU machines you own. It parks the models
nobody is using, which frees their GPU memory, and wakes the one a request
asks for. Clients use one OpenAI-compatible endpoint.

Start here:

1. [Install](install.md) mllm with one command.
2. [Run on one machine](one-machine.md): add an engine, start, deploy, send a
   request.
3. [Run on several machines](several-machines.md): one server, several GPU
   machines.

Tasks:

- [Add an engine](engines.md): register the vLLM or SGLang you have.
- [Deploy a model](deploy.md): the deployment file; start, stop, delete.
- [Make a request](requests.md): curl, streaming, the Python client.
- [Parking and switching](parking.md): more models than your GPU holds.

Reference: [CLI](/docs/reference/cli/), [configuration files](configuration.md),
[installer options](installer.md), [exit codes and errors](errors.md).

mllm does not install engines, drivers or model weights. Bring your own vLLM or
SGLang installation and your own checkpoints.

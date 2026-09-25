# mllm documentation

mllm runs vLLM and SGLang models on GPU machines you own. It parks the models
nobody is using, which frees their GPU memory, and wakes the one a request
asks for. Clients use one OpenAI-compatible endpoint.

1. [Install](install.md) mllm with one command.
2. [Quickstart](quickstart.md): one machine, one model, a park and a wake.
3. [Several machines](several-machines.md): one server, several GPU machines.

Reference: [CLI](/docs/reference/cli/), [configuration files](configuration.md),
[installer options](installer.md), [exit codes and errors](errors.md).

mllm does not install engines, drivers or model weights. Bring your own vLLM or
SGLang installation and your own checkpoints.

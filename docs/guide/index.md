# mllm documentation

mllm runs vLLM and SGLang engines on GPU machines you own. It parks the models
nobody is using so their GPU memory is released, wakes the one a request asks
for, and puts one OpenAI-compatible endpoint in front of every machine you
enroll.

- [Install](../operations/install.md) mllm from a release.

mllm does not install engines, drivers or model weights. Bring your own vLLM or
SGLang environment and your own checkpoints.

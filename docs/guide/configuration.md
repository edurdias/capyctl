# Configuration reference

mllm reads four kinds of YAML document. `mllm validate config --file <file>`
checks any of them; `--host host.yaml` also resolves a deployment against a
host. The examples below are the files in [`docs/examples/`](../examples/),
shown verbatim; a test checks that mllm accepts every one. They illustrate the
schema and are not tuned engine settings for any model.

## Deployment on one host

[`deployment-single.yaml`](../examples/deployment-single.yaml)

<!-- include: ../examples/deployment-single.yaml -->

## Deployment on several hosts

[`deployment-multinode.yaml`](../examples/deployment-multinode.yaml)

<!-- include: ../examples/deployment-multinode.yaml -->

## Host

[`host.yaml`](../examples/host.yaml)

<!-- include: ../examples/host.yaml -->

## Server

[`server.yaml`](../examples/server.yaml)

<!-- include: ../examples/server.yaml -->

## Standalone

`mllm start standalone` writes this document itself on first start; you only
need one to change its defaults.
[`standalone.yaml`](../examples/standalone.yaml)

<!-- include: ../examples/standalone.yaml -->

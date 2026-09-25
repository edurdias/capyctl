# Configuration files

mllm reads four kinds of YAML file. Check any of them with:

```bash
mllm validate config --file <file>
```

The examples below are the files in [`docs/examples/`](../examples/), shown
as they are; a test checks that mllm accepts every one. They show the format;
they are not tuned settings for any model.

## Deployment for standalone

The [Quickstart](quickstart.md) uses this one.

<!-- include: ../examples/deployment-standalone.yaml -->

## Deployment on one host

<!-- include: ../examples/deployment-single.yaml -->

## Deployment on several hosts

<!-- include: ../examples/deployment-multinode.yaml -->

## Host

<!-- include: ../examples/host.yaml -->

## Server

<!-- include: ../examples/server.yaml -->

## Standalone

`mllm start standalone` writes this file itself on first start. You only
need your own to change its defaults.

<!-- include: ../examples/standalone.yaml -->

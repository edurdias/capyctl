# Configuration files

CapyCTL reads four kinds of YAML file: deployments, and the files of the
server, a host and standalone. Check any of them with:

```bash
capyctl validate config --file <file>
```

The examples below are the files in [`docs/examples/`](../examples/), shown
as they are; a test checks that CapyCTL accepts every one. They show the format;
they are not tuned settings for any model. Every setting of the server, host
and standalone files can also be given as a flag or an environment variable,
and `capyctl config show` prints where each value came from: see
[Settings](../operations/configuration.md).

## The smallest deployment

Three fields; CapyCTL fills in the rest from the checkpoint and the machine.

<!-- include: ../examples/deployment-minimal.yaml -->

## Deployment for standalone, every field

This one names `local`, the engine standalone takes from `--vllm-bin` or
`CAPYCTL_VLLM_BIN`. With an engine added by [`capyctl engine add`](engines.md),
name that profile instead.

<!-- include: ../examples/deployment-standalone.yaml -->

## Deployment on one host

<!-- include: ../examples/deployment-single.yaml -->

## Deployment on several hosts

<!-- include: ../examples/deployment-multinode.yaml -->

## Host with unified memory

<!-- include: ../examples/host.yaml -->

## Host with a discrete GPU

<!-- include: ../examples/host-discrete.yaml -->

## Server

<!-- include: ../examples/server.yaml -->

## Standalone

`capyctl start standalone` writes this file itself on first start, and reads
the GPU to size memory. You only need your own to change its defaults.

<!-- include: ../examples/standalone.yaml -->

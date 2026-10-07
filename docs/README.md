# capyctl documentation

This directory holds the user documentation for capyctl 0.1.2 and the design and
process records its contributors work from. The project overview and
quickstart are in the repository [`README.md`](../README.md).

## Using capyctl

| Document | Contents |
|---|---|
| [`operations/install.md`](operations/install.md) | Release assets and `install.sh`, systemd units per role, file locations, restart versus drain, exit codes, discrete NVIDIA GPUs, upgrade (including to 0.1.0) and rollback. |
| [`operations/configuration.md`](operations/configuration.md) | Every setting with its YAML field, flag and environment variable, `--set` and `capyctl config show`. |
| [`operations/network-access.md`](operations/network-access.md) | Reaching the inference endpoint from other machines: the API key, narrowing to loopback or a Tailscale address, turning the key off, a TLS reverse proxy. |
| [`operations/release-notes-0.1.0.md`](operations/release-notes-0.1.0.md) | Release notes for 0.1.0. |
| [`operations/release-notes-0.1.1.md`](operations/release-notes-0.1.1.md) | Release notes for 0.1.1. |
| [`operations/release-notes-0.1.2.md`](operations/release-notes-0.1.2.md) | Release notes for 0.1.2. |
| [`examples/server.yaml`](examples/server.yaml) | Server role document: loopback management, inference on all interfaces behind the API key, networked enrollment and control listeners. |
| [`examples/host.yaml`](examples/host.yaml) | Host role document: private ingress, unified-memory resource policy, placement labels, and vLLM and SGLang installations. |
| [`examples/host-discrete.yaml`](examples/host-discrete.yaml) | Host role document for a discrete-GPU host (one 24 GB NVIDIA card): a `system` host-RAM domain and a `gpu0` device domain. |
| [`examples/standalone.yaml`](examples/standalone.yaml) | Standalone role document (embedded server and host on one machine), in the shape `capyctl start standalone` generates. |
| [`examples/deployment-minimal.yaml`](examples/deployment-minimal.yaml) | The smallest deployment: `name`, `engine` and `model`, everything else defaulted. |
| [`examples/deployment-single.yaml`](examples/deployment-single.yaml) | vLLM deployment on one host, with `engine_config` and `timeouts`. |
| [`examples/deployment-spread.yaml`](examples/deployment-spread.yaml) | SGLang deployment of two instances spread over two hosts (`instances`, `placement`). |
| [`examples/deployment-multinode.yaml`](examples/deployment-multinode.yaml) | One vLLM model across two hosts: a tensor-parallel group of two ranks (`topology`, `placement.hosts`, head first). |
| [`examples/host-b.yaml`](examples/host-b.yaml) | The group's second host: the same engines as `examples/host.yaml` and its own `resource_policy.groups.peer_address`. |

Every example passes `capyctl validate config`, the deployments also resolve
against `examples/host.yaml`, and the minimal one against
`examples/host-discrete.yaml`; `crates/capyctl-cli/tests/validate_config.rs` checks
this. Host names, addresses, paths, fingerprints, byte budgets and durations
are placeholders. The examples show the schema; they are not tested engine
recipes, and passing validation does not mean an engine will start with them.

## Contributing to capyctl

Start with [`AGENTS.md`](../AGENTS.md), the working agreement for human and
coding-agent contributors. It defines which documents are authoritative and
in what order.

| Document | Contents |
|---|---|
| [`SPEC.md`](SPEC.md) | The authoritative product requirements: architecture, interfaces, resource rules, configuration, delivery gates and the T01–T40 acceptance matrix. Where any other document disagrees, the spec wins. |
| [`design/0000-full-picture.md`](design/0000-full-picture.md) | The overall design and how the parts fit together, recorded around the spec. |
| [`design/adr/`](design/adr/) | Architecture decision records (ADR 0001 onwards). An ADR that amends the spec says so. |
| [`design/milestones/`](design/milestones/) | Milestone designs and plans (F0 foundation, F1 vLLM path, F2 SGLang and the two-host program). |
| [`runbooks/f2-current-status.md`](runbooks/f2-current-status.md) | The single status record: what is done, what remains and what is queued next. |
| [`runbooks/`](runbooks/) | Operational records kept by the maintainers: live-run evidence, engine environment notes and the carried [vLLM development-mode warning](runbooks/vllm-development-mode-warning.md). |
| [`plans/`](plans/) | Implementation plans, one per slice of work. |
| [`specs/`](specs/) | Design notes for individual features written ahead of their plans. |

The milestone, runbook and plan documents are working records. Most describe
the state of the work when they were written; the spec and the status runbook
are the current references.

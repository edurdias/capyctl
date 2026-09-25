# mllm documentation

This directory holds the user documentation for mllm 0.1.0 and the design and
process records its contributors work from. The project overview and
quickstart are in the repository [`README.md`](../README.md).

## Using mllm

| Document | Contents |
|---|---|
| [`operations/install.md`](operations/install.md) | Release assets and `install.sh`, systemd units per role, file locations, restart versus drain, exit codes, upgrade and rollback. |
| [`examples/server.yaml`](examples/server.yaml) | Server role document: loopback management and inference listeners, networked enrollment and control listeners. |
| [`examples/host.yaml`](examples/host.yaml) | Host role document: private ingress, unified-memory resource policy, placement labels, and vLLM and SGLang installations. |
| [`examples/standalone.yaml`](examples/standalone.yaml) | Standalone role document (embedded server and host on one machine), in the shape `mllm start standalone` generates. |
| [`examples/deployment-single.yaml`](examples/deployment-single.yaml) | vLLM deployment on one host, with `engine_config` and `timeouts`. |
| [`examples/deployment-multinode.yaml`](examples/deployment-multinode.yaml) | SGLang deployment of two instances spread over two hosts (`instances`, `placement`). |

Every example passes `mllm validate config`, and the deployments also resolve
against `examples/host.yaml`; `crates/mllm-cli/tests/validate_config.rs` checks
this. Host names, addresses, paths, fingerprints, byte budgets and durations
are placeholders. The examples show the schema; they are not tested engine
recipes, and passing validation does not mean an engine will start with them.

## Contributing to mllm

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
| [`superpowers/plans/`](superpowers/plans/) | Implementation plans, one per slice of work. |
| [`superpowers/specs/`](superpowers/specs/) | Design notes for individual features written ahead of their plans. |

The milestone, runbook and plan documents are working records. Most describe
the state of the work when they were written; the spec and the status runbook
are the current references.

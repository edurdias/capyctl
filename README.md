# mllm

Engine-neutral lifecycle and inference-routing controller: one endpoint, bring-your-own
inference engines, explicit deployment ownership, safe model residency transitions, and
aggregate resource control.

**Status:** F0 foundation and the F1 vLLM path are implemented, with scoped
live Spark evidence. The latest 4B routed concurrency test passed after a
readiness-ordering fix. This is not general production qualification;
management CLI wiring and real resource accounting remain incomplete.
F2 (SGLang through the same contracts) is next.

See [current gaps](docs/design/milestones/f1-open-items.md) and
[model-size and concurrency evidence](docs/runbooks/spark-model-size-qualification.md).

## Install

A release is one self-contained binary per OS/architecture (Linux x86-64 and
ARM64); mllm's Python runtime helpers are compiled into it. The repository is
private, so use a logged-in `gh` (or set `GITHUB_TOKEN`):

```bash
gh release download v0.1.0-rc.1 -R edurdias/mllm -p install.sh
sh install.sh --version 0.1.0-rc.1          # ~/.local/bin/mllm
# sudo sh install.sh --system ...           # /usr/local/bin/mllm
# add --systemd <server|host|standalone> to install that role's unit
```

The installer verifies every download against the release's `SHA256SUMS` and
refuses on a mismatch. Engines, their Python environments and model weights
are yours to install; mllm never installs them. See
[`docs/operations/install.md`](docs/operations/install.md) for services,
upgrades and rollback.

## Quickstart (standalone)

One machine, one engine, loopback only:

```bash
export MLLM_VLLM_BIN=/path/to/venv/bin/vllm   # or MLLM_SGLANG_BIN
export MLLM_MODELS_ROOT=/srv/models
mllm start standalone                          # inference on 127.0.0.1:8443
```

The first start writes its role document and credentials under
`~/.local/state/mllm` and the runtime helpers to `~/.local/state/mllm/runtime`.
In another shell, deploy a model and watch it (see
[`docs/examples/deployment-single.yaml`](docs/examples/deployment-single.yaml)):

```bash
mllm deploy model --file deployment.yaml --activate --wait
mllm list deployments
```

For a server with remote hosts: `mllm init server` and `mllm start server`
on the control-plane machine, `mllm invite host` there, then `mllm init host`,
`mllm join host --join-file ...` and `mllm start host` on each compute host.

## Documentation

- [`docs/SPEC.md`](docs/SPEC.md) — authoritative architecture and requirements (rev 0.2).
- [`docs/AGENT_HANDOFF.md`](docs/AGENT_HANDOFF.md) — coding-agent brief.
- [`docs/design/0000-full-picture.md`](docs/design/0000-full-picture.md) — approved
  decisions, crate layout, and milestone decomposition.
- [`docs/design/adr/`](docs/design/adr/) — architecture decision records.
- [`docs/examples/`](docs/examples/) — example role and deployment documents that
  `mllm validate config` accepts (checked by a test; not calibrated engine recipes).

License: Apache-2.0 (see [ADR 0006](docs/design/adr/0006-license.md)); `LICENSE` file is
added before publication.

# Status: Group listeners documented — 2026-10-08 (branch `docs/multi-host-listeners`, stacked on #71)

Owner decision 2026-10-08: ADR 0028 decision 3 (trust the network, bind to the direct link where the engine allows, known risk) stands. The several-machines guide gains "Ports a group opens": per engine (vLLM 0.30, SGLang 0.5.21, TensorFold 0.6.5) every listener, its bind (link address, all interfaces, loopback or engine-chosen), no authentication and the pickle risk, and example nftables and ufw rules for the direct link. Sourced from vLLM 0.29 and SGLang 0.5.20 sources, TensorFold 0.6.3, and `crates/capyctl-adapters` group renderers; torch store, NCCL and TensorFold's `--parallel` port are marked inferred. Found from source: SGLang with DP attention also binds `P+13` (handshake) and one ephemeral socket per DP rank on the head's link address, which `Prepare` does not probe. ADR 0028 decision 3 carries the reaffirmation. Docs only; fast `scripts/ci-local.sh` run; deep pending. Live check pending: MN1 records the actual binds with `ss -ltnp` on both hosts.

# Release note: Multi-node groups

- **Ports a group opens.** The guide lists every listener a vLLM, SGLang or
  TensorFold group opens, the address it binds and that none is
  authenticated, with example nftables and ufw rules for the direct link
  ([Ports a group opens](../guide/several-machines.md#ports-a-group-opens)).

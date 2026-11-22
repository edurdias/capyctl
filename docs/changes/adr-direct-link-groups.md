# Status: Direct-link groups amendment proposed for owner review — 2026-11-09 (branch `docs/adr-direct-link-groups`)

The owner authorized starting the design for issue #178 on 2026-11-09.
[ADR 0028](../design/adr/0028-multi-node-engine-groups.md#proposed-amendment-2026-11-09--private-link-ingress-and-group-socket-interfaces)
now contains a proposed amendment, not an accepted decision or implementation:
private-unicast ingress under the existing explicit trust declaration, exact NCCL
socket-interface and address-family rendering for every group engine, unchanged
unauthenticated-peer risks, and capability-gated rollout. It compares a fixed
address allowlist with a declared link CIDR and interface binding. The owner must
decide whether socket/bootstrap pinning is enough or whether RDMA link selection
must also be designed before implementation.

No runtime behavior or tests changed. The amendment specifies tests under the
existing T-IDs and a functional live check on two GB10 hosts connected by a cable;
that live check has not run. CPU and Fake-engine tests are not qualification.

# Release note: none

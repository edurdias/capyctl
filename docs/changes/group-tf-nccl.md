# Status: TensorFold group members get the installation's libnccl — 2026-10-15 (branch `fix/group-tf-nccl`)

Found live 2026-10-15 (MN3): both ranks of a TensorFold 0.6.5 TP2 group
exited `RuntimeError: libnccl not found (set TF_NCCL_LIB)`. TensorFold 0.6.5
and 0.6.6 (`tensorfold/cuda/comm.py` `_library`) load libnccl from the file
`TF_NCCL_LIB` names, else `find_library("nccl")`, system `/usr/lib/*` and
`dist-packages`, else torch's `lib`; a pip venv keeps it in the nvidia-nccl
wheel (`<venv>/lib/python3*/site-packages/nvidia/nccl/lib/libnccl.so.2`),
which none of those reach. CapyCTL owned `TF_NCCL_LIB` (ADR 0028 §2.1) and
stripped it, but never set it; ADR 0028 §10's note that TensorFold's NCCL is
built in was wrong.

Now the TensorFold Initialize step of a group member resolves the
installation's library from the registered executable's venv (the wheel's
`libnccl.so.2`, else torch's `lib/libnccl*.so*`) and renders it as
`TF_NCCL_LIB` on both ranks, inside the closed environment; the agent's own
value never passes through. With no library it refuses before any effect with
the closed category `nccl_library_missing` and an operator hint. Single-rank
launches are unchanged.

Tests (`capyctl-adapters`): `both_group_ranks_start_with_the_installations_libnccl`
and `a_group_member_without_libnccl_is_refused_before_any_effect` fail before
(no `TF_NCCL_LIB`, the launch spawns) and pass after;
`no_transport_variables_and_no_credentials` now asserts the rendered value on
both ranks. The agent's `group_launch` fake venv now holds the wheel's
`libnccl.so.2`, as a real installation does. CPU and Fake-engine tests are not qualification; live check still
needed: MN3 and MN5 tensorfold rows must reach Ready.

# Release note: Multi-node groups

- **TensorFold groups start.** A TensorFold multi-node group member now starts
  with its installation's libnccl (`TF_NCCL_LIB`); before, both ranks exited
  `libnccl not found`. An installation without the nvidia-nccl wheel is
  refused with `nccl_library_missing` instead of crashing.

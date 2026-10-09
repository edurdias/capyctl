# Status: DP attention head check covers P+13 — 2026-10-09 (branch `dp-attention-ports`, stacked on #86)

Owner decision 2026-10-09: extend the R33 pre-launch check (ADR 0028 §7) now, although `enable_dp_attention` is still reserved (path unreachable). Read against SGLang 0.5.21 source (`srt/server_args.py` `PortArgs.init_new`, `srt/managers/data_parallel_controller.py`): with `--dist-init-addr` the head binds `port_base..port_base+5` with `port_base = P+1`, or `P-7` when `P+6 > 65535` (the previous `P+7` trigger was off by one: at `P=65529` SGLang uses `65530..65535`), plus the REP handshake at `P + DP_ATTENTION_HANDSHAKE_PORT_DELTA` (13) with no fallback, plus one `get_zmq_socket_on_host` PUSH per DP rank on an ephemeral port. `host_checks::dp_attention_ports` now returns `P+1..P+6, P+13`, and `None` above 65522 (refused `rendezvous_port_in_use:<P>`); SGLang's own move of the six is therefore never reached. Per-rank ports documented as uncheckable in the guide, ADR 0028 §7 and the spec. Test `dp_attention_probes_the_derived_head_ports` (R33): the new assertions do not hold against the previous range-returning function, pass after. CPU tests are not qualification. Fast `scripts/ci-local.sh` passes; deep pending. Live check pending with MN1 (`ss -ltnp` on a DP-attention head) once DP attention is allowed.

# Release note: Multi-node groups

- **DP attention ports.** With SGLang DP attention, the head's start check
  now also covers the handshake port `P+13`, and a rendezvous port above
  65522 is refused because SGLang cannot bind the handshake there. The
  per-rank ports SGLang picks at start cannot be checked
  ([Ports a group opens](../guide/several-machines.md#ports-a-group-opens)).

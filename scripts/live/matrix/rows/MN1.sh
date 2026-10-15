# shellcheck shell=bash
# MN1 (design section 17.2, ADR 0028; T34): vLLM 0.30.0 brings up Qwen3-30B-A3B as a
# two-rank tensor-parallel group across host A (head, rank 0) and host B (worker,
# rank 1) through the server:
#   run_row.sh MN1
# MN2 (SGLang) and MN3 (TensorFold) are the same row (rows/MN2.sh, rows/MN3.sh).
#
# Setup: server and both hosts online (roles.sh up), each host declaring its
# direct-link address as its peer address (HOST_<A|B>_DIRECT_ADDR) and publishing
# the engine's profile with the same build fingerprint; the bring-up checkpoint on
# both hosts; no deployment active on either host. group_preflight checks all of
# that read-only and stops the row before it deploys anything when one is missing.
# The owner's host tuning (compaction 0, unlimited memlock, read-write
# /dev/infiniband/uverbs*) is reported, not changed.
#
# Expected:
#   ready      deploy --activate --wait reaches Ready through the server; status shows
#              rank 0 head on host A and rank 1 worker on host B, both launched and
#              charged in the ready phase; MemAvailable drops on both hosts.
#   20/20      twenty routed completions, each with its right answer.
#   transport  the RDMA port counters of both hosts, read before and after the twenty
#              completions, rose on both (transport: rdma, with the devices that
#              carried traffic); otherwise transport: socket is recorded (the row still
#              passes bring-up, and the result says so).
#   stop       stop settles stopped; each host settles its own member; verified cleanup
#              on both hosts (identities gone, no engine, GPU process or rendezvous
#              directory, rendezvous and engine ports free, nothing held); delete.
# shellcheck source=scripts/live/matrix/grouplib.sh
. "$MATRIX_DIR/grouplib.sh"

MN_COMPLETIONS=${MN_COMPLETIONS:-20}

bringup_row() { # bringup_row <engine>
  local engine=$1 dep host rc=0
  dep=$(group_fixture_name "$engine")
  echo "engine $engine deployment $dep head $HOST_A worker $HOST_B" | tee -a "$EVID/timeline.txt"
  step preflight group_preflight "$engine" || return 1
  for host in "${GROUP_HOSTS[@]}"; do step "idle-$(host_short "$host")" host_idle "$host" || return 1; done
  step fixture group_fixture "$engine" || return 1
  dry || cp "$RUNSTATE/fixtures/$dep.yaml" "$EVID/fixture-$dep.json"
  step mem-before mem_both before

  step deploy timed deploy deploy "$dep" --activate --wait || {
    step status-failed group_status "$dep" failed
    step ledger-failed group_ledger "$dep" failed
    for host in "${GROUP_HOSTS[@]}"; do step "errors-$(host_short "$host")" engine_errors "$host"; done
    return 1
  }
  step ready keep_group "$dep" ready
  step ready-check group_ready_check "$dep" ready || rc=1
  # What each member's engine was given for the collective (named variables only).
  step member-env member_env "$dep" ready TF_NCCL_LIB TF_COMM_BACKEND NCCL_SOCKET_IFNAME NCCL_IB_HCA GLOO_SOCKET_IFNAME MASTER_ADDR MASTER_PORT
  step inspect inspect_dep "$dep"
  step mem-ready mem_both ready

  step rdma-before rdma_read before || rc=1
  step completions completions "$dep" "$MN_COMPLETIONS" bringup || rc=1
  step rdma-after rdma_read after || rc=1
  step transport rdma_transport before after || rc=1
  for host in "${GROUP_HOSTS[@]}"; do
    step "nccl-$(host_short "$host")" engine_log_lines "$host" 'NCCL INFO (NET|Using|Channel|Connected)|NET/IB|NET/Socket|via NET'
  done
  step status-served group_status "$dep" served

  group_stop_settle "$dep" stopped || rc=1
  step evidence evidence "$dep" stopped
  step journal journal_hosts "$dep" stopped
  sleep 3
  step cleanup group_cleanup_check "$dep" stopped ready || rc=1
  step mem-after mem_both after
  step delete delete_dep "$dep" || rc=1
  return "$rc"
}

row_main() { bringup_row vllm; }

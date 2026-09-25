# shellcheck shell=bash
# M41 (T09, T34; G06, U5-G1): SIGKILL the host agent after the engine is
# spawned and before its Initialize result: one engine; uncertainty settles on
# evidence (adopted after a fresh probe, or settled) and stop works.
#   run_row.sh M41 -- sb-4
row_main() {
  local a=${1:-sb-4} ha rc=0 i
  ha=$(fixture_host "$a")
  step before host_idle "$ha" || return 1
  step deploy deploy "$a" --activate || return 1
  # Wait (read-only) until an engine process exists on the host.
  for i in $(seq 1 60); do
    rsh "$ha" "pgrep -af '$ENGINE_PGREP' | grep -vE 'pgrep|tailscaled|bash -c' | grep -q ." && break
    sleep 1
  done
  echo "engine_seen $(now_ms) after ${i}s" >>"$EVID/marks.txt"
  step engines-before rsh "$ha" "ps -eo pid,ppid,lstart,args | grep -E 'sglang|vllm' | grep -vE 'grep|tailscaled|bash -c' | cut -c1-160"
  step state-before status_dep "$a"
  echo "agent_kill $(now_ms)" >>"$EVID/marks.txt"
  step agent-kill fault agent "$ha" KILL || return 1
  sleep 20
  step state-agent-down status_dep "$a"
  step agent-up "$MATRIX_DIR/roles.sh" host-up "$ha" || return 1
  step online "$MATRIX_DIR/roles.sh" wait-online 180 || rc=1
  step settled wait_state "$a" ready 900 || rc=1
  step state-after status_dep "$a"
  step engines-after rsh "$ha" "ps -eo pid,ppid,lstart,args | grep -E 'sglang|vllm' | grep -vE 'grep|tailscaled|bash -c' | cut -c1-160"
  step owned keep_owned "$a" after
  step evidence evidence "$a" after
  step infer infer "$a" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step stop stop_dep "$a" || rc=1
  step stopped wait_state "$a" stopped 600 || rc=1
  sleep 3
  step cleanup cleanup_check "$a" "$ha" "$EVID/owned-$a-after.json" "$EVID/accounting-$a-after.json" || rc=1
  step delete delete_dep "$a" || rc=1
  return "$rc"
}

# shellcheck shell=bash
# M35 (T27; G05): both hosts tight; host-a switches sa-14 -> sa-30 while host-b
# switches vb-30 -> sb-14, at the same time: independent per-host plans, no
# double charge, both targets served.
#   run_row.sh M35 -- sa-14 sa-30 vb-30 sb-14
Q="What is 17+25? Answer with only the number."
row_main() {
  local a1=${1:-sa-14} b1=${2:-sa-30} a2=${3:-vb-30} b2=${4:-sb-14} rc=0 d p1 p2
  [ "${POLICY_a:-}" = tight ] && [ "${POLICY_b:-}" = tight ] || { echo "both hosts must be tight"; return 1; }
  step before-a host_idle "$HOST_A" || return 1
  step before-b host_idle "$HOST_B" || return 1
  step variant-b1 variant "$b1" wk --document-json '{"timeouts": {"wake": "900s"}}' || return 1
  step deploy-a1 deploy "$a1" --activate || return 1
  step deploy-a2 deploy "$a2" --activate || return 1
  step ready-a1 wait_state "$a1" ready 1200 || return 1
  step ready-a2 wait_state "$a2" ready 1800 || return 1
  FIXTURE_VARIANT=wk step deploy-b1 deploy "$b1" || return 1
  step deploy-b2 deploy "$b2" || return 1
  step ledger-before python3 "$MATRIX_DIR/ledger.py" snapshot --db "$SERVER_DB"
  echo "requests $(now_ms)" >>"$EVID/marks.txt"
  python3 "$MATRIX_DIR/infer.py" --route "$b1-wk" --prompt "$Q" --expect 42 --max-tokens 1024 --timeout 1800 --out "$EVID/requests.jsonl" >"$EVID/req-b1.out" 2>&1 &
  p1=$!
  python3 "$MATRIX_DIR/infer.py" --route "$b2" --prompt "$Q" --expect 42 --max-tokens 1024 --timeout 1800 --out "$EVID/requests.jsonl" >"$EVID/req-b2.out" 2>&1 &
  p2=$!
  for i in 1 2 3 4 5 6; do sleep 20; python3 "$MATRIX_DIR/ledger.py" snapshot --db "$SERVER_DB" >"$EVID/ledger-during-$i.json"; done
  wait "$p1" || rc=1
  echo "served_b1 $(now_ms)" >>"$EVID/marks.txt"
  wait "$p2" || rc=1
  echo "served_b2 $(now_ms)" >>"$EVID/marks.txt"
  step req-b1 cat "$EVID/req-b1.out"
  step req-b2 cat "$EVID/req-b2.out"
  for d in "$a1" "$b1-wk" "$a2" "$b2"; do step "state-$d" status_dep "$d"; done
  step switch-log bash -c "grep -aE '\"event\":\"switch\"' '$LRD/server.log' | grep -viE 'key|secret|bearer|token' | tail -n 16"
  for d in "$a1" "$b1-wk" "$a2" "$b2"; do step "stop-$d" stop_dep "$d"; done
  for d in "$a1" "$b1-wk" "$a2" "$b2"; do step "stopped-$d" wait_state "$d" stopped 900 || rc=1; done
  sleep 3
  step clean-a host_idle "$HOST_A" || rc=1
  step clean-b host_idle "$HOST_B" || rc=1
  for d in "$a1" "$b1-wk" "$a2" "$b2"; do step "delete-$d" delete_dep "$d" || rc=1; done
  return "$rc"
}

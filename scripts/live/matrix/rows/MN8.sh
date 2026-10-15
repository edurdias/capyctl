# shellcheck shell=bash
# MN8 (design section 17.2, ADR 0028 §5; T34; SOLO's `start --evict`): the group and
# a single-rank deployment on host A alternate, each started with --evict:
#   run_row.sh MN8 --tag vllm -- vllm        (one engine, vllm or sglang; default vllm)
# The single-rank deployment is the run's <v|s>a-30 fixture (Qwen3-30B-A3B on host A
# alone) as the variant <v|s>a-30-mn8; it cannot fit beside the group's host A member,
# so each start has to evict the other. MN8_CYCLES (default 2) alternations. Setup as
# MN1 (rows/MN1.sh), plus the run's fixtures (roles.sh fixtures).
#
# Expected, every cycle:
#   single  `start deployment <single> --evict` names the group as its victim; the
#           whole group stops, on both hosts (every member and plan settled, no member
#           charge or worker lease, every identity gone, host B holds no engine at
#           all and host A none of the group's); the single-rank deployment reaches
#           Ready on host A and answers.
#   group   `start deployment <group> --evict` names the single-rank deployment; it
#           stops with verified cleanup on host A; the group is Ready again on both
#           hosts (a new plan generation) and answers.
# Then stop the group with verified cleanup on both hosts; delete both.
# shellcheck source=scripts/live/matrix/grouplib.sh
. "$MATRIX_DIR/grouplib.sh"

MN8_CYCLES=${MN8_CYCLES:-2}

# group_gone_check <deployment> <label> <ready-label>: the evicted group released
# on both hosts. Host A still runs the single-rank deployment, so it is judged by
# the group's identities and ports, host B by having no engine at all.
group_gone_check() {
  local dep=$1 label=$2 rl=$3 rc=0 pid ticks boot hid host probe
  if dry; then
    log_cmd control-host "for each identity in members-$dep-$rl.json and owned-$dep-$rl.json: ssh <its host> signal_owned.py --signal 0 (want absent)"
    host_clean "$HOST_B"
    x python3 "$MATRIX_DIR/ledger.py" group --db "$SERVER_DB" --deployment "$dep"
    return 0
  fi
  while read -r pid ticks boot hid; do
    host=$(host_of_id "$hid")
    [ -n "$host" ] || { echo "identity $pid on unknown host $hid"; rc=1; continue; }
    probe=$(rsh "$host" "python3 $REMOTE_TREE/scripts/live/matrix/signal_owned.py --pid $pid --ticks $ticks --boot $boot --signal 0" || true)
    echo "$host $pid $probe"
    case $probe in *'"outcome": "absent"'*) ;; *) rc=1 ;; esac
  done < <(python3 -c 'import json,sys
for path in sys.argv[1:]:
    for i in json.load(open(path)): print(i["pid"], i["start_ticks"], i["boot_id"], i.get("host_id") or "-")' \
    "$EVID/members-$dep-$rl.json" "$EVID/owned-$dep-$rl.json")
  host_clean "$HOST_B" | tee "$EVID/gone-host-b-$label.txt"
  grep -q LEFTOVER "$EVID/gone-host-b-$label.txt" && rc=1
  group_ledger "$dep" "$label"
  python3 -c 'import json,sys; h=json.load(open(sys.argv[1]))["held"]; sys.exit(1 if any(h.values()) else 0)' \
    "$EVID/group-$dep-$label.json" || { echo "FINDING: the evicted group still holds"; rc=1; }
  return "$rc"
}

row_main() {
  local engine=${1:-vllm} dep single c rc=0 host
  case $engine in vllm|sglang) ;; *) echo "MN8 alternates a vllm or sglang group with its single-rank fixture"; return 1 ;; esac
  dep=$(group_fixture_name "$engine")
  single=${engine:0:1}a-30-mn8
  echo "engine $engine group $dep single $single on $HOST_A cycles $MN8_CYCLES" | tee -a "$EVID/timeline.txt"
  step preflight group_preflight "$engine" || return 1
  for host in "${GROUP_HOSTS[@]}"; do step "idle-$(host_short "$host")" host_idle "$host" || return 1; done
  step fixture group_fixture "$engine" || return 1
  step variant variant "${engine:0:1}a-30" mn8 || return 1

  step deploy timed deploy deploy "$dep" --activate --wait || {
    step status-failed group_status "$dep" failed
    for host in "${GROUP_HOSTS[@]}"; do step "errors-$(host_short "$host")" engine_errors "$host"; done
    return 1
  }
  step ready keep_group "$dep" g1
  step ready-check group_ready_check "$dep" g1 || rc=1
  step infer infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  FIXTURE_VARIANT=mn8 step deploy-single deploy "${engine:0:1}a-30" || return 1

  for ((c = 1; c <= MN8_CYCLES; c++)); do
    step "evict-group-$c" timed "evict-group-$c" cli start deployment "$single" --evict --format json || rc=1
    step "group-stopped-$c" wait_states "$dep" stopped 900 || rc=1
    step "group-settled-$c" wait_group_settled "$dep" 300 || rc=1
    step "status-evicted-$c" group_status "$dep" "evicted-$c"
    step "single-ready-$c" wait_states "$single" ready 1800 || { rc=1; step "single-errors-$c" engine_errors "$HOST_A"; break; }
    sleep 3
    step "group-gone-$c" group_gone_check "$dep" "gone-$c" "g$c" || rc=1
    step "single-owned-$c" keep_owned "$single" "s$c"
    step "single-infer-$c" infer "$single" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
    step "ledger-single-$c" x python3 "$MATRIX_DIR/ledger.py" snapshot --db "$SERVER_DB"

    step "evict-single-$c" timed "evict-single-$c" cli start deployment "$dep" --evict --format json || rc=1
    step "single-stopped-$c" wait_states "$single" stopped 900 || rc=1
    step "group-ready-$c" wait_states "$dep" ready 1800 || { rc=1; for host in "${GROUP_HOSTS[@]}"; do step "group-errors-$c-$(host_short "$host")" engine_errors "$host"; done; break; }
    sleep 3
    step "single-cleanup-$c" cleanup_check_partial "$single" "$HOST_A" "$EVID/owned-$single-s$c.json" || rc=1
    step "group-ready-keep-$c" keep_group "$dep" "g$((c + 1))"
    step "group-ready-check-$c" group_ready_check "$dep" "g$((c + 1))" || rc=1
    step "group-infer-$c" infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  done

  group_stop_settle "$dep" stopped || rc=1
  step evidence evidence "$dep" stopped
  step journal journal_hosts "$dep" stopped
  sleep 3
  step cleanup group_cleanup_check "$dep" stopped "g$c" || rc=1
  step delete delete_dep "$dep" || rc=1
  step delete-single delete_dep "$single" || rc=1
  return "$rc"
}

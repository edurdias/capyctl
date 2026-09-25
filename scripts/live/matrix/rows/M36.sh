# shellcheck shell=bash
# M36 / M37 (T20, T33; W13): SIGKILL of the owned engine api process, per fixture:
#   run_row.sh M36 --tag sa-14 -- sa-14
#   run_row.sh M37 --tag vb-4 -- vb-4      (rows/M37.sh reuses this)
#
# Expected: the host reports the exit; dispatch closes within seconds (routed
# requests refused, never hung); status reads `failed`; the reservation stays until
# gone evidence, then an ordinary stop releases it with every identity absent and
# the host clean; the next request relaunches on demand (a new generation) and is
# answered correctly; stop with verified cleanup; delete.

dispatch_poll() { # dispatch_poll <dep> <seconds>: status timeline every 0.5 s
  local dep=$1 secs=$2 end
  end=$(( $(now_ms) + secs * 1000 ))
  while [ "$(now_ms)" -lt "$end" ]; do
    printf '%s ' "$(now_ms)"
    status_dep "$dep" 2>/dev/null | python3 -c 'import json,sys
d=json.load(sys.stdin); d=d.get("deployment",d)
print(json.dumps({k:d.get(k) for k in ("observed_state","admission_enabled","dispatch_enabled","generation","ready_instances","conditions")}, separators=(",",":")))' || echo err
    sleep 0.5
  done
}

kill_row() {
  local fix=$1 dep host rc=0
  dep=$fix; host=$(fixture_host "$fix")
  step before host_idle "$host" || return 1
  step deploy deploy "$fix" --activate --wait || { step errors engine_errors "$host"; return 1; }
  step owned-ready keep_owned "$dep" ready
  step infer infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  echo "kill $(now_ms)" >>"$EVID/marks.txt"
  step kill fault engine "$host" "$dep" KILL || return 1
  step poll dispatch_poll "$dep" 20
  step accounting-after-kill keep_owned "$dep" killed
  step evidence-killed evidence "$dep" killed
  # No request before settlement: a request would relaunch on demand (found in
  # M37's first run, which then confounded the gen1 cleanup check).
  step settled wait_state "$dep" failed 300 || rc=1
  step status-failed status_dep "$dep"
  sleep 5
  step cleanup-gen1 cleanup_check "$dep" "$host" "$EVID/owned-$dep-ready.json" "$EVID/accounting-$dep-ready.json" gen1 || rc=1
  step relaunch timed relaunch infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step owned-gen2 keep_owned "$dep" gen2
  step status-gen2 status_dep "$dep"
  step stop stop_dep "$dep" || rc=1
  step stopped wait_state "$dep" stopped 600 || rc=1
  sleep 3
  step cleanup-gen2 cleanup_check "$dep" "$host" "$EVID/owned-$dep-gen2.json" "$EVID/accounting-$dep-gen2.json" gen2 || rc=1
  step delete delete_dep "$dep" || rc=1
  return "$rc"
}

row_main() { kill_row "${1:?fixture}"; }

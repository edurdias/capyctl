# shellcheck shell=bash
# Tier 5 (D9, P1): one deployment with two instances spread across both hosts,
# served on the replica route `qwen3-14b`, covering M54, M56, M57, M58, M59, M60:
#   run_row.sh M54 --tag rep -- va-14
#
#   M54  20 sequential requests: both instances serve; every answer correct; I1
#        probes (several, balanced) all match the model's golden.
#   M56  64 short requests, 32 at a time (the route's in-flight bound is the
#        instances' max_concurrent_requests, 2 x 16; the first live run at 64
#        concurrent got 32 answers 413 queue_full, the bound working): neither
#        host above 60% of selections.
#   M57  instance 1 stopped; 8 long streams (4000-token prompt, 2500-token output)
#        land on instance 0; instance 1 started again; then 32 short requests:
#        selections record the load inputs (router in-flight, engine
#        running/waiting, sample age); short requests lean away from the host
#        holding the long work.
#   M58  under load, SIGSTOP the host-b host agent: within about 5 s the host
#        is suspended and new requests go only to host-a; requests accepted on
#        host-b are never replayed; host-b's reservation is retained.
#   M60  SIGCONT: host-b rejoins only after a fresh probe; traffic returns to it.
#   M59  under load, SIGSTOP the host-b engine (owned api process) for 10 s, then
#        SIGCONT: its samples go stale, new work steers to host-a; stalled requests
#        finish after SIGCONT or fail within the deadline; no replay. Selections
#        are counted before, during and after the stall window.
# Then stop, verified cleanup on both hosts, delete.

REP_ROUTE=qwen3-14b
SERVER_LOG_LINE=0

sel_mark() { SERVER_LOG_LINE=$(wc -l <"$LRD/server.log"); echo "server.log line $SERVER_LOG_LINE"; }
sel_count() { # sel_count <label> [selections.py args]
  local label=$1; shift
  cli list hosts --format json >"$EVID/hosts-$label.json" 2>/dev/null || true
  python3 "$MATRIX_DIR/selections.py" --log "$LRD/server.log" --from-line "$SERVER_LOG_LINE" \
    --hosts-json "$EVID/hosts-$label.json" "$@" | tee "$EVID/selections-$label.json"
}

both_ready() { # both_ready <dep> <timeout>
  local dep=$1 timeout=$2 i n
  dry && return 0
  for ((i = 0; i < timeout; i += 3)); do
    n=$(status_dep "$dep" 2>/dev/null | python3 -c 'import json,sys
d=json.load(sys.stdin); d=d.get("deployment",d)
print(sum(1 for x in d.get("instances",[]) if x.get("observed_state")=="ready"))' 2>/dev/null || echo 0)
    [ "$n" = 2 ] && { echo "2 instances ready after ${i}s"; return 0; }
    sleep 3
  done
  echo "not 2 ready after ${timeout}s (last $n)"; return 1
}

n_ready() { # n_ready <dep> <n> <timeout>: exactly n instances ready
  local dep=$1 want=$2 timeout=$3 i n
  dry && return 0
  for ((i = 0; i < timeout; i += 3)); do
    n=$(status_dep "$dep" 2>/dev/null | python3 -c 'import json,sys
d=json.load(sys.stdin); d=d.get("deployment",d)
print(sum(1 for x in d.get("instances",[]) if x.get("observed_state")=="ready"))' 2>/dev/null || echo 0)
    [ "$n" = "$want" ] && { echo "$want instances ready after ${i}s"; return 0; }
    sleep 3
  done
  echo "not $want ready after ${timeout}s (last $n)"; return 1
}

# Host session and suspension timeline, every 0.5 s, until the stop file appears.
host_poll_start() {
  HOSTPOLL_STOP=$EVID/hostpoll.stop
  rm -f "$HOSTPOLL_STOP"
  (while [ ! -f "$HOSTPOLL_STOP" ]; do
     printf '%s ' "$(now_ms)"
     "$MLLM" list hosts "$(cli_format_flag)" json --config "$SERVER_CFG" 2>/dev/null | python3 -c 'import json,sys
try:
    hs=json.load(sys.stdin).get("hosts",[])
    print(json.dumps({h["name"]:{k:h.get(k) for k in ("online","eligible","state","responsive","suspended","unresponsive","session")} for h in hs}, separators=(",",":")))
except Exception as e: print("err", e)'
     sleep 0.5
   done) >"$EVID/hostpoll-$1.txt" 2>&1 &
  HOSTPOLL_PID=$!
}
host_poll_stop() { touch "$HOSTPOLL_STOP"; wait "$HOSTPOLL_PID" 2>/dev/null || true; }

bg_load() { # bg_load <label> <loadgen args...>
  local label=$1; shift
  python3 "$MATRIX_DIR/loadgen.py" --route "$REP_ROUTE" --out "$EVID/load-$label.jsonl" --summary "$EVID/load-$label.summary.json" "$@" \
    >"$EVID/load-$label.out" 2>&1 &
  BG_LOAD=$!
}

m57_block() { # m57_block <dep>: the M57 skew and steering check
  local dep=$1 rc=0 M57_LONG
  # M57. The first live run (2026-09-23) split the long work 4/4 (the router
  # balances it too) and its 32-token outputs ended in about 13 s, so no skew
  # existed to steer away from. Now instance 1 is stopped, the long streams
  # (long prompt, long output) all land on instance 0, instance 1 is started
  # again, and only then do the short requests arrive: they should lean to the
  # host holding none of the long work.
  step m57-stop-1 cli stop instance "$dep/1" --format json || rc=1
  step m57-one-ready n_ready "$dep" 1 600 || rc=1
  sel_mark
  bg_load m57-long --long 8 --long-tokens 4000 --long-max-tokens "${M57_LONG_MAX_TOKENS:-2500}" --long-ignore-eos --skew-delay 0 --concurrency 8
  M57_LONG=$BG_LOAD
  sleep 20
  step m57-long-selections sel_count m57-long
  step m57-start-1 cli start instance "$dep/1" --format json || rc=1
  step m57-both-ready both_ready "$dep" 900 || rc=1
  sleep 3
  step m57-status status_dep "$dep"
  sel_mark
  step m57-short load --route "$REP_ROUTE" --short 32 --concurrency 8 --max-tokens 16 || rc=1
  step m57-selections sel_count m57
  wait "$M57_LONG" || rc=1
  step m57-long-summary cat "$EVID/load-m57-long.summary.json"
  return "$rc"
}

m59_block() { # m59_block <dep>: stall the host-b engine under load, count selections per window
  # The first rerun (2026-09-24) sent 512-token requests 16 at a time: with
  # every worker waiting on a long answer no request was dispatched during the
  # 10 s stall, so there was nothing to steer. Short answers keep the workers
  # cycling through the stall.
  local dep=$1 rc=0 l0 l1 l2 l3
  # M59: freeze the host-b engine for 10 s under load.
  sel_mark
  l0=$SERVER_LOG_LINE
  bg_load m59 --stream 16 --nonstream "${M59_REQUESTS:-400}" --concurrency 16 --max-tokens "${M59_MAX_TOKENS:-32}"
  sleep 6
  l1=$(wc -l <"$LRD/server.log")
  echo "engine_stop $(now_ms) server.log $l1" >>"$EVID/marks.txt"
  step m59-sigstop fault engine "$HOST_B" "$dep" STOP || rc=1
  sleep "${M59_STALL_S:-20}"
  l2=$(wc -l <"$LRD/server.log")
  echo "engine_cont $(now_ms) server.log $l2" >>"$EVID/marks.txt"
  step m59-sigcont fault engine "$HOST_B" "$dep" CONT || rc=1
  wait "$BG_LOAD" || rc=1
  l3=$(wc -l <"$LRD/server.log")
  step m59-load-summary cat "$EVID/load-m59.summary.json"
  step m59-selections sel_count m59
  # Windows (found live 2026-09-23: whole-run counts hid the steering).
  SERVER_LOG_LINE=$l0 step m59-before sel_count m59-before --to-line "$l1"
  SERVER_LOG_LINE=$l1 step m59-stall sel_count m59-stall --to-line "$l2"
  SERVER_LOG_LINE=$l2 step m59-after sel_count m59-after --to-line "$l3"
  step m59-accounting accounting "$dep"
  return "$rc"
}

row_main() {
  local fix=${1:-va-14} dep rc=0 i
  dep=$fix-rep
  step before-a host_idle "$HOST_A" || return 1
  step before-b host_idle "$HOST_B" || return 1
  step variant variant "$fix" rep --route "$REP_ROUTE" \
    --document-json '{"host": null, "instances": 2, "placement": {"strategy": "spread", "max_per_host": 1}}' || return 1
  dry || cp "$(FIXTURE_VARIANT=rep fixture_file "$fix")" "$EVID/fixture-rep.json"
  FIXTURE_VARIANT=rep step deploy deploy "$fix" --activate || return 1
  step ready both_ready "$dep" 1200 || { rc=1; step errors-a engine_errors "$HOST_A"; step errors-b engine_errors "$HOST_B"; return 1; }
  step status status_dep "$dep"
  step owned keep_owned "$dep" ready
  snap ready

  # M54
  sel_mark
  for i in $(seq 1 20); do
    step "m54-$i" infer "$REP_ROUTE" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  done
  step m54-selections sel_count m54 --min-hosts 2 || rc=1
  for i in 1 2 3 4 5 6; do step "m54-i1-$i" i1 "$REP_ROUTE" "$fix" "$dep" || rc=1; done

  # M56
  sel_mark
  step m56-load load --route "$REP_ROUTE" --nonstream 64 --concurrency 32 --max-tokens 256 || rc=1
  step m56-selections sel_count m56 --max-share 0.6 || rc=1

  m57_block "$dep" || rc=1
  # Instance 1 was restarted by M57: M58/M60 compare against its new processes.
  step owned-after-m57 keep_owned "$dep" ready

  # M58 + M60: freeze the host-b agent under load.
  sel_mark
  host_poll_start m58
  bg_load m58 --stream 16 --nonstream 112 --concurrency 16 --max-tokens 512
  sleep 6
  echo "agent_stop $(now_ms)" >>"$EVID/marks.txt"
  step m58-sigstop fault agent "$HOST_B" STOP || rc=1
  sleep 20
  step m58-accounting keep_owned "$dep" agent-stopped
  step m58-selections-frozen sel_count m58-frozen
  echo "agent_cont $(now_ms)" >>"$EVID/marks.txt"
  step m60-sigcont fault agent "$HOST_B" CONT || rc=1
  wait "$BG_LOAD" || rc=1
  sleep 10
  host_poll_stop
  step m58-load-summary cat "$EVID/load-m58.summary.json"
  sel_mark
  step m60-traffic load --route "$REP_ROUTE" --nonstream 24 --concurrency 8 --max-tokens 64 || rc=1
  step m60-selections sel_count m60 --min-hosts 2 || rc=1
  step m60-owned keep_owned "$dep" rejoined
  step m60-same same_identities "$EVID/owned-$dep-ready.json" "$EVID/owned-$dep-rejoined.json" || rc=1

  m59_block "$dep" || rc=1
  snap after-faults

  step stop stop_dep "$dep" || rc=1
  step stopped wait_state "$dep" stopped 900 || rc=1
  sleep 3
  step cleanup-ids cleanup_check_partial "$dep" "$HOST_A" "$EVID/owned-$dep-ready.json" || rc=1
  step clean-a host_idle "$HOST_A" || rc=1
  step clean-b host_idle "$HOST_B" || rc=1
  step delete delete_dep "$dep" || rc=1
  return "$rc"
}

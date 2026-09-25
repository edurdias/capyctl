# shellcheck shell=bash
# M80 (T40; SPEC section 17): performance benchmark through the shipped router, per fixture:
#   run_row.sh M80 --tag sa-14 -- sa-14                 (cold, bench, park phases)
#   run_row.sh M80 --tag sa-14 -- sa-14 sa-30          (adds switch sa-30 -> sa-14; needs a
#                                                          host policy under which the two do not co-fit)
#   M80_PHASES="bench" M80_REPEATS=5 run_row.sh M80 --tag vb-4 -- vb-4
#
# Setup: server and the fixture's host online; no other deployment active on that
# host (the phases scan the whole host). Run after the current live phase (owner,
# 2026-09-23). bench_report.py aggregates every M80-* directory into one table.
#
# Phases (M80_PHASES, default "cold bench park switch"; switch only with a second fixture):
#   cold    M80_LIFECYCLE_N times: deploy variant <fixture>-c<i> without activation
#           (on-demand eligible), one routed request, time to its first token (this
#           includes on-demand activation), then stop with verified cleanup and delete.
#   bench   deploy <fixture>-bn --activate --wait; then for each prompt length in
#           M80_PROMPTS and concurrency in M80_CONC: a warmup (excluded; calibrates the
#           synthetic prompt length and learns whether ignore_eos is accepted), an engine
#           /metrics scrape on loopback, the measured cell (M80_REPEATS requests per
#           worker), another scrape. SGLang serves /metrics without its key, so the
#           scrape delta gives the engine's own TTFT and e2e latency for the same
#           requests and the difference is the router + ingress + agent + network path.
#           vLLM keys /metrics and the harness never reads engine keys, so the scrape
#           gives vLLM no direct baseline; its periodic throughput log lines are kept.
#           For both engines the row also saves the server's mllm latency view
#           (`status deployment --format json`, field `latency`) before and after
#           each cell: router phases, host ingress times and the engine histograms
#           the host agent forwards (`source: engine`, vLLM included). `bench.py
#           report` windows it per cell and splits the path overhead (client ->
#           router -> ingress -> engine). A server without the view records `{}`.
#   park    (deep residency only) M80_LIFECYCLE_N times on <fixture>-bn: park, wait
#           parked, one routed request, time to first token (wake on demand).
#   switch  deploy <second>-sa --activate --wait and <fixture>-sw on demand; then
#           M80_LIFECYCLE_N times: request B (time to B's first token; A must yield),
#           record A's state, request A back (recorded as switchback). The first B is
#           a cold start of B; later ones wake a parked B when residency is deep.
#
# Evidence: target/live/matrix/M80-<tag>/ with cells/*.json, records.jsonl (per-chunk
# timestamps), metrics/*.prom, latency/*.json (mllm latency view per cell), lifecycle.jsonl, netfloor.json, pagecache.txt,
# switch-states.txt, marks.txt, E0 snapshots, cleanup checks, bench.json and summary.md.
# The API key is used by bench.py from MLLM_API_KEY only; no engine key is read.
#
# Helper copied rather than shared (merge into rowlib.sh later): m80_engine_throughput
# is engine_log_lines with a narrower credential filter, because engine_log_lines drops
# every line containing "token" and so every vLLM "tokens/s" throughput line.

M80_PROMPTS=${M80_PROMPTS:-"128 2048 8192"}
M80_CONC=${M80_CONC:-"1 4 16"}
M80_REPEATS=${M80_REPEATS:-3}
M80_WARMUP=${M80_WARMUP:-1}
M80_MAX_TOKENS=${M80_MAX_TOKENS:-256}
M80_LIFECYCLE_N=${M80_LIFECYCLE_N:-3}
M80_SEED=${M80_SEED:-8080}
M80_PHASES=${M80_PHASES:-"cold bench park switch"}
M80_WAKE_DOC='{"timeouts": {"wake": "900s"}}'
BENCH=$MATRIX_DIR/bench.py

m80_mark() { dry || echo "$1 $(now_ms)" >>"$EVID/marks.txt"; }
m80_has() { case " $M80_PHASES " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }

# Context length and residency of a fixture file (models.json in a dry run).
m80_field() { # m80_field <fixture-file> <context_length|residency>
  if dry || [ ! -f "$1" ]; then
    case $2 in context_length) echo 16384 ;; residency) echo deep ;; esac
    return 0
  fi
  python3 -c 'import json,sys
d=json.load(open(sys.argv[1]))
print(d["engine_config"]["context_length"] if sys.argv[2]=="context_length" else d.get("residency",""))' "$1" "$2"
}

m80_port() { # m80_port <accounting.json>
  dry && { echo "<port>"; return 0; }
  python3 -c 'import json,sys
try: print(json.load(open(sys.argv[1]))["endpoint_leases"][0]["port"])
except Exception: print("")' "$1"
}

# Engine /metrics on the host's loopback, no key sent; the HTTP code is appended.
m80_scrape() { # m80_scrape <host> <port> <file>
  local host=$1 port=$2 out=$3
  rsh "$host" "curl -s -m 10 -w '\n# HTTP %{http_code}\n' http://127.0.0.1:$port/metrics | grep -viE 'api_key|secret|bearer|password|authorization'" \
    >"$( dry && echo /dev/null || echo "$out")" || true
}

# The server's mllm latency view for one deployment, through the CLI (the
# harness never reads the admin token). Owner decision 2026-09-23.
m80_latency() { # m80_latency <deployment> <file>
  dry && return 0
  status_dep "$1" >"$2" 2>/dev/null || echo '{}' >"$2"
}

# Page-cache conditions (SPEC section 17): a reload from the OS cache is not a cold read.
m80_pagecache() { # m80_pagecache <host> <label>
  local line
  line=$(rsh_out "$1" "MemAvailable 0 Cached 0" "awk '/^(MemAvailable|Cached):/{printf \"%s %s \", \$1, \$2}' /proc/meminfo")
  dry || echo "$2 $(now_ms) $line" >>"$EVID/pagecache.txt"
  echo "$2 $line"
}

m80_engine_throughput() { # m80_engine_throughput <host>
  rsh "$1" "grep -rhaE 'Avg prompt throughput|Avg generation throughput|gen throughput|input throughput' $RRD/host --include='*.log' --include='*.txt' --include='*.err' --include='*.out' 2>/dev/null | grep -viE 'api_key|secret|bearer|password|credential|authorization' | tail -n 200; true"
}

m80_once() { # m80_once <deployment> <label>
  x python3 "$BENCH" once --route "$1" --label "$2" --timeout 1800 --seed "$M80_SEED" --out "$EVID/lifecycle.jsonl"
}

m80_state() { # m80_state <deployment>
  dry && { echo "<state>"; return 0; }
  status_dep "$1" 2>/dev/null | python3 -c 'import json,sys
d=json.load(sys.stdin); d=d.get("deployment",d); print(d.get("observed_state",""))'
}

m80_cold() {
  local fix=$1 host=$2 i tag dep rc=0
  for i in $(seq 1 "$M80_LIFECYCLE_N"); do
    tag=c$i; dep=$fix-$tag
    step "variant-$tag" variant "$fix" "$tag" --document-json "$M80_WAKE_DOC" || return 1
    step "idle-$tag" host_idle "$host" || return 1
    step "pagecache-$tag" m80_pagecache "$host" "cold-$i"
    FIXTURE_VARIANT=$tag step "deploy-$tag" deploy "$fix" || return 1
    m80_mark "cold-$i-request"
    step "cold-$i" m80_once "$dep" "cold-$i" || rc=1
    m80_mark "cold-$i-first-token"
    step "ready-$tag" wait_state "$dep" ready 900 || rc=1
    step "owned-$tag" keep_owned "$dep" ready
    step "evidence-$tag" evidence "$dep" cold
    step "stop-$tag" stop_dep "$dep" || rc=1
    step "stopped-$tag" wait_state "$dep" stopped 600 || rc=1
    sleep 3
    step "cleanup-$tag" cleanup_check "$dep" "$host" "$EVID/owned-$dep-ready.json" "$EVID/accounting-$dep-ready.json" "$tag" || rc=1
    step "delete-$tag" delete_dep "$dep" || rc=1
  done
  return "$rc"
}

m80_sweep() {
  local fix=$1 host=$2 dep=$3 ctx port L C cell rc=0
  ctx=$(m80_field "$EVID/fixture-bn.json" context_length)
  port=$(m80_port "$EVID/accounting-$dep-ready.json")
  [ -n "$port" ] || { echo "no endpoint lease recorded for $dep"; return 1; }
  for L in $M80_PROMPTS; do
    for C in $M80_CONC; do
      cell=L$L-C$C
      step "warm-$cell" x python3 "$BENCH" run --route "$dep" --prompt-tokens "$L" --concurrency "$C" \
        --warmup "$M80_WARMUP" --warmup-only --max-tokens "$M80_MAX_TOKENS" --context-length "$ctx" \
        --seed "$M80_SEED" --out "$EVID/cells/$cell.warmup.json" --records "$EVID/records.jsonl" || rc=1
      step "scrape-$cell-before" m80_scrape "$host" "$port" "$EVID/metrics/$cell.before.prom"
      step "latency-$cell-before" m80_latency "$dep" "$EVID/latency/$cell.before.json"
      m80_mark "bench-$cell-start"
      step "bench-$cell" x python3 "$BENCH" run --route "$dep" --prompt-tokens "$L" --concurrency "$C" \
        --repeats "$M80_REPEATS" --calibration "$EVID/cells/$cell.warmup.json" --max-tokens "$M80_MAX_TOKENS" \
        --context-length "$ctx" --seed "$M80_SEED" --out "$EVID/cells/$cell.json" --records "$EVID/records.jsonl" || rc=1
      m80_mark "bench-$cell-end"
      step "scrape-$cell-after" m80_scrape "$host" "$port" "$EVID/metrics/$cell.after.prom"
      step "latency-$cell-after" m80_latency "$dep" "$EVID/latency/$cell.after.json"
    done
  done
  return "$rc"
}

m80_park() {
  local dep=$1 host=$2 i rc=0
  for i in $(seq 1 "$M80_LIFECYCLE_N"); do
    m80_mark "park-$i-issued"
    step "park-$i" timed "park-$i" park_dep "$dep" || { rc=1; continue; }
    step "parked-$i" wait_state "$dep" parked 600 || { rc=1; step "park-errors-$i" engine_errors "$host"; continue; }
    m80_mark "park-$i-parked"
    sleep 3
    step "pagecache-wake-$i" m80_pagecache "$host" "wake-$i"
    m80_mark "wake-$i-request"
    step "wake-$i" m80_once "$dep" "wake-$i" || rc=1
    m80_mark "wake-$i-first-token"
    step "woken-$i" wait_state "$dep" ready 900 || rc=1
    step "evidence-wake-$i" evidence "$dep" "wake-$i"
    sleep 3
  done
  step "owned-woken" keep_owned "$dep" woken
  step "same-woken" same_identities "$EVID/owned-$dep-ready.json" "$EVID/owned-$dep-woken.json" || rc=1
  return "$rc"
}

m80_switch() {
  local fix=$1 afix=$2 host=$3 da db i rc=0
  da=$afix-sa; db=$fix-sw
  step variant-sa variant "$afix" sa --document-json "$M80_WAKE_DOC" || return 1
  step variant-sw variant "$fix" sw --document-json "$M80_WAKE_DOC" || return 1
  echo "policy $HOST_A=${POLICY_a:-unknown} $HOST_B=${POLICY_b:-unknown}" | tee -a "$EVID/timeline.txt"
  step idle-switch host_idle "$host" || return 1
  FIXTURE_VARIANT=sa step deploy-sa deploy "$afix" --activate --wait || return 1
  step owned-sa keep_owned "$da" ready
  FIXTURE_VARIANT=sw step deploy-sw deploy "$fix" || return 1
  for i in $(seq 1 "$M80_LIFECYCLE_N"); do
    step "pagecache-switch-$i" m80_pagecache "$host" "switch-$i"
    m80_mark "switch-$i-request"
    step "switch-$i" m80_once "$db" "switch-$i" || rc=1
    m80_mark "switch-$i-first-token"
    dry || echo "switch-$i incumbent $da $(m80_state "$da") target $db $(m80_state "$db")" >>"$EVID/switch-states.txt"
    [ "$i" = 1 ] && step owned-sw keep_owned "$db" ready
    step "evidence-switch-$i" evidence "$da" "switch-$i"
    m80_mark "switchback-$i-request"
    step "switchback-$i" m80_once "$da" "switchback-$i" || rc=1
    m80_mark "switchback-$i-first-token"
    dry || echo "switchback-$i incumbent $db $(m80_state "$db") target $da $(m80_state "$da")" >>"$EVID/switch-states.txt"
  done
  step switch-log bash -c "grep -aE 'switch' '$LRD/server.log' | grep -viE 'key|secret|bearer|token' | tail -n 60"
  for d in $da $db; do step "stop-$d" stop_dep "$d"; done
  for d in $da $db; do step "stopped-$d" wait_state "$d" stopped 600 || rc=1; done
  sleep 5
  step host-clean-switch host_idle "$host" || rc=1
  step acct-sa cleanup_check_partial "$da" "$host" "$EVID/owned-$da-ready.json" || rc=1
  step acct-sw cleanup_check_partial "$db" "$host" "$EVID/owned-$db-ready.json" || rc=1
  for d in $da $db; do step "delete-$d" delete_dep "$d" || rc=1; done
  return "$rc"
}

row_main() {
  local fix=${1:?fixture, e.g. sa-14} afix=${2:-} host dep residency rc=0
  host=$(fixture_host "$fix")
  dep=$fix-bn
  mkdir -p "$EVID/cells" "$EVID/metrics" "$EVID/latency"
  echo "fixture $fix engine $(fixture_engine "$fix") host $host phases '$M80_PHASES' prompts '$M80_PROMPTS' conc '$M80_CONC' repeats $M80_REPEATS lifecycle_n $M80_LIFECYCLE_N switch_from ${afix:-none}" \
    | tee -a "$EVID/timeline.txt"
  step before host_idle "$host" || return 1
  step netfloor x python3 "$BENCH" tcprtt --target "router=127.0.0.1:8443" --target "ingress=$(host_ip "$host"):$INGRESS_PORT" \
    --out "$EVID/netfloor.json"

  m80_has cold && { m80_cold "$fix" "$host" || rc=1; }

  if m80_has bench || m80_has park; then
    step variant-bn variant "$fix" bn --document-json "$M80_WAKE_DOC" || return 1
    dry || cp "$(FIXTURE_VARIANT=bn fixture_file "$fix")" "$EVID/fixture-bn.json"
    residency=$(m80_field "$EVID/fixture-bn.json" residency)
    step idle-bn host_idle "$host" || return 1
    step pagecache-bn m80_pagecache "$host" deploy-bn
    FIXTURE_VARIANT=bn step deploy-bn timed deploy-bn deploy "$fix" --activate --wait \
      || { step engine-errors engine_errors "$host"; return 1; }
    step owned-bn keep_owned "$dep" ready
    snap ready
    if m80_has bench; then
      m80_sweep "$fix" "$host" "$dep" || rc=1
      step engine-throughput m80_engine_throughput "$host"
      snap benched
    fi
    if m80_has park; then
      if [ "$residency" = deep ]; then
        m80_park "$dep" "$host" || rc=1
        snap parked-woken
      else
        echo "park phase skipped: residency $residency" | tee -a "$EVID/timeline.txt"
      fi
    fi
    step stop-bn stop_dep "$dep" || rc=1
    step stopped-bn wait_state "$dep" stopped 600 || rc=1
    sleep 3
    step cleanup-bn cleanup_check "$dep" "$host" "$EVID/owned-$dep-ready.json" "$EVID/accounting-$dep-ready.json" bn || rc=1
    step delete-bn delete_dep "$dep" || rc=1
  fi

  if m80_has switch && [ -n "$afix" ]; then
    [ "$(fixture_host "$afix")" = "$host" ] || { echo "switch fixture $afix is not on $host"; return 1; }
    m80_switch "$fix" "$afix" "$host" || rc=1
  fi

  step after host_idle "$host" || rc=1
  step report x python3 "$BENCH" report --evid "$EVID" --fixture "$fix"
  dry || cat "$EVID/summary.md"
  return "$rc"
}

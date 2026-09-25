# shellcheck shell=bash
# M16 (T08, T22; G17): model smoke, one fixture per invocation, e.g.
#   run_row.sh M16 --tag sa-14 -- sa-14
#   FIXTURE_VARIANT=v1 run_row.sh M16 --tag sa-27f-v1 -- sa-27f   (variant or rerun: deployment sa-27f-v1)
# Expected: Ready and correct, or a recorded engine incompatibility (not a pass).
#
# Evidence beyond E0: Ready time; the checkpoint digest row (first full hash,
# state and time); two fixed prompts and a stream; the I1 golden (greedy
# 32-token logprobs, or greedy text when the router returns none); MemAvailable
# and nvidia-smi samples every second on the host from before deploy to after
# stop (P2: peak drop against the declared request, and gen_budgets.py suggest);
# a short concurrent load; the engine /metrics surface on loopback and the
# roles' logs for the W8 load scrape; stop with verified cleanup (owned
# identities gone, no engine process, GPU clean, no charges, claims, leases or
# retained bindings) and the durable request_leases after traffic.
# m16_report.py folds all of it into summary.json.

M16_PROMPT_1="What is 17+25? Answer with only the number."
M16_PROMPT_2="Name the capital of France in one word."
M16_STREAM="Count from 1 to 5 separated by commas."
# Qwen3 checkpoints think before answering; leave room for it.
M16_MAX_TOKENS=1024

mark() { echo "$1 $(now_ms)" >>"$EVID/marks.txt"; }

# Host samples, one line per second: epoch ms, MemAvailable kB, compute apps (pid,MiB;...).
sampler_start() {
  local host=$1
  SAMPLER_STOP=$RRD/m16-$(basename "$EVID").stop
  rsh "$host" "rm -f $SAMPLER_STOP; for i in \$(seq 1 7200); do [ -f $SAMPLER_STOP ] && break; \
echo \"\$(date +%s%3N) \$(awk '/MemAvailable/{print \$2}' /proc/meminfo) \$(nvidia-smi --query-compute-apps=pid,used_memory --format=csv,noheader,nounits 2>/dev/null | tr -d ' ' | tr '\n' ';')\"; sleep 1; done" \
    >"$EVID/mem-samples.txt" 2>/dev/null &
  SAMPLER_PID=$!
}

sampler_stop() {
  local host=$1
  rsh "$host" "touch $SAMPLER_STOP"
  wait "$SAMPLER_PID" || true
  rsh "$host" "rm -f $SAMPLER_STOP"
}

# W8: the engine's own /metrics on loopback (SGLang exempts it from its key,
# vLLM keys it, so 401 is expected there) and what the roles logged about load.
metrics_probe() {
  local dep=$1 host=$2 port
  port=$(accounting "$dep" | python3 -c 'import json,sys; l=json.load(sys.stdin)["endpoint_leases"]; print(l[0]["port"] if l else "")')
  echo "engine port: ${port:-none}"
  if [ -n "$port" ]; then
    rsh "$host" "curl -s -m 5 -w '\nHTTP %{http_code}\n' http://127.0.0.1:$port/metrics \
| grep -E '^(sglang|vllm):(num_requests_running|num_requests_waiting|num_running_reqs|num_queue_reqs|token_usage|kv_cache_usage_perc|gpu_cache_usage_perc|prompt_tokens_total|generation_tokens_total)|^HTTP' | head -n 20"
  fi
  echo "## host.log load/metrics lines"
  rsh "$host" "grep -aciE 'metrics|scrape|load report|reportload' $RRD/host.log; grep -aiE 'metrics|scrape|load report|reportload' $RRD/host.log | grep -viE 'key|secret|bearer' | tail -n 5"
  echo "## server.log load/metrics lines"
  grep -aciE 'metrics|scrape|load report|reportload|load sample' "$LRD/server.log" || true
  grep -aiE 'metrics|scrape|load report|reportload|load sample' "$LRD/server.log" | grep -viE 'key|secret|bearer' | tail -n 5 || true
}

# accounting, engine_errors and cleanup_check live in rowlib.sh (shared with M38, M73, M74).

row_main() {
  local fix=${1:?fixture name, e.g. sa-14} dep host rc=0
  dep=$fix${FIXTURE_VARIANT:+-$FIXTURE_VARIANT}
  host=$(fixture_host "$fix")
  cp "$(fixture_file "$fix")" "$EVID/fixture.json"
  echo "fixture $fix variant ${FIXTURE_VARIANT:-none} deployment $dep host $host" | tee -a "$EVID/timeline.txt"
  sampler_start "$host"
  sleep 4
  mark deploy
  if step deploy deploy "$fix" --activate --wait; then
    mark ready
    step status status_dep "$dep"
    step owned owned "$dep"
    accounting "$dep" >"$EVID/accounting-ready.json"
    snap ready
    step infer1 infer "$dep" "$M16_PROMPT_1" --expect 42 --max-tokens "$M16_MAX_TOKENS" || rc=1
    step infer2 infer "$dep" "$M16_PROMPT_2" --expect Paris --max-tokens "$M16_MAX_TOKENS" || rc=1
    step stream infer "$dep" "$M16_STREAM" --stream --max-tokens "$M16_MAX_TOKENS" || rc=1
    step i1 i1 "$dep" "$fix" "$dep" || rc=1
    mark load_start
    step load load --route "$dep" --stream 8 --nonstream 8 --concurrency 8 || rc=1
    mark load_end
    snap loaded
    step metrics metrics_probe "$dep" "$host"
    sleep 2
    accounting "$dep" >"$EVID/accounting-loaded.json"
  else
    rc=1
    mark deploy_failed
    step status-failed status_dep "$dep"
    step inspect-failed inspect_dep "$dep"
    accounting "$dep" >"$EVID/accounting-failed.json" 2>&1 || true
    step engine-errors engine_errors "$host"
  fi
  mark stop
  step stop stop_dep "$dep" || rc=1
  step stopped wait_state "$dep" stopped 600 || rc=1
  mark stopped
  sleep 3
  step cleanup cleanup_check "$dep" "$host" || rc=1
  sampler_stop "$host"
  python3 "$MATRIX_DIR/m16_report.py" --evid "$EVID" --fixture "$dep" --goldens "$GOLDENS" | tee "$EVID/summary.json"
  return "$rc"
}

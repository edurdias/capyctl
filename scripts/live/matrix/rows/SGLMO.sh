# shellcheck shell=bash
# SGLMO (T21; 2026-09-24 fix): an SGLang deployment of the NVFP4 (modelopt)
# anchor with `residency: deep` is refused capability_missing:deep_park with a
# restart_only hint; the restart_only variant deploys, serves and stops clean.
#   run_row.sh SGLMO --tag sb-27f -- sb-27f
row_main() {
  local fix=${1:-sb-27f} host rc=0 dep
  host=$(fixture_host "$fix"); dep=$fix-ro
  step before host_idle "$host" || return 1
  step fixture-residency grep -nE 'residency|quantization|kv_cache_dtype' "$(fixture_file "$fix")"
  step deploy-deep refused deploy "$fix" --activate --wait || rc=1
  step deep-reason bash -c "cat '$EVID/03-deploy-deep.out' '$EVID/03-deploy-deep.err' | grep -iE 'deep_park|capability|restart_only|code|message'"
  step deep-status status_dep "$fix"
  step host-after-refusal host_idle "$host" || rc=1
  step variant-ro variant "$fix" ro --residency restart_only || return 1
  step fixture-ro grep -nE 'residency|quantization' "$(FIXTURE_VARIANT=ro fixture_file "$fix")"
  FIXTURE_VARIANT=ro step deploy-ro deploy "$fix" --activate --wait || rc=1
  step owned-ro keep_owned "$dep" ready
  step status-ro status_dep "$dep"
  step infer-ro infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 --timeout 600 || rc=1
  step park-ro park_dep "$dep"
  step stop-ro stop_dep "$dep" || rc=1
  step stopped-ro wait_state "$dep" stopped 600 || rc=1
  sleep 3
  step cleanup cleanup_check "$dep" "$host" "$EVID/owned-$dep-ready.json" "$EVID/accounting-$dep-ready.json" || rc=1
  step delete-ro delete_dep "$dep" || rc=1
  step delete-deep delete_dep "$fix" || true
  return "$rc"
}

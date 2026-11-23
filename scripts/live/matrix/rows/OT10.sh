# shellcheck shell=bash
# OT10 (ADR 0037 §7, launch-time check): an engine environment without the
# OpenTelemetry packages a traced engine imports runs without engine tracing
# and serves:
#   run_row.sh OT10 --no-e0 -- a
#
# The row determines the environment: it reads, by files only (otel_probe.py
# pkgs, the list of capyctl-agent tracing_install.rs), the approved SGLang
# 0.5.22 and vLLM 0.32.0 environments, then OT_NOSDK_ENV when hosts.local.env
# names one (an owner-authorized environment of vLLM 0.31.0+ or SGLang 0.5.22+,
# OT_NOSDK_ENGINE vllm or sglang, relative to the home directory), and takes the
# first that lacks a package. When every candidate holds them all, the row
# records that and stops: the precondition is missing, not the behaviour.
#
# Expected:
#   a  pkgs-*.json: what each candidate lacks.
#   b  the chosen environment registered as `ot10`; a Qwen3-4B deployment on it,
#      under a role with engine tracing on, starts with `engine tracing: off
#      (not_installed)` in status and answers.
#   c  procs-ot10.json: no tracing option or variable on the engine;
#      spans-ot10.json: CapyCTL's spans for its requests and no engine span.
# shellcheck source=scripts/live/matrix/otel_rows.sh
. "$MATRIX_DIR/otel_rows.sh"

OT10_KIND=
OT10_VENV=

# ot10_choose <host>: the first candidate environment that lacks a package.
ot10_choose() {
  local host=$1 cand kind venv out
  local cands=("sglang:$OT_SGLANG_VENV" "vllm:$OT_VLLM_VENV")
  [ -n "${OT_NOSDK_ENV:-}" ] && cands+=("${OT_NOSDK_ENGINE:-sglang}:$REMOTE_HOME/$OT_NOSDK_ENV")
  for cand in "${cands[@]}"; do
    kind=${cand%%:*} venv=${cand#*:}
    out=$(rsh_out "$host" '{"complete": false, "missing": ["(dry run)"]}' \
      "python3 $OT_PROBE pkgs --venv $venv | tee $OT_HEV/pkgs-$(basename "$venv").json; \
$venv/bin/python3 -c 'import importlib.metadata as m; print(m.version(\"$kind\"))' 2>/dev/null | sed 's/^/version: /'") || return 1
    printf '%s %s\n%s\n' "$kind" "$venv" "$out"
    if printf '%s\n' "$out" | grep '^{' | python3 -c 'import json,sys; sys.exit(0 if not json.load(sys.stdin)["complete"] else 1)'; then
      OT10_KIND=$kind OT10_VENV=$venv
      echo "chosen: $kind $venv"
      return 0
    fi
  done
  echo "PRECONDITION: every candidate environment holds the OpenTelemetry packages; name one without them in OT_NOSDK_ENV" >&2
  return 1
}

row_main() {
  local host rc=0 d=ot10-dep
  host=$(resolve_host "${1:-a}") || return 1
  ot_begin "$host" vllm sglang || return 1
  if ! step choose ot10_choose "$host"; then
    ot_end "$host"
    return 1
  fi
  step collector-start otelcol_start "$host" ot10 || rc=1
  step start ot_start "$host" -- "${OT_SET_ON[@]}" "${OT_SET_ENGINES[@]}" || rc=1
  step add-ot10 ot_add "$host" "$OT10_VENV" ot10 || rc=1
  step engine-list ot_cli "$host" engine list --format json || rc=1
  OT_DOC_PROFILE=ot10 step doc ot_doc "$d" "$OT10_KIND" || rc=1
  step deploy ot_deploy "$host" "$d" || rc=1
  step status ot_status "$host" "$d" || rc=1
  step procs ot_procs "$host" ot10 || rc=1
  step collected ot_req "$host" --route "$d" --traceparent sampled --label ot10-collected --expect 42 || rc=1
  step stream ot_req "$host" --route "$d" --stream --traceparent sampled --label ot10-stream --expect 42 || rc=1
  step export-wait ot_export_wait
  step delete ot_delete "$host" "$d" || rc=1
  ot_end "$host" || rc=1
  step spans ot_summary ot10 || rc=1
  return "$rc"
}

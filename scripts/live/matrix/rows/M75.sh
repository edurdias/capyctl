# shellcheck shell=bash
# M75 (T02): a host that declares no engine does not boot, on each Spark:
#   run_row.sh M75 --no-e0
# Replaces the removed in-process scenario live_vllm.rs L10 (owner decision
# 2026-09-22). The other half of L10, that the shipped release binary carries
# no test engine, is scripts/check-release-clean.sh, which `sync.sh build` runs
# against the control-host build and on each Spark after every build.
#
# The snapshot-built release binary starts standalone in a fresh private state
# directory under the run root, with every engine variable removed from its
# environment. Expected (SPEC section 8): it refuses at once, exit non-zero,
# error code `invalid_config` naming "no engine installation"; nothing listens,
# no engine process is spawned. The row needs no server and touches no running
# role; the state directory is removed afterwards.

M75_ENV_UNSET="-u MLLM_VLLM_BIN -u MLLM_SGLANG_BIN -u MLLM_MODELS_ROOT -u MLLM_ENGINE_FINGERPRINT -u MLLM_ENGINE_PATH"

no_engine_boot() { # no_engine_boot <host>
  local host=$1 dir=$RRD/m75-standalone
  # The refusal is the expected outcome, so the exit status is printed rather
  # than propagated; timeout bounds a boot that wrongly succeeds.
  rsh "$host" "rm -rf $dir && mkdir -m 700 $dir && \
env $M75_ENV_UNSET MLLM_STATE_DIR=$dir timeout --signal=TERM --kill-after=10 60 $RBIN start standalone --output json 2>&1; \
echo \"exit=\$?\"; rm -rf $dir" | tee "$( dry && echo /dev/null || echo "$EVID/no-engine-$host.txt")"
  dry && return 0
  python3 - "$EVID/no-engine-$host.txt" <<'PY'
import sys
text = open(sys.argv[1]).read()
exit_line = [l for l in text.splitlines() if l.startswith("exit=")]
code = int(exit_line[-1][5:]) if exit_line else None
ok = code not in (None, 0, 124, 137) and "invalid_config" in text and "no engine installation" in text.lower()
print(f"exit={code} refused={'yes' if ok else 'NO'}")
sys.exit(0 if ok else 1)
PY
}

row_main() {
  local host rc=0
  for host in "${MATRIX_HOSTS[@]}"; do
    step "no-engine-$host" no_engine_boot "$host" || rc=1
    step "idle-$host" host_idle "$host" || rc=1
  done
  return "$rc"
}

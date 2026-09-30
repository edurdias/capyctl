# shellcheck shell=bash
# M38 (T20, T30): launch failures close, then the host still serves, per fixture:
#   run_row.sh M38 --tag va-4 -- va-4
#   run_row.sh M38 --tag sa-4 -- sa-4
# Replaces the removed in-process suite live_vllm.rs L6, L7 and L8 (owner
# decision 2026-09-22), driven through the shipped CLI against the host role.
#
# Setup: server and the fixture's host enrolled and online; no other deployment
# active on that host (the leftover scan covers the whole host).
#
#   L6  bad model path: variant <fixture>-empty whose local source is an empty
#       directory the row creates under the host's model store (a path that
#       exists and holds no checkpoint; a missing path is refused before any
#       launch). Its content_fingerprint is a non-canonical placeholder so the
#       checkpoint check does not stand in for the engine. `deploy --activate
#       --wait` must fail; the deployment must read closed, never uncertain:
#       admission shut, no binding, charge, claim or lease, the failed operation
#       recorded, no engine or GPU process on the host. The refusal may come from
#       the checkpoint digest (WE3) or from the engine; which one is evidence.
#   L8  an engine that exits at once: variant <fixture>-exit whose extra
#       arguments carry an option the installed engine's own parser rejects, so
#       the real engine entry exits during startup. Same closure. This replaces
#       the old `/bin/false` executable, which needs a host document naming a
#       non-engine; the host's installation is never changed.
#   L7  recovery: variant <fixture>-rec, the sound recipe, on the same host and
#       server afterwards: Ready, a correct routed answer, stop with verified
#       cleanup.
# Every variant is deleted at the end and the empty directory removed.

M38_EMPTY_DIR=""

deploy_as() { local tag=$1; shift; FIXTURE_VARIANT=$tag deploy "$@"; }

empty_dir_create() { # empty_dir_create <host>
  M38_EMPTY_DIR=$MODELS_ROOT/capyctl-matrix-empty-$RUN
  rsh "$1" "mkdir -p -m 700 $M38_EMPTY_DIR && [ -z \"\$(ls -A $M38_EMPTY_DIR)\" ] && ls -lad $M38_EMPTY_DIR"
}

empty_dir_remove() { # empty_dir_remove <host>
  [ -n "$M38_EMPTY_DIR" ] || return 0
  rsh "$1" "rmdir $M38_EMPTY_DIR"
}

# One failure shape: deploy must fail, then the deployment must read closed.
failure_shape() { # failure_shape <fixture> <tag> <host>
  local fix=$1 tag=$2 host=$3 dep=$1-$2 rc=0
  step "$tag-deploy" refused deploy_as "$tag" "$fix" --activate --wait || rc=1
  step "$tag-status" status_dep "$dep"
  step "$tag-inspect" inspect_dep "$dep"
  step "$tag-closed" closure_check "$dep" "$host" "$tag" || rc=1
  step "$tag-errors" engine_errors "$host"
  step "$tag-delete" delete_dep "$dep" || rc=1
  return "$rc"
}

row_main() {
  local fix=${1:?fixture, e.g. va-4} host rc=0
  host=$(fixture_host "$fix")
  echo "fixture $fix host $host" | tee -a "$EVID/timeline.txt"
  step before host_idle "$host" || return 1

  # L6
  step empty-dir empty_dir_create "$host" || return 1
  step variant-empty variant "$fix" empty --document-json \
    "{\"model\": {\"source\": {\"type\": \"local\", \"path\": \"$M38_EMPTY_DIR\"}, \"content_fingerprint\": \"sha256:capyctl-matrix-empty\"}}" || rc=1
  failure_shape "$fix" empty "$host" || rc=1
  step empty-dir-remove empty_dir_remove "$host" || rc=1

  # L8
  step variant-exit variant "$fix" exit \
    --engine-config-json '{"accept_extra_args": true, "extra_args": ["--matrix-exits-at-once"]}' || rc=1
  failure_shape "$fix" exit "$host" || rc=1

  # L7
  step variant-rec variant "$fix" rec || return 1
  step rec-deploy deploy_as rec "$fix" --activate --wait || { step rec-errors engine_errors "$host"; return 1; }
  step rec-owned owned "$fix-rec"
  dry || accounting "$fix-rec" >"$EVID/accounting-ready.json"
  step rec-infer infer "$fix-rec" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step rec-stop stop_dep "$fix-rec" || rc=1
  step rec-stopped wait_state "$fix-rec" stopped 600 || rc=1
  sleep 3
  step rec-cleanup cleanup_check "$fix-rec" "$host" || rc=1
  step rec-delete delete_dep "$fix-rec" || rc=1
  return "$rc"
}

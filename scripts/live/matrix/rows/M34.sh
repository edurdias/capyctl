# shellcheck shell=bash
# M34 (T21): refusals around parking.
#   run_row.sh M34
#
#   a  restart_only park, both engines on host-a (variants va-4-rs, sa-4-rs):
#      Ready; `park deployment` is refused or settles `unchanged`; the deployment
#      stays Ready and serving with the same processes; no sleep or release call.
#   b  host_backed residency on the unified domain: `validate config` against the
#      host-a host document refuses it offline, and `deploy` refuses it before
#      any effect (nothing recorded).
#   c  deep on a host that opted out: host-b's host document is regenerated with
#      `security.deep_park: disabled` on both profiles and the host restarted; a deep
#      fixture (sb-4) is refused at resolution; a restart_only fixture (sb-4-rs)
#      still resolves. The normal document is restored and the host restarted.

rs_case() { # rs_case <fixture>
  local fix=$1 dep=$1-rs host rc=0
  host=$(fixture_host "$fix")
  step "variant-$dep" variant "$fix" rs --residency restart_only || return 1
  FIXTURE_VARIANT=rs step "deploy-$dep" deploy "$fix" --activate --wait || return 1
  step "owned-$dep" keep_owned "$dep" ready
  step "status-$dep" status_dep "$dep"
  step "park-$dep" park_dep "$dep"
  sleep 10
  step "after-park-$dep" wait_state "$dep" ready 60 || rc=1
  step "owned-$dep-after" keep_owned "$dep" after
  step "same-$dep" same_identities "$EVID/owned-$dep-ready.json" "$EVID/owned-$dep-after.json" || rc=1
  step "infer-$dep" infer "$dep" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step "evidence-$dep" evidence "$dep" park
  step "stop-$dep" stop_dep "$dep" || rc=1
  step "stopped-$dep" wait_state "$dep" stopped 600 || rc=1
  sleep 3
  step "cleanup-$dep" cleanup_check "$dep" "$host" "$EVID/owned-$dep-ready.json" "$EVID/accounting-$dep-ready.json" "$dep" || rc=1
  step "delete-$dep" delete_dep "$dep" || rc=1
  return "$rc"
}

host_backed_case() {
  local rc=0
  step variant-hb variant sa-4 hb --document-json '{"residency": "host_backed", "host": null}' || return 1
  step validate-hb refused cli validate config --file "$(FIXTURE_VARIANT=hb fixture_file sa-4)" --host "$RUNSTATE/host-$HOST_A-${POLICY_a:-normal}.yaml" || rc=1
  FIXTURE_VARIANT=hb step deploy-hb refused deploy sa-4 || rc=1
  step absent-hb refused status_dep sa-4-hb || rc=1
  return "$rc"
}

optout_doc() { # optout_doc <host>: regenerate the normal doc with deep_park disabled and upload it
  local host=$1 doc=$RUNSTATE/host-$1-optout.yaml
  x python3 - "$RUNSTATE/host-$host-normal.yaml" "$doc" <<'PY'
import json, os, sys
d = json.load(open(sys.argv[1]))
for p in d["runtime_profiles"].values():
    p["security"]["deep_park"] = "disabled"
fd = os.open(sys.argv[2], os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
with os.fdopen(fd, "w") as h:
    json.dump(d, h, indent=1)
PY
  rcopy "$doc" "$host:$RRD/host.yaml"
  rsh "$host" "chmod 600 $RRD/host.yaml"
}

optout_case() {
  local host=$HOST_B rc=0
  step optout-idle host_idle "$host" || return 1
  step optout-doc optout_doc "$host" || return 1
  step optout-host-down "$MATRIX_DIR/roles.sh" host-down "$host" || return 1
  step optout-host-up "$MATRIX_DIR/roles.sh" host-up "$host" || return 1
  step optout-online "$MATRIX_DIR/roles.sh" wait-online 180 || rc=1
  step optout-hosts cli list hosts --output json
  step optout-deep refused deploy sb-4 --activate --wait || rc=1
  step optout-deep-status status_dep sb-4
  step optout-deep-delete delete_dep sb-4
  step variant-17rs variant sb-4 rs --residency restart_only || rc=1
  FIXTURE_VARIANT=rs step optout-rs deploy sb-4 || rc=1
  step optout-rs-status status_dep sb-4-rs
  step optout-rs-delete delete_dep sb-4-rs
  # Restore the normal document.
  step restore-doc "$MATRIX_DIR/roles.sh" host-doc "$host" normal || rc=1
  step restore-host-down "$MATRIX_DIR/roles.sh" host-down "$host" || rc=1
  step restore-host-up "$MATRIX_DIR/roles.sh" host-up "$host" || rc=1
  step restore-online "$MATRIX_DIR/roles.sh" wait-online 180 || rc=1
  return "$rc"
}

row_main() {
  local rc=0
  step before host_idle "$HOST_A" || return 1
  rs_case va-4 || rc=1
  rs_case sa-4 || rc=1
  host_backed_case || rc=1
  optout_case || rc=1
  return "$rc"
}

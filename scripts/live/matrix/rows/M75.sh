# shellcheck shell=bash
# M75 (T02): a role that declares no engine boots with none and places nothing, on each host
#   run_row.sh M75 --no-e0
# Replaces the removed in-process scenario live_vllm.rs L10 (owner decision
# 2026-09-22). The other half of L10, that the shipped release binary carries
# no test engine, is scripts/check-release-clean.sh, which `sync.sh build` runs
# against the control-host build and on each host after every build.
#
# The snapshot-built release binary starts standalone in a fresh private state
# directory under the run root, with every engine variable removed from its
# environment and its config home inside that directory, so no engines.yaml is
# read. Its listeners are loopback ports of the row's own. Expected (SPEC
# section 8, amended 2026-10-01): it boots and its JSON ready line lists no
# profile; `engine list` reaches it and reports no profile; it spawns no engine
# child; SIGTERM stops it with exit 0 within 30 s. A second boot in text mode
# shows the banner naming `capyctl engine add`, since there is no deployment
# whose status could. The row needs no server and touches no running role; the
# state directory is removed afterwards.

M75_ENV_UNSET="-u CAPYCTL_VLLM_BIN -u CAPYCTL_SGLANG_BIN -u CAPYCTL_TENSORFOLD_BIN -u CAPYCTL_MODELS_ROOT -u CAPYCTL_ENGINE_FINGERPRINT -u CAPYCTL_ENGINE_PATH"
M75_LISTEN="--listen 127.0.0.1:18475 --management-listen 127.0.0.1:17475"

no_engine_boot() { # no_engine_boot <host>
  local host=$1 dir=$RRD/m75-standalone
  local role="env $M75_ENV_UNSET CAPYCTL_STATE_DIR=$dir XDG_CONFIG_HOME=$dir/config $RBIN"
  # stop_role <label>: SIGTERM, then at most 30 s for the exit; the status is
  # printed rather than propagated, and a role still running is killed (137).
  rsh "$host" "stop_role() { kill -TERM \$pid; (sleep 30; kill -KILL \$pid) >/dev/null 2>&1 & local dog=\$!; \
wait \$pid; local rc=\$?; kill \$dog 2>/dev/null; echo \"\$1 exit=\$rc\"; }; \
rm -rf $dir && mkdir -m 700 $dir && mkdir -m 700 $dir/models || exit 1; \
$role start standalone --output json $M75_LISTEN --models-root $dir/models >$dir/role.out 2>&1 & pid=\$!; \
for _ in \$(seq 60); do grep -q '\"ready\"' $dir/role.out && break; sleep 1; done; \
echo '--- ready'; cat $dir/role.out; \
echo '--- engine list'; $role engine list --output json; \
echo '--- children'; ps -o pid=,args= --ppid \$pid; \
echo '--- stop'; stop_role json; \
$role start standalone --format text $M75_LISTEN --models-root $dir/models >$dir/banner.out 2>&1 & pid=\$!; \
for _ in \$(seq 60); do grep -q 'standalone ready' $dir/banner.out && break; sleep 1; done; \
echo '--- banner'; cat $dir/banner.out; \
echo '--- stop text'; stop_role text; \
rm -rf $dir" | tee "$( dry && echo /dev/null || echo "$EVID/no-engine-$host.txt")"
  dry && return 0
  python3 - "$EVID/no-engine-$host.txt" <<'PY'
import json, sys
sections, name = {}, None
for line in open(sys.argv[1]).read().splitlines():
    if line.startswith("--- "):
        name = line[4:]
        sections[name] = []
    elif name:
        sections[name].append(line)
def objects(section):
    found = []
    for line in sections.get(section, []):
        try:
            found.append(json.loads(line))
        except ValueError:
            pass
    return found
ready = [o for o in objects("ready") if o.get("ready") is True]
listed = [o for o in objects("engine list") if "engines" in o]
children = [l for l in sections.get("children", []) if l.strip() and "nvidia-smi" not in l]
stops = [l for k in ("stop", "stop text") for l in sections.get(k, []) if " exit=" in l]
banner = "\n".join(sections.get("banner", []))
checks = {
    "ready_no_profiles": bool(ready) and ready[0].get("profiles") == [],
    "engine_list_empty": bool(listed) and listed[0].get("agent") == "reachable" and listed[0]["engines"] == [],
    "no_engine_child": not children,
    "banner_names_engine_add": "capyctl engine add" in banner,
    "exits_in_bound": len(stops) == 2 and all(l.endswith(" exit=0") for l in stops),
}
print(" ".join(f"{k}={'yes' if v else 'NO'}" for k, v in checks.items()))
sys.exit(0 if all(checks.values()) else 1)
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

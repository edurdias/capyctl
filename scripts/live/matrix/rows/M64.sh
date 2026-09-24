# shellcheck shell=bash
# M64 (T16; G10) plus `drain host` (SPEC §4.3, W11):
#   run_row.sh M64 -- s92-4 v17-4
#
#   delete  s92-4 Ready: plain `delete deployment` is refused (409
#           delete_requires_cleanup) with nothing changed; `delete deployment --stop`
#           stops, proves absence, then removes the route (gone from /v1/models, 404
#           on inference) and the name can be deployed again; checkpoint files are
#           untouched.
#   drain   v17-4 Ready: `drain host host-b --wait` stops it with verified
#           cleanup and leaves it eligible: the next request reactivates it on
#           demand; then stop with verified cleanup; delete.

models_list() {
  python3 - <<'PY'
import http.client, json, os
c = http.client.HTTPConnection("127.0.0.1", 8443, timeout=15)
c.request("GET", "/v1/models", headers={"Authorization": "Bearer " + os.environ["MLLM_API_KEY"]})
r = c.getresponse(); body = r.read()
print(json.dumps({"status": r.status, "ids": [m["id"] for m in json.loads(body).get("data", [])]}))
PY
}

model_dir_digest() { # model_dir_digest <host> <dir>: size+mtime listing digest (cheap, unchanged-files check)
  rsh "$1" "cd $MODELS_ROOT/$2 && find . -type f -printf '%p %s %T@\n' | LC_ALL=C sort | sha256sum | cut -c1-64"
}

row_main() {
  local a=${1:-s92-4} b=${2:-v17-4} ha hb rc=0 aid=
  ha=$(fixture_host "$a"); hb=$(fixture_host "$b")
  step before-a host_idle "$ha" || return 1
  step before-b host_idle "$hb" || return 1
  step files-before model_dir_digest "$ha" qwen3-4b-instruct
  step deploy-a deploy "$a" --activate --wait || return 1
  step owned-a keep_owned "$a" ready
  # The id, not the name, judges cleanup after the delete removes the name
  # (found live 2026-09-24: the name lookup failed "no deployment").
  dry || aid=$(deployment_id "$EVID/accounting-$a-ready.json")
  step infer-a infer "$a" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step delete-plain refused delete_dep "$a" || rc=1
  step after-plain wait_state "$a" ready 10 || rc=1
  step delete-stop timed delete-stop cli delete deployment "$a" --stop --output json || rc=1
  sleep 3
  step cleanup-a cleanup_check_deleted "$aid" "$ha" "$EVID/owned-$a-ready.json" || rc=1
  step host-a host_idle "$ha" || rc=1
  step models-after models_list
  step gone refused infer "$a" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 16 --timeout 30 || rc=1
  step status-gone refused status_dep "$a" || rc=1
  step files-after model_dir_digest "$ha" qwen3-4b-instruct
  step redeploy deploy "$a" || rc=1
  step redeploy-delete delete_dep "$a" || rc=1

  step deploy-b deploy "$b" --activate --wait || return 1
  step owned-b keep_owned "$b" ready
  step drain timed drain cli drain host "$hb" --wait --output json || rc=1
  step drained wait_state "$b" stopped 300 || rc=1
  sleep 3
  step cleanup-b cleanup_check "$b" "$hb" "$EVID/owned-$b-ready.json" "$EVID/accounting-$b-ready.json" drained || rc=1
  step hosts-after-drain cli list hosts --output json
  step on-demand timed on-demand infer "$b" "What is 17+25? Answer with only the number." --expect 42 --max-tokens 1024 || rc=1
  step owned-b2 keep_owned "$b" gen2
  step stop-b stop_dep "$b" || rc=1
  step stopped-b wait_state "$b" stopped 600 || rc=1
  sleep 3
  step cleanup-b2 cleanup_check "$b" "$hb" "$EVID/owned-$b-gen2.json" "$EVID/accounting-$b-gen2.json" gen2 || rc=1
  step delete-b delete_dep "$b" || rc=1
  return "$rc"
}

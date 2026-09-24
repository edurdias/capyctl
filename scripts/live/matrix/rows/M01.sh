# shellcheck shell=bash
# M01 (T08, T37): server + host-a enrolled; deploy s92-4 --activate --wait;
# one completion. Expected: Ready; routed 200 with the correct answer; one owned
# process group. Evidence beyond E0: ownership record, ingress scope and
# generation, I1. Leaves s92-4 stopped with verified cleanup.

row_main() {
  local dep=s92-4
  step deploy deploy "$dep" --activate --wait || return 1
  step status status_dep "$dep"
  step inspect inspect_dep "$dep"
  step owned owned "$dep"
  step infer infer "$dep" "What is 17+25? Answer with only the number." --expect 42 || return 1
  step i1 i1 "$dep" "$dep" || return 1
  snap ready
  step stop stop_dep "$dep" || return 1
  step stopped wait_state "$dep" stopped 300 || return 1
}

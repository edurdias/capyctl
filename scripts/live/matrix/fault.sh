#!/usr/bin/env bash
# Fault helpers for matrix rows (decision D6; plan unit W2).
#
#   fault.sh engine <host> <deployment> <SIG> [role]   signal an owned engine process (default role api)
#   fault.sh engine-group <host> <deployment> <SIG>     signal the owned api process's group (it must lead it)
#   fault.sh agent <host> <SIG>                         signal the host role started by roles.sh host-up
#   fault.sh server <SIG>                               signal the server role started by roles.sh server-up
#   fault.sh memhog-start <host> <GiB> <seconds>        one bounded external allocation (<= 40 GiB, <= 1800 s)
#   fault.sh memhog-stop <host>                         end it early
#   fault.sh memhog-during <host> <GiB> <seconds> -- <command...>
#                                                        run a command while the allocation is held;
#                                                        a trap ends the allocation on any exit
#
# Signals go only to identities taken from ownership evidence: the server's
# recorded launch identities (pid, start ticks, boot id) or the pid files the
# harness wrote when it started a role or the allocator. Nothing is signalled by
# name, and a reused pid is refused (signal_owned.py). No firewall, interface or
# reboot change is ever made. DRY_RUN=1 prints the plan.
. "$(dirname "$0")/lib.sh"

MEMHOG_MAX_GIB=40
MEMHOG_MAX_SECONDS=1800
SIGNAL_OWNED=$REMOTE_TREE/scripts/live/matrix/signal_owned.py
OUT=${EVID:-$LIVE/faults}

record() { dry && return 0; mkdir -p "$OUT"; printf '%s %s\n' "$(now)" "$*" >>"$OUT/faults.log"; }

engine() { # engine <host> <deployment> <SIG> <role> [--group]
  local host=$1 dep=$2 sig=$3 role=${4:-api} group=${5:-} ids hid pid ticks boot result
  hid=$(host_id "$host")
  if dry; then
    x python3 "$MATRIX_DIR/ledger.py" owned --db "$SERVER_DB" --deployment "$dep" --role "$role"
    rsh "$host" "python3 $SIGNAL_OWNED --pid <pid> --ticks <start_ticks> --boot <boot_id> --signal $sig $group"
    return 0
  fi
  ids=$(python3 "$MATRIX_DIR/ledger.py" owned --db "$SERVER_DB" --deployment "$dep" --role "$role")
  read -r pid ticks boot < <(printf '%s' "$ids" | python3 -c '
import json, sys
hid = sys.argv[1]
ids = [i for i in json.load(sys.stdin) if i.get("host_id") == hid]
if len(ids) != 1:
    sys.exit(f"expected exactly one owned identity on host {hid}, found {len(ids)}")
i = ids[0]
print(i["pid"], i["start_ticks"], i["boot_id"])' "$hid") || die "no single owned $role identity for $dep on $host"
  [ -n "${pid:-}" ] || die "no owned $role identity for $dep on $host"
  record "engine $host $dep role=$role pid=$pid ticks=$ticks sig=$sig $group"
  result=$(rsh "$host" "python3 $SIGNAL_OWNED --pid $pid --ticks $ticks --boot $boot --signal $sig $group") || { echo "$result"; record "result $result"; return 1; }
  echo "$result"; record "result $result"
}

agent() {
  local host=$1 sig=$2 result
  record "agent $host sig=$sig pidfile=$RRD/host.pid"
  result=$(rsh "$host" "python3 $SIGNAL_OWNED --pidfile $RRD/host.pid --expect 'start host' --signal $sig") || { echo "$result"; return 1; }
  echo "$result"; record "result $result"
}

server() {
  local sig=$1
  record "server sig=$sig pidfile=$LRD/server.pid"
  x python3 "$MATRIX_DIR/signal_owned.py" --pidfile "$LRD/server.pid" --expect "start server" --signal "$sig"
}

memhog_start() {
  local host=$1 gib=$2 secs=$3
  python3 -c 'import sys; g=float(sys.argv[1]); s=int(sys.argv[2]); sys.exit(0 if 0<g<=int(sys.argv[3]) and 0<s<=int(sys.argv[4]) else 1)' \
    "$gib" "$secs" "$MEMHOG_MAX_GIB" "$MEMHOG_MAX_SECONDS" || die "memhog bounds: 0 < GiB <= $MEMHOG_MAX_GIB, 0 < seconds <= $MEMHOG_MAX_SECONDS"
  record "memhog-start $host gib=$gib seconds=$secs"
  # timeout is the outer bound: TERM at seconds+30, KILL 15 s later.
  rsh "$host" "tmux new-session -d -s mx-memhog-$RUN timeout --signal=TERM --kill-after=15 $((secs + 30)) \
python3 $REMOTE_TREE/scripts/live/matrix/memhog.py --gib $gib --seconds $secs --pidfile $RRD/memhog.pid"
  # Wait until the allocation is held (pid file written and pages touched).
  rsh "$host" "for i in \$(seq 1 120); do [ -f $RRD/memhog.pid ] && break; sleep 1; done; sleep 5; grep MemAvailable /proc/meminfo"
}

memhog_stop() {
  local host=$1
  record "memhog-stop $host"
  rsh "$host" "if [ -f $RRD/memhog.pid ]; then python3 $SIGNAL_OWNED --pidfile $RRD/memhog.pid --expect memhog.py --signal TERM; \
for i in \$(seq 1 30); do [ -f $RRD/memhog.pid ] || break; sleep 1; done; fi; grep MemAvailable /proc/meminfo"
}

memhog_during() {
  local host=$1 gib=$2 secs=$3
  shift 3
  [ "${1:-}" = -- ] && shift
  # Cleanup trap: the allocation ends however this function exits.
  trap 'memhog_stop "'"$host"'" || true' EXIT INT TERM
  memhog_start "$host" "$gib" "$secs"
  local rc=0
  x "$@" || rc=$?
  memhog_stop "$host"
  trap - EXIT INT TERM
  return "$rc"
}

cmd=${1:-}; shift || true
load_run
case $cmd in
  engine) [ $# -ge 3 ] || exit 2; engine "$1" "$2" "$3" "${4:-api}" ;;
  engine-group) [ $# -eq 3 ] || exit 2; engine "$1" "$2" "$3" api --group ;;
  agent) [ $# -eq 2 ] || exit 2; agent "$@" ;;
  server) [ $# -eq 1 ] || exit 2; server "$@" ;;
  memhog-start) [ $# -eq 3 ] || exit 2; memhog_start "$@" ;;
  memhog-stop) [ $# -eq 1 ] || exit 2; memhog_stop "$@" ;;
  memhog-during) [ $# -ge 4 ] || exit 2; memhog_during "$@" ;;
  *) sed -n '2,20p' "$0"; exit 2 ;;
esac

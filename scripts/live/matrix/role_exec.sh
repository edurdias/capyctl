#!/usr/bin/env bash
# Start a role under a tmux pane and record its ownership identity (pid, start
# ticks, boot id), so later signals target exactly this process and never a
# name match.
#
# The role runs as a child of this script, not as the pane's own process:
# tmux continues a stopped pane process at once (it sends SIGCONT when its
# child stops), which silently voided the SIGSTOP fault of M58 (found live
# 2026-09-23). A grandchild of tmux can be stopped. This script waits for the
# role and exits with its status; the role's pid is the one recorded.
#
# usage: role_exec.sh <pidfile> <logfile> <command> [args...]
set -euo pipefail
pidfile=$1 log=$2
shift 2
umask 077
printf '# %s exec %s\n' "$(date -u +%FT%TZ)" "$*" >>"$log"
"$@" >>"$log" 2>&1 &
child=$!
# /proc/<pid>/stat field 22 is the start time in clock ticks.
read -r _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ ticks _ </proc/$child/stat
boot=$(cat /proc/sys/kernel/random/boot_id)
printf '%s %s %s\n' "$child" "$ticks" "$boot" >"$pidfile.tmp"
mv "$pidfile.tmp" "$pidfile"
# Forward a terminal hangup or interrupt to the role; its own signals come from
# the harness by recorded identity.
trap 'kill -TERM "$child" 2>/dev/null || true' HUP INT
status=0
wait "$child" || status=$?
exit "$status"

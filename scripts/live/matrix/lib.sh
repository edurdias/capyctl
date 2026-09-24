# shellcheck shell=bash disable=SC2034  # settings are used by the scripts that source this file
# Shared settings and helpers for the two-host matrix harness (plan unit W2).
#
# Source this file; do not execute it. Every effect goes through `x` (local) or
# `rsh` (remote), which log the command and, with DRY_RUN=1, only print it.
#
# Secrets: the inference API key is read from server-credentials.json into the
# environment by `load_api_key` and is never printed or written to evidence.
# The management token never leaves the CLI, which reads it itself via --config.

set -euo pipefail

MATRIX_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$MATRIX_DIR/../../.." && pwd)
LIVE=${MLLM_MATRIX_LIVE:-$REPO/target/live/matrix}
RUNSTATE=$LIVE/run
SNAPSHOT=$LIVE/snapshot
DRY_RUN=${DRY_RUN:-0}

# Machines. Addresses are the Tailscale 100.64/10 addresses recorded in the
# Phase B host documents (target/live/phase-b/host-b-*.yaml).
REMOTE_HOME=${MLLM_REMOTE_HOME:-$HOME}
# MLLM_REMOTE_TREE lets a second worktree keep its own tree and binary on the
# Sparks (the 2026-09-24 soak ran from ~/mllm-soak beside another agent's tree).
REMOTE_TREE=${MLLM_REMOTE_TREE:-$REMOTE_HOME/mllm-f2}
SERVER_IP=${MLLM_SERVER_IP:-100.64.0.20}
MATRIX_HOSTS=(host-a host-b)
MODELS_ROOT=$REMOTE_HOME/models
SGLANG_VENV=$REMOTE_HOME/mllm-sglang-0.5.20-venv
INGRESS_PORT=9443
SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=15)
# Pattern bracketed so it cannot match its own argv: the pre-flight arrives on the
# Spark as `bash -lc '... pgrep -af "..." ...'` (and Tailscale SSH's wrapper carries
# the same string), so a plain pattern would match the command asking the question.
ENGINE_PGREP='sglang[.]launch_server|sglang_entr[y]|vllm_entr[y]|vllm[ ]serve|Engine[C]ore|sglang::schedule[r]'

die() { echo "matrix: $*" >&2; exit 2; }
dry() { [ "$DRY_RUN" = 1 ]; }
# control-host's date is uutils, whose %3N is not zero-padded; bash's clock is exact.
now() { local t=$EPOCHREALTIME; TZ=UTC printf '%(%Y-%m-%dT%H:%M:%S)T.%.3sZ\n' "${t%.*}" "${t#*.}"; }
now_ms() { local t=$EPOCHREALTIME; echo $(( ${t%.*} * 1000 + 10#${t#*.} / 1000 )); }

host_ip() {
  case $1 in
    host-a) echo 100.64.0.10 ;;
    host-b) echo 100.64.0.11 ;;
    *) die "unknown host $1" ;;
  esac
}
host_short() {
  case $1 in host-a) echo 92 ;; host-b) echo 17 ;; *) die "unknown host $1" ;; esac
}
short_host() {
  case $1 in 92) echo host-a ;; 17) echo host-b ;; *) die "unknown host code $1" ;; esac
}
# The host-b vLLM environment is the byte-identical copy the owner authorized on
# 2026-09-22; host-b's older vLLM environments are never used.
vllm_venv() {
  case $1 in
    host-a) echo "$REMOTE_HOME/mllm-vllm-venv2" ;;
    host-b) echo "$REMOTE_HOME/mllm-vllm-0.29-venv" ;;
    *) die "unknown host $1" ;;
  esac
}

# Command log: every planned or executed effect, without secrets.
CMDLOG=${CMDLOG:-}
log_cmd() {
  local where=$1; shift
  local tag='+'; dry && tag='DRY'
  printf '%s [%s] %s\n' "$tag" "$where" "$*" >&2
  if [ -n "$CMDLOG" ]; then printf '%s %s [%s] %s\n' "$(now)" "$tag" "$where" "$*" >>"$CMDLOG"; fi
}

# Run a local command.
x() {
  log_cmd control-host "$*"
  dry && return 0
  "$@"
}

# Run a script on a Spark in a login shell (PATH then includes ~/.local/bin).
rsh() {
  local host=$1 script=$2
  log_cmd "$host" "$script"
  # A dry run still parses every remote script, so quoting errors surface here.
  if dry; then bash -n <<<"$script" || die "remote script for $host does not parse"; return 0; fi
  # Harness rehearsal only (MATRIX_LOCAL_RSH=1, with MLLM_REMOTE_HOME pointing at a
  # scratch tree of fake venvs and models): run the "remote" script on this machine.
  if [ "${MATRIX_LOCAL_RSH:-0}" = 1 ]; then bash -c "$script" </dev/null; return; fi
  # shellcheck disable=SC2029  # the script is meant to expand remotely
  # -n: never consume the caller's stdin (rsh runs inside `while read` loops).
  ssh -n "${SSH_OPTS[@]}" "$host" "bash -lc $(printf '%q' "$script")"
}

# Same as rsh, but its stdout is data the caller needs; in a dry run the caller's
# placeholder (second argument) is printed instead.
rsh_out() {
  local host=$1 placeholder=$2 script=$3
  if dry; then
    log_cmd "$host" "$script"
    bash -n <<<"$script" || die "remote script for $host does not parse"
    printf '%s\n' "$placeholder"; return 0
  fi
  rsh "$host" "$script"
}

rcopy() { # rcopy <src> <host:dst>
  x scp -q "${SSH_OPTS[@]}" "$1" "$2"
}

# Run state (non-secret) lives in $RUNSTATE/run.env.
load_run() {
  if [ -f "$RUNSTATE/run.env" ]; then
    # shellcheck disable=SC1091
    . "$RUNSTATE/run.env"
  elif dry; then
    RUN=matrix-DRYRUN
  else
    die "no run: start one with roles.sh up (or roles.sh server-init)"
  fi
  LRD=$HOME/mllm-runs/$RUN               # control-host run root (server state, private)
  RRD=$REMOTE_HOME/mllm-runs/$RUN        # Spark run root (host state, private)
  SERVER_CFG=$LRD/server.yaml
  SERVER_DB=$LRD/server/srv.sqlite3
  MLLM=$LRD/mllm                         # the snapshot-built server binary
  RBIN=$REMOTE_TREE/target/release/mllm  # the snapshot-built host binary
}

save_run_var() { # save_run_var NAME VALUE
  dry && { log_cmd control-host "record $1=$2 in $RUNSTATE/run.env"; return 0; }
  mkdir -p "$RUNSTATE"
  touch "$RUNSTATE/run.env"
  grep -v "^$1=" "$RUNSTATE/run.env" >"$RUNSTATE/run.env.tmp" || true
  printf '%s=%q\n' "$1" "$2" >>"$RUNSTATE/run.env.tmp"
  mv "$RUNSTATE/run.env.tmp" "$RUNSTATE/run.env"
}

# The CLI against the run's server. Output is the caller's to redirect.
cli() {
  log_cmd control-host "mllm $* --config $SERVER_CFG"
  dry && { echo '{}'; return 0; }
  "$MLLM" "$@" --config "$SERVER_CFG"
}

# Read the inference key into the environment only. Never echo it.
load_api_key() {
  if dry; then log_cmd control-host "export MLLM_API_KEY=<api_key from $LRD/server/identity/server-credentials.json>"; export MLLM_API_KEY=dry-run; return 0; fi
  MLLM_API_KEY=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["api_key"])' \
    "$LRD/server/identity/server-credentials.json")
  export MLLM_API_KEY
}

# Host enrolment ids recorded by roles.sh enroll.
host_id() {
  local var
  var="HOST_ID_$(host_short "$1")"
  if [ -n "${!var:-}" ]; then echo "${!var}"; elif dry; then echo "01DRYRUNHOSTID$(host_short "$1")0000000000"; else die "host $1 is not enrolled in run $RUN"; fi
}

# Tree digest over a directory: every regular file except build output, VCS,
# process artifacts, logs and bytecode, by relative path. The same function runs
# locally over the snapshot and remotely over ~/mllm-f2, so equality proves the
# Spark builds exactly the snapshot.
TREE_DIGEST_SH='find . \( -name target -o -name .git -o -name .superpowers -o -name __pycache__ \) -prune -o -type f ! -name "*.log" -print0 | LC_ALL=C sort -z | xargs -0 sha256sum | sha256sum | cut -c1-64'

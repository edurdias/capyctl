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
LIVE=${CAPYCTL_MATRIX_LIVE:-$REPO/target/live/matrix}
RUNSTATE=$LIVE/run
SNAPSHOT=$LIVE/snapshot
DRY_RUN=${DRY_RUN:-0}

die() { echo "matrix: $*" >&2; exit 2; }

# Machines come from an untracked local file so that no lab-specific host name,
# address or path lives in the tree. hosts.example.env documents every variable;
# copy it to hosts.local.env and fill it in. CAPYCTL_MATRIX_HOSTS_ENV points at
# another file.
HOSTS_ENV=${CAPYCTL_MATRIX_HOSTS_ENV:-$MATRIX_DIR/hosts.local.env}
[ -f "$HOSTS_ENV" ] || die "missing $HOSTS_ENV: copy $MATRIX_DIR/hosts.example.env to hosts.local.env and set the lab's hosts"
# shellcheck disable=SC1090
. "$HOSTS_ENV"
for _v in HOST_A HOST_B CONTROL_HOST HOST_A_ADDR HOST_B_ADDR CONTROL_HOST_ADDR \
  REMOTE_HOME SGLANG_VENV_DIR HOST_A_VLLM_VENV_DIR HOST_B_VLLM_VENV_DIR; do
  [ -n "${!_v:-}" ] || die "$HOSTS_ENV does not set $_v (see hosts.example.env)"
done
unset _v
[ "$HOST_A" != "$HOST_B" ] || die "HOST_A and HOST_B must differ"
export HOST_A HOST_B

REMOTE_HOME=${CAPYCTL_REMOTE_HOME:-$REMOTE_HOME}
# CAPYCTL_REMOTE_TREE lets a second worktree keep its own tree and binary on the
# hosts (the 2026-09-24 soak ran from ~/capyctl-soak beside another tree).
REMOTE_TREE=${CAPYCTL_REMOTE_TREE:-$REMOTE_HOME/capyctl-f2}
SERVER_IP=${CAPYCTL_SERVER_IP:-$CONTROL_HOST_ADDR}
MATRIX_HOSTS=("$HOST_A" "$HOST_B")
MODELS_ROOT=$REMOTE_HOME/models
export MODELS_ROOT
SGLANG_VENV=$REMOTE_HOME/$SGLANG_VENV_DIR
INGRESS_PORT=9443
SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=15)
# Pattern bracketed so it cannot match its own argv: the pre-flight arrives on the
# host as `bash -lc '... pgrep -af "..." ...'` (and Tailscale SSH's wrapper carries
# the same string), so a plain pattern would match the command asking the question.
ENGINE_PGREP='sglang[.]launch_server|sglang_entr[y]|vllm_entr[y]|vllm[ ]serve|Engine[C]ore|sglang::schedule[r]'

dry() { [ "$DRY_RUN" = 1 ]; }
# control-host's date is uutils, whose %3N is not zero-padded; bash's clock is exact.
now() { local t=$EPOCHREALTIME; TZ=UTC printf '%(%Y-%m-%dT%H:%M:%S)T.%.3sZ\n' "${t%.*}" "${t#*.}"; }
now_ms() { local t=$EPOCHREALTIME; echo $(( ${t%.*} * 1000 + 10#${t#*.} / 1000 )); }

host_ip() {
  case $1 in
    "$HOST_A") echo "$HOST_A_ADDR" ;;
    "$HOST_B") echo "$HOST_B_ADDR" ;;
    *) die "unknown host $1" ;;
  esac
}
# Fixture and run-state code of a host: a for HOST_A, b for HOST_B.
host_short() {
  case $1 in "$HOST_A") echo a ;; "$HOST_B") echo b ;; *) die "unknown host $1" ;; esac
}
short_host() {
  case $1 in a) echo "$HOST_A" ;; b) echo "$HOST_B" ;; *) die "unknown host code $1" ;; esac
}
# A row's host argument: the code a or b, or a configured host name.
resolve_host() {
  case $1 in a|b) short_host "$1" ;; "$HOST_A"|"$HOST_B") echo "$1" ;; *) die "unknown host $1 (use a, b, $HOST_A or $HOST_B)" ;; esac
}
# Each host's vLLM environment. Only the environments the owner authorized are
# named in hosts.local.env; no other environment on a host is used.
vllm_venv() {
  case $1 in
    "$HOST_A") echo "$REMOTE_HOME/$HOST_A_VLLM_VENV_DIR" ;;
    "$HOST_B") echo "$REMOTE_HOME/$HOST_B_VLLM_VENV_DIR" ;;
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

# Run a script on a host in a login shell (PATH then includes ~/.local/bin).
rsh() {
  local host=$1 script=$2
  log_cmd "$host" "$script"
  # A dry run still parses every remote script, so quoting errors surface here.
  if dry; then bash -n <<<"$script" || die "remote script for $host does not parse"; return 0; fi
  # Harness rehearsal only (MATRIX_LOCAL_RSH=1, with CAPYCTL_REMOTE_HOME pointing at a
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
  LRD=${CAPYCTL_LOCAL_RUN_ROOT:-$HOME/capyctl-runs/$RUN}         # control-host run root (server state, private)
  RRD=${CAPYCTL_REMOTE_RUN_ROOT:-$REMOTE_HOME/capyctl-runs/$RUN}  # host run root (host state, private)
  SERVER_CFG=$LRD/server.yaml
  SERVER_DB=$LRD/server/srv.sqlite3
  # Release validation (CAPYCTL_LOCAL_BIN / CAPYCTL_REMOTE_BIN, e.g. ~/.local/bin/capyctl
  # from install.sh) runs the installed binaries instead of snapshot builds; the
  # rows then only need the harness scripts under CAPYCTL_REMOTE_TREE.
  CAPYCTL=${CAPYCTL_LOCAL_BIN:-$LRD/capyctl}                          # the server binary
  RBIN=${CAPYCTL_REMOTE_BIN:-$REMOTE_TREE/target/release/capyctl}  # the host binary
}

# ADR 0018 (row ENG4): one host may run another binary than the rest, e.g. an
# rc.3 agent beside new ones. CAPYCTL_REMOTE_BIN_a / CAPYCTL_REMOTE_BIN_b override
# RBIN for that host only.
rbin() { # rbin <host>
  local var
  var="CAPYCTL_REMOTE_BIN_$(host_short "$1")"
  printf '%s\n' "${!var:-$RBIN}"
}

save_run_var() { # save_run_var NAME VALUE
  dry && { log_cmd control-host "record $1=$2 in $RUNSTATE/run.env"; return 0; }
  mkdir -p "$RUNSTATE"
  touch "$RUNSTATE/run.env"
  grep -v "^$1=" "$RUNSTATE/run.env" >"$RUNSTATE/run.env.tmp" || true
  printf '%s=%q\n' "$1" "$2" >>"$RUNSTATE/run.env.tmp"
  mv "$RUNSTATE/run.env.tmp" "$RUNSTATE/run.env"
}

# Machine-readable CLI output is `--format json`: without it, record views
# (list, status, engine list/detect) print a table. A release binary from
# before the table default (rc.4 and earlier, e.g. ENG4's rc.3 or a release
# under validation) knows only `--output json`, which every later binary still
# accepts, so the flag is translated for it. The probe is cached per binary.
cli_format_probe() {
  [ "${CLI_FORMAT_BIN:-}" = "$CAPYCTL" ] && return 0
  CLI_FORMAT_BIN=$CAPYCTL
  case "$("$CAPYCTL" --help 2>/dev/null)" in
    *--format*) CLI_FORMAT_FLAG=--format ;;
    *) CLI_FORMAT_FLAG=--output ;;
  esac
}

# The CLI against the run's server. Output is the caller's to redirect.
cli() {
  local args=() arg
  dry || cli_format_probe
  for arg in "$@"; do
    [ "$arg" = --format ] && [ "${CLI_FORMAT_FLAG:---format}" = --output ] && arg=--output
    args+=("$arg")
  done
  log_cmd control-host "capyctl ${args[*]} --config $SERVER_CFG"
  dry && { echo '{}'; return 0; }
  "$CAPYCTL" "${args[@]}" --config "$SERVER_CFG"
}

# Read the inference key into the environment only. Never echo it.
load_api_key() {
  if dry; then log_cmd control-host "export CAPYCTL_API_KEY=<api_key from $LRD/server/identity/server-credentials.json>"; export CAPYCTL_API_KEY=dry-run; return 0; fi
  CAPYCTL_API_KEY=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["api_key"])' \
    "$LRD/server/identity/server-credentials.json")
  export CAPYCTL_API_KEY
}

# Host enrolment ids recorded by roles.sh enroll.
host_id() {
  local var
  var="HOST_ID_$(host_short "$1")"
  if [ -n "${!var:-}" ]; then echo "${!var}"; elif dry; then echo "01DRYRUNHOSTID$(host_short "$1")0000000000"; else die "host $1 is not enrolled in run $RUN"; fi
}

# Tree digest over a directory: every regular file except build output, VCS,
# hidden directories (local working notes), logs and bytecode, by relative path.
# The same function runs locally over the snapshot and remotely over ~/capyctl-f2,
# so equality proves the host builds exactly the snapshot.
TREE_DIGEST_SH='find . \( -name target -o -name .git -o \( -type d -name ".?*" \) -o -name __pycache__ \) -prune -o -type f ! -name "*.log" -print0 | LC_ALL=C sort -z | xargs -0 sha256sum | sha256sum | cut -c1-64'

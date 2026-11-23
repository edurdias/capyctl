# shellcheck shell=bash disable=SC2034  # settings are used by the scripts that source this file
# The OpenTelemetry Collector of the ADR 0037 live rows OT1-OT11, sourced by
# otel_rows.sh after rowlib.sh.
#
# ADR 0037 "Live qualification" and owner decision 2026-10-27 (question 11): the
# collector is the OpenTelemetry Collector release binary that the owner placed in
# the home directory on host A and host B (OTELCOL_BIN in hosts.local.env). No
# receiver script exists in this tree. A row starts the binary with
# templates/otelcol.yaml (an OTLP/HTTP receiver bound to loopback, a file exporter
# writing into the row's evidence directory on the host), records the binary's
# version and SHA-256, stops it by its recorded identity (role_exec.sh pid file,
# signal_owned.py) and copies the host's evidence directory into the row's.
# Every collector a row started and has not stopped is stopped by ot_end.

OTELCOL=${OTELCOL_BIN:+$REMOTE_HOME/$OTELCOL_BIN}
OTELCOL_PORT=${OTELCOL_PORT:-4318}
OTELCOL_LISTEN=127.0.0.1:$OTELCOL_PORT
# The endpoint every role and engine of the rows exports to (§12: the full
# traces URL, used exactly as written).
OT_ENDPOINT=http://$OTELCOL_LISTEN/v1/traces
# The row's evidence directory on a host: the collector's file, its config and
# log, the role logs and the probes' records. It is never deleted; ot_fetch
# copies it into $EVID/host-<a|b>.
OT_HEV=$RRD/otel-evidence/$(basename "$EVID")
# "<host> <label>" of each collector started and not yet stopped.
OT_COLLECTORS=()

# ot_hev_init <host>: a fresh evidence directory on the host (an earlier one for
# the same row is moved aside, never deleted), owner-only.
ot_hev_init() {
  rsh "$1" "mkdir -p -m 700 $RRD/otel-evidence && if [ -d $OT_HEV ]; then mv $OT_HEV $OT_HEV.prev-\$(date -u +%Y%m%dT%H%M%SZ); fi && mkdir -m 700 $OT_HEV && echo $OT_HEV"
}

# otelcol_pre <host>: the binary is there and executable, its version and
# SHA-256 (into otelcol-binary.txt in the evidence), the SHA-256 OTELCOL_SHA256
# names when set, and nothing listening on the receiver's port.
otelcol_pre() {
  local host=$1
  [ -n "$OTELCOL" ] || { echo "PRECONDITION: $HOSTS_ENV sets no OTELCOL_BIN (see hosts.example.env)" >&2; return 1; }
  rsh "$host" "rc=0
[ -x $OTELCOL ] || { echo 'PRECONDITION: no OpenTelemetry Collector binary at $OTELCOL (owner decision 2026-10-27, question 11)' >&2; exit 1; }
{ echo \"binary: $OTELCOL\"; echo \"version: \$($OTELCOL --version 2>&1 | head -n 1)\"; echo \"sha256: \$(sha256sum $OTELCOL | cut -d ' ' -f 1)\"; } | tee $OT_HEV/otelcol-binary.txt
sum=\$(sha256sum $OTELCOL | cut -d ' ' -f 1)
if [ -n '${OTELCOL_SHA256:-}' ] && [ \"\$sum\" != '${OTELCOL_SHA256:-}' ]; then echo 'PRECONDITION: $OTELCOL is not the binary OTELCOL_SHA256 names' >&2; rc=1; fi
if ss -Hltn | grep -Eq '[:.]$OTELCOL_PORT\\b'; then echo 'PRECONDITION: something already listens on port $OTELCOL_PORT' >&2; ss -Hltnp | grep -E '[:.]$OTELCOL_PORT\\b' >&2; rc=1; fi
exit \$rc"
}

# otelcol_start <host> <label>: render the template for this run, copy it to the
# host and start the collector under the harness's tmux server; wait until its
# receiver listens on loopback.
otelcol_start() {
  local host=$1 label=$2 cfg tpl
  cfg=$EVID/otelcol-$label.yaml
  tpl=$(<"$MATRIX_DIR/templates/otelcol.yaml")
  tpl=${tpl//@LISTEN@/$OTELCOL_LISTEN}
  tpl=${tpl//@FILE@/$OT_HEV/collector-$label.jsonl}
  printf '%s\n' "$tpl" >"$cfg"
  rcopy "$cfg" "$host:$OT_HEV/otelcol-$label.yaml" || return 1
  rsh "$host" "chmod 600 $OT_HEV/otelcol-$label.yaml && $MX_TMUX new-session -d -s mx-otelcol-$label-$RUN \
$REMOTE_TREE/scripts/live/matrix/role_exec.sh $OT_HEV/otelcol-$label.pid $OT_HEV/otelcol-$label.log \
$OTELCOL --config $OT_HEV/otelcol-$label.yaml" || return 1
  OT_COLLECTORS+=("$host $label")
  rsh "$host" "for i in \$(seq 1 30); do if ss -Hltn | grep -Eq '127[.]0[.]0[.]1:$OTELCOL_PORT\\b'; then echo 'collector $label listening on $OTELCOL_LISTEN'; exit 0; fi; sleep 1; done; tail -n 30 $OT_HEV/otelcol-$label.log; exit 1"
}

# otelcol_stop <host> <label>: SIGTERM to the recorded identity (the collector
# flushes its file on shutdown), then wait for it to exit.
otelcol_stop() {
  local host=$1 label=$2 i kept=()
  for i in "${OT_COLLECTORS[@]}"; do [ "$i" = "$host $label" ] || kept+=("$i"); done
  OT_COLLECTORS=("${kept[@]}")
  rsh "$host" "python3 $REMOTE_TREE/scripts/live/matrix/signal_owned.py --pidfile $OT_HEV/otelcol-$label.pid --expect '$OTELCOL' --signal TERM && \
read -r pid _ < $OT_HEV/otelcol-$label.pid && for i in \$(seq 1 30); do [ -d /proc/\$pid ] || { echo 'collector $label stopped'; ls -l $OT_HEV/collector-$label.jsonl 2>/dev/null || echo 'collector $label wrote no file'; exit 0; }; sleep 1; done; echo 'collector $label still running after 30 s' >&2; exit 1"
}

# otelcol_stop_all: every collector the row started and has not stopped.
otelcol_stop_all() {
  local entry rc=0
  for entry in "${OT_COLLECTORS[@]}"; do
    # shellcheck disable=SC2086  # "<host> <label>"
    step "collector-stop-${entry#* }" otelcol_stop $entry || rc=1
  done
  return "$rc"
}

# ot_fetch <host>: the host's evidence directory into $EVID/host-<a|b>.
ot_fetch() {
  local host=$1
  x scp -q -r "${SSH_OPTS[@]}" "$host:$OT_HEV" "$EVID/host-$(host_short "$host")"
}

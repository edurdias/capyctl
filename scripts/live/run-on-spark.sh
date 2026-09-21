#!/usr/bin/env bash
# Live run on host-a. Refuse to start beside another engine.
# Both Spark hosts are authorized, but this runner's installation paths and
# fixtures are specifically for host-a.
set -euo pipefail
HOST=host-a
[ "${1:-$HOST}" = "$HOST" ] || { echo "only $HOST is authorized" >&2; exit 2; }
SUITE=${2:-all}
case "$SUITE" in all|vllm|sglang) ;; *) echo "suite must be all, vllm or sglang" >&2; exit 2 ;; esac
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
# The pattern is written so it cannot match its own text. The pre-flight arrives on
# the box as `bash -c '... pgrep -af "..." ...'`, and over a Tailscale SSH the
# tailscaled wrapper carries the same string, so a plain pattern matched the command
# that was asking the question and refused on an empty box. Bracketing one character
# of each alternative leaves the regex meaning unchanged while the literal argv it
# appears in no longer matches it. The grep filter drops the remaining wrappers, so
# what is printed is engines and nothing else.
ssh -o BatchMode=yes "$HOST" 'if pgrep -af "sglang[.]launch_server|vllm[ ]serve|Engine[C]ore|sglang::scheduler" | grep -vE "pgrep|tailscaled|bash -c"; then echo "another engine is on the box; refusing" >&2; exit 3; fi'
rsync -az --delete --exclude target --exclude .git --exclude .superpowers \
  --exclude crates/mllm-cli/tests/live_interactive.rs \
  --exclude crates/mllm-cli/tests/repro_sglang_unarmed.rs \
  --exclude __pycache__ ./ "$HOST:~/mllm-f2/"
# Recover evidence even when compilation or the live gate fails.
collect_evidence() {
  local result=$?
  trap - EXIT
  mkdir -p "target/live/$STAMP"
  rsync -az "$HOST:~/mllm-f2/target/live/current/" "target/live/$STAMP/" || result=1
  echo "evidence under target/live/$STAMP"
  exit "$result"
}
trap collect_evidence EXIT
ssh -o BatchMode=yes "$HOST" bash -s -- "$SUITE" "$STAMP" <<'REMOTE'
set -euo pipefail
export PATH=$HOME/.cargo/bin:$HOME/.local/bin:$PATH PROTOC=$HOME/.local/bin/protoc
cd ~/mllm-f2
if [ -d target/live/current ]; then
  mv target/live/current "target/live/previous-$2"
fi
mkdir -p target/live/current
# The protected wrapper requires every ancestor to reject group/other writes.
namei -l "$PWD/runtime/sglang_entry.py" | tee target/live/current/wrapper-path.txt
# SPEC §9.1 / T21: this authorized isolated run explicitly opts in.
export MLLM_DEEP_PARK=on
cargo build --release --bin mllm
# L10's other half: the shipped binary must carry no test engine. Asserted here
# because it reads the linked binary's own symbols, which no test inside that
# binary can do about itself.
bash scripts/check-release-clean.sh
export MLLM_LIVE=1 MLLM_VLLM_BIN=$HOME/mllm-vllm-venv2/bin/vllm MLLM_MODELS_ROOT=$HOME/models
mkdir -p target/live/current
# One thread: three scenarios export environment variables to boot standalone
# against a different installation, and the environment is process-global.
if [ "$1" != sglang ]; then
  cargo test --release -p mllm-cli --test live_vllm -- --test-threads=1 --nocapture 2>&1 | tee target/live/current/test-output.log
fi
# The SGLang live gate. The guarded launcher execs the interpreter named by
# MLLM_SGLANG_BIN with `-IS` and the protected wrapper, so the bin is the venv's
# python, not a console script. The fingerprint is the installed SGLang's own
# report, so the pinned recipe is qualified against what will actually launch.
# The vLLM bin is unset for this suite: a host publishes exactly one engine.
if [ "$1" != vllm ]; then (
  unset MLLM_VLLM_BIN
  export MLLM_SGLANG_BIN=$HOME/mllm-sglang-f2-venv/bin/python3
  export MLLM_ENGINE_FINGERPRINT=$("$MLLM_SGLANG_BIN" -c 'import sglang; print(sglang.__version__)')
  cargo test --release -p mllm-cli --test live_sglang -- --test-threads=1 --nocapture 2>&1 | tee -a target/live/current/test-output.log
)
fi
REMOTE

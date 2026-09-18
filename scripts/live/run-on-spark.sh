#!/usr/bin/env bash
# Live run of S1 on the only authorized host (AGENTS.md). Refuses anything else.
#
# The refusals are the point. host-a is the one host this project may touch,
# and a live run that starts beside another engine would either fight it for the
# device or quietly measure its memory instead of its own. Nothing is killed by
# name: if something is already running, this prints what it found and stops, and a
# person decides what to do about it.
set -euo pipefail
HOST=host-a
[ "${1:-$HOST}" = "$HOST" ] || { echo "only $HOST is authorized" >&2; exit 2; }
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
# The pattern is written so it cannot match its own text. The pre-flight arrives on
# the box as `bash -c '... pgrep -af "..." ...'`, and over a Tailscale SSH the
# tailscaled wrapper carries the same string, so a plain pattern matched the command
# that was asking the question and refused on an empty box. Bracketing one character
# of each alternative leaves the regex meaning unchanged while the literal argv it
# appears in no longer matches it. The grep filter drops the remaining wrappers, so
# what is printed is engines and nothing else.
ssh -o BatchMode=yes "$HOST" 'if pgrep -af "sglang[.]launch_server|vllm[ ]serve|Engine[C]ore|sglang::scheduler" | grep -vE "pgrep|tailscaled|bash -c"; then echo "another engine is on the box; refusing" >&2; exit 3; fi'
rsync -az --delete --exclude target --exclude .git --exclude .superpowers ./ "$HOST:~/mllm-f2/"
ssh -o BatchMode=yes "$HOST" bash -s <<'REMOTE'
set -euo pipefail
export PATH=$HOME/.cargo/bin:$HOME/.local/bin:$PATH PROTOC=$HOME/.local/bin/protoc
cd ~/mllm-f2
cargo build --release --bin mllm
# L10's other half: the shipped binary must carry no test engine. Asserted here
# because it reads the linked binary's own symbols, which no test inside that
# binary can do about itself.
bash scripts/check-release-clean.sh
cargo build --release --tests -p mllm-cli
export MLLM_LIVE=1 MLLM_VLLM_BIN=$HOME/mllm-vllm-venv2/bin/vllm MLLM_MODELS_ROOT=$HOME/models
mkdir -p target/live/current
# One thread: three scenarios export environment variables to boot standalone
# against a different installation, and the environment is process-global.
cargo test --release -p mllm-cli --test live_vllm -- --test-threads=1 --nocapture 2>&1 | tee target/live/current/test-output.log
REMOTE
mkdir -p "target/live/$STAMP"
rsync -az "$HOST:~/mllm-f2/target/live/current/" "target/live/$STAMP/"
echo "evidence under target/live/$STAMP"

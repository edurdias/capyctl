#!/usr/bin/env bash
# Snapshot, sync and build for the matrix (plan unit W2).
#
#   sync.sh snapshot        copy the worktree to target/live/matrix/snapshot/tree, record its digest
#   sync.sh push [host..]   rsync the snapshot to ~/mllm-f2 on the Sparks and verify the digest there
#   sync.sh build [host..]  build target/release/mllm from the snapshot on control-host and on the Sparks,
#                           then scripts/check-release-clean.sh on every binary built
#   sync.sh runtime [host..] resync only runtime/ and verify files and permissions
#   sync.sh all             snapshot, push, build, runtime check
#
# DRY_RUN=1 prints every command without running it.
. "$(dirname "$0")/lib.sh"

# Excluded from the snapshot: build output, VCS, process artifacts (owner rule),
# logs and bytecode.
SNAP_EXCLUDES=(--exclude target --exclude .git --exclude .superpowers
  --exclude '*.log' --exclude __pycache__)
# Files a Spark runtime directory must hold before any launch (WE2: vLLM runs
# through vllm_entry.py; development mode loads mllm_vllm_guard.py; WE3 checkpoint
# identity runs through pinned_file_observation.py; SGLang parking enrolls the
# saver observation through sglang_observation_enrollment.py).
RUNTIME_REQUIRED=(sglang_entry.py sglang_device.py sglang_server_args.py vllm_entry.py mllm_vllm_guard.py pinned_file_observation.py
  sglang_observation_enrollment.py sglang_saver_residency.py sglang_observation_server.py sglang_observation_transport.py
  sglang_scheduler_observer.py sglang_saver_binding.py memory_saver_observer.py owner_only.py engine_capabilities.py)

hosts_or_all() { if [ $# -gt 0 ]; then printf '%s\n' "$@"; else printf '%s\n' "${MATRIX_HOSTS[@]}"; fi; }

snapshot() {
  x mkdir -p "$SNAPSHOT/tree"
  # The rsync never opens excluded paths, so the owner's excluded files are not read.
  # Content comparison, no source mtimes: a file edited in the worktree before the
  # last build but copied after it would otherwise keep an older mtime than the
  # build's fingerprint, and cargo would reuse stale artifacts (found live, M16:
  # an unresolved `mllm_scheduler::placement` from a stale mllm-scheduler rlib).
  # Copied files get the copy time; unchanged files keep theirs.
  x rsync -rlpD --checksum --delete --chmod=Dgo-w,Fgo-w "${SNAP_EXCLUDES[@]}" "$REPO/" "$SNAPSHOT/tree/"
  if dry; then log_cmd control-host "(cd $SNAPSHOT/tree && $TREE_DIGEST_SH) > $SNAPSHOT/tree.sha256"; return 0; fi
  (cd "$SNAPSHOT/tree" && eval "$TREE_DIGEST_SH") >"$SNAPSHOT/tree.sha256"
  git -C "$REPO" rev-parse HEAD >"$SNAPSHOT/commit"
  git -C "$REPO" status --porcelain=v1 --untracked-files=no | wc -l >"$SNAPSHOT/dirty-count"
  sha256sum "$SNAPSHOT/tree/Cargo.lock" | cut -c1-64 >"$SNAPSHOT/cargo-lock.sha256"
  echo "snapshot $(cat "$SNAPSHOT/tree.sha256") commit $(cat "$SNAPSHOT/commit") dirty $(cat "$SNAPSHOT/dirty-count")"
}

snapshot_digest() { if dry; then echo "<snapshot-digest>"; else cat "$SNAPSHOT/tree.sha256"; fi; }

push() {
  local host want got
  want=$(snapshot_digest)
  for host in $(hosts_or_all "$@"); do
    # Group/other write is stripped: the protected wrappers refuse any ancestor
    # or file that others can write (Phase B found mllm_vllm_guard.py at 0664).
    # Same reason as the snapshot: compare content, never carry mtimes across.
    x rsync -rlpDz --checksum --delete --chmod=Dgo-w,Fgo-w "${SNAP_EXCLUDES[@]}" "$SNAPSHOT/tree/" "$host:$REMOTE_TREE/"
    got=$(rsh_out "$host" "$want" "cd $REMOTE_TREE && $TREE_DIGEST_SH")
    [ "$got" = "$want" ] || die "$host tree digest $got != snapshot $want"
    echo "$host tree $got"
  done
}

build() {
  local host out
  dry || mkdir -p "$RUNSTATE"
  # control-host: the server binary is built from the snapshot, not the live worktree.
  x cargo build --release --locked --bin mllm --manifest-path "$SNAPSHOT/tree/Cargo.toml" --target-dir "$LIVE/build"
  # The shipped binary carries no test engine (M75's other half); checked on the
  # binary this build produced, as on each Spark below.
  # shellcheck disable=SC2016  # $1 expands in the inner shell
  x env CARGO_TARGET_DIR="$LIVE/build" bash -c 'cd "$1" && bash scripts/check-release-clean.sh' _ "$SNAPSHOT/tree"
  if ! dry; then sha256sum "$LIVE/build/release/mllm" | tee "$RUNSTATE/binary-control-host.txt"; fi
  for host in $(hosts_or_all "$@"); do
    out=$(rsh_out "$host" "<sha256>  $REMOTE_TREE/target/release/mllm" \
      "export PATH=\$HOME/.cargo/bin:\$HOME/.local/bin:\$PATH PROTOC=\$HOME/.local/bin/protoc; cd $REMOTE_TREE && cargo build --release --locked --bin mllm && bash scripts/check-release-clean.sh && sha256sum target/release/mllm")
    dry || printf '%s\n' "$out" | tail -1 | tee "$RUNSTATE/binary-$host.txt"
  done
}

runtime() {
  local host
  for host in $(hosts_or_all "$@"); do
    # --delete-excluded: an excluded path is otherwise kept on the receiver, and
    # stale bytecode there makes the host refuse every launch `runtime_integrity`
    # (found live 2026-09-24). Only __pycache__ is excluded here, so nothing else
    # is affected.
    x rsync -rlpDz --checksum --delete --delete-excluded --chmod=Dgo-w,Fgo-w --exclude __pycache__ "$SNAPSHOT/tree/runtime/" "$host:$REMOTE_TREE/runtime/"
    x rsync -az --chmod=Dgo-w,Fgo-w "$SNAPSHOT/tree/scripts/live/matrix/check_runtime.py" "$host:$REMOTE_TREE/scripts/live/matrix/"
    rsh "$host" "python3 $REMOTE_TREE/scripts/live/matrix/check_runtime.py $REMOTE_TREE/runtime ${RUNTIME_REQUIRED[*]}"
  done
}

cmd=${1:-}; shift || true
case $cmd in
  snapshot) snapshot ;;
  push) push "$@" ;;
  build) build "$@" ;;
  runtime) runtime "$@" ;;
  all) snapshot; push "$@"; build "$@"; runtime "$@" ;;
  *) sed -n '2,11p' "$0"; exit 2 ;;
esac

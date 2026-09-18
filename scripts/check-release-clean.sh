#!/usr/bin/env bash
# The shipped binary must carry no test engine.
#
# The Fake engine, the fake launcher and their fixtures live in mllm-testkit,
# which every other crate reaches only through [dev-dependencies]. This check is
# what keeps that true after a careless edit: a release build carrying the Fake
# would ship a lane in which a deployment reports Ready with no engine behind it.
#
# The release profile strips symbol names, so the search is for what a linked-in
# fake still leaves behind — panic locations naming its source files and the
# identifiers it prints. The matches are collected rather than piped into
# `grep -q`, because a quiet grep closes the pipe early and `pipefail` would then
# read the writer's broken pipe as "nothing found".
set -euo pipefail

cargo build --release --bin mllm --offline

artifacts=$(strings target/release/mllm |
  grep -E "FakeEngine|FakeLauncher|mllm_testkit|mllm-testkit|src/fake/|fake-engine|fake-lifecycle" || true)
if [ -n "$artifacts" ]; then
  echo "release binary contains test artifacts:" >&2
  echo "$artifacts" >&2
  exit 1
fi

tree=$(cargo tree -p mllm-cli -e normal --offline)
if grep -q "mllm-testkit" <<<"$tree"; then
  echo "mllm-testkit is a normal dependency" >&2
  exit 1
fi

echo "release binary clean"

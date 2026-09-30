#!/usr/bin/env bash
# ADR 0022: the project is CapyCTL (`capyctl`). No tracked file may name the
# old project name outside the history set, which records the past, and the
# two places that must name it (ADR 0022 and the 0.1.0 upgrade note).
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

allowed=(
  'docs/plans/'
  'docs/specs/'
  'docs/design/adr/00[01][0-9]-'
  'docs/design/adr/002[01]-'
  'docs/design/adr/0022-rename-capyctl.md'
  'docs/runbooks/f2-current-status.md'
  'docs/operations/release-notes-0.1.0.md'
  'scripts/check-name.sh'
)
pattern=$(printf '^%s|' "${allowed[@]}")
pattern=${pattern%|}

hits=$(git grep -n -i -I 'mllm' -- . | grep -Ev "$pattern" || true)
# A tracked path may name it too, even with nothing inside.
paths=$(git ls-files | grep -i 'mllm' | grep -Ev "$pattern" || true)
if [ -n "$paths" ]; then
  hits=$(printf '%s\n%s' "$hits" "${paths//$'\n'/$': (path)\n'}: (path)")
  hits=${hits#$'\n'}
fi
if [ -n "$hits" ]; then
  head -n 50 <<<"$hits" >&2
  echo "check-name: $(wc -l <<<"$hits") line(s) still name the old project name" >&2
  exit 1
fi
echo "check-name: ok"

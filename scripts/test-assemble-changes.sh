#!/usr/bin/env bash
# Exercise scripts/assemble-changes.sh and scripts/release-notes-body.sh on
# scratch checkouts.
#
#   scripts/test-assemble-changes.sh
#
# Each case builds a small git repository holding a status runbook, release
# notes and change files committed at fixed times, so "newest first" is
# deterministic. Cases:
#
#   - --dry-run prints the assembled result as a diff and changes nothing;
#   - a run puts the status entries at the top of the runbook, newest first
#     (an uncommitted file first of all), each release note after the last
#     bullet of its section (before a section's trailing table), a new section
#     at the end, nothing for "none", keeps a "#" line inside a code fence as
#     text, and removes the change files but not README.md;
#   - a second run has nothing to do;
#   - a release notes file that does not exist is created;
#   - refusals: an empty file, no "# Status:" first, no "# Release note:",
#     another level-1 heading, text before "# Status:", a relative link
#     without ../, "none" with text, an empty release note; none of them
#     changes any file;
#   - the release page body: every relative link (inline, with a title, a
#     reference definition, a same-folder file, the notes' own anchor, a
#     folder) pinned to the full release commit, given abbreviated; absolute
#     links, code spans and fences unchanged; refused, with nothing printed:
#     a heading added after the release commit, a "heading" inside a code
#     fence, a missing file, an anchor on a folder, a link out of the
#     repository.
set -euo pipefail

root=$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)
script=$root/scripts/assemble-changes.sh
work=$(mktemp -d "${TMPDIR:-/tmp}/capyctl-test-assemble.XXXXXX")
trap 'rm -rf "$work"' EXIT

failures=0
fail() { echo "FAIL: $*" >&2; failures=$((failures + 1)); }
pass() { echo "ok:   $*"; }

g() { git -C "$tree" -c user.name=test -c user.email=test@example.invalid "$@"; }

# A scratch checkout with the base runbook and notes committed at t=100.
new_tree() { # dir
  tree=$1
  mkdir -p "$tree/docs/runbooks" "$tree/docs/operations" "$tree/docs/changes"
  cat >"$tree/docs/runbooks/f2-current-status.md" <<'EOF'
# Current implementation and launch status

## Existing entry — 2026-09-30

Existing text.
EOF
  cat >"$tree/docs/operations/release-notes-0.0.1.md" <<'EOF'
# CapyCTL 0.0.1

Unreleased.

## Memory

- **Old memory one.** Text
  continued.
- **Old memory two.** Text.

## Multi-node groups

Intro paragraph.

- **Group bullet.** Text
  more.

| Model | Engine |
|---|---|
| a | b |

## Fixes

- **Old fix.** Text.
EOF
  printf '# Change files\n' >"$tree/docs/changes/README.md"
  git init -q "$tree"
  g add -A
  GIT_COMMITTER_DATE='@100 +0000' GIT_AUTHOR_DATE='@100 +0000' g commit -q -m base
}

add_change() { # name epoch|- ; content on stdin
  cat >"$tree/docs/changes/$1"
  if [ "$2" != - ]; then
    g add "docs/changes/$1"
    GIT_COMMITTER_DATE="@$2 +0000" GIT_AUTHOR_DATE="@$2 +0000" g commit -q -m "add $1"
  fi
}

snapshot() { (cd "$tree" && find docs -type f | LC_ALL=C sort | xargs sha256sum); }

# --- the main fixture ---------------------------------------------------------
main_fixture() {
  new_tree "$1"
  add_change older.md 1000 <<'EOF'
# Status: Older change — 2026-10-01 (branch `older`)

Older status text, see [the guide](../guide/x.md).

```bash
# a comment, not a heading
run it
```

# Release note: Memory

- **Older memory note.** Text with a [link](../guide/x.md#a).
EOF
  add_change middle.md 1500 <<'EOF'
# Status: Middle change — 2026-10-02

Middle status text.

# Release note: Multi-node groups

- **Middle group note.** Text.
EOF
  add_change newer.md 2000 <<'EOF'
# Status: Newer change — 2026-10-03

Newer status text.

# Release note: Requests

- **Newer request note.** Text.
EOF
  add_change internal.md - <<'EOF'

# Status: Internal change — 2026-10-04

Internal status text.

# Release note: none
EOF
}

expected_runbook=$work/expected-runbook
cat >"$expected_runbook" <<'EOF'
# Current implementation and launch status

## Internal change — 2026-10-04

Internal status text.

## Newer change — 2026-10-03

Newer status text.

## Middle change — 2026-10-02

Middle status text.

## Older change — 2026-10-01 (branch `older`)

Older status text, see [the guide](../guide/x.md).

```bash
# a comment, not a heading
run it
```

## Existing entry — 2026-09-30

Existing text.
EOF
expected_notes=$work/expected-notes
cat >"$expected_notes" <<'EOF'
# CapyCTL 0.0.1

Unreleased.

## Memory

- **Old memory one.** Text
  continued.
- **Old memory two.** Text.
- **Older memory note.** Text with a [link](../guide/x.md#a).

## Multi-node groups

Intro paragraph.

- **Group bullet.** Text
  more.
- **Middle group note.** Text.

| Model | Engine |
|---|---|
| a | b |

## Fixes

- **Old fix.** Text.

## Requests

- **Newer request note.** Text.
EOF

# --- dry run ------------------------------------------------------------------
main_fixture "$work/dry"
before=$(snapshot)
if out=$("$script" --dry-run --root "$tree" 0.0.1 2>&1); then
  if [ "$(snapshot)" != "$before" ]; then
    fail "dry run changed files"
  elif ! grep -q '^+## Internal change' <<<"$out" || ! grep -q '^+- \*\*Middle group note' <<<"$out" \
    || ! grep -q '^would remove docs/changes/older.md$' <<<"$out"; then
    fail "dry run output lacks the diff or the removals: $out"
  else
    pass "dry run shows the result and changes nothing"
  fi
else
  fail "dry run exited non-zero: $out"
fi

# --- the run ------------------------------------------------------------------
main_fixture "$work/run"
if out=$("$script" --root "$tree" 0.0.1 2>&1); then
  if diff -u "$expected_runbook" "$tree/docs/runbooks/f2-current-status.md"; then
    pass "status entries at the top of the runbook, newest first"
  else
    fail "runbook differs from the expected result"
  fi
  if diff -u "$expected_notes" "$tree/docs/operations/release-notes-0.0.1.md"; then
    pass "release notes under their sections, a new section at the end"
  else
    fail "release notes differ from the expected result"
  fi
  left=$(cd "$tree/docs/changes" && ls)
  if [ "$left" = README.md ]; then
    pass "change files removed, README.md kept"
  else
    fail "docs/changes holds: $left"
  fi
else
  fail "run exited non-zero: $out"
fi

before=$(snapshot)
if out=$("$script" --root "$tree" 0.0.1 2>&1) && grep -q 'nothing to do' <<<"$out" \
  && [ "$(snapshot)" = "$before" ]; then
  pass "a second run has nothing to do"
else
  fail "second run: $out"
fi

# --- a release with no notes yet ----------------------------------------------
new_tree "$work/fresh"
add_change one.md 1000 <<'EOF'
# Status: One — 2026-10-05

Text.

# Release note: Fixes

- **A fix.** Text.
EOF
if out=$("$script" --root "$tree" 0.0.2 2>&1) \
  && [ "$(cat "$tree/docs/operations/release-notes-0.0.2.md")" = "$(printf '# CapyCTL 0.0.2\n\nUnreleased.\n\n## Fixes\n\n- **A fix.** Text.')" ]; then
  pass "a missing release notes file is created"
else
  fail "missing notes file: $out"
fi

# --- refusals -----------------------------------------------------------------
refuse() { # label message ; bad change file on stdin
  local label=$1 message=$2 out
  new_tree "$work/refuse-$failures-$RANDOM"
  add_change good.md 1000 <<'EOF'
# Status: Good — 2026-10-05

Text.

# Release note: Fixes

- **Good.** Text.
EOF
  add_change bad.md 2000
  before=$(snapshot)
  if out=$("$script" --root "$tree" 0.0.1 2>&1); then
    fail "$label: accepted"
  elif [ "$(snapshot)" != "$before" ]; then
    fail "$label: files changed"
  elif ! grep -qF -- "$message" <<<"$out"; then
    fail "$label: message '$out' lacks '$message'"
  else
    pass "refuses $label"
  fi
}

refuse "an empty file" 'no "# Status: <title>" heading' </dev/null
refuse "a file without # Status:" '"# Release note:" must follow "# Status:"' <<'EOF'
# Release note: Fixes

- **x.** y.
EOF
refuse "a file without # Release note:" 'no "# Release note: <section>" heading' <<'EOF'
# Status: x

y.
EOF
refuse "another level-1 heading" 'unexpected heading' <<'EOF'
# Status: x

y.

# Notes

z.

# Release note: Fixes

- **x.** y.
EOF
refuse "text before # Status:" 'text before "# Status:"' <<'EOF'
stray
# Status: x

y.

# Release note: Fixes

- **x.** y.
EOF
refuse "a relative link without ../" 'link "configuration.md#x" must start with ../' <<'EOF'
# Status: x

y.

# Release note: Fixes

- **x.** See [settings](configuration.md#x).
EOF
refuse "none with text" 'a release note of "none" has no text' <<'EOF'
# Status: x

y.

# Release note: none

- **x.** y.
EOF
refuse "an empty release note" 'the release note is empty' <<'EOF'
# Status: x

y.

# Release note: Fixes
EOF

# --- the release page body (scripts/release-notes-body.sh) --------------------
body_script=$root/scripts/release-notes-body.sh
new_tree "$work/body"
mkdir -p "$tree/docs/guide"
cat >"$tree/docs/guide/a.md" <<'EOF'
# Guide

## A heading with `code` and llama.cpp

## Repeated

## Repeated

<a id="explicit-anchor"></a>

```bash
## not a heading
```
EOF
printf '# Settings\n' >"$tree/docs/operations/configuration.md"
cat >"$tree/docs/operations/release-notes-0.0.3.md" <<'EOF'
# CapyCTL 0.0.3

## Fixes

- **Guide.** See [a heading](../guide/a.md#a-heading-with-code-and-llamacpp) and
  [again](../guide/a.md#repeated-1), [with a title](../guide/a.md "the guide").
- **Same folder.** [settings](configuration.md), [the page](https://example.com/x),
  [mail](mailto:x@example.invalid), [here](#fixes), [guides](../guide/).
- **Code.** `[kept](../guide/missing.md)` stays.

```text
[kept too](../guide/missing.md)
```

[ref]: ../guide/a.md#explicit-anchor
EOF
g add -A
GIT_COMMITTER_DATE='@200 +0000' GIT_AUTHOR_DATE='@200 +0000' g commit -q -m release
release=$(g rev-parse HEAD)
printf '\n## Later\n' >>"$tree/docs/guide/a.md"
g add -A
GIT_COMMITTER_DATE='@300 +0000' GIT_AUTHOR_DATE='@300 +0000' g commit -q -m later
expected_body=$work/expected-body
sed "s/@SHA@/$release/g" >"$expected_body" <<'EOF'
# CapyCTL 0.0.3

## Fixes

- **Guide.** See [a heading](https://github.com/edurdias/capyctl/blob/@SHA@/docs/guide/a.md#a-heading-with-code-and-llamacpp) and
  [again](https://github.com/edurdias/capyctl/blob/@SHA@/docs/guide/a.md#repeated-1), [with a title](https://github.com/edurdias/capyctl/blob/@SHA@/docs/guide/a.md "the guide").
- **Same folder.** [settings](https://github.com/edurdias/capyctl/blob/@SHA@/docs/operations/configuration.md), [the page](https://example.com/x),
  [mail](mailto:x@example.invalid), [here](https://github.com/edurdias/capyctl/blob/@SHA@/docs/operations/release-notes-0.0.3.md#fixes), [guides](https://github.com/edurdias/capyctl/tree/@SHA@/docs/guide).
- **Code.** `[kept](../guide/missing.md)` stays.

```text
[kept too](../guide/missing.md)
```

[ref]: https://github.com/edurdias/capyctl/blob/@SHA@/docs/guide/a.md#explicit-anchor
EOF
before=$(snapshot)
if out=$("$body_script" --root "$tree" 0.0.3 "${release:0:12}" 2>"$work/body-err") \
  && [ "$out" = "$(cat "$expected_body")" ] && [ "$(snapshot)" = "$before" ]; then
  pass "the release body pins every relative link to the full release commit"
else
  fail "release body: $(cat "$work/body-err")"
  diff <(echo "$out") "$expected_body" >&2 || true
fi

refuse_body() { # label message ; the notes' extra line on stdin
  local label=$1 message=$2 out
  cat "$tree/docs/operations/release-notes-0.0.3.md" - >"$work/notes-extra"
  cp "$work/notes-extra" "$tree/docs/operations/release-notes-0.0.3.md"
  if out=$("$body_script" --root "$tree" 0.0.3 "$release" 2>"$work/body-err"); then
    fail "$label: accepted"
  elif [ -n "$out" ]; then
    fail "$label: printed a body"
  elif ! grep -qF -- "$message" "$work/body-err"; then
    fail "$label: message '$(cat "$work/body-err")' lacks '$message'"
  else
    pass "the release body refuses $label"
  fi
  g checkout -q -- docs/operations/release-notes-0.0.3.md
}

refuse_body "a heading added after the release commit" \
  "line 16: ../guide/a.md#later: docs/guide/a.md has no heading #later" <<'EOF'
- [later](../guide/a.md#later)
EOF
refuse_body "a heading inside a code fence" "has no heading #not-a-heading" <<'EOF'
- [fenced](../guide/a.md#not-a-heading)
EOF
refuse_body "a missing file" "line 16: ../guide/b.md: docs/guide/b.md does not exist" <<'EOF'
- [missing](../guide/b.md)
EOF
refuse_body "an anchor of a directory" "docs/guide has no heading #x" <<'EOF'
- [folder](../guide/#x)
EOF
refuse_body "a link out of the repository" "../../../x.md leaves the repository" <<'EOF'
- [out](../../../x.md)
EOF

echo
if [ "$failures" -gt 0 ]; then
  echo "assemble-changes.sh tests FAILED ($failures)" >&2
  exit 1
fi
echo "assemble-changes.sh tests passed"

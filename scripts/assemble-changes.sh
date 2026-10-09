#!/usr/bin/env bash
# Fold the pending change files into the status runbook and a release's notes.
#
#   scripts/assemble-changes.sh [--dry-run] [--root DIR] VERSION
#
# A pull request records its change in docs/changes/<slug>.md instead of
# editing docs/runbooks/f2-current-status.md and the release notes, so open
# pull requests stop conflicting on the top of those two files. When a release
# is cut, this script, for every change file (docs/changes/README.md aside):
#
#   - puts its status entry, as a `## <title>` section, at the top of the
#     runbook, newest change first;
#   - appends its release note to the `## <section>` it names in
#     docs/operations/release-notes-VERSION.md, after that section's last
#     bullet (the section is added at the end when the file has none, and the
#     file is created when it does not exist);
#   - removes it.
#
# A change is as new as the commit that added its file (rebase merges make
# that the merge time); a file not committed yet counts as the newest. Ties
# go by file name. Every file is checked before anything is written: a file
# that does not have the format docs/changes/README.md describes stops the run
# with nothing changed. --dry-run prints the result as a diff and changes
# nothing. --root assembles another checkout (the test uses a scratch one).
set -euo pipefail

dry_run=0
root=""
version=""
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) dry_run=1 ;;
    --root) root="${2:?--root needs a directory}"; shift ;;
    -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
    -*) echo "unknown argument: $1" >&2; exit 2 ;;
    *)
      [ -z "$version" ] || { echo "one VERSION only" >&2; exit 2; }
      version="$1"
      ;;
  esac
  shift
done
if [ -z "$version" ]; then
  echo "usage: scripts/assemble-changes.sh [--dry-run] [--root DIR] VERSION" >&2
  exit 2
fi
if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$ ]]; then
  echo "VERSION must look like 0.1.3, not '$version'" >&2
  exit 2
fi
if [ -z "$root" ]; then
  root=$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)
fi
root=$(cd "$root" && pwd)

changes_rel=docs/changes
runbook_rel=docs/runbooks/f2-current-status.md
notes_rel=docs/operations/release-notes-$version.md
runbook=$root/$runbook_rel
notes=$root/$notes_rel

[ -f "$runbook" ] || { echo "no status runbook at $runbook_rel" >&2; exit 1; }

files=()
while IFS= read -r f; do
  files+=("$f")
done < <(find "$root/$changes_rel" -maxdepth 1 -type f -name '*.md' ! -name README.md -printf '%f\n' 2>/dev/null | LC_ALL=C sort)
if [ ${#files[@]} -eq 0 ]; then
  echo "assemble-changes: no change files in $changes_rel; nothing to do"
  exit 0
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/capyctl-assemble.XXXXXX")
trap 'rm -rf "$work"' EXIT

# --- parse and check every file ---------------------------------------------
# Writes $out/title, $out/status, $out/section and $out/note. Headings inside
# fenced code blocks are text. A relative link must start with ../ so it
# resolves the same from docs/changes, docs/runbooks and docs/operations.
parse() { # file out
  awk -v out="$2" -v name="$1" '
    function fail(msg) { printf "%s:%d: %s\n", name, NR, msg > "/dev/stderr"; bad = 1; exit 1 }
    function check_links(s,   t) {
      while (match(s, /\]\([^)]*\)/)) {
        t = substr(s, RSTART + 2, RLENGTH - 3)
        sub(/^[ <]+/, "", t)
        if (t !~ /^(\.\.\/|#|https?:|mailto:)/)
          fail("link \"" t "\" must start with ../ (or be a URL or #anchor) to resolve after the move")
        s = substr(s, RSTART + RLENGTH)
      }
    }
    {
      is_fence = ($0 ~ /^ ? ? ?(```|~~~)/)
      if (!fenced && !is_fence && $0 ~ /^# /) {
        if ($0 ~ /^# Status: *[^ ]/) {
          if (part != "") fail("\"# Status:\" must be the first heading, and only once")
          part = "status"; title = $0; sub(/^# Status: */, "", title)
        } else if ($0 ~ /^# Release note: *[^ ]/) {
          if (part != "status") fail("\"# Release note:\" must follow \"# Status:\", and only once")
          part = "note"; section = $0; sub(/^# Release note: */, "", section)
          sub(/ +$/, "", section)
        } else {
          fail("unexpected heading; a change file has \"# Status: <title>\" then \"# Release note: <section>\"")
        }
        next
      }
      if (is_fence) fenced = !fenced
      if (part == "") {
        if ($0 !~ /^[ \t]*$/) fail("text before \"# Status:\"")
        next
      }
      if (!fenced && !is_fence) check_links($0)
      if (part == "status") s[++ns] = $0; else n[++nn] = $0
    }
    function emit(a, cnt, file,   i, j, kept) {
      i = 1; while (i <= cnt && a[i] ~ /^[ \t]*$/) i++
      j = cnt; while (j >= i && a[j] ~ /^[ \t]*$/) j--
      kept = j - i + 1
      printf "" > file
      for (; i <= j; i++) print a[i] > file
      return kept
    }
    END {
      if (bad) exit 1
      if (fenced) fail("unclosed code fence")
      if (title == "") fail("no \"# Status: <title>\" heading")
      if (section == "") fail("no \"# Release note: <section>\" heading (use \"none\" for no release note)")
      if (emit(s, ns, out "/status") <= 0) fail("the status entry is empty")
      kept = emit(n, nn, out "/note")
      if (tolower(section) == "none") {
        if (kept > 0) fail("a release note of \"none\" has no text")
        section = ""
      } else if (kept <= 0) fail("the release note is empty")
      print title > (out "/title")
      printf "%s", section > (out "/section")
    }
  ' "$root/$changes_rel/$1"
}

failed=0
keys=()
for f in "${files[@]}"; do
  mkdir -p "$work/parsed/$f"
  parse "$f" "$work/parsed/$f" || failed=1
  added=""
  if git -C "$root" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    added=$(git -C "$root" log -1 --format=%ct --diff-filter=A -- "$changes_rel/$f" 2>/dev/null || true)
  fi
  keys+=("${added:-9999999999} $f")
done
if [ "$failed" = 1 ]; then
  echo "assemble-changes: fix the change files above; nothing was changed" >&2
  exit 1
fi

# Newest first, ties by name.
newest_first=()
while IFS= read -r line; do
  newest_first+=("${line#* }")
done < <(printf '%s\n' "${keys[@]}" | LC_ALL=C sort -k1,1nr -k2,2)

# --- the runbook: every status entry before its first `## ` section ---------
block=$work/runbook-block
: >"$block"
for f in "${newest_first[@]}"; do
  {
    printf '## %s\n\n' "$(cat "$work/parsed/$f/title")"
    cat "$work/parsed/$f/status"
    printf '\n'
  } >>"$block"
done
awk -v block="$block" '
  function put(   l) { while ((getline l < block) > 0) print l; done = 1 }
  {
    is_fence = ($0 ~ /^ ? ? ?(```|~~~)/)
    if (!done && !fenced && $0 ~ /^## /) put()
    if (is_fence) fenced = !fenced
    print
  }
  END { if (!done) { print ""; put() } }
' "$runbook" >"$work/runbook.new"

# --- the release notes: oldest first, each after its section's last bullet --
if [ -f "$notes" ]; then
  cp "$notes" "$work/notes.new"
else
  printf '# CapyCTL %s\n\nUnreleased.\n' "$version" >"$work/notes.new"
fi
for ((i = ${#newest_first[@]} - 1; i >= 0; i--)); do
  f=${newest_first[$i]}
  section=$(cat "$work/parsed/$f/section")
  [ -n "$section" ] || continue
  awk -v section="$section" -v note="$work/parsed/$f/note" '
    { line[NR] = $0 }
    END {
      while ((getline l < note) > 0) body[++nb] = l
      joins = (body[1] ~ /^[-*] /)
      start = 0; last = 0; content = 0; fenced = 0
      for (i = 1; i <= NR; i++) {
        is_fence = (line[i] ~ /^ ? ? ?(```|~~~)/)
        heading = (!fenced && !is_fence && line[i] ~ /^##? /)
        if (is_fence) fenced = !fenced
        if (!start) { if (heading && line[i] == "## " section) start = i; continue }
        if (heading) break
        if (line[i] ~ /^[ \t]*$/) continue
        content = i
        if (!fenced && line[i] ~ /^[-*] /) { last = i; inbullet = 1 }
        else if (inbullet && line[i] ~ /^[ \t]+[^ \t]/) last = i
        else inbullet = 0
      }
      if (!start) {
        for (i = 1; i <= NR; i++) print line[i]
        j = NR; while (j > 0 && line[j] ~ /^[ \t]*$/) j--
        if (j == NR && NR > 0) print ""
        print "## " section; print ""
        for (k = 1; k <= nb; k++) print body[k]
        exit
      }
      at = last ? last : (content ? content : start)
      tight = (last && joins)
      for (i = 1; i <= NR; i++) {
        print line[i]
        if (i != at) continue
        if (!tight) print ""
        for (k = 1; k <= nb; k++) print body[k]
        if (i < NR && line[i + 1] !~ /^[ \t]*$/) print ""
      }
    }
  ' "$work/notes.new" >"$work/notes.next"
  mv "$work/notes.next" "$work/notes.new"
done

# --- write, or show --------------------------------------------------------
if [ "$dry_run" = 1 ]; then
  diff -u --label "a/$runbook_rel" --label "b/$runbook_rel" "$runbook" "$work/runbook.new" || true
  if [ -f "$notes" ]; then
    diff -u --label "a/$notes_rel" --label "b/$notes_rel" "$notes" "$work/notes.new" || true
  else
    diff -u --label /dev/null --label "b/$notes_rel" /dev/null "$work/notes.new" || true
  fi
  for f in "${newest_first[@]}"; do echo "would remove $changes_rel/$f"; done
  echo "assemble-changes: dry run, nothing changed"
  exit 0
fi

cp "$work/runbook.new" "$runbook"
cp "$work/notes.new" "$notes"
for f in "${newest_first[@]}"; do
  rm "$root/$changes_rel/$f"
  echo "assembled $changes_rel/$f"
done
echo "assemble-changes: ${#newest_first[@]} change file(s) folded into $runbook_rel and $notes_rel"

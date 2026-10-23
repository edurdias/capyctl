#!/usr/bin/env bash
# Print a release's notes as the GitHub release page's body.
#
#   scripts/release-notes-body.sh [--root DIR] VERSION COMMIT
#
# docs/operations/release-notes-VERSION.md links to the other documents by
# relative paths (../guide/engines.md#lmcache), which work in the repository
# and break on the release page. This prints the notes, as the checkout has
# them, with every relative link made absolute and pinned to COMMIT, the
# release commit:
#
#   https://github.com/edurdias/capyctl/blob/<full commit>/docs/guide/engines.md#lmcache
#
# Anchors are kept. A link to an anchor of the notes themselves points at the
# notes file. Absolute links (https:, mailto:, ...) and anything inside a code
# span or fence are left as they are.
#
# Every rewritten link is checked first: its file (or directory) must exist at
# COMMIT, and its anchor must be a heading of that file at COMMIT (GitHub's
# heading ids) or an explicit id or name there. Any link that does not resolve
# is reported with its line, and nothing is printed. Needs git and python3.
set -euo pipefail

root=""
args=()
while [ $# -gt 0 ]; do
  case "$1" in
    --root) root="${2:?--root needs a directory}"; shift ;;
    -h|--help) sed -n '2,21p' "$0"; exit 0 ;;
    -*) echo "unknown argument: $1" >&2; exit 2 ;;
    *) args+=("$1") ;;
  esac
  shift
done
if [ ${#args[@]} -ne 2 ]; then
  echo "usage: scripts/release-notes-body.sh [--root DIR] VERSION COMMIT" >&2
  exit 2
fi
version=${args[0]}
commit=${args[1]}
if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$ ]]; then
  echo "VERSION must look like 0.1.3, not '$version'" >&2
  exit 2
fi
if [ -z "$root" ]; then
  root=$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)
fi
root=$(cd "$root" && pwd)
notes_rel=docs/operations/release-notes-$version.md
[ -f "$root/$notes_rel" ] || { echo "no release notes at $notes_rel" >&2; exit 1; }
if ! sha=$(git -C "$root" rev-parse --verify --quiet "$commit^{commit}"); then
  echo "COMMIT '$commit' is not a commit in $root" >&2
  exit 1
fi

exec python3 - "$root" "$sha" "$notes_rel" <<'PY'
import posixpath
import re
import subprocess
import sys

REPOSITORY = "https://github.com/edurdias/capyctl"
root, sha, notes_rel = sys.argv[1:4]

FENCE = re.compile(r"^ {0,3}(`{3,}|~{3,})")
# An inline link or image destination: `](dest)` or `](dest "title")`.
INLINE = re.compile(r"\]\(\s*(<[^>]*>|[^\s)]+)((?:\s+\"[^\"]*\")?\s*\))")
# A link reference definition: `[label]: dest`.
REFERENCE = re.compile(r"^( {0,3}\[[^\]]+\]:\s*)(<[^>]*>|\S+)(.*)$")
CODE_SPAN = re.compile(r"(`+)(.+?)\1")
SCHEME = re.compile(r"^[A-Za-z][A-Za-z0-9+.-]*:")
HEADING = re.compile(r"^ {0,3}(#{1,6})[ \t]+(.*?)(?:[ \t]+#+)?[ \t]*$")
EXPLICIT = re.compile(r"""\b(?:id|name)\s*=\s*["']([^"']+)["']""")


def git(*args):
    return subprocess.run(["git", "-C", root, *args], capture_output=True, text=True)


kinds = {}


def kind(path):
    """`blob`, `tree` or None: what `path` is at the commit."""
    if path not in kinds:
        result = git("cat-file", "-t", "%s:%s" % (sha, path))
        kinds[path] = result.stdout.strip() if result.returncode == 0 else None
    return kinds[path]


def outside_fences(lines):
    """(number, line) of every line outside a fenced code block."""
    fence = None
    for number, line in enumerate(lines, 1):
        match = FENCE.match(line)
        if fence is None and match:
            fence = match.group(1)
            continue
        if fence is not None:
            if match and match.group(1)[0] == fence[0] and len(match.group(1)) >= len(fence):
                fence = None
            continue
        yield number, line


def slug(text):
    """GitHub's id for a heading: its text, lower case, punctuation dropped."""
    text = re.sub(r"!?\[([^\]]*)\]\([^)]*\)", r"\1", text)
    text = re.sub(r"<[^>]+>", "", text).replace("`", "")
    return re.sub(r"[^\w\- ]", "", text.lower()).replace(" ", "-")


anchors_by_path = {}


def anchors(path):
    """Every id a document's headings and explicit anchors give at the commit."""
    if path not in anchors_by_path:
        text = git("show", "%s:%s" % (sha, path)).stdout
        found, seen = set(), {}
        for _, line in outside_fences(text.splitlines()):
            heading = HEADING.match(line)
            if heading:
                base = slug(heading.group(2))
                count = seen.get(base, 0)
                seen[base] = count + 1
                found.add(base if count == 0 else "%s-%d" % (base, count))
            found.update(EXPLICIT.findall(line))
        anchors_by_path[path] = found
    return anchors_by_path[path]


problems = []


def pinned(destination, number):
    """`destination` made absolute at the commit, or itself when it is absolute."""
    bare = destination[1:-1] if destination.startswith("<") else destination
    if SCHEME.match(bare) or bare.startswith("//"):
        return destination
    target, _, anchor = bare.partition("#")
    if target:
        path = posixpath.normpath(posixpath.join(posixpath.dirname(notes_rel), target))
    else:
        path = notes_rel
    if path == ".." or path.startswith("../") or path.startswith("/"):
        problems.append("line %d: %s leaves the repository" % (number, bare))
        return destination
    what = kind(path)
    if what is None:
        problems.append("line %d: %s: %s does not exist at %s" % (number, bare, path, sha[:12]))
        return destination
    if anchor and (what != "blob" or anchor not in anchors(path)):
        problems.append("line %d: %s: %s has no heading #%s at %s"
                        % (number, bare, path, anchor, sha[:12]))
        return destination
    url = "%s/%s/%s/%s" % (REPOSITORY, what, sha, path)
    return url + ("#" + anchor if anchor else "")


def rewrite(line, number):
    """The line with every link outside a code span pinned."""
    reference = REFERENCE.match(line)
    if reference:
        return reference.group(1) + pinned(reference.group(2), number) + reference.group(3)
    parts = []
    last = 0
    for span in CODE_SPAN.finditer(line):
        parts.append((line[last:span.start()], True))
        parts.append((span.group(0), False))
        last = span.end()
    parts.append((line[last:], True))
    return "".join(
        INLINE.sub(lambda m: "](" + pinned(m.group(1), number) + m.group(2), text)
        if linked else text
        for text, linked in parts)


with open("%s/%s" % (root, notes_rel), encoding="utf-8") as handle:
    lines = handle.read().split("\n")
body = list(lines)
for number, line in outside_fences(lines):
    body[number - 1] = rewrite(line, number)
if problems:
    sys.stderr.write("release-notes-body: %d link(s) in %s do not resolve at %s:\n"
                     % (len(problems), notes_rel, sha))
    for problem in problems:
        sys.stderr.write("  " + problem + "\n")
    sys.exit(1)
sys.stdout.write("\n".join(body))
PY

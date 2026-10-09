# Status: Change files instead of runbook and release note edits — 2026-10-09 (branch `doc-changes-convention`)

Owner decision 2026-10-09. Every pull request added its entry at the top of
this runbook and to `docs/operations/release-notes-0.1.3.md`, so each merge
made every open pull request conflict. A pull request now adds one file,
`docs/changes/<short-slug>.md`, with `# Status: <title>` (this entry) and
`# Release note: <section>` (or `none`); the format is in
[docs/changes/README.md](../changes/README.md). When a release is cut,
`scripts/assemble-changes.sh [--dry-run] <version>` puts the status entries at
the top of the runbook, newest first by the commit that added each file, and
each release note after the last bullet of its section in
`release-notes-<version>.md` (adding a missing section at the end, or the
file), checks every file first (headings, `../` relative links) and removes
them. AGENTS.md, CONTRIBUTING.md and [Releasing](../operations/releasing.md)
describe the convention; the runbook stays the single status authority and no
existing entry moved. CI and `scripts/ci-local.sh` run
`scripts/test-assemble-changes.sh` (step `changes`) and shellcheck both
scripts.

Tests: `scripts/test-assemble-changes.sh` (dry run changes nothing; runbook and
notes compared byte for byte; second run is a no-op; missing notes file
created; eight refusals leave every file unchanged). It failed before the
script existed and passes after, with mawk and busybox awk. Not added: a check
that a pull request touching code adds a change file (proposed in the pull
request).

# Release note: none

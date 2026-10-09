# Change files

A pull request that changes behavior, or anything the status runbook tracks,
adds one file here, `docs/changes/<short-slug>.md`, instead of editing
[the status runbook](../runbooks/f2-current-status.md) or the release notes in
`docs/operations/`. Every pull request used to add its entry at the top of
those two files, so each merge made every other open pull request conflict;
separate files do not.

The runbook stays the single status authority. When a release is cut,
`scripts/assemble-changes.sh <version>` folds every file here into it and into
`docs/operations/release-notes-<version>.md`, then removes them (see
[Releasing](../operations/releasing.md)). Until then the files here are the
pending part of the runbook: read them with it.

## Format

Two sections, in this order:

````markdown
# Status: <title> — <date> (branch `<branch>`)

The entry the runbook would have had: owner decision and spec or ADR
references, what changed, the tests (failing before, passing after), the
statement that CPU and Fake-engine tests are not qualification, and the live
check still needed.

# Release note: <section>

- **What the user sees.** The release note, in the voice of the release notes.
````

- `# Status:` becomes the runbook heading `## <title>`; its text follows
  unchanged. The runbook lists the newest change first, by when the file
  landed on `main`.
- `# Release note:` names the release notes section the note goes under
  (`Memory`, `Requests`, `Fixes`, ...): the note is added after that section's
  last bullet, and a section the notes do not have yet is added at the end.
  Write `# Release note: none` with nothing under it for a change users do
  not see.
- Only those two headings may be level 1; use `##` or deeper inside a section.
  A `#` line inside a fenced code block is text.
- Relative links start with `../` (`../guide/engines.md#anchor`,
  `../operations/configuration.md`), so they resolve from here, from the
  runbook and from the release notes alike. The script refuses any other
  relative link.
- One file per pull request. If a later pull request changes the same work
  before a release, edit its file.

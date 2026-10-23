# Status: Release notes on the release page link to the release commit — 2026-10-23 (branch `fix/release-notes-links`)

`scripts/assemble-changes.sh` writes the release notes with repository-relative links
(`../guide/engines.md#lmcache`), which render in the repository but break when the notes
are pasted as a GitHub release's description: the 0.1.6 release page still has them
broken, and 0.1.7's links were pinned to its release commit by hand. The repository file
stays relative. `scripts/release-notes-body.sh <version> <commit>` prints the release page
body: the checkout's `docs/operations/release-notes-<version>.md` with every relative link
(inline, with a title, or a reference definition; a link to the notes' own anchor goes to
the notes file) made `https://github.com/edurdias/capyctl/blob/<full commit>/docs/...`
(`tree/` for a folder), anchors kept; absolute links, code spans and fenced blocks are
left as they are. Every rewritten link is checked first: its file must exist at that
commit and its anchor must be a heading there (GitHub's heading ids, duplicates numbered)
or an explicit `id`/`name`; otherwise it lists each failing link with its line and prints
nothing. `docs/operations/releasing.md` step 4 now uses it, also for correcting a
published release's notes. The script is in the shellcheck lists of `scripts/ci-local.sh`
and the CI workflow.

Checked against the real releases: on the current notes, `0.1.7 v0.1.7` prints exactly
the body published on the 0.1.7 release page (corrected by hand on 2026-10-23), and
`0.1.6 v0.1.6`, and every release from 0.1.0 to 0.1.5 at its tag, resolve all their links.

Tests: `scripts/test-assemble-changes.sh` (CI step `changes`) gains a scratch repository
whose notes hold each link form; the body is compared byte for byte with the expected one
for an abbreviated commit, and the script refuses, printing nothing, a heading added
after the release commit, a heading inside a code fence, a missing file, an anchor on a
folder and a link out of the repository. No product behavior changes.

# Release note: none

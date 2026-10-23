# Status: 0.1.7 release notes say when LMCache is used — 2026-10-23 (branch `docs/release-notes-017-lmcache`)

The 0.1.7 release notes said LMCache "is on by default (`lmcache: auto`)". A deployment
that parks deep, the default residency, runs without LMCache in 0.1.7: no engine, model
type and GPU combination is qualified for deep parking with it (ADR 0033 §7, "parking
wins"). The LMCache bullet now says so and points to "Parking with LMCache" for
`residency: restart_only`. The published GitHub release notes were corrected the same
day, with their links pinned to the release commit. Docs only.

# Release note: none

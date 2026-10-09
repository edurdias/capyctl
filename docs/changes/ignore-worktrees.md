# Status: Local git worktrees live under `.worktrees/` — 2026-10-09 (branch `chore/ignore-worktrees`)

Owner decision 2026-10-09. Contributors and agents create git worktrees under
`.worktrees/<name>` inside the checkout instead of as sibling directories.
`.gitignore` now ignores `/.worktrees/`, so they never show up as untracked
files. No code or behavior change.

# Release note: none

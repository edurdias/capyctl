# scripts/

## Private denylist

`scripts/verify-packaging.sh` checks the release binary and the embedded
runtime sources for names that must never land in a tracked file or a
shipped artifact — tool/agent names, personal names, lab-machine names, and
similar private identifiers.

The pattern list itself is not tracked (this repo's tree must never carry
those names). Maintainers keep it locally in `scripts/private-denylist.txt`,
one case-insensitive substring pattern per line; `#` starts a comment, blank
lines are ignored. The file is gitignored. It typically lists things like:
the coding-assistant tool/vendor name and its instruction-file name, the
lab's machine hostnames, and maintainers' personal names — never write these
literal names into this file or any other tracked file; only into the
gitignored denylist file itself.

If the file is absent, `verify-packaging.sh` skips the check (reported as
SKIPPED, not a failure) rather than guessing at names that must stay out of
the tracked tree.

Builder-identifying paths ($HOME, $CARGO_HOME, the checkout path) and, when
`scripts/live/matrix/hosts.local.env` is present, lab host identifiers are
checked separately and always run regardless of the denylist file.

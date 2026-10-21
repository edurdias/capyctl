# Status: Hosts under a systemd user service are no longer refused — 2026-10-21 (branch `fix/cgroup-path-at`)

SPEC §7.2 and the operations guide's "Memory limits" section
([install](../operations/install.md)): a host names the cgroup v2 cgroup that
bounded its memory reading in each host domain's `memory_source`
(`cgroup_v2:<cgroup>`). The controller checked that token with the alphabet it
uses for every other name (letters, digits, `-_.:/`), so a cgroup systemd
names with `@` (a user service, `user@1000.service`, or any template unit
instance, `foo@bar.service`) failed the inventory check and ended the session
with `host session authorization failed`. Found by the CI-shape deep gate,
which runs under `systemd-run --user --scope` and so places the tests' hosts
under `user@1000.service`.

`memory_source` now has its own check: letters, digits, `-_.:/@`, and `\`
only as the start of a systemd `\xNN` escape (`dev-disk-by\x2duuid`), up to
512 characters. Control characters, whitespace, other punctuation and
non-ASCII are still refused. Every other inventory name keeps the narrower
check. The token is status only: it reaches `capyctl inspect host --json` and
the session view, never a file path or an SQL statement, and the host does not
validate it before publishing.

Tests (failing before, passing after where new): the controller accepts a
session whose memory source is a user-service cgroup or a template-unit
cgroup and shows it in the host view, and still refuses one with a newline,
an escape sequence, a space, or past 512 characters (T07, T26); unit tests
cover `\xNN` escapes, the length bound at 512 and 513, malformed escapes and
other refused characters, and that other names still refuse `@` (T26). The
CI-shape deep gate passes. CPU and Fake-engine tests are not qualification. A
live check is still needed: a role run as a systemd user service or template
unit joins its server and reports its cgroup.

# Release note: Fixes

- **Hosts running under a systemd user service or a template unit were
  refused.** A host whose memory is bounded by a cgroup named with `@`
  (`user@1000.service`, `capyctl@host.service`) reported it, and the server
  ended its session with `host session authorization failed`. Such hosts now
  join.

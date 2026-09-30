# Rename mllm to CapyCTL

Status: approved design, 2026-09-30 (owner). Recorded as ADR 0022.

## Goal

The project, its binary and its repository are renamed from `mllm` to
CapyCTL (`capyctl`) before the first public release. 0.1.0 is the first
public release, so no public install depends on the old names; the rename is
complete and keeps no aliases.

Success means: no tracked file outside the history set mentions `mllm`; the
binary, crates, variables, paths, units, archives, installer, site and docs
use the new name; a check prevents the old name from returning; all local
checks pass; a live run shows `capyctl` end to end.

## Names

| Old | New |
|---|---|
| binary `mllm` | `capyctl` |
| crates `mllm-*`, Rust paths `mllm_*` | `capyctl-*`, `capyctl_*` |
| variables `MLLM_*` | `CAPYCTL_*` |
| `~/.local/state/mllm`, `~/.config/mllm`, `~/.local/share/mllm` | the same with `capyctl` |
| `/etc/mllm`, `/var/lib/mllm`, service user and group `mllm` | `/etc/capyctl`, `/var/lib/capyctl`, `capyctl` |
| units `mllm-server|host|standalone.service` | `capyctl-*.service` |
| protocol package, internal headers, temp-file prefixes, runtime helper names | `capyctl` equivalents |
| archives `mllm-<version>-linux-<arch>.tar.gz` | `capyctl-<version>-linux-<arch>.tar.gz` |
| installer default repository `edurdias/mllm` | `edurdias/capyctl` |
| site `https://edurdias.github.io/mllm/` | `https://edurdias.github.io/capyctl/` |

Prose uses "CapyCTL" for the project and `capyctl` for the command, paths and
code. The README header and the site hero show the logo and the tagline
"Control what runs next"; the rest stays plain technical prose.

JSON field names do not change. Values that carry the product name change
with it.

## History set (not renamed)

`docs/plans/`, `docs/specs/` (other than this file), the existing entries of
`docs/runbooks/f2-current-status.md`, and ADRs 0001–0021 record the past and
keep their text. ADR 0022 records the rename and maps the old names to the
new ones so those records stay readable. Git history is not rewritten.

## Logo

Only the logo is added: `docs/brand/capyctl-logo.png` (1536×1024, the
owner's current logo), with `docs/brand/README.md` saying it is temporary
until the final brand exports arrive. The README header and the site hero
use it as it is. No favicon, social image, cropped mark or colour theme
changes are part of this change.

## Method

1. `git mv` every path with `mllm` in its name (crate directories, units,
   runtime helpers, fixtures).
2. One scripted substitution pass over tracked files outside the history set,
   in this order: `MLLM_` → `CAPYCTL_`, `mllm-` → `capyctl-`, `mllm_` →
   `capyctl_`, `Mllm` → `Capyctl`, `mllm` → `capyctl`.
3. A hand pass over prose: "CapyCTL" where the project is meant; logo and
   tagline in the README and on the site; ADR 0022; the release notes gain an
   upgrade note (move `~/.local/state/mllm` to `~/.local/state/capyctl`, or
   start fresh; the same for `/etc` and `/var/lib`, and reinstall the units).
4. A leftover check (`scripts/check-name.sh`, run by
   `scripts/verify-packaging.sh` and by CI) fails when `mllm` appears in any
   tracked file outside the history set, matched case-insensitively.

Machine-local, untracked files (`scripts/private-denylist.txt`,
`scripts/live/matrix/hosts.local.env`, `site/voice-denylist.local.txt`) are
updated by hand where they name old paths; they are never committed.

## Verification

The full local check set (formatting, Clippy with warnings denied, core and
workspace suites, packaging with no SKIP lines, installer tests against
`capyctl-*` archives, site check, harness syntax), the leftover check, and a
live run on the laptop: `capyctl` in a terminal and piped, the new state and
config paths, the units' names, and the JSON banner. CPU and Fake-engine
tests are not qualification.

## Rollout

1. Pull request, then merge.
2. `gh repo rename capyctl`; update the local remote; move the clone to
   `~/projects/edurdias/capyctl`; update the local memory that names the path.
3. Lab hosts and the laptop reinstall once from the new archives; old state
   under `mllm` paths is not migrated.
4. Rebuild 0.1.0 as `capyctl-0.1.0-*`.

## Out of scope

The other brand assets, a colour theme, a vector logo, compatibility aliases
for old names, and rewriting git history.

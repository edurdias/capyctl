# ADR 0022 — The project is CapyCTL

**Status:** Accepted (owner decision, 2026-09-30).
**Design:** `docs/specs/2026-09-30-rename-capyctl-design.md`.

## Context

The project was developed as `mllm`. Before its first public release (0.1.0)
the owner renamed it CapyCTL. No public install depends on the old names.

## Decision

Everything is renamed, with no aliases for the old names:

| Old | New |
|---|---|
| binary `mllm`, crates `mllm-*` | `capyctl`, `capyctl-*` |
| variables `MLLM_*` | `CAPYCTL_*` |
| `~/.local/state/mllm`, `~/.config/mllm`, `~/.local/share/mllm` | the same with `capyctl` |
| `/etc/mllm`, `/var/lib/mllm`, user and group `mllm` | `/etc/capyctl`, `/var/lib/capyctl`, `capyctl` |
| units `mllm-server|host|standalone.service` | `capyctl-*.service` |
| archives `mllm-<version>-linux-<arch>.tar.gz` | `capyctl-<version>-linux-<arch>.tar.gz` |
| repository `edurdias/mllm`, site `edurdias.github.io/mllm` | `edurdias/capyctl`, `edurdias.github.io/capyctl` |

Prose says CapyCTL; commands, paths and code say `capyctl`.

## Consequences

Plans, specs, earlier status entries and ADRs 0001–0021 keep the old name;
read `mllm` there as `capyctl` using the table above. Git history is not
rewritten. Existing state under the old paths is not migrated: move it or
start fresh (see the 0.1.0 release notes). `scripts/check-name.sh` keeps the
old name out of everything else.

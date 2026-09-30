# Rename to CapyCTL Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rename the project, binary and every tracked name from `mllm` to CapyCTL (`capyctl`), with no aliases, before the first public release.

**Architecture:** A name check script defines the allowlist and fails on any leftover `mllm`; a one-shot scripted pass moves paths with `git mv` and substitutes names in a fixed order outside the history set; a hand pass fixes prose, adds the logo, tagline, ADR 0022 and the upgrade note; the check is wired into packaging verification and CI.

**Tech Stack:** bash, git, perl (in-place substitution), Rust/cargo, Node site build.

**Spec:** `docs/specs/2026-09-30-rename-capyctl-design.md`

## Global Constraints

- Binary `capyctl`; crates `capyctl-*`; Rust paths `capyctl_*`; variables `CAPYCTL_*`.
- Paths `~/.local/state/capyctl`, `~/.config/capyctl`, `~/.local/share/capyctl`, `/etc/capyctl`, `/var/lib/capyctl`; service user and group `capyctl`; units `capyctl-server|host|standalone.service`.
- Archives `capyctl-<version>-linux-<arch>.tar.gz`; installer default repository `edurdias/capyctl`; site `https://edurdias.github.io/capyctl/`.
- Substitution order: `MLLM_` → `CAPYCTL_`, `mllm-` → `capyctl-`, `mllm_` → `capyctl_`, `Mllm` → `Capyctl`, `mllm` → `capyctl`.
- History set, never edited: `docs/plans/`, `docs/specs/`, existing entries of `docs/runbooks/f2-current-status.md`, ADRs 0001–0021.
- Allowed to name `mllm` besides the history set: ADR 0022 and the upgrade note in `docs/operations/release-notes-0.1.0.md`.
- Prose: "CapyCTL" for the project, `capyctl` for the command, paths and code. Tagline "Control what runs next" in the README header and the site hero only.
- Logo: only `docs/brand/capyctl-logo.png` (the owner's 1536×1024 file), used as-is. No favicon, social image, cropped mark or colour changes.
- JSON field names unchanged.
- No AI attribution, machine names, IPs or home paths in committed files or commit messages. Untracked machine-local files are never committed or printed.
- CPU and Fake-engine tests are not qualification.

## Review Focus

1. **A real engine directory whose name contains `mllm`** (for example `~/mllm-vllm-venv2` on a lab host) — tracked docs and tests may mention one as an example and get renamed, but the machine-local `scripts/live/matrix/hosts.local.env` must keep the real paths. Pinned in Task 4.
2. **Substitution inside the history set** — a bulk pass that touches `docs/plans/` or old runbook entries corrupts the record. Pinned in Task 1 (the check allowlist) and Task 2 (the pass excludes the set; a git diff check proves no history file changed).
3. **`Cargo.lock` and `site/package-lock.json` consistency** — renamed package names must still build with `--locked` and `npm ci`. Pinned in Task 2 and Task 4.
4. **Words that merely contain the letters** — the case-insensitive pass must not hit unrelated words; `grep -iow '[a-z]*mllm[a-z]*'` over the tree before the pass lists every distinct token so any false hit is seen. Pinned in Task 2.
5. **The installer and site pointing at the renamed repository before GitHub is renamed** — download URLs 404 until the repo rename; the local installer tests use `file://` releases so they pass regardless. Pinned in Task 4 (installer fixtures) and the rollout task.

---

## File Structure

- Create `scripts/check-name.sh` — fails when `mllm` appears in tracked files outside its allowlist.
- Create `docs/design/adr/0022-rename-capyctl.md`.
- Create `docs/brand/capyctl-logo.png`, `docs/brand/README.md`.
- Move `crates/mllm-*` → `crates/capyctl-*`, `crates/mllm-protocol/proto/mllm/` → `crates/capyctl-protocol/proto/capyctl/`, `packaging/systemd/*/mllm-*.service` → `capyctl-*.service`, `runtime/mllm_vllm_guard.py` → `runtime/capyctl_vllm_guard.py`, `runtime/tests/test_mllm_vllm_guard.py` → `runtime/tests/test_capyctl_vllm_guard.py`.
- Modify every other tracked file outside the history set that names `mllm` (substitution pass), then README, guides, operations docs, SPEC, AGENTS, community files and the site by hand.
- Modify `scripts/verify-packaging.sh`, `.github/workflows/ci.yml` (run the check).

---

### Task 1: Name check and ADR 0022

**Files:**
- Create: `scripts/check-name.sh`
- Create: `docs/design/adr/0022-rename-capyctl.md`

**Interfaces:**
- Produces: `scripts/check-name.sh` — exit 0 when clean, exit 1 listing `path:line` hits otherwise; run from the repository root.

- [ ] **Step 1: Write the check**

Create `scripts/check-name.sh` (mode 0755):

```bash
#!/usr/bin/env bash
# ADR 0022: the project is CapyCTL (`capyctl`). No tracked file may name the
# old project name outside the history set, which records the past, and the
# two places that must name it (ADR 0022 and the 0.1.0 upgrade note).
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

allowed=(
  'docs/plans/'
  'docs/specs/'
  'docs/design/adr/00[01][0-9]-'
  'docs/design/adr/002[01]-'
  'docs/design/adr/0022-rename-capyctl.md'
  'docs/runbooks/f2-current-status.md'
  'docs/operations/release-notes-0.1.0.md'
  'scripts/check-name.sh'
)
pattern=$(printf '^%s|' "${allowed[@]}")
pattern=${pattern%|}

hits=$(git grep -n -i -I 'mllm' -- . | grep -Ev "$pattern" || true)
if [ -n "$hits" ]; then
  printf '%s\n' "$hits" | head -50 >&2
  echo "check-name: $(printf '%s\n' "$hits" | wc -l) line(s) still name the old project name" >&2
  exit 1
fi
echo "check-name: ok"
```

The runbook stays allowed as a whole file because its earlier entries are history; the new entry this change adds uses the new name.

- [ ] **Step 2: Run it to see it fail on the current tree**

Run: `scripts/check-name.sh; echo "exit $?"`
Expected: many hits (for example `Cargo.toml`, `README.md`) and `exit 1`.

- [ ] **Step 3: Write ADR 0022**

Create `docs/design/adr/0022-rename-capyctl.md`:

```markdown
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
```

- [ ] **Step 4: Commit**

```bash
git add scripts/check-name.sh docs/design/adr/0022-rename-capyctl.md
git commit -m "docs: record ADR 0022 and add the name check"
```

---

### Task 2: Scripted rename

**Files:**
- Move: every tracked path containing `mllm` outside the history set (list in File Structure).
- Modify: every tracked text file outside the history set containing `mllm`, including `Cargo.toml`, `Cargo.lock`, `site/package.json`, `site/package-lock.json`.

**Interfaces:**
- Consumes: `scripts/check-name.sh` (Task 1).
- Produces: a tree where `cargo build --workspace --locked` succeeds and the binary is `target/<profile>/capyctl`.

- [ ] **Step 1: List every distinct token before the pass**

Run: `git grep -ohiI '[a-z0-9_.-]*mllm[a-z0-9_.-]*' -- . ':!docs/plans' ':!docs/specs' | tr 'A-Z' 'a-z' | sort | uniq -c | sort -rn > /tmp/mllm-tokens.txt; wc -l /tmp/mllm-tokens.txt`

Read the list. Every token must be a name of this project (crate, variable, path, unit, prefix). If a token is part of an unrelated word or an external name that must not change (for example an engine directory on a real machine, or a third-party package), write it down: Step 3 skips it with a per-token exclusion, and the Task 3 report names it.

- [ ] **Step 2: Move paths**

```bash
set -e
for c in crates/mllm-*; do git mv "$c" "crates/capyctl-${c#crates/mllm-}"; done
git mv crates/capyctl-protocol/proto/mllm crates/capyctl-protocol/proto/capyctl
for f in packaging/systemd/*/mllm-*.service; do git mv "$f" "${f%/*}/capyctl-${f##*/mllm-}"; done
git mv runtime/mllm_vllm_guard.py runtime/capyctl_vllm_guard.py
git mv runtime/tests/test_mllm_vllm_guard.py runtime/tests/test_capyctl_vllm_guard.py
git ls-files | grep -i mllm | grep -Ev '^docs/(plans|specs)/' || echo "no paths left"
```

Expected: `no paths left`, or only history-set paths (none are expected).

- [ ] **Step 3: Substitute names in order**

```bash
set -e
git ls-files -z -- . \
  ':!docs/plans' ':!docs/specs' \
  ':!docs/design/adr/00[01][0-9]-*' ':!docs/design/adr/0020-*' ':!docs/design/adr/0021-*' \
  ':!docs/design/adr/0022-rename-capyctl.md' \
  ':!docs/runbooks/f2-current-status.md' ':!scripts/check-name.sh' \
  | xargs -0 grep -lIi 'mllm' \
  | while IFS= read -r f; do
      perl -pi -e 's/MLLM_/CAPYCTL_/g; s/mllm-/capyctl-/g; s/mllm_/capyctl_/g; s/Mllm/Capyctl/g; s/MLLM/CAPYCTL/g; s/mllm/capyctl/g' "$f"
    done
git diff --stat -- docs/plans docs/specs docs/runbooks/f2-current-status.md 'docs/design/adr/00[01]*' | tail -1
```

Expected: the last command prints nothing (no history file changed). `docs/operations/release-notes-0.1.0.md` is substituted here too; Task 3 adds its upgrade note, which names the old paths on purpose.

`s/MLLM/CAPYCTL/g` runs after `MLLM_` to catch uppercase names without an underscore (for example `MLLM` in a heading).

If Step 1 found tokens to keep, add a negative lookahead for each to the perl expression (for example `s/mllm(?!-vllm-venv2)/capyctl/g` only if that exact external name must stay) and record it.

- [ ] **Step 4: Build**

Run: `cargo build --workspace --locked 2>&1 | tail -5 && ls target/debug/capyctl`
Expected: build succeeds; `target/debug/capyctl` exists. If `--locked` fails because `Cargo.lock` is out of date, run `cargo build --workspace` once, confirm the only lock changes are package names (`git diff Cargo.lock | grep '^[+-]name' | head`), then continue.

- [ ] **Step 5: Run the crate tests that exercise names and paths**

Run: `cargo test -p capyctl-cli -p capyctl-config --all-targets --no-fail-fast --locked 2>&1 | grep -E '^test result|FAILED|panicked' | sort | uniq -c`
Expected: every `test result: ok`. A failure is a test expecting an old literal that the pass did not reach (for example a string built from parts); fix it to the new name.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "refactor: rename mllm to capyctl in code, packaging and scripts"
```

---

### Task 3: Prose, logo, tagline and upgrade note

**Files:**
- Create: `docs/brand/capyctl-logo.png` (copy of the owner's file, provided at dispatch), `docs/brand/README.md`
- Modify: `README.md`, `docs/guide/*.md`, `docs/operations/*.md`, `docs/SPEC.md`, `AGENTS.md`, `CONTRIBUTING.md`, `SUPPORT.md`, `SECURITY.md`, `CODE_OF_CONDUCT.md`, `site/` pages and config, `docs/operations/release-notes-0.1.0.md`, `docs/runbooks/f2-current-status.md` (new entry only)

**Interfaces:**
- Consumes: the renamed tree (Task 2).

- [ ] **Step 1: Project name in prose**

In the files listed above, where a sentence means the project (not the command, a path or code), change `capyctl` to `CapyCTL`. Leave `capyctl` inside code spans, code blocks, command lines, paths, URLs and identifiers. Headings that name the project become `CapyCTL`. Example: "capyctl gives you one OpenAI-compatible endpoint" → "CapyCTL gives you one OpenAI-compatible endpoint"; "`capyctl start standalone`" stays.

- [ ] **Step 2: Logo and tagline**

Copy the logo to `docs/brand/capyctl-logo.png`. Create `docs/brand/README.md`:

```markdown
# Brand

`capyctl-logo.png` is the current CapyCTL logo (1536×1024 PNG). It is
temporary until the final brand exports replace it; keep the file name so the
README and the site pick up the replacement without changes.
```

In `README.md`, replace the first line with:

```markdown
<p align="center"><img src="docs/brand/capyctl-logo.png" alt="CapyCTL" width="480"></p>

# CapyCTL

Control what runs next.
```

On the site, copy the same file to `site/public/brand/capyctl-logo.png` and show it at the top of the landing page with the tagline "Control what runs next" beneath it, reusing the existing hero component's layout (read `site/src/` to find it). Add nothing else: no favicon, social image or colour changes.

- [ ] **Step 3: Upgrade note**

In `docs/operations/release-notes-0.1.0.md`, under "Upgrading from a release candidate", add:

```markdown
- The project is now CapyCTL. The binary is `capyctl`, variables start with
  `CAPYCTL_`, and the services are `capyctl-server`, `capyctl-host` and
  `capyctl-standalone`. Nothing reads the old `mllm` names. To keep existing
  state, stop the old services and move `~/.local/state/mllm` to
  `~/.local/state/capyctl` and `~/.config/mllm` to `~/.config/capyctl` (for a
  system install, `/etc/mllm` to `/etc/capyctl` and `/var/lib/mllm` to
  `/var/lib/capyctl`, owned by a `capyctl` user), then install the new units;
  or start fresh.
```

- [ ] **Step 4: Status runbook entry**

Add at the top of `docs/runbooks/f2-current-status.md` (below the title):

```markdown
## Rename to CapyCTL — 2026-09-30

ADR 0022: the project, binary and repository are CapyCTL (`capyctl`), with no
aliases for the old names. Crates, variables, paths, units, archives, the
installer, the site and the docs use the new name; earlier entries below keep
the old one. `scripts/check-name.sh` keeps it out of everything else. CPU and
Fake-engine tests are not qualification.
```

- [ ] **Step 5: Name check**

Run: `scripts/check-name.sh`
Expected: `check-name: ok`. Any hit outside the allowlist is fixed in place.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "docs: name the project CapyCTL and add the logo"
```

---

### Task 4: Wire the check, machine-local files, full verification

**Files:**
- Modify: `scripts/verify-packaging.sh` (run `scripts/check-name.sh` as a check), `.github/workflows/ci.yml` (a step running it; add it to the ShellCheck list)
- Modify (machine-local, never committed or printed): `scripts/private-denylist.txt`, `scripts/live/matrix/hosts.local.env`, `site/voice-denylist.local.txt` — only where they name the project's own paths (state directories, units); real engine directories keep their names.

- [ ] **Step 1: Wire the check**

In `scripts/verify-packaging.sh`, next to the other static checks, add:

```bash
if scripts/check-name.sh >"$work/check-name.log" 2>&1; then
  pass "no tracked file names the old project name (ADR 0022)"
else
  fail "tracked files still name the old project name:"
  cat "$work/check-name.log" >&2
fi
```

(use the script's existing `pass`/`fail` helpers and `$work` directory; read the file for their exact names). In `.github/workflows/ci.yml`, add `scripts/check-name.sh` to the ShellCheck step's file list and a step `- name: Name check` / `run: scripts/check-name.sh` after formatting.

- [ ] **Step 2: Machine-local files**

Open each untracked file listed above without printing it. Replace the project's own old paths and names (for example a state directory `mllm` or a unit name) with the new ones; leave engine directories and anything outside the project as they are. Confirm with `git status --short` that none of them is tracked.

- [ ] **Step 3: Full local verification**

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test -p capyctl-adapters -p capyctl-store -p capyctl-controller -p capyctl-management -p harness --all-targets --no-fail-fast --locked -- --test-threads=4
cargo test --workspace --all-targets --locked
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s runtime/tests -p 'test_*.py'
scripts/check-name.sh
scripts/verify-packaging.sh
scripts/test-install.sh
(cd site && npm ci && MLLM_PUBLISH=1 npm run check)
bash -n scripts/live/matrix/*.sh scripts/live/matrix/rows/*.sh
```

The site variable is renamed by the pass; use whatever `site/` now reads (search `PUBLISH` in `site/scripts`), for example `CAPYCTL_PUBLISH=1`. The runtime suite needs `TMS_SOURCE_ARCHIVE` as described in `CONTRIBUTING.md`; if it is not set up, report it as not run rather than failed.

Expected: all pass; packaging prints no SKIP lines; the archive is `capyctl-0.1.0-linux-x86_64.tar.gz`.

- [ ] **Step 4: Live check on the laptop**

Build `cargo build --release -p capyctl-cli`. In a fresh state directory under the home directory (not `/tmp`): `target/release/capyctl --version`; `capyctl engine add ~/mllm-vllm-venv` (the laptop's real engine directory keeps its name); start standalone under a pseudo-terminal (`script -qfc "<cmd>" /dev/null`) and confirm the text banner shows the `capyctl` state path; start it again with output redirected and confirm the JSON banner; `capyctl list hosts`; stop the role by its exact PID; remove the state directory; confirm no process and no GPU compute app is left. Do not deploy models.

- [ ] **Step 5: Commit**

```bash
git add scripts/verify-packaging.sh .github/workflows/ci.yml
git commit -m "ci: keep the old project name out of tracked files"
```

---

### Task 5: Rollout (controller, after merge)

- [ ] **Step 1:** Push the branch, open the PR against `main` (no attribution lines), merge once local checks are green.
- [ ] **Step 2:** `gh repo rename capyctl --repo edurdias/mllm --yes`; `git remote set-url origin https://github.com/edurdias/capyctl.git`; `git fetch`.
- [ ] **Step 3:** Move the clone: `mv ~/projects/edurdias/mllm ~/projects/edurdias/capyctl`; update the persistent memory entry that names the clone path and repository.
- [ ] **Step 4:** Check `gh repo view edurdias/capyctl --json name,url` and that `https://github.com/edurdias/mllm` redirects.

# Status: Releasing: per-architecture packaging checks and installing a draft — 2026-10-15 (branch `docs/releasing-draft-install`)

Found while releasing 0.1.3. `releasing.md` step 3 suggested passing both tarballs to
`scripts/verify-packaging.sh`; the script checks only the first and runs its binary, so the
aarch64 tarball failed on an x86_64 machine (`Exec format error`, and the install tests looked
for a tarball of the host's architecture). Each architecture is now checked on a machine of that
architecture (0.1.3: x86_64 on a maintainer machine, aarch64 on a GB10, 24 checks each, no SKIP),
with a temporary `SHELLCHECK` binary where none is installed.

Step 5 said a draft installs with `gh auth login` or `GITHUB_TOKEN`. GitHub's release-by-tag API
returns 404 for a draft, so `install.sh` with only a token reports the release as not found. The
step now gives the two ways that work: `gh` logged in on the host, or the draft's assets copied
into a directory and `CAPYCTL_INSTALL_BASE_URL=file://...` (the installer still verifies
`SHA256SUMS`), with `PREFIX` to keep the check apart from an installed capyctl. It also names
the several-machines guide for the server and host (both on one machine), since the one-machine
guide covers standalone only. Docs only; no behaviour changed.

# Release note: none

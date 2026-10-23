# Releasing

Only the owner publishes a release. Contributors and automation stop at a draft.

1. **Bump the version.** On a branch, set `version` in the workspace
   `Cargo.toml`, run `cargo update -w` so `Cargo.lock` follows, and update any
   documented version examples.

   Then fold the pending change files into the status runbook and the release
   notes. Pull requests do not edit either; each adds a file under
   `docs/changes/` ([format](../changes/README.md)) with a status entry and a
   release note. Preview, then assemble:

   ```bash
   scripts/assemble-changes.sh --dry-run <version>
   scripts/assemble-changes.sh <version>
   ```

   This puts every status entry at the top of
   `docs/runbooks/f2-current-status.md`, newest first (by when its file landed
   on `main`), appends every release note to its section of
   `docs/operations/release-notes-<version>.md` (creating the file, or a
   section it lacks, when needed) and removes the change files, keeping
   `docs/changes/README.md`. A malformed change file stops the run before
   anything is written; fix it and run again. Read the assembled notes as a
   whole: order the sections, merge repeated ones, drop the `Unreleased.`
   line, and commit the result with the version bump.

   Merge the pull request once the verification in `CONTRIBUTING.md` passes.

2. **Build both architectures** from the merged `main`, in one of two ways:
   - Run the **Release build** workflow manually (Actions → Release build → Run
     workflow) with the exact Rust version `rust-toolchain.toml` pins (`1.99.0`
     today). It builds x86_64 and aarch64, checks the archives are reproducible, runs
     `scripts/verify-packaging.sh`, and uploads a `release-assets` artifact
     holding both tarballs, `install.sh` and `SHA256SUMS`. It never publishes.
   - Or build locally on one x86_64 and one aarch64 Linux machine with the same
     Rust version: `packaging/release.sh dist` on each, copy both tarballs and
     their `.sha256` files into one directory with `install.sh`, then run
     `packaging/release.sh --sums <dir>`.

3. **Run the privacy scan locally.** Hosted runners do not have the private
   denylist, so their packaging check reports the denylist and lab-host checks
   as `SKIP`. Before publishing, run on a maintainer machine that has
   `scripts/private-denylist.txt` and `scripts/live/matrix/hosts.local.env`:

   ```bash
   CAPYCTL_VERIFY_STRICT=1 scripts/verify-packaging.sh <dir>/capyctl-<version>-linux-<arch>.tar.gz
   ```

   The script checks the first tarball it is given and runs its binary, so
   check each architecture on a machine of that architecture (an x86_64
   machine for the x86_64 tarball, an aarch64 one for aarch64), with the
   release's `SHA256SUMS`, `install.sh` and both tarballs in the same
   directory. A `shellcheck` binary in a temporary directory, named by
   `SHELLCHECK=...`, will do on a machine without one. Every check must pass
   with no `SKIP` lines, for both architectures. Remove the denylist copy from
   any machine it was copied to for the check.

4. **Create a draft release** tagged `v<version>` on the release commit and
   upload the two tarballs, their `.sha256` files, `install.sh` and
   `SHA256SUMS`. Its description is the release notes with their links
   pinned to that commit: the notes link to the other documents by relative
   paths, which work in the repository and break on the release page.

   ```bash
   scripts/release-notes-body.sh <version> <release commit> >release-body.md
   ```

   It prints `docs/operations/release-notes-<version>.md` with every relative
   link made `https://github.com/edurdias/capyctl/blob/<commit>/docs/...`,
   anchors kept, and fails, printing nothing, if a link's file or heading
   does not exist at that commit; fix the notes and run it again. Paste
   `release-body.md` as the description (or pass it with
   `gh release create --draft --notes-file release-body.md`). To correct a
   published release's notes later, run it on the corrected notes with the
   same release commit.

5. **Check it live.** Install from the draft on a real host with
   `install.sh --version v<version>`, start the server and a host
   ([Several machines](../guide/several-machines.md), both on one machine
   works), and run a deployment on a native engine. CPU and Fake-engine tests
   are not qualification; a release is not checked until a native engine has
   served a request.

   A draft is not reachable by `GITHUB_TOKEN` alone: GitHub's release-by-tag
   API returns 404 for a draft, so `install.sh` finds nothing. Either log in
   with `gh auth login` on that host, or download the draft's tarball for the
   host's architecture and `SHA256SUMS` (`gh release download v<version> -p
   ...` on a machine where `gh` is logged in), copy them into one directory on
   the host, and point the installer at it:

   ```bash
   CAPYCTL_INSTALL_BASE_URL=file:///path/to/that/directory \
     sh install.sh --version v<version>
   ```

   The installer still verifies the tarball against `SHA256SUMS`. Use
   `PREFIX=...` to keep the check apart from a capyctl already installed on
   that host.

6. **Publish** the draft. The site's `install.sh` then resolves the new release
   as the latest.

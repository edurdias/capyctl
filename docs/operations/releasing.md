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
     workflow) with an exact Rust version such as `1.90.0`. It builds x86_64 and
     aarch64, checks the archives are reproducible, runs
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
   CAPYCTL_VERIFY_STRICT=1 scripts/verify-packaging.sh <dir>/capyctl-*.tar.gz
   ```

   Every check must pass with no `SKIP` lines.

4. **Create a draft release** tagged `v<version>` on the release commit and
   upload the two tarballs, their `.sha256` files, `install.sh` and
   `SHA256SUMS`. Paste the release notes as the description.

5. **Check it live.** Install from the draft on a real host with
   `install.sh --version v<version>` (a draft needs `gh auth login` or
   `GITHUB_TOKEN`), start the server and a host, and run a deployment on a
   native engine. CPU and Fake-engine tests are not
   qualification; a release is not checked until a native engine has served a
   request.

6. **Publish** the draft. The site's `install.sh` then resolves the new release
   as the latest.

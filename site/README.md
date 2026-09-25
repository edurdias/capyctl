# mllm website

The project site: a landing page at `/` and the documentation at `/docs/`,
built with Astro and Starlight into a static `dist/`. Node is only needed
here; see `.nvmrc`. Building also needs the Rust toolchain, because the CLI
reference and the landing page's terminal output are generated from the CLI.

```bash
cargo fetch --locked # once: the generators run cargo offline
npm ci
npm run dev          # local preview with live reload
npm run check        # tests, build, link, hero, size and voice checks
CHROME_PATH=/path/to/chrome npm run lighthouse   # after a build
```

Astro's telemetry is turned off in every script.

## Where the content comes from

- `/docs/` pages are synced from `docs/` at build time (`scripts/pages.mjs`
  lists them). Edit the file under `docs/`, never the generated copy under
  `src/content/docs/docs/`.
- `docs/examples/*.yaml` are embedded verbatim on the configuration page.
- The CLI reference is generated from the clap definition
  (`cargo run -p mllm-cli --example cli_reference`).
- The landing page's `mllm list deployments` table is rendered by the CLI's
  own table code from `src/data/hero-deployments.json` and
  `src/data/hero-hosts.json` (`cargo run -p mllm-cli --example hero_table`).
- The version shown is the workspace version in `Cargo.toml`.
- Command output in the guide is pasted from a real run:
  `node scripts/transcript/capture.mjs ../target/debug/mllm` runs every
  command the guide shows against a built binary, in private sandbox homes
  under `~/.cache/mllm-site-docs`, with a stand-in engine
  (`scripts/transcript/fake-vllm.py`) that answers mllm's calls and loads
  nothing. It prints the transcript to copy from; it is not a test of any
  engine. `npm run check` also checks that every `mllm` command the guide and
  the landing page show exists in the built CLI's `--help`.

## Settings

The site is published to GitHub Pages by `.github/workflows/site.yml` on every
push to `main`. Each URL is a build setting whose default is the production
value (`scripts/lib/settings.mjs`):

- `MLLM_SITE_URL`: where the site is served; its path is the base path every
  link is built under.
- `MLLM_INSTALL_URL`: the installer the install command runs. The build copies
  `packaging/install.sh` to `<MLLM_SITE_URL>/install.sh`.
- `MLLM_REPO_URL`: the repository, for links. The installer's own default
  repository must match it.
- `MLLM_PUBLISH=1`: a publish build. It fails when a setting is empty or a
  placeholder, or when the installer downloads from another repository. Any
  other build with such a value shows a preview banner on every page.
- `voice-denylist.local.txt` (not committed): names that must never appear on
  the site, one per line. The configured URLs are exempt.

## Before launch

- Recompute the GPU table (`src/data/gpu.mjs`) against actual weight sizes of
  current models and update `tests/gpu.test.mjs` on purpose.
- Run every command on the landing page and in the guide against the
  release the site documents.
- Settle the remaining open decisions in the website spec: a community link
  and a logo.

# mllm website

The project site: a landing page at `/` and the documentation at `/docs/`,
built with Astro and Starlight into a static `dist/`. Node is only needed
here; see `.nvmrc`. Building also needs the Rust toolchain, because the CLI
reference and the landing page's terminal output are generated from the CLI.

```bash
npm ci
npm run dev          # local preview with live reload
npm run check        # tests, build with link validation, size and voice checks
CHROME_PATH=/path/to/chrome npm run lighthouse   # after a build
```

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

## Settings

- `MLLM_REPO_URL`: the public repository, for links and the installer. The
  default is a placeholder; set it for any build that will be published.
- `MLLM_INSTALL_PUBLIC=1`: show the `curl | sh` installer once releases are public.
- `MLLM_SITE_URL`: the site's own URL, for canonical links and the sitemap.
- `voice-denylist.local.txt` (not committed): names that must never appear on
  the site, one per line.

## Before launch

- Recompute the GPU table (`src/data/gpu.mjs`) against actual weight sizes of
  current models and update `tests/gpu.test.mjs` on purpose.
- Run every command on the landing page and in the quickstart against the
  release the site documents.
- Settle the open decisions in the website spec: domain and hosting, the
  installer one-liner, a community link, a logo.

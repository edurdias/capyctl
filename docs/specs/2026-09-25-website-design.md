# mllm website — design

Date: 2026-09-25. Status: approved in brainstorming with the owner; this document
awaits the owner's review before an implementation plan is written. Build starts
after 0.1.0 ships.

## Goal

A minimal, technical project home for mllm as an open-source community project.
A home lab user should understand what mllm does within one screen, see it working
in a terminal block, and reach install instructions and documentation in one tap.

Success means:

- The first screen states what mllm is in one sentence and shows real CLI output.
- A visitor can install and deploy a first model by following the site's docs
  alone.
- Docs are generated from the repository, so a release never leaves the site
  stale.
- The page loads fast on a phone and reads well in light and dark mode.

## Audience and voice

- **Audience:** home users and prosumers running models on GPU machines they own
  (gaming cards through workstation cards and unified-memory boxes, one or several
  machines).
- **Voice:** plain and technical, for the home lab community. State what it does,
  show commands and output. No marketing language, no superlatives, no sales
  calls to action.
- **Attribution:** the site names no parent company and has no "powered by" line.
  mllm stands on its own.
- **Hardware:** never name the maintainers' machines or say where mllm was tested.
  Host names in examples are generic (`gpu-box`, `workstation`). GPU classes appear
  only as illustrative memory budgets, never as support claims.

## Non-goals

- No company page, pricing, sign-up, newsletter or analytics beyond what the host
  provides by default.
- No blog at launch.
- No comparison table against other projects at launch (NVIDIA PAIR, Ollama); the
  "why" section explains mllm's approach without naming others.
- No hosted demo.

## Site structure

Two parts, one build:

1. `/` — a single landing page.
2. `/docs/` — documentation.

### Landing page

Sections in order (approved in the phone-width mockup, v2):

1. **Hero.** Heading: "A model manager for vLLM and SGLang on your own GPUs."
   Sub-line: "Park, wake and switch models on machines that can't hold them all.
   One OpenAI-compatible endpoint across every box on your network." Two buttons:
   Install (to `/docs/install/`) and Docs. Below, a terminal block with the
   `mllm list deployments` table (the CLI's default output) showing two `ready`
   models and one `parked` model on two generic hosts. The output must match
   the real CLI format of the release the site documents.
2. **Why.** Heading "More models than memory". One paragraph: most home GPUs hold
   one or two models at a time; mllm parks the idle ones, releases their GPU
   memory and wakes the one a request asks for, behind the same endpoint.
3. **How it works.** A diagram: your apps → `:8443/v1` (OpenAI API) → mllm server
   (router, scheduler, memory ledger) → gRPC over mutual TLS → hosts, each showing
   models with their engine and state. One line under it: engines listen on
   loopback only; the server decides where each model runs and tracks the GPU
   memory it hands out. The diagram is inline SVG or HTML, not an image, so it
   themes with dark mode.
4. **What it looks like on your GPU.** A table of GPU classes: gaming card (24 GB),
   high-end card (32 GB), workstation card (48–96 GB), unified-memory box (128 GB),
   several machines. Each row gives a typical setup in words (which model sizes
   stay live, which are parked). A note under it: illustrative, from model weight
   sizes; KV cache and engine overhead vary; one model runs on one GPU today. The
   table must be recomputed from actual weight sizes before launch and must not
   claim support for any card.
5. **Engines and platforms.** A small table: engines (vLLM, SGLang), hosts (Linux
   x86-64 and ARM64, NVIDIA GPUs), models (anything the engine can load), API
   (OpenAI-compatible, streaming, tool calls). Engine versions, when shown, are the
   ones the documented release was verified with, taken from its release notes.
6. **Install.** Three commands: the installer, `mllm start standalone`, and
   `mllm deploy model --file … --activate`. One line: one static binary; bring your
   own engine and weights. The installer command shown depends on the repository
   being public (see Open decisions).
7. **Footer.** GitHub · Docs · Releases · Apache-2.0.

### Docs

Built with Starlight. Content at launch:

- **Install** — from `docs/operations/install.md`.
- **Quickstart** — standalone: install, point at an engine and a models directory,
  deploy, send a request, watch a park and a wake. New page.
- **Concepts** — deployments, instances, hosts, parking (shallow and deep),
  switching, the memory ledger. New page, written from `docs/SPEC.md` and the ADRs
  in user-facing language; no internal terms (leases, fencing, epochs) unless
  defined.
- **Multiple machines** — server and hosts: init, invite, join, start, revoke and
  recover. New page.
- **Configuration reference** — the deployment, host, server and standalone
  documents, with the files in `docs/examples/` embedded verbatim (they are
  already validated by a test).
- **CLI reference** — generated from the clap definitions at build time (for
  example with `clap-markdown`), never hand-written.
- **Exit codes and errors** — from the tables in `install.md` and the SPEC error
  vocabulary.

Single source rule: pages that already exist in `docs/` are pulled into the site at
build time by a sync step, not copied by hand. New user-facing pages live under
`docs/` too (for example `docs/guide/`), so the repository and the site show the
same text.

## Visual system (style B, "clean technical")

- Light and dark themes following the system setting, with a toggle.
- Neutral warm grey palette (stone), one warm accent (orange 600 in light mode,
  a lighter orange in dark mode). All colours are CSS custom properties shared by
  the landing page and Starlight's theme overrides.
- Text: a system sans-serif stack (or Inter if self-hosted); code and terminal
  blocks: a system monospace stack. No web font is required for first paint.
- Code blocks: dark background in both themes, muted prompt symbol, accent for
  state words (`parked`).
- Layout: one column, max width about 720 px for text, generous whitespace, thin
  1 px rules between sections. Phone first: no horizontal scroll at 360 px;
  terminal blocks scroll horizontally inside their own box.

## Technical approach

- **Location:** `site/` in this repository, so a release PR can update the site.
- **Stack:** Astro with Starlight for `/docs/`. The landing page is a hand-written
  Astro page with static HTML and CSS and no client-side JavaScript, except the
  theme toggle.
- **Search:** Starlight's built-in Pagefind (static, offline, no service).
- **Toolchain:** Node only for the site; pinned versions and a lockfile under
  `site/`. The Rust workspace does not depend on it.
- **Generated content:** a build step runs the release's `mllm` binary (or a
  `cargo run` of the CLI crate) to produce the CLI reference and the real
  `mllm list deployments` output format.
- **Hosting:** a static host; chosen at open-source time (see Open decisions).

## Quality checks

- `npm run build` in `site/` succeeds with no broken internal links (Starlight's
  link validation or a link checker).
- Lighthouse on the landing page (mobile): performance, accessibility, best
  practices and SEO each at least 95. Total transferred size of the landing page
  under 100 KB excluding fonts.
- Colour contrast meets WCAG AA in both themes.
- Every command shown on the landing page and in the quickstart is run against the
  documented release before launch.

## Open decisions (before launch, not before building)

1. **Domain and hosting.** Decided 2026-09-25: GitHub Pages at the
   repository's `github.io` path, no custom domain, published by
   `.github/workflows/site.yml`.
2. **Installer one-liner.** Decided 2026-09-25: `curl -fsSL <site>/install.sh | sh`,
   served by the site, downloading public release assets. `gh` and a token are
   only a fallback for a private repository.
3. **Community link.** GitHub Discussions, Discord, or neither at launch.
4. **Logo.** A wordmark in the sans-serif is enough for launch.

## Mockups

The approved mockups were produced during brainstorming (visual direction and
the phone-width page layout v2). They are not committed; this document is the
record.

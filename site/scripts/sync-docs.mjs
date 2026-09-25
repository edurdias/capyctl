import { copyFileSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { PAGES } from './pages.mjs';
import { toStarlight } from './lib/sync.mjs';
import { DEFAULTS } from './lib/settings.mjs';
import { BASE, INSTALL_COMMAND, INSTALL_URL, PREVIEW, PREVIEW_NOTE, RELEASE_VERSION, REPO_URL } from '../site.config.mjs';

const repo = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const out = join(repo, 'site', 'src', 'content', 'docs');
const read = (p) => readFileSync(join(repo, p), 'utf8');

// The generated directory is rebuilt from scratch; gen-cli.mjs runs after this.
rmSync(join(out, 'docs'), { recursive: true, force: true });
for (const page of PAGES) {
  const file = join(out, page.slug === 'docs' ? 'docs/index.md' : `${page.slug}.md`);
  mkdirSync(dirname(file), { recursive: true });
  writeFileSync(file, toStarlight(read(page.source), page, PAGES, REPO_URL, read, {
    settings: { installUrl: INSTALL_URL, installCommand: INSTALL_COMMAND, version: RELEASE_VERSION, defaultInstallUrl: DEFAULTS.MLLM_INSTALL_URL },
    banner: PREVIEW ? PREVIEW_NOTE : undefined,
    base: BASE,
  }));
}
// Owner decision 2026-09-25: the site serves the installer at
// <MLLM_SITE_URL>/install.sh, byte for byte packaging/install.sh.
copyFileSync(join(repo, 'packaging', 'install.sh'), join(repo, 'site', 'public', 'install.sh'));
console.log(`synced ${PAGES.length} pages and install.sh`);

import { mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { PAGES } from './pages.mjs';
import { toStarlight } from './lib/sync.mjs';
import { REPO_URL } from '../site.config.mjs';

const repo = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const out = join(repo, 'site', 'src', 'content', 'docs');
const read = (p) => readFileSync(join(repo, p), 'utf8');

// The generated directory is rebuilt from scratch; gen-cli.mjs runs after this.
rmSync(join(out, 'docs'), { recursive: true, force: true });
for (const page of PAGES) {
  const file = join(out, page.slug === 'docs' ? 'docs/index.md' : `${page.slug}.md`);
  mkdirSync(dirname(file), { recursive: true });
  writeFileSync(file, toStarlight(read(page.source), page, PAGES, REPO_URL, read));
}
console.log(`synced ${PAGES.length} pages`);

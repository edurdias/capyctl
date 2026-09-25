// Owner decision 2026-09-25: the site is served under a base path (GitHub
// Pages). Starlight validates links between docs pages; this checks every
// built page, the landing page included: each same-site href or src must be
// under the base path and name a file that exists in dist/.
import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { BASE } from '../site.config.mjs';
import { siteLinks, targetFile } from './lib/links.mjs';

const dist = new URL('../dist/', import.meta.url).pathname;
const failures = [];
let checked = 0;
for (const entry of readdirSync(dist, { recursive: true })) {
  if (!entry.endsWith('.html')) continue;
  for (const link of siteLinks(readFileSync(join(dist, entry), 'utf8'))) {
    checked++;
    const file = targetFile(link, BASE);
    if (file === null) { failures.push(`${entry}: ${link} is not under ${BASE}/`); continue; }
    const path = join(dist, file);
    const found = existsSync(path) && (statSync(path).isFile() || existsSync(join(path, 'index.html')));
    if (!found) failures.push(`${entry}: ${link} does not exist`);
  }
}
if (failures.length) { console.error(failures.join('\n')); process.exit(1); }
console.log(`links: ${checked} same-site links resolve under ${BASE || '/'}`);

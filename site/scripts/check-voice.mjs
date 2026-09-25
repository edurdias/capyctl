// Website spec, Audience and voice: fail the build output on forbidden
// language in visible text, and on any local denylist name anywhere in a
// built file (code blocks and attributes included).
import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { visibleText, findViolations, findInternalTerms } from './lib/voice.mjs';
import { INSTALL_URL, REPO_URL, SITE_ORIGIN } from '../site.config.mjs';

// Owner decision 2026-09-25: the configured URLs (the repository and the
// GitHub Pages host) may carry a name the denylist otherwise keeps out, and
// only there. Everything else is still checked.
const hosts = [REPO_URL, INSTALL_URL, SITE_ORIGIN].map((u) => new URL(u).host);
const paths = [REPO_URL, new URL(REPO_URL).pathname];
const allowed = [...new Set([REPO_URL, INSTALL_URL, SITE_ORIGIN, ...paths, ...hosts])].sort((a, b) => b.length - a.length);
const withoutUrls = (text) => allowed.reduce((t, u) => t.split(u).join(' '), text);

const dist = new URL('../dist/', import.meta.url).pathname;
const local = new URL('../voice-denylist.local.txt', import.meta.url).pathname;
const denylist = existsSync(local) ? readFileSync(local, 'utf8').split('\n').map((s) => s.trim()).filter(Boolean) : [];
if (!denylist.length) console.warn('voice: no local denylist; machine and company names are not checked');

const failures = [];
for (const entry of readdirSync(dist, { recursive: true })) {
  if (!/\.(html|js|css|json|txt|xml|svg)$/.test(entry)) continue;
  const raw = readFileSync(join(dist, entry), 'utf8');
  const hits = [
    ...(entry.endsWith('.html') ? findViolations(visibleText(raw), []) : []),
    ...(entry.startsWith('docs/') && entry.endsWith('.html') ? findInternalTerms(visibleText(raw)).map((t) => `internal term "${t}"`) : []),
    ...findViolations(withoutUrls(raw), denylist),
  ];
  if (hits.length) failures.push(`${entry}: ${[...new Set(hits)].join(', ')}`);
}
if (failures.length) { console.error(failures.join('\n')); process.exit(1); }
console.log('voice: ok');

// Website spec, Audience and voice: fail the build output on forbidden
// language in visible text, and on any local denylist name anywhere in a
// built file (code blocks and attributes included).
import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { visibleText, findViolations } from './lib/voice.mjs';

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
    ...findViolations(raw, denylist),
  ];
  if (hits.length) failures.push(`${entry}: ${[...new Set(hits)].join(', ')}`);
}
if (failures.length) { console.error(failures.join('\n')); process.exit(1); }
console.log('voice: ok');

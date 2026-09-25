// Website spec, Quality checks: landing page under 100 KB transferred, fonts excluded.
import { readFileSync } from 'node:fs';
import { gzipSync } from 'node:zlib';

const dist = new URL('../dist/', import.meta.url);
const html = readFileSync(new URL('index.html', dist));
const assets = [...html.toString().matchAll(/(?:href|src)="(\/[^"#?]+\.(?:css|js|svg|png|webp|avif|ico))"/g)].map((m) => m[1]);
let total = gzipSync(html).length;
for (const a of new Set(assets)) total += gzipSync(readFileSync(new URL(`.${a}`, dist))).length;
const kb = (total / 1024).toFixed(1);
if (total >= 100 * 1024) { console.error(`landing page: ${kb} KB (limit 100 KB)`); process.exit(1); }
console.log(`landing page: ${kb} KB`);

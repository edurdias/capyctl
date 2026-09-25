// Website spec, Quality checks: mobile Lighthouse >= 95 in all four categories.
// Serves dist/ itself: `astro preview` keeps one server per machine and would
// silently reuse another checkout's.
import { createServer } from 'node:http';
import { existsSync, readFileSync, statSync } from 'node:fs';
import { extname, join, normalize } from 'node:path';
import lighthouse from 'lighthouse';
import * as chromeLauncher from 'chrome-launcher';

if (!process.env.CHROME_PATH) {
  console.error('Set CHROME_PATH to a Chrome or Chromium binary (for example a Playwright chromium).');
  process.exit(2);
}
const dist = new URL('../dist/', import.meta.url).pathname;
if (!existsSync(join(dist, 'index.html'))) { console.error('dist/ is missing; run npm run build first'); process.exit(2); }
const TYPES = { '.html': 'text/html; charset=utf-8', '.css': 'text/css', '.js': 'text/javascript', '.svg': 'image/svg+xml', '.json': 'application/json', '.xml': 'application/xml', '.txt': 'text/plain' };
const server = createServer((req, res) => {
  let file = join(dist, normalize(decodeURIComponent(new URL(req.url, 'http://x').pathname)));
  if (!file.startsWith(dist)) { res.writeHead(403).end(); return; }
  if (existsSync(file) && statSync(file).isDirectory()) file = join(file, 'index.html');
  if (!existsSync(file)) { res.writeHead(404).end('not found'); return; }
  res.writeHead(200, { 'content-type': TYPES[extname(file)] ?? 'application/octet-stream' }).end(readFileSync(file));
});
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const url = `http://127.0.0.1:${server.address().port}/`;
try {
  const chrome = await chromeLauncher.launch({ chromeFlags: ['--headless=new', '--no-sandbox'] });
  try {
    const { lhr } = await lighthouse(url, {
      port: chrome.port, onlyCategories: ['performance', 'accessibility', 'best-practices', 'seo'],
    });
    if (lhr.runtimeError) throw new Error(`lighthouse: ${lhr.runtimeError.code}: ${lhr.runtimeError.message}`);
    const scores = Object.fromEntries(Object.values(lhr.categories).map((c) => [c.id, Math.round(c.score * 100)]));
    console.log(scores);
    const low = Object.entries(scores).filter(([, s]) => s < 95);
    if (low.length) {
      for (const cat of Object.values(lhr.categories)) {
        for (const ref of cat.auditRefs) {
          const a = lhr.audits[ref.id];
          if (ref.weight > 0 && a.score !== null && a.score < 1) console.error(`${cat.id}: ${a.id} (${a.score})`);
        }
      }
      console.error(`below 95: ${low.map(([k, s]) => `${k} ${s}`).join(', ')}`);
      process.exitCode = 1;
    }
  } finally { await chrome.kill(); }
} finally { server.close(); }

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import { rewriteLink, expandIncludes, toStarlight } from '../scripts/lib/sync.mjs';

const REPO = 'https://github.com/example/mllm';
const PAGES = [
  { source: 'docs/guide/index.md', slug: 'docs', title: 'Overview' },
  { source: 'docs/operations/install.md', slug: 'docs/install' },
  { source: 'docs/guide/quickstart.md', slug: 'docs/quickstart' },
];

test('link to another synced page becomes its site route, keeping the hash', () => {
  assert.equal(rewriteLink('../operations/install.md#layout', 'docs/guide/quickstart.md', PAGES, REPO), '/docs/install/#layout');
  assert.equal(rewriteLink('index.md', 'docs/guide/quickstart.md', PAGES, REPO), '/docs/');
});

test('link to a repository file that is not a page goes to GitHub', () => {
  assert.equal(rewriteLink('../examples/server.yaml', 'docs/operations/install.md', PAGES, REPO), `${REPO}/blob/main/docs/examples/server.yaml`);
  assert.equal(rewriteLink('../../crates/mllm-cli/src/output.rs', 'docs/guide/errors.md', PAGES, REPO), `${REPO}/blob/main/crates/mllm-cli/src/output.rs`);
});

test('external, absolute-site and hash-only links are unchanged', () => {
  for (const href of ['https://example.org/x', 'mailto:a@b.c', '#layout', '/docs/install/']) {
    assert.equal(rewriteLink(href, 'docs/guide/quickstart.md', PAGES, REPO), href);
  }
});

test('include marker embeds the file byte for byte, with a longer fence if needed', () => {
  const content = 'a: 1\n# ```not a fence end\nb: "x"\n';
  const out = expandIncludes('Before\n\n<!-- include: ../examples/host.yaml -->\n\nAfter\n', 'docs/guide/configuration.md', (p) => {
    assert.equal(p, 'docs/examples/host.yaml');
    return content;
  });
  assert.equal(out, 'Before\n\n````yaml title="host.yaml"\n' + content + '````\n\nAfter\n');
});

test('a missing include fails and names the file', () => {
  assert.throws(() => expandIncludes('<!-- include: nope.yaml -->\n', 'docs/guide/configuration.md', () => { throw new Error('ENOENT'); }), /docs\/guide\/nope\.yaml/);
});

test('toStarlight takes the H1 as title, drops it, and leaves links inside code fences alone', () => {
  const md = '# Quick "start"\n\nSee [install](../operations/install.md).\n\n```md\n[x](../operations/install.md)\n```\n';
  const out = toStarlight(md, PAGES[2], PAGES, REPO, () => '');
  assert.equal(out, '---\ntitle: "Quick \\"start\\""\n---\n\nSee [install](/docs/install/).\n\n```md\n[x](../operations/install.md)\n```\n');
});

test('an explicit page title wins over the H1', () => {
  const out = toStarlight('# Something else\n\nBody\n', PAGES[0], PAGES, REPO, () => '');
  assert.match(out, /^---\ntitle: "Overview"\n---\n\nBody\n$/);
});

test('every docs/examples file is embedded verbatim on the configuration page', () => {
  const root = new URL('../../', import.meta.url);
  const read = (p) => readFileSync(new URL(p, root), 'utf8');
  const page = { source: 'docs/guide/configuration.md', slug: 'docs/reference/configuration', title: 'Configuration' };
  const out = toStarlight(read(page.source), page, [page], REPO, read);
  const files = readdirSync(new URL('docs/examples/', root)).filter((f) => f.endsWith('.yaml'));
  assert.equal(files.length, 5);
  for (const f of files) assert.ok(out.includes(read(`docs/examples/${f}`)), f);
});

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import { rewriteLink, expandIncludes, toStarlight, substitute, inlineSvgs } from '../scripts/lib/sync.mjs';

const REPO = 'https://github.com/example/capyctl';
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
  assert.equal(rewriteLink('../../crates/capyctl-cli/src/output.rs', 'docs/guide/errors.md', PAGES, REPO), `${REPO}/blob/main/crates/capyctl-cli/src/output.rs`);
});

test('external, absolute-site and hash-only links are unchanged', () => {
  for (const href of ['https://example.org/x', 'mailto:a@b.c', '#layout', '/docs/install/']) { // no base path
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

test('every docs/examples file is embedded verbatim on the configuration page or the engines page', () => {
  const root = new URL('../../', import.meta.url);
  const read = (p) => readFileSync(new URL(p, root), 'utf8');
  const pages = [
    { source: 'docs/guide/configuration.md', slug: 'docs/reference/configuration', title: 'Configuration' },
    { source: 'docs/guide/engines.md', slug: 'docs/engines' },
  ];
  const out = pages.map((page) => toStarlight(read(page.source), page, pages, REPO, read)).join('\n');
  const files = readdirSync(new URL('docs/examples/', root)).filter((f) => f.endsWith('.yaml'));
  assert.equal(files.length, 14);
  for (const f of files) assert.ok(out.includes(read(`docs/examples/${f}`)), f);
});

test('a link to a directory goes to its tree', () => {
  assert.equal(rewriteLink('../examples/', 'docs/guide/configuration.md', PAGES, REPO), `${REPO}/tree/main/docs/examples/`);
});

const SETTINGS = { installUrl: 'https://get.example.net/install.sh', installCommand: 'curl -fsSL https://get.example.net/install.sh | sh -s -- --version v1.0.0-rc.1', version: '1.0.0-rc.1', defaultInstallUrl: 'https://h.test/install.sh' };

test('the main install line becomes the exact install command; other lines get the URL and version', () => {
  const md = 'Run\n\n```bash\ncurl -fsSL https://h.test/install.sh | sh\ncurl -fsSL https://h.test/install.sh | sh -s -- --uninstall\n```\n\nVersion `v<version>`.\n';
  assert.equal(substitute(md, SETTINGS), 'Run\n\n```bash\n' + SETTINGS.installCommand + '\ncurl -fsSL https://get.example.net/install.sh | sh -s -- --uninstall\n```\n\nVersion `v1.0.0-rc.1`.\n');
});

test('a preview build puts a banner on every synced page', () => {
  const out = toStarlight('# T\n\nBody\n', PAGES[2], PAGES, REPO, () => '', { settings: SETTINGS, banner: 'Preview' });
  assert.match(out, /^---\ntitle: "T"\nbanner:\n  content: "Preview"\n---\n/);
});

test('links to pages and site paths get the base path', () => {
  assert.equal(rewriteLink('quickstart.md', 'docs/guide/index.md', PAGES, REPO, '/capyctl'), '/capyctl/docs/quickstart/');
  assert.equal(rewriteLink('/docs/reference/cli/', 'docs/guide/index.md', PAGES, REPO, '/capyctl'), '/capyctl/docs/reference/cli/');
  assert.equal(rewriteLink('../examples/host.yaml', 'docs/guide/index.md', PAGES, REPO, '/capyctl'), `${REPO}/blob/main/docs/examples/host.yaml`);
});

test('an SVG image on its own line is embedded inline; code fences and inline images are left alone', () => {
  const svg = '<?xml version="1.0"?>\n<svg role="img"><title>T</title>\n<rect/></svg>\n';
  const md = 'A\n\n![T](how.svg)\n\n```md\n![T](how.svg)\n```\n\nSee ![x](how.svg) here.\n';
  const out = inlineSvgs(md, 'docs/guide/how-it-works.md', (p) => { assert.equal(p, 'docs/guide/how.svg'); return svg; });
  assert.equal(out, 'A\n\n<figure class="diagram">\n<svg role="img"><title>T</title>\n<rect/></svg>\n</figure>\n\n```md\n![T](how.svg)\n```\n\nSee ![x](how.svg) here.\n');
});

test('an inline SVG with a blank line is refused', () => {
  assert.throws(() => inlineSvgs('![T](a.svg)\n', 'docs/guide/x.md', () => '<svg>\n\n</svg>'), /blank line/);
});

test('every diagram the guide shows is accessible and themed', () => {
  const root = new URL('../../', import.meta.url);
  for (const f of readdirSync(new URL('docs/guide/', root)).filter((x) => x.endsWith('.svg'))) {
    const svg = readFileSync(new URL(`docs/guide/${f}`, root), 'utf8');
    assert.match(svg, /^<svg [^>]*role="img"[^>]*aria-labelledby="(\S+) (\S+)"/, f);
    const [, title, desc] = svg.match(/aria-labelledby="(\S+) (\S+)"/);
    assert.match(svg, new RegExp(`<title id="${title}">[^<]+</title>`), f);
    assert.match(svg, new RegExp(`<desc id="${desc}">[^<]+</desc>`), f);
    // Website spec, Visual system: colours come from the theme, so the
    // diagram follows light, dark and the toggle.
    assert.doesNotMatch(svg.replace(/var\([^)]*\)/g, ''), /(fill|stroke):\s*#/, f);
    // Readable on a phone: nothing drawn wider than a 360 px screen.
    assert.match(svg, /viewBox="0 0 360 \d+"/, f);
    assert.doesNotMatch(svg, /\n\s*\n/, f);
  }
});

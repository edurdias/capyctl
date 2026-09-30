import { test } from 'node:test';
import assert from 'node:assert/strict';
import { siteLinks, targetFile } from '../scripts/lib/links.mjs';

test('finds same-site href and src, ignoring external, protocol-relative and hash links', () => {
  const html = '<a href="/capyctl/docs/#x">d</a><img src="/capyctl/a.svg"><a href="https://x.org/">e</a><a href="//cdn/x">c</a><a href="#top">t</a>';
  assert.deepEqual(siteLinks(html), ['/capyctl/docs/', '/capyctl/a.svg']);
});

test('a link must be under the base path', () => {
  assert.equal(targetFile('/capyctl/docs/', '/capyctl'), '/docs/');
  assert.equal(targetFile('/capyctl', '/capyctl'), '/');
  assert.equal(targetFile('/docs/', '/capyctl'), null);
  assert.equal(targetFile('/docs/', ''), '/docs/');
});

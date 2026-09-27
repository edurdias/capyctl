import { test } from 'node:test';
import assert from 'node:assert/strict';
import { siteLinks, targetFile } from '../scripts/lib/links.mjs';

test('finds same-site href and src, ignoring external, protocol-relative and hash links', () => {
  const html = '<a href="/mllm/docs/#x">d</a><img src="/mllm/a.svg"><a href="https://x.org/">e</a><a href="//cdn/x">c</a><a href="#top">t</a>';
  assert.deepEqual(siteLinks(html), ['/mllm/docs/', '/mllm/a.svg']);
});

test('a link must be under the base path', () => {
  assert.equal(targetFile('/mllm/docs/', '/mllm'), '/docs/');
  assert.equal(targetFile('/mllm', '/mllm'), '/');
  assert.equal(targetFile('/docs/', '/mllm'), null);
  assert.equal(targetFile('/docs/', ''), '/docs/');
});

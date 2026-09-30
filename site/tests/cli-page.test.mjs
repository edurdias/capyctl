import { test } from 'node:test';
import assert from 'node:assert/strict';
import { cliPage } from '../scripts/lib/cli-page.mjs';

const RAW = '# Command-Line Help for `capyctl`\n\nThis document contains the help content for the `capyctl` command-line program.\n\n## `capyctl`\n\ncapyctl control-plane CLI\n';

test('wraps clap-markdown output with frontmatter and drops its own title and preamble', () => {
  const out = cliPage(RAW);
  assert.ok(out.startsWith('---\ntitle: "CLI reference"\ndescription: '));
  assert.ok(!out.includes('Command-Line Help'));
  assert.ok(!out.includes('This document contains'));
  assert.ok(out.includes('## `capyctl`\n\ncapyctl control-plane CLI\n'));
  assert.ok(out.includes('Generated from the command definitions'));
});

test('rejects output that does not look like clap-markdown', () => {
  assert.throws(() => cliPage('hello'), /unexpected clap-markdown output/);
});

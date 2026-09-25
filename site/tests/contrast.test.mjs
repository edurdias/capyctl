import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { ratio, themes } from '../scripts/lib/contrast.mjs';

const css = readFileSync(new URL('../src/styles/tokens.css', import.meta.url), 'utf8');

test('ratio matches WCAG reference values', () => {
  assert.equal(ratio('#000000', '#ffffff'), 21);
  assert.equal(ratio('#c2410c', '#fafaf9'), 4.96);
});

// Website spec, Quality checks: WCAG AA in both themes.
const TEXT = [['text', 'bg'], ['muted', 'bg'], ['accent-text', 'bg'], ['text', 'surface'], ['muted', 'surface'],
  ['button-text', 'button-bg'], ['code-text', 'code-bg'], ['code-muted', 'code-bg'], ['code-accent', 'code-bg']];
for (const [name, vars] of Object.entries(themes(css))) {
  test(`${name}: text pairs reach 4.5:1`, () => {
    for (const [fg, bg] of TEXT) assert.ok(ratio(vars[fg], vars[bg]) >= 4.5, `${fg} on ${bg}: ${ratio(vars[fg], vars[bg])}`);
  });
  test(`${name}: accent as a UI component reaches 3:1`, () => {
    assert.ok(ratio(vars.accent, vars.bg) >= 3);
  });
}

test('without JavaScript, the light system setting gets the same light tokens', () => {
  const light = themes(css).light;
  const start = css.indexOf('@media (prefers-color-scheme: light)');
  assert.ok(start >= 0);
  const fallback = Object.fromEntries([...css.slice(start, css.indexOf('}', start)).matchAll(/--mllm-([a-z-]+):\s*(#[0-9a-f]{6})/gi)].map((m) => [m[1], m[2].toLowerCase()]));
  assert.deepEqual(fallback, light);
});

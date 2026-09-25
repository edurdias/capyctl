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

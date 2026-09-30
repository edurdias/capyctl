import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { readdirSync } from 'node:fs';
import { ratio, themes, starlightThemes } from '../scripts/lib/contrast.mjs';

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
  const fallback = Object.fromEntries([...css.slice(start, css.indexOf('}', start)).matchAll(/--capyctl-([a-z-]+):\s*(#[0-9a-f]{6})/gi)].map((m) => [m[1], m[2].toLowerCase()]));
  assert.deepEqual(fallback, light);
});

// The docs diagrams (docs/guide/*.svg) draw text in these Starlight tokens on
// these fills; each pair must reach WCAG AA in both themes.
const DIAGRAM_TEXT = [['white', 'gray-6'], ['white', 'black'], ['white', 'accent-low'], ['white', 'gray-5'],
  ['gray-3', 'gray-6'], ['gray-3', 'black'], ['gray-3', 'accent-low'], ['accent', 'accent-low'], ['accent', 'gray-6']];
for (const [name, vars] of Object.entries(starlightThemes(css))) {
  test(`${name}: diagram text pairs reach 4.5:1`, () => {
    for (const [fg, bg] of DIAGRAM_TEXT) assert.ok(ratio(vars[fg], vars[bg]) >= 4.5, `${fg} on ${bg}: ${ratio(vars[fg], vars[bg])}`);
  });
}

test('a diagram shown outside the site (an image on GitHub) falls back to the same colours', () => {
  const sl = starlightThemes(css);
  const dir = new URL('../../docs/guide/', import.meta.url);
  for (const f of readdirSync(dir).filter((x) => x.endsWith('.svg'))) {
    const svg = readFileSync(new URL(f, dir), 'utf8');
    const dark = svg.indexOf('@media (prefers-color-scheme:dark)');
    assert.ok(dark > 0, `${f}: no dark fallback`);
    for (const [part, theme] of [[svg.slice(0, dark), 'light'], [svg.slice(dark, svg.indexOf('}}', dark)), 'dark']]) {
      for (const m of part.matchAll(/var\(--sl-color-([a-z0-9-]+),(#[0-9a-f]{6})\)/gi)) {
        assert.equal(m[2].toLowerCase(), sl[theme][m[1]], `${f}: ${theme} fallback for --sl-color-${m[1]}`);
      }
    }
  }
});

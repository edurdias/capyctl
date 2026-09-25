// Website spec, Quality checks: colour contrast meets WCAG AA in both themes.
const lum = (hex) => {
  const c = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255)
    .map((x) => (x <= 0.03928 ? x / 12.92 : ((x + 0.055) / 1.055) ** 2.4));
  return 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
};

export function ratio(a, b) {
  const [hi, lo] = [lum(a), lum(b)].sort((x, y) => y - x);
  return Math.round(((hi + 0.05) / (lo + 0.05)) * 100) / 100;
}

const block = (css, selector) => {
  const start = css.indexOf(selector);
  if (start < 0) throw new Error(`selector not found: ${selector}`);
  const body = css.slice(css.indexOf('{', start) + 1, css.indexOf('}', start));
  return Object.fromEntries([...body.matchAll(/--mllm-([a-z-]+):\s*(#[0-9a-f]{6})/gi)].map((m) => [m[1], m[2].toLowerCase()]));
};

export function themes(css) {
  return { dark: block(css, ':root,\n::backdrop'), light: block(css, ":root[data-theme='light']") };
}

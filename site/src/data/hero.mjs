// Website spec, Landing page 1: the hero shows `capyctl list deployments` as the
// CLI prints it by default, a table. The table text is rendered at build time
// by the CLI's own table code (scripts/gen-hero.mjs); this only marks the
// `parked` state for colour.
export const HERO_COMMAND = 'capyctl list deployments';

export function heroSegments(text) {
  return text.replace(/\n$/, '').split('\n').map((line) => {
    const segments = [];
    let rest = line;
    for (;;) {
      const m = rest.match(/(^|\s)(parked)(?=\s|$)/);
      if (!m) break;
      const at = m.index + m[1].length;
      if (at > 0) segments.push({ text: rest.slice(0, at), parked: false });
      segments.push({ text: 'parked', parked: true });
      rest = rest.slice(at + 6);
    }
    if (rest || !segments.length) segments.push({ text: rest, parked: false });
    return segments;
  });
}

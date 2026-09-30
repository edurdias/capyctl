// Website spec, Audience and voice: no parent company, no "powered by", never
// say where capyctl was tested, no marketing language. Machine and company names
// are kept out of the repository: list them one per line in
// site/voice-denylist.local.txt (gitignored).
export const PHRASES = ['powered by', 'tested on', 'tested with', 'verified on', 'owner decision',
  'seamless', 'blazing', 'powerful', 'effortless', 'world-class'];

// Owner feedback 2026-09-25: the user docs use no internal terms. Whole words,
// matched in the visible text of every /docs/ page.
export const INTERNAL_TERMS = ['spec §', 'adr', 'lease', 'leases', 'fencing', 'epoch', 'epochs',
  'ledger', 'reservation', 'reservations'];

export function visibleText(html) {
  return html
    .replace(/<(script|style|pre|code)\b[\s\S]*?<\/\1>/gi, ' ')
    .replace(/<[^>]+>/g, ' ')
    .replace(/&[a-z#0-9]+;/gi, ' ');
}

export function findViolations(text, denylist) {
  const lower = text.toLowerCase();
  return [...PHRASES, ...denylist.map((d) => d.toLowerCase())].filter((p) => p && lower.includes(p));
}

// Terms the docs define for users where they use them ("How it works"
// defines the memory ledger), removed before the check.
export const DEFINED_TERMS = ['memory ledger'];

export function findInternalTerms(text) {
  const lower = DEFINED_TERMS.reduce((t, d) => t.split(d).join(' '), text.toLowerCase());
  const escape = (t) => t.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  // A boundary on each side that ends in a letter or digit ("§6" still counts).
  const edge = (c) => (/[a-z0-9]/.test(c) ? '(?:$|[^a-z0-9])' : '');
  return INTERNAL_TERMS.filter((t) => new RegExp(`(?:^|[^a-z0-9])${escape(t)}${edge(t.at(-1))}`).test(lower));
}

// Owner feedback 2026-09-25: the landing page names no port. Its text,
// terminal blocks included, carries no `:<port>` and none of capyctl's default
// port numbers.
export function findPorts(html) {
  const text = html
    .replace(/<(script|style)\b[\s\S]*?<\/\1>/gi, ' ')
    .replace(/<[^>]+>/g, ' ')
    .replace(/&[a-z#0-9]+;/gi, ' ');
  return [...new Set([...text.matchAll(/:\d{2,5}\b|\b(?:7443|7444|7445|8443|8444)\b/g)].map((m) => m[0]))];
}

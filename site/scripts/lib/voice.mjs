// Website spec, Audience and voice: no parent company, no "powered by", never
// say where mllm was tested. Machine and company names are kept out of the
// repository: list them one per line in site/voice-denylist.local.txt
// (gitignored).
export const PHRASES = ['powered by', 'tested on', 'tested with', 'verified on', 'owner decision'];

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

// Owner feedback 2026-09-25: the hero block must be byte-identical to real
// `mllm list deployments` output. This reads the block back out of the built
// page as a browser would show it.
const ENTITIES = { amp: '&', lt: '<', gt: '>', quot: '"', '#39': "'", apos: "'" };

export function heroOutput(html) {
  const m = html.match(/<pre class="terminal"[^>]*aria-label="Example terminal output"[^>]*>([\s\S]*?)<\/pre>/);
  if (!m) throw new Error('hero terminal block not found');
  const text = m[1].replace(/<[^>]+>/g, '').replace(/&(amp|lt|gt|quot|#39|apos);/g, (_, e) => ENTITIES[e]);
  const [command, ...rest] = text.split('\n');
  return { command, output: rest.join('\n') };
}

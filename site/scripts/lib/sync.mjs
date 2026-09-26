// Website spec, Docs: "pages that already exist in docs/ are pulled into the
// site at build time by a sync step, not copied by hand."
import path from 'node:path/posix';

const INCLUDE = /^<!-- include: (\S+) -->$/;
const FENCE = /^(`{3,}|~{3,})/;
const LINK = /\]\(([^)\s]+)\)/g;

export function rewriteLink(href, sourcePath, pages, repoUrl, base = '') {
  if (/^[a-z][a-z0-9+.-]*:/i.test(href) || href.startsWith('#')) return href;
  // A site path written in a doc (`/docs/reference/cli/`) is under the base path.
  if (href.startsWith('/')) return `${base}${href}`;
  const [target, hash] = href.split('#');
  const resolved = path.normalize(path.join(path.dirname(sourcePath), target));
  const suffix = hash ? `#${hash}` : '';
  const page = pages.find((p) => p.source === resolved);
  if (page) return `${base}/${page.slug}/${suffix}`;
  // A directory is a tree on GitHub, a file a blob.
  const kind = target.endsWith('/') ? 'tree' : 'blob';
  return `${repoUrl}/${kind}/main/${resolved}${suffix}`;
}

export function expandIncludes(md, sourcePath, read) {
  return md.split('\n').map((line) => {
    const m = line.match(INCLUDE);
    if (!m) return line;
    const repoPath = path.normalize(path.join(path.dirname(sourcePath), m[1]));
    let content;
    try { content = read(repoPath); } catch (err) { throw new Error(`include not found: ${repoPath} (${err.message})`); }
    if (!content.endsWith('\n')) content += '\n';
    let fence = '```';
    while (content.includes(fence)) fence += '`';
    const lang = path.extname(repoPath).slice(1) || 'text';
    // Website spec, Docs: examples are embedded verbatim.
    return `${fence}${lang} title="${path.basename(repoPath)}"\n${content}${fence}`;
  }).join('\n');
}

// An SVG image on a line of its own is embedded inline, so it takes the
// site's theme colours; on GitHub the same line shows it as an image.
const SVG_IMAGE = /^!\[[^\]]*\]\(([^)\s]+\.svg)\)$/;

export function inlineSvgs(md, sourcePath, read) {
  let open = null;
  return md.split('\n').map((line) => {
    const f = line.match(FENCE);
    if (f) {
      if (!open) open = f[1];
      else if (line.startsWith(open) && line.trim() === open) open = null;
      return line;
    }
    const m = !open && line.match(SVG_IMAGE);
    if (!m) return line;
    const repoPath = path.normalize(path.join(path.dirname(sourcePath), m[1]));
    const svg = read(repoPath).replace(/^<\?xml[^>]*>\s*/, '').trim();
    if (/\n\s*\n/.test(svg)) throw new Error(`${repoPath}: a blank line would end the inline SVG early`);
    return `<figure class="diagram">\n${svg}\n</figure>`;
  }).join('\n');
}

function rewriteOutsideFences(md, fn) {
  let open = null;
  return md.split('\n').map((line) => {
    const f = line.match(FENCE);
    if (f) {
      if (!open) open = f[1];
      else if (line.startsWith(open) && line.trim() === open) open = null;
      return line;
    }
    return open ? line : line.replace(LINK, (_, href) => `](${fn(href)})`);
  }).join('\n');
}

// The docs name the production install URL and `<version>`; the site shows
// the configured values. The main install line becomes the site's exact
// install command.
export function substitute(md, { installUrl, installCommand, version, defaultInstallUrl }) {
  const main = `curl -fsSL ${defaultInstallUrl} | sh`;
  let open = null;
  return md.split('\n').map((line) => {
    const f = line.match(FENCE);
    if (f) {
      if (!open) open = f[1];
      else if (line.startsWith(open) && line.trim() === open) open = null;
    }
    if (open && line.trim() === main) return line.replace(main, installCommand);
    return line.replaceAll(defaultInstallUrl, installUrl).replaceAll('<version>', version);
  }).join('\n');
}

export function toStarlight(md, page, pages, repoUrl, read, { settings, banner, base = '' } = {}) {
  let body = settings ? substitute(md, settings) : md;
  let title = page.title;
  const h1 = body.match(/^# (.+)\n+/);
  if (h1) {
    title ??= h1[1].trim();
    body = body.slice(h1[0].length);
  }
  if (!title) throw new Error(`${page.source}: no title and no H1`);
  body = inlineSvgs(body, page.source, read);
  body = rewriteOutsideFences(body, (href) => rewriteLink(href, page.source, pages, repoUrl, base));
  body = expandIncludes(body, page.source, read);
  const head = [`title: ${JSON.stringify(title)}`];
  if (banner) head.push('banner:', `  content: ${JSON.stringify(banner)}`);
  return `---\n${head.join('\n')}\n---\n\n${body}`;
}

// Website spec, Docs: "pages that already exist in docs/ are pulled into the
// site at build time by a sync step, not copied by hand."
import path from 'node:path/posix';

const INCLUDE = /^<!-- include: (\S+) -->$/;
const FENCE = /^(`{3,}|~{3,})/;
const LINK = /\]\(([^)\s]+)\)/g;

export function rewriteLink(href, sourcePath, pages, repoUrl) {
  if (/^[a-z][a-z0-9+.-]*:/i.test(href) || href.startsWith('#') || href.startsWith('/')) return href;
  const [target, hash] = href.split('#');
  const resolved = path.normalize(path.join(path.dirname(sourcePath), target));
  const suffix = hash ? `#${hash}` : '';
  const page = pages.find((p) => p.source === resolved);
  if (page) return `/${page.slug}/${suffix}`;
  return `${repoUrl}/blob/main/${resolved}${suffix}`;
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

export function toStarlight(md, page, pages, repoUrl, read) {
  let body = md;
  let title = page.title;
  const h1 = body.match(/^# (.+)\n+/);
  if (h1) {
    title ??= h1[1].trim();
    body = body.slice(h1[0].length);
  }
  if (!title) throw new Error(`${page.source}: no title and no H1`);
  body = rewriteOutsideFences(body, (href) => rewriteLink(href, page.source, pages, repoUrl));
  body = expandIncludes(body, page.source, read);
  return `---\ntitle: ${JSON.stringify(title)}\n---\n\n${body}`;
}

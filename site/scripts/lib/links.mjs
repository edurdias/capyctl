// Same-site links in a built page: href and src values that start with "/"
// (not "//"), without their query or fragment.
export function siteLinks(html) {
  return [...html.matchAll(/\s(?:href|src)="(\/(?!\/)[^"#?]*)/g)].map((m) => m[1]);
}

/** The dist/-relative path a link names, or null when it is outside `base`. */
export function targetFile(link, base) {
  if (base && link !== base && !link.startsWith(`${base}/`)) return null;
  return decodeURIComponent(link.slice(base.length)) || '/';
}

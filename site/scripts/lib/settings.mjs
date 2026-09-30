// Website spec, Open decisions 1 and 2, settled by the owner on 2026-09-25:
// the site is served by GitHub Pages under a path, and it serves the
// installer itself. Each URL is one build setting whose default is the
// production value. A value set empty or to a placeholder makes a preview
// build (with a visible banner); a publish build (CAPYCTL_PUBLISH=1) refuses it.

export const DEFAULTS = {
  CAPYCTL_REPO_URL: 'https://github.com/edurdias/capyctl',
  CAPYCTL_INSTALL_URL: 'https://edurdias.github.io/capyctl/install.sh',
  CAPYCTL_SITE_URL: 'https://edurdias.github.io/capyctl',
};

export const isPlaceholder = (url) =>
  !url || /\/OWNER\/|\.invalid(?:[/:]|$)|\bexample\.(?:com|org|net)\b|<[^>]*>/i.test(url);

const trim = (url) => url.replace(/\/+$/, '');
const parse = (url) => { try { return new URL(url); } catch { return null; } };

/** Resolve the settings from `env`. `installerRepo` is the default
 * repository written in packaging/install.sh. */
export function resolveSettings(env, installerRepo) {
  const raw = Object.fromEntries(Object.entries(DEFAULTS).map(([k, v]) => [k, k in env ? env[k].trim() : v]));
  const placeholders = Object.keys(raw).filter((k) => isPlaceholder(raw[k]) || !parse(raw[k]));
  const fallback = (k) => (placeholders.includes(k) ? 'https://capyctl.invalid' : raw[k]);
  const site = new URL(trim(fallback('CAPYCTL_SITE_URL')));
  const settings = {
    repoUrl: trim(fallback('CAPYCTL_REPO_URL')),
    installUrl: raw.CAPYCTL_INSTALL_URL || 'https://capyctl.invalid/install.sh',
    siteUrl: trim(site.href),
    // Astro's `site` is the origin and `base` the path the site is served under.
    siteOrigin: site.origin,
    base: trim(site.pathname),
    publish: env.CAPYCTL_PUBLISH === '1',
    placeholders,
    preview: placeholders.length > 0,
  };
  settings.repoSlug = new URL(settings.repoUrl).pathname.replace(/^\/+/, '');

  if (settings.publish) {
    const problems = placeholders.map((k) => `${k} is empty or a placeholder (${JSON.stringify(raw[k])})`);
    for (const k of Object.keys(raw)) {
      if (!placeholders.includes(k) && !raw[k].startsWith('https://')) problems.push(`${k} must be an https:// URL`);
    }
    // Review item 3: the site runs the installer without --repo, so the
    // installer's own default must be the repository the site links to.
    if (!placeholders.includes('CAPYCTL_REPO_URL') && installerRepo !== settings.repoSlug) {
      problems.push(`packaging/install.sh downloads from ${installerRepo}, but CAPYCTL_REPO_URL is ${settings.repoSlug}`);
    }
    if (problems.length) throw new Error(`publish build refused:\n- ${problems.join('\n- ')}`);
  }
  return settings;
}

/** `path` (starting with "/") under the site's base path. */
export const withBase = (base, path) => `${base}${path}`;

/** The one install command the site shows. A pre-release is never GitHub's
 * "latest", so it is named. */
export function installCommand(installUrl, version) {
  const base = `curl -fsSL ${installUrl} | sh`;
  return version.includes('-') ? `${base} -s -- --version v${version}` : base;
}

import { existsSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { installCommand, resolveSettings, withBase } from './scripts/lib/settings.mjs';

// The workspace root of this checkout, found from the working directory
// upwards, because Astro bundles this module into its build output, where
// import.meta.url no longer points at site/.
function workspaceRoot() {
  for (let dir = process.cwd(); ; dir = dirname(dir)) {
    const file = join(dir, 'Cargo.toml');
    if (existsSync(file) && readFileSync(file, 'utf8').includes('[workspace.package]')) return dir;
    if (dirname(dir) === dir) throw new Error('workspace Cargo.toml not found above the working directory');
  }
}
const root = workspaceRoot();

// The release the site documents: the workspace version of this checkout.
const cargo = readFileSync(join(root, 'Cargo.toml'), 'utf8');
const version = cargo.match(/\[workspace\.package\][^[]*?\nversion = "([^"]+)"/s);
if (!version) throw new Error('workspace version not found in Cargo.toml');
export const RELEASE_VERSION = version[1];

const installer = readFileSync(join(root, 'packaging', 'install.sh'), 'utf8').match(/^repo=(\S+)$/m);
if (!installer) throw new Error('default repository not found in packaging/install.sh');

// Website spec, Open decisions 1 and 2; see scripts/lib/settings.mjs.
const settings = resolveSettings(process.env, installer[1]);
export const REPO_URL = settings.repoUrl;
export const REPO_SLUG = settings.repoSlug;
export const INSTALL_URL = settings.installUrl;
export const SITE_URL = settings.siteUrl;
export const SITE_ORIGIN = settings.siteOrigin;
export const BASE = settings.base;
/** A site path (starting with "/") under the base path, e.g. `href('/docs/')`. */
export const href = (path) => withBase(BASE, path);
export const PREVIEW = settings.preview;
export const PREVIEW_NOTE = `Preview build: ${settings.placeholders.join(', ')} ${settings.placeholders.length === 1 ? 'is' : 'are'} not set to a real URL, so some links and the install command do not work.`;
export const WORKSPACE_ROOT = root;
export const INSTALL_COMMAND = installCommand(INSTALL_URL, RELEASE_VERSION);

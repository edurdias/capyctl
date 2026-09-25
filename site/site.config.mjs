import { existsSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';

// Website spec, Open decisions 1: the public repository's organisation is not
// settled, so the default is a conspicuous placeholder. Set MLLM_REPO_URL for
// any build that will be published.
export const REPO_URL = process.env.MLLM_REPO_URL ?? 'https://github.com/OWNER/mllm';
export const REPO_SLUG = new URL(REPO_URL).pathname.slice(1);

// The release the site documents: the workspace version of this checkout.
// Found from the working directory upwards, because Astro bundles this module
// into its build output, where import.meta.url no longer points at site/.
function workspaceManifest() {
  for (let dir = process.cwd(); ; dir = dirname(dir)) {
    const file = join(dir, 'Cargo.toml');
    if (existsSync(file) && readFileSync(file, 'utf8').includes('[workspace.package]')) return file;
    if (dirname(dir) === dir) throw new Error('workspace Cargo.toml not found above the working directory');
  }
}
const cargo = readFileSync(workspaceManifest(), 'utf8');
const version = cargo.match(/\[workspace\.package\][^[]*?\nversion = "([^"]+)"/s);
if (!version) throw new Error('workspace version not found in Cargo.toml');
export const RELEASE_VERSION = version[1];

// Website spec, Open decisions 2: `curl | sh` works only once releases are public.
export const INSTALL_PUBLIC = process.env.MLLM_INSTALL_PUBLIC === '1';

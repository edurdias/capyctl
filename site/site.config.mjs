import { readFileSync } from 'node:fs';

// Website spec, Open decisions 1: the public repository's organisation is not
// settled, so the default is a conspicuous placeholder. Set MLLM_REPO_URL for
// any build that will be published.
export const REPO_URL = process.env.MLLM_REPO_URL ?? 'https://github.com/OWNER/mllm';
export const REPO_SLUG = new URL(REPO_URL).pathname.slice(1);

// The release the site documents: the workspace version of this checkout.
const cargo = readFileSync(new URL('../Cargo.toml', import.meta.url), 'utf8');
const version = cargo.match(/\[workspace\.package\][^[]*?\nversion = "([^"]+)"/s);
if (!version) throw new Error('workspace version not found in Cargo.toml');
export const RELEASE_VERSION = version[1];

// Website spec, Open decisions 2: `curl | sh` works only once releases are public.
export const INSTALL_PUBLIC = process.env.MLLM_INSTALL_PUBLIC === '1';

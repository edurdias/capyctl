import { test } from 'node:test';
import assert from 'node:assert/strict';
import { resolveSettings, installCommand, isPlaceholder, DEFAULTS } from '../scripts/lib/settings.mjs';

const REPO = new URL(DEFAULTS.MLLM_REPO_URL).pathname.slice(1);

test('the defaults are the production values: a publish build needs no settings', () => {
  for (const url of Object.values(DEFAULTS)) assert.ok(!isPlaceholder(url), url);
  const s = resolveSettings({ MLLM_PUBLISH: '1' }, REPO);
  assert.equal(s.preview, false);
  assert.equal(s.siteOrigin, new URL(DEFAULTS.MLLM_SITE_URL).origin);
  assert.equal(s.base, '/mllm');
  assert.equal(s.installUrl, `${DEFAULTS.MLLM_SITE_URL}/install.sh`);
});

test('the site URL splits into an origin and a base path; trailing slashes are stripped', () => {
  const s = resolveSettings({ MLLM_SITE_URL: 'https://mllm.dev/', MLLM_REPO_URL: 'https://github.com/a/mllm/' }, 'a/mllm');
  assert.equal(s.base, '');
  assert.equal(s.siteOrigin, 'https://mllm.dev');
  assert.equal(s.repoUrl, 'https://github.com/a/mllm');
  assert.equal(s.repoSlug, 'a/mllm');
});

test('an empty or placeholder value makes a preview build', () => {
  const s = resolveSettings({ MLLM_INSTALL_URL: '', MLLM_REPO_URL: 'https://github.com/OWNER/mllm' }, REPO);
  assert.equal(s.preview, true);
  assert.deepEqual(s.placeholders, ['MLLM_REPO_URL', 'MLLM_INSTALL_URL']);
});

test('a publish build refuses an empty or placeholder value', () => {
  assert.throws(() => resolveSettings({ MLLM_INSTALL_URL: '', MLLM_PUBLISH: '1' }, REPO), /MLLM_INSTALL_URL is empty or a placeholder/);
  assert.throws(() => resolveSettings({ MLLM_SITE_URL: 'https://mllm.invalid', MLLM_PUBLISH: '1' }, REPO), /MLLM_SITE_URL/);
});

test('a publish build refuses an installer that downloads from another repository', () => {
  assert.throws(() => resolveSettings({ MLLM_PUBLISH: '1' }, 'elsewhere/mllm'), /install\.sh downloads from elsewhere\/mllm/);
});

test('a publish build needs https', () => {
  assert.throws(() => resolveSettings({ MLLM_INSTALL_URL: 'http://mllm.dev/install.sh', MLLM_PUBLISH: '1' }, REPO), /https/);
});

test('the install command is one line; a pre-release is named', () => {
  assert.equal(installCommand('https://h/install.sh', '0.1.0'), 'curl -fsSL https://h/install.sh | sh');
  assert.equal(installCommand('https://h/install.sh', '0.1.0-rc.4'), 'curl -fsSL https://h/install.sh | sh -s -- --version v0.1.0-rc.4');
});

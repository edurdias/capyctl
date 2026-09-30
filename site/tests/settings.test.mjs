import { test } from 'node:test';
import assert from 'node:assert/strict';
import { resolveSettings, installCommand, isPlaceholder, DEFAULTS } from '../scripts/lib/settings.mjs';

const REPO = new URL(DEFAULTS.CAPYCTL_REPO_URL).pathname.slice(1);

test('the defaults are the production values: a publish build needs no settings', () => {
  for (const url of Object.values(DEFAULTS)) assert.ok(!isPlaceholder(url), url);
  const s = resolveSettings({ CAPYCTL_PUBLISH: '1' }, REPO);
  assert.equal(s.preview, false);
  assert.equal(s.siteOrigin, new URL(DEFAULTS.CAPYCTL_SITE_URL).origin);
  assert.equal(s.base, '/capyctl');
  assert.equal(s.installUrl, `${DEFAULTS.CAPYCTL_SITE_URL}/install.sh`);
});

test('the site URL splits into an origin and a base path; trailing slashes are stripped', () => {
  const s = resolveSettings({ CAPYCTL_SITE_URL: 'https://capyctl.dev/', CAPYCTL_REPO_URL: 'https://github.com/a/capyctl/' }, 'a/capyctl');
  assert.equal(s.base, '');
  assert.equal(s.siteOrigin, 'https://capyctl.dev');
  assert.equal(s.repoUrl, 'https://github.com/a/capyctl');
  assert.equal(s.repoSlug, 'a/capyctl');
});

test('an empty or placeholder value makes a preview build', () => {
  const s = resolveSettings({ CAPYCTL_INSTALL_URL: '', CAPYCTL_REPO_URL: 'https://github.com/OWNER/capyctl' }, REPO);
  assert.equal(s.preview, true);
  assert.deepEqual(s.placeholders, ['CAPYCTL_REPO_URL', 'CAPYCTL_INSTALL_URL']);
});

test('a publish build refuses an empty or placeholder value', () => {
  assert.throws(() => resolveSettings({ CAPYCTL_INSTALL_URL: '', CAPYCTL_PUBLISH: '1' }, REPO), /CAPYCTL_INSTALL_URL is empty or a placeholder/);
  assert.throws(() => resolveSettings({ CAPYCTL_SITE_URL: 'https://capyctl.invalid', CAPYCTL_PUBLISH: '1' }, REPO), /CAPYCTL_SITE_URL/);
});

test('a publish build refuses an installer that downloads from another repository', () => {
  assert.throws(() => resolveSettings({ CAPYCTL_PUBLISH: '1' }, 'elsewhere/capyctl'), /install\.sh downloads from elsewhere\/capyctl/);
});

test('a publish build needs https', () => {
  assert.throws(() => resolveSettings({ CAPYCTL_INSTALL_URL: 'http://capyctl.dev/install.sh', CAPYCTL_PUBLISH: '1' }, REPO), /https/);
});

test('the install command is one line; a pre-release is named', () => {
  assert.equal(installCommand('https://h/install.sh', '0.1.0'), 'curl -fsSL https://h/install.sh | sh');
  assert.equal(installCommand('https://h/install.sh', '0.1.0-rc.4'), 'curl -fsSL https://h/install.sh | sh -s -- --version v0.1.0-rc.4');
});

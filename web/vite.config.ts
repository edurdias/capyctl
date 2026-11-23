import { execFileSync } from 'node:child_process';
import { defineConfig, type Plugin } from 'vitest/config';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import license, { type Dependency } from 'rollup-plugin-license';

// The commit this bundle was built from. The release packaging refuses a
// bundle whose commit differs from the one it packages (plan KTD17), so the
// release script's own value wins when it sets one.
function buildCommit(): string {
  const fromEnv = process.env.CAPYCTL_BUILD_COMMIT?.trim();
  if (fromEnv) {
    return fromEnv;
  }
  return execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
}

function buildCommitFile(): Plugin {
  return {
    name: 'capyctl-build-commit',
    apply: 'build',
    generateBundle() {
      this.emitFile({ type: 'asset', fileName: 'BUILD_COMMIT', source: `${buildCommit()}\n` });
    },
  };
}

// Third-party notices for every package that ends up in the bundle, served
// at /ui/licenses.txt (plan KTD17). The build fails on a package whose
// license is not compatible with CapyCTL's Apache-2.0 distribution.
const ALLOWED_LICENSES: Record<string, true> = {
  MIT: true,
  ISC: true,
  'Apache-2.0': true,
  'BSD-2-Clause': true,
  'BSD-3-Clause': true,
  '0BSD': true,
};

function noticeText(dependencies: Dependency[]): string {
  const entries = [...dependencies]
    .sort((a, b) => (a.name ?? '').localeCompare(b.name ?? ''))
    .map((dependency) =>
      [
        `${dependency.name ?? 'unknown'} ${dependency.version ?? ''}`.trim(),
        `License: ${dependency.license ?? 'unknown'}`,
        '',
        (dependency.licenseText ?? '').trim(),
        (dependency.noticeText ?? '').trim(),
      ]
        .join('\n')
        .trim(),
    );
  return `Third-party software bundled in the CapyCTL web UI\n\n${entries.join('\n\n----------------------------------------\n\n')}\n`;
}

function notices(): Plugin[] {
  let text: string | undefined;
  const scan = license({
    thirdParty: {
      includePrivate: false,
      allow: {
        test: (dependency) => dependency.license !== null && ALLOWED_LICENSES[dependency.license] === true,
        failOnUnlicensed: true,
        failOnViolation: true,
      },
      output: (dependencies) => {
        text = noticeText(dependencies);
      },
    },
  }) as unknown as Plugin;
  return [
    { ...scan, apply: 'build' },
    {
      name: 'capyctl-licenses',
      apply: 'build',
      generateBundle() {
        if (text === undefined) {
          this.error('third-party notices were not collected');
        }
        this.emitFile({ type: 'asset', fileName: 'licenses.txt', source: text });
      },
    },
  ];
}

export default defineConfig({
  base: '/ui/',
  plugins: [svelte(), buildCommitFile(), ...notices()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    sourcemap: false,
    assetsDir: 'assets',
  },
  resolve: process.env.VITEST ? { conditions: ['browser'] } : undefined,
  test: {
    environment: 'jsdom',
    include: ['tests/**/*.test.ts'],
    restoreMocks: true,
    unstubGlobals: true,
  },
});

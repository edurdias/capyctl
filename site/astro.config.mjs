import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import starlightLinksValidator from 'starlight-links-validator';
import { BASE, REPO_URL, SITE_ORIGIN } from './site.config.mjs';

export default defineConfig({
  // Website spec, Open decisions 1: CAPYCTL_SITE_URL (scripts/lib/settings.mjs),
  // split into the origin and the path GitHub Pages serves the site under.
  site: SITE_ORIGIN,
  base: BASE || '/',
  // Website spec, Technical approach: the landing page ships no client-side
  // JavaScript except the theme toggle, so no link prefetching script.
  prefetch: false,
  integrations: [
    starlight({
      title: 'CapyCTL',
      favicon: '/favicon-32.png',
      logo: { light: './public/brand/header-light.webp', dark: './public/brand/header-dark.webp', alt: 'CapyCTL', replacesTitle: true },
      description: 'Control what runs next: run vLLM, SGLang, TensorFold and llama.cpp models on your own GPUs behind one OpenAI-compatible endpoint.',
      social: [{ icon: 'github', label: 'GitHub', href: REPO_URL }],
      // Website spec, Visual system: code blocks are dark in both themes.
      expressiveCode: { themes: ['github-dark'] },
      customCss: ['./src/styles/tokens.css'],
      sidebar: [
        { label: 'Start', items: ['docs', 'docs/how-it-works', 'docs/install', 'docs/one-machine', 'docs/several-machines'] },
        { label: 'Tasks', items: ['docs/install-engines', 'docs/engines', 'docs/deploy', 'docs/requests', 'docs/parking'] },
        { label: 'Reference', items: ['docs/reference/cli', 'docs/reference/configuration', 'docs/reference/settings', 'docs/reference/engine-flags', 'docs/reference/network-access', 'docs/reference/installer', 'docs/reference/errors'] },
      ],
      // Website spec, Quality checks: no broken internal links.
      plugins: [starlightLinksValidator()],
    }),
  ],
});

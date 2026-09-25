import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import starlightLinksValidator from 'starlight-links-validator';
import { REPO_URL } from './site.config.mjs';

export default defineConfig({
  // Hosting is undecided (website spec, Open decisions 1); `site` only feeds
  // canonical URLs and the sitemap. Override with MLLM_SITE_URL.
  site: process.env.MLLM_SITE_URL ?? 'https://mllm.invalid',
  integrations: [
    starlight({
      title: 'mllm',
      description: 'A model manager for vLLM and SGLang on your own GPUs.',
      social: [{ icon: 'github', label: 'GitHub', href: REPO_URL }],
      // Website spec, Visual system: code blocks are dark in both themes.
      expressiveCode: { themes: ['github-dark'] },
      customCss: ['./src/styles/tokens.css'],
      sidebar: [
        { label: 'Start', items: ['docs', 'docs/install'] },
        { label: 'Reference', items: ['docs/reference/cli'] },
      ],
      // Website spec, Quality checks: no broken internal links.
      plugins: [starlightLinksValidator()],
    }),
  ],
});

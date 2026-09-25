// Source (repository-relative) -> Starlight slug. The sidebar order is set in
// astro.config.mjs.
export const PAGES = [
  { source: 'docs/guide/index.md', slug: 'docs', title: 'Overview' },
  { source: 'docs/guide/install.md', slug: 'docs/install' },
  { source: 'docs/guide/one-machine.md', slug: 'docs/one-machine' },
  { source: 'docs/guide/several-machines.md', slug: 'docs/several-machines' },
  { source: 'docs/guide/engines.md', slug: 'docs/engines' },
  { source: 'docs/guide/deploy.md', slug: 'docs/deploy' },
  { source: 'docs/guide/requests.md', slug: 'docs/requests' },
  { source: 'docs/guide/parking.md', slug: 'docs/parking' },
  { source: 'docs/guide/configuration.md', slug: 'docs/reference/configuration', title: 'Configuration files' },
  { source: 'docs/guide/installer.md', slug: 'docs/reference/installer' },
  { source: 'docs/guide/errors.md', slug: 'docs/reference/errors' },
];

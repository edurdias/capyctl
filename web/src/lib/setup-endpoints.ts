// The management routes the set-up screens call (plan U15, U16), in one place
// so they can follow the server.

const host = (id: string) => `/management/v1/hosts/${encodeURIComponent(id)}`;

// SPEC §14, ADR 0040 §9: deployments are created, replaced and checked.
export const DEPLOYMENTS = {
  create: '/management/v1/deployments',
  replace: (id: string) => `/management/v1/deployments/${encodeURIComponent(id)}`,
  check: '/management/v1/deployments/check',
  // C2: edit the accepted source, never the frozen effective deployment.
  source: (id: string) => `/management/v1/deployments/${encodeURIComponent(id)}/source`,
  effectiveConfig: (id: string) => `/management/v1/deployments/${encodeURIComponent(id)}/effective-config`,
} as const;

// SPEC §4.1: host invitations (crates/capyctl-management/src/enrollment.rs).
export const INVITATIONS = '/management/v1/host-invitations';

// ADR 0040 §§10, 11: host administration.
export const HOST_ADMIN = {
  detected: (id: string, paths: string[]) => {
    const query = paths.map((path) => `path=${encodeURIComponent(path)}`).join('&');
    return `${host(id)}/engines/detected${query ? `?${query}` : ''}`;
  },
  addEngine: (id: string) => `${host(id)}/engines`,
  removeEngine: (id: string, name: string) => `${host(id)}/engines/${encodeURIComponent(name)}/remove`,
  modelStore: (id: string) => `${host(id)}/model-store`,
  prune: (id: string) => `${host(id)}/model-store/prune`,
  config: (id: string) => `${host(id)}/config`,
  customPreview: (id: string) => `${host(id)}/custom-engines/preview`,
  customConfirm: (id: string) => `${host(id)}/custom-engines`,
} as const;

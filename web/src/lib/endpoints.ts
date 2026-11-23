// The management routes the playground and settings screens call, in one
// place so they can follow the server.

// ADR 0040 §6: browser sessions.
export const SESSIONS = {
  list: '/management/v1/ui/sessions',
  revoke: (id: string) => `/management/v1/ui/sessions/${encodeURIComponent(id)}/revoke`,
  revokeAll: '/management/v1/ui/sessions/revoke-all',
  signOut: '/management/v1/ui/sign-out',
} as const;

// ADR 0040 §12: the playground forwards into the role's router, with the
// router's bodies and answers.
export const PLAYGROUND = {
  chat: '/management/v1/playground/chat/completions',
  decisions: '/management/v1/playground/systemone',
} as const;

// ADR 0040 §13: files the build writes beside the bundle.
export const BUILD_ASSETS = {
  commit: '/ui/BUILD_COMMIT',
  licenses: '/ui/licenses.txt',
} as const;

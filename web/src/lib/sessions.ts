import { request } from './api';
import { SESSIONS } from './endpoints';

// ADR 0040 §6 (crates/capyctl-management/src/ui_sessions.rs `view`): one
// browser session, with `current` marking the caller's own.
export interface UiSession {
  id: string;
  label: string;
  created_at_ms: number;
  last_seen_at_ms: number;
  current: boolean;
}

export async function listSessions(): Promise<UiSession[]> {
  const answer = await request<{ sessions: UiSession[] }>(SESSIONS.list);
  return answer.sessions;
}

// The routes take an empty body (ui_sessions.rs `command`).
export async function revokeSession(id: string): Promise<void> {
  await request<unknown>(SESSIONS.revoke(id), { method: 'POST' });
}

export async function revokeAllSessions(): Promise<void> {
  await request<unknown>(SESSIONS.revokeAll, { method: 'POST' });
}

export async function signOut(): Promise<void> {
  await request<unknown>(SESSIONS.signOut, { method: 'POST' });
}

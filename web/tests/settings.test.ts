// ADR 0040 §6 (plan U18, SPEC R8): session management in Settings, driven
// through the mounted App against ui_sessions.rs's shapes.
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { button, dialog, open, role, setUp, snapshotWith, tearDown, tick } from './fixtures';
import { json } from './helpers';

beforeEach(setUp);
afterEach(tearDown);

const SESSIONS = [
  { id: 'laptop', label: 'Firefox on Linux', created_at_ms: 1_793_000_000_000, last_seen_at_ms: 1_793_100_000_000, current: true },
  { id: 'phone', label: 'Safari on iPhone', created_at_ms: 1_793_050_000_000, last_seen_at_ms: 1_793_090_000_000, current: false },
];

function sessionsRole() {
  return role({
    snapshots: [snapshotWith([])],
    route: (call) => {
      if (call.url === '/management/v1/ui/sessions' && call.method === 'GET') {
        return json({ api_version: '1', sessions: SESSIONS });
      }
      if (call.url === '/management/v1/ui/sessions/phone/revoke') {
        return json({ api_version: '1', revoked: true });
      }
      if (call.url === '/management/v1/ui/sessions/revoke-all') {
        return json({ api_version: '1', revoked: 2 });
      }
      if (call.url === '/management/v1/ui/sign-out') {
        return json({ api_version: '1', signed_out: true });
      }
      return undefined;
    },
  });
}

function rows(): string[] {
  return Array.from(document.querySelectorAll('[data-session]')).map((row) => row.getAttribute('data-session') ?? '');
}

describe('settings', () => {
  it('marks this device and removes a revoked session’s row', async () => {
    const server = sessionsRole();
    await open('/settings');
    expect(rows()).toEqual(['laptop', 'phone']);
    expect(document.querySelector('[data-session="laptop"]')?.textContent).toContain('This device');

    button('Revoke', document.querySelector('[data-session="phone"]')!).click();
    await tick();
    expect(server.posts().map((c) => c.url)).toEqual(['/management/v1/ui/sessions/phone/revoke']);
    expect(rows()).toEqual(['laptop']);
  });

  it('lands this tab on the sign-in page after revoke-all is confirmed', async () => {
    const server = sessionsRole();
    await open('/settings');
    button('Revoke all sessions').click();
    await tick();
    expect(dialog().textContent).toContain('You will be signed out here too.');
    expect(server.posts()).toEqual([]);

    button('Revoke all', dialog()).click();
    await tick();
    expect(server.posts().map((c) => c.url)).toEqual(['/management/v1/ui/sessions/revoke-all']);
    expect(window.location.pathname).toBe('/ui/sign-in');
  });

  it('signs out, clearing the session, and shows the sign-in page', async () => {
    const server = sessionsRole();
    await open('/settings');
    button('Sign out').click();
    await tick();
    expect(server.posts().map((c) => c.url)).toEqual(['/management/v1/ui/sign-out']);
    expect(window.location.pathname).toBe('/ui/sign-in');
  });
});

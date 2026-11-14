// ADR 0040 (plan KTD6, U12): the sign-in page, driven through the mounted
// App as a browser opening the link from `capyctl open ui` would.
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import App from '../src/App.svelte';
import { cursor, installFetch, json, managementError, snapshot, type Call } from './helpers';

const SESSIONS = '/management/v1/ui/sessions';

let app: ReturnType<typeof mount> | undefined;

function open(url: string) {
  window.history.replaceState(null, '', url);
  window.dispatchEvent(new PopStateEvent('popstate'));
  app = mount(App, { target: document.body });
  flushSync();
}

function button(label: string): HTMLButtonElement | undefined {
  return Array.from(document.querySelectorAll('button')).find((b) => b.textContent?.trim() === label);
}

afterEach(() => {
  if (app) {
    unmount(app);
    app = undefined;
  }
});

describe('sign-in page', () => {
  it('removes #t= before any request and posts the token only when the button is pressed', async () => {
    const replaceState = vi.spyOn(window.history, 'replaceState');
    const calls = installFetch((call: Call) => {
      if (call.url === SESSIONS) {
        return json({ api_version: '1' });
      }
      if (call.url === '/management/v1/snapshot') {
        return json(snapshot(cursor(1)));
      }
      return new Promise<Response>(() => {});
    });
    open('/ui/sign-in#t=abc');

    expect(window.location.hash).toBe('');
    expect(window.location.pathname).toBe('/ui/sign-in');
    expect(replaceState).toHaveBeenCalled();
    expect(calls).toHaveLength(0);

    button('Sign in this browser')!.click();
    await vi.waitFor(() => expect(window.location.pathname).toBe('/ui/'));

    const exchange = calls.filter((call) => call.url === SESSIONS);
    expect(exchange).toHaveLength(1);
    expect(calls[0]).toBe(exchange[0]);
    expect(exchange[0].method).toBe('POST');
    expect(JSON.parse(exchange[0].body ?? '')).toEqual({ token: 'abc' });
    expect(exchange[0].headers.get('X-CapyCTL-UI')).toBe('1');
    expect(exchange[0].headers.get('Content-Type')).toBe('application/json');
    expect(calls.every((call) => call.hash === '')).toBe(true);
  });

  it('shows the instructions with the "already used or expired" line when the exchange answers 401', async () => {
    installFetch(() => managementError(401, 'unauthenticated'));
    open('/ui/sign-in#t=spent');

    button('Sign in this browser')!.click();
    await vi.waitFor(() => expect(document.body.textContent).toContain('This sign-in link was already used or has expired.'));
    expect(document.body.textContent).toContain('capyctl open ui');
    expect(button('Sign in this browser')).toBeUndefined();
    expect(window.location.pathname).toBe('/ui/sign-in');
  });

  it('shows the instructions without a link and sends nothing', () => {
    const calls = installFetch(() => new Promise<Response>(() => {}));
    open('/ui/sign-in');

    expect(document.body.textContent).toContain('capyctl open ui');
    expect(document.body.textContent).not.toContain('already used or has expired');
    expect(button('Sign in this browser')).toBeUndefined();
    expect(calls).toHaveLength(0);
  });
});

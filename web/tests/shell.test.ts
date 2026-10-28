// ADR 0040 (plan U12): the shell's "Reconnecting…" indicator and its sign-in
// routing, driven through the mounted App.
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import App from '../src/App.svelte';
import { navigate } from '../src/lib/router';
import { cursor, installFetch, json, managementError, settle, setVisibility, snapshot, StreamMock } from './helpers';

let app: ReturnType<typeof mount> | undefined;

function render() {
  app = mount(App, { target: document.body });
  flushSync();
}

beforeEach(() => {
  vi.useFakeTimers();
  setVisibility('visible');
  navigate('/', { replace: true });
});

afterEach(() => {
  if (app) {
    unmount(app);
    app = undefined;
  }
  vi.useRealTimers();
});

describe('app shell', () => {
  it('shows Reconnecting… after three seconds away from the stream and clears it when streaming returns', async () => {
    vi.spyOn(Math, 'random').mockReturnValue(0);
    const streams: StreamMock[] = [];
    let online = true;
    installFetch((call) => {
      if (call.url === '/management/v1/snapshot') {
        return json(snapshot(cursor(5)));
      }
      if (call.url !== '/management/v1/events') {
        // The Overview's own reads (hosts, load) stay pending.
        return new Promise<Response>(() => {});
      }
      if (!online) {
        return Promise.reject(new TypeError('Failed to fetch'));
      }
      const stream = new StreamMock(call.signal);
      streams.push(stream);
      return stream.response;
    });
    render();
    await settle();
    expect(document.body.textContent).not.toContain('Reconnecting…');

    // The network drops: the stream ends and reconnecting fails.
    online = false;
    streams[0].end();
    await settle();
    await vi.advanceTimersByTimeAsync(2999);
    flushSync();
    expect(document.body.textContent).not.toContain('Reconnecting…');
    await vi.advanceTimersByTimeAsync(1);
    flushSync();
    expect(document.body.textContent).toContain('Reconnecting…');
    expect(document.body.textContent).toContain('What you see may be out of date.');

    // Retries come at 0.5, 1.5 and 3.5 s; the last one finds the network.
    online = true;
    await vi.advanceTimersByTimeAsync(500);
    flushSync();
    expect(streams).toHaveLength(2);
    expect(document.body.textContent).not.toContain('Reconnecting…');
  });

  it('routes a 401 to the sign-in instructions page', async () => {
    installFetch((call) => {
      if (call.url === '/management/v1/snapshot') {
        return managementError(401, 'unauthenticated');
      }
      throw new Error(`unexpected request ${call.url}`);
    });
    render();
    await settle();
    flushSync();

    expect(window.location.pathname).toBe('/ui/sign-in');
    expect(document.body.textContent).toContain('capyctl open ui');
    expect(document.body.textContent).not.toContain('already used or has expired');
  });

  it('shows the sidebar sections and the brand', () => {
    installFetch(() => new Promise<Response>(() => {}));
    render();
    const labels = Array.from(document.querySelectorAll('nav[aria-label="Sections"] li a'), (a) => a.textContent);
    expect(labels).toEqual(['Overview', 'Models', 'Hosts', 'Engines', 'Playground', 'Activity', 'Settings']);
    expect(document.querySelector('img[alt="CapyCTL"]')?.getAttribute('src')).toMatch(/horizontal-dark\.png$/);
  });
});

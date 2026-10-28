// ADR 0040 (plan KTD8, U12): the event stream client and the live state it
// keeps, driven through startLiveState as the shell runs it.
import { get } from 'svelte/store';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { navigate } from '../src/lib/router';
import { snapshot as snapshotStore, startLiveState, streamState } from '../src/lib/state';
import { cursor, installFetch, json, managementError, settle, setVisibility, snapshot, StreamMock, type Call } from './helpers';

const EVENTS = '/management/v1/events';
const SNAPSHOT = '/management/v1/snapshot';

// Each events request takes the next answer: 'stream' opens a StreamMock,
// a number answers that status with the management envelope.
function server(options: { snapshots: string[]; events: Array<'stream' | number> }) {
  const streams: StreamMock[] = [];
  const calls = installFetch((call: Call) => {
    if (call.url === SNAPSHOT) {
      // Later reads (the refresh an event triggers) see the last state.
      const at = options.snapshots.length > 1 ? options.snapshots.shift()! : options.snapshots[0];
      return json(snapshot(at));
    }
    if (call.url.startsWith(EVENTS)) {
      const next = options.events.shift();
      if (next === undefined) {
        // Hold the request open, as a busy server would.
        return new Promise<Response>(() => {});
      }
      if (next === 'stream') {
        const stream = new StreamMock(call.signal);
        streams.push(stream);
        return stream.response;
      }
      const code = { 400: 'invalid_cursor', 401: 'unauthenticated', 410: 'cursor_expired', 429: 'queue_full' }[next];
      return managementError(next, code ?? 'internal', next === 429);
    }
    throw new Error(`unexpected request ${call.method} ${call.url}`);
  });
  const eventCalls = () => calls.filter((call) => call.url.startsWith(EVENTS));
  const snapshotCalls = () => calls.filter((call) => call.url === SNAPSHOT);
  return { calls, streams, eventCalls, snapshotCalls };
}

let stop: (() => void) | undefined;

beforeEach(() => {
  vi.useFakeTimers();
  setVisibility('visible');
  navigate('/', { replace: true });
});

afterEach(() => {
  stop?.();
  stop = undefined;
  vi.useRealTimers();
});

describe('event stream client', () => {
  it('reloads the snapshot on a 410 at reconnect and resumes from the new cursor, sending no action (AE4)', async () => {
    const s = server({ snapshots: [cursor(5), cursor(42)], events: ['stream', 410, 'stream'] });
    stop = startLiveState();
    await settle();
    expect(get(streamState)).toBe('streaming');

    s.streams[0].event(cursor(6), 'ready_committed');
    await settle();
    // The server ends the stream at its lifetime; the reconnect finds the
    // cursor expired.
    s.streams[0].end();
    await settle();

    const events = s.eventCalls();
    expect(events).toHaveLength(3);
    expect(events[1].headers.get('Last-Event-ID')).toBe(cursor(6));
    expect(s.snapshotCalls()).toHaveLength(2);
    expect(events[2].headers.get('Last-Event-ID')).toBe(cursor(42));
    expect(get(snapshotStore)?.cursor).toBe(cursor(42));
    expect(get(streamState)).toBe('streaming');
    expect(s.calls.every((call) => call.method === 'GET')).toBe(true);
  });

  it('sends the cursor only in Last-Event-ID, never as ?after=, with the UI header on every request', async () => {
    const s = server({ snapshots: [cursor(5)], events: ['stream', 'stream'] });
    stop = startLiveState();
    await settle();
    s.streams[0].event(cursor(9), 'park_accepted');
    await settle();
    s.streams[0].end();
    await settle();

    const events = s.eventCalls();
    expect(events).toHaveLength(2);
    expect(events.map((call) => call.headers.get('Last-Event-ID'))).toEqual([cursor(5), cursor(9)]);
    for (const call of events) {
      expect(call.url).toBe(EVENTS);
      expect(new URL(call.url, 'http://box').searchParams.has('after')).toBe(false);
    }
    for (const call of s.calls) {
      expect(call.headers.get('X-CapyCTL-UI')).toBe('1');
    }
  });

  it.each([
    [0, 500],
    [1, 1000],
  ])('waits a jittered delay after a 429 before reconnecting (random %d → %d ms)', async (random, delay) => {
    vi.spyOn(Math, 'random').mockReturnValue(random);
    const s = server({ snapshots: [cursor(5)], events: ['stream', 429, 'stream'] });
    stop = startLiveState();
    await settle();
    s.streams[0].event(cursor(6), 'ready_committed');
    await settle();
    s.streams[0].end();
    await settle();
    expect(get(streamState)).toBe('backoff');
    expect(s.eventCalls()).toHaveLength(2);

    await vi.advanceTimersByTimeAsync(delay - 1);
    expect(s.eventCalls()).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(1);
    expect(s.eventCalls()).toHaveLength(3);
    expect(s.eventCalls()[2].headers.get('Last-Event-ID')).toBe(cursor(6));
    expect(get(streamState)).toBe('streaming');
  });

  it('moves to sign-in on a 401 and stops reconnecting', async () => {
    const s = server({ snapshots: [cursor(5)], events: ['stream', 401] });
    stop = startLiveState();
    await settle();
    s.streams[0].end();
    await settle();

    expect(get(streamState)).toBe('signin');
    expect(window.location.pathname).toBe('/ui/sign-in');
    const sent = s.calls.length;
    await vi.advanceTimersByTimeAsync(10 * 60_000);
    expect(s.calls).toHaveLength(sent);
  });

  it('closes the stream while the tab is hidden and reopens it from the last cursor when shown', async () => {
    const s = server({ snapshots: [cursor(5)], events: ['stream', 'stream'] });
    stop = startLiveState();
    await settle();
    s.streams[0].event(cursor(7), 'parked_committed');
    await settle();

    setVisibility('hidden');
    await settle();
    expect(s.eventCalls()[0].signal?.aborted).toBe(true);
    expect(get(streamState)).toBe('paused');
    await vi.advanceTimersByTimeAsync(10 * 60_000);
    expect(s.eventCalls()).toHaveLength(1);

    setVisibility('visible');
    await settle();
    expect(s.eventCalls()).toHaveLength(2);
    expect(s.eventCalls()[1].headers.get('Last-Event-ID')).toBe(cursor(7));
    expect(get(streamState)).toBe('streaming');
  });
});

import { writable, type Readable } from 'svelte/store';
import { request, signedOut } from './api';
import { EventClient, type StreamState } from './events';
import type { ManagementEvent, Snapshot } from './types';

export const SNAPSHOT_PATH = '/management/v1/snapshot';

// How long the stream may be away before the shell says so (plan U12).
export const RECONNECTING_NOTICE_MS = 3000;
// Events arrive in bursts (one action records several); one snapshot read
// covers a burst.
const REFRESH_COALESCE_MS = 250;
const REFRESH_RETRY_MS = 1000;
const RECENT_EVENTS = 100;

const snapshotStore = writable<Snapshot | null>(null);
const recentStore = writable<ManagementEvent[]>([]);
const streamStore = writable<StreamState>('stopped');
const reconnectingStore = writable(false);
let snapshotGeneration = 0;
let latestSnapshot: Snapshot | null = null;

// SPEC R11, AE4 (plan KTD8): an older concurrent read must not undo a newer
// snapshot. A new database incarnation is established by stream recovery.
function publishSnapshot(value: Snapshot, generation: number): Snapshot {
  if (generation !== snapshotGeneration) {
    return latestSnapshot ?? value;
  }
  if (latestSnapshot !== null) {
    const [incarnation, sequence] = value.cursor.split(':');
    const [currentIncarnation, currentSequence] = latestSnapshot.cursor.split(':');
    if (incarnation === currentIncarnation && BigInt(sequence) < BigInt(currentSequence)) {
      return latestSnapshot;
    }
  }
  latestSnapshot = value;
  snapshotStore.set(value);
  return value;
}

// The current state of the role, from the snapshot kept current by the event
// stream.
export const snapshot: Readable<Snapshot | null> = { subscribe: snapshotStore.subscribe };
// The events received since this tab opened, newest first.
export const recentEvents: Readable<ManagementEvent[]> = { subscribe: recentStore.subscribe };
export const streamState: Readable<StreamState> = { subscribe: streamStore.subscribe };
// True once the stream has been away for RECONNECTING_NOTICE_MS: the shell
// shows "Reconnecting…" and live values may be out of date.
export const reconnecting: Readable<boolean> = { subscribe: reconnectingStore.subscribe };

// Reads the snapshot now, outside the event stream: an action refused with
// `revision_conflict` shows the current version at once (plan U14).
export async function reloadSnapshot(signal?: AbortSignal): Promise<Snapshot> {
  const generation = snapshotGeneration;
  const value = await request<Snapshot>(SNAPSHOT_PATH, { signal });
  signal?.throwIfAborted();
  return publishSnapshot(value, generation);
}

// Starts the live state for a signed-in shell and returns its stop function.
//
// Each event is applied by reading the snapshot again rather than by
// re-deriving it in the browser: model states are derived from the store's
// tables (crates/capyctl-store/src/snapshot.rs), and a second derivation here
// would drift from the CLI's. The stream keeps its own cursor; a snapshot
// read never moves it, so no event is skipped.
export function startLiveState(): () => void {
  snapshotGeneration += 1;
  latestSnapshot = null;
  let stopped = false;
  let refreshTimer: ReturnType<typeof setTimeout> | null = null;
  let refreshing = false;
  let refreshAgain = false;
  let noticeTimer: ReturnType<typeof setTimeout> | null = null;
  let streamed = false;

  const scheduleRefresh = (delay: number) => {
    if (stopped || refreshTimer !== null) {
      return;
    }
    refreshTimer = setTimeout(() => {
      refreshTimer = null;
      void refresh();
    }, delay);
  };

  const refresh = async () => {
    if (refreshing) {
      refreshAgain = true;
      return;
    }
    refreshing = true;
    const generation = snapshotGeneration;
    try {
      const value = await request<Snapshot>(SNAPSHOT_PATH);
      if (!stopped) {
        publishSnapshot(value, generation);
      }
    } catch {
      // A 401 already routed to sign-in; anything else tries again.
      scheduleRefresh(REFRESH_RETRY_MS);
    } finally {
      refreshing = false;
    }
    if (refreshAgain) {
      refreshAgain = false;
      scheduleRefresh(REFRESH_COALESCE_MS);
    }
  };

  const clearNotice = () => {
    if (noticeTimer !== null) {
      clearTimeout(noticeTimer);
      noticeTimer = null;
    }
    reconnectingStore.set(false);
  };

  const onStateChange = (state: StreamState) => {
    streamStore.set(state);
    const away = state === 'reconnecting' || state === 'backoff' || (state === 'snapshot' && streamed);
    if (state === 'streaming') {
      streamed = true;
    }
    if (away) {
      noticeTimer ??= setTimeout(() => reconnectingStore.set(true), RECONNECTING_NOTICE_MS);
    } else {
      clearNotice();
    }
  };

  const client = new EventClient({
    loadSnapshot: async (signal) => {
      // The event client decides what a 401 means for the stream.
      const generation = ++snapshotGeneration;
      const value = await request<Snapshot>(SNAPSHOT_PATH, { signal, signInOn401: false });
      if (!stopped && !signal.aborted && generation === snapshotGeneration) {
        publishSnapshot(value, generation);
        // Invalidate reads begun before or during this recovery, including
        // reads of the previous database incarnation.
        snapshotGeneration += 1;
      }
      return value.cursor;
    },
    onEvent: (event) => {
      recentStore.update((events) => [event, ...events].slice(0, RECENT_EVENTS));
      scheduleRefresh(REFRESH_COALESCE_MS);
    },
    onStateChange,
    onSignIn: signedOut,
  });
  client.start();

  return () => {
    stopped = true;
    snapshotGeneration += 1;
    client.stop();
    clearTimeout(refreshTimer ?? undefined);
    clearNotice();
  };
}

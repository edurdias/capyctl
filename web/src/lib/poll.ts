import { recentEvents } from './state';

// Reads that the event stream does not carry (host memory, load, latency)
// are read again on an interval and after each burst of events, so a change
// shows within about two seconds of its event (plan, Success Criteria).
// Returns the stop function.
export function livePoll(read: () => Promise<void>, intervalMs: number): () => void {
  let stopped = false;
  let running = false;
  let again = false;
  let eventTimer: ReturnType<typeof setTimeout> | null = null;
  const run = async () => {
    if (stopped) {
      return;
    }
    if (running) {
      again = true;
      return;
    }
    running = true;
    try {
      await read();
    } catch {
      // The next tick tries again; a 401 already routed to sign-in.
    } finally {
      running = false;
    }
    if (again) {
      again = false;
      void run();
    }
  };
  void run();
  const interval = setInterval(() => void run(), intervalMs);
  let first = true;
  const unsubscribe = recentEvents.subscribe(() => {
    // The subscription answers at once with the current list.
    if (first) {
      first = false;
      return;
    }
    eventTimer ??= setTimeout(() => {
      eventTimer = null;
      void run();
    }, 500);
  });
  return () => {
    stopped = true;
    clearInterval(interval);
    clearTimeout(eventTimer ?? undefined);
    unsubscribe();
  };
}

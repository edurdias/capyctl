import { createParser } from 'eventsource-parser';
import { ApiError, parseError, signedOut, uiHeaders } from './api';
import { EVENTS_PATH } from './events';
import { STREAM_ERROR_EVENT, type EventKind, type ManagementEvent, type ManagementEventData } from './types';

// SPEC R14, R15: each event kind in the guide's words. Internal terms
// (leases, epochs, reservations, fencing) never appear.
export const EVENT_WORDS: Record<EventKind, string> = {
  initialize_accepted: 'Start requested',
  initialize_armed: 'Starting',
  owned_launch_associated: 'Engine launched',
  ready_committed: 'Ready',
  initialize_uncertain: 'Checking whether the start finished',
  initialize_expired_unarmed: 'Start timed out before it began',
  initialize_failed_released: 'Start failed',
  ordinary_cleanup_accepted: 'Stop requested',
  ordinary_cleanup_armed: 'Stopping',
  ordinary_cleanup_completed: 'Stopped',
  ordinary_cleanup_expired_unarmed: 'Stop timed out before it began',
  ordinary_unarmed_stop_accepted: 'Stop requested',
  ordinary_unarmed_stop_completed: 'Stopped',
  park_accepted: 'Park requested',
  park_armed: 'Parking',
  parked_committed: 'Parked',
  park_refused: 'Park refused',
  park_uncertain: 'Checking whether the park finished',
  restore_accepted: 'Wake requested',
  restore_armed: 'Waking',
  restored_committed: 'Woke up and ready',
  restore_refused: 'Wake refused',
  restore_uncertain: 'Checking whether the wake finished',
  residency_cancelled: 'Park or wake cancelled',
  switch_planned: 'Making room',
  switch_admission_closed: 'Making room: holding new requests',
  switch_released: 'Making room: model switched out',
  switch_completed: 'Made room',
  switch_failed: 'Making room failed',
  managed_configuration_accepted: 'Configuration saved',
  deployment_deleted: 'Model deleted',
  coordinator_session_started: 'CapyCTL started',
  host_resource_policy_bootstrapped: 'Host settings recorded',
  host_resource_policy_updated: 'Host settings changed',
  host_resource_policy_migrated: 'Host settings upgraded',
  installation_drift_flagged: 'An engine installation changed on its host',
  host_revoked: 'Host revoked',
  host_recovery_invited: 'Host recovery invitation created',
  host_recovered: 'Host recovered',
  host_drain_intent_expired: 'Host drain ran out of time',
  request_cancellation_acknowledged: 'Cancelled requests finished',
  lmcache_off: 'KV cache offload turned off',
  lmcache_server_exited: 'KV cache offload server exited',
  topology_auto_changed: 'GPU layout changed automatically',
  instance_restarted_for_held_requests: 'Instance restarted to finish held requests',
};

// A kind this build does not know reads as its code in words.
export function eventWord(kind: string): string {
  return Object.hasOwn(EVENT_WORDS, kind) ? EVENT_WORDS[kind as EventKind] : kind.replaceAll('_', ' ');
}

// `<database incarnation ULID>:<sequence>` (events.rs `cursor`).
export function cursorParts(cursor: string): { incarnation: string; sequence: number } {
  const [incarnation, sequence] = cursor.split(':');
  return { incarnation, sequence: Number(sequence) };
}

export const ACTIVITY_PAGE = 50;

export interface ActivityPage {
  // Oldest first.
  events: ManagementEvent[];
  // The page reaches the oldest event the role keeps: nothing older can be
  // read (docs/operations/data-retention.md).
  boundary: boolean;
}

// Reads the events after `after` (or from the oldest kept when null) up to
// and including sequence `through`, then closes the stream. The stream sends
// a heartbeat only once it has nothing more, so a heartbeat ends the read too.
async function readRange(after: string | null, through: number, signal: AbortSignal): Promise<ManagementEvent[]> {
  signal.throwIfAborted();
  const controller = new AbortController();
  const abort = () => controller.abort();
  signal.addEventListener('abort', abort, { once: true });
  let reader: ReadableStreamDefaultReader<Uint8Array> | null = null;
  try {
    // ADR 0040 (plan KTD8): the cursor travels in Last-Event-ID.
    const headers = uiHeaders({ Accept: 'text/event-stream' });
    if (after !== null) {
      headers['Last-Event-ID'] = after;
    }
    const response = await fetch(EVENTS_PATH, {
      method: 'GET',
      headers,
      credentials: 'same-origin',
      cache: 'no-store',
      signal: controller.signal,
    });
    signal.throwIfAborted();
    if (!response.ok || response.body === null) {
      if (response.status === 401) {
        signedOut();
      }
      let body: unknown = undefined;
      try {
        body = await response.json();
      } catch {
        body = undefined;
      }
      throw parseError(response.status, body);
    }
    const events: ManagementEvent[] = [];
    let done = false;
    const parser = createParser({
      onEvent: (message) => {
        if (done) {
          return;
        }
        if (message.event === STREAM_ERROR_EVENT) {
          throw parseError(200, JSON.parse(message.data));
        }
        if (message.id === undefined || message.event === undefined) {
          return;
        }
        const sequence = cursorParts(message.id).sequence;
        if (sequence > through) {
          done = true;
          return;
        }
        events.push({ cursor: message.id, kind: message.event as EventKind, data: JSON.parse(message.data) as ManagementEventData });
        if (sequence === through) {
          done = true;
        }
      },
      onComment: () => {
        done = true;
      },
    });
    reader = response.body.getReader();
    const decoder = new TextDecoder();
    while (!done) {
      const { done: ended, value } = await reader.read();
      if (ended) {
        // SPEC R14: only reaching `through` or the server's caught-up
        // heartbeat proves this page complete. A lifetime end is not proof,
        // so the caller must retry this page instead of advancing past it.
        throw new Error('Activity stream ended before the page was complete.');
      }
      parser.feed(decoder.decode(value, { stream: true }));
    }
    signal.throwIfAborted();
    return events;
  } catch (caught) {
    signal.throwIfAborted();
    if (caught instanceof ApiError) {
      throw caught;
    }
    throw new ApiError({
      status: 0,
      code: null,
      message: 'Activity stopped arriving before this page was complete. Show older events to try again.',
      retryable: true,
      operationId: null,
      details: {},
      envelope: 'none',
    });
  } finally {
    signal.removeEventListener('abort', abort);
    controller.abort();
    if (reader !== null) {
      await reader.cancel().catch(() => {});
      reader.releaseLock();
    }
  }
}

// SPEC R14: one page of the event history, the ACTIVITY_PAGE events through
// sequence `through` of database `incarnation`. A page that starts before
// the oldest event kept (the server answers 410 `cursor_expired`) reads from
// the oldest kept instead and marks the retention boundary.
export async function readActivityPage(incarnation: string, through: number, signal: AbortSignal): Promise<ActivityPage> {
  const from = through - ACTIVITY_PAGE;
  if (from <= 0) {
    return { events: await readRange(null, through, signal), boundary: true };
  }
  try {
    return { events: await readRange(`${incarnation}:${from}`, through, signal), boundary: false };
  } catch (error) {
    if (error instanceof ApiError && error.code === 'cursor_expired') {
      return { events: await readRange(null, through, signal), boundary: true };
    }
    throw error;
  }
}

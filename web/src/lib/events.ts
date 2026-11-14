import { createParser } from 'eventsource-parser';
import { ApiError, uiHeaders } from './api';
import { STREAM_ERROR_EVENT, type EventKind, type ManagementEvent, type ManagementEventData, type StreamErrorData } from './types';

export const EVENTS_PATH = '/management/v1/events';

// The tab's event stream states (plan, High-Level Technical Design):
// Snapshot → Streaming; Streaming → Reconnecting on lifetime end or network
// loss, → Paused while hidden; Reconnecting → Snapshot on 410/400 or a
// resnapshot_required stream error, → Backoff on 429; any 401 → SignIn.
export type StreamState = 'stopped' | 'snapshot' | 'streaming' | 'paused' | 'reconnecting' | 'backoff' | 'signin';

export interface EventClientOptions {
  // Loads the snapshot and returns its cursor. Throws ApiError on refusal.
  loadSnapshot(signal: AbortSignal): Promise<string>;
  onEvent(event: ManagementEvent): void;
  onStateChange(state: StreamState): void;
  onSignIn(): void;
}

const RETRY_BASE_MS = 1000;
const RETRY_CEILING_MS = 30_000;

// Exponential backoff with jitter: half the window fixed, half random, so
// tabs that lost the server together do not come back together.
function jitteredDelay(attempt: number): number {
  const window = Math.min(RETRY_CEILING_MS, RETRY_BASE_MS * 2 ** attempt);
  return window / 2 + Math.random() * (window / 2);
}

type Outcome = 'ended' | 'resnapshot' | 'backoff' | 'retry';

// ADR 0040 (plan KTD8): the stream is read with fetch and eventsource-parser,
// not EventSource, which hides HTTP statuses, cannot send the UI header, and
// sends its own Last-Event-ID beside `?after=`. The cursor travels only in
// Last-Event-ID. Reconnecting never sends an action (SPEC R11).
export class EventClient {
  #options: EventClientOptions;
  #state: StreamState = 'stopped';
  #cursor: string | null = null;
  #controller: AbortController | null = null;
  #timer: ReturnType<typeof setTimeout> | null = null;
  #failures = 0;
  // Bumped on every transition, so a continuation of an abandoned attempt
  // (an aborted fetch, a late snapshot) does nothing.
  #generation = 0;

  constructor(options: EventClientOptions) {
    this.#options = options;
  }

  get state(): StreamState {
    return this.#state;
  }

  start(): void {
    document.addEventListener('visibilitychange', this.#onVisibility);
    if (document.visibilityState === 'hidden') {
      this.#pause();
    } else {
      void this.#resnapshot();
    }
  }

  stop(): void {
    document.removeEventListener('visibilitychange', this.#onVisibility);
    this.#cancel();
    this.#enter('stopped');
  }

  // Plan KTD8: a hidden tab closes its stream, which keeps it within the
  // browser's six HTTP/1.1 connections and the server's 16 streams.
  #onVisibility = (): void => {
    if (this.#state === 'stopped' || this.#state === 'signin') {
      return;
    }
    if (document.visibilityState === 'hidden') {
      this.#pause();
    } else if (this.#state === 'paused') {
      if (this.#cursor === null) {
        void this.#resnapshot();
      } else {
        this.#reconnect(0);
      }
    }
  };

  #enter(state: StreamState): void {
    this.#state = state;
    this.#options.onStateChange(state);
  }

  #cancel(): number {
    this.#generation += 1;
    this.#controller?.abort();
    this.#controller = null;
    if (this.#timer !== null) {
      clearTimeout(this.#timer);
      this.#timer = null;
    }
    return this.#generation;
  }

  #pause(): void {
    this.#cancel();
    this.#enter('paused');
  }

  #signIn(): void {
    document.removeEventListener('visibilitychange', this.#onVisibility);
    this.#cancel();
    this.#enter('signin');
    this.#options.onSignIn();
  }

  #backoff(): void {
    const generation = this.#cancel();
    this.#enter('backoff');
    const delay = jitteredDelay(this.#failures);
    this.#failures += 1;
    this.#timer = setTimeout(() => {
      if (generation !== this.#generation) {
        return;
      }
      // Without a snapshot there is no cursor to resume from.
      if (this.#cursor === null) {
        void this.#resnapshot();
      } else {
        this.#reconnect(0);
      }
    }, delay);
  }

  #reconnect(delay: number): void {
    const generation = this.#cancel();
    this.#enter('reconnecting');
    if (delay === 0) {
      void this.#connect(generation);
      return;
    }
    this.#timer = setTimeout(() => {
      if (generation === this.#generation) {
        void this.#connect(generation);
      }
    }, delay);
  }

  #retryLater(): void {
    const delay = jitteredDelay(this.#failures);
    this.#failures += 1;
    this.#reconnect(delay);
  }

  async #resnapshot(): Promise<void> {
    const generation = this.#cancel();
    this.#enter('snapshot');
    const controller = new AbortController();
    this.#controller = controller;
    let cursor: string;
    try {
      cursor = await this.#options.loadSnapshot(controller.signal);
    } catch (error) {
      if (generation !== this.#generation) {
        return;
      }
      if (error instanceof ApiError && error.status === 401) {
        this.#signIn();
      } else if (error instanceof ApiError && error.status === 429) {
        this.#backoff();
      } else {
        const delay = jitteredDelay(this.#failures);
        this.#failures += 1;
        this.#timer = setTimeout(() => {
          if (generation === this.#generation) {
            void this.#resnapshot();
          }
        }, delay);
      }
      return;
    }
    if (generation !== this.#generation) {
      return;
    }
    this.#cursor = cursor;
    await this.#connect(generation);
  }

  async #connect(generation: number): Promise<void> {
    const controller = new AbortController();
    this.#controller = controller;
    const headers = uiHeaders({ Accept: 'text/event-stream' });
    if (this.#cursor !== null) {
      headers['Last-Event-ID'] = this.#cursor;
    }
    let response: Response;
    try {
      response = await fetch(EVENTS_PATH, {
        method: 'GET',
        headers,
        credentials: 'same-origin',
        cache: 'no-store',
        signal: controller.signal,
      });
    } catch {
      if (generation === this.#generation) {
        this.#retryLater();
      }
      return;
    }
    if (generation !== this.#generation) {
      return;
    }
    if (response.status === 401) {
      this.#signIn();
      return;
    }
    if (response.status === 429) {
      this.#backoff();
      return;
    }
    if (response.status === 410 || response.status === 400) {
      void this.#resnapshot();
      return;
    }
    if (!response.ok || response.body === null) {
      this.#retryLater();
      return;
    }
    this.#failures = 0;
    this.#enter('streaming');
    const outcome = await this.#read(response.body, generation);
    if (generation !== this.#generation) {
      return;
    }
    switch (outcome) {
      case 'ended':
        // The server ends every stream at its lifetime (300 s at most);
        // resume from the cursor straight away.
        this.#reconnect(0);
        break;
      case 'resnapshot':
        void this.#resnapshot();
        break;
      case 'backoff':
        this.#backoff();
        break;
      case 'retry':
        this.#retryLater();
        break;
    }
  }

  async #read(body: ReadableStream<Uint8Array>, generation: number): Promise<Outcome> {
    let terminal: StreamErrorData['error'] | null = null;
    const parser = createParser({
      onEvent: (message) => {
        if (generation !== this.#generation) {
          return;
        }
        if (message.event === STREAM_ERROR_EVENT) {
          terminal = (JSON.parse(message.data) as StreamErrorData).error;
          return;
        }
        if (message.id === undefined || message.event === undefined) {
          return;
        }
        this.#cursor = message.id;
        this.#options.onEvent({
          cursor: message.id,
          kind: message.event as EventKind,
          data: JSON.parse(message.data) as ManagementEventData,
        });
      },
    });
    const reader = body.getReader();
    const decoder = new TextDecoder();
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) {
          break;
        }
        parser.feed(decoder.decode(value, { stream: true }));
      }
    } catch {
      return 'retry';
    }
    const error = terminal as StreamErrorData['error'] | null;
    if (error === null) {
      return 'ended';
    }
    if (error.resnapshot_required) {
      return 'resnapshot';
    }
    return error.retryable ? 'backoff' : 'retry';
  }
}
